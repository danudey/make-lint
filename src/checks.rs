//! The checks.
//!
//! Syntax-only checks read the tree directly. Value checks read the evaluator's
//! [`Analysis`], and every one of them stops at an `Unknown`: a finding is only
//! reported when the evaluator actually knew the answer.

use crate::ast::*;
use crate::builtins;
use crate::diag::{Diagnostic, Severity};
use crate::eval::{self, Analysis};
use crate::funcs;
use crate::span::Span;
use crate::value::Stability;
use crate::workspace::Workspace;

use std::collections::{BTreeMap, HashMap, HashSet};

pub fn run(ws: &Workspace) -> Vec<Diagnostic> {
    run_with(ws, &eval::Options::default())
}

pub fn run_with(ws: &Workspace, opts: &eval::Options) -> Vec<Diagnostic> {
    let index = Index::build(ws);
    let analysis = eval::analyse_with(ws, opts);
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

    mk001_undefined_variable(&analysis, &index, &mut out);
    mk002_unused_variable(ws, &analysis, &index, &mut out);
    mk003_clobbered_value(&analysis, &index, &mut out);
    mk004_append_before_definition(&analysis, &index, &mut out);
    mk005_deferred_shell(ws, &index, &mut out);
    mk009_self_reference(&analysis, &mut out);
    mk021_unmakeable_prerequisite(ws, &analysis, &index, &mut out);

    if index.generated {
        for &root in &ws.roots {
            out.push(
                Diagnostic::new(
                    "MK098",
                    Severity::Note,
                    crate::span::Span::new(root, 0, 0),
                    "this file looks generated, so advice about how it is written is suppressed",
                )
                .with_help(
                    "syntax and correctness checks still ran; lint the source it is generated from",
                ),
            );
        }
    }

    let duplicated = mk006_duplicate_value(ws, &analysis, &index, &mut out);
    mk007_alias(ws, &analysis, &index, &mut out);
    mk008_same_expression(ws, &analysis, &index, &duplicated, &mut out);
    mk040_command_not_run(&analysis, &mut out);
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
    /// Targets containing `%`, which can stand in for many names.
    pattern_targets: Vec<String>,
    /// Every name read anywhere, however it is read.
    referenced: HashSet<String>,
    /// How many times each name is read, for judging repeated expansion.
    reference_counts: HashMap<String, usize>,
    /// `vpath` or `VPATH` is in play, so a prerequisite may live elsewhere.
    has_vpath: bool,
    /// The linted file announces itself as generated, so advice about how it is
    /// written has nowhere to go.
    generated: bool,
    /// False when some `include` could not be resolved, so the set of defined
    /// names is incomplete and "undefined" cannot be claimed.
    complete: bool,
}

impl Index {
    fn build(ws: &Workspace) -> Index {
        let mut defined = HashSet::new();
        let mut targets = HashSet::new();
        let mut pattern_targets = Vec::new();
        let mut referenced: HashSet<String> = HashSet::new();
        let mut reference_counts: HashMap<String, usize> = HashMap::new();
        let mut has_vpath = false;
        let mut complete = true;
        for mf in ws.makefiles() {
            for rule in &mf.rules {
                if let Some(list) = rule.targets.literal() {
                    for t in list.split_whitespace() {
                        if t.contains('%') {
                            pattern_targets.push(t.to_string());
                        } else {
                            targets.insert(t.to_string());
                        }
                    }
                }
                if let RuleKind::Static { target_pattern } = &rule.kind
                    && let Some(p) = target_pattern.literal()
                {
                    pattern_targets.push(p.trim().to_string());
                }
            }
            for_each_expr(mf, &mut |e| {
                e.visit(&mut |p| {
                    if let Piece::Var(v) = p
                        && let Some(n) = v.name.literal()
                    {
                        let n = n.trim().to_string();
                        *reference_counts.entry(n.clone()).or_default() += 1;
                        referenced.insert(n);
                    }
                    let Piece::Func(c) = p else { return };
                    // `$(call f,..)`, `$(origin X)` and friends read a name
                    // without writing `$(X)`.
                    if matches!(c.name.as_str(), "call" | "origin" | "value" | "flavor")
                        && let Some(n) = c.args.first().and_then(|a| a.literal())
                    {
                        let n = n.trim().to_string();
                        *reference_counts.entry(n.clone()).or_default() += 1;
                        referenced.insert(n);
                    }
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
                Item::Vpath { .. } => has_vpath = true,
                _ => {}
            });
        }
        has_vpath |= defined.contains("VPATH");
        let generated = ws.roots.iter().any(|&r| looks_generated(&ws.sources.get(r).text));
        Index {
            defined,
            targets,
            pattern_targets,
            referenced,
            reference_counts,
            has_vpath,
            generated,
            complete,
        }
    }
}

/// How many leading lines are searched for a generator's banner.
const BANNER_LINES: usize = 20;

/// True when a makefile says in its own header that a tool wrote it. Advising
/// someone to restructure generated output is useless; the generator decides.
fn looks_generated(text: &str) -> bool {
    text.lines().take(BANNER_LINES).filter(|l| l.trim_start().starts_with('#')).any(|l| {
        let l = l.to_ascii_lowercase();
        l.contains("generated by")
            || l.contains("generated automatically")
            || l.contains("automatically generated")
            || l.contains("generated from")
            || l.contains("do not edit")
    })
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
    // The run-on characters sit immediately after the reference, so one span
    // covers `$FOO` and one replacement fixes it.
    let whole = Span::new(v.span.file, v.span.start as usize, v.span.end as usize + run.len());
    out.push(
        Diagnostic::warn(
            "MK010",
            v.span,
            format!("`${name}` refers to the one-character variable `{name}`, then the literal text `{run}`"),
        )
        .with_help(format!("write `$({name}{run})` for the variable, or `$${name}{run}` for a shell variable"))
        .with_fix(crate::fix::Fix {
            span: whole,
            replacement: format!("$({name}{run})"),
            description: format!("`${name}{run}` to `$({name}{run})`"),
        }),
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
    match nearest_defined(name, index) {
        Some(similar) => out.push(
            Diagnostic::warn(
                "MK030",
                first.span,
                format!("`$(call {name},...)` names an undefined macro; did you mean `{similar}`?"),
            )
            .with_help(
                "make expands an undefined macro to the empty string, so the mistake is silent",
            ),
        ),
        None => out.push(
            Diagnostic::new(
                "MK030",
                Severity::Note,
                first.span,
                format!("`$(call {name},...)` names a macro this workspace never defines"),
            )
            .with_help(
                "a makefile meant to be included by another may get the macro from its parent",
            ),
        ),
    }
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

// ---------------------------------------------------------------------------
// MK001 — a variable that is read but never assigned
// ---------------------------------------------------------------------------

fn mk001_undefined_variable(a: &Analysis, index: &Index, out: &mut Vec<Diagnostic>) {
    // An unreadable include, or a conditional we gave up on, may hold the
    // assignment.
    if !index.complete || index.generated || a.degraded {
        return;
    }
    let mut reported: HashSet<&str> = HashSet::new();
    for r in &a.undefined {
        // `$(eval)` and `$(foreach)` bindings are only visible syntactically.
        if index.defined.contains(&r.name) || !reported.insert(&r.name) {
            continue;
        }
        // A name with a space in it is a mistyped function call, which MK031
        // already reports with a better message.
        if r.name.contains(char::is_whitespace) {
            continue;
        }
        let count = a.undefined.iter().filter(|o| o.name == r.name).count();
        let times = match count {
            1 => String::new(),
            n => format!(" (read {n} times)"),
        };

        // Make has no way to declare "this comes from the environment", so an
        // unassigned name is only *suspicious*, not wrong. What is nearly
        // always wrong is a near-miss of a name the makefile does define.
        match nearest_defined(&r.name, index) {
            Some(similar) => out.push(
                Diagnostic::warn(
                    "MK001",
                    r.span,
                    format!("`{}` is never assigned; did you mean `{similar}`?", r.name),
                )
                .with_help("make expands an unset variable to the empty string, so the mistake is silent"),
            ),
            None => out.push(
                Diagnostic::new(
                    "MK001",
                    Severity::Note,
                    r.span,
                    format!("`{}` is read but never assigned{times}", r.name),
                )
                .with_help("this is fine if it comes from the environment or the command line; `?=` would make that explicit"),
            ),
        }
    }
}

/// A defined name close enough to `name` to be the one that was meant.
///
/// Similarity alone is far too weak in a makefile: `BUILD_IMAGE` and
/// `BUILD_IMAGES`, or `X_C_FILES` and `X_O_FILES`, are deliberate pairs, not
/// misspellings. The signature of a real typo is that *both halves* are
/// visible: one name is read and never assigned, and its near-twin is assigned
/// and never read. Anything the makefile actually uses is a real variable.
fn nearest_defined(name: &str, index: &Index) -> Option<String> {
    if name.len() < 4 {
        return None;
    }
    let max = if name.len() >= 8 { 2 } else { 1 };
    index
        .defined
        .iter()
        .filter(|d| d.as_str() != name)
        .filter(|d| !index.referenced.contains(*d))
        .filter(|d| !same_numbered_family(name, d) && !plural_of(name, d))
        .filter_map(|d| builtins::edit_distance(name, d, max).map(|dist| (dist, d)))
        .min_by_key(|&(dist, d)| (dist, d.clone()))
        .map(|(_, d)| d.clone())
}

/// True when one name is the other with a trailing `s`. A list and its element
/// are two variables, not a mistake.
fn plural_of(a: &str, b: &str) -> bool {
    let pair =
        |x: &str, y: &str| x.len() + 1 == y.len() && y.starts_with(x) && y.ends_with(['s', 'S']);
    pair(a, b) || pair(b, a)
}

/// True when two names differ only in their digits, as `am__append_1` and
/// `am__append_2` do. Those are a deliberate family, not a misspelling.
fn same_numbered_family(a: &str, b: &str) -> bool {
    let strip = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    strip(a) == strip(b)
}

// ---------------------------------------------------------------------------
// MK002 — a variable that is assigned but never read
// ---------------------------------------------------------------------------

fn mk002_unused_variable(ws: &Workspace, a: &Analysis, index: &Index, out: &mut Vec<Diagnostic>) {
    if !index.complete || index.generated || a.degraded {
        return;
    }
    for var in a.vars.values() {
        if index.referenced.contains(&var.name) || var.ambiguous {
            continue;
        }
        // Exported variables are read by the commands a recipe runs; `?=` and
        // `override` say outright that something outside sets this; and make's
        // own variables are read by the built-in implicit rules.
        if var.flags.is_export
            || var.flags.is_override
            || var.op == AssignOp::Conditional
            || builtins::is_known_variable(&var.name)
            || var.name.starts_with('.')
        {
            continue;
        }
        // Only judge the file the user pointed at. An included library defines
        // variables for makefiles that are not in this workspace, and nothing
        // here can see whether they are read.
        if !ws.roots.contains(&var.span.file) {
            continue;
        }
        out.push(
            Diagnostic::new(
                "MK002",
                Severity::Note,
                var.span,
                format!("`{}` is assigned but never read", var.name),
            )
            .with_help("if another makefile is meant to read it, this file cannot tell"),
        );
    }
}

// ---------------------------------------------------------------------------
// MK003 — an assignment that discards a value nothing had read
// ---------------------------------------------------------------------------

fn mk003_clobbered_value(a: &Analysis, index: &Index, out: &mut Vec<Diagnostic>) {
    if index.generated {
        return;
    }
    for c in &a.clobbered {
        out.push(
            Diagnostic::warn(
                "MK003",
                c.span,
                format!(
                    "this assignment to `{}` discards the previous value, which nothing had read",
                    c.name
                ),
            )
            .with_label(c.previous, "the value discarded was set here")
            .with_help(
                "use `?=` if the earlier assignment was meant as a default, or `+=` to add to it",
            ),
        );
    }
}

// ---------------------------------------------------------------------------
// MK004 — `+=` before the variable has any definition
// ---------------------------------------------------------------------------

fn mk004_append_before_definition(a: &Analysis, index: &Index, out: &mut Vec<Diagnostic>) {
    if !index.complete || index.generated || a.degraded {
        return;
    }
    for r in &a.blind_appends {
        // Appending to a make variable picks up the environment's value, which
        // is normally the point.
        if builtins::is_known_variable(&r.name) {
            continue;
        }
        out.push(
            Diagnostic::new(
                "MK004",
                Severity::Note,
                r.span,
                format!("`{}` has no definition yet, so `+=` acts as `=`", r.name),
            )
            .with_help("harmless unless the definition was meant to come first"),
        );
    }
}

// ---------------------------------------------------------------------------
// MK005 — `VAR = $(shell ...)` re-runs the command on every expansion
// ---------------------------------------------------------------------------

fn mk005_deferred_shell(ws: &Workspace, index: &Index, out: &mut Vec<Diagnostic>) {
    if index.generated {
        return;
    }
    for mf in ws.makefiles() {
        mf.walk_items(&mut |it| {
            let Item::Assign(assign) = it else { return };
            if !assign.op.is_deferred() || assign.target.is_some() {
                return;
            }
            let Some(name) = assign.name.literal() else { return };
            let name = name.trim().to_string();

            let mut shell_span = None;
            assign.value.visit(&mut |p| {
                if let Piece::Func(c) = p
                    && c.name == "shell"
                    && shell_span.is_none()
                {
                    shell_span = Some(c.span);
                }
            });
            let Some(span) = shell_span else { return };

            // One reference means one run, which is what `:=` would do anyway.
            let uses = index.reference_counts.get(&name).copied().unwrap_or(0);
            if uses < 2 {
                return;
            }
            out.push(
                Diagnostic::warn(
                    "MK005",
                    span,
                    format!(
                        "`{name}` is assigned with `{}`, so this command runs again on each of its {uses} expansions",
                        assign.op.as_str()
                    ),
                )
                .with_label(assign.op_span, "assigned here")
                .with_help("use `:=` to run the command once while the makefile is read"),
            );
        });
    }
}

// ---------------------------------------------------------------------------
// MK009 — a recursive variable that refers to itself
// ---------------------------------------------------------------------------

fn mk009_self_reference(a: &Analysis, out: &mut Vec<Diagnostic>) {
    for r in &a.recursive {
        out.push(
            Diagnostic::error(
                "MK009",
                r.span,
                format!("`{}` refers to itself, so expanding it never terminates", r.name),
            )
            .with_help("make reports \"Recursive variable references itself (eventually)\"; use `:=` to capture the old value"),
        );
    }
}

// ---------------------------------------------------------------------------
// MK021 — a prerequisite that no rule builds and no file provides
// ---------------------------------------------------------------------------

fn mk021_unmakeable_prerequisite(
    ws: &Workspace,
    a: &Analysis,
    index: &Index,
    out: &mut Vec<Diagnostic>,
) {
    // VPATH lets make find prerequisites in directories we are not tracking,
    // and an unresolved include may hold the missing rule.
    if !index.complete || index.has_vpath || a.degraded {
        return;
    }
    let base = ws.base_dir();
    let mut reported: HashSet<String> = HashSet::new();

    for set in &a.prereqs {
        // A prerequisite list we could not fully expand tells us nothing.
        let Some(list) = set.value.as_known() else { continue };
        let Some(mf) = ws.get(set.file) else { continue };
        let rule = &mf.rules[set.rule];

        for word in list.split_whitespace() {
            if word.contains('%') || word.starts_with('.') {
                continue;
            }
            // make expands wildcards in prerequisites, so a glob is not a name
            // we can look up.
            if funcs::has_meta(word) {
                continue;
            }
            // A bare word with no slash or dot is almost always a phony target,
            // and a makefile meant to be included expects its includer to
            // define those. Only judge names shaped like files.
            if !word.contains('/') && !word.contains('.') {
                continue;
            }
            if index.targets.contains(word) || a.targets.contains(word) {
                continue;
            }
            let patterns = index
                .pattern_targets
                .iter()
                .map(String::as_str)
                .chain(a.pattern_targets.iter().map(String::as_str))
                .chain(builtins::IMPLICIT_TARGETS.iter().copied());
            if patterns.into_iter().any(|p| funcs::pattern_match(p, word).is_some()) {
                continue;
            }
            if base.join(word).symlink_metadata().is_ok() {
                continue;
            }
            if !reported.insert(word.to_string()) {
                continue;
            }
            out.push(
                Diagnostic::warn(
                    "MK021",
                    rule.span,
                    format!("prerequisite `{word}` has no rule and no file"),
                )
                .with_help("make will fail with \"No rule to make target\" unless something outside this makefile creates it"),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// MK006 / MK007 / MK008 — duplicated values
// ---------------------------------------------------------------------------

/// Values shorter than this are not evidence of anything: half a makefile's
/// variables are `1`, `y`, `.` or empty.
const MIN_DUPLICATE_LEN: usize = 4;

/// How many peers to name in one diagnostic before summarising.
const MAX_PEER_LABELS: usize = 4;

/// Below this length a value needs no structure to be worth comparing.
const DISTINCTIVE_LEN: usize = 16;

/// True when a shared value says nothing about the makefile's structure.
///
/// The strongest signal is structure. A compound value like
/// `github.com/projectcalico/calico/api` appearing twice is duplication; a bare
/// word like `latest`, `master` or `amd64` is a token many variables hold for
/// unrelated reasons, and pairing them up is noise.
fn trivial_value(value: &str) -> bool {
    let v = value.trim();
    if v.len() < MIN_DUPLICATE_LEN {
        return true;
    }
    let structured = v.contains(['/', ':', '.', '=', ' ', '\t', ',']);
    if !structured && v.len() < DISTINCTIVE_LEN {
        return true;
    }
    // Nothing alphanumeric: separators, flags, punctuation runs.
    if !v.chars().any(char::is_alphanumeric) {
        return true;
    }
    matches!(
        v.to_ascii_lowercase().as_str(),
        "true"
            | "false"
            | "yes"
            | "no"
            | "none"
            | "null"
            | "nil"
            | "auto"
            | "all"
            | "any"
            | "default"
            | "off"
            | "enabled"
            | "disabled"
            | "unknown"
            | "todo"
            | "unset"
            | "empty"
    )
}

/// Transitive closure of "this variable's value was read from that one".
fn derivations(a: &Analysis) -> HashMap<&str, HashSet<&str>> {
    let mut closure: HashMap<&str, HashSet<&str>> = HashMap::new();
    for name in a.reads.keys() {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<&str> = vec![name.as_str()];
        while let Some(cur) = stack.pop() {
            let Some(direct) = a.reads.get(cur) else { continue };
            for d in direct {
                if seen.insert(d.as_str()) {
                    stack.push(d.as_str());
                }
            }
        }
        closure.insert(name.as_str(), seen);
    }
    closure
}

/// The variable this one is a plain alias of: its value is exactly one
/// reference and nothing else.
fn alias_target(value: &Expr) -> Option<String> {
    let mut refs = Vec::new();
    for p in &value.pieces {
        match p {
            Piece::Text { text, .. } if text.trim().is_empty() => {}
            Piece::Var(v) if v.subst.is_none() => refs.push(v.name.literal()?),
            _ => return None,
        }
    }
    match refs.as_slice() {
        [one] => Some(one.trim().to_string()),
        _ => None,
    }
}

fn shorten(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Variables that resolve to the same value without deriving from one another.
fn mk006_duplicate_value(
    ws: &Workspace,
    a: &Analysis,
    index: &Index,
    out: &mut Vec<Diagnostic>,
) -> HashSet<String> {
    let mut reported: HashSet<String> = HashSet::new();
    if !index.complete || index.generated || a.degraded {
        return reported;
    }
    let derived = derivations(a);

    // Group by value. Only fully known, stable, non-trivial values qualify.
    let mut groups: BTreeMap<&str, Vec<&eval::ResolvedVar>> = BTreeMap::new();
    for var in a.vars.values() {
        if var.synthetic
            || var.ambiguous
            || var.name.starts_with('.')
            || var.value.stability >= Stability::Volatile
        {
            continue;
        }
        let Some(text) = var.value.as_known() else { continue };
        if trivial_value(text) {
            continue;
        }
        groups.entry(text).or_default().push(var);
    }

    for (text, members) in groups {
        if members.len() < 2 {
            continue;
        }
        // Drop anything whose value came *from* another member: an alias
        // agreeing with its source is arithmetic, not a coincidence.
        let names: HashSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
        let mut independent: Vec<&eval::ResolvedVar> = members
            .iter()
            .copied()
            .filter(|m| {
                let deps = derived.get(m.name.as_str());
                !deps.is_some_and(|d| names.iter().any(|n| *n != m.name && d.contains(n)))
            })
            .collect();
        if independent.len() < 2 {
            continue;
        }
        independent.sort_by_key(|v| (v.span.file, v.span.start));

        let last = independent.pop().unwrap();
        let mut d = Diagnostic::warn(
            "MK006",
            last.span,
            format!(
                "`{}` duplicates the value of {}: `{}`",
                last.name,
                match independent.len() {
                    1 => format!("`{}`", independent[0].name),
                    n => format!("{n} other variables"),
                },
                shorten(text, 60)
            ),
        );
        for peer in independent.iter().take(MAX_PEER_LABELS) {
            d = d.with_label(peer.span, format!("`{}` has the same value", peer.name));
        }
        if independent.len() > MAX_PEER_LABELS {
            d = d.with_help(format!(
                "{} further variables share it; consider one definition the others reference",
                independent.len() - MAX_PEER_LABELS
            ));
        } else {
            d = d.with_help("consider making one the definition and the others reference it");
        }
        let _ = ws;
        out.push(d);

        reported.insert(last.name.clone());
        reported.extend(independent.iter().map(|v| v.name.clone()));
    }
    reported
}

/// `A = $(B)`: a second name for the same thing.
fn mk007_alias(ws: &Workspace, a: &Analysis, index: &Index, out: &mut Vec<Diagnostic>) {
    if !index.complete || index.generated {
        return;
    }
    for mf in ws.makefiles() {
        mf.walk_items(&mut |it| {
            let Item::Assign(assign) = it else { return };
            if assign.target.is_some() || assign.op == AssignOp::Append {
                return;
            }
            let Some(name) = assign.name.literal() else { return };
            let name = name.trim().to_string();
            let Some(target) = alias_target(&assign.value) else { return };
            if target == name || !a.vars.contains_key(&target) {
                return;
            }
            // A rename left half-done shows up as one of the pair being unused;
            // MK002 covers that. This is about two live names for one value.
            if !index.referenced.contains(&name) || !index.referenced.contains(&target) {
                return;
            }
            out.push(
                Diagnostic::new(
                    "MK007",
                    Severity::Note,
                    assign.span,
                    format!("`{name}` is another name for `{target}`; both are used"),
                )
                .with_help("harmless, but a reader has to know they are the same"),
            );
        });
    }
}

/// Variables defined by byte-identical expressions. Weaker evidence than MK006
/// because the value itself could not be resolved, but the claim is exact: the
/// same expression is written twice.
fn mk008_same_expression(
    ws: &Workspace,
    a: &Analysis,
    index: &Index,
    covered: &HashSet<String>,
    out: &mut Vec<Diagnostic>,
) {
    if !index.complete || index.generated || a.degraded {
        return;
    }
    struct Site {
        name: String,
        span: Span,
        deferred: bool,
    }
    let mut groups: BTreeMap<String, Vec<Site>> = BTreeMap::new();

    for mf in ws.makefiles() {
        mf.walk_items(&mut |it| {
            let Item::Assign(assign) = it else { return };
            if assign.target.is_some() || assign.op == AssignOp::Append {
                return;
            }
            let Some(name) = assign.name.literal() else { return };
            let name = name.trim().to_string();
            if covered.contains(&name) {
                return;
            }
            // Only where the value could not be resolved; a known value is
            // MK006's business and it has better evidence.
            match a.vars.get(&name) {
                Some(v) if !v.value.is_known() && !v.ambiguous && !v.synthetic => {}
                _ => return,
            }
            // An expression, not a bare literal, and long enough to mean
            // something.
            let text = normalise_ws(ws.sources.snippet(assign.value.span));
            if !text.contains('$') || text.len() < MIN_DUPLICATE_LEN {
                return;
            }
            groups.entry(text).or_default().push(Site {
                name,
                span: assign.span,
                deferred: assign.op.is_deferred(),
            });
        });
    }

    for (text, mut sites) in groups {
        // Different flavours can genuinely differ, since `:=` freezes the value
        // where it stands. Only compare like with like.
        sites.dedup_by(|a, b| a.name == b.name && a.span == b.span);
        let mut distinct: Vec<&Site> = Vec::new();
        for s in &sites {
            if !distinct.iter().any(|o| o.name == s.name) {
                distinct.push(s);
            }
        }
        if distinct.len() < 2 || distinct.iter().any(|s| s.deferred != distinct[0].deferred) {
            continue;
        }
        distinct.sort_by_key(|s| (s.span.file, s.span.start));
        let last = distinct.pop().unwrap();
        let mut d = Diagnostic::new(
            "MK008",
            Severity::Note,
            last.span,
            format!(
                "`{}` is defined by the same expression as {}: `{}`",
                last.name,
                match distinct.len() {
                    1 => format!("`{}`", distinct[0].name),
                    n => format!("{n} other variables"),
                },
                shorten(&text, 60)
            ),
        );
        for peer in distinct.iter().take(MAX_PEER_LABELS) {
            d = d.with_label(peer.span, format!("`{}` is defined the same way", peer.name));
        }
        out.push(d.with_help("the value could not be resolved, so this compares the expressions"));
    }
}

fn normalise_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// ---------------------------------------------------------------------------
// MK040 — a `$(shell ...)` the oracle would not run
// ---------------------------------------------------------------------------

/// Reports each command that was not run, and why. Without this a refused
/// command is indistinguishable from one that produced nothing, and the reader
/// has no way to see that a value is missing rather than empty.
fn mk040_command_not_run(a: &Analysis, out: &mut Vec<Diagnostic>) {
    let mut seen: HashSet<&str> = HashSet::new();
    for r in &a.shell_refusals {
        if !seen.insert(r.command.as_str()) {
            continue;
        }
        let allowable = matches!(r.reason, crate::shell::DenyReason::NotAllowed(_));
        let mut d = Diagnostic::new(
            "MK040",
            Severity::Note,
            r.span,
            format!("`$(shell {})` was not run: {}", shorten(&r.command, 50), r.reason.describe()),
        );
        d = if allowable {
            d.with_help("values derived from it stay unknown; `--allow-command NAME` permits it")
        } else {
            d.with_help("values derived from it stay unknown")
        };
        out.push(d);
    }
}
