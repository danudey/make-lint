//! The value lattice.
//!
//! A makefile cannot always be evaluated statically: `$(shell ...)` runs a
//! command, `$(wildcard)` reads the tree, a variable may come from the
//! environment, and an undecidable conditional leaves two possible values. So
//! evaluation does not produce a `String`; it produces a [`Value`] that either
//! knows its text, knows part of it, or knows why it does not know.
//!
//! The rule the checks depend on: **never report a finding derived from an
//! `Unknown`.** Precision is traded away freely to keep that true.

use crate::span::Span;

/// How reproducible a known value is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stability {
    /// The same text on any machine, at any time.
    Deterministic,
    /// Depends on the working tree, as `$(wildcard)` does.
    Filesystem,
    /// Depends on the environment or the clock. Two such values that happen to
    /// match are not evidence of anything.
    Volatile,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    /// Referenced but never assigned; may arrive from the environment.
    Undefined,
    /// `$(shell ...)`. Phase 4 resolves the allowlisted ones.
    Shell,
    /// A `!=` assignment.
    ShellAssign,
    /// `$(NAME)` where NAME itself could not be resolved.
    ComputedName,
    /// An automatic variable, which only has a value while a recipe runs.
    Automatic,
    /// A variable a recursive definition refers to itself.
    Recursion,
    /// Assigned differently in branches of a conditional that could not be
    /// decided statically.
    ConditionalBranches,
    /// Defined by `$(eval ...)` during expansion.
    Eval,
    /// A function this evaluator does not model.
    Unsupported(&'static str),
    /// Depth or fork budget exhausted.
    Budget,
}

impl UnknownReason {
    pub fn describe(&self) -> String {
        match self {
            UnknownReason::Undefined => "the variable is never assigned".into(),
            UnknownReason::Shell => "a `$(shell ...)` command was not run".into(),
            UnknownReason::ShellAssign => "a `!=` shell assignment was not run".into(),
            UnknownReason::ComputedName => "the variable name is computed".into(),
            UnknownReason::Automatic => {
                "an automatic variable has no value until a recipe runs".into()
            }
            UnknownReason::Recursion => "the definition refers to itself".into(),
            UnknownReason::ConditionalBranches => {
                "conditional branches assign it differently".into()
            }
            UnknownReason::Eval => "it is defined by `$(eval ...)`".into(),
            UnknownReason::Unsupported(f) => format!("`$({f} ...)` is not modelled"),
            UnknownReason::Budget => "evaluation gave up here".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unknown {
    pub reason: UnknownReason,
    pub span: Span,
}

/// A run of a partially known value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chunk {
    Text(String),
    Opaque(Unknown),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueKind {
    Known(String),
    /// Literal text around holes, as in `bin/$(shell git rev-parse HEAD)`.
    Partial(Vec<Chunk>),
    Unknown(Unknown),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub kind: ValueKind,
    pub stability: Stability,
}

impl Value {
    pub fn known(text: impl Into<String>) -> Value {
        Value { kind: ValueKind::Known(text.into()), stability: Stability::Deterministic }
    }

    pub fn known_with(text: impl Into<String>, stability: Stability) -> Value {
        Value { kind: ValueKind::Known(text.into()), stability }
    }

    pub fn unknown(reason: UnknownReason, span: Span) -> Value {
        Value {
            kind: ValueKind::Unknown(Unknown { reason, span }),
            stability: Stability::Deterministic,
        }
    }

    pub fn as_known(&self) -> Option<&str> {
        match &self.kind {
            ValueKind::Known(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_known(&self) -> bool {
        matches!(self.kind, ValueKind::Known(_))
    }

    /// The first thing that could not be resolved, for explaining a skip.
    pub fn first_unknown(&self) -> Option<&Unknown> {
        match &self.kind {
            ValueKind::Known(_) => None,
            ValueKind::Unknown(u) => Some(u),
            ValueKind::Partial(cs) => cs.iter().find_map(|c| match c {
                Chunk::Opaque(u) => Some(u),
                Chunk::Text(_) => None,
            }),
        }
    }

    /// Propagate this value's unknown-ness to a result that depends on it.
    pub fn propagate(&self, fallback: Span) -> Value {
        match self.first_unknown() {
            Some(u) => Value { kind: ValueKind::Unknown(u.clone()), stability: self.stability },
            None => Value::unknown(UnknownReason::Budget, fallback),
        }
    }

    /// Join values end to end. All known gives a known result; otherwise the
    /// literal runs are preserved around the holes.
    pub fn concat(parts: Vec<Value>) -> Value {
        let stability = parts.iter().map(|p| p.stability).max().unwrap_or(Stability::Deterministic);

        if parts.iter().all(Value::is_known) {
            let mut s = String::new();
            for p in &parts {
                s.push_str(p.as_known().unwrap());
            }
            return Value { kind: ValueKind::Known(s), stability };
        }

        let mut chunks: Vec<Chunk> = Vec::new();
        for p in parts {
            match p.kind {
                ValueKind::Known(t) => push_text(&mut chunks, t),
                ValueKind::Unknown(u) => chunks.push(Chunk::Opaque(u)),
                ValueKind::Partial(cs) => {
                    for c in cs {
                        match c {
                            Chunk::Text(t) => push_text(&mut chunks, t),
                            o => chunks.push(o),
                        }
                    }
                }
            }
        }
        // Nothing known at all is just Unknown.
        if let [Chunk::Opaque(u)] = chunks.as_slice() {
            return Value { kind: ValueKind::Unknown(u.clone()), stability };
        }
        Value { kind: ValueKind::Partial(chunks), stability }
    }

    /// A stable description of a partial value's shape, so two partials built
    /// the same way from the same holes can be recognised as possibly equal.
    pub fn shape(&self) -> Option<String> {
        let ValueKind::Partial(cs) = &self.kind else { return None };
        let mut s = String::new();
        for c in cs {
            match c {
                Chunk::Text(t) => {
                    s.push('"');
                    s.push_str(t);
                    s.push('"');
                }
                Chunk::Opaque(u) => {
                    s.push_str(&format!("<{}:{}>", u.span.file.0, u.span.start));
                }
            }
        }
        Some(s)
    }
}

fn push_text(chunks: &mut Vec<Chunk>, t: String) {
    if t.is_empty() {
        return;
    }
    if let Some(Chunk::Text(last)) = chunks.last_mut() {
        last.push_str(&t);
    } else {
        chunks.push(Chunk::Text(t));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::FileId;

    fn sp() -> Span {
        Span::new(FileId(0), 0, 1)
    }

    #[test]
    fn all_known_concatenates() {
        let v = Value::concat(vec![Value::known("a"), Value::known("b")]);
        assert_eq!(v.as_known(), Some("ab"));
    }

    #[test]
    fn a_hole_makes_it_partial_and_keeps_the_text() {
        let v = Value::concat(vec![
            Value::known("bin/"),
            Value::unknown(UnknownReason::Shell, sp()),
            Value::known("/x"),
        ]);
        assert!(v.as_known().is_none());
        let ValueKind::Partial(cs) = &v.kind else { panic!("{v:?}") };
        assert_eq!(cs.len(), 3);
        assert!(matches!(&cs[0], Chunk::Text(t) if t == "bin/"));
    }

    #[test]
    fn a_lone_hole_stays_unknown() {
        let v = Value::concat(vec![Value::known(""), Value::unknown(UnknownReason::Shell, sp())]);
        assert!(matches!(v.kind, ValueKind::Unknown(_)));
    }

    #[test]
    fn stability_is_the_worst_of_the_parts() {
        let v =
            Value::concat(vec![Value::known("a"), Value::known_with("b", Stability::Filesystem)]);
        assert_eq!(v.stability, Stability::Filesystem);
    }

    #[test]
    fn adjacent_text_merges() {
        let v = Value::concat(vec![
            Value::known("a"),
            Value::known("b"),
            Value::unknown(UnknownReason::Shell, sp()),
        ]);
        let ValueKind::Partial(cs) = &v.kind else { panic!() };
        assert!(matches!(&cs[0], Chunk::Text(t) if t == "ab"));
        assert_eq!(cs.len(), 2);
    }
}
