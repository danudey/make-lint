//! Parses make value text into an [`Expr`]: literals interleaved with
//! `$(...)`, `${...}` and `$X` references.

use crate::ast::{Expr, FuncCall, Piece, RefStyle, Subst, VarRef};
use crate::builtins;
use crate::diag::Diagnostic;
use crate::lexer::LogicalLine;

/// Parse the half-open range `start..end` of `line.text`.
pub fn parse(line: &LogicalLine, start: usize, end: usize, diags: &mut Vec<Diagnostic>) -> Expr {
    let mut p = ExprParser { line, diags };
    p.range(start, end)
}

struct ExprParser<'a, 'd> {
    line: &'a LogicalLine,
    diags: &'d mut Vec<Diagnostic>,
}

impl ExprParser<'_, '_> {
    fn text(&self) -> &str {
        &self.line.text
    }

    fn bytes(&self) -> &[u8] {
        self.line.text.as_bytes()
    }

    /// Advance by `n` bytes then to the next char boundary, clamped to `end`.
    fn bump(&self, i: usize, n: usize, end: usize) -> usize {
        let mut j = (i + n).min(end);
        while j < end && !self.text().is_char_boundary(j) {
            j += 1;
        }
        j
    }

    fn range(&mut self, start: usize, end: usize) -> Expr {
        let span = self.line.span_of(start, end);
        let mut pieces = Vec::new();
        let mut lit = start;
        let mut i = start;

        while i < end {
            if self.bytes()[i] != b'$' {
                i = self.bump(i, 1, end);
                continue;
            }
            self.flush(&mut pieces, lit, i);

            if i + 1 >= end {
                // A trailing bare `$`; make passes it through.
                pieces.push(Piece::Text { text: "$".into(), span: self.line.span_of(i, end) });
                lit = end;
                break;
            }

            match self.bytes()[i + 1] {
                b'$' => {
                    pieces.push(Piece::Dollar { span: self.line.span_of(i, i + 2) });
                    i += 2;
                }
                b'(' => i = self.reference(&mut pieces, i, end, b'(', b')'),
                b'{' => i = self.reference(&mut pieces, i, end, b'{', b'}'),
                _ => i = self.bare(&mut pieces, i, end),
            }
            lit = i;
        }
        self.flush(&mut pieces, lit, end);
        Expr { pieces, span }
    }

    fn flush(&mut self, pieces: &mut Vec<Piece>, from: usize, to: usize) {
        if from < to {
            pieces.push(Piece::Text {
                text: self.text()[from..to].to_string(),
                span: self.line.span_of(from, to),
            });
        }
    }

    /// `$X` — a one-character variable name.
    fn bare(&mut self, pieces: &mut Vec<Piece>, dollar: usize, end: usize) -> usize {
        let name_start = dollar + 1;
        let ch = self.text()[name_start..end].chars().next().unwrap();
        let name_end = name_start + ch.len_utf8();

        let mut run = name_end;
        while run < end && is_ident(self.bytes()[run]) {
            run += 1;
        }
        let bare_run_on =
            (run > name_end && is_ident_start(ch)).then(|| self.text()[name_end..run].to_string());

        let span = self.line.span_of(dollar, name_end);
        pieces.push(Piece::Var(VarRef {
            name: Expr {
                pieces: vec![Piece::Text {
                    text: ch.to_string(),
                    span: self.line.span_of(name_start, name_end),
                }],
                span: self.line.span_of(name_start, name_end),
            },
            subst: None,
            style: RefStyle::Bare,
            bare_run_on,
            span,
        }));
        name_end
    }

    /// `$(...)` or `${...}`, which is either a function call or a variable
    /// reference (possibly with a `:from=to` substitution).
    fn reference(
        &mut self,
        pieces: &mut Vec<Piece>,
        dollar: usize,
        end: usize,
        open: u8,
        close: u8,
    ) -> usize {
        let inner = dollar + 2;
        let Some(cp) = self.find_close(inner, end, open, close) else {
            self.diags.push(
                Diagnostic::error(
                    "MK033",
                    self.line.span_of(dollar, (dollar + 2).min(end)),
                    format!("unterminated variable reference: no matching `{}`", close as char),
                )
                .with_help("make will treat the rest of the line as part of the reference"),
            );
            self.flush(pieces, dollar, end);
            return end;
        };

        // Function call: the first word must be a known built-in *and* be
        // followed by whitespace, matching make's own dispatch.
        if let Some(rel) = self.text()[inner..cp].find(char::is_whitespace) {
            let name = self.text()[inner..inner + rel].to_string();
            if let Some(f) = builtins::function(&name) {
                let name_span = self.line.span_of(inner, inner + rel);
                let mut args_start = inner + rel;
                while args_start < cp && self.bytes()[args_start].is_ascii_whitespace() {
                    args_start += 1;
                }
                let args = self.split_args(args_start, cp, f.max_args);
                pieces.push(Piece::Func(FuncCall {
                    name,
                    name_span,
                    args,
                    span: self.line.span_of(dollar, cp + 1),
                }));
                return cp + 1;
            }
        }

        // Substitution reference: `$(VAR:from=to)`.
        let mut subst = None;
        let mut name_end = cp;
        if let Some(colon) = self.find_top_level(inner, cp, b':')
            && let Some(eq) = self.find_top_level(colon + 1, cp, b'=')
        {
            name_end = colon;
            subst = Some(Subst { from: self.range(colon + 1, eq), to: self.range(eq + 1, cp) });
        }

        let name = self.range(inner, name_end);
        pieces.push(Piece::Var(VarRef {
            name,
            subst,
            style: if open == b'(' { RefStyle::Paren } else { RefStyle::Brace },
            bare_run_on: None,
            span: self.line.span_of(dollar, cp + 1),
        }));
        cp + 1
    }

    /// Offset of the `close` matching the already-consumed opener.
    fn find_close(&self, from: usize, end: usize, open: u8, close: u8) -> Option<usize> {
        let b = self.bytes();
        let mut depth = 1usize;
        let mut i = from;
        while i < end {
            let c = b[i];
            if c == b'$' && i + 1 < end && b[i + 1] == b'$' {
                i += 2;
                continue;
            }
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            i = self.bump(i, 1, end);
        }
        None
    }

    /// First `needle` in `from..end` that is not inside a nested reference.
    fn find_top_level(&self, from: usize, end: usize, needle: u8) -> Option<usize> {
        let b = self.bytes();
        let mut depth = 0i32;
        let mut i = from;
        while i < end {
            let c = b[i];
            if c == b'$' && i + 1 < end && b[i + 1] == b'$' {
                i += 2;
                continue;
            }
            match c {
                b'(' | b'{' => depth += 1,
                b')' | b'}' => depth -= 1,
                _ if c == needle && depth == 0 => return Some(i),
                _ => {}
            }
            i = self.bump(i, 1, end);
        }
        None
    }

    /// Split a function body on top-level commas into at most `max` arguments;
    /// the last argument absorbs any remaining commas, as make does.
    fn split_args(&mut self, start: usize, end: usize, max: usize) -> Vec<Expr> {
        if max <= 1 {
            return vec![self.range(start, end)];
        }
        let mut args = Vec::new();
        let mut seg = start;
        let mut depth = 0i32;
        let mut i = start;
        while i < end {
            let b = self.bytes();
            let c = b[i];
            if c == b'$' && i + 1 < end && b[i + 1] == b'$' {
                i += 2;
                continue;
            }
            match c {
                b'(' | b'{' => depth += 1,
                b')' | b'}' => depth -= 1,
                b',' if depth == 0 && args.len() + 1 < max => {
                    args.push(self.range(seg, i));
                    seg = i + 1;
                }
                _ => {}
            }
            i = self.bump(i, 1, end);
        }
        args.push(self.range(seg, end));
        args
    }
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Piece;
    use crate::lexer::{Lexer, Mode};
    use crate::span::SourceMap;
    use std::path::PathBuf;

    fn expr_of(src: &str) -> (Expr, Vec<Diagnostic>) {
        let mut sm = SourceMap::default();
        let id = sm.add(PathBuf::from("M"), format!("{src}\n"));
        let text = sm.get(id).text.clone();
        let mut lx = Lexer::new(id, &text);
        let line = lx.next_line(Mode::Normal).unwrap();
        let mut diags = Vec::new();
        let e = parse(&line, 0, line.text.len(), &mut diags);
        // Leak is fine: test-only.
        (e, diags)
    }

    #[test]
    fn plain_variable() {
        let (e, d) = expr_of("$(CC)");
        assert!(d.is_empty());
        assert_eq!(e.pieces.len(), 1);
        match &e.pieces[0] {
            Piece::Var(v) => assert_eq!(v.name.literal().unwrap(), "CC"),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn function_and_nesting() {
        let (e, _) = expr_of("$(patsubst %.c,%.o,$(SRCS))");
        match &e.pieces[0] {
            Piece::Func(f) => {
                assert_eq!(f.name, "patsubst");
                assert_eq!(f.args.len(), 3);
                assert_eq!(f.args[0].literal().unwrap(), "%.c");
                assert!(f.args[2].literal().is_none());
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn single_arg_function_keeps_commas() {
        let (e, _) = expr_of("$(shell echo a,b)");
        match &e.pieces[0] {
            Piece::Func(f) => {
                assert_eq!(f.args.len(), 1);
                assert_eq!(f.args[0].literal().unwrap(), "echo a,b");
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn last_arg_absorbs_extra_commas() {
        let (e, _) = expr_of("$(subst a,b,c,d)");
        match &e.pieces[0] {
            Piece::Func(f) => {
                assert_eq!(f.args.len(), 3);
                assert_eq!(f.args[2].literal().unwrap(), "c,d");
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn substitution_reference() {
        let (e, _) = expr_of("$(SRCS:.c=.o)");
        match &e.pieces[0] {
            Piece::Var(v) => {
                assert_eq!(v.name.literal().unwrap(), "SRCS");
                let s = v.subst.as_ref().unwrap();
                assert_eq!(s.from.literal().unwrap(), ".c");
                assert_eq!(s.to.literal().unwrap(), ".o");
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn colon_without_equals_is_part_of_the_name() {
        let (e, _) = expr_of("$(a:b)");
        match &e.pieces[0] {
            Piece::Var(v) => assert_eq!(v.name.literal().unwrap(), "a:b"),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn bare_reference_run_on() {
        let (e, _) = expr_of("$FOO");
        match &e.pieces[0] {
            Piece::Var(v) => {
                assert_eq!(v.style, RefStyle::Bare);
                assert_eq!(v.name.literal().unwrap(), "F");
                assert_eq!(v.bare_run_on.as_deref(), Some("OO"));
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn automatic_variable_has_no_run_on() {
        let (e, _) = expr_of("$@x");
        match &e.pieces[0] {
            Piece::Var(v) => assert!(v.bare_run_on.is_none()),
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn dollar_dollar_is_literal() {
        let (e, _) = expr_of("$$FOO");
        assert!(matches!(e.pieces[0], Piece::Dollar { .. }));
        assert_eq!(e.literal().unwrap(), "$FOO");
    }

    #[test]
    fn unterminated_reference_reports() {
        let (_, d) = expr_of("$(CC");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code, "MK033");
    }

    #[test]
    fn unknown_function_name_becomes_a_variable() {
        let (e, _) = expr_of("$(pastsubst a,b,c)");
        match &e.pieces[0] {
            Piece::Var(v) => assert_eq!(v.name.literal().unwrap(), "pastsubst a,b,c"),
            p => panic!("{p:?}"),
        }
    }
}
