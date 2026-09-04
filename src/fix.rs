//! Applying the fixes that are purely mechanical.
//!
//! Only two findings have a rewrite with a single obvious answer: `$FOO`
//! becomes `$(FOO)`, and a space-indented recipe line becomes a tab-indented
//! one. Everything else needs a decision a linter should not make on someone's
//! behalf, so no other check offers a fix.

use crate::diag::Diagnostic;
use crate::span::{FileId, SourceMap, Span};

use std::collections::BTreeMap;

/// An exact textual replacement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fix {
    pub span: Span,
    pub replacement: String,
    /// Shown in the summary, e.g. "`$FOO` to `$(FOO)`".
    pub description: String,
}

#[derive(Default, Debug)]
pub struct Applied {
    pub fixes: usize,
    pub files: usize,
    /// Fixes dropped because they overlapped one already applied.
    pub skipped: usize,
}

/// Rewrite the files in place. Returns what changed.
pub fn apply(diags: &[Diagnostic], sources: &SourceMap) -> Result<Applied, String> {
    let mut by_file: BTreeMap<FileId, Vec<&Fix>> = BTreeMap::new();
    for d in diags {
        if let Some(f) = &d.fix {
            by_file.entry(f.span.file).or_default().push(f);
        }
    }

    let mut applied = Applied::default();
    for (file, mut fixes) in by_file {
        let source = sources.get(file);
        let mut text = source.text.clone();

        // Choose a non-overlapping set first, earliest and widest preferred, so
        // a fix that covers a whole construct wins over one covering part of it.
        fixes.sort_by_key(|f| (f.span.start, std::cmp::Reverse(f.span.end)));
        let mut kept: Vec<&Fix> = Vec::with_capacity(fixes.len());
        let mut reach = 0u32;
        for f in fixes {
            let (start, end) = (f.span.start as usize, f.span.end as usize);
            let sane =
                end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end);
            if !sane || (f.span.start < reach && !kept.is_empty()) {
                applied.skipped += 1;
                continue;
            }
            reach = f.span.end;
            kept.push(f);
        }

        // Apply last first, so the offsets ahead of each edit stay valid.
        let changed = kept.len();
        for f in kept.iter().rev() {
            text.replace_range(f.span.start as usize..f.span.end as usize, &f.replacement);
        }

        if changed > 0 {
            std::fs::write(&source.path, &text)
                .map_err(|e| format!("{}: {e}", source.path.display()))?;
            applied.fixes += changed;
            applied.files += 1;
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn fixture(body: &str) -> (PathBuf, SourceMap, FileId) {
        // Tests run in parallel, so each needs its own directory.
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("make-lint-fix-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Makefile");
        std::fs::write(&path, body).unwrap();
        let mut sm = SourceMap::default();
        let id = sm.add(path.clone(), body.to_string());
        (path, sm, id)
    }

    fn with_fix(id: FileId, start: usize, end: usize, to: &str) -> Diagnostic {
        Diagnostic::warn("MK010", Span::new(id, start, end), "x").with_fix(Fix {
            span: Span::new(id, start, end),
            replacement: to.to_string(),
            description: "t".into(),
        })
    }

    #[test]
    fn several_fixes_in_one_file_all_land() {
        let body = "A = $FOO\nB = $BAR\n";
        let (path, sm, id) = fixture(body);
        let a = body.find("$FOO").unwrap();
        let b = body.find("$BAR").unwrap();
        let diags = vec![with_fix(id, a, a + 4, "$(FOO)"), with_fix(id, b, b + 4, "$(BAR)")];
        let out = apply(&diags, &sm).unwrap();
        assert_eq!((out.fixes, out.files, out.skipped), (2, 1, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A = $(FOO)\nB = $(BAR)\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn overlapping_fixes_are_skipped_rather_than_corrupting_the_file() {
        let body = "A = $FOO\n";
        let (path, sm, id) = fixture(body);
        let a = body.find("$FOO").unwrap();
        let diags = vec![with_fix(id, a, a + 4, "$(FOO)"), with_fix(id, a + 1, a + 3, "zz")];
        let out = apply(&diags, &sm).unwrap();
        assert_eq!((out.fixes, out.skipped), (1, 1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A = $(FOO)\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_file_with_no_fixes_is_not_written() {
        let body = "A = 1\n";
        let (path, sm, _) = fixture(body);
        let out = apply(&[], &sm).unwrap();
        assert_eq!((out.fixes, out.files), (0, 0));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
