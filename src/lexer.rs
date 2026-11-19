//! Lexer: turns source text into tokens, including Python-style
//! NEWLINE / INDENT / DEDENT layout tokens.

use crate::diagnostics::Diagnostic;
use crate::span::Span;
use crate::token::{FPart, Tok, Token};

pub struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    /// Added to every span (used when lexing f-string sub-expressions).
    base: usize,
    tokens: Vec<Token>,
    diags: Vec<Diagnostic>,
    indent_stack: Vec<usize>,
    bracket_depth: usize,
    /// Open `{` of map literals (a `}` without one is a stray block brace).
    map_braces: usize,
    at_line_start: bool,
    /// Expression mode: no layout tokens at all (f-string interpolations).
    expr_mode: bool,
}

/// Lex a whole file.
pub fn lex(src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    lex_at(src, 0)
}

/// Lex a whole file whose spans start at `base` (standard-library modules).
pub fn lex_at(src: &str, base: usize) -> (Vec<Token>, Vec<Diagnostic>) {
    let lx = Lexer::new(src, base, false);
    lx.run()
}

/// Lex an embedded expression (f-string interpolation) located at `base`.
pub fn lex_expr(src: &str, base: usize) -> (Vec<Token>, Vec<Diagnostic>) {
    let lx = Lexer::new(src, base, true);
    lx.run()
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str, base: usize, expr_mode: bool) -> Self {
        Lexer {
            src: text.as_bytes(),
            text,
            pos: 0,
            base,
            tokens: Vec::new(),
            diags: Vec::new(),
            indent_stack: vec![0],
            bracket_depth: 0,
            map_braces: 0,
            at_line_start: !expr_mode,
            expr_mode,
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(start + self.base, end + self.base)
    }

    fn peek(&self) -> u8 {
        self.src.get(self.pos).copied().unwrap_or(0)
    }

    fn peek_at(&self, n: usize) -> u8 {
        self.src.get(self.pos + n).copied().unwrap_or(0)
    }

    fn push(&mut self, tok: Tok, start: usize, end: usize) {
        let span = self.span(start, end);
        self.tokens.push(Token { tok, span });
    }

    fn error(&mut self, msg: impl Into<String>, start: usize, end: usize) {
        let span = self.span(start, end);
        self.diags.push(Diagnostic::error(msg, span));
    }

    fn last_is_newline_or_empty(&self) -> bool {
        matches!(self.tokens.last().map(|t| &t.tok), None | Some(Tok::Newline) | Some(Tok::Indent) | Some(Tok::Dedent))
    }

    fn run(mut self) -> (Vec<Token>, Vec<Diagnostic>) {
        loop {
            if self.at_line_start && self.bracket_depth == 0 && !self.handle_indentation() {
                break; // EOF
            }
            if self.pos >= self.src.len() {
                break;
            }
            let c = self.peek();
            let start = self.pos;
            match c {
                b' ' | b'\t' => self.pos += 1,
                b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    if self.bracket_depth == 0 && !self.expr_mode {
                        if !self.last_is_newline_or_empty() {
                            self.push(Tok::Newline, start, start + 1);
                        }
                        self.at_line_start = true;
                    }
                }
                b'#' => {
                    while self.pos < self.src.len() && self.peek() != b'\n' {
                        self.pos += 1;
                    }
                }
                b'0'..=b'9' => self.number(),
                b'"' => {
                    self.pos += 1;
                    let s = self.string_body(start);
                    self.push(Tok::Str(s), start, self.pos);
                }
                b'f' if self.peek_at(1) == b'"' => {
                    self.pos += 2;
                    let parts = self.fstring_body(start);
                    self.push(Tok::FStr(parts), start, self.pos);
                }
                c if c == b'_' || c.is_ascii_alphabetic() => {
                    while self.peek() == b'_' || self.peek().is_ascii_alphanumeric() {
                        self.pos += 1;
                    }
                    let word = &self.text[start..self.pos];
                    let tok = Tok::keyword(word).unwrap_or_else(|| Tok::Ident(word.to_string()));
                    self.push(tok, start, self.pos);
                }
                _ => self.punct(),
            }
        }
        if !self.expr_mode {
            if !self.last_is_newline_or_empty() {
                let end = self.src.len();
                self.push(Tok::Newline, end, end);
            }
            let end = self.src.len();
            while self.indent_stack.len() > 1 {
                self.indent_stack.pop();
                self.push(Tok::Dedent, end, end);
            }
        }
        let end = self.src.len();
        self.push(Tok::Eof, end, end);
        (self.tokens, self.diags)
    }

    /// Measure indentation at the start of a line and emit INDENT/DEDENT.
    /// Returns false at EOF.
    fn handle_indentation(&mut self) -> bool {
        loop {
            let line_start = self.pos;
            let mut width = 0;
            let mut saw_tab = None;
            while self.pos < self.src.len() {
                match self.peek() {
                    b' ' => width += 1,
                    b'\t' => {
                        saw_tab.get_or_insert(self.pos);
                        width += 1;
                    }
                    b'\r' => {}
                    _ => break,
                }
                self.pos += 1;
            }
            if self.pos >= self.src.len() {
                return false;
            }
            match self.peek() {
                b'\n' => {
                    self.pos += 1;
                    continue; // blank line
                }
                b'#' => {
                    while self.pos < self.src.len() && self.peek() != b'\n' {
                        self.pos += 1;
                    }
                    continue; // comment-only line
                }
                _ => {}
            }
            if let Some(tab) = saw_tab {
                self.error("tabs are not allowed in indentation; use spaces", tab, tab + 1);
            }
            self.at_line_start = false;
            let current = *self.indent_stack.last().unwrap();
            if width > current {
                self.indent_stack.push(width);
                self.push(Tok::Indent, line_start, self.pos);
            } else if width < current {
                while *self.indent_stack.last().unwrap() > width {
                    self.indent_stack.pop();
                    self.push(Tok::Dedent, self.pos, self.pos);
                }
                if *self.indent_stack.last().unwrap() != width {
                    self.error("unindent does not match any outer indentation level", line_start, self.pos);
                    // Recover by treating this as the enclosing level.
                    self.indent_stack.push(width);
                }
            }
            return true;
        }
    }

    fn number(&mut self) {
        let start = self.pos;
        // Hex / binary literals.
        if self.peek() == b'0' && matches!(self.peek_at(1), b'x' | b'X' | b'b' | b'B') {
            let radix = if matches!(self.peek_at(1), b'x' | b'X') { 16 } else { 2 };
            self.pos += 2;
            let digits_start = self.pos;
            while self.peek() == b'_' || (self.peek() as char).is_digit(radix) {
                self.pos += 1;
            }
            let digits: String = self.text[digits_start..self.pos].chars().filter(|&c| c != '_').collect();
            match u64::from_str_radix(&digits, radix) {
                Ok(v) => self.push(Tok::Int(v), start, self.pos),
                Err(_) => {
                    self.error("invalid integer literal", start, self.pos);
                    self.push(Tok::Int(0), start, self.pos);
                }
            }
            return;
        }
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.pos += 1;
        }
        let mut is_float = false;
        // `t.0.1`: right after a `.`, a number is a tuple element's index, never a float.
        let after_dot = matches!(self.tokens.last(), Some(Token { tok: Tok::Dot | Tok::QuestionDot, .. }));
        if self.peek() == b'.' && self.peek_at(1).is_ascii_digit() && !after_dot {
            is_float = true;
            self.pos += 1;
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), b'e' | b'E') && !after_dot {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), b'+' | b'-') {
                self.pos += 1;
            }
            if self.peek().is_ascii_digit() {
                is_float = true;
                while self.peek().is_ascii_digit() {
                    self.pos += 1;
                }
            } else {
                self.pos = save;
            }
        }
        let text: String = self.text[start..self.pos].chars().filter(|&c| c != '_').collect();
        if is_float {
            match text.parse::<f64>() {
                Ok(v) => self.push(Tok::Float(v), start, self.pos),
                Err(_) => {
                    self.error("invalid float literal", start, self.pos);
                    self.push(Tok::Float(0.0), start, self.pos);
                }
            }
        } else {
            match text.parse::<u64>() {
                Ok(v) if v <= 1u64 << 63 => self.push(Tok::Int(v), start, self.pos),
                _ => {
                    self.error("integer literal is too large for `Int` (64-bit)", start, self.pos);
                    self.push(Tok::Int(0), start, self.pos);
                }
            }
        }
        if self.peek() == b'_' || self.peek().is_ascii_alphabetic() {
            let s = self.pos;
            while self.peek() == b'_' || self.peek().is_ascii_alphanumeric() {
                self.pos += 1;
            }
            self.error("invalid suffix on number literal", s, self.pos);
        }
    }

    /// Parse an escape sequence after a backslash. Returns the char.
    fn escape(&mut self) -> Option<char> {
        let esc_start = self.pos - 1;
        let c = self.peek();
        self.pos += 1;
        Some(match c {
            b'n' => '\n',
            b't' => '\t',
            b'r' => '\r',
            b'0' => '\0',
            b'\\' => '\\',
            b'"' => '"',
            b'\'' => '\'',
            b'{' => '{',
            b'}' => '}',
            _ => {
                self.error("unknown escape sequence", esc_start, self.pos);
                return None;
            }
        })
    }

    /// Consume one (possibly multi-byte) char from the source.
    fn take_char(&mut self) -> char {
        let ch = self.text[self.pos..].chars().next().unwrap();
        self.pos += ch.len_utf8();
        ch
    }

    fn string_body(&mut self, start: usize) -> String {
        let mut out = String::new();
        loop {
            match self.peek() {
                0 if self.pos >= self.src.len() => {
                    self.error("unterminated string literal", start, self.pos);
                    return out;
                }
                b'\n' => {
                    self.error("unterminated string literal", start, self.pos);
                    return out;
                }
                b'"' => {
                    self.pos += 1;
                    return out;
                }
                b'\\' => {
                    self.pos += 1;
                    if let Some(c) = self.escape() {
                        out.push(c);
                    }
                }
                _ => out.push(self.take_char()),
            }
        }
    }

    fn fstring_body(&mut self, start: usize) -> Vec<FPart> {
        let mut parts = Vec::new();
        let mut lit = String::new();
        loop {
            match self.peek() {
                0 if self.pos >= self.src.len() => {
                    self.error("unterminated f-string", start, self.pos);
                    break;
                }
                b'\n' => {
                    self.error("unterminated f-string", start, self.pos);
                    break;
                }
                b'"' => {
                    self.pos += 1;
                    break;
                }
                b'\\' => {
                    self.pos += 1;
                    if let Some(c) = self.escape() {
                        lit.push(c);
                    }
                }
                b'{' if self.peek_at(1) == b'{' => {
                    self.pos += 2;
                    lit.push('{');
                }
                b'}' if self.peek_at(1) == b'}' => {
                    self.pos += 2;
                    lit.push('}');
                }
                b'}' => {
                    self.error("single `}` in f-string; use `}}` for a literal brace", self.pos, self.pos + 1);
                    self.pos += 1;
                }
                b'{' => {
                    let open = self.pos;
                    self.pos += 1;
                    let expr_start = self.pos;
                    let mut depth = 0usize;
                    let mut ok = false;
                    while self.pos < self.src.len() {
                        match self.peek() {
                            b'\n' => break,
                            b'"' => {
                                // nested string literal inside the interpolation
                                self.pos += 1;
                                while self.pos < self.src.len() && !matches!(self.peek(), b'"' | b'\n') {
                                    if self.peek() == b'\\' {
                                        self.pos += 1;
                                    }
                                    self.pos += 1;
                                }
                                if self.peek() == b'"' {
                                    self.pos += 1;
                                }
                                continue;
                            }
                            b'(' | b'[' | b'{' => depth += 1,
                            b')' | b']' => depth = depth.saturating_sub(1),
                            b'}' if depth == 0 => {
                                ok = true;
                                break;
                            }
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                    if !ok {
                        self.error("unclosed `{` in f-string", open, self.pos);
                        break;
                    }
                    let expr_src = &self.text[expr_start..self.pos];
                    self.pos += 1; // closing }
                    if expr_src.trim().is_empty() {
                        self.error("empty expression in f-string", open, self.pos);
                        continue;
                    }
                    if !lit.is_empty() {
                        parts.push(FPart::Lit(std::mem::take(&mut lit)));
                    }
                    parts.push(FPart::Expr(expr_src.to_string(), (expr_start + self.base) as u32));
                }
                _ => lit.push(self.take_char()),
            }
        }
        if !lit.is_empty() {
            parts.push(FPart::Lit(lit));
        }
        parts
    }

    fn punct(&mut self) {
        let start = self.pos;
        let c = self.peek();
        let n = self.peek_at(1);
        let (tok, len) = match (c, n) {
            (b'-', b'>') => (Tok::Arrow, 2),
            (b'.', b'.') if self.peek_at(2) == b'=' => (Tok::DotDotEq, 3),
            (b'.', b'.') => (Tok::DotDot, 2),
            (b'=', b'=') => (Tok::EqEq, 2),
            (b'!', b'=') => (Tok::NotEq, 2),
            (b'!', b'!') => (Tok::BangBang, 2),
            (b'?', b'.') => (Tok::QuestionDot, 2),
            (b'?', _) => (Tok::Question, 1),
            (b'|', n) if n != b'|' => (Tok::Pipe, 1),
            (b'<', b'=') if self.peek_at(2) == b'>' => (Tok::Spaceship, 3),
            (b'<', b'=') => (Tok::Le, 2),
            (b'>', b'=') => (Tok::Ge, 2),
            (b'+', b'=') => (Tok::PlusEq, 2),
            (b'-', b'=') => (Tok::MinusEq, 2),
            (b'*', b'=') => (Tok::StarEq, 2),
            (b'/', b'=') => (Tok::SlashEq, 2),
            (b'%', b'=') => (Tok::PercentEq, 2),
            (b'(', _) => (Tok::LParen, 1),
            (b')', _) => (Tok::RParen, 1),
            (b'[', _) => (Tok::LBracket, 1),
            (b']', _) => (Tok::RBracket, 1),
            (b',', _) => (Tok::Comma, 1),
            (b':', _) => (Tok::Colon, 1),
            (b'.', _) => (Tok::Dot, 1),
            (b'+', _) => (Tok::Plus, 1),
            (b'-', _) => (Tok::Minus, 1),
            (b'*', _) => (Tok::Star, 1),
            (b'/', _) => (Tok::Slash, 1),
            (b'%', _) => (Tok::Percent, 1),
            (b'=', _) => (Tok::Assign, 1),
            (b'<', _) => (Tok::Lt, 1),
            (b'>', _) => (Tok::Gt, 1),
            (b'{', _) if self.brace_opens_map() => (Tok::LBrace, 1),
            (b'}', _) if self.map_braces > 0 => (Tok::RBrace, 1),
            (b'{', _) | (b'}', _) => {
                self.pos += 1;
                // Treat `{` at the end of a line as `:` so the rest still parses.
                let rest = &self.text[self.pos..];
                let line_rest = rest.split('\n').next().unwrap_or("");
                let at_eol = line_rest.trim().is_empty() || line_rest.trim_start().starts_with('#');
                let span = self.span(start, self.pos);
                let d = if c == b'{' {
                    if at_eol {
                        self.push(Tok::Colon, start, self.pos);
                    }
                    Diagnostic::error("Tarn uses `:` and indentation for blocks, not `{`", span)
                        .with_primary("replace this with `:`")
                        .with_help("end the line with `:` and indent the block below it by 4 spaces:\n\nwhile x < 10:\n    x += 1")
                } else {
                    Diagnostic::error("Tarn blocks end by un-indenting, not with `}`", span)
                        .with_primary("remove this")
                        .with_help("delete the `}`; a block ends where the indentation goes back")
                };
                self.diags.push(d);
                return;
            }
            _ => {
                let ch = self.take_char();
                let msg = match ch {
                    '&' if self.peek() == b'&' => "unexpected `&&`; Tarn uses `and`".to_string(),
                    '|' if self.peek() == b'|' => "unexpected `||`; Tarn uses `or`".to_string(),
                    '!' => "unexpected `!`; Tarn uses `not` (and `!!` to assert a value isn't null)".to_string(),
                    '\'' => "single quotes are not strings in Tarn; use double quotes".to_string(),
                    _ => format!("unexpected character `{ch}`"),
                };
                self.error(msg, start, self.pos);
                return;
            }
        };
        match tok {
            Tok::LParen | Tok::LBracket => self.bracket_depth += 1,
            Tok::RParen | Tok::RBracket => self.bracket_depth = self.bracket_depth.saturating_sub(1),
            Tok::LBrace => {
                self.bracket_depth += 1;
                self.map_braces += 1;
            }
            Tok::RBrace => {
                self.bracket_depth = self.bracket_depth.saturating_sub(1);
                self.map_braces -= 1;
            }
            _ => {}
        }
        self.pos += len;
        self.push(tok, start, self.pos);
    }

    /// Is this `{` the start of a map literal? It is where a value can start
    /// (after `=`, `(`, `,`, `return`, an operator...). After something that
    /// ends a value (`if x {`, `fn main() {`, `else {`) it's a block brace
    /// from another language, which gets an error explaining `:`.
    fn brace_opens_map(&self) -> bool {
        !matches!(
            self.tokens.last().map(|t| &t.tok),
            Some(
                Tok::Ident(_)
                    | Tok::Int(_)
                    | Tok::Float(_)
                    | Tok::Str(_)
                    | Tok::FStr(_)
                    | Tok::RParen
                    | Tok::RBracket
                    | Tok::RBrace
                    | Tok::True
                    | Tok::False
                    | Tok::Null
                    | Tok::Else
                    | Tok::BangBang
                    | Tok::Arrow
                    | Tok::Question
            )
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        let (t, d) = lex(src);
        assert!(d.is_empty(), "unexpected diagnostics: {d:?}");
        t.into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn indentation_blocks() {
        let src = "fn main():\n    if x:\n        pass\n    y\n";
        use Tok::*;
        assert_eq!(
            toks(src),
            vec![
                Fn, Ident("main".into()), LParen, RParen, Colon, Newline,
                Indent, If, Ident("x".into()), Colon, Newline,
                Indent, Pass, Newline,
                Dedent, Ident("y".into()), Newline,
                Dedent, Eof
            ]
        );
    }

    #[test]
    fn blank_lines_and_comments_ignored() {
        let src = "a\n\n   # comment\n\nb # trailing\n";
        use Tok::*;
        assert_eq!(toks(src), vec![Ident("a".into()), Newline, Ident("b".into()), Newline, Eof]);
    }

    #[test]
    fn brackets_join_lines() {
        let src = "f(1,\n      2)\n";
        use Tok::*;
        assert_eq!(toks(src), vec![Ident("f".into()), LParen, Int(1), Comma, Int(2), RParen, Newline, Eof]);
    }

    #[test]
    fn bad_dedent_is_error() {
        let (_, d) = lex("fn a():\n    x\n  y\n");
        assert!(d.iter().any(|d| d.message.contains("unindent")));
    }

    #[test]
    fn tabs_are_errors() {
        let (_, d) = lex("fn a():\n\tx\n");
        assert!(d.iter().any(|d| d.message.contains("tabs")));
    }

    #[test]
    fn numbers_and_ranges() {
        use Tok::*;
        assert_eq!(toks("0..10\n"), vec![Int(0), DotDot, Int(10), Newline, Eof]);
        assert_eq!(toks("1_000 2.5 1e3 0xff\n"), vec![Int(1000), Float(2.5), Float(1000.0), Int(255), Newline, Eof]);
    }

    #[test]
    fn fstring_parts() {
        let t = toks("f\"a {x + 1} b {{c}}\"\n");
        match &t[0] {
            Tok::FStr(parts) => {
                assert_eq!(parts[0], FPart::Lit("a ".into()));
                assert!(matches!(&parts[1], FPart::Expr(s, 5) if s == "x + 1"));
                assert_eq!(parts[2], FPart::Lit(" b {c}".into()));
            }
            other => panic!("expected f-string, got {other:?}"),
        }
    }
}
