//! Pure helpers behind make's built-in functions: `%` patterns, word and path
//! operations, and the glob that `$(wildcard)` needs.

use std::path::{Path, PathBuf};

/// Match `word` against a make pattern and return the text the `%` stood for.
/// A pattern with no `%` matches only itself.
pub fn pattern_match<'w>(pattern: &str, word: &'w str) -> Option<&'w str> {
    match pattern.split_once('%') {
        None => (pattern == word).then_some(""),
        Some((prefix, suffix)) => {
            if word.len() < prefix.len() + suffix.len() {
                return None;
            }
            let rest = word.strip_prefix(prefix)?;
            rest.strip_suffix(suffix)
        }
    }
}

/// Substitute `stem` for the `%` in `replacement`.
pub fn pattern_apply(replacement: &str, stem: &str) -> String {
    match replacement.split_once('%') {
        None => replacement.to_string(),
        Some((a, b)) => format!("{a}{stem}{b}"),
    }
}

pub fn patsubst(pattern: &str, replacement: &str, text: &str) -> String {
    join_words(text.split_whitespace().map(|w| match pattern_match(pattern, w) {
        Some(stem) => pattern_apply(replacement, stem),
        None => w.to_string(),
    }))
}

pub fn filter(patterns: &str, text: &str, keep: bool) -> String {
    let pats: Vec<&str> = patterns.split_whitespace().collect();
    join_words(
        text.split_whitespace()
            .filter(|w| pats.iter().any(|p| pattern_match(p, w).is_some()) == keep)
            .map(str::to_string),
    )
}

/// `$(sort)`: sorted, with duplicates removed.
pub fn sort_words(text: &str) -> String {
    let mut ws: Vec<&str> = text.split_whitespace().collect();
    ws.sort_unstable();
    ws.dedup();
    ws.join(" ")
}

pub fn join_words(words: impl Iterator<Item = String>) -> String {
    words.collect::<Vec<_>>().join(" ")
}

/// `$(dir)`: everything up to and including the last slash, or `./`.
pub fn dir_of(word: &str) -> String {
    match word.rfind('/') {
        Some(i) => word[..=i].to_string(),
        None => "./".to_string(),
    }
}

pub fn notdir_of(word: &str) -> String {
    match word.rfind('/') {
        Some(i) => word[i + 1..].to_string(),
        None => word.to_string(),
    }
}

/// The suffix of a word, including the dot. Empty when it has none. A dot in a
/// directory component does not count.
pub fn suffix_of(word: &str) -> &str {
    let base_at = word.rfind('/').map_or(0, |i| i + 1);
    match word[base_at..].rfind('.') {
        Some(i) => &word[base_at + i..],
        None => "",
    }
}

/// The word with its suffix removed.
pub fn basename_of(word: &str) -> &str {
    let s = suffix_of(word);
    &word[..word.len() - s.len()]
}

/// `$(join a,b)`: concatenate the words pairwise, keeping any extras.
pub fn join_lists(a: &str, b: &str) -> String {
    let (mut ai, mut bi) = (a.split_whitespace(), b.split_whitespace());
    let mut out = Vec::new();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => break,
            (x, y) => out.push(format!("{}{}", x.unwrap_or(""), y.unwrap_or(""))),
        }
    }
    out.join(" ")
}

/// Collapse `.` and `..` without touching the filesystem.
pub fn normalise(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            std::path::Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Globbing for $(wildcard)
// ---------------------------------------------------------------------------

/// Longest pattern `fnmatch` will consider, to bound backtracking.
const MAX_PATTERN: usize = 256;

pub fn has_meta(s: &str) -> bool {
    s.contains(['*', '?', '['])
}

/// Shell-style match of one path component. Supports `*`, `?` and `[...]`.
pub fn fnmatch(pattern: &str, name: &str) -> bool {
    if pattern.len() > MAX_PATTERN || name.len() > MAX_PATTERN {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    match_chars(&p, &n)
}

fn match_chars(pat: &[char], s: &[char]) -> bool {
    match pat.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|i| match_chars(&pat[1..], &s[i..])),
        Some('?') => !s.is_empty() && match_chars(&pat[1..], &s[1..]),
        Some('[') => match bracket(pat) {
            Some((set, negated, rest)) => {
                !s.is_empty()
                    && (in_set(&set, s[0]) != negated)
                    && match_chars(&pat[rest..], &s[1..])
            }
            // An unterminated `[` is a literal.
            None => !s.is_empty() && s[0] == '[' && match_chars(&pat[1..], &s[1..]),
        },
        Some(&c) => !s.is_empty() && s[0] == c && match_chars(&pat[1..], &s[1..]),
    }
}

/// Parse `[...]` starting at `pat[0]`, returning its members, whether it is
/// negated, and the index just past the closing bracket.
fn bracket(pat: &[char]) -> Option<(Vec<char>, bool, usize)> {
    let mut i = 1;
    let negated = matches!(pat.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    let mut set = Vec::new();
    // A `]` immediately after the opener is a literal member.
    if pat.get(i) == Some(&']') {
        set.push(']');
        i += 1;
    }
    while let Some(&c) = pat.get(i) {
        if c == ']' {
            return Some((set, negated, i + 1));
        }
        if pat.get(i + 1) == Some(&'-') && pat.get(i + 2).is_some_and(|&e| e != ']') {
            set.push('\u{0}'); // range marker
            set.push(c);
            set.push(pat[i + 2]);
            i += 3;
        } else {
            set.push(c);
            i += 1;
        }
    }
    None
}

fn in_set(set: &[char], c: char) -> bool {
    let mut i = 0;
    while i < set.len() {
        if set[i] == '\u{0}' && i + 2 < set.len() {
            if set[i + 1] <= c && c <= set[i + 2] {
                return true;
            }
            i += 3;
        } else {
            if set[i] == c {
                return true;
            }
            i += 1;
        }
    }
    false
}

/// Expand a glob against the filesystem, as `$(wildcard)` does. Results are
/// relative to `base` when the pattern is, and sorted for reproducibility.
///
/// This only reads directory listings. It is the one filesystem access the
/// evaluator makes, and it runs nothing.
pub fn glob(base: &Path, pattern: &str) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let root = if absolute { PathBuf::from("/") } else { base.to_path_buf() };
    let comps: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return Vec::new();
    }

    let mut current = vec![root];
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        let mut next = Vec::new();
        for dir in &current {
            if has_meta(comp) {
                let Ok(entries) = std::fs::read_dir(dir) else { continue };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else { continue };
                    // A leading dot must be matched explicitly, as in the shell.
                    if name.starts_with('.') && !comp.starts_with('.') {
                        continue;
                    }
                    if fnmatch(comp, name) {
                        next.push(dir.join(name));
                    }
                }
            } else {
                let p = dir.join(comp);
                if p.symlink_metadata().is_ok() {
                    next.push(p);
                }
            }
        }
        if !last {
            next.retain(|p| p.is_dir());
        }
        current = next;
        if current.is_empty() {
            return Vec::new();
        }
    }

    let mut out: Vec<String> = current
        .iter()
        .map(|p| match p.strip_prefix(base) {
            Ok(r) if !absolute => r.to_string_lossy().into_owned(),
            _ => p.to_string_lossy().into_owned(),
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert_eq!(pattern_match("%.c", "a.c"), Some("a"));
        assert_eq!(pattern_match("%.c", "a.o"), None);
        assert_eq!(pattern_match("src/%.c", "src/a.c"), Some("a"));
        assert_eq!(pattern_match("a.c", "a.c"), Some(""));
        assert_eq!(pattern_match("%.c", ".c"), Some(""));
    }

    #[test]
    fn patsubst_leaves_non_matching_words() {
        assert_eq!(patsubst("%.c", "%.o", "a.c b.h c.c"), "a.o b.h c.o");
        assert_eq!(patsubst("%.c", "x", "a.c b.c"), "x x");
    }

    #[test]
    fn filtering() {
        assert_eq!(filter("%.c %.h", "a.c b.o c.h", true), "a.c c.h");
        assert_eq!(filter("%.c", "a.c b.o", false), "b.o");
    }

    #[test]
    fn sorting_deduplicates() {
        assert_eq!(sort_words("b a c a"), "a b c");
    }

    #[test]
    fn path_pieces() {
        assert_eq!(dir_of("src/a.c"), "src/");
        assert_eq!(dir_of("a.c"), "./");
        assert_eq!(notdir_of("src/a.c"), "a.c");
        assert_eq!(suffix_of("src/a.c"), ".c");
        assert_eq!(suffix_of("src.d/a"), "");
        assert_eq!(basename_of("src/a.c"), "src/a");
        assert_eq!(basename_of("src.d/a"), "src.d/a");
    }

    #[test]
    fn joining_keeps_extra_words() {
        assert_eq!(join_lists("a b", "1 2"), "a1 b2");
        assert_eq!(join_lists("a b c", "1"), "a1 b c");
    }

    #[test]
    fn globbing_patterns() {
        assert!(fnmatch("*.c", "a.c"));
        assert!(!fnmatch("*.c", "a.o"));
        assert!(fnmatch("a?c", "abc"));
        assert!(fnmatch("[abc]x", "bx"));
        assert!(!fnmatch("[abc]x", "dx"));
        assert!(fnmatch("[!abc]x", "dx"));
        assert!(fnmatch("[a-f]x", "cx"));
        assert!(!fnmatch("[a-f]x", "zx"));
        assert!(fnmatch("*", ""));
        assert!(fnmatch("a*b*c", "axxbyyc"));
    }

    #[test]
    fn glob_reads_a_real_directory() {
        let dir = std::env::temp_dir().join(format!("make-lint-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.c"), "").unwrap();
        std::fs::write(dir.join("src/b.c"), "").unwrap();
        std::fs::write(dir.join("src/c.h"), "").unwrap();
        std::fs::write(dir.join("src/.hidden.c"), "").unwrap();

        assert_eq!(glob(&dir, "src/*.c"), vec!["src/a.c", "src/b.c"]);
        assert_eq!(glob(&dir, "src/a.c"), vec!["src/a.c"]);
        assert!(glob(&dir, "src/*.zz").is_empty());
        assert!(glob(&dir, "nope/*.c").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn normalising_paths() {
        assert_eq!(normalise(Path::new("a/./b/../c")), PathBuf::from("a/c"));
    }
}
