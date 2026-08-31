//! Physical-line reader that assembles *logical* lines.
//!
//! Makefile lexing is parser-driven: whether a line is a recipe, a normal line,
//! or a raw `define` body line depends on parser context, so the parser calls
//! [`Lexer::next_line`] with an explicit [`Mode`].
//!
//! Continuations and comment stripping change byte offsets, so every logical
//! line carries a segment table mapping offsets in the assembled text back to
//! offsets in the original file. All spans reported by the parser go through
//! [`LogicalLine::span_of`], so diagnostics point at real source.

use crate::span::{FileId, Span};

#[derive(Copy, Clone, Debug)]
struct Seg {
    text_off: u32,
    src_off: u32,
    len: u32,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Directives, assignments, rule headers. Continuations collapse to a
    /// single space; `#` starts a comment.
    Normal,
    /// Recipe body. One leading recipe prefix is stripped, backslash-newline is
    /// preserved for the shell, and `#` is *not* a make comment.
    Recipe,
    /// One physical line, verbatim. Used for `define` bodies.
    Raw,
}

#[derive(Debug)]
pub struct LogicalLine {
    pub file: FileId,
    /// Assembled text: comments removed, continuations resolved.
    pub text: String,
    /// Byte range in the source file covered by this logical line.
    pub src_start: u32,
    pub src_end: u32,
    /// True if the first physical line began with the recipe prefix character.
    pub had_recipe_prefix: bool,
    /// Number of leading space characters on the first physical line.
    pub leading_spaces: usize,
    /// True if the line is indented with spaces and *then* a tab, which make
    /// does not accept as a recipe.
    pub spaces_then_tab: bool,
    segs: Vec<Seg>,
}

impl LogicalLine {
    /// Map an offset in `self.text` to a byte offset in the source file.
    fn map(&self, text_off: usize) -> usize {
        match self.segs.binary_search_by_key(&(text_off as u32), |s| s.text_off) {
            Ok(i) => self.segs[i].src_off as usize,
            Err(0) => self.src_start as usize,
            Err(i) => {
                let s = &self.segs[i - 1];
                let d = text_off - s.text_off as usize;
                s.src_off as usize + d.min(s.len as usize)
            }
        }
    }

    /// Source span for the half-open text range `a..b`.
    pub fn span_of(&self, a: usize, b: usize) -> Span {
        let start = self.map(a);
        let end = self.map(b).max(start);
        Span::new(self.file, start, end)
    }

    pub fn full_span(&self) -> Span {
        Span::new(self.file, self.src_start as usize, self.src_end as usize)
    }

    pub fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    /// Offset of the first non-whitespace character in `text`.
    pub fn content_start(&self) -> usize {
        self.text.len() - self.text.trim_start().len()
    }
}

struct LineBuilder {
    text: String,
    segs: Vec<Seg>,
}

impl LineBuilder {
    fn new() -> Self {
        LineBuilder { text: String::new(), segs: Vec::new() }
    }

    /// Append text that exists in the source at `src_off`.
    fn push(&mut self, src_off: usize, s: &str) {
        if s.is_empty() {
            return;
        }
        if let Some(last) = self.segs.last_mut() {
            let contiguous_src = last.src_off as usize + last.len as usize == src_off;
            let contiguous_text = last.text_off as usize + last.len as usize == self.text.len();
            if contiguous_src && contiguous_text {
                last.len += s.len() as u32;
                self.text.push_str(s);
                return;
            }
        }
        self.segs.push(Seg {
            text_off: self.text.len() as u32,
            src_off: src_off as u32,
            len: s.len() as u32,
        });
        self.text.push_str(s);
    }

    /// Append text with no direct source counterpart (the space a continuation
    /// collapses to, the newline a recipe continuation keeps). It anchors to
    /// `src_off` so spans still land somewhere sensible.
    fn push_synthetic(&mut self, src_off: usize, s: &str) {
        self.segs.push(Seg { text_off: self.text.len() as u32, src_off: src_off as u32, len: 0 });
        self.text.push_str(s);
    }
}

pub struct Lexer<'a> {
    file: FileId,
    text: &'a str,
    pos: usize,
    /// `.RECIPEPREFIX`; only ever changed by that directive.
    pub recipe_prefix: u8,
}

impl<'a> Lexer<'a> {
    pub fn new(file: FileId, text: &'a str) -> Self {
        Lexer { file, text, pos: 0, recipe_prefix: b'\t' }
    }

    pub fn at_eof(&self) -> bool {
        self.pos >= self.text.len()
    }

    pub fn offset(&self) -> usize {
        self.pos
    }

    /// Content end (excluding `\r\n`) and start of the following physical line.
    fn phys_bounds(&self, from: usize) -> (usize, usize) {
        let bytes = self.text.as_bytes();
        let mut end = from;
        while end < bytes.len() && bytes[end] != b'\n' {
            end += 1;
        }
        let next = (end + 1).min(bytes.len());
        if end > from && bytes[end - 1] == b'\r' {
            end -= 1;
        }
        (end, next)
    }

    /// Raw text of the current physical line.
    pub fn peek_raw(&self) -> &'a str {
        let (ce, _) = self.phys_bounds(self.pos);
        &self.text[self.pos..ce]
    }

    pub fn peek_recipe_prefix(&self) -> bool {
        self.text.as_bytes().get(self.pos) == Some(&self.recipe_prefix)
    }

    /// True when the current physical line is empty or is a comment-only line.
    /// Such lines do not terminate a recipe.
    pub fn peek_blank_or_comment(&self) -> bool {
        let t = self.peek_raw().trim();
        t.is_empty() || t.starts_with('#')
    }

    /// First whitespace-delimited word of the current physical line, ignoring a
    /// leading recipe prefix. Used to spot conditionals inside recipes.
    pub fn peek_word(&self) -> &'a str {
        let mut s = self.peek_raw();
        if let Some(rest) = s.strip_prefix(self.recipe_prefix as char) {
            s = rest;
        }
        s.split_whitespace().next().unwrap_or("")
    }

    /// Advance past the current physical line without producing a logical line.
    pub fn skip_physical(&mut self) {
        let (_, next) = self.phys_bounds(self.pos);
        self.pos = next;
    }

    pub fn next_line(&mut self, mode: Mode) -> Option<LogicalLine> {
        if self.at_eof() {
            return None;
        }
        match mode {
            Mode::Normal => Some(self.next_normal()),
            Mode::Recipe => Some(self.next_recipe()),
            Mode::Raw => Some(self.next_raw()),
        }
    }

    fn line_prefix_info(&self, at: usize) -> (bool, usize, bool) {
        let bytes = self.text.as_bytes();
        let had_prefix = bytes.get(at) == Some(&self.recipe_prefix);
        let mut i = at;
        let mut spaces = 0usize;
        while bytes.get(i) == Some(&b' ') {
            spaces += 1;
            i += 1;
        }
        let spaces_then_tab = spaces > 0 && bytes.get(i) == Some(&b'\t');
        (had_prefix, spaces, spaces_then_tab)
    }

    fn next_normal(&mut self) -> LogicalLine {
        let start = self.pos;
        let (had_recipe_prefix, leading_spaces, spaces_then_tab) = self.line_prefix_info(start);
        let mut b = LineBuilder::new();
        let bytes = self.text.as_bytes();
        let mut first = true;
        let mut in_comment = false;
        let mut end_src;
        // Open `$(`/`${` references, as (opener, closer) pairs. A `#` inside one
        // is not a comment, and the nesting persists across continuation lines.
        let mut refs: Vec<(u8, u8)> = Vec::new();

        loop {
            let (ce, next) = self.phys_bounds(self.pos);
            end_src = ce;
            let mut i = self.pos;
            if !first {
                while i < ce && matches!(bytes[i], b' ' | b'\t') {
                    i += 1;
                }
            }
            let mut seg_start = i;
            let continued;

            if in_comment {
                continued = odd_trailing_backslashes(&self.text[self.pos..ce]);
            } else {
                let mut cont = false;
                while i < ce {
                    let c = bytes[i];

                    if c == b'$' && i + 1 < ce {
                        match bytes[i + 1] {
                            b'$' => i += 2,
                            b'(' => {
                                refs.push((b'(', b')'));
                                i += 2;
                            }
                            b'{' => {
                                refs.push((b'{', b'}'));
                                i += 2;
                            }
                            // `$X`: a one-character name, no nesting.
                            _ => i = self.bump(i, 2, ce),
                        }
                        continue;
                    }

                    // Inside a reference make counts bare brackets of the same
                    // kind too, which is why `$(shell echo '(' )#c` is an
                    // unterminated call rather than a comment.
                    if let Some(&(open, close)) = refs.last() {
                        if c == open {
                            refs.push((open, close));
                            i += 1;
                            continue;
                        }
                        if c == close {
                            refs.pop();
                            i += 1;
                            continue;
                        }
                    }

                    match c {
                        b'\\' => {
                            if i + 1 == ce {
                                b.push(seg_start, &self.text[seg_start..i]);
                                b.push_synthetic(i, " ");
                                seg_start = ce;
                                cont = true;
                                i = ce;
                            } else if bytes[i + 1] == b'#' && refs.is_empty() {
                                // `\#` is an escaped hash; make drops the backslash.
                                b.push(seg_start, &self.text[seg_start..i]);
                                b.push(i + 1, "#");
                                i += 2;
                                seg_start = i;
                            } else {
                                i = self.bump(i, 2, ce);
                            }
                        }
                        b'#' if refs.is_empty() => {
                            b.push(seg_start, &self.text[seg_start..i]);
                            seg_start = ce;
                            in_comment = true;
                            cont = odd_trailing_backslashes(&self.text[i..ce]);
                            i = ce;
                        }
                        _ => i = self.bump(i, 1, ce),
                    }
                }
                if seg_start < ce {
                    b.push(seg_start, &self.text[seg_start..ce]);
                }
                continued = cont;
            }

            self.pos = next;
            if continued && self.pos < self.text.len() {
                first = false;
                continue;
            }
            break;
        }

        LogicalLine {
            file: self.file,
            text: b.text,
            segs: b.segs,
            src_start: start as u32,
            src_end: end_src as u32,
            had_recipe_prefix,
            leading_spaces,
            spaces_then_tab,
        }
    }

    fn next_recipe(&mut self) -> LogicalLine {
        let start = self.pos;
        let (had_recipe_prefix, leading_spaces, spaces_then_tab) = self.line_prefix_info(start);
        let mut b = LineBuilder::new();
        let bytes = self.text.as_bytes();
        let mut end_src;

        loop {
            let (ce, next) = self.phys_bounds(self.pos);
            end_src = ce;
            let mut i = self.pos;
            if bytes.get(i) == Some(&self.recipe_prefix) {
                i += 1;
            }
            b.push(i, &self.text[i..ce]);
            let continued = odd_trailing_backslashes(&self.text[i..ce]);
            self.pos = next;
            if continued && self.pos < self.text.len() {
                // make hands the backslash-newline to the shell verbatim.
                b.push_synthetic(ce, "\n");
                continue;
            }
            break;
        }

        LogicalLine {
            file: self.file,
            text: b.text,
            segs: b.segs,
            src_start: start as u32,
            src_end: end_src as u32,
            had_recipe_prefix,
            leading_spaces,
            spaces_then_tab,
        }
    }

    fn next_raw(&mut self) -> LogicalLine {
        let start = self.pos;
        let (had_recipe_prefix, leading_spaces, spaces_then_tab) = self.line_prefix_info(start);
        let (ce, next) = self.phys_bounds(self.pos);
        let mut b = LineBuilder::new();
        b.push(start, &self.text[start..ce]);
        self.pos = next;
        LogicalLine {
            file: self.file,
            text: b.text,
            segs: b.segs,
            src_start: start as u32,
            src_end: ce as u32,
            had_recipe_prefix,
            leading_spaces,
            spaces_then_tab,
        }
    }

    /// Read a `define` body up to its matching `endef`, as one logical line with
    /// embedded newlines. The body must be a single unit because a reference can
    /// span lines, as in `$(eval X := a \` continued on the next line.
    ///
    /// Comments are *not* stripped: make stores a `define` body verbatim.
    /// Returns the body and whether a matching `endef` was found.
    pub fn read_define_body(&mut self) -> (LogicalLine, bool) {
        let start = self.pos;
        let mut b = LineBuilder::new();
        let mut depth = 1usize;
        let mut closed = false;
        let mut end_src = start;

        while !self.at_eof() {
            let (ce, next) = self.phys_bounds(self.pos);
            match first_word(&self.text[self.pos..ce]) {
                "define" => depth += 1,
                "endef" => {
                    depth -= 1;
                    if depth == 0 {
                        self.pos = next;
                        closed = true;
                        break;
                    }
                }
                _ => {}
            }
            if !b.text.is_empty() {
                b.push_synthetic(self.pos, "\n");
            }
            b.push(self.pos, &self.text[self.pos..ce]);
            end_src = ce;
            self.pos = next;
        }

        let line = LogicalLine {
            file: self.file,
            text: b.text,
            segs: b.segs,
            src_start: start as u32,
            src_end: end_src as u32,
            had_recipe_prefix: false,
            leading_spaces: 0,
            spaces_then_tab: false,
        };
        (line, closed)
    }

    /// Advance `i` by `n` bytes, then to the next char boundary, clamped to `ce`.
    fn bump(&self, i: usize, n: usize, ce: usize) -> usize {
        let mut j = (i + n).min(ce);
        while j < ce && !self.text.is_char_boundary(j) {
            j += 1;
        }
        j
    }
}

fn first_word(line: &str) -> &str {
    let s = line.trim_start();
    &s[..s.find(char::is_whitespace).unwrap_or(s.len())]
}

fn odd_trailing_backslashes(s: &str) -> bool {
    s.bytes().rev().take_while(|&c| c == b'\\').count() % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::SourceMap;
    use std::path::PathBuf;

    fn lex(src: &str, modes: &[Mode]) -> Vec<String> {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("M"), src.to_string());
        let text = sm.get(id).text.clone();
        let mut lx = Lexer::new(id, &text);
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(l) = lx.next_line(*modes.get(i).unwrap_or(&Mode::Normal)) {
            out.push(l.text);
            i += 1;
        }
        out
    }

    #[test]
    fn continuation_collapses_to_space() {
        assert_eq!(lex("A = one \\\n    two\n", &[]), vec!["A = one  two"]);
    }

    #[test]
    fn comment_stripped_and_escaped_hash_kept() {
        assert_eq!(lex("A = b # note\n", &[]), vec!["A = b "]);
        assert_eq!(lex("A = b\\#c\n", &[]), vec!["A = b#c"]);
    }

    #[test]
    fn comment_swallows_continuation() {
        assert_eq!(lex("A = b # note \\\nstill comment\nB = c\n", &[]), vec!["A = b ", "B = c"]);
    }

    // Verified against GNU Make 4.4.1: `A := $(shell echo 'x#y')` yields `x#y`,
    // so `#` inside a reference is not a comment.
    #[test]
    fn hash_inside_a_reference_is_not_a_comment() {
        assert_eq!(lex("A := $(shell echo 'x#y')\n", &[]), vec!["A := $(shell echo 'x#y')"]);
        assert_eq!(lex("F := ${shell echo 'q#r'}\n", &[]), vec!["F := ${shell echo 'q#r'}"]);
        assert_eq!(lex("D := $(shell echo a) # c\n", &[]), vec!["D := $(shell echo a) "]);
        assert_eq!(lex("G := a#b\n", &[]), vec!["G := a"]);
    }

    // Make counts bare brackets inside a reference too, which is why it rejects
    // `H := $(shell echo '(' )#c` as an unterminated call rather than a comment.
    #[test]
    fn bare_bracket_inside_a_reference_counts() {
        assert_eq!(lex("H := $(shell echo '(' )#c\n", &[]), vec!["H := $(shell echo '(' )#c"]);
    }

    #[test]
    fn reference_spanning_a_continuation_suppresses_comments() {
        let src = "A := $(shell echo a \\\n  b#c)\n";
        assert_eq!(lex(src, &[]), vec!["A := $(shell echo a  b#c)"]);
    }

    #[test]
    fn define_body_is_one_unit_with_comments_intact() {
        let src = "define d\n\t$(eval X := a \\\n\t\tb) # keep\nendef\nA=1\n";
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("M"), src.to_string());
        let text = sm.get(id).text.clone();
        let mut lx = Lexer::new(id, &text);
        lx.next_line(Mode::Normal).unwrap(); // `define d`
        let (body, closed) = lx.read_define_body();
        assert!(closed);
        assert_eq!(body.text, "\t$(eval X := a \\\n\t\tb) # keep");
        // The lexer is positioned after `endef`.
        assert_eq!(lx.next_line(Mode::Normal).unwrap().text, "A=1");
    }

    #[test]
    fn double_backslash_is_not_a_continuation() {
        assert_eq!(lex("A = b\\\\\nB = c\n", &[]), vec!["A = b\\\\", "B = c"]);
    }

    #[test]
    fn recipe_keeps_backslash_and_strips_next_tab() {
        let out = lex("\techo a \\\n\techo b\n", &[Mode::Recipe]);
        assert_eq!(out, vec!["echo a \\\necho b"]);
    }

    #[test]
    fn recipe_keeps_hash() {
        assert_eq!(lex("\techo a # b\n", &[Mode::Recipe]), vec!["echo a # b"]);
    }

    #[test]
    fn spans_survive_continuations() {
        let src = "A = one \\\n    two\n";
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("M"), src.to_string());
        let text = sm.get(id).text.clone();
        let mut lx = Lexer::new(id, &text);
        let l = lx.next_line(Mode::Normal).unwrap();
        let at = l.text.find("two").unwrap();
        let sp = l.span_of(at, at + 3);
        assert_eq!(&src[sp.start as usize..sp.end as usize], "two");
    }
}
