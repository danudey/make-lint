//! GNU Make built-in function table and automatic variables.
//!
//! Arities follow GNU Make's own `function_table`. A `max_args` of 1 means make
//! does **not** split the body on commas at all (`$(shell echo a,b)` passes
//! `echo a,b` verbatim). When there are more commas than `max_args - 1`, the
//! final argument absorbs the remainder.

pub const UNLIMITED: usize = usize::MAX;

#[derive(Copy, Clone, Debug)]
pub struct Func {
    pub name: &'static str,
    pub min_args: usize,
    pub max_args: usize,
}

pub static FUNCTIONS: &[Func] = &[
    Func { name: "abspath", min_args: 1, max_args: 1 },
    Func { name: "addprefix", min_args: 2, max_args: 2 },
    Func { name: "addsuffix", min_args: 2, max_args: 2 },
    Func { name: "and", min_args: 1, max_args: UNLIMITED },
    Func { name: "basename", min_args: 1, max_args: 1 },
    Func { name: "call", min_args: 1, max_args: UNLIMITED },
    Func { name: "dir", min_args: 1, max_args: 1 },
    Func { name: "error", min_args: 1, max_args: 1 },
    Func { name: "eval", min_args: 1, max_args: 1 },
    Func { name: "file", min_args: 1, max_args: 2 },
    Func { name: "filter", min_args: 2, max_args: 2 },
    Func { name: "filter-out", min_args: 2, max_args: 2 },
    Func { name: "findstring", min_args: 2, max_args: 2 },
    Func { name: "firstword", min_args: 1, max_args: 1 },
    Func { name: "flavor", min_args: 1, max_args: 1 },
    Func { name: "foreach", min_args: 3, max_args: 3 },
    Func { name: "guile", min_args: 1, max_args: 1 },
    Func { name: "if", min_args: 2, max_args: 3 },
    Func { name: "info", min_args: 1, max_args: 1 },
    Func { name: "intcmp", min_args: 2, max_args: 5 },
    Func { name: "join", min_args: 2, max_args: 2 },
    Func { name: "lastword", min_args: 1, max_args: 1 },
    Func { name: "let", min_args: 3, max_args: 3 },
    Func { name: "notdir", min_args: 1, max_args: 1 },
    Func { name: "or", min_args: 1, max_args: UNLIMITED },
    Func { name: "origin", min_args: 1, max_args: 1 },
    Func { name: "patsubst", min_args: 3, max_args: 3 },
    Func { name: "realpath", min_args: 1, max_args: 1 },
    Func { name: "shell", min_args: 1, max_args: 1 },
    Func { name: "sort", min_args: 1, max_args: 1 },
    Func { name: "strip", min_args: 1, max_args: 1 },
    Func { name: "subst", min_args: 3, max_args: 3 },
    Func { name: "suffix", min_args: 1, max_args: 1 },
    Func { name: "value", min_args: 1, max_args: 1 },
    Func { name: "warning", min_args: 1, max_args: 1 },
    Func { name: "wildcard", min_args: 1, max_args: 1 },
    Func { name: "word", min_args: 2, max_args: 2 },
    Func { name: "wordlist", min_args: 3, max_args: 3 },
    Func { name: "words", min_args: 1, max_args: 1 },
];

pub fn function(name: &str) -> Option<&'static Func> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

/// Single-character automatic variables (`$@`, `$<`, ...) plus their `D`/`F`
/// suffixed forms, which appear as `$(@D)`.
pub static AUTOMATIC: &[&str] = &["@", "%", "<", "?", "^", "+", "|", "*"];

pub fn is_automatic(name: &str) -> bool {
    match name.len() {
        1 => AUTOMATIC.contains(&name),
        2 => {
            let (head, tail) = name.split_at(1);
            AUTOMATIC.contains(&head) && (tail == "D" || tail == "F")
        }
        _ => false,
    }
}

/// Directive keywords recognised at the start of a logical line.
pub static DIRECTIVES: &[&str] = &[
    "include", "-include", "sinclude", "ifeq", "ifneq", "ifdef", "ifndef", "else", "endif",
    "define", "endef", "override", "export", "unexport", "private", "undefine", "vpath", "load",
    "-load",
];

pub fn is_conditional_directive(word: &str) -> bool {
    matches!(word, "ifeq" | "ifneq" | "ifdef" | "ifndef" | "else" | "endif")
}

/// Levenshtein distance, capped for early exit.
pub fn edit_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > max {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[b.len()];
    (d <= max).then_some(d)
}

/// Closest built-in function name to `name`, within `max` edits.
pub fn nearest_function(name: &str, max: usize) -> Option<&'static str> {
    FUNCTIONS
        .iter()
        .filter_map(|f| edit_distance(name, f.name, max).map(|d| (d, f.name)))
        .min_by_key(|&(d, n)| (d, n))
        .map(|(_, n)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arities() {
        assert_eq!(function("patsubst").unwrap().min_args, 3);
        assert_eq!(function("shell").unwrap().max_args, 1);
        assert!(function("nope").is_none());
    }

    #[test]
    fn automatics() {
        assert!(is_automatic("@"));
        assert!(is_automatic("@D"));
        assert!(!is_automatic("A"));
        assert!(!is_automatic("@X"));
    }

    #[test]
    fn typos() {
        assert_eq!(nearest_function("pastsubst", 2), Some("patsubst"));
        assert_eq!(nearest_function("filterout", 2), Some("filter-out"));
        assert_eq!(nearest_function("zzzzzzzzzz", 2), None);
    }
}
