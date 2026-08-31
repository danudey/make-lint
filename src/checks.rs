//! Phase 1 checks: everything derivable from syntax alone.
//!
//! Nothing here expands a variable or runs a command. Checks that need values
//! (undefined/redefined variables, duplicate values, unreachable prerequisites)
//! arrive with the evaluator.

use crate::ast::*;
use crate::builtins;
use crate::diag::Diagnostic;
use crate::workspace::Workspace;

use std::collections::{HashMap, HashSet};

pub fn run(ws: &Workspace) -> Vec<Diagnostic> {
    let index = Index::build(ws);
    let mut out = Vec::new();
    for mf in ws.makefiles() {
        for_each_expr(mf, &mut |e| {
            e.visit(&mut |p| match p {
                Piece::Var(v) => {
                    mk010_bare_reference(v, &mut out);
                    mk031_suspicious_name(v, &index, &mut out);
                }
                Piece::Func(c) => {
                    mk030_undefined_macro(c, &index, &mut out);
                    mk032_argument_count(c, &mut out);
                }
                _ => {}
            });
        });
        mk050_missing_include(mf, &index, &mut out);
    }
    mk020_duplicate_recipe(ws, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// Names defined anywhere in the workspace. Conditional branches are unioned:
/// over-approximating definedness means fewer false "undefined" reports, which
/// is the right trade for a linter.
struct Index {
    defined: HashSet<String>,
    /// Every literal rule target in the workspace. An `include` of one of these
    /// is make's "remaking makefiles" feature, not a missing file.
    targets: HashSet<String>,
    /// False when some `include` could not be resolved, so the set of defined
    /// names is incomplete and "undefined" cannot be claimed.
    complete: bool,
}

impl Index {
    fn build(ws: &Workspace) -> Index {
        let mut defined = HashSet::new();
        let mut targets = HashSet::new();
        let mut complete = true;
        for mf in ws.makefiles() {
            for rule in &mf.rules {
                if let Some(list) = rule.targets.literal() {
                    targets.extend(list.split_whitespace().map(str::to_string));
                }
            }
            for_each_expr(mf, &mut |e| {
                e.visit(&mut |p| {
                    let Piece::Func(c) = p else { return };
                    match c.name.as_str() {
                        // `$(foreach file,...)` and `$(let a b,...)` bind names
                        // that are real variables inside the body, so `$(file)`
                        // there is legitimate.
                        "foreach" | "let" => {
                            if let Some(names) = c.args.first().and_then(|a| a.literal()) {
                                defined.extend(names.split_whitespace().map(str::to_string));
                            }
                        }
                        // `$(eval dir := $(3))` defines a variable at expansion
                        // time. The leading literal is enough to read the name.
                        "eval" => {
                            if let Some(a) = c.args.first() {
                                defined.extend(assigned_names(&a.leading_literal()));
                            }
                        }
                        _ => {}
                    }
                });
            });
            mf.walk_items(&mut |it| match it {
                Item::Assign(a) => {
                    if let Some(n) = a.name.literal() {
                        defined.insert(n.trim().to_string());
                    }
                }
                Item::Define(d) => {
                    if let Some(n) = d.name.literal() {
                        defined.insert(n.trim().to_string());
                    }
                }
                Item::Include(inc) => {
                    if inc.dynamic || !inc.missing.is_empty() {
                        complete = false;
                    }
                }
                _ => {}
            });
        }
        Index { defined, targets, complete }
    }
}

/// Variable names assigned by the statements in `text`, which is the literal
/// prefix of an `$(eval ...)` body. Anything that is not a plain name before an
/// assignment operator is ignored.
fn assigned_names(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let eq = line.find('=')?;
            let name = line[..eq].trim_end_matches([':', '+', '?', '!']).trim();
            let plain = !name.is_empty()
                && !name.contains(|c: char| c.is_whitespace() || "$(){}:".contains(c));
            plain.then(|| name.to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------------
// MK010 — `$FOO` is `$(F)` followed by literal text
// ---------------------------------------------------------------------------

fn mk010_bare_reference(v: &VarRef, out: &mut Vec<Diagnostic>) {
    if v.style != RefStyle::Bare {
        return;
    }
    let Some(run) = &v.bare_run_on else { return };
    let Some(name) = v.name.literal() else { return };
    if builtins::is_automatic(&name) {
        return;
    }
    out.push(
        Diagnostic::warn(
            "MK010",
            v.span,
            format!("`${name}` refers to the one-character variable `{name}`, then the literal text `{run}`"),
        )
        .with_help(format!("write `$({name}{run})` for the variable, or `$${name}{run}` for a shell variable")),
    );
}

// ---------------------------------------------------------------------------
// MK020 — two recipes for the same target
// ---------------------------------------------------------------------------

fn mk020_duplicate_recipe(ws: &Workspace, out: &mut Vec<Diagnostic>) {
    struct Prev {
        file: crate::span::FileId,
        span: crate::span::Span,
        path: BranchPath,
    }
    let mut seen: HashMap<String, Vec<Prev>> = HashMap::new();

    for (file, idx) in ws.rules_in_read_order() {
        let Some(mf) = ws.get(file) else { continue };
        {
            let rule = &mf.rules[idx];
            // `::` rules are explicitly allowed to have several recipes, and a
            // recipe-less rule only adds prerequisites.
            // A rule whose every recipe line sits inside a conditional may end
            // up with no recipe at all, so it neither clashes nor is clashed with.
            if matches!(rule.kind, RuleKind::Double) || !rule.has_unconditional_recipe() {
                continue;
            }
            let Some(list) = rule.targets.literal() else { continue };
            for target in list.split_whitespace() {
                // Pattern rules legitimately overlap.
                if target.contains('%') {
                    continue;
                }
                let prevs = seen.entry(target.to_string()).or_default();
                // Two definitions in opposite branches of one conditional are
                // the normal way to vary a recipe; make only ever sees one.
                let clash = prevs
                    .iter()
                    .find(|p| p.file != file || !mutually_exclusive(&p.path, &rule.branch_path));
                if let Some(first) = clash {
                    out.push(
                        Diagnostic::warn(
                            "MK020",
                            rule.span,
                            format!("target `{target}` already has a recipe"),
                        )
                        .with_label(first.span, "first recipe defined here")
                        .with_help("make uses the last recipe and discards the earlier one; use `::` if both should run"),
                    );
                }
                prevs.push(Prev { file, span: rule.span, path: rule.branch_path.clone() });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// MK030 — `$(call f,...)` where `f` is never defined
// ---------------------------------------------------------------------------

fn mk030_undefined_macro(c: &FuncCall, index: &Index, out: &mut Vec<Diagnostic>) {
    // An unreadable include may hold the definition.
    if c.name != "call" || !index.complete {
        return;
    }
    let Some(first) = c.args.first() else { return };
    let Some(name) = first.literal() else { return };
    let name = name.trim();
    if name.is_empty() || index.defined.contains(name) || builtins::function(name).is_some() {
        return;
    }
    out.push(
        Diagnostic::warn("MK030", first.span, format!("`$(call {name},...)` names an undefined macro"))
            .with_help("make expands an undefined macro to the empty string; `$(eval)`-defined macros are not visible to this check"),
    );
}

// ---------------------------------------------------------------------------
// MK031 — variable name that is really a mistyped function call
// ---------------------------------------------------------------------------

/// Shortest name a typo suggestion is allowed to be based on. Below this,
/// edit distance 2 matches almost every short function name and the suggestion
/// is worse than none.
const MIN_TYPO_LEN: usize = 4;

fn mk031_suspicious_name(v: &VarRef, index: &Index, out: &mut Vec<Diagnostic>) {
    if v.style == RefStyle::Bare {
        return;
    }
    let full = v.name.literal();

    // `$(strip)` with no argument: the name alone is a function name.
    if let Some(name) = &full
        && let Some(f) = builtins::function(name.trim())
        && f.min_args >= 1
        // ...unless something really does define a variable by that name.
        && !index.defined.contains(name.trim())
    {
        out.push(
            Diagnostic::warn(
                "MK031",
                v.span,
                format!(
                    "`$({name})` is a variable reference, not a call to the `{}` function",
                    f.name
                ),
            )
            .with_help(format!("`{}` needs {} argument(s) after a space", f.name, f.min_args)),
        );
        return;
    }

    // The name only needs a space in its *leading literal* to be suspect; the
    // rest may contain nested references, as in `$(pastsubst %.c,%.o,$(SRCS))`.
    let lead = v.name.leading_literal();
    if !lead.contains(char::is_whitespace) {
        return;
    }
    let head = lead.split_whitespace().next().unwrap_or("");
    let shown = full.unwrap_or_else(|| format!("{}...", lead.trim_end()));

    match builtins::nearest_function(head, 2) {
        Some(f) if f != head && head.len() >= MIN_TYPO_LEN => out.push(
            Diagnostic::warn(
                "MK031",
                v.span,
                format!("`{head}` is not a make function, so `$({shown})` expands to the empty string"),
            )
            .with_help(format!("did you mean `$({f} ...)`?")),
        ),
        _ => out.push(
            Diagnostic::warn(
                "MK031",
                v.span,
                format!("`$({shown})` names a variable containing a space, so make expands it to the empty string"),
            )
            .with_help("a name with spaces cannot be assigned with `=`; for a shell command substitution in a recipe write `$$(...)`"),
        ),
    }
}

// ---------------------------------------------------------------------------
// MK032 — too few arguments to a built-in function
// ---------------------------------------------------------------------------

fn mk032_argument_count(c: &FuncCall, out: &mut Vec<Diagnostic>) {
    let Some(f) = builtins::function(&c.name) else { return };
    let n = c.args.len();
    if n >= f.min_args {
        return;
    }
    out.push(Diagnostic::error(
        "MK032",
        c.span,
        format!(
            "`$({})` takes at least {} argument{}, found {n}",
            f.name,
            f.min_args,
            if f.min_args == 1 { "" } else { "s" }
        ),
    ));
}

// ---------------------------------------------------------------------------
// MK050 — non-optional include that does not resolve
// ---------------------------------------------------------------------------

fn mk050_missing_include(mf: &Makefile, index: &Index, out: &mut Vec<Diagnostic>) {
    mf.walk_items(&mut |it| {
        let Item::Include(inc) = it else { return };
        // A guarded include is the other half of an `ifeq`; the branch that is
        // actually taken supplies the file.
        if inc.optional || inc.guarded {
            return;
        }
        for m in &inc.missing {
            // make remakes a makefile that is itself a target, then restarts.
            if index.targets.contains(m.trim_start_matches("./")) {
                continue;
            }
            out.push(
                Diagnostic::warn("MK050", inc.span, format!("included file `{m}` was not found"))
                    .with_help("if it is generated by a rule, use `-include` or ignore this"),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Expression traversal
// ---------------------------------------------------------------------------

/// Every expression in a makefile, including those inside conditionals,
/// recipes, and `define` bodies.
fn for_each_expr(mf: &Makefile, f: &mut impl FnMut(&Expr)) {
    mf.walk_items(&mut |it| match it {
        Item::Assign(a) => {
            if let Some(t) = &a.target {
                f(t);
            }
            f(&a.name);
            f(&a.value);
        }
        Item::Include(i) => f(&i.paths),
        Item::Define(d) => {
            f(&d.name);
            f(&d.body_expr);
        }
        Item::Export(e) => f(&e.names),
        Item::Undefine { name, .. } => f(name),
        Item::Expression { expr, .. } => f(expr),
        Item::Vpath { args, .. } => f(args),
        Item::Conditional(c) => {
            for b in &c.branches {
                match &b.cond {
                    Cond::Eq { lhs, rhs, .. } => {
                        f(lhs);
                        f(rhs);
                    }
                    Cond::Def { name, .. } => f(name),
                    Cond::Malformed => {}
                }
            }
        }
        Item::Rule(_) | Item::Unparsed { .. } => {}
    });

    for r in &mf.rules {
        f(&r.targets);
        f(&r.prereqs);
        if let Some(o) = &r.order_only {
            f(o);
        }
        if let RuleKind::Static { target_pattern } = &r.kind {
            f(target_pattern);
        }
        for line in &r.recipe {
            f(&line.expr);
        }
    }
}
