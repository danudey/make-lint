//! Loads a makefile and everything it statically includes.

use crate::ast::{Include, Item, Makefile};
use crate::diag::Diagnostic;
use crate::parser;
use crate::span::{FileId, SourceMap, Span};

use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct Workspace {
    pub sources: SourceMap,
    /// Parsed trees indexed by `FileId`.
    files: Vec<Option<Makefile>>,
    pub diags: Vec<Diagnostic>,
    pub include_dirs: Vec<PathBuf>,
    /// Roots given on the command line, in order.
    pub roots: Vec<FileId>,
}

impl Workspace {
    pub fn new(include_dirs: Vec<PathBuf>) -> Self {
        Workspace { include_dirs, ..Default::default() }
    }

    pub fn get(&self, id: FileId) -> Option<&Makefile> {
        self.files.get(id.0 as usize).and_then(|f| f.as_ref())
    }

    pub fn makefiles(&self) -> impl Iterator<Item = &Makefile> {
        self.files.iter().filter_map(|f| f.as_ref())
    }

    /// Rules in the order make reads them, descending into each `include` at
    /// the point it appears. Checks that care about "which definition wins"
    /// must use this rather than file-load order, since an included file is
    /// read before the rest of its includer.
    pub fn rules_in_read_order(&self) -> Vec<(FileId, usize)> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for &root in &self.roots {
            self.visit_file(root, &mut out, &mut seen);
        }
        out
    }

    fn visit_file(
        &self,
        id: FileId,
        out: &mut Vec<(FileId, usize)>,
        seen: &mut std::collections::HashSet<FileId>,
    ) {
        if !seen.insert(id) {
            return;
        }
        if let Some(mf) = self.get(id) {
            self.visit_items(id, &mf.items, out, seen);
        }
    }

    fn visit_items(
        &self,
        id: FileId,
        items: &[Item],
        out: &mut Vec<(FileId, usize)>,
        seen: &mut std::collections::HashSet<FileId>,
    ) {
        for it in items {
            match it {
                Item::Rule(i) => out.push((id, *i)),
                Item::Include(inc) => {
                    for p in &inc.resolved {
                        if let Some(inc_id) = self.sources.find(p) {
                            self.visit_file(inc_id, out, seen);
                        }
                    }
                }
                Item::Conditional(c) => {
                    for b in &c.branches {
                        self.visit_items(id, &b.body, out, seen);
                    }
                    if let Some(e) = &c.else_body {
                        self.visit_items(id, e, out, seen);
                    }
                }
                _ => {}
            }
        }
    }

    /// Read, parse, and recursively follow statically resolvable includes.
    pub fn load_root(&mut self, path: &Path) -> Result<FileId, std::io::Error> {
        let id = self.load(path, None)?;
        self.roots.push(id);
        Ok(id)
    }

    /// Load a root whose text came from somewhere other than the file itself —
    /// an editor buffer with unsaved edits. `path` is where that buffer would
    /// be saved: it never has to exist, but it fixes the directory the
    /// buffer's `include` directives resolve against, and those are still read
    /// from disk.
    pub fn load_root_text(&mut self, path: &Path, text: String) -> FileId {
        let key = canonical_key(path);
        // Reported as the editor named it: it has to match the buffer's own
        // path for the diagnostics to land back on it.
        let id = self.add_parsed(key, Some(path.to_path_buf()), text);
        self.roots.push(id);
        id
    }

    fn load(&mut self, path: &Path, from: Option<Span>) -> Result<FileId, std::io::Error> {
        let key = canonical_key(path);
        if let Some(id) = self.sources.find(&key) {
            return Ok(id);
        }
        let text = std::fs::read_to_string(path).map_err(|e| {
            if let Some(span) = from {
                self.diags.push(Diagnostic::error(
                    "MK050",
                    span,
                    format!("cannot read included file `{}`: {e}", path.display()),
                ));
            }
            e
        })?;
        Ok(self.add_parsed(key, None, text))
    }

    /// Parse one already-read file into the workspace and queue its includes.
    /// `reported` overrides the path the file is named by in output; without
    /// one it is named by its key.
    fn add_parsed(&mut self, key: PathBuf, reported: Option<PathBuf>, text: String) -> FileId {
        let reported = reported.unwrap_or_else(|| key.clone());
        let id = self.sources.add_as(key.clone(), reported, text);
        let text = self.sources.get(id).text.clone();
        let (mut mf, diags) = parser::parse(id, &text);
        self.diags.extend(diags);

        let base = key.parent().map(Path::to_path_buf).unwrap_or_default();
        let dirs = self.include_dirs.clone();
        let mut queue: Vec<(PathBuf, Span)> = Vec::new();
        mf.walk_items_mut(&mut |it| {
            if let Item::Include(inc) = it {
                resolve_include(inc, &base, &dirs, &mut queue);
            }
        });

        let slot = id.0 as usize;
        if self.files.len() <= slot {
            self.files.resize_with(slot + 1, || None);
        }
        self.files[slot] = Some(mf);

        for (p, span) in queue {
            let _ = self.load(&p, Some(span));
        }
        id
    }
}

/// The key a file is stored under in the source map. Canonical where the file
/// exists; otherwise absolute, since an unsaved buffer still needs its
/// includes resolved from the directory it will be saved in.
fn canonical_key(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path.to_path_buf(),
    }
}

fn resolve_include(
    inc: &mut Include,
    base: &Path,
    dirs: &[PathBuf],
    queue: &mut Vec<(PathBuf, Span)>,
) {
    let Some(list) = inc.paths.literal() else {
        inc.dynamic = true;
        return;
    };
    for word in list.split_whitespace() {
        // A wildcard would need `$(wildcard)` semantics; leave it to phase 2.
        if word.contains(['*', '?', '[']) {
            inc.dynamic = true;
            continue;
        }
        let mut found = None;
        for dir in std::iter::once(base).chain(dirs.iter().map(PathBuf::as_path)) {
            let cand = dir.join(word);
            if cand.is_file() {
                found = Some(cand);
                break;
            }
        }
        match found {
            Some(p) => {
                queue.push((p.clone(), inc.span));
                // Store the canonical form: it is the key the source map uses,
                // so `rules_in_read_order` can look the file back up.
                inc.resolved.push(std::fs::canonicalize(&p).unwrap_or(p));
            }
            None => inc.missing.push(word.to_string()),
        }
    }
}

impl Workspace {
    /// The directory make would run in: the first root makefile's directory.
    /// Relative paths in prerequisites and `$(wildcard)` resolve against it.
    pub fn base_dir(&self) -> PathBuf {
        self.roots
            .first()
            .and_then(|&r| self.sources.get(r).path.parent().map(Path::to_path_buf))
            .unwrap_or_default()
    }
}
