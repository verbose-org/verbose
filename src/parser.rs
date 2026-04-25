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
            } else if self.check_ident("service") {
                items.push(Item::Service(self.parse_service()?));
            } else if self.check_ident("resource") {
                items.push(Item::Resource(self.parse_resource()?));
            } else {
                return Err(self.error("expected 'concept', 'rule', 'reaction', 'service', or 'resource' at top level"));
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
            if (name == "sum" || name == "count" || name == "min" || name == "max") && self.check_kind(&TokenKind::LParen) {
                // Sugar: sum(coll, var => expr) → fold(coll, 0, __acc, var => __acc + expr)
                //        count(coll, var => pred) → fold(coll, 0, __acc, var => __acc + if pred then 1 else 0)
                self.advance(); // (
                let collection = self.parse_expr()?;
                self.expect_kind(TokenKind::Comma)?;
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

        while !self.check_kind(&TokenKind::Dedent) && !self.at_eof() {
            let key = self.expect_ident_any()?;
            self.expect_kind(TokenKind::Colon)?;
            match key.as_str() {
                "bound" => {
                    bound = Some(self.expect_number()?);
                }
                _ => {
                    return Err(self.error(&format!(
                        "unknown key '{}' in termination block (allowed: bound)",
                        key
                    )));
                }
            }
            self.expect_kind(TokenKind::Newline)?;
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Termination { bound })
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
        let mut log: Option<Effect> = None;
        let mut log_on_error: ErrorPolicy = ErrorPolicy::Drop;
        // Phase 10 slice 10: optional `concurrency:` knob. Default is
        // Sequential — preserves the slice 9 binary byte-for-byte when
        // the line is omitted.
        let mut concurrency: ConcurrencyMode = ConcurrencyMode::Sequential;

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
                // Phase 8 slice 8a: optional single-effect log block. Only
                // `append_file "path" <expr>` is accepted; multi-effect and
                // other effect variants land in later slices.
                //
                // Phase 8 slice 8d: an optional `on_error: drop | abort`
                // line may follow the append_file line, controlling what
                // happens when a log syscall fails. Drop is the default
                // (matches slice 8a behaviour).
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
                log = Some(Effect::AppendFile { path, content });
                // Optional on_error line.
                if self.check_ident("on_error") {
                    self.advance();
                    self.expect_kind(TokenKind::Colon)?;
                    let policy_name = self.expect_ident_any()?;
                    log_on_error = match policy_name.as_str() {
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
            } else {
                return Err(self.error("expected attribute, 'listen:', 'handler:', 'log:', or 'concurrency:' in service"));
            }
        }
        self.expect_kind(TokenKind::Dedent)?;

        Ok(Service {
            name,
            intention: intention.ok_or_else(|| self.error("service missing @intention"))?,
            source: source.ok_or_else(|| self.error("service missing @source"))?,
            protocol: protocol.ok_or_else(|| self.error("service missing 'listen.protocol'"))?,
            port: port.ok_or_else(|| self.error("service missing 'listen.port'"))?,
            max_request: max_request.ok_or_else(|| self.error("service missing 'listen.max_request'"))?,
            handler: handler.ok_or_else(|| self.error("service missing 'handler'"))?,
            log,
            log_on_error,
            concurrency,
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
            } else {
                return Err(self.error("expected attribute, 'path:', 'max:', or 'on_read_error:' in resource"));
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
        let src = "@verbose 0.1.0\n\nconcept T\n  @intention: \"t\"\n  @source: f.intent:1\n  fields:\n    x : number\n\nrule test\n  @intention: \"t\"\n  @source: f.intent:1\n  input:\n    t : T\n  output:\n    r : number\n  logic:\n    r = if t.x > 10 then 1 else 0\n  proofs:\n    purity:\n      reads: [t.x]\n      calls: []\n    termination:\n      bound: 3\n";
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
}
