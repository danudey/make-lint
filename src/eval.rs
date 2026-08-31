//! The evaluator: a read pass over the item tree that builds make's variable
//! table, then expands what the checks need.
//!
//! Three things make this different from just running make:
//!
//! * Nothing is executed. `$(shell ...)` and `!=` produce [`UnknownReason::Shell`].
//!   `$(wildcard)` is the one exception: it only lists directories.
//! * A conditional whose condition cannot be decided is *forked*. Each branch is
//!   read against a copy of the table and the results are merged; a variable the
//!   branches disagree about becomes `Unknown`. A decidable condition is simply
//!   taken, so precision is kept wherever make's own answer is knowable.
//! * Recursive (`=`) variables keep their unexpanded text and are resolved at
//!   the end, which is the value make would use in a recipe.

use crate::ast::*;
use crate::builtins;
use crate::funcs;
use crate::span::{FileId, Span};
use crate::value::*;
use crate::workspace::Workspace;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

/// Expansion nesting limit, to bound pathological input.
const MAX_DEPTH: usize = 120;
/// Conditionals deeper than this stop forking and go straight to `Unknown`.
const MAX_FORK_DEPTH: usize = 4;
/// Total forks allowed, so a file full of `ifdef` cannot blow up.
const FORK_BUDGET: u32 = 3000;

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Reference {
    pub name: String,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Clobber {
    pub name: String,
    /// The assignment that discarded the value.
    pub span: Span,
    /// The assignment whose value was discarded.
    pub previous: Span,
}

#[derive(Clone, Debug)]
pub struct ResolvedVar {
    pub name: String,
    pub value: Value,
    pub op: AssignOp,
    pub span: Span,
    pub flags: AssignFlags,
    /// The branches of an undecidable conditional disagreed about this one.
    pub ambiguous: bool,
}

#[derive(Clone, Debug)]
pub struct RulePrereqs {
    pub file: FileId,
    pub rule: usize,
    pub value: Value,
}

#[derive(Default, Debug)]
pub struct Analysis {
    /// The final variable table, as make would have it after reading.
    pub vars: BTreeMap<String, ResolvedVar>,
    /// References to names that resolved to nothing.
    pub undefined: Vec<Reference>,
    /// Recursive variables that refer to themselves.
    pub recursive: Vec<Reference>,
    /// Assignments that discarded a value nothing had read.
    pub clobbered: Vec<Clobber>,
    /// `+=` where the variable had no definition yet.
    pub blind_appends: Vec<Reference>,
    /// Prerequisites, expanded against the final table.
    pub prereqs: Vec<RulePrereqs>,
    /// Rule targets, expanded against the final table. A target spelled
    /// `$(BINDIR)/tool` only becomes a name here.
    pub targets: HashSet<String>,
    /// Expanded targets containing `%`.
    pub pattern_targets: Vec<String>,
    /// True when evaluation ran out of fork budget and had to approximate.
    pub degraded: bool,
}

pub fn analyse(ws: &Workspace) -> Analysis {
    let mut ev = Evaluator::new(ws);
    let roots = ws.roots.clone();
    for root in roots {
        ev.read_file(root);
    }
    ev.finish()
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Body<'a> {
    /// Recursive flavour: unexpanded parts, joined with a space by `+=`.
    Deferred(Vec<&'a Expr>),
    /// Simple flavour: expanded at the point of assignment.
    Immediate(Value),
}

#[derive(Clone, Debug)]
struct VarDef<'a> {
    name: String,
    body: Body<'a>,
    op: AssignOp,
    span: Span,
    flags: AssignFlags,
    /// Read-order position of this assignment.
    seq: u32,
    ambiguous: bool,
}

/// Expansion context: `$(call)` arguments and `$(foreach)` bindings.
#[derive(Default, Clone)]
struct Ctx {
    /// `$0` is the macro name, `$1`.. the arguments.
    args: Vec<Value>,
    locals: Vec<(String, Value)>,
    /// Variables being expanded, for self-reference detection.
    stack: Vec<String>,
    depth: usize,
    /// Set when an expansion read an argument or loop binding, which makes its
    /// result context-dependent and so not cacheable.
    impure: bool,
}

struct Evaluator<'a> {
    ws: &'a Workspace,
    /// Append-only store, so the environment can be cloned cheaply for forks.
    defs: Vec<VarDef<'a>>,
    env: HashMap<String, u32>,
    seq: u32,
    /// Last read-order position at which a name was read during an expansion
    /// make performs while reading.
    last_read: HashMap<String, u32>,
    /// True while expanding something make expands at read time.
    immediate: bool,
    /// Names assigned somewhere in the workspace, used to decide `ifdef` for a
    /// variable nothing has set yet.
    assigned_anywhere: HashSet<String>,
    include_stack: Vec<FileId>,
    fork_depth: usize,
    fork_budget: u32,
    resolved: HashMap<u32, Value>,
    base_dir: PathBuf,
    out: Analysis,
}

impl<'a> Evaluator<'a> {
    fn new(ws: &'a Workspace) -> Self {
        let mut assigned_anywhere = HashSet::new();
        for mf in ws.makefiles() {
            mf.walk_items(&mut |it| match it {
                Item::Assign(a) => {
                    if let Some(n) = a.name.literal() {
                        assigned_anywhere.insert(n.trim().to_string());
                    }
                }
                Item::Define(d) => {
                    if let Some(n) = d.name.literal() {
                        assigned_anywhere.insert(n.trim().to_string());
                    }
                }
                _ => {}
            });
        }
        let base_dir = ws
            .roots
            .first()
            .and_then(|&r| ws.sources.get(r).path.parent().map(PathBuf::from))
            .unwrap_or_default();

        let mut ev = Evaluator {
            ws,
            defs: Vec::new(),
            env: HashMap::new(),
            seq: 0,
            last_read: HashMap::new(),
            immediate: false,
            assigned_anywhere,
            include_stack: Vec::new(),
            fork_depth: 0,
            fork_budget: FORK_BUDGET,
            resolved: HashMap::new(),
            base_dir: base_dir.clone(),
            out: Analysis::default(),
        };
        // make sets CURDIR itself, and enough makefiles build paths from it
        // that leaving it empty would lose real values.
        ev.push_def(
            "CURDIR".to_string(),
            VarDef {
                name: "CURDIR".to_string(),
                body: Body::Immediate(Value::known(base_dir.to_string_lossy().into_owned())),
                op: AssignOp::Simple,
                span: Span::new(FileId(0), 0, 0),
                flags: AssignFlags::default(),
                seq: 0,
                ambiguous: false,
            },
        );
        ev
    }

    // -----------------------------------------------------------------------
    // Read pass
    // -----------------------------------------------------------------------

    fn read_file(&mut self, id: FileId) {
        if self.include_stack.contains(&id) {
            return;
        }
        self.include_stack.push(id);
        let ws: &'a Workspace = self.ws;
        if let Some(mf) = ws.get(id) {
            self.read_items(&mf.items);
        }
        self.include_stack.pop();
    }

    fn read_items(&mut self, items: &'a [Item]) {
        for it in items {
            match it {
                Item::Assign(a) => self.do_assign(a),
                Item::Define(d) => self.do_define(d),
                Item::Conditional(c) => self.do_conditional(c),
                Item::Include(inc) => {
                    let ws: &'a Workspace = self.ws;
                    for p in &inc.resolved {
                        if let Some(id) = ws.sources.find(p) {
                            self.read_file(id);
                        }
                    }
                }
                // A bare `$(error ...)` / `$(eval ...)` line.
                Item::Expression { expr, .. } => {
                    let mut ctx = Ctx::default();
                    self.immediate = true;
                    self.expand(expr, &mut ctx);
                    self.immediate = false;
                }
                Item::Undefine { name, .. } => {
                    let mut ctx = Ctx::default();
                    self.immediate = true;
                    let n = self.expand(name, &mut ctx);
                    self.immediate = false;
                    if let Some(n) = n.as_known() {
                        self.env.remove(n.trim());
                    }
                }
                Item::Rule(_) | Item::Export(_) | Item::Vpath { .. } | Item::Unparsed { .. } => {}
            }
        }
    }

    fn push_def(&mut self, name: String, def: VarDef<'a>) {
        self.defs.push(def);
        let id = (self.defs.len() - 1) as u32;
        self.env.insert(name, id);
    }

    fn do_assign(&mut self, a: &'a Assign) {
        // Target-specific variables apply only while building that target;
        // folding them into the global table would be wrong.
        if a.target.is_some() {
            return;
        }
        let mut ctx = Ctx::default();
        self.immediate = true;
        let name = self.expand(&a.name, &mut ctx);
        self.immediate = false;
        let Some(name) = name.as_known().map(|s| s.trim().to_string()) else { return };
        if name.is_empty() {
            return;
        }

        self.seq += 1;
        let seq = self.seq;
        let existing = self.env.get(&name).copied();

        let body = match a.op {
            AssignOp::Conditional => {
                if existing.is_some() {
                    return;
                }
                Body::Deferred(vec![&a.value])
            }
            AssignOp::Append => match existing {
                None => {
                    self.out.blind_appends.push(Reference { name: name.clone(), span: a.span });
                    Body::Deferred(vec![&a.value])
                }
                Some(id) => {
                    let mut body = self.defs[id as usize].body.clone();
                    match &mut body {
                        Body::Deferred(parts) => parts.push(&a.value),
                        Body::Immediate(v) => {
                            self.immediate = true;
                            let add = self.expand(&a.value, &mut ctx);
                            self.immediate = false;
                            *v = Value::concat(vec![v.clone(), Value::known(" "), add]);
                        }
                    }
                    body
                }
            },
            AssignOp::Shell => {
                self.record_clobber(&name, existing, a.span);
                Body::Immediate(Value::unknown(UnknownReason::ShellAssign, a.value.span))
            }
            AssignOp::Recursive => {
                self.record_clobber(&name, existing, a.span);
                Body::Deferred(vec![&a.value])
            }
            AssignOp::Simple | AssignOp::SimplePosix | AssignOp::Immediate => {
                self.immediate = true;
                let v = self.expand(&a.value, &mut ctx);
                self.immediate = false;
                // Checked after expanding: `X := $(X) more` reads the old value.
                self.record_clobber(&name, existing, a.span);
                Body::Immediate(v)
            }
        };

        self.push_def(
            name.clone(),
            VarDef { name, body, op: a.op, span: a.span, flags: a.flags, seq, ambiguous: false },
        );
    }

    fn do_define(&mut self, d: &'a Define) {
        let mut ctx = Ctx::default();
        self.immediate = true;
        let name = self.expand(&d.name, &mut ctx);
        self.immediate = false;
        let Some(name) = name.as_known().map(|s| s.trim().to_string()) else { return };
        if name.is_empty() {
            return;
        }
        self.seq += 1;
        let seq = self.seq;

        let body = match d.op {
            AssignOp::Simple | AssignOp::SimplePosix | AssignOp::Immediate => {
                self.immediate = true;
                let v = self.expand(&d.body_expr, &mut ctx);
                self.immediate = false;
                Body::Immediate(v)
            }
            _ => Body::Deferred(vec![&d.body_expr]),
        };
        self.push_def(
            name.clone(),
            VarDef { name, body, op: d.op, span: d.span, flags: d.flags, seq, ambiguous: false },
        );
    }

    /// An assignment that throws away a value nothing had read yet.
    fn record_clobber(&mut self, name: &str, existing: Option<u32>, span: Span) {
        // Inside an undecided conditional an override is the normal idiom.
        if self.fork_depth > 0 {
            return;
        }
        let Some(id) = existing else { return };
        let prev = &self.defs[id as usize];
        // `?=` is a default meant to be overridden, and an ambiguous previous
        // value may not have existed at all.
        if prev.ambiguous || prev.op == AssignOp::Conditional {
            return;
        }
        let (prev_seq, previous) = (prev.seq, prev.span);
        if self.last_read.get(name).copied().unwrap_or(0) < prev_seq {
            self.out.clobbered.push(Clobber { name: name.to_string(), span, previous });
        }
    }

    // -----------------------------------------------------------------------
    // Conditionals
    // -----------------------------------------------------------------------

    fn do_conditional(&mut self, c: &'a Conditional) {
        for (i, br) in c.branches.iter().enumerate() {
            match self.eval_cond(&br.cond) {
                Some(true) => {
                    self.read_items(&br.body);
                    return;
                }
                Some(false) => continue,
                None => return self.fork(c, i),
            }
        }
        if let Some(e) = &c.else_body {
            self.read_items(e);
        }
    }

    /// Read every branch that is still possible against its own copy of the
    /// table, then merge. Variables the branches agree on keep their value.
    fn fork(&mut self, c: &'a Conditional, from: usize) {
        if self.fork_depth >= MAX_FORK_DEPTH || self.fork_budget == 0 {
            return self.degrade(c, from);
        }
        self.fork_budget -= 1;
        self.fork_depth += 1;

        let base = self.env.clone();
        let mut outcomes: Vec<HashMap<String, u32>> = Vec::new();

        for br in &c.branches[from..] {
            self.env = base.clone();
            self.read_items(&br.body);
            outcomes.push(std::mem::take(&mut self.env));
        }
        self.env = base.clone();
        if let Some(e) = &c.else_body {
            self.read_items(e);
        }
        outcomes.push(std::mem::take(&mut self.env));

        self.fork_depth -= 1;
        self.env = self.merge(&base, outcomes, c.span);
    }

    /// Keep what every outcome agrees on; mark the rest unknown.
    fn merge(
        &mut self,
        base: &HashMap<String, u32>,
        outcomes: Vec<HashMap<String, u32>>,
        span: Span,
    ) -> HashMap<String, u32> {
        let mut names: HashSet<&String> = HashSet::new();
        for o in &outcomes {
            names.extend(o.keys());
        }
        names.extend(base.keys());

        let mut merged = HashMap::with_capacity(names.len());
        let mut ambiguous: Vec<String> = Vec::new();
        for name in names {
            let first = outcomes[0].get(name).copied();
            if outcomes.iter().all(|o| o.get(name).copied() == first) {
                if let Some(id) = first {
                    merged.insert(name.clone(), id);
                }
            } else {
                ambiguous.push(name.clone());
            }
        }
        for name in ambiguous {
            self.seq += 1;
            let seq = self.seq;
            // Prefer the last branch's definition site so the span points at
            // real source rather than at the `if`.
            let site = outcomes
                .iter()
                .rev()
                .find_map(|o| o.get(&name))
                .map(|&id| self.defs[id as usize].span)
                .unwrap_or(span);
            self.defs.push(VarDef {
                name: name.clone(),
                body: Body::Immediate(Value::unknown(UnknownReason::ConditionalBranches, site)),
                op: AssignOp::Recursive,
                span: site,
                flags: AssignFlags::default(),
                seq,
                ambiguous: true,
            });
            merged.insert(name, (self.defs.len() - 1) as u32);
        }
        merged
    }

    /// Out of budget: read every branch in sequence and mark whatever they
    /// touch as unknown, rather than pretending one branch won.
    fn degrade(&mut self, c: &'a Conditional, from: usize) {
        self.out.degraded = true;
        let before = self.env.clone();
        self.fork_depth += 1;
        for br in &c.branches[from..] {
            self.read_items(&br.body);
        }
        if let Some(e) = &c.else_body {
            self.read_items(e);
        }
        self.fork_depth -= 1;

        let touched: Vec<String> = self
            .env
            .iter()
            .filter(|(k, v)| before.get(*k) != Some(*v))
            .map(|(k, _)| k.clone())
            .collect();
        for name in touched {
            let id = self.env[&name];
            let span = self.defs[id as usize].span;
            self.defs.push(VarDef {
                name: name.clone(),
                body: Body::Immediate(Value::unknown(UnknownReason::Budget, span)),
                op: AssignOp::Recursive,
                span,
                flags: AssignFlags::default(),
                seq: self.seq,
                ambiguous: true,
            });
            self.env.insert(name, (self.defs.len() - 1) as u32);
        }
    }

    fn eval_cond(&mut self, cond: &'a Cond) -> Option<bool> {
        self.immediate = true;
        let mut ctx = Ctx::default();
        let result = match cond {
            Cond::Eq { lhs, rhs, negated } => {
                let l = self.expand(lhs, &mut ctx);
                let r = self.expand(rhs, &mut ctx);
                match (l.as_known(), r.as_known()) {
                    (Some(a), Some(b)) => Some((a == b) != *negated),
                    _ => None,
                }
            }
            Cond::Def { name, negated } => {
                let n = self.expand(name, &mut ctx);
                match n.as_known().map(|s| s.trim().to_string()) {
                    None => None,
                    Some(n) => match self.env.get(&n).copied() {
                        Some(id) => {
                            let v = self.resolve_def(id, &mut ctx);
                            v.as_known().map(|s| s.is_empty() == *negated)
                        }
                        // Nothing in the workspace ever sets it, so a plain
                        // `make` sees it unset. If something does set it
                        // elsewhere, the answer depends on where we are.
                        None if !self.assigned_anywhere.contains(&n) => Some(*negated),
                        None => None,
                    },
                }
            }
            Cond::Malformed => None,
        };
        self.immediate = false;
        result
    }

    // -----------------------------------------------------------------------
    // Expansion
    // -----------------------------------------------------------------------

    fn resolve_def(&mut self, id: u32, ctx: &mut Ctx) -> Value {
        let cacheable = ctx.args.is_empty() && ctx.locals.is_empty();
        if cacheable && let Some(v) = self.resolved.get(&id) {
            return v.clone();
        }
        let def = &self.defs[id as usize];
        let (name, span) = (def.name.clone(), def.span);
        let body = def.body.clone();

        match body {
            Body::Immediate(v) => v,
            Body::Deferred(parts) => {
                if ctx.stack.contains(&name) {
                    self.out.recursive.push(Reference { name, span });
                    return Value::unknown(UnknownReason::Recursion, span);
                }
                ctx.stack.push(name);
                let outer_impure = std::mem::replace(&mut ctx.impure, false);
                let mut vals = Vec::with_capacity(parts.len() * 2);
                for (i, p) in parts.iter().enumerate() {
                    if i > 0 {
                        vals.push(Value::known(" "));
                    }
                    vals.push(self.expand(p, ctx));
                }
                let impure = ctx.impure;
                ctx.impure = outer_impure || impure;
                ctx.stack.pop();

                let v = Value::concat(vals);
                if cacheable && !impure {
                    self.resolved.insert(id, v.clone());
                }
                v
            }
        }
    }

    fn expand(&mut self, e: &'a Expr, ctx: &mut Ctx) -> Value {
        if ctx.depth > MAX_DEPTH {
            return Value::unknown(UnknownReason::Budget, e.span);
        }
        ctx.depth += 1;
        let mut parts = Vec::with_capacity(e.pieces.len());
        for p in &e.pieces {
            parts.push(match p {
                Piece::Text { text, .. } => Value::known(text.clone()),
                Piece::Dollar { .. } => Value::known("$"),
                Piece::Var(v) => self.expand_var(v, ctx),
                Piece::Func(c) => self.expand_func(c, ctx),
            });
        }
        ctx.depth -= 1;
        Value::concat(parts)
    }

    fn expand_var(&mut self, v: &'a VarRef, ctx: &mut Ctx) -> Value {
        let name = self.expand(&v.name, ctx);
        let Some(name) = name.as_known().map(str::to_string) else {
            return Value::unknown(UnknownReason::ComputedName, v.span);
        };
        let base = self.lookup(&name, v.span, ctx);
        let Some(subst) = &v.subst else { return base };

        let from = self.expand(&subst.from, ctx);
        let to = self.expand(&subst.to, ctx);
        let (Some(text), Some(from), Some(to)) = (base.as_known(), from.as_known(), to.as_known())
        else {
            return base.propagate(v.span);
        };
        // `$(V:.c=.o)` is patsubst with a leading `%` implied on both sides.
        let pattern = if from.contains('%') { from.to_string() } else { format!("%{from}") };
        let replace = if to.contains('%') { to.to_string() } else { format!("%{to}") };
        Value::known_with(funcs::patsubst(&pattern, &replace, text), base.stability)
    }

    fn lookup(&mut self, name: &str, span: Span, ctx: &mut Ctx) -> Value {
        // `$(1)`..`$(n)` inside `$(call)`.
        if !ctx.args.is_empty()
            && let Ok(n) = name.parse::<usize>()
        {
            ctx.impure = true;
            return ctx.args.get(n).cloned().unwrap_or_else(|| Value::known(""));
        }
        if let Some((_, v)) = ctx.locals.iter().rev().find(|(k, _)| k == name) {
            ctx.impure = true;
            return v.clone();
        }
        if builtins::is_automatic(name) {
            return Value::unknown(UnknownReason::Automatic, span);
        }
        if self.immediate {
            self.last_read.insert(name.to_string(), self.seq);
        }
        if let Some(&id) = self.env.get(name) {
            return self.resolve_def(id, ctx);
        }
        if let Some(v) = builtins::default_value(name) {
            return Value::known(v);
        }
        if builtins::is_known_variable(name) {
            return Value::known("");
        }
        // `$(1)`, `$(2)`... are macro parameters. Outside a `$(call)` they are
        // empty, but they are never a missing variable.
        if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
            return Value::known("");
        }
        self.out.undefined.push(Reference { name: name.to_string(), span });
        Value::unknown(UnknownReason::Undefined, span)
    }

    fn arg(&mut self, c: &'a FuncCall, i: usize, ctx: &mut Ctx) -> Value {
        match c.args.get(i) {
            Some(e) => self.expand(e, ctx),
            None => Value::known(""),
        }
    }

    fn expand_func(&mut self, c: &'a FuncCall, ctx: &mut Ctx) -> Value {
        // Expand argument `i`, or bail out with its unknown-ness.
        macro_rules! s {
            ($i:expr) => {{
                let v = self.arg(c, $i, ctx);
                match v.as_known() {
                    Some(t) => t.to_string(),
                    None => return v.propagate(c.span),
                }
            }};
        }

        match c.name.as_str() {
            "subst" => {
                let (f, t, x) = (s!(0), s!(1), s!(2));
                Value::known(if f.is_empty() { x } else { x.replace(&f, &t) })
            }
            "patsubst" => {
                let (p, r, x) = (s!(0), s!(1), s!(2));
                Value::known(funcs::patsubst(&p, &r, &x))
            }
            "strip" => Value::known(s!(0).split_whitespace().collect::<Vec<_>>().join(" ")),
            "findstring" => {
                let (a, b) = (s!(0), s!(1));
                Value::known(if b.contains(&a) { a } else { String::new() })
            }
            "filter" => {
                let (p, x) = (s!(0), s!(1));
                Value::known(funcs::filter(&p, &x, true))
            }
            "filter-out" => {
                let (p, x) = (s!(0), s!(1));
                Value::known(funcs::filter(&p, &x, false))
            }
            "sort" => Value::known(funcs::sort_words(&s!(0))),
            "words" => Value::known(s!(0).split_whitespace().count().to_string()),
            "firstword" => Value::known(s!(0).split_whitespace().next().unwrap_or("")),
            "lastword" => Value::known(s!(0).split_whitespace().next_back().unwrap_or("")),
            "word" => {
                let (n, x) = (s!(0), s!(1));
                let Ok(n) = n.trim().parse::<usize>() else {
                    return Value::unknown(UnknownReason::Unsupported("word"), c.span);
                };
                Value::known(if n == 0 {
                    ""
                } else {
                    x.split_whitespace().nth(n - 1).unwrap_or("")
                })
            }
            "wordlist" => {
                let (a, b, x) = (s!(0), s!(1), s!(2));
                let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) else {
                    return Value::unknown(UnknownReason::Unsupported("wordlist"), c.span);
                };
                let ws: Vec<&str> = x.split_whitespace().collect();
                let lo = a.saturating_sub(1).min(ws.len());
                let hi = b.min(ws.len());
                Value::known(if lo < hi { ws[lo..hi].join(" ") } else { String::new() })
            }
            "dir" => Value::known(funcs::join_words(s!(0).split_whitespace().map(funcs::dir_of))),
            "notdir" => {
                Value::known(funcs::join_words(s!(0).split_whitespace().map(funcs::notdir_of)))
            }
            "suffix" => Value::known(funcs::join_words(
                s!(0)
                    .split_whitespace()
                    .map(funcs::suffix_of)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            )),
            "basename" => Value::known(funcs::join_words(
                s!(0).split_whitespace().map(|w| funcs::basename_of(w).to_string()),
            )),
            "addsuffix" => {
                let (suf, x) = (s!(0), s!(1));
                Value::known(funcs::join_words(x.split_whitespace().map(|w| format!("{w}{suf}"))))
            }
            "addprefix" => {
                let (pre, x) = (s!(0), s!(1));
                Value::known(funcs::join_words(x.split_whitespace().map(|w| format!("{pre}{w}"))))
            }
            "join" => {
                let (a, b) = (s!(0), s!(1));
                Value::known(funcs::join_lists(&a, &b))
            }

            // Reads the working tree, so the result is only as stable as it is.
            "wildcard" => {
                let pat = s!(0);
                let mut hits = Vec::new();
                for p in pat.split_whitespace() {
                    hits.extend(funcs::glob(&self.base_dir, p));
                }
                Value::known_with(hits.join(" "), Stability::Filesystem)
            }
            "abspath" | "realpath" => {
                let x = s!(0);
                let out = funcs::join_words(x.split_whitespace().map(|w| {
                    let p =
                        if w.starts_with('/') { PathBuf::from(w) } else { self.base_dir.join(w) };
                    if c.name == "realpath" {
                        std::fs::canonicalize(&p).unwrap_or(p).to_string_lossy().into_owned()
                    } else {
                        funcs::normalise(&p).to_string_lossy().into_owned()
                    }
                }));
                Value::known_with(out, Stability::Filesystem)
            }

            "if" => {
                let cond = self.arg(c, 0, ctx);
                let Some(t) = cond.as_known() else { return cond.propagate(c.span) };
                if t.trim().is_empty() { self.arg(c, 2, ctx) } else { self.arg(c, 1, ctx) }
            }
            "or" => {
                for i in 0..c.args.len() {
                    let v = self.arg(c, i, ctx);
                    let Some(t) = v.as_known() else { return v.propagate(c.span) };
                    if !t.trim().is_empty() {
                        return v;
                    }
                }
                Value::known("")
            }
            "and" => {
                let mut last = Value::known("");
                for i in 0..c.args.len() {
                    let v = self.arg(c, i, ctx);
                    let Some(t) = v.as_known() else { return v.propagate(c.span) };
                    if t.trim().is_empty() {
                        return Value::known("");
                    }
                    last = v;
                }
                last
            }
            "intcmp" => {
                let (a, b) = (s!(0), s!(1));
                let (Ok(a), Ok(b)) = (a.trim().parse::<i64>(), b.trim().parse::<i64>()) else {
                    return Value::unknown(UnknownReason::Unsupported("intcmp"), c.span);
                };
                // $(intcmp lhs,rhs[,lt][,eq][,gt]); with fewer parts the value
                // is lhs when the comparison holds.
                let pick = match (a.cmp(&b), c.args.len()) {
                    (std::cmp::Ordering::Less, n) if n > 2 => 2,
                    (std::cmp::Ordering::Equal, n) if n > 3 => 3,
                    (std::cmp::Ordering::Equal, n) if n > 2 => 2,
                    (std::cmp::Ordering::Greater, n) if n > 4 => 4,
                    (std::cmp::Ordering::Greater, n) if n > 2 => 2,
                    _ => return Value::known(if a == b { a.to_string() } else { String::new() }),
                };
                self.arg(c, pick, ctx)
            }

            "foreach" => {
                let var = s!(0);
                let list = s!(1);
                let Some(body) = c.args.get(2) else { return Value::known("") };
                let var = var.trim().to_string();
                let mut out = Vec::new();
                for (i, w) in list.split_whitespace().enumerate() {
                    if i > 0 {
                        out.push(Value::known(" "));
                    }
                    ctx.locals.push((var.clone(), Value::known(w)));
                    let v = self.expand(body, ctx);
                    ctx.locals.pop();
                    out.push(v);
                }
                Value::concat(out)
            }
            "let" => {
                let names = s!(0);
                let list = s!(1);
                let Some(body) = c.args.get(2) else { return Value::known("") };
                let names: Vec<&str> = names.split_whitespace().collect();
                let mut words = list.split_whitespace().collect::<Vec<_>>().into_iter();
                let pushed = names.len();
                for (i, n) in names.iter().enumerate() {
                    // The last name takes every remaining word.
                    let v = if i + 1 == pushed {
                        words.by_ref().collect::<Vec<_>>().join(" ")
                    } else {
                        words.next().unwrap_or("").to_string()
                    };
                    ctx.locals.push((n.to_string(), Value::known(v)));
                }
                let v = self.expand(body, ctx);
                ctx.locals.truncate(ctx.locals.len() - pushed);
                v
            }
            "call" => {
                let name = s!(0);
                let name = name.trim().to_string();
                let Some(&id) = self.env.get(&name) else {
                    // make expands an undefined macro to nothing.
                    return Value::known("");
                };
                let mut args = vec![Value::known(name)];
                for i in 1..c.args.len() {
                    args.push(self.arg(c, i, ctx));
                }
                let saved_args = std::mem::replace(&mut ctx.args, args);
                let saved_locals = std::mem::take(&mut ctx.locals);
                let v = self.resolve_def(id, ctx);
                ctx.args = saved_args;
                ctx.locals = saved_locals;
                ctx.impure = true;
                v
            }

            "value" => {
                let name = s!(0);
                match self.env.get(name.trim()).copied() {
                    Some(id) => match &self.defs[id as usize].body {
                        Body::Deferred(parts) => {
                            let ws: &'a Workspace = self.ws;
                            let text: Vec<String> = parts
                                .iter()
                                .map(|p| ws.sources.snippet(p.span).to_string())
                                .collect();
                            Value::known(text.join(" "))
                        }
                        Body::Immediate(v) => v.clone(),
                    },
                    None => Value::known(""),
                }
            }
            "origin" => {
                let name = s!(0);
                let name = name.trim();
                Value::known(if self.env.contains_key(name) {
                    "file"
                } else if builtins::default_value(name).is_some() {
                    "default"
                } else if builtins::is_automatic(name) {
                    "automatic"
                } else {
                    "undefined"
                })
            }
            "flavor" => {
                let name = s!(0);
                Value::known(match self.env.get(name.trim()).copied() {
                    None => "undefined",
                    Some(id) => match self.defs[id as usize].body {
                        Body::Deferred(_) => "recursive",
                        Body::Immediate(_) => "simple",
                    },
                })
            }

            // `$(eval)` at read time can still define a plain variable; during
            // later expansion the table is already fixed, so it cannot.
            "eval" => {
                let text = s!(0);
                if self.immediate {
                    self.apply_eval(&text, c.span);
                }
                Value::known("")
            }

            // These print and expand to nothing.
            "error" | "warning" | "info" => Value::known(""),

            "shell" => Value::unknown(UnknownReason::Shell, c.span),
            other => Value::unknown(
                UnknownReason::Unsupported(match other {
                    "file" => "file",
                    "guile" => "guile",
                    _ => "unknown",
                }),
                c.span,
            ),
        }
    }

    /// Apply the plain `NAME op VALUE` statements of an already-expanded
    /// `$(eval ...)` body. Rules and directives inside `eval` are not modelled.
    fn apply_eval(&mut self, text: &str, span: Span) {
        for line in text.lines() {
            let Some(eq) = line.find('=') else { continue };
            let name = line[..eq].trim_end_matches([':', '+', '?', '!']).trim();
            if name.is_empty()
                || name.contains(|ch: char| ch.is_whitespace() || "$(){}:".contains(ch))
            {
                continue;
            }
            let value = line[eq + 1..].trim().to_string();
            self.seq += 1;
            let seq = self.seq;
            self.push_def(
                name.to_string(),
                VarDef {
                    name: name.to_string(),
                    // The body was expanded once already, so this is literal.
                    body: Body::Immediate(Value::known(value)),
                    op: AssignOp::Simple,
                    span,
                    flags: AssignFlags::default(),
                    seq,
                    ambiguous: false,
                },
            );
        }
    }

    // -----------------------------------------------------------------------
    // Finish
    // -----------------------------------------------------------------------

    fn finish(mut self) -> Analysis {
        let live: Vec<(String, u32)> = self.env.iter().map(|(k, &v)| (k.clone(), v)).collect();
        for (name, id) in live {
            let mut ctx = Ctx::default();
            let value = self.resolve_def(id, &mut ctx);
            let d = &self.defs[id as usize];
            let var = ResolvedVar {
                name: name.clone(),
                value,
                op: d.op,
                span: d.span,
                flags: d.flags,
                ambiguous: d.ambiguous,
            };
            self.out.vars.insert(name, var);
        }

        // make expands prerequisites after every makefile has been read.
        let ws: &'a Workspace = self.ws;
        for (file, idx) in ws.rules_in_read_order() {
            let Some(mf) = ws.get(file) else { continue };
            let rule = &mf.rules[idx];

            let mut ctx = Ctx::default();
            let value = self.expand(&rule.prereqs, &mut ctx);
            self.out.prereqs.push(RulePrereqs { file, rule: idx, value });

            // Most variable reads live in recipes. Expanding them records
            // those uses; the resulting text is not otherwise needed.
            for line in &rule.recipe {
                let mut ctx = Ctx::default();
                self.expand(&line.expr, &mut ctx);
            }

            let mut ctx = Ctx::default();
            if let Some(list) = self.expand(&rule.targets, &mut ctx).as_known() {
                for t in list.split_whitespace() {
                    if t.contains('%') {
                        self.out.pattern_targets.push(t.to_string());
                    } else {
                        self.out.targets.insert(t.to_string());
                    }
                }
            }
            if let RuleKind::Static { target_pattern } = &rule.kind {
                let mut ctx = Ctx::default();
                if let Some(p) = self.expand(target_pattern, &mut ctx).as_known() {
                    self.out.pattern_targets.push(p.trim().to_string());
                }
            }
        }

        dedup_refs(&mut self.out.undefined);
        dedup_refs(&mut self.out.recursive);
        self.out
    }
}

fn dedup_refs(v: &mut Vec<Reference>) {
    let mut seen = HashSet::new();
    v.retain(|r| seen.insert((r.name.clone(), r.span.start, r.span.file)));
}
