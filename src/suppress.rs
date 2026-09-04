//! `# make-lint: disable=...` comments.
//!
//! Read from the raw source rather than the tree, because the lexer strips
//! comments long before a check runs.
//!
//! A marker at the end of a line of content suppresses findings on that line. A
//! marker on a line of its own suppresses the next line. `disable-file`
//! suppresses the whole file. With no `=`, every code is suppressed.

use crate::diag::{Diagnostic, Severity};
use crate::rules;
use crate::span::{FileId, SourceMap};

use std::collections::{HashMap, HashSet};

const MARKER: &str = "make-lint:";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Codes {
    All,
    Only(HashSet<String>),
}

impl Codes {
    fn covers(&self, code: &str) -> bool {
        match self {
            Codes::All => true,
            Codes::Only(set) => set.contains(code),
        }
    }
}

#[derive(Default)]
pub struct Suppressions {
    /// Keyed by file and 1-based line.
    lines: HashMap<(FileId, usize), Codes>,
    files: HashMap<FileId, Codes>,
    /// Codes named in a comment that no rule matches, so a silent no-op can be
    /// reported rather than left to be discovered later.
    pub unknown: Vec<(FileId, usize, String)>,
}

impl Suppressions {
    pub fn scan(sources: &SourceMap) -> Suppressions {
        let mut s = Suppressions::default();
        for file in sources.iter() {
            for (i, text) in file.text.lines().enumerate() {
                let line = i + 1;
                let Some(at) = text.find(MARKER) else { continue };
                // The marker only counts inside a comment.
                let Some(hash) = text[..at].rfind('#') else { continue };

                let directive = text[at + MARKER.len()..].trim();
                let (word, list) = match directive.split_once('=') {
                    Some((w, l)) => (w.trim(), Some(l)),
                    None => (directive.split_whitespace().next().unwrap_or(""), None),
                };

                let codes = match list {
                    None => Codes::All,
                    Some(l) => {
                        let mut set = HashSet::new();
                        for raw in l.split(',') {
                            let key = raw.trim();
                            if key.is_empty() {
                                continue;
                            }
                            match rules::lookup(key) {
                                Some(r) => {
                                    set.insert(r.code.to_string());
                                }
                                None => s.unknown.push((file.id, line, key.to_string())),
                            }
                        }
                        Codes::Only(set)
                    }
                };

                match word {
                    "disable-file" => {
                        s.files.insert(file.id, codes);
                    }
                    "disable" => {
                        // A marker with content before it belongs to this line;
                        // on a line of its own it speaks for the next.
                        let own_line = text[..hash].trim().is_empty();
                        let target = if own_line { line + 1 } else { line };
                        s.lines.insert((file.id, target), codes);
                    }
                    _ => s.unknown.push((file.id, line, word.to_string())),
                }
            }
        }
        s
    }

    fn suppresses(&self, file: FileId, line: usize, code: &str) -> bool {
        if self.files.get(&file).is_some_and(|c| c.covers(code)) {
            return true;
        }
        self.lines.get(&(file, line)).is_some_and(|c| c.covers(code))
    }

    /// Remove suppressed diagnostics, returning how many went.
    pub fn apply(&self, diags: &mut Vec<Diagnostic>, sources: &SourceMap) -> usize {
        let before = diags.len();
        diags.retain(|d| {
            let (line, _) = sources.get(d.primary.file).line_col(d.primary.start);
            !self.suppresses(d.primary.file, line, d.code)
        });
        before - diags.len()
    }

    /// A comment naming a rule that does not exist silently does nothing, which
    /// is worth saying out loud.
    pub fn report_unknown(&self, sources: &SourceMap, out: &mut Vec<Diagnostic>) {
        for (file, line, key) in &self.unknown {
            let start = sources.get(*file).line_start(line - 1);
            let end = start + sources.get(*file).line_text(line - 1).len();
            out.push(
                Diagnostic::new(
                    "MK097",
                    Severity::Warning,
                    crate::span::Span::new(*file, start, end),
                    format!("`{key}` is not a rule, so this comment suppresses nothing"),
                )
                .with_help("run `make-lint --list-rules` to see the codes"),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;
    use std::path::PathBuf;

    fn setup(text: &str) -> (SourceMap, FileId, Suppressions) {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("Makefile"), text.to_string());
        let s = Suppressions::scan(&sm);
        (sm, id, s)
    }

    fn diag_on(sm: &SourceMap, id: FileId, line: usize, code: &'static str) -> Diagnostic {
        let start = sm.get(id).line_start(line - 1);
        Diagnostic::warn(code, Span::new(id, start, start + 1), "x")
    }

    #[test]
    fn a_trailing_marker_covers_its_own_line() {
        let (sm, id, s) = setup("A := 1  # make-lint: disable=MK006\nB := 2\n");
        let mut d = vec![diag_on(&sm, id, 1, "MK006"), diag_on(&sm, id, 2, "MK006")];
        assert_eq!(s.apply(&mut d, &sm), 1);
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn a_marker_on_its_own_line_covers_the_next() {
        let (sm, id, s) = setup("# make-lint: disable=MK006\nA := 1\nB := 2\n");
        let mut d = vec![diag_on(&sm, id, 2, "MK006"), diag_on(&sm, id, 3, "MK006")];
        assert_eq!(s.apply(&mut d, &sm), 1);
        assert_eq!(d[0].primary.start, sm.get(id).line_start(2) as u32);
    }

    #[test]
    fn only_the_named_codes_are_suppressed() {
        let (sm, id, s) = setup("A := 1 # make-lint: disable=MK006,MK010\n");
        let mut d = vec![
            diag_on(&sm, id, 1, "MK006"),
            diag_on(&sm, id, 1, "MK010"),
            diag_on(&sm, id, 1, "MK003"),
        ];
        s.apply(&mut d, &sm);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, "MK003");
    }

    #[test]
    fn a_bare_disable_covers_everything_on_the_line() {
        let (sm, id, s) = setup("A := 1 # make-lint: disable\n");
        let mut d = vec![diag_on(&sm, id, 1, "MK006"), diag_on(&sm, id, 1, "MK003")];
        s.apply(&mut d, &sm);
        assert!(d.is_empty());
    }

    #[test]
    fn disable_file_covers_the_whole_file() {
        let (sm, id, s) = setup("# make-lint: disable-file=MK006\nA := 1\nB := 2\n");
        let mut d = vec![diag_on(&sm, id, 2, "MK006"), diag_on(&sm, id, 3, "MK006")];
        s.apply(&mut d, &sm);
        assert!(d.is_empty());
    }

    #[test]
    fn a_slug_works_as_well_as_a_code() {
        let (sm, id, s) = setup("A := 1 # make-lint: disable=duplicate-value\n");
        let mut d = vec![diag_on(&sm, id, 1, "MK006")];
        s.apply(&mut d, &sm);
        assert!(d.is_empty());
    }

    #[test]
    fn a_marker_outside_a_comment_is_ignored() {
        let (sm, id, s) = setup("A := make-lint: disable=MK006\n");
        let mut d = vec![diag_on(&sm, id, 1, "MK006")];
        assert_eq!(s.apply(&mut d, &sm), 0);
    }

    #[test]
    fn an_unknown_rule_is_reported_rather_than_ignored() {
        let (sm, _, s) = setup("A := 1 # make-lint: disable=MK999\n");
        let mut out = Vec::new();
        s.report_unknown(&sm, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].code, "MK097");
        assert!(out[0].message.contains("MK999"));
    }
}
