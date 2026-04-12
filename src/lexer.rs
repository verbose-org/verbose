use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Number(i64),
    StringLit(String),
    Attribute(String),

    Colon,
    Comma,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Equal,
    Gt,
    Lt,
    GtEq,
    LtEq,
    Dot,
    Minus,
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
            TokenKind::Attribute(s) => write!(f, "Attribute(@{})", s),
            TokenKind::Colon => write!(f, "':'"),
            TokenKind::Comma => write!(f, "','"),
            TokenKind::LBracket => write!(f, "'['"),
            TokenKind::RBracket => write!(f, "']'"),
            TokenKind::LParen => write!(f, "'('"),
            TokenKind::RParen => write!(f, "')'"),
            TokenKind::Equal => write!(f, "'='"),
            TokenKind::Gt => write!(f, "'>'"),
            TokenKind::Lt => write!(f, "'<'"),
            TokenKind::GtEq => write!(f, "'>='"),
            TokenKind::LtEq => write!(f, "'<='"),
            TokenKind::Dot => write!(f, "'.'"),
            TokenKind::Minus => write!(f, "'-'"),
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
            self.advance();
            let start = self.pos;
            while self.pos < self.src.len() && self.src[self.pos] != b'"' {
                if self.src[self.pos] == b'\n' {
                    return Err(LexError {
                        line: start_line,
                        col: start_col,
                        message: "unterminated string literal".into(),
                    });
                }
                self.advance();
            }
            if self.pos >= self.src.len() {
                return Err(LexError {
                    line: start_line,
                    col: start_col,
                    message: "unterminated string literal".into(),
                });
            }
            let s = std::str::from_utf8(&self.src[start..self.pos])
                .unwrap()
                .to_string();
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
            b'=' => {
                self.advance();
                TokenKind::Equal
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
            b'-' => {
                self.advance();
                TokenKind::Minus
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
}
