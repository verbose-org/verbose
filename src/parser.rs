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
            if self.check_ident("concept_group") {
                items.push(Item::ConceptGroup(self.parse_concept_group()?));
            } else if self.check_ident("concept") {
                items.push(Item::Concept(self.parse_concept()?));
            } else if self.check_ident("rule") {
                items.push(Item::Rule(self.parse_rule()?));
            } else if self.check_ident("reaction") {
                items.push(Item::Reaction(self.parse_reaction()?));
            } else if self.check_ident("service") {
                items.push(Item::Service(self.parse_service()?));
            } else if self.check_ident("resource") {
                items.push(Item::Resource(self.parse_resource()?));
            } else if self.check_ident("connection") {
                items.push(Item::Connection(self.parse_connection()?));
            } else {
                return Err(self.error("expected 'concept', 'concept_group', 'rule', 'reaction', 'service', 'resource', or 'connection' at top level"));
            }
        }
        Ok(Program { version, uses, items })
    }

    /// Phase B slice 1: parse a `concept_group` block.
    ///
    /// Grammar:
    /// ```
    /// concept_group <Name> [max_depth: <N>, max_nodes: <M>]
    ///   @intention: "..."
    ///   @source: file.intent:NN
    ///
    ///   concept Foo
    ///     ...
    ///   concept Bar
    ///     ...
    /// ```
    ///
    /// `max_depth` and `max_nodes` are declared in a single bracketed
    /// header on the same physical line as `concept_group <Name>`. Both
    /// are required; the order is fixed (depth then nodes) so the
    /// auditor never has to guess which bound is which. Inside the
    /// indented body, after the standard `@intention` / `@source`
    /// attributes, one or more `concept` blocks declare the sum types
    /// that compose the group; we reuse `parse_concept` so the variant
    /// syntax inside is byte-for-byte identical to a top-level concept.
    fn parse_concept_group(&mut self) -> Result<ConceptGroup, ParseError> {
        self.expect_ident("concept_group")?;
        let name = self.expect_ident_any()?;

        // Header bounds: `[max_depth: <N>, max_nodes: <M>]`. Required.
        // Order is fixed for predictable audit reading; the verifier
        // bounds-checks the values, the parser just enforces shape.
        self.expect_kind(TokenKind::LBracket)?;
        self.expect_ident("max_depth")?;
        self.expect_kind(TokenKind::Colon)?;
        let depth_raw = self.expect_number()?;
        if depth_raw < 0 {
            return Err(self.error(&format!(
                "concept_group '{}' max_depth must be non-negative, got {}",
                name, depth_raw
            )));
        }
        if depth_raw > u32::MAX as i64 {
            return Err(self.error(&format!(
                "concept_group '{}' max_depth {} exceeds u32 range",
                name, depth_raw
            )));
        }
        let max_depth = depth_raw as u32;
        self.expect_kind(TokenKind::Comma)?;
        self.expect_ident("max_nodes")?;
        self.expect_kind(TokenKind::Colon)?;
        let nodes_raw = self.expect_number()?;
        if nodes_raw < 0 {
            return Err(self.error(&format!(
                "concept_group '{}' max_nodes must be non-negative, got {}",
                name, nodes_raw
            )));
        }
        if nodes_raw > u32::MAX as i64 {
            return Err(self.error(&format!(
                "concept_group '{}' max_nodes {} exceeds u32 range",
                name, nodes_raw
            )));
        }
        let max_nodes = nodes_raw as u32;
        self.expect_kind(TokenKind::RBracket)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention: Option<String> = None;
        let mut source: Option<SourceRef> = None;
        let mut concepts: Vec<Concept> = Vec::new();

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
                            "unknown attribute '@{}' in concept_group (allowed: @intention, @source)",
                            other
                        )));
                    }
                }
            } else if self.check_ident("concept") {
                concepts.push(self.parse_concept()?);
            } else {
                return Err(self.error(
                    "expected attribute or 'concept ...' block in concept_group body",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let intention = intention.ok_or_else(|| {
            self.error(&format!("concept_group '{}' missing @intention", name))
        })?;
        let source = source.ok_or_else(|| {
            self.error(&format!("concept_group '{}' missing @source", name))
        })?;
        if concepts.is_empty() {
            return Err(self.error(&format!(
                "concept_group '{}' is empty — declare at least one concept inside",
                name
            )));
        }

        Ok(ConceptGroup {
            name,
            intention,
            source,
            max_depth,
            max_nodes,
            concepts,
        })
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
        // Phase A slice 1: optional `variants:` block. Mutually
        // exclusive with `fields:` — see parse_variants_block.
        let mut variants: Option<Vec<Variant>> = None;

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
            } else if self.check_ident("variants") {
                variants = Some(self.parse_variants_block()?);
            } else {
                return Err(self.error(
                    "expected attribute, 'fields:', or 'variants:' in concept body",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let intention = intention
            .ok_or_else(|| self.error(&format!("concept '{}' missing @intention", name)))?;
        let source = source
            .ok_or_else(|| self.error(&format!("concept '{}' missing @source", name)))?;

        // Phase A slice 1: concept must have exactly one of fields/variants.
        // Both empty → no shape declared. Both non-empty → ambiguous declaration.
        let (fields, variants) = match (fields, variants) {
            (Some(f), None) => (f, vec![]),
            (None, Some(v)) => (vec![], v),
            (Some(_), Some(_)) => {
                return Err(self.error(&format!(
                    "concept '{}' has both 'fields:' and 'variants:' blocks — only one is allowed (record OR sum type, not both)",
                    name
                )));
            }
            (None, None) => {
                return Err(self.error(&format!(
                    "concept '{}' missing 'fields:' or 'variants:' block",
                    name
                )));
            }
        };

        Ok(Concept {
            name,
            intention,
            source,
            fields,
            variants,
        })
    }

    /// Parse a `variants:` block.
    ///
    /// Syntax (Phase A slice 1):
    /// ```
    /// variants:
    ///   VarA of (x : number, y : text)
    ///   VarB of (z : bool)
    ///   VarC                              ; no payload
    /// ```
    ///
    /// One variant per indented line. Field syntax mirrors `fields:`
    /// (name : type), but inside a parenthesised payload after `of`.
    /// A no-payload variant omits `of (...)` entirely.
    fn parse_variants_block(&mut self) -> Result<Vec<Variant>, ParseError> {
        self.expect_ident("variants")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;
        let mut variants = Vec::new();
        let mut seen_names = std::collections::HashSet::new();
        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let variant_name = self.expect_ident_any()?;
            if !seen_names.insert(variant_name.clone()) {
                return Err(self.error(&format!(
                    "duplicate variant name '{}' in variants block",
                    variant_name
                )));
            }
            // Optional `of (...)` payload.
            let mut fields: Vec<Field> = Vec::new();
            let mut payload_field_names = std::collections::HashSet::new();
            if self.check_ident("of") {
                self.advance();
                self.expect_kind(TokenKind::LParen)?;
                while !self.check_kind(&TokenKind::RParen) {
                    let fname = self.expect_ident_any()?;
                    if !payload_field_names.insert(fname.clone()) {
                        return Err(self.error(&format!(
                            "duplicate payload field '{}' in variant '{}'",
                            fname, variant_name
                        )));
                    }
                    self.expect_kind(TokenKind::Colon)?;
                    let ty = self.parse_type()?;
                    // No range bounds on variant payload fields in slice 1;
                    // can be lifted later if needed.
                    fields.push(Field { name: fname, ty, range: None });
                    if self.check_kind(&TokenKind::Comma) {
                        self.advance();
                    } else {
                        break;
                    }
                }
                self.expect_kind(TokenKind::RParen)?;
            }
            self.expect_kind(TokenKind::Newline)?;
            variants.push(Variant {
                name: variant_name,
                fields,
            });
        }
        self.expect_kind(TokenKind::Dedent)?;
        if variants.is_empty() {
            return Err(self.error(
                "variants block cannot be empty — declare at least one variant",
            ));
        }
        Ok(variants)
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
                if self.check_kind(&TokenKind::Dot) {
                    // text/bytes [..N] — max byte length bound
                    self.advance(); // first dot
                    self.expect_kind(TokenKind::Dot)?; // second dot
                    let max = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::RBracket)?;
                    if !matches!(ty, Type::Text | Type::Bytes) {
                        return Err(self.error("[..N] bound syntax is only valid for text or bytes fields; use [min, max] for numbers"));
                    }
                    if max <= 0 {
                        return Err(self.error("max-length bound must be positive"));
                    }
                    Some((0, max))
                } else {
                    // number [min, max] — arithmetic range bound
                    let min = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::Comma)?;
                    let max = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::RBracket)?;
                    Some((min, max))
                }
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
            "bytes" => Type::Bytes,
            "collection" => {
                self.expect_kind(TokenKind::LParen)?;
                let inner = self.expect_ident_any()?;
                self.expect_kind(TokenKind::RParen)?;
                Type::Collection(inner)
            }
            "Result" => {
                // Result(T, E) — a declared failure path. T and E are arbitrary types.
                self.expect_kind(TokenKind::LParen)?;
                let t = self.parse_type()?;
                self.expect_kind(TokenKind::Comma)?;
                let e = self.parse_type()?;
                self.expect_kind(TokenKind::RParen)?;
                Type::Result(Box::new(t), Box::new(e))
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
        let mut context: Option<(String, Type)> = None;
        let mut output: Option<(String, Type)> = None;
        let mut logic = None;
        let mut proofs = None;
        let mut hints = None;
        let mut layer: Option<Layer> = None;

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
                    "layer" => {
                        // @layer: domain | application | interface
                        // Optional. When present, the verifier enforces that this
                        // rule only calls other layered rules, and only those
                        // of layers this one is allowed to call.
                        self.advance();
                        self.expect_kind(TokenKind::Colon)?;
                        let name_ident = self.expect_ident_any()?;
                        layer = Some(match name_ident.as_str() {
                            "domain" => Layer::Domain,
                            "application" => Layer::Application,
                            "interface" => Layer::Interface,
                            other => {
                                return Err(self.error(&format!(
                                    "unknown layer '{}' (allowed: domain, application, interface)",
                                    other
                                )));
                            }
                        });
                        self.expect_kind(TokenKind::Newline)?;
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown attribute '@{}' in rule (allowed: @intention, @source, @layer)",
                            other
                        )));
                    }
                }
            } else if self.check_ident("context") {
                context = Some(self.parse_binding_block("context")?);
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

        let (context_name, context_ty) = match context {
            Some((n, t)) => (Some(n), Some(t)),
            None => (None, None),
        };

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
            layer,
            context_name,
            context_ty,
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
        // Statement terminator: either NEWLINE then DEDENT (the usual
        // single-line form), or just DEDENT (when the expression was
        // a multi-line if/then/else whose balance loop consumed the
        // trailing NEWLINE while closing matching INDENTs).
        if matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.advance();
        }
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
            // Multi-line `if/then/else`: NEWLINE / INDENT / DEDENT
            // tokens between the structural keywords (`if`, `then`,
            // `else`) and their adjoining sub-expressions are skipped,
            // INDENT/DEDENT in balanced pairs tracked by a local depth
            // counter. Lets both the same-column form
            //     status = if cond
            //     then concat(...)
            //     else concat(...)
            // AND the indented form
            //     status = if cond
            //              then concat(...)
            //              else concat(...)
            // parse cleanly. The eval surfaced the trap on Opus 4.7
            // generating examples/holdout/flight_status — Opus
            // gravitates to the indented form (a common natural-language
            // habit) and the parser used to reject "expected 'then',
            // got INDENT".
            //
            // The depth counter ensures we only consume DEDENTs that
            // match INDENTs we ate during this if-expression. DEDENTs
            // that close the surrounding block (e.g. the `logic:` body)
            // stay untouched for the parent parser. After the
            // else-expression, we consume any remaining matching
            // DEDENTs so the if-expression leaves the indent state
            // exactly where it found it.
            let mut depth: u32 = 0;
            self.skip_if_separators(&mut depth);
            let condition = self.parse_expr()?;
            self.skip_if_separators(&mut depth);
            self.expect_ident("then")?;
            self.skip_if_separators(&mut depth);
            let then_expr = self.parse_expr()?;
            self.skip_if_separators(&mut depth);
            self.expect_ident("else")?;
            self.skip_if_separators(&mut depth);
            let else_expr = self.parse_expr()?;
            // Balance any indent we opened. NEWLINEs interleaved with
            // DEDENTs are skipped along the way.
            while depth > 0 {
                match self.peek_kind() {
                    Some(TokenKind::Newline) => self.advance(),
                    Some(TokenKind::Dedent) => { depth -= 1; self.advance(); }
                    _ => break,
                }
            }
            return Ok(Expr::If(
                Box::new(condition),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        // Phase A slice 3 — block-form pattern match:
        //
        //   match <scrutinee>:
        //     VarA(b1, _, b3) => body_a
        //     VarB(n)         => body_b
        //     VarC            => body_c          -- no-payload variant
        //
        // After `match` we parse the scrutinee on the same line, an
        // explicit `:`, then NEWLINE INDENT, one arm per line, DEDENT.
        // Arms are positional destructurings of the matched variant's
        // payload — `_` is a wildcard (no binding), every other ident
        // binds the field's value to that name in the arm body.
        //
        // Exhaustiveness, arm-arity, and binder-collision are verifier
        // checks (slice 3 of Phase A), not parser-level. The parser
        // only enforces the syntactic shape; semantic validation
        // happens against the resolved sum-type concept.
        //
        // `match` is reserved in expression position here: once we see
        // it, we commit to parsing the block form. The standalone
        // identifier `match` is not used elsewhere in the language.
        if self.check_ident("match") {
            self.advance(); // match
            let scrutinee = self.parse_expr()?;
            self.expect_kind(TokenKind::Colon)?;
            self.expect_kind(TokenKind::Newline)?;
            self.expect_kind(TokenKind::Indent)?;
            let mut arms: Vec<MatchArm> = Vec::new();
            while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
                // Skip stray blank lines between arms.
                if self.check_kind(&TokenKind::Newline) {
                    self.advance();
                    continue;
                }
                let variant_name = self.expect_ident_any()?;
                let mut binders: Vec<Option<String>> = Vec::new();
                if self.check_kind(&TokenKind::LParen) {
                    self.advance(); // (
                    if !self.check_kind(&TokenKind::RParen) {
                        loop {
                            let binder = self.expect_ident_any()?;
                            // `_` is the wildcard; every other ident
                            // binds the field positionally.
                            if binder == "_" {
                                binders.push(None);
                            } else {
                                binders.push(Some(binder));
                            }
                            if self.check_kind(&TokenKind::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect_kind(TokenKind::RParen)?;
                }
                self.expect_kind(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                // Each arm ends with a NEWLINE (lexer always emits one
                // at end-of-line). The DEDENT closes the block.
                self.expect_kind(TokenKind::Newline)?;
                arms.push(MatchArm { variant_name, binders, body });
            }
            self.expect_kind(TokenKind::Dedent)?;
            return Ok(Expr::MatchVariant(Box::new(scrutinee), arms));
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
            if self.check_kind(&TokenKind::DoubleColon) {
                // Phase A slice 2: variant construction.
                // `ConceptName::VariantName { field: expr, ... }`  (with payload)
                // `ConceptName::VariantName`                       (no payload)
                //
                // The verifier resolves the concept/variant and cross-checks
                // the field set. Same field-block shape as `Record(name, fields)`
                // below, with the variant qualifier captured as the second
                // String in the AST node.
                self.advance(); // ::
                let variant_name = self.expect_ident_any()?;
                let mut fields = Vec::new();
                if self.check_kind(&TokenKind::LBrace) {
                    self.advance(); // {
                    if !self.check_kind(&TokenKind::RBrace) {
                        loop {
                            let field_name = self.expect_ident_any()?;
                            self.expect_kind(TokenKind::Colon)?;
                            let value = self.parse_expr()?;
                            fields.push((field_name, value));
                            if self.check_kind(&TokenKind::Comma) {
                                self.advance();
                                if self.check_kind(&TokenKind::RBrace) {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect_kind(TokenKind::RBrace)?;
                }
                return Ok(Expr::VariantConstruct(name, variant_name, fields));
            }
            if (name == "sum" || name == "count" || name == "min" || name == "max") && self.check_kind(&TokenKind::LParen) {
                // Sugar: sum(coll, var => expr) → fold(coll, 0, __acc, var => __acc + expr)
                //        count(coll, var => pred) → fold(coll, 0, __acc, var => __acc + if pred then 1 else 0)
                //
                // For `min` and `max`, an alternative shape exists since
                // 2026-05-01: `min(a, b)` / `max(a, b)` are binary scalar
                // primitives with no lambda. We disambiguate by parsing
                // the first two args, then checking the next token: if it's
                // `=>`, the second arg was actually the lambda var (must be
                // an Ident) and we proceed with the fold path; if it's `)`,
                // we shipped the binary form. `sum`/`count` only have the
                // fold form and skip the disambiguation.
                self.advance(); // (
                let collection = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let second_pos = self.pos;
                let second = self.parse_expr()?;
                let is_fold_lambda = self.check_kind(&TokenKind::FatArrow);
                if (name == "min" || name == "max") && !is_fold_lambda {
                    // Binary form: `second` is the second number arg.
                    self.expect_kind(TokenKind::RParen)?;
                    return Ok(if name == "min" {
                        Expr::Min(Box::new(collection), Box::new(second))
                    } else {
                        Expr::Max(Box::new(collection), Box::new(second))
                    });
                }
                // Fold form: rewind to `second_pos` and re-parse the var as
                // an Ident (parse_expr could have consumed more than just
                // the bare ident in obscure shapes; the rewind keeps the
                // existing path semantically identical to before this slice).
                self.pos = second_pos;
                let var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;

                let acc = "__acc".to_string();
                let fold_body = match name.as_str() {
                    "sum" => {
                        // __acc + expr
                        Expr::Binary(BinOp::Add, Box::new(Expr::Ident(acc.clone())), Box::new(body))
                    }
                    "count" => {
                        // __acc + if pred then 1 else 0
                        Expr::Binary(
                            BinOp::Add,
                            Box::new(Expr::Ident(acc.clone())),
                            Box::new(Expr::If(Box::new(body), Box::new(Expr::Number(1)), Box::new(Expr::Number(0)))),
                        )
                    }
                    "min" => {
                        // if expr < __acc then expr else __acc
                        Expr::If(
                            Box::new(Expr::Binary(BinOp::Lt, Box::new(body.clone()), Box::new(Expr::Ident(acc.clone())))),
                            Box::new(body),
                            Box::new(Expr::Ident(acc.clone())),
                        )
                    }
                    "max" => {
                        // if expr > __acc then expr else __acc
                        Expr::If(
                            Box::new(Expr::Binary(BinOp::Gt, Box::new(body.clone()), Box::new(Expr::Ident(acc.clone())))),
                            Box::new(body),
                            Box::new(Expr::Ident(acc.clone())),
                        )
                    }
                    _ => unreachable!(),
                };

                // Initial value: 0 for sum/count, MAX for min, MIN for max
                let initial = match name.as_str() {
                    "min" => Expr::Number(i64::MAX),
                    "max" => Expr::Number(i64::MIN),
                    _ => Expr::Number(0),
                };

                Ok(Expr::Fold(
                    Box::new(collection),
                    Box::new(initial),
                    acc,
                    var,
                    Box::new(fold_body),
                ))
            } else if name == "fold" && self.check_kind(&TokenKind::LParen) {
                self.advance(); // (
                let collection = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let initial = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let acc = self.expect_ident_any()?;
                self.expect_kind(TokenKind::Comma)?;
                let item = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Fold(
                    Box::new(collection),
                    Box::new(initial),
                    acc,
                    item,
                    Box::new(body),
                ))
            } else if name == "fold_bytes" && self.check_kind(&TokenKind::LParen) {
                // `fold_bytes(<text>, <init>, acc, byte, idx => <body>)`
                // Byte-level iteration with three bound Number-typed names
                // (acc, byte, idx). Same arity shape as `fold` plus one
                // extra identifier (idx) before the `=>`. Body returns
                // Number (the next accumulator value).
                self.advance(); // (
                let text = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let initial = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let acc = self.expect_ident_any()?;
                self.expect_kind(TokenKind::Comma)?;
                let byte_var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::Comma)?;
                let idx_var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::FoldBytes(
                    Box::new(text),
                    Box::new(initial),
                    acc,
                    byte_var,
                    idx_var,
                    Box::new(body),
                ))
            } else if (name == "all" || name == "any") && self.check_kind(&TokenKind::LParen) {
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
            } else if (name == "map" || name == "filter") && self.check_kind(&TokenKind::LParen) {
                // map(coll, var => body)    returns collection(T)
                // filter(coll, var => pred) returns collection of same element type
                self.advance(); // (
                let collection = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let body = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                if name == "map" {
                    Ok(Expr::Map(Box::new(collection), var, Box::new(body)))
                } else {
                    Ok(Expr::Filter(Box::new(collection), var, Box::new(body)))
                }
            } else if (name == "Ok" || name == "Err") && self.check_kind(&TokenKind::LParen) {
                // Result constructors: Ok(expr) | Err(expr).
                // Intercepted before the generic call branch so they are never
                // treated as rule calls named "Ok" or "Err".
                self.advance(); // (
                let inner = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                if name == "Ok" {
                    Ok(Expr::Ok(Box::new(inner)))
                } else {
                    Ok(Expr::Err(Box::new(inner)))
                }
            } else if self.check_kind(&TokenKind::LBrace) {
                // Record constructor: ConceptName { field: expr, field: expr, ... }
                // Single-line by convention — the lexer's paren-depth tracking
                // suppresses INDENT/DEDENT inside braces, but for clarity all
                // examples and tests use a single line. The concept name is
                // looked up by the verifier; the parser only ensures syntax.
                self.advance(); // {
                let mut fields = Vec::new();
                if !self.check_kind(&TokenKind::RBrace) {
                    loop {
                        let field_name = self.expect_ident_any()?;
                        self.expect_kind(TokenKind::Colon)?;
                        let value = self.parse_expr()?;
                        fields.push((field_name, value));
                        if self.check_kind(&TokenKind::Comma) {
                            self.advance();
                            // Trailing comma allowed: stop if next is closing brace.
                            if self.check_kind(&TokenKind::RBrace) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect_kind(TokenKind::RBrace)?;
                Ok(Expr::Record(name, fields))
            } else if name == "concat" && self.check_kind(&TokenKind::LParen) {
                // concat(e1, e2, ...) — variadic text builder. At least one arg.
                // Each arg must be scalar (number/bool/text); the verifier
                // rejects collection / Result / record arguments.
                self.advance(); // (
                let mut args = Vec::new();
                if !self.check_kind(&TokenKind::RParen) {
                    args.push(self.parse_expr()?);
                    while self.check_kind(&TokenKind::Comma) {
                        self.advance();
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect_kind(TokenKind::RParen)?;
                if args.is_empty() {
                    return Err(self.error("concat() requires at least one argument"));
                }
                Ok(Expr::Concat(args))
            } else if name == "read" && self.check_kind(&TokenKind::LParen) {
                // Phase 9 slice 1: read(<resource_name>) — load the
                // declared resource's contents as text. The argument must
                // be a bare identifier naming a top-level resource; the
                // verifier checks that the name resolves and that the
                // rule's `reads:` proof lists it.
                self.advance(); // (
                let resource_name = self.expect_ident_any()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Read(resource_name))
            } else if name == "fetch" && self.check_kind(&TokenKind::LParen) {
                // Phase 11 slice 1: fetch(<connection_name>, <request_bytes>)
                // — dial the declared TCP endpoint, send `request_bytes`
                // (which must produce text), read up to max_response bytes,
                // close the socket, return the response as text. The
                // first argument is a bare identifier naming a top-level
                // connection; the verifier checks that the name resolves
                // and that the rule's `reads:` proof lists it. The second
                // argument is any text-producing expression (string
                // literal, concat, etc.).
                self.advance(); // (
                let connection_name = self.expect_ident_any()?;
                self.expect_kind(TokenKind::Comma)?;
                let request = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Fetch(connection_name, Box::new(request)))
            } else if name == "json_escape" && self.check_kind(&TokenKind::LParen) {
                // Phase 12 (json_escape): json_escape(<text_expr>) — pure
                // text-transform that escapes JSON-significant bytes in
                // its input. Exactly one argument; zero or two-plus is a
                // parse-time error. The verifier checks that the inner
                // expression produces text.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("json_escape requires exactly one argument, got zero"));
                }
                let inner = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("json_escape requires exactly one argument, got more than one"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::JsonEscape(Box::new(inner)))
            } else if name == "parse_int" && self.check_kind(&TokenKind::LParen) {
                // Phase 12 (parse_int): parse_int(<text_expr>) — strict
                // text-to-number conversion. Exactly one argument; zero or
                // two-plus is a parse-time error. The verifier checks that
                // the inner expression produces text; the runtime fails
                // closed (sys_exit 1) if the bytes don't form a valid
                // signed integer.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("parse_int requires exactly one argument, got zero"));
                }
                let inner = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("parse_int requires exactly one argument, got more than one"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::ParseInt(Box::new(inner)))
            } else if name == "now_unix" && self.check_kind(&TokenKind::LParen) {
                // `now_unix()` — nullary primitive returning the current
                // Unix epoch seconds as a number. No arguments allowed; any
                // token between `(` and `)` is a parse-time error. The
                // verifier requires the rule's `reads:` proof to declare
                // the synthetic name `now`.
                self.advance(); // (
                if !self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("now_unix takes no arguments"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::NowUnix)
            } else if name == "starts_with" && self.check_kind(&TokenKind::LParen) {
                // `starts_with(<haystack>, <needle>)` — byte-level prefix
                // test returning bool. Exactly two arguments; zero / one /
                // three-plus is a parse-time error so the message points at
                // the call site rather than failing later in the verifier.
                // The verifier enforces that both children are text-typed.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("starts_with requires exactly two arguments, got zero"));
                }
                let haystack = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("starts_with requires exactly two arguments, got one"));
                }
                self.advance(); // ,
                let needle = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("starts_with requires exactly two arguments, got more than two"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::StartsWith(Box::new(haystack), Box::new(needle)))
            } else if name == "contains" && self.check_kind(&TokenKind::LParen) {
                // `contains(<haystack>, <needle>)` — byte-level substring
                // test returning bool. Same arity rules as starts_with:
                // exactly two arguments, zero / one / three-plus is a
                // parse-time error so the message points at the call
                // site. The verifier enforces that both children are
                // text-typed.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("contains requires exactly two arguments, got zero"));
                }
                let haystack = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("contains requires exactly two arguments, got one"));
                }
                self.advance(); // ,
                let needle = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("contains requires exactly two arguments, got more than two"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Contains(Box::new(haystack), Box::new(needle)))
            } else if name == "ends_with" && self.check_kind(&TokenKind::LParen) {
                // `ends_with(<haystack>, <needle>)` — byte-level suffix test
                // returning bool. Mirror of starts_with: exactly two
                // arguments; zero / one / three-plus is a parse-time error
                // so the message points at the call site. The verifier
                // enforces that both children are text-typed.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("ends_with requires exactly two arguments, got zero"));
                }
                let haystack = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("ends_with requires exactly two arguments, got one"));
                }
                self.advance(); // ,
                let needle = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("ends_with requires exactly two arguments, got more than two"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::EndsWith(Box::new(haystack), Box::new(needle)))
            } else if name == "length" && self.check_kind(&TokenKind::LParen) {
                // `length(<text_expr>)` — byte count of inner text as a
                // number. Exactly one argument; zero or two-plus is a
                // parse-time error. The verifier checks that the inner
                // expression produces text. Same shape as parse_int.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("length requires exactly one argument, got zero"));
                }
                let inner = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("length requires exactly one argument, got more than one"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Length(Box::new(inner)))
            } else if name == "substring" && self.check_kind(&TokenKind::LParen) {
                // `substring(<text>, <start>, <end>)` — half-open slice
                // by byte offset, returns text. Exactly three arguments;
                // any other arity is a parse-time error. The verifier
                // checks text-typed first arg and Number-typed start/end;
                // bounds are enforced mechanically at runtime, fail-closed.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("substring requires exactly three arguments (text, start, end), got zero"));
                }
                let text_arg = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("substring requires exactly three arguments (text, start, end), got one"));
                }
                self.expect_kind(TokenKind::Comma)?;
                let start_arg = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("substring requires exactly three arguments (text, start, end), got two"));
                }
                self.expect_kind(TokenKind::Comma)?;
                let end_arg = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("substring requires exactly three arguments (text, start, end), got more than three"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Substring(Box::new(text_arg), Box::new(start_arg), Box::new(end_arg)))
            } else if name == "byte_at" && self.check_kind(&TokenKind::LParen) {
                // `byte_at(<text>, <index>)` — read the byte at a given
                // offset of the text, returning a Number in 0..256.
                // Exactly two arguments; any other arity is a parse-time
                // error. The verifier checks text-typed first arg and
                // Number-typed index; bounds are enforced fail-closed
                // at runtime.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("byte_at requires exactly two arguments (text, index), got zero"));
                }
                let text_arg = self.parse_expr()?;
                if !self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("byte_at requires exactly two arguments (text, index), got one"));
                }
                self.expect_kind(TokenKind::Comma)?;
                let index_arg = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("byte_at requires exactly two arguments (text, index), got more than two"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::ByteAt(Box::new(text_arg), Box::new(index_arg)))
            } else if name == "abs" && self.check_kind(&TokenKind::LParen) {
                // `abs(<number_expr>)` — absolute value. Exactly one argument;
                // zero or two-plus is a parse-time error. The verifier checks
                // that the inner expression produces number (NOT text — this
                // is the key difference from parse_int/length). Same arity
                // shape as parse_int.
                self.advance(); // (
                if self.check_kind(&TokenKind::RParen) {
                    return Err(self.error("abs requires exactly one argument, got zero"));
                }
                let inner = self.parse_expr()?;
                if self.check_kind(&TokenKind::Comma) {
                    return Err(self.error("abs requires exactly one argument, got more than one"));
                }
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::Abs(Box::new(inner)))
            } else if name == "match_result" && self.check_kind(&TokenKind::LParen) {
                // match_result(target, ok_var => ok_body, err_var => err_body)
                // The Result consumer. Both arms are explicit — no implicit
                // Err-propagation — so the reader sees what happens on failure.
                self.advance(); // (
                let target = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let ok_var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let ok_body = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
                let err_var = self.expect_ident_any()?;
                self.expect_kind(TokenKind::FatArrow)?;
                let err_body = self.parse_expr()?;
                self.expect_kind(TokenKind::RParen)?;
                Ok(Expr::MatchResult(
                    Box::new(target),
                    ok_var,
                    Box::new(ok_body),
                    err_var,
                    Box::new(err_body),
                ))
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

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            if self.check_ident("purity") {
                purity = Some(self.parse_purity_block()?);
            } else if self.check_ident("termination") {
                termination = Some(self.parse_termination_block()?);
            } else {
                return Err(self.error(
                    "expected 'purity' or 'termination' in proofs block",
                ));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        let purity = purity.ok_or_else(|| self.error("proofs missing 'purity'"))?;
        let termination = termination.ok_or_else(|| self.error("proofs missing 'termination'"))?;

        Ok(Proofs {
            purity,
            termination,
        })
    }

    fn parse_purity_block(&mut self) -> Result<Purity, ParseError> {
        self.expect_ident("purity")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut reads = None;
        let mut calls = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "reads" => reads = Some(self.parse_path_list()?),
                "calls" => calls = Some(self.parse_path_list()?),
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in purity block (allowed: reads, calls)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        let reads = reads.ok_or_else(|| self.error("purity missing 'reads'"))?;
        let calls = calls.ok_or_else(|| self.error("purity missing 'calls'"))?;

        Ok(Purity {
            reads,
            calls,
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

    fn parse_termination_block(&mut self) -> Result<Termination, ParseError> {
        self.expect_ident("termination")?;
        self.expect_kind(TokenKind::Colon)?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut bound = None;
        let mut structural = None;
        let mut decreasing = None;
        let mut increasing = None;

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "bound" => {
                    bound = Some(self.expect_number()?);
                }
                "structural" => {
                    structural = Some(self.expect_ident_any()?);
                }
                "decreasing" => {
                    decreasing = Some(self.expect_ident_any()?);
                }
                "increasing" => {
                    increasing = Some(self.expect_ident_any()?);
                }
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in termination block (allowed: bound, structural, decreasing, increasing)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Termination { bound, structural, decreasing, increasing })
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
                    let effect = match kind_name.as_str() {
                        "print" => {
                            // print e1 e2 ... — positional args printed space-separated.
                            let mut args = Vec::new();
                            while !self.check_kind(&TokenKind::Newline) && !self.at_eof() {
                                args.push(self.parse_expr()?);
                            }
                            Effect::Print(args)
                        }
                        "append_file" => {
                            // append_file "path" content_expr
                            // Path MUST be a string literal at the source level —
                            // the auditor must be able to read every file path
                            // this program can touch. No dynamic paths.
                            let path = self.expect_string().map_err(|_| {
                                self.error("append_file requires a string literal path — dynamic paths are refused so the auditor can see every file this program can write")
                            })?;
                            let content = self.parse_expr()?;
                            Effect::AppendFile { path, content }
                        }
                        _ => {
                            return Err(self.error(&format!(
                                "unknown effect '{}' (allowed: print, append_file)",
                                kind_name
                            )))
                        }
                    };
                    self.expect_kind(TokenKind::Newline)?;
                    effects.push(effect);
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

    /// Phase 7: parse a `service` top-level block. Grammar:
    ///
    ///     service <name>
    ///       @intention: "..."
    ///       @source: file.intent:N
    ///       listen:
    ///         protocol   : raw_tcp
    ///         port       : 9999
    ///         max_request: 4096
    ///       handler: <rule_name>
    ///
    /// The `listen:` block carries the three properties that constrain what
    /// the binary may do on the network: which wire protocol, which port,
    /// and the maximum bytes read per request. All three are mandatory.
    /// Protocol is drawn from a closed set (raw_tcp only in the first
    /// slice); unknown names are rejected at parse time, not at verify time.
    fn parse_service(&mut self) -> Result<Service, ParseError> {
        self.expect_ident("service")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut protocol = None;
        let mut port = None;
        let mut max_request = None;
        let mut handler = None;
        // Phase 8 slice 8e: zero or more `log:` blocks, each with its own
        // append_file effect AND its own on_error policy. The parser pushes
        // into this Vec for every `log:` block encountered, instead of
        // overwriting a single Option as slice 8a did.
        let mut logs: Vec<LogBlock> = Vec::new();
        // Phase 10 slice 10: optional `concurrency:` knob. Default is
        // Sequential — preserves the slice 9 binary byte-for-byte when
        // the line is omitted.
        let mut concurrency: ConcurrencyMode = ConcurrencyMode::Sequential;
        // Mutable state fields persisting across requests (Number-only in slice 1).
        let mut state_fields: Vec<StateField> = Vec::new();
        // Post-response mutation block.
        let mut after_sets: Vec<StateSet> = Vec::new();

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
                            "unknown attribute '@{}' in service",
                            other
                        )));
                    }
                }
            } else if self.check_ident("listen") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                self.expect_kind(TokenKind::Newline)?;
                self.expect_kind(TokenKind::Indent)?;
                while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
                    let key = self.expect_ident_any()?;
                    self.expect_kind(TokenKind::Colon)?;
                    match key.as_str() {
                        "protocol" => {
                            let n = self.expect_ident_any()?;
                            protocol = Some(match n.as_str() {
                                "raw_tcp" => Protocol::RawTcp,
                                "http_1_0" => Protocol::Http10,
                                _ => return Err(self.error(&format!(
                                    "unknown protocol '{}' (allowed: raw_tcp, http_1_0)",
                                    n
                                ))),
                            });
                        }
                        "port" => {
                            let n = self.expect_number()?;
                            if !(1..=65535).contains(&n) {
                                return Err(self.error(&format!(
                                    "port {} out of range [1, 65535]",
                                    n
                                )));
                            }
                            port = Some(n as u16);
                        }
                        "max_request" => {
                            let n = self.expect_number()?;
                            if n <= 0 {
                                return Err(self.error(&format!(
                                    "max_request must be positive, got {}",
                                    n
                                )));
                            }
                            if n > u32::MAX as i64 {
                                return Err(self.error(&format!(
                                    "max_request {} exceeds u32 range",
                                    n
                                )));
                            }
                            max_request = Some(n as u32);
                        }
                        _ => {
                            return Err(self.error(&format!(
                                "unknown key '{}' in listen block (allowed: protocol, port, max_request)",
                                key
                            )));
                        }
                    }
                    self.expect_kind(TokenKind::Newline)?;
                }
                self.expect_kind(TokenKind::Dedent)?;
            } else if self.check_ident("handler") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                handler = Some(self.expect_ident_any()?);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("log") {
                // Phase 8 slice 8a/8e: a `log:` block. Only
                // `append_file "path" <expr>` is accepted as the effect; an
                // optional `on_error: drop | abort` follows. Slice 8e
                // permits MULTIPLE `log:` blocks in the same service —
                // each is independent and fires in source order between
                // the handler and the response write.
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                self.expect_kind(TokenKind::Newline)?;
                self.expect_kind(TokenKind::Indent)?;
                let kind_name = self.expect_ident_any()?;
                if kind_name != "append_file" {
                    return Err(self.error(&format!(
                        "Phase 8 slice 8a: only 'append_file' is accepted in service log blocks; got '{}'",
                        kind_name
                    )));
                }
                let path = self.expect_string().map_err(|_| {
                    self.error("append_file requires a string literal path — the auditor must see every file the service can touch")
                })?;
                let content = self.parse_expr()?;
                self.expect_kind(TokenKind::Newline)?;
                let mut on_error = ErrorPolicy::Drop;
                if self.check_ident("on_error") {
                    self.advance();
                    self.expect_kind(TokenKind::Colon)?;
                    let policy_name = self.expect_ident_any()?;
                    on_error = match policy_name.as_str() {
                        "drop" => ErrorPolicy::Drop,
                        "abort" => ErrorPolicy::Abort,
                        other => {
                            return Err(self.error(&format!(
                                "unknown on_error policy '{}' (allowed: drop, abort)",
                                other
                            )));
                        }
                    };
                    self.expect_kind(TokenKind::Newline)?;
                }
                self.expect_kind(TokenKind::Dedent)?;
                logs.push(LogBlock {
                    effect: Effect::AppendFile { path, content },
                    on_error,
                });
            } else if self.check_ident("concurrency") {
                // Phase 10 slice 10: closed-set concurrency knob. Mirrors
                // the on_error parser pattern — single ident on the RHS,
                // unknown values rejected at parse time.
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let mode_name = self.expect_ident_any()?;
                concurrency = match mode_name.as_str() {
                    "sequential" => ConcurrencyMode::Sequential,
                    "forked" => ConcurrencyMode::Forked,
                    other => {
                        return Err(self.error(&format!(
                            "unknown concurrency mode '{}' (allowed: sequential, forked)",
                            other
                        )));
                    }
                };
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("state") {
                // Mutable state block. Each line: `name : number = <literal>`.
                // Number-only in this slice; text state is a follow-up.
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                self.expect_kind(TokenKind::Newline)?;
                self.expect_kind(TokenKind::Indent)?;
                while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
                    let fname = self.expect_ident_any()?;
                    self.expect_kind(TokenKind::Colon)?;
                    let ty = self.parse_type()?;
                    if ty != Type::Number {
                        return Err(self.error(&format!(
                            "state field '{}': only 'number' type is supported in this slice; got {:?}",
                            fname, ty
                        )));
                    }
                    // Expect `= <literal>`
                    self.expect_kind(TokenKind::Equal)?;
                    let initial_value = self.parse_signed_number()?;
                    state_fields.push(StateField {
                        name: fname,
                        ty,
                        initial_value,
                    });
                    self.expect_kind(TokenKind::Newline)?;
                }
                self.expect_kind(TokenKind::Dedent)?;
            } else if self.check_ident("after") {
                // Post-response mutation block. Each line: `set <name> = <expr>`.
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                self.expect_kind(TokenKind::Newline)?;
                self.expect_kind(TokenKind::Indent)?;
                while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
                    if !self.check_ident("set") {
                        return Err(self.error("expected 'set <field> = <expr>' in after block"));
                    }
                    self.advance();
                    let field_name = self.expect_ident_any()?;
                    self.expect_kind(TokenKind::Equal)?;
                    let value = self.parse_expr()?;
                    after_sets.push(StateSet { field_name, value });
                    self.expect_kind(TokenKind::Newline)?;
                }
                self.expect_kind(TokenKind::Dedent)?;
            } else {
                return Err(self.error("expected attribute, 'listen:', 'handler:', 'log:', 'concurrency:', 'state:', or 'after:' in service"));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        // Validate: after_sets non-empty requires state_fields non-empty.
        if !after_sets.is_empty() && state_fields.is_empty() {
            return Err(self.error("'after:' block requires a 'state:' block declaring the fields it mutates"));
        }

        Ok(Service {
            name,
            intention: intention.ok_or_else(|| self.error("service missing @intention"))?,
            source: source.ok_or_else(|| self.error("service missing @source"))?,
            protocol: protocol.ok_or_else(|| self.error("service missing 'listen.protocol'"))?,
            port: port.ok_or_else(|| self.error("service missing 'listen.port'"))?,
            max_request: max_request.ok_or_else(|| self.error("service missing 'listen.max_request'"))?,
            handler: handler.ok_or_else(|| self.error("service missing 'handler'"))?,
            logs,
            concurrency,
            state_fields,
            after_sets,
        })
    }

    /// Phase 9 slice 1: parse a top-level `resource` block declaring a
    /// read-only file handle the program can open at runtime.
    ///
    /// Grammar:
    ///   resource <name>
    ///     @intention: "..."
    ///     @source: file.intent:NN
    ///     path: "/literal/path"
    ///     max:  <number>
    ///     on_read_error: abort        (optional; default = abort)
    ///
    /// Path is a string literal (not an expression) so the auditor reads
    /// the source — or `strings` the binary — and sees every file the
    /// program can attempt to open. `max` bounds the per-read stack
    /// buffer; the verifier enforces 1 ≤ max ≤ 64 MiB.
    fn parse_resource(&mut self) -> Result<Resource, ParseError> {
        self.expect_ident("resource")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut path: Option<String> = None;
        let mut max_bytes: Option<u32> = None;
        let mut on_read_error: ErrorPolicy = ErrorPolicy::Abort;
        // Phase 9 slice 9.4: opt-in caching of the resource read (one
        // syscall sequence at server startup vs. one per request). Default
        // is `false` so existing programs stay byte-for-byte identical to
        // the pre-9.4 binary.
        let mut cache: bool = false;

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
                            "unknown attribute '@{}' in resource",
                            other
                        )));
                    }
                }
            } else if self.check_ident("path") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let p = self.expect_string().map_err(|_| {
                    self.error("resource path must be a string literal — the auditor reads the binary and sees every file the program can touch")
                })?;
                path = Some(p);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("max") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let n = self.expect_number()?;
                if n <= 0 {
                    return Err(self.error(&format!(
                        "resource max must be positive, got {}",
                        n
                    )));
                }
                if n > u32::MAX as i64 {
                    return Err(self.error(&format!(
                        "resource max {} exceeds u32 range",
                        n
                    )));
                }
                max_bytes = Some(n as u32);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("on_read_error") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let policy_name = self.expect_ident_any()?;
                on_read_error = match policy_name.as_str() {
                    "abort" => ErrorPolicy::Abort,
                    "drop" => {
                        return Err(self.error(
                            "resource on_read_error: 'drop' lands in a later slice; only 'abort' is accepted today",
                        ));
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown on_read_error policy '{}' (allowed: abort)",
                            other
                        )));
                    }
                };
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("cache") {
                // Phase 9 slice 9.4: closed-set bool. Mirrors the
                // on_read_error / concurrency parser pattern — single
                // ident on the RHS, unknown values rejected at parse
                // time. `drop` is already refused above so the rule
                // "cache: true requires abort" is structurally guaranteed.
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let value_name = self.expect_ident_any()?;
                cache = match value_name.as_str() {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(self.error(&format!(
                            "cache must be 'true' or 'false', got '{}'",
                            other
                        )));
                    }
                };
                self.expect_kind(TokenKind::Newline)?;
            } else {
                return Err(self.error("expected attribute, 'path:', 'max:', 'on_read_error:', or 'cache:' in resource"));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Resource {
            name,
            intention: intention.ok_or_else(|| self.error("resource missing @intention"))?,
            source: source.ok_or_else(|| self.error("resource missing @source"))?,
            path: path.ok_or_else(|| self.error("resource missing 'path:'"))?,
            max_bytes: max_bytes.ok_or_else(|| self.error("resource missing 'max:'"))?,
            on_read_error,
            cache,
        })
    }

    /// Phase 11 slice 1: parse a top-level `connection` block declaring an
    /// outbound TCP endpoint the program can dial at runtime.
    ///
    /// Grammar:
    ///   connection <name>
    ///     @intention: "..."
    ///     @source: file.intent:NN
    ///     host: "X.X.X.X"               (IPv4 dotted quad — no DNS)
    ///     port: <number>                 (1..=65535)
    ///     max_response: <number>         (1..=64 MiB; stack buffer bound)
    ///     on_connect_error: abort        (optional; default = abort)
    ///
    /// `host` is a string literal (not an expression) and is parsed here
    /// as four dot-separated octets so the auditor reads the source — or
    /// `strings` the binary — and sees every IP the program can attempt
    /// to dial. `max_response` bounds the per-fetch stack buffer; the
    /// verifier enforces 1 ≤ max_response ≤ 64 MiB.
    fn parse_connection(&mut self) -> Result<Connection, ParseError> {
        self.expect_ident("connection")?;
        let name = self.expect_ident_any()?;
        self.expect_kind(TokenKind::Newline)?;
        self.expect_kind(TokenKind::Indent)?;

        let mut intention = None;
        let mut source = None;
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut max_response: Option<u32> = None;
        let mut on_connect_error: ErrorPolicy = ErrorPolicy::Abort;

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
                            "unknown attribute '@{}' in connection",
                            other
                        )));
                    }
                }
            } else if self.check_ident("host") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let h = self.expect_string().map_err(|_| {
                    self.error("connection host must be a string literal containing an IPv4 dotted quad (e.g. \"127.0.0.1\")")
                })?;
                // Validate IPv4 dotted quad: four 0..=255 octets separated
                // by dots. Reject DNS names, IPv6, "localhost".
                let octets: Vec<&str> = h.split('.').collect();
                if octets.len() != 4 {
                    return Err(self.error(&format!(
                        "connection host '{}' is not an IPv4 dotted quad (expected four 0..=255 octets separated by dots; DNS names and IPv6 are not supported in slice 1)",
                        h
                    )));
                }
                for oct in &octets {
                    if oct.is_empty() {
                        return Err(self.error(&format!(
                            "connection host '{}' has an empty octet",
                            h
                        )));
                    }
                    let n: u32 = oct.parse().map_err(|_| {
                        self.error(&format!(
                            "connection host '{}' octet '{}' is not a number",
                            h, oct
                        ))
                    })?;
                    if n > 255 {
                        return Err(self.error(&format!(
                            "connection host '{}' octet '{}' exceeds 255",
                            h, oct
                        )));
                    }
                }
                host = Some(h);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("port") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let n = self.expect_number()?;
                if n < 1 || n > 65535 {
                    return Err(self.error(&format!(
                        "connection port {} out of range (must be 1..=65535)",
                        n
                    )));
                }
                port = Some(n as u16);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("max_response") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let n = self.expect_number()?;
                if n <= 0 {
                    return Err(self.error(&format!(
                        "connection max_response must be positive, got {}",
                        n
                    )));
                }
                if n > u32::MAX as i64 {
                    return Err(self.error(&format!(
                        "connection max_response {} exceeds u32 range",
                        n
                    )));
                }
                max_response = Some(n as u32);
                self.expect_kind(TokenKind::Newline)?;
            } else if self.check_ident("on_connect_error") {
                self.advance();
                self.expect_kind(TokenKind::Colon)?;
                let policy_name = self.expect_ident_any()?;
                on_connect_error = match policy_name.as_str() {
                    "abort" => ErrorPolicy::Abort,
                    "drop" => {
                        return Err(self.error(
                            "connection on_connect_error: 'drop' lands in a later slice; only 'abort' is accepted today",
                        ));
                    }
                    other => {
                        return Err(self.error(&format!(
                            "unknown on_connect_error policy '{}' (allowed: abort)",
                            other
                        )));
                    }
                };
                self.expect_kind(TokenKind::Newline)?;
            } else {
                return Err(self.error("expected attribute, 'host:', 'port:', 'max_response:', or 'on_connect_error:' in connection"));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Connection {
            name,
            intention: intention.ok_or_else(|| self.error("connection missing @intention"))?,
            source: source.ok_or_else(|| self.error("connection missing @source"))?,
            host: host.ok_or_else(|| self.error("connection missing 'host:'"))?,
            port: port.ok_or_else(|| self.error("connection missing 'port:'"))?,
            max_response: max_response.ok_or_else(|| self.error("connection missing 'max_response:'"))?,
            on_connect_error,
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
                    // overflow : [min, max] — bounds are the declaration,
                    // mechanically verified against interval arithmetic.
                    self.expect_kind(TokenKind::LBracket)?;
                    let min = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::Comma)?;
                    let max = self.parse_signed_number()?;
                    self.expect_kind(TokenKind::RBracket)?;
                    overflow = Some(OverflowHint { min, max });
                }
                _ => {
                    // Trust-based hints (vectorizable, parallel, cache_result) must
                    // carry a string justification — the reason the AI claims the hint
                    // applies. Absence of the hint means "not claimed"; presence always
                    // includes the rationale the auditor can read.
                    let reason = self.expect_string().map_err(|_| {
                        self.error(&format!(
                            "hint '{}' requires a string justification, e.g. '{}: \"why this applies\"'",
                            key, key
                        ))
                    })?;
                    if reason.trim().is_empty() {
                        return Err(self.error(&format!(
                            "hint '{}' justification must not be empty — state why the hint applies",
                            key
                        )));
                    }
                    match key.as_str() {
                        "vectorizable" => vectorizable = Some(reason),
                        "parallel" => parallel = Some(reason),
                        "cache_result" => cache_result = Some(reason),
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

    /// Consume any NEWLINE tokens at the cursor. Used to relax
    /// line-sensitivity in expressions that legitimately span lines
    /// (today: `if/then/else`). INDENT/DEDENT are deliberately NOT
    /// consumed — they still mark the surrounding block boundaries.
    fn skip_newlines(&mut self) {
        while matches!(self.peek_kind(), Some(TokenKind::Newline)) {
            self.advance();
        }
    }

    /// Skip NEWLINE / INDENT / DEDENT inside an `if/then/else`
    /// expression, tracking how many INDENT tokens we've consumed so
    /// the parent caller can balance them with matching DEDENTs after
    /// the else-expression. INDENT increments `depth`; DEDENT
    /// decrements it (and is consumed) only if `depth > 0`, otherwise
    /// the DEDENT is left in place for the surrounding block to see.
    /// This is the discipline that lets the indented continuation form
    ///   status = if cond
    ///            then "a"
    ///            else "b"
    /// parse without swallowing the DEDENT that closes the enclosing
    /// `logic:` (or other) block.
    fn skip_if_separators(&mut self, depth: &mut u32) {
        loop {
            match self.peek_kind() {
                Some(TokenKind::Newline) => self.advance(),
                Some(TokenKind::Indent) => { *depth += 1; self.advance(); }
                Some(TokenKind::Dedent) if *depth > 0 => { *depth -= 1; self.advance(); }
                _ => break,
            }
        }
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
      calls   : []
    termination:
      bound : 1
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
                assert_eq!(r.proofs.purity.reads.len(), 1);
                assert_eq!(r.proofs.purity.reads[0].segments, vec!["i", "amount"]);
                assert_eq!(r.proofs.termination.bound, Some(1));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn if_then_else_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = if t.x > 10 then 1 else 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound : 3\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::If(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    /// `if/then/else` allows NEWLINE between the structural keywords.
    /// Surfaced by Opus 4.7 generating a multi-line if for the
    /// `flight_status` hold-out intent — the model produced
    ///   r = if cond
    ///       then concat(...)
    ///       else concat(...)
    /// with `then` on the next line at the same indent column. The
    /// parser previously rejected with `expected 'then', got NEWLINE`.
    /// Same-column continuation now parses; indented continuation
    /// (which would require INDENT/DEDENT swallowing inside the
    /// expression) stays refused — that's its own slice.
    #[test]
    fn if_then_else_allows_newlines_at_same_indent() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = if t.x > 10\n    then 1\n    else 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound : 3\n";
        let p = parse(src).unwrap_or_else(|e| panic!("multi-line if must parse: {:?}", e));
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::If(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    /// `if/then/else` allows the genuinely-INDENTED continuation form:
    ///   r = if cond
    ///       then ...     <- column deeper than `if`
    ///       else ...     <- same deeper column
    /// The parser tracks an indent depth across the if-expression and
    /// consumes a balancing DEDENT for each INDENT it ate. The
    /// surrounding block's DEDENT (one less indent level out) is left
    /// untouched. `parse_logic_block`'s statement terminator was made
    /// lenient about NEWLINE-before-DEDENT to accommodate the case
    /// where the balance loop consumed the trailing NEWLINE.
    ///
    /// Pinned because Opus 4.7 gravitates to this form when generating
    /// long `if` expressions for the holdout intents — `examples/holdout/
    /// flight_status` triggered the prior rejection.
    #[test]
    fn if_then_else_allows_indented_continuation() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = if t.x > 10\n        then 1\n        else 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound : 3\n";
        let p = parse(src).unwrap_or_else(|e| panic!("indented multi-line if must parse: {:?}", e));
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::If(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn let_bindings_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    let y = t.x * 2\n    r = y + 1\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 2\n";
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
    fn text_max_len_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    name : text [..64]\n    bio  : text [..1024]\n    plain : text\n";
        let p = parse(src).unwrap();
        match &p.items[0] {
            Item::Concept(c) => {
                assert_eq!(c.fields[0].ty, Type::Text);
                assert_eq!(c.fields[0].range, Some((0, 64)));
                assert_eq!(c.fields[1].ty, Type::Text);
                assert_eq!(c.fields[1].range, Some((0, 1024)));
                assert_eq!(c.fields[2].ty, Type::Text);
                assert_eq!(c.fields[2].range, None);
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
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    xs : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = all(t.xs, x => x > 0)\n  proofs:\n    purity:\n      reads: [t.xs]\n      calls: []\n    termination:\n      bound: 2\n";
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
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n  hints:\n    vectorizable: \"no cross-element dependency in the predicate\"\n    overflow: [0, 1]\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                let h = r.hints.as_ref().unwrap();
                assert_eq!(
                    h.vectorizable.as_deref(),
                    Some("no cross-element dependency in the predicate")
                );
                assert!(h.parallel.is_none());
                assert!(h.overflow.is_some());
                let ov = h.overflow.as_ref().unwrap();
                assert_eq!(ov.min, 0);
                assert_eq!(ov.max, 1);
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn bare_hint_rejected() {
        // The identity of the rule: a hint without justification is refused,
        // because the auditor has no "why" to read.
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n  hints:\n    vectorizable: yes\n";
        let err = parse(src).err().expect("bare hint should be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("justification") || msg.contains("string"),
            "expected error about missing justification, got: {}",
            msg
        );
    }

    #[test]
    fn empty_hint_justification_rejected() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n  hints:\n    vectorizable: \"\"\n";
        let err = parse(src).err().expect("empty justification should be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("empty"),
            "expected error about empty justification, got: {}",
            msg
        );
    }

    #[test]
    fn precedence_correct() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    a : number\n    b : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.a + 1 > t.b * 2 and t.a < 100\n  proofs:\n    purity:\n      reads: [t.a, t.b]\n      calls: []\n    termination:\n      bound: 5\n";
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
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = not (t.x > 0)\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 2\n";
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
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule r\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    y : bool\n  logic:\n    y = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n\nreaction notify\n  @intention: \"notify when triggered\"\n  @source: f.intent:1\n  trigger: r\n  effects:\n    print \"hello\"\n";
        let p = parse(src).unwrap();
        assert_eq!(p.items.len(), 3); // concept + rule + reaction
        match &p.items[2] {
            Item::Reaction(rx) => {
                assert_eq!(rx.name, "notify");
                assert_eq!(rx.trigger, "r");
                assert_eq!(rx.effects.len(), 1);
                assert!(matches!(rx.effects[0], Effect::Print(_)));
            }
            _ => panic!("expected reaction"),
        }
    }

    #[test]
    fn fold_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    items : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = fold(t.items, 0, acc, x => acc + 1)\n  proofs:\n    purity:\n      reads: [t.items]\n      calls: []\n    termination:\n      bound: 2\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Fold(_, _, _, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn map_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    items : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : collection(number)\n  logic:\n    r = map(t.items, x => x + 1)\n  proofs:\n    purity:\n      reads: [t.items]\n      calls: []\n    termination:\n      bound: 2\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Map(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn result_type_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule try_div\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : Result(number, text)\n  logic:\n    r = if t.x > 0 then Ok(t.x) else Err(\"negative\")\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 3\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                // Output type is Result(number, text)
                match &r.output_ty {
                    Type::Result(t, e) => {
                        assert_eq!(**t, Type::Number);
                        assert_eq!(**e, Type::Text);
                    }
                    other => panic!("expected Result type, got {:?}", other),
                }
                // Logic uses Ok/Err under an if/else
                assert!(matches!(&r.logic.value, Expr::If(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn append_file_with_literal_path_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule trig\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    b : bool\n  logic:\n    b = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n\nreaction log\n  @intention: \"log\"\n  @source: f.intent:1\n  trigger: trig\n  effects:\n    append_file \"/tmp/x.log\" \"hi\\n\"\n";
        let p = parse(src).unwrap();
        match &p.items[2] {
            Item::Reaction(rx) => match &rx.effects[0] {
                Effect::AppendFile { path, .. } => assert_eq!(path, "/tmp/x.log"),
                other => panic!("expected AppendFile, got {:?}", other),
            },
            _ => panic!("expected reaction"),
        }
    }

    #[test]
    fn append_file_with_non_literal_path_rejected() {
        // The path MUST be a string literal at the source level. Using a
        // field or a concat() expression is refused so the auditor can see
        // every file this program could ever touch.
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule trig\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    b : bool\n  logic:\n    b = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n\nreaction log\n  @intention: \"log\"\n  @source: f.intent:1\n  trigger: trig\n  effects:\n    append_file t.x \"hi\\n\"\n";
        let err = parse(src).err().expect("non-literal path should be rejected");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("string literal") || msg.contains("dynamic"),
            "expected error about literal path, got {}",
            msg
        );
    }

    #[test]
    fn concat_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule make\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    i : T\n  output:\n    r : text\n  logic:\n    r = concat(\"age \", i.x, \" years\")\n  proofs:\n    purity:\n      reads: [i.x]\n      calls: []\n    termination:\n      bound: 4\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => match &r.logic.value {
                Expr::Concat(args) => {
                    assert_eq!(args.len(), 3);
                    assert!(matches!(&args[0], Expr::Text(s) if s == "age "));
                }
                other => panic!("expected Concat, got {:?}", other),
            },
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn concat_empty_rejected() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule make\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    i : T\n  output:\n    r : text\n  logic:\n    r = concat()\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n";
        assert!(parse(src).is_err(), "empty concat should be rejected");
    }

    #[test]
    fn record_constructor_parsed() {
        let src = "@verbose 0.1.0\n\nconcept Pair\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    a : number\n    b : number\n\nconcept In\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule make\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    i : In\n  output:\n    p : Pair\n  logic:\n    p = Pair { a: i.x, b: i.x + 1 }\n  proofs:\n    purity:\n      reads: [i.x]\n      calls: []\n    termination:\n      bound: 3\n";
        let p = parse(src).unwrap();
        match &p.items[2] {
            Item::Rule(r) => match &r.logic.value {
                Expr::Record(name, fields) => {
                    assert_eq!(name, "Pair");
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].0, "a");
                    assert_eq!(fields[1].0, "b");
                }
                other => panic!("expected Record, got {:?}", other),
            },
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn match_result_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule consume\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : Result(number, text)\n  logic:\n    r = match_result(Ok(t.x), v => Ok(v + 1), e => Err(e))\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 10\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(
                    &r.logic.value,
                    Expr::MatchResult(_, _, _, _, _)
                ));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn filter_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    items : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : collection(X)\n  logic:\n    r = filter(t.items, x => x > 0)\n  proofs:\n    purity:\n      reads: [t.items]\n      calls: []\n    termination:\n      bound: 2\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Filter(_, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn sum_desugars_to_fold() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    items : collection(X)\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = sum(t.items, x => x)\n  proofs:\n    purity:\n      reads: [t.items]\n      calls: []\n    termination:\n      bound: 2\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                // sum desugars to fold
                assert!(matches!(&r.logic.value, Expr::Fold(_, _, _, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    #[test]
    fn string_comparison_parsed() {
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    s : text\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.s == \"active\"\n  proofs:\n    purity:\n      reads: [t.s]\n      calls: []\n    termination:\n      bound: 1\n";
        let p = parse(src).unwrap();
        match &p.items[1] {
            Item::Rule(r) => {
                assert!(matches!(&r.logic.value, Expr::Binary(BinOp::Eq, _, _)));
            }
            _ => panic!("expected rule"),
        }
    }

    // ─── Phase 7: service construct parser tests ─────────────────────────

    fn service_src(listen_body: &str, handler_line: &str) -> String {
        format!(
            "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule h\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : bool\n  logic:\n    r = t.x > 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"a test service\"\n  @source: f.intent:1\n  listen:\n{}  {}\n",
            listen_body, handler_line
        )
    }

    #[test]
    fn service_parsed_happy_path() {
        let src = service_src(
            "    protocol: raw_tcp\n    port: 9999\n    max_request: 4096\n",
            "handler: h",
        );
        let p = parse(&src).unwrap();
        match &p.items[2] {
            Item::Service(s) => {
                assert_eq!(s.name, "s");
                assert_eq!(s.protocol, Protocol::RawTcp);
                assert_eq!(s.port, 9999);
                assert_eq!(s.max_request, 4096);
                assert_eq!(s.handler, "h");
            }
            _ => panic!("expected service as third item"),
        }
    }

    #[test]
    fn service_rejects_unknown_protocol() {
        let src = service_src(
            "    protocol: quic\n    port: 443\n    max_request: 1024\n",
            "handler: h",
        );
        let err = parse(&src).err().expect("unknown protocol should be rejected at parse time");
        assert!(
            format!("{:?}", err).contains("unknown protocol"),
            "expected 'unknown protocol' error, got {:?}",
            err
        );
    }

    #[test]
    fn service_rejects_out_of_range_port() {
        let src = service_src(
            "    protocol: raw_tcp\n    port: 70000\n    max_request: 1024\n",
            "handler: h",
        );
        let err = parse(&src).err().expect("port above 65535 should be rejected");
        assert!(
            format!("{:?}", err).contains("out of range"),
            "expected port range error, got {:?}",
            err
        );
    }

    #[test]
    fn service_rejects_non_positive_max_request() {
        let src = service_src(
            "    protocol: raw_tcp\n    port: 9999\n    max_request: 0\n",
            "handler: h",
        );
        let err = parse(&src).err().expect("max_request=0 should be rejected");
        assert!(
            format!("{:?}", err).contains("max_request must be positive"),
            "expected max_request positivity error, got {:?}",
            err
        );
    }

    #[test]
    fn service_rejects_missing_listen_key() {
        // listen block without port
        let src = service_src(
            "    protocol: raw_tcp\n    max_request: 1024\n",
            "handler: h",
        );
        let err = parse(&src).err().expect("missing port should be rejected");
        assert!(
            format!("{:?}", err).contains("listen.port"),
            "expected listen.port missing error, got {:?}",
            err
        );
    }

    /// Phase A slice 1 — sum-type concept declarations.
    ///
    /// A `concept Foo variants: VarA of (...) | VarB | ...` parses and
    /// verifies. The variant declarations populate `concept.variants`;
    /// `concept.fields` stays empty (mutually exclusive with variants).
    /// Construction and pattern match are later slices (A.2 / A.3).
    ///
    /// Pinned cases:
    ///   (a) Multiple variants with various payload shapes
    ///   (b) No-payload variant (`Eof` form, no `of (...)`)
    ///   (c) Rejection: concept with both `fields:` and `variants:`
    ///   (d) Rejection: concept with neither
    ///   (e) Rejection: duplicate variant name within one concept
    ///   (f) Rejection: duplicate payload field name within one variant
    #[test]
    fn phase_a1_sum_type_concept_declaration() {
        // (a) + (b): happy path, multiple variants including no-payload
        let src = r#"@verbose 0.1.0

concept TokenKind
  @intention: "x"
  @source: invoices.intent:1
  variants:
    Ident of (name : text)
    Int of (value : number)
    Op of (sym : text)
    Eof
"#;
        let program = parse(src).expect("variants declaration should parse");
        let concept = program.items.iter().find_map(|i| match i {
            Item::Concept(c) if c.name == "TokenKind" => Some(c),
            _ => None,
        }).expect("TokenKind concept");
        assert!(concept.fields.is_empty(), "sum-type concept has no top-level fields");
        assert_eq!(concept.variants.len(), 4);
        assert_eq!(concept.variants[0].name, "Ident");
        assert_eq!(concept.variants[0].fields.len(), 1);
        assert_eq!(concept.variants[0].fields[0].name, "name");
        assert!(matches!(concept.variants[0].fields[0].ty, Type::Text));
        assert_eq!(concept.variants[1].name, "Int");
        assert_eq!(concept.variants[1].fields[0].name, "value");
        assert!(matches!(concept.variants[1].fields[0].ty, Type::Number));
        assert_eq!(concept.variants[3].name, "Eof");
        assert!(concept.variants[3].fields.is_empty(),
            "no-payload variant has zero fields");

        // (c) Both fields and variants → reject
        let both = r#"@verbose 0.1.0

concept Bad
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number
  variants:
    Foo
"#;
        let err = parse(both).err().expect("both blocks must be rejected");
        assert!(
            format!("{:?}", err).contains("both 'fields:' and 'variants:'"),
            "error should name the conflict: {:?}",
            err
        );

        // (d) Neither → reject
        let neither = r#"@verbose 0.1.0

concept Empty
  @intention: "x"
  @source: invoices.intent:1
"#;
        let err = parse(neither).err().expect("neither block must be rejected");
        assert!(
            format!("{:?}", err).contains("missing 'fields:' or 'variants:'"),
            "error should ask for one or the other: {:?}",
            err
        );

        // (e) Duplicate variant name → reject
        let dup_variant = r#"@verbose 0.1.0

concept Dup
  @intention: "x"
  @source: invoices.intent:1
  variants:
    A
    A
"#;
        let err = parse(dup_variant).err().expect("duplicate variant must be rejected");
        assert!(
            format!("{:?}", err).contains("duplicate variant name"),
            "error should name the duplication: {:?}",
            err
        );

        // (f) Duplicate payload field → reject
        let dup_field = r#"@verbose 0.1.0

concept Dup
  @intention: "x"
  @source: invoices.intent:1
  variants:
    Foo of (x : number, x : text)
"#;
        let err = parse(dup_field).err().expect("duplicate payload field must be rejected");
        assert!(
            format!("{:?}", err).contains("duplicate payload field"),
            "error should name the duplication: {:?}",
            err
        );
    }

    // ─── Mutable state: parser tests ─────────────────────────────────────

    #[test]
    fn service_state_block_parsed() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: f.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "ok" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: f.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    counter : number = 0
    total   : number = 100
  after:
    set counter = state.counter + 1
"#;
        let p = parse(src).unwrap();
        let svc = p.items.iter().find_map(|i| match i {
            Item::Service(s) => Some(s),
            _ => None,
        }).expect("expected a service item");
        assert_eq!(svc.state_fields.len(), 2);
        assert_eq!(svc.state_fields[0].name, "counter");
        assert_eq!(svc.state_fields[0].initial_value, 0);
        assert_eq!(svc.state_fields[1].name, "total");
        assert_eq!(svc.state_fields[1].initial_value, 100);
        assert_eq!(svc.after_sets.len(), 1);
        assert_eq!(svc.after_sets[0].field_name, "counter");
    }

    #[test]
    fn service_state_rejects_text_type() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: f.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "ok" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: f.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    name : text = 0
"#;
        let err = parse(src).err().expect("text state should be rejected");
        assert!(
            format!("{:?}", err).contains("only 'number' type"),
            "expected type rejection, got {:?}",
            err
        );
    }

    #[test]
    fn service_after_without_state_rejected() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: f.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "ok" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: f.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  after:
    set counter = 1
"#;
        let err = parse(src).err().expect("after without state should be rejected");
        assert!(
            format!("{:?}", err).contains("requires a 'state:' block"),
            "expected state-required error, got {:?}",
            err
        );
    }

    #[test]
    fn service_empty_state_and_after_backward_compat() {
        // Existing services without state:/after: must parse identically.
        let src = service_src(
            "    protocol: raw_tcp\n    port: 9999\n    max_request: 4096\n",
            "handler: h",
        );
        let p = parse(&src).unwrap();
        match &p.items[2] {
            Item::Service(s) => {
                assert!(s.state_fields.is_empty());
                assert!(s.after_sets.is_empty());
            }
            _ => panic!("expected service"),
        }
    }
}
