//! The rule catalogue.
//!
//! One place that knows every code, so `--explain`, `--list-rules`, SARIF's
//! rule metadata and the config file's severity overrides all agree, and a
//! typo in a config file can be rejected instead of silently ignored.

use crate::diag::Severity;

pub struct Rule {
    pub code: &'static str,
    /// Stable slug, used as SARIF's rule name.
    pub name: &'static str,
    /// Level shown in listings and in SARIF's default configuration. A few
    /// rules choose a lower level per finding; this is the highest they use.
    pub severity: Severity,
    pub summary: &'static str,
    pub explanation: &'static str,
}

use Severity::{Error, Note, Warning};

pub static RULES: &[Rule] = &[
    Rule {
        code: "MK001",
        name: "undefined-variable",
        severity: Warning,
        summary: "a variable is read but never assigned",
        explanation: "\
Make expands an unset variable to the empty string, so the mistake is silent.
It is only a warning when it looks like a real misspelling: one name read and
never assigned, and its near-twin assigned and never read. Otherwise it is a
note, because make has no way to declare that a variable arrives from the
environment or the command line, and many do.",
    },
    Rule {
        code: "MK002",
        name: "unused-variable",
        severity: Note,
        summary: "a variable is assigned and never read",
        explanation: "\
Reported only for the file being linted. An included library defines variables
for makefiles that are not in this workspace, and nothing here can see whether
they are read. Exported variables, `?=` defaults and `override` are skipped,
since all three say something outside sets or reads them.",
    },
    Rule {
        code: "MK003",
        name: "clobbered-value",
        severity: Warning,
        summary: "an assignment discards a value nothing had read",
        explanation: "\
The earlier value was computed and then thrown away without ever being used, so
either it was pointless or the second assignment was meant to be `?=` or `+=`.
An override inside a conditional does not count, nor does overriding a `?=`
default: both are normal ways to vary a value.",
    },
    Rule {
        code: "MK004",
        name: "append-before-definition",
        severity: Note,
        summary: "`+=` used before the variable has any definition",
        explanation: "\
Make treats it as a plain assignment, picking up an environment value if there
is one. Usually deliberate, occasionally a sign that a definition was meant to
come first.",
    },
    Rule {
        code: "MK005",
        name: "deferred-shell",
        severity: Warning,
        summary: "`VAR = $(shell ...)` re-runs the command on every expansion",
        explanation: "\
A recursive assignment stores the text, not the result, so the command runs
again each time the variable is read. Reported once a variable is read at least
twice. `:=` runs it once while the makefile is read.",
    },
    Rule {
        code: "MK006",
        name: "duplicate-value",
        severity: Warning,
        summary: "two variables independently resolve to the same value",
        explanation: "\
Neither derives from the other, so the value is written down twice and can drift.
Only distinctive values count: a compound value like a repository path is
evidence, a bare word like `latest` is a token many variables hold for unrelated
reasons. Values that change between runs are ignored.",
    },
    Rule {
        code: "MK007",
        name: "alias",
        severity: Note,
        summary: "a variable is a second live name for another",
        explanation: "\
`A = $(B)` where both names are used. Harmless, but a reader has to know they
are the same thing.",
    },
    Rule {
        code: "MK008",
        name: "duplicate-expression",
        severity: Note,
        summary: "two variables are defined by the same expression",
        explanation: "\
The value could not be resolved, so the expressions are compared instead. The
claim is exact: the same expression is written twice. Only definitions of the
same flavour are compared, since `:=` freezes a value where it stands.",
    },
    Rule {
        code: "MK009",
        name: "self-reference",
        severity: Error,
        summary: "a recursive variable refers to itself",
        explanation: "\
Make refuses this outright with \"Recursive variable references itself
(eventually)\". Use `:=` to capture the previous value instead.",
    },
    Rule {
        code: "MK010",
        name: "bare-dollar",
        severity: Warning,
        summary: "`$FOO` is `$(F)` followed by the literal text `OO`",
        explanation: "\
Make reads `$` plus one character as a variable reference. Write `$(FOO)` for
the make variable, or `$$FOO` for a shell variable inside a recipe.",
    },
    Rule {
        code: "MK020",
        name: "duplicate-recipe",
        severity: Warning,
        summary: "a target has more than one recipe",
        explanation: "\
Make keeps the last recipe and discards the earlier one, warning as it goes. Use
`::` if both were meant to run. Definitions in opposite branches of one
conditional do not count, since make only ever sees one.",
    },
    Rule {
        code: "MK021",
        name: "unmakeable-prerequisite",
        severity: Warning,
        summary: "a prerequisite has no rule, no file, and no pattern that covers it",
        explanation: "\
Make will fail with \"No rule to make target\". Bare words are not judged: they
are almost always phony targets an including makefile defines. Neither are
globs, which make expands itself.",
    },
    Rule {
        code: "MK025",
        name: "space-indented-recipe",
        severity: Error,
        summary: "a recipe line is indented with spaces",
        explanation: "\
Make only recognises a recipe when the tab is the first character of the line.
Spaces, or spaces before the tab, mean the line is not part of the recipe.",
    },
    Rule {
        code: "MK026",
        name: "recipe-outside-rule",
        severity: Error,
        summary: "a recipe line appears before any rule",
        explanation: "\
Make reports this as \"recipe commences before first target\".",
    },
    Rule {
        code: "MK030",
        name: "undefined-macro",
        severity: Warning,
        summary: "`$(call f,...)` names a macro that is never defined",
        explanation: "\
Make expands an undefined macro to the empty string. A warning when the name is
a likely misspelling of one that exists; otherwise a note, since a makefile
meant to be included may get the macro from its parent.",
    },
    Rule {
        code: "MK031",
        name: "suspicious-variable-name",
        severity: Warning,
        summary: "a variable reference that is really a mistyped function call",
        explanation: "\
Covers a misspelt built-in, a function name used with no arguments, and a name
containing a space. All three expand to the empty string. `$(id -u)` in a recipe
is the common case: that is a make variable named `id -u`, not shell command
substitution, which would be `$$(id -u)`.",
    },
    Rule {
        code: "MK032",
        name: "function-arity",
        severity: Error,
        summary: "too few arguments to a built-in function",
        explanation: "Make reports \"insufficient number of arguments\".",
    },
    Rule {
        code: "MK033",
        name: "unterminated-reference",
        severity: Error,
        summary: "a `$(` or `${` is never closed",
        explanation: "\
Make swallows the rest of the line, and usually the rest of the file, looking
for the closing bracket.",
    },
    Rule {
        code: "MK034",
        name: "unmatched-define",
        severity: Error,
        summary: "`define` without `endef`, or the reverse",
        explanation: "The body of the definition is not what it looks like.",
    },
    Rule {
        code: "MK035",
        name: "unmatched-conditional",
        severity: Error,
        summary: "an unmatched `else` or `endif`, or a missing `endif`",
        explanation: "Make cannot tell where the conditional ends.",
    },
    Rule {
        code: "MK036",
        name: "malformed-condition",
        severity: Error,
        summary: "a malformed `ifeq` or `ifdef` condition",
        explanation: "\
`ifeq` takes `(a,b)` or two quoted strings; `ifdef` takes a variable name.",
    },
    Rule {
        code: "MK040",
        name: "command-not-run",
        severity: Note,
        summary: "a `$(shell ...)` command was not run",
        explanation: "\
Only commands that can be proven read-only are run. Anything else leaves its
value unknown, and every check that would have used it stands down. The message
gives the reason; `--allow-command NAME` permits one more command.",
    },
    Rule {
        code: "MK050",
        name: "missing-include",
        severity: Warning,
        summary: "a non-optional `include` names a file that does not exist",
        explanation: "\
Make fails unless something creates it first. An `include` of a file that is
itself a target is make's remaking feature and is not reported, nor is one
guarded by a conditional.",
    },
    Rule {
        code: "MK097",
        name: "unknown-suppression",
        severity: Warning,
        summary: "a suppression comment names a rule that does not exist",
        explanation: "\
The comment silently does nothing, which is worse than an error: the reader
believes a finding is handled when it is not.",
    },
    Rule {
        code: "MK098",
        name: "generated-file",
        severity: Note,
        summary: "the file announces itself as generated",
        explanation: "\
Advice about how a generated file is written has nowhere to go: the generator
decides. Syntax and correctness checks still run.",
    },
    Rule {
        code: "MK099",
        name: "unparsed-line",
        severity: Note,
        summary: "a line the parser could not classify",
        explanation: "\
Not necessarily wrong, but nothing on that line was analysed. Worth reporting as
a make-lint gap.",
    },
];

pub fn rule(code: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.code == code)
}

/// Accepts either the code or the slug, so config files can use either.
pub fn lookup(key: &str) -> Option<&'static Rule> {
    RULES.iter().find(|r| r.code.eq_ignore_ascii_case(key) || r.name == key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn codes_and_names_are_unique() {
        let mut codes = HashSet::new();
        let mut names = HashSet::new();
        for r in RULES {
            assert!(codes.insert(r.code), "duplicate code {}", r.code);
            assert!(names.insert(r.name), "duplicate name {}", r.name);
            assert!(!r.summary.is_empty() && !r.explanation.is_empty(), "{}", r.code);
        }
    }

    #[test]
    fn lookup_accepts_a_code_or_a_slug() {
        assert_eq!(lookup("MK006").unwrap().name, "duplicate-value");
        assert_eq!(lookup("mk006").unwrap().code, "MK006");
        assert_eq!(lookup("duplicate-value").unwrap().code, "MK006");
        assert!(lookup("MK999").is_none());
    }
}
