use super::*;
use std::collections::HashSet;

impl Parser {
    pub(super) fn parse_workload(&mut self) -> Result<Workload, ParseError> {
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let mut objective = None;
        let mut cases = Vec::new();
        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            match key.as_str() {
                "objective" => {
                    if objective.is_some() {
                        return Err(self.error("workload: duplicate 'objective'"));
                    }
                    self.expect_kind(TokenKind::Colon)?;
                    objective = Some(match self.expect_ident_any()?.as_str() {
                        "elapsed" => WorkloadObjective::Elapsed,
                        "cpu" => WorkloadObjective::Cpu,
                        _ => return Err(self.error("workload objective must be elapsed or cpu")),
                    });
                    self.expect_kind(TokenKind::Newline)?;
                }
                "case" => cases.push(self.parse_workload_case()?),
                _ => return Err(self.error(&format!("workload: unknown field '{key}'"))),
            }
        }
        self.expect_kind(TokenKind::Dedent)?;
        let workload = Workload {
            objective: objective.ok_or_else(|| self.error("workload: missing 'objective'"))?,
            cases,
        };
        if let Some(message) = crate::execution::workload::shape_error(&workload) {
            return Err(self.error(&message));
        }
        Ok(workload)
    }

    fn parse_workload_case(&mut self) -> Result<WorkloadCase, ParseError> {
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let mut seen = HashSet::new();
        let (mut weight, mut records, mut target_us) = (None, None, None);
        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            if !seen.insert(key.clone()) {
                return Err(self.error(&format!("workload case '{name}': duplicate '{key}'")));
            }
            let maximum = match key.as_str() {
                "weight" | "records" => 1_000_000,
                "target_us" => 1_000_000_000,
                _ => {
                    return Err(
                        self.error(&format!("workload case '{name}': unknown field '{key}'"))
                    )
                }
            };
            self.expect_kind(TokenKind::Colon)?;
            let message =
                format!("workload case '{name}': {key} must be an integer in [1, {maximum}]");
            let n = self.expect_number().map_err(|_| self.error(&message))?;
            if !(1..=maximum).contains(&n) {
                return Err(self.error(&message));
            }
            match key.as_str() {
                "weight" => weight = Some(n as u32),
                "records" => records = Some(n as u32),
                _ => target_us = Some(n as u32),
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;
        Ok(WorkloadCase {
            name: name.clone(),
            weight: weight
                .ok_or_else(|| self.error(&format!("workload case '{name}': missing 'weight'")))?,
            records: records
                .ok_or_else(|| self.error(&format!("workload case '{name}': missing 'records'")))?,
            target_us,
        })
    }
}
