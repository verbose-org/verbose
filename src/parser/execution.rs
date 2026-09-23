use super::*;
use std::collections::HashSet;

impl Parser {
    pub(super) fn parse_execution(&mut self) -> Result<Execution, ParseError> {
        self.expect_ident("execution")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let mut seen = HashSet::new();
        let (mut intention, mut source, mut input, mut phases, mut limit) =
            (None, None, None, None, None);
        let (mut mode, mut max_in_flight) = (None, None);
        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let attribute = self.peek_attribute_name();
            let key = if let Some(attr) = &attribute {
                self.advance();
                format!("@{attr}")
            } else {
                self.expect_ident_any()?
            };
            if !seen.insert(key.clone()) {
                return Err(self.error(&format!("execution '{name}': duplicate '{key}'")));
            }
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "@intention" => intention = Some(self.expect_string()?),
                "@source" => source = Some(self.parse_source_ref()?),
                "input" => input = Some(self.expect_ident_any()?),
                "mode" => {
                    let value = self.expect_ident_any()?;
                    if value != "sequential" && value != "concurrent" {
                        return Err(self.error("execution mode must be sequential or concurrent"));
                    }
                    mode = Some(value);
                }
                "on_failure" => {
                    if self.expect_ident_any()? != "stop" {
                        return Err(self.error("execution supports only on_failure: stop"));
                    }
                }
                "phases" => {
                    self.expect_kind(TokenKind::LBracket)?;
                    let mut names = Vec::new();
                    if !self.check_kind(&TokenKind::RBracket) {
                        loop {
                            names.push(self.expect_ident_any()?);
                            if !self.check_kind(&TokenKind::Comma) {
                                break;
                            }
                            self.advance();
                        }
                    }
                    self.expect_kind(TokenKind::RBracket)?;
                    phases = Some(names);
                }
                "native_stack" => {
                    let n = self.expect_number()?;
                    if !(1..=2_097_152).contains(&n) {
                        return Err(
                            self.error("execution native_stack must be in [1, 2097152] bytes")
                        );
                    }
                    limit = Some(n as u32);
                }
                "max_in_flight" => {
                    let n = self.expect_number()?;
                    if !(1..=64).contains(&n) {
                        return Err(self.error("execution max_in_flight must be in [1, 64]"));
                    }
                    max_in_flight = Some(n as u32);
                }
                _ => return Err(self.error(&format!("execution '{name}': unknown field '{key}'"))),
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;
        for key in [
            "@intention",
            "@source",
            "input",
            "mode",
            "phases",
            "on_failure",
        ] {
            if !seen.contains(key) {
                return Err(self.error(&format!("execution '{name}': missing '{key}'")));
            }
        }
        let mode = if mode.as_deref() == Some("concurrent") {
            if limit.is_some() {
                return Err(self.error("concurrent execution refuses native_stack: native scheduler layout is not supported yet"));
            }
            ExecutionMode::Concurrent {
                max_in_flight: max_in_flight
                    .ok_or_else(|| self.error("concurrent execution requires max_in_flight"))?,
            }
        } else {
            if max_in_flight.is_some() {
                return Err(self.error("sequential execution does not accept max_in_flight"));
            }
            ExecutionMode::Sequential {
                native_stack: limit
                    .ok_or_else(|| self.error("sequential execution requires native_stack"))?,
            }
        };
        Ok(Execution {
            name,
            intention: intention.unwrap(),
            source: source.unwrap(),
            input: input.unwrap(),
            phases: phases.unwrap(),
            mode,
        })
    }
}
