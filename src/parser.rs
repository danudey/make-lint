//! Recursive-descent parser over logical lines.
//!
//! Deliberately *not* an evaluator: conditionals keep both branches, values stay
//! unexpanded, and nothing is executed. Recipe lines are appended to a rule in a
//! side table so that a conditional interrupting a recipe (very common) does not
//! orphan the lines inside it.

use crate::ast::*;
use crate::builtins;
use crate::diag::Diagnostic;
use crate::expr;
use crate::lexer::{Lexer, LogicalLine, Mode};
use crate::span::{FileId, Span};

pub fn parse(file: FileId, text: &str) -> (Makefile, Vec<Diagnostic>) {
    let mut p = Parser {
        lx: Lexer::new(file, text),
        diags: Vec::new(),
        rules: Vec::new(),
        current_rule: None,
        cond_depth: 0,
        branch_stack: Vec::new(),
        next_cond_id: 0,
    };
    let (items, stop) = p.parse_block(false);
    if let Stop::Else { span, .. } | Stop::Endif { span } = stop {
        p.diags.push(Diagnostic::error("MK035", span, "unmatched conditional directive"));
    }
    let Parser { diags, rules, .. } = p;
    (Makefile { file, items, rules }, diags)
}

struct Parser<'a> {
    lx: Lexer<'a>,
    diags: Vec<Diagnostic>,
    rules: Vec<Rule>,
    /// Rule whose recipe is currently being collected.
    current_rule: Option<usize>,
    /// Nesting depth of enclosing conditionals.
    cond_depth: usize,
    /// Enclosing conditional branches, innermost last.
    branch_stack: BranchPath,
    next_cond_id: u32,
}

enum Stop {
    Eof,
    Else { cond: Option<Cond>, span: Span },
    Endif { span: Span },
}

impl Parser<'_> {
    // -----------------------------------------------------------------------
    // Block driver
    // -----------------------------------------------------------------------

    fn parse_block(&mut self, nested: bool) -> (Vec<Item>, Stop) {
        let mut items = Vec::new();
        loop {
            if self.lx.at_eof() {
                return (items, Stop::Eof);
            }

            // Recipe lines: a leading recipe prefix while a rule is open.
            if let Some(idx) = self.current_rule
                && self.lx.peek_recipe_prefix()
                && !builtins::is_conditional_directive(self.lx.peek_word())
            {
                let line = self.lx.next_line(Mode::Recipe).unwrap();
                if !line.text.trim().is_empty() {
                    let mut d = Vec::new();
                    let e = expr::parse(&line, 0, line.text.len(), &mut d);
                    self.diags.append(&mut d);
                    let branch_path = self.branch_stack.clone();
                    self.rules[idx].recipe.push(RecipeLine {
                        expr: e,
                        span: line.full_span(),
                        branch_path,
                    });
                }
                continue;
            }

            // Blank and comment-only lines never terminate a recipe.
            if self.lx.peek_blank_or_comment() {
                self.lx.next_line(Mode::Normal);
                continue;
            }

            let line = self.lx.next_line(Mode::Normal).unwrap();
            match self.parse_line(line, nested) {
                LineResult::Item(it) => {
                    // A conditional is transparent to a recipe: make keeps
                    // collecting recipe lines across `ifeq`/`endif`. Anything
                    // else terminates the recipe.
                    if !matches!(it, Item::Rule(_) | Item::Conditional(_)) {
                        self.current_rule = None;
                    }
                    items.push(it);
                }
                LineResult::Nothing => {}
                LineResult::Stop(s) => return (items, s),
            }
        }
    }

    // -----------------------------------------------------------------------
    // One logical line
    // -----------------------------------------------------------------------

    fn parse_line(&mut self, line: LogicalLine, nested: bool) -> LineResult {
        let cs = line.content_start();
        let end = line.text.len();
        if cs >= end {
            return LineResult::Nothing;
        }
        let word = word_at(&line.text, cs);

        // `else` / `endif` unwind to the enclosing conditional.
        match word {
            "endif" => {
                let span = line.full_span();
                return if nested {
                    LineResult::Stop(Stop::Endif { span })
                } else {
                    self.diags.push(Diagnostic::error(
                        "MK035",
                        span,
                        "`endif` without a matching conditional",
                    ));
                    LineResult::Nothing
                };
            }
            "else" => {
                let span = line.full_span();
                if !nested {
                    self.diags.push(Diagnostic::error(
                        "MK035",
                        span,
                        "`else` without a matching conditional",
                    ));
                    return LineResult::Nothing;
                }
                let after = cs + word.len();
                let (ts, te) = trim_range(&line.text, after, end);
                let cond = if ts < te {
                    let w = word_at(&line.text, ts);
                    if builtins::is_conditional_directive(w) && w != "else" && w != "endif" {
                        Some(self.parse_cond(&line, w, ts + w.len(), te))
                    } else {
                        self.diags.push(Diagnostic::error(
                            "MK035",
                            line.span_of(ts, te),
                            "unexpected text after `else`",
                        ));
                        None
                    }
                } else {
                    None
                };
                return LineResult::Stop(Stop::Else { cond, span });
            }
            _ => {}
        }

        // An assignment whose name *is* a directive keyword, e.g. `export = 1`.
        let sep = find_separator(&line.text, cs, end);
        let name_is_keyword = matches!(
            sep,
            Some(Sep { kind: SepKind::Assign(..), pos }) if line.text[cs..pos].trim() == word
        );

        if !name_is_keyword {
            match word {
                "include" | "-include" | "sinclude" => {
                    return LineResult::Item(self.parse_include(&line, cs, word, end));
                }
                "ifeq" | "ifneq" | "ifdef" | "ifndef" => {
                    return LineResult::Item(self.parse_conditional(
                        &line,
                        word,
                        cs + word.len(),
                        end,
                    ));
                }
                "define" => {
                    return LineResult::Item(self.parse_define(
                        &line,
                        cs + word.len(),
                        end,
                        AssignFlags::default(),
                    ));
                }
                "endef" => {
                    self.diags.push(Diagnostic::error(
                        "MK034",
                        line.full_span(),
                        "`endef` without a matching `define`",
                    ));
                    return LineResult::Nothing;
                }
                "override" | "private" | "export" | "unexport" => {
                    return self.parse_prefixed(&line, cs, end);
                }
                "undefine" => {
                    let (ts, te) = trim_range(&line.text, cs + word.len(), end);
                    let name = self.expr(&line, ts, te);
                    return LineResult::Item(Item::Undefine { name, span: line.full_span() });
                }
                "vpath" => {
                    let (ts, te) = trim_range(&line.text, cs + word.len(), end);
                    let args = self.expr(&line, ts, te);
                    return LineResult::Item(Item::Vpath { args, span: line.full_span() });
                }
                "load" | "-load" => {
                    return LineResult::Item(Item::Unparsed { span: line.full_span() });
                }
                _ => {}
            }
        }

        match sep {
            Some(Sep { kind: SepKind::Assign(op, oplen), pos }) => {
                let a =
                    self.build_assign(&line, cs, pos, oplen, op, end, None, AssignFlags::default());
                LineResult::Item(Item::Assign(a))
            }
            Some(Sep { kind: SepKind::Colon(count), pos }) => {
                LineResult::Item(self.parse_rule(&line, cs, pos, count, end))
            }
            None => LineResult::Item(self.parse_bare_line(&line, cs, end)),
        }
    }

    /// A line with no assignment operator and no rule colon. It is either a
    /// misindented recipe, or a bare expansion such as `$(error ...)`.
    fn parse_bare_line(&mut self, line: &LogicalLine, cs: usize, end: usize) -> Item {
        let misindented = line.spaces_then_tab
            || (self.current_rule.is_some() && line.leading_spaces > 0)
            || line.had_recipe_prefix;
        if !misindented && line.text.as_bytes().get(cs) == Some(&b'$') {
            let expr = self.expr(line, cs, end);
            return Item::Expression { expr, span: line.full_span() };
        }
        self.report_unparsed(line, cs, end);
        Item::Unparsed { span: line.full_span() }
    }

    fn report_unparsed(&mut self, line: &LogicalLine, cs: usize, end: usize) {
        if line.spaces_then_tab {
            self.diags.push(
                Diagnostic::error(
                    "MK025",
                    line.span_of(0, cs),
                    "recipe line indented with spaces before the tab",
                )
                .with_help("make only recognises a recipe when the tab is the first character"),
            );
        } else if self.current_rule.is_some() && line.leading_spaces > 0 {
            self.diags.push(
                Diagnostic::error(
                    "MK025",
                    line.span_of(0, cs),
                    "recipe line indented with spaces, not a tab",
                )
                .with_help("make will not treat this as part of the recipe")
                .with_fix(crate::fix::Fix {
                    span: line.span_of(0, cs),
                    replacement: "\t".to_string(),
                    description: "leading spaces to a tab".to_string(),
                }),
            );
        } else if line.had_recipe_prefix {
            self.diags.push(
                Diagnostic::error("MK026", line.span_of(0, end), "recipe line outside of any rule")
                    .with_help("make reports this as \"recipe commences before first target\""),
            );
        } else {
            self.diags.push(Diagnostic::note(
                "MK099",
                line.span_of(cs, end),
                "line could not be parsed",
            ));
        }
    }

    // -----------------------------------------------------------------------
    // Directives
    // -----------------------------------------------------------------------

    fn parse_include(&mut self, line: &LogicalLine, cs: usize, word: &str, end: usize) -> Item {
        let (ts, te) = trim_range(&line.text, cs + word.len(), end);
        let paths = self.expr(line, ts, te);
        Item::Include(Include {
            paths,
            optional: word != "include",
            span: line.full_span(),
            resolved: Vec::new(),
            missing: Vec::new(),
            dynamic: false,
            guarded: self.cond_depth > 0,
        })
    }

    fn parse_conditional(&mut self, line: &LogicalLine, word: &str, at: usize, end: usize) -> Item {
        let start_span = line.full_span();
        let mut cond = self.parse_cond(line, word, at, end);
        let mut branches = Vec::new();
        let mut else_body = None;
        let mut span = start_span;

        self.cond_depth += 1;
        let cond_id = self.next_cond_id;
        self.next_cond_id += 1;
        let mut branch_ix = 0u32;
        loop {
            self.branch_stack.push((cond_id, branch_ix));
            let (body, stop) = self.parse_block(true);
            self.branch_stack.pop();
            branch_ix += 1;
            branches.push(CondBranch { cond: cond.clone(), body, span });
            match stop {
                Stop::Endif { span: s } => {
                    span = start_span.join(s);
                    break;
                }
                Stop::Else { cond: Some(c), span: s } => {
                    cond = c;
                    span = s;
                }
                Stop::Else { cond: None, span: s } => {
                    self.branch_stack.push((cond_id, branch_ix));
                    let (body, stop) = self.parse_block(true);
                    self.branch_stack.pop();
                    else_body = Some(body);
                    if let Stop::Endif { span: e } = stop {
                        span = start_span.join(e);
                    } else {
                        self.diags.push(Diagnostic::error("MK035", s, "missing `endif`"));
                    }
                    break;
                }
                Stop::Eof => {
                    self.diags.push(Diagnostic::error("MK035", start_span, "missing `endif`"));
                    break;
                }
            }
        }
        self.cond_depth -= 1;
        Item::Conditional(Conditional { branches, else_body, span })
    }

    fn parse_cond(&mut self, line: &LogicalLine, word: &str, at: usize, end: usize) -> Cond {
        let (ts, te) = trim_range(&line.text, at, end);
        let negated = word == "ifneq" || word == "ifndef";

        if word == "ifdef" || word == "ifndef" {
            return Cond::Def { name: self.expr(line, ts, te), negated };
        }

        let text = &line.text;
        if text.as_bytes().get(ts) == Some(&b'(') {
            if let Some(cp) = find_matching_paren(text, ts, te)
                && let Some(comma) = find_top_level_byte(text, ts + 1, cp, b',')
            {
                let (ls, le) = trim_range(text, ts + 1, comma);
                let (rs, re) = trim_range(text, comma + 1, cp);
                return Cond::Eq {
                    lhs: self.expr(line, ls, le),
                    rhs: self.expr(line, rs, re),
                    negated,
                };
            }
        } else if let Some((a, b)) = split_quoted_pair(text, ts, te) {
            return Cond::Eq {
                lhs: self.expr(line, a.0, a.1),
                rhs: self.expr(line, b.0, b.1),
                negated,
            };
        }

        self.diags.push(Diagnostic::error(
            "MK036",
            line.span_of(ts, te),
            format!("malformed `{word}` condition"),
        ));
        Cond::Malformed
    }

    fn parse_define(
        &mut self,
        line: &LogicalLine,
        at: usize,
        end: usize,
        flags: AssignFlags,
    ) -> Item {
        let (ts, te) = trim_range(&line.text, at, end);
        let (name, op) = match find_separator(&line.text, ts, te) {
            Some(Sep { kind: SepKind::Assign(op, _), pos }) => {
                let (ns, ne) = trim_range(&line.text, ts, pos);
                (self.expr(line, ns, ne), op)
            }
            _ => (self.expr(line, ts, te), AssignOp::Recursive),
        };

        let (body_line, closed) = self.lx.read_define_body();
        let mut d = Vec::new();
        let body_expr = expr::parse(&body_line, 0, body_line.text.len(), &mut d);
        self.diags.append(&mut d);

        if !closed {
            self.diags.push(Diagnostic::error(
                "MK034",
                line.full_span(),
                "`define` without a matching `endef`",
            ));
        }

        Item::Define(Define {
            name,
            op,
            body: body_line.text.clone(),
            body_expr,
            body_span: body_line.full_span(),
            flags,
            span: line.full_span(),
        })
    }

    /// `override` / `private` / `export` / `unexport`, which either prefix an
    /// assignment or stand alone as a directive.
    fn parse_prefixed(&mut self, line: &LogicalLine, cs: usize, end: usize) -> LineResult {
        let mut flags = AssignFlags::default();
        let mut at = cs;
        let mut export_kind = None;

        loop {
            let (ts, _) = trim_range(&line.text, at, end);
            let w = word_at(&line.text, ts);
            match w {
                "override" => flags.is_override = true,
                "private" => flags.is_private = true,
                "export" => export_kind = Some(ExportKind::Export),
                "unexport" => export_kind = Some(ExportKind::Unexport),
                _ => break,
            }
            at = ts + w.len();
        }

        let (ts, te) = trim_range(&line.text, at, end);

        if let Some(Sep { kind: SepKind::Assign(op, oplen), pos }) =
            find_separator(&line.text, ts, te)
        {
            flags.is_export |= export_kind == Some(ExportKind::Export);
            let a = self.build_assign(line, ts, pos, oplen, op, te, None, flags);
            return LineResult::Item(Item::Assign(a));
        }

        if word_at(&line.text, ts) == "define" {
            let w = word_at(&line.text, ts);
            flags.is_export |= export_kind == Some(ExportKind::Export);
            return LineResult::Item(self.parse_define(line, ts + w.len(), te, flags));
        }

        match export_kind {
            Some(kind) => LineResult::Item(Item::Export(ExportDirective {
                kind,
                names: self.expr(line, ts, te),
                span: line.full_span(),
            })),
            None => {
                self.report_unparsed(line, cs, end);
                LineResult::Item(Item::Unparsed { span: line.full_span() })
            }
        }
    }

    // -----------------------------------------------------------------------
    // Assignments and rules
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn build_assign(
        &mut self,
        line: &LogicalLine,
        name_start: usize,
        op_pos: usize,
        op_len: usize,
        op: AssignOp,
        end: usize,
        target: Option<Expr>,
        flags: AssignFlags,
    ) -> Assign {
        let (ns, ne) = trim_range(&line.text, name_start, op_pos);
        let (vs, ve) = trim_range(&line.text, op_pos + op_len, end);
        let name = self.expr(line, ns, ne);
        let value = self.expr(line, vs, ve);

        // Honour `.RECIPEPREFIX` so later recipe lines lex correctly.
        if target.is_none()
            && name.literal().as_deref() == Some(".RECIPEPREFIX")
            && !op.is_deferred()
        {
            let v = value.literal().unwrap_or_default();
            self.lx.recipe_prefix = v.bytes().next().unwrap_or(b'\t');
        }

        Assign {
            name,
            op,
            op_span: line.span_of(op_pos, op_pos + op_len),
            value,
            flags,
            target,
            span: line.full_span(),
        }
    }

    fn parse_rule(
        &mut self,
        line: &LogicalLine,
        cs: usize,
        colon: usize,
        colon_count: usize,
        end: usize,
    ) -> Item {
        let text = &line.text;
        let after = colon + colon_count;

        // An inline recipe terminates the header: `target: prereq; cmd`.
        let semi = find_top_level_byte(text, after, end, b';');
        let header_end = semi.unwrap_or(end);

        let (ts, te) = trim_range(text, cs, colon);
        let targets = self.expr(line, ts, te);

        let mut kind = if colon_count == 2 { RuleKind::Double } else { RuleKind::Normal };
        let mut prereq_start = after;

        match find_separator(text, after, header_end) {
            // `targets: VAR = value` — a target-specific variable.
            Some(Sep { kind: SepKind::Assign(op, oplen), pos }) => {
                let a = self.build_assign(
                    line,
                    after,
                    pos,
                    oplen,
                    op,
                    header_end,
                    Some(targets),
                    AssignFlags::default(),
                );
                return Item::Assign(a);
            }
            // `targets: target-pattern: prereq-patterns`
            Some(Sep { kind: SepKind::Colon(_), pos }) => {
                let (ps, pe) = trim_range(text, after, pos);
                kind = RuleKind::Static { target_pattern: self.expr(line, ps, pe) };
                prereq_start = pos + 1;
            }
            None => {}
        }

        let (ps, pe) = trim_range(text, prereq_start, header_end);
        let (prereqs, order_only) = match find_top_level_byte(text, ps, pe, b'|') {
            Some(bar) => {
                let (a, b) = trim_range(text, ps, bar);
                let (c, d) = trim_range(text, bar + 1, pe);
                (self.expr(line, a, b), Some(self.expr(line, c, d)))
            }
            None => (self.expr(line, ps, pe), None),
        };

        let mut recipe = Vec::new();
        if let Some(semi) = semi {
            let (rs, re) = trim_range(text, semi + 1, end);
            if rs < re {
                recipe.push(RecipeLine {
                    expr: self.expr(line, rs, re),
                    span: line.span_of(rs, re),
                    branch_path: self.branch_stack.clone(),
                });
            }
        }

        self.rules.push(Rule {
            targets,
            kind,
            prereqs,
            order_only,
            recipe,
            span: line.full_span(),
            branch_path: self.branch_stack.clone(),
        });
        let idx = self.rules.len() - 1;
        self.current_rule = Some(idx);
        Item::Rule(idx)
    }

    fn expr(&mut self, line: &LogicalLine, start: usize, end: usize) -> Expr {
        let mut d = Vec::new();
        let e = expr::parse(line, start, end, &mut d);
        self.diags.append(&mut d);
        e
    }
}

enum LineResult {
    Item(Item),
    Nothing,
    Stop(Stop),
}

// ---------------------------------------------------------------------------
// Top-level scanning
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug)]
struct Sep {
    pos: usize,
    kind: SepKind,
}

#[derive(Copy, Clone, Debug)]
enum SepKind {
    /// Operator and its byte length.
    Assign(AssignOp, usize),
    /// Number of colons (1 or 2).
    Colon(usize),
}

/// Advance past `$(...)`, `${...}`, `$$`, `$X` and `\<char>` escapes.
/// Returns `None` when position `i` is an ordinary top-level byte.
fn skip_opaque(text: &str, i: usize, end: usize) -> Option<usize> {
    let b = text.as_bytes();
    match b[i] {
        b'$' if i + 1 < end => match b[i + 1] {
            b'$' => Some(i + 2),
            b'(' => Some(skip_ref(text, i + 2, end, b'(', b')')),
            b'{' => Some(skip_ref(text, i + 2, end, b'{', b'}')),
            _ => Some(next_boundary(text, i + 2, end)),
        },
        b'\\' if i + 1 < end => Some(next_boundary(text, i + 2, end)),
        _ => None,
    }
}

fn skip_ref(text: &str, from: usize, end: usize, open: u8, close: u8) -> usize {
    let b = text.as_bytes();
    let mut depth = 1usize;
    let mut i = from;
    while i < end {
        if b[i] == b'$' && i + 1 < end && b[i + 1] == b'$' {
            i += 2;
            continue;
        }
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i = next_boundary(text, i + 1, end);
    }
    end
}

fn next_boundary(text: &str, mut i: usize, end: usize) -> usize {
    i = i.min(end);
    while i < end && !text.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// First top-level assignment operator or rule colon in `from..end`.
fn find_separator(text: &str, from: usize, end: usize) -> Option<Sep> {
    let mut i = from;
    while i < end {
        if let Some(j) = skip_opaque(text, i, end) {
            i = j;
            continue;
        }
        let rest = &text[i..end];
        let kind = if rest.starts_with(":::=") {
            Some(SepKind::Assign(AssignOp::Immediate, 4))
        } else if rest.starts_with("::=") {
            Some(SepKind::Assign(AssignOp::SimplePosix, 3))
        } else if rest.starts_with(":=") {
            Some(SepKind::Assign(AssignOp::Simple, 2))
        } else if rest.starts_with("+=") {
            Some(SepKind::Assign(AssignOp::Append, 2))
        } else if rest.starts_with("?=") {
            Some(SepKind::Assign(AssignOp::Conditional, 2))
        } else if rest.starts_with("!=") {
            Some(SepKind::Assign(AssignOp::Shell, 2))
        } else if rest.starts_with('=') {
            Some(SepKind::Assign(AssignOp::Recursive, 1))
        } else if rest.starts_with("::") {
            Some(SepKind::Colon(2))
        } else if rest.starts_with(':') {
            Some(SepKind::Colon(1))
        } else {
            None
        };
        if let Some(kind) = kind {
            return Some(Sep { pos: i, kind });
        }
        i = next_boundary(text, i + 1, end);
    }
    None
}

fn find_top_level_byte(text: &str, from: usize, end: usize, needle: u8) -> Option<usize> {
    let b = text.as_bytes();
    let mut i = from;
    while i < end {
        if let Some(j) = skip_opaque(text, i, end) {
            i = j;
            continue;
        }
        if b[i] == needle {
            return Some(i);
        }
        i = next_boundary(text, i + 1, end);
    }
    None
}

fn find_matching_paren(text: &str, open_pos: usize, end: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0i32;
    let mut i = open_pos;
    while i < end {
        if b[i] == b'$' && i + 1 < end && b[i + 1] == b'$' {
            i += 2;
            continue;
        }
        match b[i] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i = next_boundary(text, i + 1, end);
    }
    None
}

/// `ifeq "a" "b"` / `ifeq 'a' 'b'`.
fn split_quoted_pair(
    text: &str,
    start: usize,
    end: usize,
) -> Option<((usize, usize), (usize, usize))> {
    let b = text.as_bytes();
    let q1 = *b.get(start)?;
    if q1 != b'"' && q1 != b'\'' {
        return None;
    }
    let e1 = start + 1 + text[start + 1..end].find(q1 as char)?;
    let (s2, _) = trim_range(text, e1 + 1, end);
    let q2 = *b.get(s2)?;
    if q2 != b'"' && q2 != b'\'' {
        return None;
    }
    let e2 = s2 + 1 + text[s2 + 1..end].find(q2 as char)?;
    Some(((start + 1, e1), (s2 + 1, e2)))
}

fn trim_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let s = &text[start..end];
    let a = start + (s.len() - s.trim_start().len());
    let b = end - (s.len() - s.trim_end().len());
    (a, b.max(a))
}

fn word_at(text: &str, at: usize) -> &str {
    let s = &text[at..];
    let n = s.find(|c: char| c.is_whitespace()).unwrap_or(s.len());
    &s[..n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceMap;
    use std::path::PathBuf;

    fn p(src: &str) -> (Makefile, Vec<Diagnostic>, SourceMap) {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("Makefile"), src.to_string());
        let text = sm.get(id).text.clone();
        let (mf, d) = parse(id, &text);
        (mf, d, sm)
    }

    #[test]
    fn simple_assignment() {
        let (mf, d, _) = p("CC := gcc\n");
        assert!(d.is_empty(), "{d:?}");
        let Item::Assign(a) = &mf.items[0] else { panic!() };
        assert_eq!(a.name.literal().unwrap(), "CC");
        assert_eq!(a.op, AssignOp::Simple);
        assert_eq!(a.value.literal().unwrap(), "gcc");
    }

    #[test]
    fn all_assignment_operators() {
        let src = "A = 1\nB := 2\nC ::= 3\nD :::= 4\nE += 5\nF ?= 6\nG != echo 7\n";
        let (mf, d, _) = p(src);
        assert!(d.is_empty(), "{d:?}");
        let ops: Vec<_> = mf
            .items
            .iter()
            .map(|i| match i {
                Item::Assign(a) => a.op,
                _ => panic!("{i:?}"),
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                AssignOp::Recursive,
                AssignOp::Simple,
                AssignOp::SimplePosix,
                AssignOp::Immediate,
                AssignOp::Append,
                AssignOp::Conditional,
                AssignOp::Shell,
            ]
        );
    }

    #[test]
    fn rule_with_recipe() {
        let (mf, d, _) = p("all: a b\n\techo hi\n\techo bye\n");
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(mf.rules.len(), 1);
        let r = &mf.rules[0];
        assert_eq!(r.targets.literal().unwrap(), "all");
        assert_eq!(r.prereqs.literal().unwrap(), "a b");
        assert_eq!(r.recipe.len(), 2);
    }

    #[test]
    fn inline_recipe_and_order_only() {
        let (mf, d, _) = p("all: a | b ; echo hi\n");
        assert!(d.is_empty(), "{d:?}");
        let r = &mf.rules[0];
        assert_eq!(r.prereqs.literal().unwrap(), "a");
        assert_eq!(r.order_only.as_ref().unwrap().literal().unwrap(), "b");
        assert_eq!(r.recipe.len(), 1);
    }

    #[test]
    fn double_colon_rule() {
        let (mf, _, _) = p("all:: a\n\techo\n");
        assert!(matches!(mf.rules[0].kind, RuleKind::Double));
    }

    #[test]
    fn static_pattern_rule() {
        let (mf, d, _) = p("$(OBJS): %.o: %.c\n\techo\n");
        assert!(d.is_empty(), "{d:?}");
        let RuleKind::Static { target_pattern } = &mf.rules[0].kind else { panic!() };
        assert_eq!(target_pattern.literal().unwrap(), "%.o");
        assert_eq!(mf.rules[0].prereqs.literal().unwrap(), "%.c");
    }

    #[test]
    fn target_specific_variable() {
        let (mf, d, _) = p("debug: CFLAGS += -g\n");
        assert!(d.is_empty(), "{d:?}");
        let Item::Assign(a) = &mf.items[0] else { panic!("{:?}", mf.items[0]) };
        assert_eq!(a.target.as_ref().unwrap().literal().unwrap(), "debug");
        assert_eq!(a.name.literal().unwrap(), "CFLAGS");
        assert_eq!(a.op, AssignOp::Append);
    }

    #[test]
    fn conditional_with_else_if() {
        let src =
            "ifeq ($(OS),Linux)\nA = 1\nelse ifeq ($(OS),Darwin)\nA = 2\nelse\nA = 3\nendif\n";
        let (mf, d, _) = p(src);
        assert!(d.is_empty(), "{d:?}");
        let Item::Conditional(c) = &mf.items[0] else { panic!() };
        assert_eq!(c.branches.len(), 2);
        assert_eq!(c.else_body.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn conditional_does_not_orphan_recipe_lines() {
        let src = "all:\n\techo a\nifeq ($(X),1)\n\techo b\nendif\n\techo c\n";
        let (mf, d, _) = p(src);
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(mf.rules.len(), 1);
        assert_eq!(mf.rules[0].recipe.len(), 3);
    }

    #[test]
    fn ifdef_and_quoted_ifeq() {
        let (mf, d, _) = p("ifdef FOO\nA=1\nendif\nifeq \"x\" \"y\"\nB=2\nendif\n");
        assert!(d.is_empty(), "{d:?}");
        let Item::Conditional(c) = &mf.items[0] else { panic!() };
        assert!(matches!(c.branches[0].cond, Cond::Def { negated: false, .. }));
        let Item::Conditional(c) = &mf.items[1] else { panic!() };
        let Cond::Eq { lhs, rhs, .. } = &c.branches[0].cond else { panic!() };
        assert_eq!(lhs.literal().unwrap(), "x");
        assert_eq!(rhs.literal().unwrap(), "y");
    }

    #[test]
    fn define_block() {
        let (mf, d, _) = p("define greet\necho hello\necho world\nendef\n");
        assert!(d.is_empty(), "{d:?}");
        let Item::Define(def) = &mf.items[0] else { panic!() };
        assert_eq!(def.name.literal().unwrap(), "greet");
        assert_eq!(def.body, "echo hello\necho world");
        assert!(def.body_expr.literal().is_some());
    }

    #[test]
    fn export_prefix_vs_directive() {
        let (mf, d, _) = p("export PATH\nexport FOO = 1\nexport = 2\noverride BAR := 3\n");
        assert!(d.is_empty(), "{d:?}");
        assert!(matches!(&mf.items[0], Item::Export(_)));
        let Item::Assign(a) = &mf.items[1] else { panic!() };
        assert!(a.flags.is_export);
        let Item::Assign(a) = &mf.items[2] else { panic!("{:?}", mf.items[2]) };
        assert_eq!(a.name.literal().unwrap(), "export");
        let Item::Assign(a) = &mf.items[3] else { panic!() };
        assert!(a.flags.is_override);
    }

    #[test]
    fn variable_reference_in_target_is_not_a_separator() {
        let (mf, d, _) = p("$(BIN)/x: y\n\techo\n");
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(mf.rules.len(), 1);
    }

    #[test]
    fn spaces_instead_of_tab_is_reported() {
        let (_, d, _) = p("all:\n    echo hi\n");
        assert!(d.iter().any(|x| x.code == "MK025"), "{d:?}");
    }

    #[test]
    fn recipe_before_first_target_is_reported() {
        let (_, d, _) = p("\techo hi\n");
        assert!(d.iter().any(|x| x.code == "MK026"), "{d:?}");
    }

    #[test]
    fn unmatched_endif_is_reported() {
        let (_, d, _) = p("endif\n");
        assert!(d.iter().any(|x| x.code == "MK035"), "{d:?}");
    }

    #[test]
    fn include_directives() {
        let (mf, d, _) = p("include a.mk\n-include b.mk\nsinclude c.mk\n");
        assert!(d.is_empty(), "{d:?}");
        let opts: Vec<bool> = mf
            .items
            .iter()
            .map(|i| match i {
                Item::Include(inc) => inc.optional,
                _ => panic!(),
            })
            .collect();
        assert_eq!(opts, vec![false, true, true]);
    }

    #[test]
    fn recipe_prefix_directive_is_honoured() {
        let (mf, d, _) = p(".RECIPEPREFIX := >\nall:\n> echo hi\n");
        assert!(d.is_empty(), "{d:?}");
        assert_eq!(mf.rules[0].recipe.len(), 1);
    }
}
