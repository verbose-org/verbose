use crate::ast::*;
pub fn result_type() -> Type {
    Type::Result(Box::new(Type::Number), Box::new(Type::BoundsError))
}

use crate::verifier::{walk_expr_children, VerifyError};
use std::collections::{BTreeSet, HashMap};

fn any(e: &Expr, predicate: &impl Fn(&Expr) -> bool) -> bool {
    if predicate(e) {
        return true;
    }
    let mut found = false;
    walk_expr_children(e, &mut |c| found |= any(c, predicate));
    found
}
fn rule_any(r: &Rule, p: &impl Fn(&Expr) -> bool) -> bool {
    any(&r.logic.value, p) || r.logic.bindings.iter().any(|(_, e)| any(e, p))
}
fn has_type(t: &Type) -> bool {
    match t {
        Type::BoundsError => true,
        Type::Result(a, b) => has_type(a) || has_type(b),
        Type::Named(n) | Type::Collection(n) => n == "BoundsError",
        _ => false,
    }
}
/// Include callers and their dependencies: every execution participating in the
/// new contract is checked, even if its public output is an ordinary scalar.
pub fn active_rules(p: &Program) -> BTreeSet<String> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let mut active: BTreeSet<_> = rules
        .iter()
        .filter(|r| has_type(&r.output_ty) || rule_any(r, &|e| matches!(e, Expr::TryByteAt(_, _))))
        .map(|r| r.name.clone())
        .collect();
    if active.is_empty() {
        return active;
    }
    loop {
        let before = active.len();
        for r in &rules {
            if rule_any(r, &|e| matches!(e, Expr::Call(n, _) if active.contains(n))) {
                active.insert(r.name.clone());
            }
            if active.contains(&r.name) {
                let mut calls = Vec::new();
                fn collect(e: &Expr, out: &mut Vec<String>) {
                    if let Expr::Call(n, _) = e {
                        out.push(n.clone());
                    }
                    walk_expr_children(e, &mut |c| collect(c, out));
                }
                for (_, e) in &r.logic.bindings {
                    collect(e, &mut calls);
                }
                collect(&r.logic.value, &mut calls);
                active.extend(calls);
            }
        }
        if before == active.len() {
            return active;
        }
    }
}

#[derive(Clone)]
struct Val {
    ty: Type,
    obligation: Option<usize>,
}
#[derive(Clone, Default)]
struct State {
    env: HashMap<String, Val>,
    pending: BTreeSet<usize>,
}
type Paths = Vec<(State, Val)>;
struct Check<'a> {
    rule: &'a Rule,
    program: &'a Program,
    next: usize,
    steps: usize,
}
impl Check<'_> {
    fn value(&mut self, mut s: State, ty: Type) -> (State, Val) {
        let obligation = if ty == result_type() {
            self.next += 1;
            s.pending.insert(self.next);
            Some(self.next)
        } else {
            None
        };
        (s, Val { ty, obligation })
    }
    fn typed(&mut self, e: &Expr, s: State, ty: &Type) -> Result<Vec<State>, String> {
        self.expr(e, s)?
            .into_iter()
            .map(|(s, v)| {
                if &v.ty == ty {
                    Ok(s)
                } else {
                    Err(format!(
                        "expected {:?}, found {:?}; BoundsError is not a scalar",
                        ty, v.ty
                    ))
                }
            })
            .collect()
    }
    fn expr(&mut self, e: &Expr, s: State) -> Result<Paths, String> {
        self.steps += 1;
        if self.steps > 100_000 {
            return Err("obligation analysis exceeded 100000 steps; simplify branching".into());
        }
        let ty = match e {
            Expr::Number(_) => Type::Number,
            Expr::Text(_) => Type::Text,
            Expr::Bytes(_) => Type::Bytes,
            Expr::Ident(n) => {
                if let Some(v) = s.env.get(n).cloned() {
                    return Ok(vec![(s, v)]);
                }
                if n == "true" || n == "false" {
                    Type::Bool
                } else {
                    return Err(format!(
                        "unknown binding '{}' in bounded-result analysis",
                        n
                    ));
                }
            }
            Expr::Field(b, name) => {
                if !matches!(b.as_ref(), Expr::Ident(n) if n == &self.rule.input_name && !s.env.contains_key(n))
                {
                    return Err(
                        "only numeric input fields are supported by the bounded-result contract"
                            .into(),
                    );
                }
                let c = self
                    .program
                    .items
                    .iter()
                    .find_map(|i| match i {
                        Item::Concept(c) if self.rule.input_ty == Type::Named(c.name.clone()) => {
                            Some(c)
                        }
                        _ => None,
                    })
                    .ok_or("unknown input concept")?;
                c.fields
                    .iter()
                    .find(|f| &f.name == name)
                    .ok_or("unknown input field")?
                    .ty
                    .clone()
            }
            Expr::TryByteAt(table, index) => {
                if !matches!(table.as_ref(), Expr::Bytes(_) | Expr::Text(_)) {
                    return Err("try_byte_at requires a literal bytes or text table".into());
                }
                return Ok(self
                    .typed(index, s, &Type::Number)?
                    .into_iter()
                    .map(|s| self.value(s, result_type()))
                    .collect());
            }
            Expr::Ok(v) | Expr::Err(v) => {
                let want = if matches!(e, Expr::Ok(_)) {
                    Type::Number
                } else {
                    Type::BoundsError
                };
                return Ok(self
                    .typed(v, s, &want)?
                    .into_iter()
                    .map(|s| self.value(s, result_type()))
                    .collect());
            }
            Expr::If(c, a, b) => {
                let mut out = Vec::new();
                for s in self.typed(c, s, &Type::Bool)? {
                    out.extend(self.expr(a, s.clone())?);
                    out.extend(self.expr(b, s)?);
                }
                if out.iter().any(|(_, v)| v.ty != out[0].1.ty) {
                    return Err("conditional branches have different types".into());
                }
                return Ok(out);
            }
            Expr::MatchResult(t, ok, a, err, b) => {
                let mut out = Vec::new();
                for (mut s, v) in self.expr(t, s)? {
                    if v.ty != result_type() {
                        return Err("match_result scrutinee must be Result(number, BoundsError) in this contract".into());
                    }
                    s.pending
                        .remove(&v.obligation.ok_or("unknown result obligation")?);
                    for (name, body, ty) in [(ok, a, Type::Number), (err, b, Type::BoundsError)] {
                        let mut branch = s.clone();
                        let old = branch.env.insert(
                            name.clone(),
                            Val {
                                ty,
                                obligation: None,
                            },
                        );
                        for (mut after, v) in self.expr(body, branch)? {
                            if let Some(old) = &old {
                                after.env.insert(name.clone(), old.clone());
                            } else {
                                after.env.remove(name);
                            }
                            out.push((after, v));
                        }
                    }
                }
                if out.iter().any(|(_, v)| v.ty != out[0].1.ty) {
                    return Err("match_result branches have different types".into());
                }
                return Ok(out);
            }
            Expr::Call(n, args) => {
                let callee = self
                    .program
                    .items
                    .iter()
                    .find_map(|i| match i {
                        Item::Rule(r) if &r.name == n => Some(r),
                        _ => None,
                    })
                    .ok_or("unknown callee")?;
                if args.len() != 1
                    || !matches!(&args[0], Expr::Ident(n) if n == &self.rule.input_name && !s.env.contains_key(n))
                    || callee.input_ty != self.rule.input_ty
                {
                    return Err("bounded-result calls require callee(input) with the same numeric input concept".into());
                }
                callee.output_ty.clone()
            }
            Expr::Binary(op, a, b) => {
                let (input, output) = match op {
                    BinOp::And | BinOp::Or => (Type::Bool, Type::Bool),
                    BinOp::Eq
                    | BinOp::NotEq
                    | BinOp::Lt
                    | BinOp::Gt
                    | BinOp::LtEq
                    | BinOp::GtEq => (Type::Number, Type::Bool),
                    _ => (Type::Number, Type::Number),
                };
                let mut out = Vec::new();
                for s in self.typed(a, s, &input)? {
                    // Short circuit operators must discharge obligations even
                    // on the path which never evaluates their right operand.
                    if matches!(op, BinOp::And | BinOp::Or) {
                        out.push(self.value(s.clone(), output.clone()));
                    }
                    for s in self.typed(b, s, &input)? {
                        out.push(self.value(s, output.clone()));
                    }
                }
                return Ok(out);
            }
            Expr::Neg(a) | Expr::Abs(a) | Expr::BitNot(a) | Expr::Not(a) => {
                let want = if matches!(e, Expr::Not(_)) {
                    Type::Bool
                } else {
                    Type::Number
                };
                let out = if matches!(e, Expr::Not(_)) {
                    Type::Bool
                } else {
                    Type::Number
                };
                return Ok(self
                    .typed(a, s, &want)?
                    .into_iter()
                    .map(|s| self.value(s, out.clone()))
                    .collect());
            }
            Expr::Min(a, b)
            | Expr::Max(a, b)
            | Expr::BitAnd(a, b)
            | Expr::BitOr(a, b)
            | Expr::BitXor(a, b)
            | Expr::Shl(a, b)
            | Expr::Shr(a, b) => {
                let mut out = Vec::new();
                for s in self.typed(a, s, &Type::Number)? {
                    for s in self.typed(b, s, &Type::Number)? {
                        out.push(self.value(s, Type::Number));
                    }
                }
                return Ok(out);
            }
            Expr::ByteAt(t, i) => {
                if !matches!(t.as_ref(), Expr::Text(_) | Expr::Bytes(_)) {
                    return Err(
                        "bounded-result index computations support byte_at on literal tables only"
                            .into(),
                    );
                }
                return Ok(self
                    .typed(i, s, &Type::Number)?
                    .into_iter()
                    .map(|s| self.value(s, Type::Number))
                    .collect());
            }
            _ => {
                return Err(format!(
                    "unsupported expression in bounded-result obligation analysis: {}",
                    crate::verifier::describe_expr_kind(e)
                ))
            }
        };
        Ok(vec![self.value(s, ty)])
    }
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    let active = active_rules(p);
    let mut errors = Vec::new();
    let mut report =
        |context: String, message: String| errors.push(VerifyError { context, message });
    let uses_contract = |e: &Expr| {
        any(e, &|e| {
            matches!(e, Expr::TryByteAt(_, _))
                || matches!(e, Expr::Call(n, _) if active.contains(n))
        })
    };
    let effect_uses_contract = |effect: &Effect| match effect {
        Effect::Print(args) => args.iter().any(&uses_contract),
        Effect::AppendFile { content, .. } => uses_contract(content),
    };
    for item in &p.items {
        let concepts: Vec<&Concept> = match item {
            Item::Concept(c) => vec![c],
            Item::ConceptGroup(g) => g.concepts.iter().collect(),
            _ => vec![],
        };
        for c in concepts {
            if c.name == "BoundsError"
                || c.fields
                    .iter()
                    .chain(c.variants.iter().flat_map(|v| &v.fields))
                    .any(|f| has_type(&f.ty))
            {
                report(c.name.clone(), "BoundsError is reserved exclusively for rule Result(number, BoundsError) outputs; aggregate storage is unsupported".into());
            }
        }
        match item {
            Item::Service(s)
                if active.contains(&s.handler)
                    || s.logs.iter().any(|log| effect_uses_contract(&log.effect))
                    || s.after_sets.iter().any(|set| uses_contract(&set.value))
                    || s.state_fields.iter().any(|field| has_type(&field.ty)) =>
            {
                report(
                    s.name.clone(),
                    "bounded results are unsupported in service contexts".into(),
                )
            }
            Item::Reaction(r)
                if active.contains(&r.trigger) || r.effects.iter().any(&effect_uses_contract) =>
            {
                report(
                    r.name.clone(),
                    "bounded results are unsupported with reaction effects".into(),
                )
            }
            Item::Rule(r) => {
                if has_type(&r.input_ty)
                    || r.context_ty.as_ref().map_or(false, has_type)
                    || (has_type(&r.output_ty) && r.output_ty != result_type())
                {
                    report(r.name.clone(), "BoundsError is reserved exclusively for Result(number, BoundsError) outputs".into());
                }
                if !active.contains(&r.name) {
                    continue;
                }
                let check = || -> Result<(), String> {
                    if r.context_ty.is_some() {
                        return Err("bounded results do not support context inputs".into());
                    }
                    let concept = p
                        .items
                        .iter()
                        .find_map(|i| match i {
                            Item::Concept(c) if r.input_ty == Type::Named(c.name.clone()) => {
                                Some(c)
                            }
                            _ => None,
                        })
                        .ok_or("bounded results require a numeric input concept")?;
                    if !concept.variants.is_empty()
                        || concept.fields.iter().any(|f| f.ty != Type::Number)
                    {
                        return Err("bounded results require a numeric input concept".into());
                    }
                    if r.output_ty != result_type()
                        && !matches!(r.output_ty, Type::Number | Type::Bool)
                    {
                        return Err("bounded results cannot be nested or stored in aggregates; supported outputs: number, bool, Result(number, BoundsError)".into());
                    }
                    fn cycle(p: &Program, r: &Rule, stack: &mut BTreeSet<String>) -> bool {
                        if !stack.insert(r.name.clone()) {
                            return true;
                        }
                        let found = rule_any(r, &|e| {
                            match e { Expr::Call(n, _) => p.items.iter().any(|i| matches!(i, Item::Rule(c) if &c.name == n && cycle(p, c, &mut stack.clone()))), _ => false }
                        });
                        stack.remove(&r.name);
                        found
                    }
                    if cycle(p, r, &mut BTreeSet::new()) {
                        return Err(
                            "recursion is unsupported by the bounded-result contract".into()
                        );
                    }
                    let mut c = Check {
                        rule: r,
                        program: p,
                        next: 0,
                        steps: 0,
                    };
                    let mut states = vec![State::default()];
                    for (name, e) in &r.logic.bindings {
                        let mut next = Vec::new();
                        for s in states {
                            for (mut s, v) in c.expr(e, s)? {
                                s.env.insert(name.clone(), v);
                                next.push(s);
                            }
                        }
                        states = next;
                    }
                    for s in states {
                        for (mut s, v) in c.expr(&r.logic.value, s)? {
                            if v.ty != r.output_ty {
                                return Err(format!(
                                    "declared output {:?}, found {:?}",
                                    r.output_ty, v.ty
                                ));
                            }
                            if let Some(id) = v.obligation {
                                s.pending.remove(&id);
                            }
                            if !s.pending.is_empty() {
                                return Err("unhandled bounded result on a reachable path: return it or consume it with match_result (aliases preserve the obligation)".into());
                            }
                        }
                    }
                    Ok(())
                };
                if let Err(message) = check() {
                    report(format!("rule '{}' / bounded result", r.name), message);
                }
            }
            _ => {}
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        interpreter::{self, Value},
        lexer::Lexer,
        parser::Parser,
    };
    use std::path::Path;
    fn parse(s: &str) -> Program {
        Parser::new(Lexer::new(s).tokenize().unwrap())
            .parse_program()
            .unwrap()
    }
    fn fixture() -> Program {
        parse(include_str!("../examples/try_byte_at.verbose"))
    }
    fn expression(s: &str) -> Expr {
        let p = parse(
            &include_str!("../examples/try_byte_at.verbose")
                .replace("try_byte_at(b\"a\\x00\\xff\", i.index)", s),
        );
        if let Item::Rule(r) = &p.items[1] {
            r.logic.value.clone()
        } else {
            unreachable!()
        }
    }
    fn custom(body: &str, output: Type, bindings: &[(&str, &str)]) -> Program {
        let mut p = fixture();
        p.items.truncate(2);
        if let Item::Rule(r) = &mut p.items[1] {
            r.logic.value = expression(body);
            r.output_ty = output;
            r.logic.bindings = bindings
                .iter()
                .map(|(n, e)| (n.to_string(), expression(e)))
                .collect();
        }
        p
    }
    fn eval(p: &Program, name: &str, index: i64) -> Result<Value, interpreter::RuntimeError> {
        let rules: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
            .collect();
        let concepts: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| {
                if let Item::Concept(c) = i {
                    Some(c)
                } else {
                    None
                }
            })
            .collect();
        interpreter::eval_rule(
            rules.iter().find(|r| r.name == name).unwrap(),
            &rules,
            &concepts,
            &[],
            &HashMap::from([("index".into(), Value::Number(index))]),
        )
    }
    fn differential(p: &Program, name: &str, indices: &[i64]) {
        assert!(verify(p).is_empty(), "{:?}", verify(p));
        let path = format!("/tmp/verbose-bounds-test-{}", std::process::id());
        crate::native::compile_native(p, name, &path, false, false).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        crate::native::compile_native(p, name, &path, false, false).unwrap();
        assert_eq!(bytes, std::fs::read(&path).unwrap(), "reproducible bytes");
        for &index in indices {
            let native = std::process::Command::new(&path)
                .arg(index.to_string())
                .output()
                .unwrap();
            let (stdout, stderr, code) = match eval(p, name, index) {
                Ok(Value::Ok(v)) => (format!("{}\n", v), String::new(), 0),
                Ok(Value::Err(v)) => (String::new(), format!("{}\n", v), 1),
                Ok(Value::Bool(v)) => (format!("{}\n", v), String::new(), if v { 0 } else { 1 }),
                Ok(Value::Number(v)) => (format!("{}\n", v), String::new(), 0),
                Err(_) => (String::new(), String::new(), 1),
                other => panic!("unexpected value: {:?}", other),
            };
            assert_eq!(native.status.code(), Some(code), "index={index}");
            assert_eq!(native.stdout, stdout.as_bytes(), "index={index}");
            assert_eq!(native.stderr, stderr.as_bytes(), "index={index}");
        }
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn bounds_reference_examples() {
        let p = fixture();
        assert!(
            crate::verifier::verify_program(&p, Path::new("examples")).is_empty(),
            "{:?}",
            crate::verifier::verify_program(&p, Path::new("examples"))
        );
        for name in ["lookup_byte", "byte_or_zero", "forward_byte"] {
            differential(&p, name, &[i64::MIN, -1, 0, 1, 2, 3, i64::MAX]);
        }
        let (optimized, _) = crate::optimizer::optimize_program(&p);
        differential(&optimized, "forward_byte", &[-1, 0, 2, 3]);
    }
    #[test]
    fn bounds_literal_edges() {
        for table in ["b\"\"", "\"\"", "b\"\\x00\\xff\"", "\"é💡\"", "b\"abc\""] {
            let p = custom(
                &format!("try_byte_at({table}, i.index)"),
                result_type(),
                &[],
            );
            differential(
                &p,
                "lookup_byte",
                &[i64::MIN, -1, 0, 1, 2, 3, 4, 5, 6, i64::MAX],
            );
        }
        let p = custom("try_byte_at(b\"abc\", -1)", result_type(), &[]);
        differential(&p, "lookup_byte", &[0]);
    }
    #[test]
    fn bounds_obligations_and_branch_types() {
        let good = [
            ("r", result_type(), vec![("r","try_byte_at(b\"x\", i.index)")]),
            ("a", result_type(), vec![("r","try_byte_at(b\"x\", i.index)"),("a","r")]),
            ("match_result(a, v => v, e => 0)",Type::Number,vec![("r","try_byte_at(b\"x\", i.index)"),("a","r")]),
            ("if i.index > 0 then match_result(r, v => v, e => 0) else match_result(r, v => v + 1, e => 2)",Type::Number,vec![("r","try_byte_at(b\"x\", i.index)")]),
            ("match_result(r, v => Ok(v), e => Err(e))",result_type(),vec![("r","if i.index > 0 then try_byte_at(b\"a\", 0) else try_byte_at(b\"b\", 0)")]),
            ("match_result(r, r => r + 1, r => 0)",Type::Number,vec![("r","try_byte_at(b\"x\", i.index)")]),
            ("match_result(r, v => v, e => 0) + match_result(s, v => v, e => 1)",Type::Number,vec![("r","try_byte_at(b\"ab\", i.index)"),("s","try_byte_at(b\"yz\", i.index + 1)")]),
        ];
        for (body, ty, lets) in good {
            differential(&custom(body, ty, &lets), "lookup_byte", &[-1, 0, 1, 2]);
        }
        let bad = [
            ("0",vec![("r","try_byte_at(b\"x\", i.index)")]),
            ("0",vec![("r","try_byte_at(b\"x\", i.index)"),("alias","r")]),
            ("if i.index > 0 then match_result(r, v => v, e => 0) else 0",vec![("r","try_byte_at(b\"x\", i.index)")]),
            ("r",vec![("r","try_byte_at(b\"x\", i.index)"),("r","0")]),
            ("match_result(try_byte_at(b\"x\", 0), v => v, e => e + 1)",vec![]),
            ("match_result(1, v => v, e => 0) + match_result(try_byte_at(b\"x\", 0), v => v, e => 0)",vec![]),
            ("match_result(try_byte_at(b\"x\", 0), v => v, e => length(e))",vec![]),
            ("match_result(try_byte_at(table, 0), v => v, e => 0)",vec![("table","b\"x\"")]),
            ("match_result(try_byte_at(b\"x\", 0), v => v, e => 0)",vec![("r","try_byte_at(b\"x\", 0)")]),
        ];
        for (body, lets) in bad {
            let p = custom(body, Type::Number, &lets);
            assert!(!verify(&p).is_empty(), "accepted {body}");
        }
    }
    #[test]
    fn bounds_index_failure_is_internal() {
        let p = custom(
            "match_result(try_byte_at(b\"x\", byte_at(b\"\", i.index)), v => v, e => 77)",
            Type::Number,
            &[],
        );
        assert!(eval(&p, "lookup_byte", 0)
            .unwrap_err()
            .message
            .contains("byte_at"));
        differential(&p, "lookup_byte", &[-1, 0, 1]);
    }
    #[test]
    fn bounds_refusals_and_legacy_result() {
        for (body, ty) in [
            (
                "Ok(try_byte_at(b\"x\", 0))",
                Type::Result(Box::new(result_type()), Box::new(Type::Text)),
            ),
            ("Err(0)", result_type()),
            (
                "match_result(try_byte_at(b\"x\", 0), v => v, e => e)",
                Type::Number,
            ),
            ("try_byte_at(b\"x\", 0)", Type::BoundsError),
            (
                "try_byte_at(b\"x\", 0)",
                Type::Result(Box::new(Type::Text), Box::new(Type::BoundsError)),
            ),
            (
                "match_result(try_byte_at(b\"x\", 0), v => lookup_byte(i), e => lookup_byte(i))",
                result_type(),
            ),
        ] {
            assert!(!verify(&custom(body, ty, &[])).is_empty());
        }
        let p = custom(
            "Ok(1)",
            Type::Result(Box::new(Type::Number), Box::new(Type::Text)),
            &[("ignored", "Err(\"old contract\")")],
        );
        assert!(verify(&p).is_empty());
        let p = fixture();
        let dir = std::env::temp_dir().join(format!("verbose-bounds-wasm-{}", std::process::id()));
        assert!(!dir.exists());
        assert!(
            crate::wasm::compile_wasm(&p, "byte_or_zero", dir.to_str().unwrap())
                .unwrap_err()
                .message
                .contains("BoundsError")
        );
        assert!(!dir.exists());
    }
    #[test]
    fn bounds_effect_contexts_and_successive_calls() {
        let mut p = fixture();
        let call = expression("lookup_byte(i)");
        let expr = expression("match_result(try_byte_at(b\"x\", 0), v => v, e => 0)");
        let source = if let Item::Rule(r) = &p.items[1] {
            r.source.clone()
        } else {
            unreachable!()
        };
        p.items.push(Item::Reaction(Reaction {
            name: "effect".into(),
            intention: "test".into(),
            source,
            trigger: "unrelated".into(),
            effects: vec![Effect::Print(vec![expr])],
        }));
        assert!(verify(&p)
            .iter()
            .any(|e| e.message.contains("reaction effects")));
        p.items.pop();
        if let Item::Rule(r) = &mut p.items[1] {
            r.logic.bindings.push(("clock".into(), Expr::NowUnix));
        }
        assert!(verify(&p).iter().any(|e| e.message.contains("now_unix")));
        p = fixture();
        if let Item::Rule(r) = &mut p.items[3] {
            r.logic.bindings = vec![
                (
                    "one".into(),
                    Expr::Call("lookup_byte".into(), vec![Expr::Ident("request".into())]),
                ),
                (
                    "two".into(),
                    Expr::Call("lookup_byte".into(), vec![Expr::Ident("request".into())]),
                ),
            ];
            r.logic.value =
                expression("match_result(one, v => v, e => 0) + match_result(two, v => v, e => 1)");
            r.output_ty = Type::Number;
        }
        differential(&p, "forward_byte", &[-1, 0, 1, 2, 3]);
        if let Item::Rule(r) = &mut p.items[3] {
            r.logic.bindings = vec![("bad".into(), call)];
        }
        assert!(verify(&p)
            .iter()
            .any(|e| e.message.contains("callee(input)")));
    }

    #[test]
    fn bounds_self_hosted_refuses_before_artifact() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let p = parse(include_str!("../examples/vexprparse.verbose"));
        let bin = format!("/tmp/verbose-bounds-self-{}", std::process::id());
        crate::native::compile_native_stdin_raw(&p, "elf_program_src", &bin).unwrap();
        for source in [
            include_str!("../examples/try_byte_at.verbose").to_string(),
            include_str!("../examples/try_byte_at.verbose").replace("try_byte_at", "byte_at"),
        ] {
            let mut child = Command::new(&bin)
                .arg("0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(
                output.stdout.is_empty(),
                "unsupported contract emitted an artifact"
            );
        }
        // String contents and comments must not be mistaken for identifiers.
        let source = "@verbose 0.1.0\nrule main\n  @intention: \"BoundsError try_byte_at\"\n  @source: \"test.intent\":1\n  output:\n    out : number\n  logic:\n    out = 7\n  proofs:\n    purity:\n      reads : []\n      calls : []\n    termination:\n      bound : 8\n-- try_byte_at BoundsError\n";
        let mut child = Command::new(&bin)
            .arg("0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(output.stdout.starts_with(b"\x7fELF"));
        std::fs::remove_file(bin).unwrap();
    }
}
