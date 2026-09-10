//! Source files, byte spans, and line/column resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct FileId(pub u32);

/// A half-open byte range within one source file.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Self {
        let start = start as u32;
        Span { file, start, end: (end as u32).max(start) }
    }

    pub fn point(file: FileId, at: usize) -> Self {
        Self::new(file, at, at)
    }

    pub fn join(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span { file: self.file, start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

pub struct SourceFile {
    pub id: FileId,
    /// Canonical path: how the file is looked up, and where `--fix` writes it.
    pub path: PathBuf,
    /// The path as reported back to the caller. The same as `path` except for
    /// an editor buffer, which is named the way the editor named it —
    /// canonicalising that would hand back a path the editor cannot match
    /// (`/private/var/...` on macOS, `\\?\C:\...` on Windows).
    pub reported: PathBuf,
    pub text: String,
    line_starts: Vec<u32>,
}

impl SourceFile {
    fn new(id: FileId, path: PathBuf, reported: PathBuf, text: String) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        SourceFile { id, path, reported, text, line_starts }
    }

    /// 0-based index of the line containing `off`.
    pub fn line_index(&self, off: u32) -> usize {
        match self.line_starts.binary_search(&off) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    /// 1-based line and 1-based column, counted in characters.
    pub fn line_col(&self, off: u32) -> (usize, usize) {
        let li = self.line_index(off);
        let start = self.line_starts[li] as usize;
        let off = (off as usize).min(self.text.len());
        let col = self.text[start..off].chars().count() + 1;
        (li + 1, col)
    }

    /// Text of a 0-based line, without its trailing newline.
    pub fn line_text(&self, line_index: usize) -> &str {
        let start = self.line_starts[line_index] as usize;
        let end = self
            .line_starts
            .get(line_index + 1)
            .map(|&e| e as usize - 1)
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches('\r')
    }

    pub fn line_start(&self, line_index: usize) -> usize {
        self.line_starts[line_index] as usize
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }
}

#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
    by_path: HashMap<PathBuf, FileId>,
}

impl SourceMap {
    pub fn add(&mut self, path: PathBuf, text: String) -> FileId {
        self.add_as(path.clone(), path, text)
    }

    /// Add a file that is reported under a different path than it is keyed by.
    pub fn add_as(&mut self, path: PathBuf, reported: PathBuf, text: String) -> FileId {
        if let Some(&id) = self.by_path.get(&path) {
            return id;
        }
        let id = FileId(self.files.len() as u32);
        self.by_path.insert(path.clone(), id);
        self.files.push(SourceFile::new(id, path, reported, text));
        id
    }

    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn find(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(path).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    /// Text covered by a span.
    pub fn snippet(&self, span: Span) -> &str {
        let f = self.get(span.file);
        let start = (span.start as usize).min(f.text.len());
        let end = (span.end as usize).min(f.text.len());
        &f.text[start..end]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_and_text() {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("M"), "abc\ndef\n".into());
        let f = sm.get(id);
        assert_eq!(f.line_col(0), (1, 1));
        assert_eq!(f.line_col(4), (2, 1));
        assert_eq!(f.line_col(6), (2, 3));
        assert_eq!(f.line_text(1), "def");
    }
}
