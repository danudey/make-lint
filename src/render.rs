//! Diagnostic output: annotated source snippets for humans, JSON for CI.

use crate::diag::{Diagnostic, Severity};
use crate::span::{SourceMap, Span};

use std::fmt::Write as _;

const TAB_WIDTH: usize = 4;

/// SGR terminal attribute selectors, used only to colour terminal output.
mod sgr {
    pub const RESET: &str = "0";
    pub const BOLD: &str = "1";
    pub const DIM: &str = "2";
    pub const RED: &str = "1;31";
    pub const YELLOW: &str = "1;33";
    pub const CYAN: &str = "1;36";
}

pub struct Style {
    pub color: bool,
}

impl Style {
    fn paint(&self, attr: &str, s: &str) -> String {
        if !self.color {
            return s.to_string();
        }
        let esc = char::from(27);
        format!("{esc}[{attr}m{s}{esc}[{}m", sgr::RESET)
    }

    fn severity(&self, sev: Severity) -> String {
        let attr = match sev {
            Severity::Error => sgr::RED,
            Severity::Warning => sgr::YELLOW,
            Severity::Note => sgr::CYAN,
        };
        self.paint(attr, sev.as_str())
    }

    fn bold(&self, s: &str) -> String {
        self.paint(sgr::BOLD, s)
    }

    fn dim(&self, s: &str) -> String {
        self.paint(sgr::DIM, s)
    }

    fn red(&self, s: &str) -> String {
        self.paint(sgr::RED, s)
    }
}

pub fn sort(diags: &mut [Diagnostic], sm: &SourceMap) {
    diags.sort_by(|a, b| {
        let pa = &sm.get(a.primary.file).path;
        let pb = &sm.get(b.primary.file).path;
        pa.cmp(pb).then(a.primary.start.cmp(&b.primary.start)).then(a.code.cmp(b.code))
    });
}

pub fn text(diags: &[Diagnostic], sm: &SourceMap, style: &Style) -> String {
    let mut out = String::new();
    for d in diags {
        let f = sm.get(d.primary.file);
        let (line, col) = f.line_col(d.primary.start);
        let path = f.path.display().to_string();
        let _ = writeln!(
            out,
            "{}: {}[{}]: {}",
            style.bold(&format!("{path}:{line}:{col}")),
            style.severity(d.severity),
            style.bold(d.code),
            d.message
        );
        snippet(&mut out, sm, d.primary, style, '^');
        for label in &d.secondary {
            let lf = sm.get(label.span.file);
            let (l, c) = lf.line_col(label.span.start);
            let p = lf.path.display().to_string();
            let _ = writeln!(out, "  {} {p}:{l}:{c}: {}", style.dim("-->"), label.message);
            snippet(&mut out, sm, label.span, style, '-');
        }
        if let Some(h) = &d.help {
            let _ = writeln!(out, "  {} {h}", style.dim("= help:"));
        }
        out.push('\n');
    }
    out
}

fn snippet(out: &mut String, sm: &SourceMap, span: Span, style: &Style, caret: char) {
    let f = sm.get(span.file);
    let li = f.line_index(span.start);
    let raw = f.line_text(li);
    let line_start = f.line_start(li);

    // Tabs are load-bearing in makefiles; expand them so carets line up.
    let mut display = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c == '\t' {
            display.push_str(&" ".repeat(TAB_WIDTH));
        } else {
            display.push(c);
        }
    }

    let col_start = display_col(raw, (span.start as usize).saturating_sub(line_start));
    let end_in_line = (span.end as usize).saturating_sub(line_start).min(raw.len());
    let col_end = display_col(raw, end_in_line).max(col_start + 1);

    let num = (li + 1).to_string();
    let gutter = " ".repeat(num.len());
    let bar = style.dim("|");
    let _ = writeln!(out, "{gutter} {bar}");
    let _ = writeln!(out, "{} {bar} {display}", style.dim(&num));
    let _ = writeln!(
        out,
        "{gutter} {bar} {}{}",
        " ".repeat(col_start),
        style.red(&caret.to_string().repeat(col_end - col_start))
    );
}

/// Display column (0-based) of byte offset `off` within `line`.
fn display_col(line: &str, off: usize) -> usize {
    let mut off = off.min(line.len());
    while off > 0 && !line.is_char_boundary(off) {
        off -= 1;
    }
    line[..off].chars().map(|c| if c == '\t' { TAB_WIDTH } else { 1 }).sum()
}

pub fn json(diags: &[Diagnostic], sm: &SourceMap) -> String {
    let mut out = String::from("[");
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let f = sm.get(d.primary.file);
        let (line, col) = f.line_col(d.primary.start);
        let (end_line, end_col) = f.line_col(d.primary.end);
        let _ = write!(
            out,
            concat!(
                r#"{{"code":{},"severity":{},"file":{},"line":{},"column":{},"#,
                r#""endLine":{},"endColumn":{},"message":{}"#
            ),
            quote(d.code),
            quote(d.severity.as_str()),
            quote(&f.path.display().to_string()),
            line,
            col,
            end_line,
            end_col,
            quote(&d.message)
        );
        if let Some(h) = &d.help {
            let _ = write!(out, r#","help":{}"#, quote(h));
        }
        if !d.secondary.is_empty() {
            out.push_str(r#","related":["#);
            for (j, l) in d.secondary.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                let lf = sm.get(l.span.file);
                let (ll, lc) = lf.line_col(l.span.start);
                let _ = write!(
                    out,
                    r#"{{"file":{},"line":{},"column":{},"message":{}}}"#,
                    quote(&lf.path.display().to_string()),
                    ll,
                    lc,
                    quote(&l.message)
                );
            }
            out.push(']');
        }
        out.push('}');
    }
    out.push_str("]\n");
    out
}

/// Minimal JSON string literal writer.
fn quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn caret_accounts_for_tabs() {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("Makefile"), "all:\n\techo $FOO\n".into());
        let off = sm.get(id).text.find("$FOO").unwrap();
        let d = Diagnostic::warn("MK010", Span::new(id, off, off + 2), "bare ref");
        let s = text(&[d], &sm, &Style { color: false });
        // Tab expands to 4 columns, then "echo " is 5 more, so `$FOO` starts at
        // display column 9.
        let caret_line = s.lines().nth(3).unwrap();
        assert_eq!(caret_line, format!("  | {}^^", " ".repeat(9)));
    }

    #[test]
    fn json_escapes_control_characters() {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("Makefile"), "A = 1\n".into());
        let d = Diagnostic::warn("MK001", Span::new(id, 0, 1), "say \"hi\"\n");
        let j = json(&[d], &sm);
        assert!(j.contains(r#""message":"say \"hi\"\n""#), "{j}");
    }
}
