use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Number(i64),
    StringLit(String),
    BytesLit(Vec<u8>),
    Attribute(String),

    Colon,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Equal,
    EqualEqual,
    FatArrow,
    NotEqual,
    Gt,
    Lt,
    GtEq,
    LtEq,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    DoubleColon,

    Indent,
    Dedent,
    Newline,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Ident(s) => write!(f, "Ident({})", s),
            TokenKind::Number(n) => write!(f, "Number({})", n),
            TokenKind::StringLit(s) => write!(f, "String({:?})", s),
            TokenKind::BytesLit(b) => write!(f, "Bytes({:?})", b),
            TokenKind::Attribute(s) => write!(f, "Attribute(@{})", s),
            TokenKind::Colon => write!(f, "':'"),
            TokenKind::Comma => write!(f, "','"),
            TokenKind::LBracket => write!(f, "'['"),
            TokenKind::RBracket => write!(f, "']'"),
            TokenKind::LParen => write!(f, "'('"),
            TokenKind::RParen => write!(f, "')'"),
            TokenKind::LBrace => write!(f, "'{{'"),
            TokenKind::RBrace => write!(f, "'}}'"),
            TokenKind::Equal => write!(f, "'='"),
            TokenKind::EqualEqual => write!(f, "'=='"),
            TokenKind::FatArrow => write!(f, "'=>'"),
            TokenKind::NotEqual => write!(f, "'!='"),
            TokenKind::Gt => write!(f, "'>'"),
            TokenKind::Lt => write!(f, "'<'"),
            TokenKind::GtEq => write!(f, "'>='"),
            TokenKind::LtEq => write!(f, "'<='"),
            TokenKind::Dot => write!(f, "'.'"),
            TokenKind::Plus => write!(f, "'+'"),
            TokenKind::Minus => write!(f, "'-'"),
            TokenKind::Star => write!(f, "'*'"),
            TokenKind::Slash => write!(f, "'/'"),
            TokenKind::Percent => write!(f, "'%'"),
            TokenKind::DoubleColon => write!(f, "'::'"),
            TokenKind::Indent => write!(f, "INDENT"),
            TokenKind::Dedent => write!(f, "DEDENT"),
            TokenKind::Newline => write!(f, "NEWLINE"),
            TokenKind::Eof => write!(f, "EOF"),
        }
    }
}

#[derive(Debug)]
pub struct LexError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error at {}:{}: {}", self.line, self.col, self.message)
    }
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: usize,
    col: usize,
    indent_stack: Vec<usize>,
    tokens: Vec<Token>,
    at_line_start: bool,
    paren_depth: i32,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            line: 1,
            col: 1,
            indent_stack: vec![0],
            tokens: Vec::new(),
            at_line_start: true,
            paren_depth: 0,
        }
    }

    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        while self.pos < self.src.len() {
            if self.at_line_start && self.paren_depth == 0 {
                self.handle_line_start()?;
                if self.pos >= self.src.len() {
                    break;
                }
            }

            let c = self.src[self.pos];

            if c == b'-' && self.peek(1) == Some(b'-') {
                while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                    self.advance();
                }
                continue;
            }

            if c == b'\n' {
                let nl_line = self.line;
                let nl_col = self.col;
                self.pos += 1;
                self.line += 1;
                self.col = 1;
                if self.paren_depth == 0 {
                    let last_is_nl = matches!(
                        self.tokens.last().map(|t| &t.kind),
                        Some(TokenKind::Newline) | Some(TokenKind::Indent) | Some(TokenKind::Dedent)
                    );
                    if !self.tokens.is_empty() && !last_is_nl {
                        self.emit_token(TokenKind::Newline, nl_line, nl_col);
                    }
                    self.at_line_start = true;
                }
                continue;
            }

            if c == b' ' || c == b'\t' {
                self.advance();
                continue;
            }

            self.read_token()?;
        }

        let last_is_nl = matches!(
            self.tokens.last().map(|t| &t.kind),
            Some(TokenKind::Newline) | None
        );
        if !last_is_nl {
            self.emit_token(TokenKind::Newline, self.line, self.col);
        }

        while self.indent_stack.len() > 1 {
            self.indent_stack.pop();
            self.emit_token(TokenKind::Dedent, self.line, self.col);
        }

        self.emit_token(TokenKind::Eof, self.line, self.col);
        Ok(self.tokens)
    }

    fn handle_line_start(&mut self) -> Result<(), LexError> {
        let mut width = 0usize;
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' => {
                    width += 1;
                    self.advance();
                }
                b'\t' => {
                    return Err(LexError {
                        line: self.line,
                        col: self.col,
                        message: "tabs not allowed for indentation; use spaces".into(),
                    });
                }
                _ => break,
            }
        }

        if self.pos >= self.src.len() {
            self.at_line_start = false;
            return Ok(());
        }
        let next = self.src[self.pos];
        if next == b'\n' {
            self.at_line_start = false;
            return Ok(());
        }
        if next == b'-' && self.peek(1) == Some(b'-') {
            self.at_line_start = false;
            return Ok(());
        }

        let current = *self.indent_stack.last().unwrap();
        if width > current {
            self.indent_stack.push(width);
            self.emit_token(TokenKind::Indent, self.line, 1);
        } else if width < current {
            while *self.indent_stack.last().unwrap() > width {
                self.indent_stack.pop();
                self.emit_token(TokenKind::Dedent, self.line, 1);
            }
            if *self.indent_stack.last().unwrap() != width {
                return Err(LexError {
                    line: self.line,
                    col: 1,
                    message: format!(
                        "inconsistent indentation: width {} does not match any enclosing block",
                        width
                    ),
                });
            }
        }
        self.at_line_start = false;
        Ok(())
    }

    fn read_token(&mut self) -> Result<(), LexError> {
        let start_line = self.line;
        let start_col = self.col;
        let c = self.src[self.pos];

        if c == b'@' {
            self.advance();
            let start = self.pos;
            while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
                self.advance();
            }
            if start == self.pos {
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: "expected identifier after '@'".into(),
                });
            }
            let name = std::str::from_utf8(&self.src[start..self.pos])
                .unwrap()
                .to_string();
            self.emit_token(TokenKind::Attribute(name), start_line, start_col);
            return Ok(());
        }

        if c == b'"' {
            // String literal with a small, closed set of escape sequences:
            // \n, \t, \\, \". Anything else after a backslash is a lex error —
            // we do not silently pass "\q" through as two characters, because
            // that would let typos slip into string content without warning.
            self.advance();
            let mut s = String::new();
            while self.pos < self.src.len() && self.src[self.pos] != b'"' {
                let ch = self.src[self.pos];
                if ch == b'\n' {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unterminated string literal".into(),
                    });
                }
                if ch == b'\\' {
                    self.advance();
                    if self.pos >= self.src.len() {
                        return Err(LexError {
                            line: start_line,
                            col: start_col,
                            message: "unterminated string literal after '\\'".into(),
                        });
                    }
                    let esc = self.src[self.pos];
                    match esc {
                        b'n' => s.push('\n'),
                        b't' => s.push('\t'),
                        // Phase 11 slice 1: `\r` joins the closed set so
                        // wire-protocol literals (HTTP/1.0 CRLF, raw TCP
                        // line terminators) can be expressed without
                        // out-of-band bytes. Same audit discipline as the
                        // other escapes — typos still fail at lex time.
                        b'r' => s.push('\r'),
                        b'\\' => s.push('\\'),
                        b'"' => s.push('"'),
                        other => {
                            return Err(LexError {
                                line: self.line,
                                col: self.col,
                                message: format!(
                                    "unknown escape '\\{}' (supported: \\n, \\r, \\t, \\\\, \\\")",
                                    other as char
                                ),
                            });
                        }
                    }
                    self.advance();
                } else {
                    s.push(ch as char);
                    self.advance();
                }
            }
            if self.pos >= self.src.len() {
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: "unterminated string literal".into(),
                });
            }
            self.advance();
            self.emit_token(TokenKind::StringLit(s), start_line, start_col);
            return Ok(());
        }

        if c.is_ascii_digit() {
            let start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos].is_ascii_digit() {
                self.advance();
            }
            let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            let n: i64 = s.parse().map_err(|_| LexError {
                line: start_line,
                col: start_col,
                message: format!("invalid number: {}", s),
            })?;
            self.emit_token(TokenKind::Number(n), start_line, start_col);
            return Ok(());
        }

        // Byte-string literal: `b"..."`. Only when `b` is IMMEDIATELY followed by
        // a `"` — a bare `b` (or `bar`, `b + 1`) stays an identifier. Distinct from
        // the regular `"..."` text path: a byte string carries raw bytes (Vec<u8>),
        // not UTF-8, and additionally supports `\xNN` (two hex digits → one byte
        // 0..=255). This is the only place `\xNN` is legal — text stays a closed
        // UTF-8 escape set, untouched.
        if c == b'b' && self.peek(1) == Some(b'"') {
            self.advance(); // consume the `b`
            self.advance(); // consume the opening `"`
            let mut bytes: Vec<u8> = Vec::new();
            while self.pos < self.src.len() && self.src[self.pos] != b'"' {
                let ch = self.src[self.pos];
                if ch == b'\n' {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unterminated byte-string literal".into(),
                    });
                }
                if ch == b'\\' {
                    self.advance();
                    if self.pos >= self.src.len() {
                        return Err(LexError {
                            line: start_line,
                            col: start_col,
                            message: "unterminated byte-string literal after '\\'".into(),
                        });
                    }
                    let esc = self.src[self.pos];
                    match esc {
                        b'n' => bytes.push(b'\n'),
                        b't' => bytes.push(b'\t'),
                        b'r' => bytes.push(b'\r'),
                        b'\\' => bytes.push(b'\\'),
                        b'"' => bytes.push(b'"'),
                        b'x' | b'X' => {
                            // \xNN — exactly two hex digits → one byte 0..=255.
                            let hi = self.peek(1);
                            let lo = self.peek(2);
                            let (hi, lo) = match (hi, lo) {
                                (Some(h), Some(l))
                                    if (h as char).is_ascii_hexdigit()
                                        && (l as char).is_ascii_hexdigit() =>
                                {
                                    (h, l)
                                }
                                _ => {
                                    return Err(LexError {
                                        line: self.line,
                                        col: self.col,
                                        message: "invalid \\x escape: expected two hex digits"
                                            .into(),
                                    });
                                }
                            };
                            let val = (hex_val(hi) << 4) | hex_val(lo);
                            bytes.push(val);
                            // Consume the two hex digits (the `x` itself is
                            // consumed by the shared advance below).
                            self.advance();
                            self.advance();
                        }
                        other => {
                            return Err(LexError {
                                line: self.line,
                                col: self.col,
                                message: format!(
                                    "unknown escape '\\{}' in byte string (supported: \\n, \\r, \\t, \\\\, \\\", \\xNN)",
                                    other as char
                                ),
                            });
                        }
                    }
                    self.advance();
                } else {
                    bytes.push(ch);
                    self.advance();
                }
            }
            if self.pos >= self.src.len() {
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: "unterminated byte-string literal".into(),
                });
            }
            self.advance(); // consume closing `"`
            self.emit_token(TokenKind::BytesLit(bytes), start_line, start_col);
            return Ok(());
        }

        if is_ident_start(c) {
            let start = self.pos;
            while self.pos < self.src.len() && is_ident_continue(self.src[self.pos]) {
                self.advance();
            }
            let name = std::str::from_utf8(&self.src[start..self.pos])
                .unwrap()
                .to_string();
            self.emit_token(TokenKind::Ident(name), start_line, start_col);
            return Ok(());
        }

        let kind = match c {
            b':' => {
                if self.peek(1) == Some(b':') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::DoubleColon, start_line, start_col);
                    return Ok(());
                }
                self.advance();
                TokenKind::Colon
            }
            b',' => {
                self.advance();
                TokenKind::Comma
            }
            b'[' => {
                self.advance();
                self.paren_depth += 1;
                TokenKind::LBracket
            }
            b']' => {
                self.advance();
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unexpected ']'".into(),
                    });
                }
                TokenKind::RBracket
            }
            b'(' => {
                self.advance();
                self.paren_depth += 1;
                TokenKind::LParen
            }
            b')' => {
                self.advance();
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unexpected ')'".into(),
                    });
                }
                TokenKind::RParen
            }
            b'{' => {
                self.advance();
                self.paren_depth += 1;
                TokenKind::LBrace
            }
            b'}' => {
                self.advance();
                self.paren_depth -= 1;
                if self.paren_depth < 0 {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unexpected '}'".into(),
                    });
                }
                TokenKind::RBrace
            }
            b'=' => {
                if self.peek(1) == Some(b'=') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::EqualEqual, start_line, start_col);
                    return Ok(());
                }
                if self.peek(1) == Some(b'>') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::FatArrow, start_line, start_col);
                    return Ok(());
                }
                self.advance();
                TokenKind::Equal
            }
            b'!' => {
                if self.peek(1) == Some(b'=') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::NotEqual, start_line, start_col);
                    return Ok(());
                }
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: "unexpected '!' (did you mean '!='?)".into(),
                });
            }
            b'>' => {
                if self.peek(1) == Some(b'=') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::GtEq, start_line, start_col);
                    return Ok(());
                }
                self.advance();
                TokenKind::Gt
            }
            b'<' => {
                if self.peek(1) == Some(b'=') {
                    self.advance();
                    self.advance();
                    self.emit_token(TokenKind::LtEq, start_line, start_col);
                    return Ok(());
                }
                self.advance();
                TokenKind::Lt
            }
            b'.' => {
                self.advance();
                TokenKind::Dot
            }
            b'+' => {
                self.advance();
                TokenKind::Plus
            }
            b'-' => {
                self.advance();
                TokenKind::Minus
            }
            b'*' => {
                self.advance();
                TokenKind::Star
            }
            b'/' => {
                self.advance();
                TokenKind::Slash
            }
            b'%' => {
                self.advance();
                TokenKind::Percent
            }
            _ => {
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: format!("unexpected character: {:?}", c as char),
                });
            }
        };
        self.emit_token(kind, start_line, start_col);
        Ok(())
    }

    fn emit_token(&mut self, kind: TokenKind, line: usize, col: usize) {
        self.tokens.push(Token { kind, line, col });
    }

    fn advance(&mut self) {
        self.pos += 1;
        self.col += 1;
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

// Map a single ASCII hex digit (0-9, a-f, A-F) to its 0..=15 value. Callers
// guarantee the byte is a hex digit (checked via is_ascii_hexdigit) before
// calling, so the fallthrough is unreachable in practice.
fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        Lexer::new(src)
            .tokenize()
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn empty_input() {
        assert_eq!(kinds(""), vec![TokenKind::Eof]);
    }

    #[test]
    fn single_ident() {
        assert_eq!(
            kinds("foo\n"),
            vec![
                TokenKind::Ident("foo".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn indent_dedent() {
        assert_eq!(
            kinds("a\n  b\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Indent,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::Dedent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn nested_indent() {
        assert_eq!(
            kinds("a\n  b\n    c\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Indent,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::Indent,
                TokenKind::Ident("c".into()),
                TokenKind::Newline,
                TokenKind::Dedent,
                TokenKind::Dedent,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comment_ignored() {
        assert_eq!(
            kinds("a -- a comment\nb\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn comment_only_line_ignored() {
        assert_eq!(
            kinds("a\n  -- just a comment\nb\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn blank_line_ignored() {
        assert_eq!(
            kinds("a\n\nb\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Newline,
                TokenKind::Ident("b".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn attribute_version() {
        assert_eq!(
            kinds("@verbose 0.1.0\n"),
            vec![
                TokenKind::Attribute("verbose".into()),
                TokenKind::Number(0),
                TokenKind::Dot,
                TokenKind::Number(1),
                TokenKind::Dot,
                TokenKind::Number(0),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn string_literal() {
        assert_eq!(
            kinds("\"hello world\"\n"),
            vec![
                TokenKind::StringLit("hello world".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn string_escape_sequences() {
        // \n, \t, \\, \" are the only supported escapes — anything else is
        // a lex error, not a silent pass-through. That means a typo in a
        // string literal surfaces at compile time instead of being written
        // to a file as literal backslash + letter.
        let src = "\"line1\\nline2\\ttabbed\\\\slash\\\"quote\"\n";
        let tokens = Lexer::new(src).tokenize().unwrap();
        let s = tokens.iter().find_map(|t| match &t.kind {
            TokenKind::StringLit(s) => Some(s.clone()),
            _ => None,
        }).expect("no StringLit token produced");
        assert_eq!(s, "line1\nline2\ttabbed\\slash\"quote");
    }

    #[test]
    fn unknown_escape_rejected() {
        let src = "\"oops\\q\"\n";
        let err = Lexer::new(src).tokenize().err();
        assert!(err.is_some(), "expected lex error on \\q escape");
        assert!(
            format!("{}", err.unwrap()).contains("unknown escape"),
            "expected 'unknown escape' message",
        );
    }

    #[test]
    fn list_across_lines() {
        assert_eq!(
            kinds("[a,\n b]\n"),
            vec![
                TokenKind::LBracket,
                TokenKind::Ident("a".into()),
                TokenKind::Comma,
                TokenKind::Ident("b".into()),
                TokenKind::RBracket,
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn punctuation() {
        assert_eq!(
            kinds("a.b :: c >= 10\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Dot,
                TokenKind::Ident("b".into()),
                TokenKind::DoubleColon,
                TokenKind::Ident("c".into()),
                TokenKind::GtEq,
                TokenKind::Number(10),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tabs_rejected() {
        let result = Lexer::new("a\n\tb\n").tokenize();
        assert!(result.is_err());
    }

    #[test]
    fn equality_operators() {
        assert_eq!(
            kinds("a == b != c\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::EqualEqual,
                TokenKind::Ident("b".into()),
                TokenKind::NotEqual,
                TokenKind::Ident("c".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn fat_arrow() {
        assert_eq!(
            kinds("x => y\n"),
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::FatArrow,
                TokenKind::Ident("y".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn arithmetic_operators() {
        assert_eq!(
            kinds("a + b * c / d % e - f\n"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Plus,
                TokenKind::Ident("b".into()),
                TokenKind::Star,
                TokenKind::Ident("c".into()),
                TokenKind::Slash,
                TokenKind::Ident("d".into()),
                TokenKind::Percent,
                TokenKind::Ident("e".into()),
                TokenKind::Minus,
                TokenKind::Ident("f".into()),
                TokenKind::Newline,
                TokenKind::Eof,
            ]
        );
    }
}
