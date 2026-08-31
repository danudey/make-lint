//! Makefile syntax tree.
//!
//! Values are kept unexpanded. Phase 2 (the evaluator) consumes this tree; the
//! phase 1 checks work on it directly.

use crate::span::{FileId, Span};

// ---------------------------------------------------------------------------
// Expressions (the contents of any make value: literals and `$(...)` refs)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Expr {
    pub pieces: Vec<Piece>,
    pub span: Span,
}

impl Expr {
    pub fn empty(span: Span) -> Expr {
        Expr { pieces: Vec::new(), span }
    }

    pub fn is_empty(&self) -> bool {
        self.pieces.is_empty()
    }

    /// The expression's text if it contains no variable or function reference.
    /// `$$` counts as a literal `$`.
    pub fn literal(&self) -> Option<String> {
        let mut s = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text { text, .. } => s.push_str(text),
                Piece::Dollar { .. } => s.push('$'),
                Piece::Var(_) | Piece::Func(_) => return None,
            }
        }
        Some(s)
    }

    /// The literal text before the first variable or function reference.
    /// `$(pastsubst %.c,%.o,$(SRCS))` has a name whose leading literal is
    /// `pastsubst %.c,%.o,`, which is enough to spot a mistyped function.
    pub fn leading_literal(&self) -> String {
        let mut s = String::new();
        for p in &self.pieces {
            match p {
                Piece::Text { text, .. } => s.push_str(text),
                Piece::Dollar { .. } => s.push('$'),
                Piece::Var(_) | Piece::Func(_) => break,
            }
        }
        s
    }

    /// Visit every piece, descending into nested expressions.
    pub fn visit(&self, f: &mut impl FnMut(&Piece)) {
        for p in &self.pieces {
            f(p);
            match p {
                Piece::Var(v) => {
                    v.name.visit(f);
                    if let Some(s) = &v.subst {
                        s.from.visit(f);
                        s.to.visit(f);
                    }
                }
                Piece::Func(c) => {
                    for a in &c.args {
                        a.visit(f);
                    }
                }
                Piece::Text { .. } | Piece::Dollar { .. } => {}
            }
        }
    }
}

#[derive(Clone, Debug)]
pub enum Piece {
    Text {
        text: String,
        span: Span,
    },
    /// `$$`, which expands to a literal dollar.
    Dollar {
        span: Span,
    },
    Var(VarRef),
    Func(FuncCall),
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RefStyle {
    /// `$(NAME)`
    Paren,
    /// `${NAME}`
    Brace,
    /// `$N` — a single character name.
    Bare,
}

#[derive(Clone, Debug)]
pub struct VarRef {
    pub name: Expr,
    /// `$(VAR:from=to)`
    pub subst: Option<Subst>,
    pub style: RefStyle,
    /// For [`RefStyle::Bare`], the identifier characters that immediately
    /// follow. `$FOO` parses as `$(F)` plus the literal `OO`, which is almost
    /// never what the author meant.
    pub bare_run_on: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Subst {
    pub from: Expr,
    pub to: Expr,
}

#[derive(Clone, Debug)]
pub struct FuncCall {
    pub name: String,
    pub name_span: Span,
    pub args: Vec<Expr>,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum AssignOp {
    /// `=` — deferred expansion.
    Recursive,
    /// `:=` — expanded at assignment time.
    Simple,
    /// `::=` — POSIX spelling of `:=`.
    SimplePosix,
    /// `:::=` — immediate expansion with re-escaping (make 4.4).
    Immediate,
    /// `+=`
    Append,
    /// `?=`
    Conditional,
    /// `!=` — value is the output of a shell command.
    Shell,
}

impl AssignOp {
    pub fn as_str(self) -> &'static str {
        match self {
            AssignOp::Recursive => "=",
            AssignOp::Simple => ":=",
            AssignOp::SimplePosix => "::=",
            AssignOp::Immediate => ":::=",
            AssignOp::Append => "+=",
            AssignOp::Conditional => "?=",
            AssignOp::Shell => "!=",
        }
    }

    /// True when the value text is stored unexpanded and re-expanded on use.
    pub fn is_deferred(self) -> bool {
        matches!(self, AssignOp::Recursive | AssignOp::Conditional | AssignOp::Append)
    }
}

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct AssignFlags {
    pub is_override: bool,
    pub is_export: bool,
    pub is_private: bool,
}

#[derive(Clone, Debug)]
pub struct Assign {
    pub name: Expr,
    pub op: AssignOp,
    pub op_span: Span,
    pub value: Expr,
    pub flags: AssignFlags,
    /// Present for target-specific variables: `targets: VAR = value`.
    pub target: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum RuleKind {
    /// `target: prereqs`
    Normal,
    /// `target:: prereqs` — multiple independent recipes are legal.
    Double,
    /// `targets: target-pattern: prereq-patterns`
    Static { target_pattern: Expr },
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub targets: Expr,
    pub kind: RuleKind,
    pub prereqs: Expr,
    /// Prerequisites after `|`.
    pub order_only: Option<Expr>,
    pub recipe: Vec<RecipeLine>,
    /// Span of the rule header line only.
    pub span: Span,
    /// Enclosing conditional branches, as `(conditional id, branch index)`.
    /// Two items whose paths disagree on the branch taken for the same
    /// conditional can never both be active.
    pub branch_path: BranchPath,
}

impl Rule {
    pub fn has_recipe(&self) -> bool {
        !self.recipe.is_empty()
    }

    /// True when at least one recipe line is not itself inside a conditional
    /// opened after the rule header. If every line is conditional, make may see
    /// this rule with no recipe at all, so it cannot be said to define one.
    pub fn has_unconditional_recipe(&self) -> bool {
        self.recipe.iter().any(|l| l.branch_path.len() <= self.branch_path.len())
    }
}

/// Position within the conditional tree of one file. Ids are per-file.
pub type BranchPath = Vec<(u32, u32)>;

/// True when `a` and `b` sit in different branches of the same conditional, so
/// make can never see both. Only meaningful for paths from the same file.
pub fn mutually_exclusive(a: &BranchPath, b: &BranchPath) -> bool {
    a.iter().any(|&(cond, br)| b.iter().any(|&(c2, b2)| c2 == cond && b2 != br))
}

#[derive(Clone, Debug)]
pub struct RecipeLine {
    pub expr: Expr,
    pub span: Span,
    /// Conditional branches enclosing this line, which may be deeper than the
    /// rule header's: `target:` then `ifeq ... recipe ... endif`.
    pub branch_path: BranchPath,
}

#[derive(Clone, Debug)]
pub struct Include {
    pub paths: Expr,
    /// `-include` / `sinclude`: a missing file is not an error.
    pub optional: bool,
    pub span: Span,
    /// Paths resolved on disk, filled in by the workspace loader.
    pub resolved: Vec<std::path::PathBuf>,
    /// Literal paths that could not be found on disk.
    pub missing: Vec<String>,
    /// True when the path expression could not be resolved statically.
    pub dynamic: bool,
    /// True when the directive sits inside a conditional. A missing file is
    /// then usually deliberate: the other branch handles it.
    pub guarded: bool,
}

#[derive(Clone, Debug)]
pub enum Cond {
    Eq {
        lhs: Expr,
        rhs: Expr,
        negated: bool,
    },
    Def {
        name: Expr,
        negated: bool,
    },
    /// Recorded so a malformed conditional still nests correctly.
    Malformed,
}

#[derive(Clone, Debug)]
pub struct CondBranch {
    pub cond: Cond,
    pub body: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Conditional {
    /// The `if...` branch followed by any `else if...` branches.
    pub branches: Vec<CondBranch>,
    /// Body of a bare `else`, if present.
    pub else_body: Option<Vec<Item>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Define {
    pub name: Expr,
    pub op: AssignOp,
    /// Body text, verbatim, lines joined with `\n`.
    pub body: String,
    /// The body parsed as a single expression. It has to be one unit because a
    /// reference may span body lines.
    pub body_expr: Expr,
    pub body_span: Span,
    pub flags: AssignFlags,
    pub span: Span,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExportKind {
    Export,
    Unexport,
}

#[derive(Clone, Debug)]
pub struct ExportDirective {
    pub kind: ExportKind,
    /// Empty means "export everything".
    pub names: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Item {
    Assign(Assign),
    /// Index into [`Makefile::rules`]. Rules live in a side table because a
    /// recipe can be interrupted by conditionals, so recipe lines are appended
    /// from several places in the item tree.
    Rule(usize),
    Include(Include),
    Conditional(Conditional),
    Define(Define),
    Export(ExportDirective),
    Undefine {
        name: Expr,
        span: Span,
    },
    /// A line that is nothing but an expansion, such as `$(error ...)` or
    /// `$(eval ...)`. Legal make: it must expand to nothing or to makefile text.
    Expression {
        expr: Expr,
        span: Span,
    },
    Vpath {
        args: Expr,
        span: Span,
    },
    /// A line the parser could not classify.
    Unparsed {
        span: Span,
    },
}

#[derive(Debug)]
pub struct Makefile {
    pub file: FileId,
    pub items: Vec<Item>,
    pub rules: Vec<Rule>,
}

impl Makefile {
    /// Depth-first walk over every item, descending into conditionals.
    pub fn walk_items(&self, f: &mut impl FnMut(&Item)) {
        walk(&self.items, f);
    }

    pub fn walk_items_mut(&mut self, f: &mut impl FnMut(&mut Item)) {
        walk_mut(&mut self.items, f);
    }
}

fn walk(items: &[Item], f: &mut impl FnMut(&Item)) {
    for it in items {
        f(it);
        if let Item::Conditional(c) = it {
            for b in &c.branches {
                walk(&b.body, f);
            }
            if let Some(e) = &c.else_body {
                walk(e, f);
            }
        }
    }
}

fn walk_mut(items: &mut [Item], f: &mut impl FnMut(&mut Item)) {
    for it in items {
        f(it);
        if let Item::Conditional(c) = it {
            for b in &mut c.branches {
                walk_mut(&mut b.body, f);
            }
            if let Some(e) = &mut c.else_body {
                walk_mut(e, f);
            }
        }
    }
}
