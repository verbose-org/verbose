use std::fmt;

use crate::ast::*;
use crate::lexer::{Token, TokenKind};

#[derive(Debug)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at {}:{}: {}", self.line, self.col, self.message)
    }
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let version = self.parse_version_directive()?;

        // Parse optional 'use' declarations
        let mut uses = Vec::new();
        while self.check_ident("use") {
            self.advance();
            let path = self.expect_string()?;
            self.expect_kind(TokenKind::Newline)?;
            uses.push(path);
        }

        let mut items = Vec::new();
        while !self.at_eof() {
            if self.check_ident("concept") {
                items.push(Item::Concept(self.parse_concept()?));
            } else if self.check_ident("rule") {
                items.push(Item::Rule(self.parse_rule()?));
            } else if self.check_ident("reaction") {
                items.push(Item::Reaction(self.parse_reaction()?));
            } else {
                return Err(self.error("expected 'concept', 'rule', or 'reaction' at top level"));
            }
        }
        Ok(Program { version, uses, items })
    }

    fn parse_version_directive(&mut self) -> Result<Version, ParseError> {
        self.expect_attribute("verbose")?;
        let major = self.expect_number()?;
        self.expect_kind(TokenKind::Dot)?;
        let minor = self.expect_number()?;
        self.expect_kind(TokenKind::Dot)?;
        let patch = self.expect_number()?;
        self.expect_kind(TokenKind::Newline)?;
        Ok(Version {
            major: major as u32,
            minor: minor as u32,
            patch: patch as u32,
        })
    }

    fn parse_concept(&mut self) -> Result<Concept, ParseError> {
        self.expect_ident("concept")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut fields = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            if let Some(attr) = self.peek_attribute_name() {
                match attr.as_str() {
                    "intention" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        intention = Some(self.expect_string()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    "source" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        source = Some(self.parse_source_ref()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown attribute '@{}' in concept (allowed: @intention, @source)",
                            other
                        )));
                    }
                }
            } else if self.check_ident("fields") {
                fields = Some(self.parse_fields_block()?);
            } else {
                return Err(self.error(
                    "expected attribute or 'fields:' in concept body",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let intention = intention
            .ok_or_else(|| self.error(&format!("concept '{}' missing @intention", name)))?;
        let source = source
            .ok_or_else(|| self.error(&format!("concept '{}' missing @source", name)))?;
        let fields = fields
            .ok_or_else(|| self.error(&format!("concept '{}' missing 'fields:' block", name)))?;

        Ok(Concept {
            name,
            intention,
            source,
            fields,
        })
    }

    fn parse_fields_block(&mut self) -> Result<Vec<Field>, ParseError> {
        self.expect_ident("fields")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let mut fields = Vec::new();
        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let name = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            let ty = self.parse_type()?;
            let range = if self.check_kind(&TokenKind::LBracket) {
                self.advance();
                let min = self.parse_signed_number()?;
                self.expect_kind(TokenKind::Comma)?;
                let max = self.parse_signed_number()?;
                self.expect_kind(TokenKind::RBracket)?;
                Some((min, max))
            } else {
                None
            };
            self.expect_kind(TokenKind::Newline)?;
            fields.push(Field { name, ty, range });
        }
        self.expect_kind(TokenKind::Dedent)?;
        Ok(fields)
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let name = self.expect_ident_any()?;
        Ok(match name.as_str() {
            "number" => Type::Number,
            "bool" => Type::Bool,
            "text" => Type::Text,
            "collection" => {
                self.expect_kind(TokenKind::LParen)?;
                let inner = self.expect_ident_any()?;
                self.expect_kind(TokenKind::RParen)?;
                Type::Collection(inner)
            }
            _ => Type::Named(name),
        })
    }

    fn parse_source_ref(&mut self) -> Result<SourceRef, ParseError> {
        // Accept both: "path/to/file.intent":line  OR  file.intent:line
        let is_string = matches!(self.peek_kind(), Some(TokenKind::StringLit(_)));
        let file = if is_string {
            self.expect_string()?
        } else {
            let mut parts = vec![self.expect_ident_any()?];
            while self.check_kind(&TokenKind::Dot) {
                self.advance();
                parts.push(self.expect_ident_any()?);
            }
            parts.join(".")
        };
        self.expect_kind(TokenKind::Colon)?;
        let line = self.expect_number()? as u32;
        Ok(SourceRef { file, line })
    }

    fn parse_rule(&mut self) -> Result<Rule, ParseError> {
        self.expect_ident("rule")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut input: Option<(String, Type)> = None;
        let mut output: Option<(String, Type)> = None;
        let mut logic = None;
        let mut proofs = None;
        let mut hints = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            if let Some(attr) = self.peek_attribute_name() {
                match attr.as_str() {
                    "intention" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        intention = Some(self.expect_string()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    "source" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        source = Some(self.parse_source_ref()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown attribute '@{}' in rule (allowed: @intention, @source)",
                            other
                        )));
                    }
                }
            } else if self.check_ident("input") {
                input = Some(self.parse_binding_block("input")?);
            } else if self.check_ident("output") {
                output = Some(self.parse_binding_block("output")?);
            } else if self.check_ident("logic") {
                logic = Some(self.parse_logic_block()?);
            } else if self.check_ident("proofs") {
                proofs = Some(self.parse_proofs_block()?);
            } else if self.check_ident("hints") {
                hints = Some(self.parse_hints_block()?);
            } else {
                return Err(self.error(
                    "expected attribute or section in rule body",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let intention = intention
            .ok_or_else(|| self.error(&format!("rule '{}' missing @intention", name)))?;
        let source =
            source.ok_or_else(|| self.error(&format!("rule '{}' missing @source", name)))?;
        let (input_name, input_ty) =
            input.ok_or_else(|| self.error(&format!("rule '{}' missing 'input' block", name)))?;
        let (output_name, output_ty) = output
            .ok_or_else(|| self.error(&format!("rule '{}' missing 'output' block", name)))?;
        let logic =
            logic.ok_or_else(|| self.error(&format!("rule '{}' missing 'logic' block", name)))?;
        let proofs =
            proofs.ok_or_else(|| self.error(&format!("rule '{}' missing 'proofs' block", name)))?;

        Ok(Rule {
            name,
            intention,
            source,
            input_name,
            input_ty,
            output_name,
            output_ty,
            logic,
            proofs,
            hints,
        })
    }

    fn parse_binding_block(&mut self, keyword: &str) -> Result<(String, Type), ParseError> {
        self.expect_ident(keyword)?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Colon)?;
        let ty = self.parse_type()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Dedent)?;
        Ok((name, ty))
    }

    fn parse_logic_block(&mut self) -> Result<LogicStmt, ParseError> {
        self.expect_ident("logic")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut bindings = Vec::new();

        // Parse optional let bindings
        while self.check_ident("let") {
            self.advance();
            let name = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Equal)?;
            let value = self.parse_expr()?;
            self.expect_kind(TokenKind::Newline)?;
            bindings.push((name, value));
        }

        // Parse final assignment: target = expr
        let target = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Equal)?;
        let value = self.parse_expr()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Dedent)?;
        Ok(LogicStmt {
            bindings,
            target,
            value,
        })
    }

    /// Expression grammar with precedence (lowest to highest):
    ///   expr       = or_expr
    ///   or_expr    = and_expr ('or' and_expr)*
    ///   and_expr   = cmp_expr ('and' cmp_expr)*
    ///   cmp_expr   = add_expr (('>' | '<' | '>=' | '<=') add_expr)?
    ///   add_expr   = mul_expr (('+' | '-') mul_expr)*
    ///   mul_expr   = unary (('*' | '/') unary)*
    ///   unary      = 'not' unary | '-' unary | primary
    ///   primary    = '(' expr ')' | NUMBER | STRING | IDENT '(' args ')' | IDENT ('.' IDENT)*
    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        self.parse_or_expr()
    }

    fn parse_or_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_and_expr()?;
        while self.check_ident("or") {
            self.advance();
            let right = self.parse_and_expr()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_cmp_expr()?;
        while self.check_ident("and") {
            self.advance();
            let right = self.parse_cmp_expr()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_cmp_expr(&mut self) -> Result<Expr, ParseError> {
        let left = self.parse_add_expr()?;
        let op = match self.peek_kind() {
            Some(TokenKind::EqualEqual) => Some(BinOp::Eq),
            Some(TokenKind::NotEqual) => Some(BinOp::NotEq),
            Some(TokenKind::Gt) => Some(BinOp::Gt),
            Some(TokenKind::Lt) => Some(BinOp::Lt),
            Some(TokenKind::GtEq) => Some(BinOp::GtEq),
            Some(TokenKind::LtEq) => Some(BinOp::LtEq),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let right = self.parse_add_expr()?;
            return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
        }
        Ok(left)
    }

    fn parse_add_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_mul_expr()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Plus) => Some(BinOp::Add),
                Some(TokenKind::Minus) => Some(BinOp::Sub),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_mul_expr()?;
                left = Expr::Binary(op, Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_mul_expr(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.parse_unary()?;
        loop {
            let op = match self.peek_kind() {
                Some(TokenKind::Star) => Some(BinOp::Mul),
                Some(TokenKind::Slash) => Some(BinOp::Div),
                Some(TokenKind::Percent) => Some(BinOp::Mod),
                _ => None,
            };
            if let Some(op) = op {
                self.advance();
                let right = self.parse_unary()?;
                left = Expr::Binary(op, Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        if self.check_ident("not") {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(Expr::Not(Box::new(inner)));
        }
        if matches!(self.peek_kind(), Some(TokenKind::Minus)) {
            self.advance();
            let inner = self.parse_unary()?;
            return Ok(Expr::Neg(Box::new(inner)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        if self.check_ident("if") {
            self.advance();
            let condition = self.parse_expr()?;
            self.expect_ident("then")?;
            let then_expr = self.parse_expr()?;
            self.expect_ident("else")?;
            let else_expr = self.parse_expr()?;
            return Ok(Expr::If(
                Box::new(condition),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        if matches!(self.peek_kind(), Some(TokenKind::LParen)) {
            self.advance();
            let expr = self.parse_expr()?;
            self.expect_kind(TokenKind::RParen)?;
            return Ok(expr);
        }
        let is_number = matches!(self.peek_kind(), Some(TokenKind::Number(_)));
        let is_string = matches!(self.peek_kind(), Some(TokenKind::StringLit(_)));
        let is_ident = matches!(self.peek_kind(), Some(TokenKind::Ident(_)));
        if is_string {
            let s = self.expect_string()?;
            Ok(Expr::Text(s))
        } else if is_number {
            let n = self.expect_number()?;
            Ok(Expr::Number(n))
        } else if is_ident {
            let name = self.expect_ident_any()?;
            if (name == "all" || name == "any") && self.check_kind(&TokenKind::LParen) {
                let kind = if name == "all" {
                    QuantifierKind::All
                } else {
                    QuantifierKind::Any
                };
                self.advance();
                let collection = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let predicate = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Quantifier(kind, Box::new(collection), var, Box::new(predicate)))
            } else if self.check_kind(&TokenKind::LParen) {
                self.advance();
                let mut args = Vec::new();
                if !self.check_kind(&TokenKind::RParen) {
                    args.push(self.parse_expr()?);
                    while self.check_kind(&TokenKind::Comma) {
                        self.advance();
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Call(name, args))
            } else {
                let mut expr = Expr::Ident(name);
                while self.check_kind(&TokenKind::Dot) {
                    self.advance();
                    let field = self.expect_ident_any()?;
                    expr = Expr::Field(Box::new(expr), field);
                }
                Ok(expr)
            }
        } else {
            Err(self.error("expected expression (number or identifier)"))
        }
    }

    fn parse_proofs_block(&mut self) -> Result<Proofs, ParseError> {
        self.expect_ident("proofs")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut purity = None;
        let mut termination = None;
        let mut determinism = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            if self.check_ident("purity") {
                purity = Some(self.parse_purity_block()?);
            } else if self.check_ident("termination") {
                termination = Some(self.parse_termination_block()?);
            } else if self.check_ident("determinism") {
                determinism = Some(self.parse_determinism_block()?);
            } else {
                return Err(self.error(
                    "expected 'purity', 'termination', or 'determinism' in proofs block",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let purity = purity.ok_or_else(|| self.error("proofs missing 'purity'"))?;
        let termination = termination.ok_or_else(|| self.error("proofs missing 'termination'"))?;
        let determinism = determinism.ok_or_else(|| self.error("proofs missing 'determinism'"))?;

        Ok(Proofs {
            purity,
            termination,
            determinism,
        })
    }

    fn parse_purity_block(&mut self) -> Result<Purity, ParseError> {
        self.expect_ident("purity")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut reads = None;
        let mut writes = None;
        let mut calls = None;
        let mut verdict = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "reads" => reads = Some(self.parse_path_list()?),
                "writes" => writes = Some(self.parse_path_list()?),
                "calls" => calls = Some(self.parse_path_list()?),
                "verdict" => verdict = Some(self.parse_purity_verdict()?),
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in purity block (allowed: reads, writes, calls, verdict)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        let reads = reads.ok_or_else(|| self.error("purity missing 'reads'"))?;
        let writes = writes.ok_or_else(|| self.error("purity missing 'writes'"))?;
        let calls = calls.ok_or_else(|| self.error("purity missing 'calls'"))?;
        let verdict = verdict.ok_or_else(|| self.error("purity missing 'verdict'"))?;

        Ok(Purity {
            reads,
            writes,
            calls,
            verdict,
        })
    }

    fn parse_path_list(&mut self) -> Result<Vec<Path>, ParseError> {
        self.expect_kind(TokenKind::LBracket)?;
        let mut out = Vec::new();
        if !self.check_kind(&TokenKind::RBracket) {
            out.push(self.parse_path()?);
            while self.check_kind(&TokenKind::Comma) {
                self.advance();
                out.push(self.parse_path()?);
            }
        }
        self.expect_kind(TokenKind::RBracket)?;
        Ok(out)
    }

    fn parse_path(&mut self) -> Result<Path, ParseError> {
        let mut segments = vec![self.expect_ident_any()?];
        loop {
            if self.check_kind(&TokenKind::Dot) || self.check_kind(&TokenKind::DoubleColon) {
                self.advance();
                segments.push(self.expect_ident_any()?);
            } else {
                break;
            }
        }
        Ok(Path { segments })
    }

    fn parse_purity_verdict(&mut self) -> Result<PurityVerdict, ParseError> {
        let name = self.expect_ident_any()?;
        match name.as_str() {
            "pure" => Ok(PurityVerdict::Pure),
            "impure" => Ok(PurityVerdict::Impure),
            "pure_except" => {
                self.expect_kind(TokenKind::LParen)?;
                let mut items = Vec::new();
                if !self.check_kind(&TokenKind::RParen) {
                    items.push(self.parse_path()?);
                    while self.check_kind(&TokenKind::Comma) {
                        self.advance();
                        items.push(self.parse_path()?);
                    }
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(PurityVerdict::PureExcept(items))
            }
            _ => Err(self.error(&format!(
                "unknown purity verdict '{}' (allowed: pure, pure_except(...), impure)",
                name
            ))),
        }
    }

    fn parse_termination_block(&mut self) -> Result<Termination, ParseError> {
        self.expect_ident("termination")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut form = None;
        let mut bound = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "form" => {
                    let n = self.expect_ident_any()?;
                    form = Some(match n.as_str() {
                        "constant_bound" => TerminationForm::ConstantBound,
                        "variable_bound" => TerminationForm::VariableBound,
                        "decreasing_recursion" => TerminationForm::DecreasingRecursion,
                        "unproven" => TerminationForm::Unproven,
                        _ => return Err(self.error(&format!(
                            "unknown termination form '{}' (allowed: constant_bound, variable_bound, decreasing_recursion, unproven)",
                            n
                        ))),
                    });
                }
                "bound" => {
                    bound = Some(self.expect_number()?);
                }
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in termination block (allowed: form, bound)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        let form = form.ok_or_else(|| self.error("termination missing 'form'"))?;
        Ok(Termination { form, bound })
    }

    fn parse_determinism_block(&mut self) -> Result<Determinism, ParseError> {
        self.expect_ident("determinism")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut form = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "form" => {
                    let n = self.expect_ident_any()?;
                    form = Some(match n.as_str() {
                        "total" => DeterminismForm::Total,
                        "conditional" => DeterminismForm::Conditional,
                        "nondeterministic" => DeterminismForm::Nondeterministic,
                        _ => return Err(self.error(&format!(
                            "unknown determinism form '{}' (allowed: total, conditional, nondeterministic)",
                            n
                        ))),
                    });
                }
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in determinism block (allowed: form)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        let form = form.ok_or_else(|| self.error("determinism missing 'form'"))?;
        Ok(Determinism { form })
    }

    fn parse_reaction(&mut self) -> Result<Reaction, ParseError> {
        self.expect_ident("reaction")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut trigger = None;
        let mut effects = Vec::new();

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            if let Some(attr) = self.peek_attribute_name() {
                match attr.as_str() {
                    "intention" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        intention = Some(self.expect_string()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    "source" => {
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        source = Some(self.parse_source_ref()?);
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown attribute '@{}' in reaction",
                            other
                        )));
                    }
                }
            } else if self.check_ident("trigger") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                trigger = Some(self.expect_ident_any()?);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("effects") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                self.expect_kind(TokenKind::Newline)?;
                self.expect_kind(TokenKind::Indent)?;
                while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
                    let kind_name = self.expect_ident_any()?;
                    let kind = match kind_name.as_str() {
                        "print" => EffectKind::Print,
                        _ => {
                            return Err(self.error(&format!(
                                "unknown effect '{}' (allowed: print)",
                                kind_name
                            )))
                        }
                    };
                    let mut args = Vec::new();
                    // Parse effect arguments until newline
                    while !self.check_kind(&TokenKind::Newline) && !self.at_eof() {
                        args.push(self.parse_expr()?);
                    }
                    self.expect_kind(TokenKind::Newline)?;
                    effects.push(Effect { kind, args });
                }
                self.expect_kind(TokenKind::Dedent)?;
            } else {
                return Err(self.error("expected attribute, 'trigger:', or 'effects:' in reaction"));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Reaction {
            name,
            intention: intention.ok_or_else(|| self.error("reaction missing @intention"))?,
            source: source.ok_or_else(|| self.error("reaction missing @source"))?,
            trigger: trigger.ok_or_else(|| self.error("reaction missing 'trigger'"))?,
            effects,
        })
    }

    fn parse_hints_block(&mut self) -> Result<Hints, ParseError> {
        self.expect_ident("hints")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut vectorizable = None;
        let mut parallel = None;
        let mut cache_result = None;
        let mut overflow = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "overflow" => {
                    // overflow : [min, max]
                    self.expect_kind(TokenKind::LBracket)?;
                    let min = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::Comma)?;
                    let max = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::RBracket)?;
                    overflow = Some(OverflowHint { min, max });
                }
                _ => {
                    let val = self.expect_ident_any()?;
                    let b = match val.as_str() {
                        "yes" => true,
                        "no" => false,
                        _ => {
                            return Err(self.error(&format!(
                                "expected 'yes' or 'no' for hint '{}', got '{}'",
                                key, val
                            )))
                        }
                    };
                    match key.as_str() {
                        "vectorizable" => vectorizable = Some(b),
                        "parallel" => parallel = Some(b),
                        "cache_result" => cache_result = Some(b),
                        _ => {
                            return Err(self.error(&format!(
                                "unknown hint '{}' (allowed: vectorizable, parallel, cache_result, overflow)",
                                key
                            )))
                        }
                    }
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Hints {
            vectorizable,
            parallel,
            cache_result,
            overflow,
        })
    }

    fn parse_signed_number(&mut self) -> Result<i64, ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::Minus)) {
            self.advance();
            let n = self.expect_number()?;
            Ok(-n)
        } else {
            self.expect_number()
        }
    }

    // --- cursor helpers ---

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.tokens.get(self.pos).map(|t| &t.kind)
    }

    fn advance(&mut self) {
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Eof))
    }

    fn check_kind(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == Some(kind)
    }

    fn check_ident(&self, name: &str) -> bool {
        matches!(self.peek_kind(), Some(TokenKind::Ident(n)) if n.as_str() == name)
    }

    fn peek_attribute_name(&self) -> Option<String> {
        if let Some(TokenKind::Attribute(n)) = self.peek_kind() {
            Some(n.clone())
        } else {
            None
        }
    }

    fn expect_kind(&mut self, kind: TokenKind) -> Result<(), ParseError> {
        if self.check_kind(&kind) {
            self.advance();
            Ok(())
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected {}, got {}", kind, got)))
        }
    }

    fn expect_ident(&mut self, name: &str) -> Result<(), ParseError> {
        if self.check_ident(name) {
            self.advance();
            Ok(())
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected '{}', got {}", name, got)))
        }
    }

    fn expect_ident_any(&mut self) -> Result<String, ParseError> {
        if let Some(TokenKind::Ident(n)) = self.peek_kind() {
            let n = n.clone();
            self.advance();
            Ok(n)
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected identifier, got {}", got)))
        }
    }

    fn expect_number(&mut self) -> Result<i64, ParseError> {
        if let Some(TokenKind::Number(n)) = self.peek_kind() {
            let n = *n;
            self.advance();
            Ok(n)
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected number, got {}", got)))
        }
    }

    fn expect_string(&mut self) -> Result<String, ParseError> {
        if let Some(TokenKind::StringLit(s)) = self.peek_kind() {
            let s = s.clone();
            self.advance();
            Ok(s)
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected string literal, got {}", got)))
        }
    }

    fn expect_attribute(&mut self, name: &str) -> Result<(), ParseError> {
        if matches!(self.peek_kind(), Some(TokenKind::Attribute(n)) if n.as_str() == name) {
            self.advance();
            Ok(())
        } else {
            let got = self.peek().kind.clone();
            Err(self.error(&format!("expected '@{}', got {}", name, got)))
        }
    }

    fn error(&self, message: &str) -> ParseError {
        let t = self.peek();
        ParseError {
            line: t.line,
            col: t.col,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;

    fn parse(src: &str) -> Result<Program, ParseError> {
        let tokens = Lexer::new(src).tokenize().map_err(|e| ParseError {
            line: e.line,
            col: e.col,
            message: e.message,
        })?;
        Parser::new(tokens).parse_program()
    }

    #[test]
    fn minimal_concept() {
        let src = "@verbose 0.1.0\n\nconcept Foo\n  @intention: \"a foo\"\n  @source: foo.intent:1\n  fields:\n    x : number\n";
        let p = parse(src).unwrap();
        assert_eq!(
            p.version,
            Version {
                major: 0,
                minor: 1,
                patch: 0,
            }
        );
        assert_eq!(p.items.len(), 1);
        match &p.items[0] {
            Item::Concept(c) => {
                assert_eq!(c.name, "Foo");
                assert_eq!(c.intention, "a foo");
                assert_eq!(c.source.file, "foo.intent");
                assert_eq!(c.source.line, 1);
                assert_eq!(c.fields.len(), 1);
                assert_eq!(c.fields[0].name, "x");
                assert_eq!(c.fields[0].ty, Type::Number);
            }
            _ => panic!("expected concept"),
        }
    }

    #[test]
    fn unknown_attribute_rejected() {
        let src = "@verbose 0.1.0\n\nconcept Foo\n  @intention: \"x\"\n  @source: f.intent:1\n  @bogus: \"nope\"\n  fields:\n    x : number\n";
        let err = parse(src).unwrap_err();
        assert!(err.message.contains("@bogus"), "got: {}", err.message);
    }

    #[test]
    fn missing_required_attribute() {
        let src = "@verbose 0.1.0\n\nconcept Foo\n  @intention: \"x\"\n  fields:\n    x : number\n";
        let err = parse(src).unwrap_err();
        assert!(err.message.contains("@source"), "got: {}", err.message);
    }

    #[test]
    fn full_pure_rule() {
        let src = r#"@verbose 0.1.0

concept Invoice
  @intention: "invoice"
  @source: i.intent:1
  fields:
    amount : number

rule important_invoice
  @intention: "important"
  @source: i.intent:2
  input:
    i : Invoice
  output:
    important : bool
  logic:
    important = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
"#;
        let p = parse(src).unwrap();
        assert_eq!(p.items.len(), 2);
        match &p.items[1] {
            Item::Rule(r) => {
                assert_eq!(r.name, "important_invoice");
                assert_eq!(r.input_name, "i");
                assert_eq!(r.input_ty, Type::Named("Invoice".into()));
                assert_eq!(r.output_name, "important");
                assert_eq!(r.output_ty, Type::Bool);
                assert_eq!(r.logic.target, "important");
                match &r.logic.value {
                    Expr::Binary(BinOp::Gt, left, right) => {
                        match left.as_ref() {
                            Expr::Field(base, f) => {
                                assert!(matches!(base.as_ref(), Expr::Ident(n) if n == "i"));
                                assert_eq!(f, "amount");
                            }
                            _ => panic!("expected field access"),
                        }
                        assert!(matches!(right.as_ref(), Expr::Number(10000)));
                    }
                    _ => panic!("expected Gt comparison"),
                }
                assert!(matches!(r.proofs.purity.verdict, PurityVerdict::Pure));
                assert_eq!(r.proofs.purity.reads.len(), 1);
                assert_eq!(r.proofs.purity.reads[0].segments, vec!["i", "amount"]);
                assert_eq!(r.proofs.termination.form, TerminationForm::ConstantBound);
                assert_eq!(r.proofs.termination.bound, Some(1));
                assert_eq!(r.proofs.determinism.form, DeterminismForm::Total);
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn if_then_else_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = if t.x > 10 then 1 else 0\n  proofs:\n    purity:\n      reads: [t.x]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 3\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::If(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn let_bindings_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    let y = t.x * 2\n    r = y + 1\n  proofs:\n    purity:\n      reads: [t.x]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 2\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert_eq!(r.logic.bindings.len(), 1);
                assert_eq!(r.logic.bindings[0].0, "y");
                assert_eq!(r.logic.target, "r");
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn field_ranges_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number [0, 100]\n    y : number [-50, 50]\n";
        let p = parse(src).unwrap();
        match &p.items[0] {
            Item::Concept(c) => {
                assert_eq!(c.fields[0].range, Some((0, 100)));
                assert_eq!(c.fields[1].range, Some((-50, 50)));
            }
            _ => panic!("expected concept"),
        }
    }

    #[test]
    fn collection_type_parsed() {
        let src = "@verbose 0.1.0\n\nconcept Item\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nconcept Box\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    items : collection(Item)\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Concept(c) => {
                assert_eq!(c.fields[0].ty, Type::Collection("Item".into()));
            }
            _ => panic!("expected concept"),
        }
    }

    #[test]
    fn quantifier_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    xs : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = all(t.xs, x => x > 0)\n  proofs:\n    purity:\n      reads: [t.xs]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 2\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Quantifier(QuantifierKind::All, _, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn hints_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 1\n    determinism:\n      form: total\n  hints:\n    vectorizable: yes\n    parallel: no\n    overflow: [0, 1]\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                let h = r.hints.as_ref().unwrap();
                assert_eq!(h.vectorizable, Some(true));
                assert_eq!(h.parallel, Some(false));
                assert!(h.overflow.is_some());
                let ov = h.overflow.as_ref().unwrap();
                assert_eq!(ov.min, 0);
                assert_eq!(ov.max, 1);
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn precedence_correct() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    a : number\n    b : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.a + 1 > t.b * 2 and t.a < 100\n  proofs:\n    purity:\n      reads: [t.a, t.b]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 5\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                // Should be: And(Gt(Add(a,1), Mul(b,2)), Lt(a, 100))
                assert!(matches!(&r.logic.value, Expr::Binary(BinOp::And, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn not_and_neg_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = not (t.x > 0)\n  proofs:\n    purity:\n      reads: [t.x]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 2\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Not(_)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn reaction_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule r\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    y : bool\n  logic:\n    y = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 1\n    determinism:\n      form: total\n\nreaction notify\n  @intention: \"notify when triggered\"\n  @source: f.intent:1\n  trigger: r\n  effects:\n    print \"hello\"\n";
        let p = parse(src).unwrap();
        assert_eq!(p.items.len(), 3); // concept + rule + reaction
        match &p.items[2] {
            Item::Reaction(rx) => {
                assert_eq!(rx.name, "notify");
                assert_eq!(rx.trigger, "r");
                assert_eq!(rx.effects.len(), 1);
                assert_eq!(rx.effects[0].kind, EffectKind::Print);
            }
            _ => panic!("expected reaction"),
        }
    }

    #[test]
    fn string_comparison_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    s : text\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.s == \"active\"\n  proofs:\n    purity:\n      reads: [t.s]\n      writes: []\n      calls: []\n      verdict: pure\n    termination:\n      form: constant_bound\n      bound: 1\n    determinism:\n      form: total\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Binary(BinOp::Eq, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }
}
