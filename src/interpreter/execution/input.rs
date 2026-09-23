//! Closed JSON transport for the flat numeric/text execution input contract.
//! Keep legacy interpreter input acceptance unchanged.
use super::{error, RuntimeError, Value};
use std::collections::HashMap;

pub(super) fn parse(text: &str) -> Result<Vec<HashMap<String, Value>>, RuntimeError> {
    let mut r = Reader { text, at: 0 };
    r.expect(b'[')?;
    let mut records = Vec::new();
    if !r.take(b']') {
        loop {
            r.expect(b'{')?;
            let mut fields = HashMap::new();
            if !r.take(b'}') {
                loop {
                    let key = r.string()?;
                    r.expect(b':')?;
                    let value = r.value()?;
                    if fields.insert(key, value).is_some() {
                        return Err(r.error("duplicate field"));
                    }
                    if r.take(b'}') {
                        break;
                    }
                    r.expect(b',')?;
                }
            }
            records.push(fields);
            if r.take(b']') {
                break;
            }
            r.expect(b',')?;
        }
    }
    r.space();
    if r.at != text.len() {
        return Err(r.error("trailing input"));
    }
    Ok(records)
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}
impl Reader<'_> {
    fn error(&self, message: &str) -> RuntimeError {
        error(format!(
            "execution input JSON at byte {}: {message}",
            self.at
        ))
    }
    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\r' | b'\n' | b'\t'))
        {
            self.at += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.text.as_bytes().get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8) -> Result<(), RuntimeError> {
        if self.take(byte) {
            Ok(())
        } else {
            Err(self.error(&format!("expected '{}'", byte as char)))
        }
    }
    fn hex(&mut self) -> Result<u32, RuntimeError> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = self
                .text
                .as_bytes()
                .get(self.at)
                .and_then(|b| (*b as char).to_digit(16))
                .ok_or_else(|| self.error("invalid Unicode escape"))?;
            self.at += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<String, RuntimeError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let c = self.text[self.at..]
                .chars()
                .next()
                .ok_or_else(|| self.error("unterminated string"))?;
            self.at += c.len_utf8();
            match c {
                '"' => return Ok(out),
                '\u{0}'..='\u{1f}' => return Err(self.error("unescaped control character")),
                '\\' => {
                    let escaped = self
                        .text
                        .as_bytes()
                        .get(self.at)
                        .copied()
                        .ok_or_else(|| self.error("incomplete escape"))?;
                    // All accepted escapes are ASCII; a non-ASCII byte returns
                    // an error before slicing at a non-character boundary.
                    self.at += 1;
                    out.push(match escaped {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut code = self.hex()?;
                            if (0xd800..=0xdbff).contains(&code) {
                                if !self.text[self.at..].starts_with("\\u") {
                                    return Err(self.error("missing low surrogate"));
                                }
                                self.at += 2;
                                let low = self.hex()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return Err(self.error("invalid low surrogate"));
                                }
                                code = 0x10000 + ((code - 0xd800) << 10) + low - 0xdc00;
                            }
                            char::from_u32(code)
                                .ok_or_else(|| self.error("invalid Unicode scalar"))?
                        }
                        _ => return Err(self.error("unknown escape")),
                    });
                }
                _ => out.push(c),
            }
        }
    }
    fn value(&mut self) -> Result<Value, RuntimeError> {
        self.space();
        if self.text.as_bytes().get(self.at) == Some(&b'"') {
            return self.string().map(Value::Text);
        }
        for (literal, value) in [("true", true), ("false", false)] {
            if self.text[self.at..].starts_with(literal) {
                self.at += literal.len();
                return Ok(Value::Bool(value));
            }
        }
        let start = self.at;
        if self.text.as_bytes().get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        let digits = self.at;
        while self
            .text
            .as_bytes()
            .get(self.at)
            .is_some_and(u8::is_ascii_digit)
        {
            self.at += 1;
        }
        if self.at == digits {
            return Err(self.error("expected text or an i64 integer"));
        }
        if self.at - digits > 1 && self.text.as_bytes()[digits] == b'0' {
            return Err(self.error("leading zero in number"));
        }
        self.text[start..self.at]
            .parse::<i64>()
            .map(Value::Number)
            .map_err(|_| self.error("integer outside i64 range"))
    }
}
