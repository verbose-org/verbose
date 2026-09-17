//! Strict overflow contracts: no unknown interval is evidence of safety.
//! The participating call graph is pure, acyclic, scalar, and forwards the
//! same input record. Its enforced entry bounds are the proof's premises.
use crate::ast::*;
use crate::verifier::{walk_expr_children, VerifyError};
use std::collections::{BTreeSet, HashMap};

pub(crate) mod native_opt;

pub fn has_contract(r: &Rule) -> bool {
    r.hints.as_ref().and_then(|h| h.overflow.as_ref()).is_some()
}

fn calls(e: &Expr, names: &mut BTreeSet<String>) {
    if let Expr::Call(n, _) = e {
        names.insert(n.clone());
    }
    walk_expr_children(e, &mut |c| calls(c, names));
}

pub fn participating(rules: &[&Rule]) -> BTreeSet<String> {
    let mut active: BTreeSet<_> = rules
        .iter()
        .filter(|r| has_contract(r))
        .map(|r| r.name.clone())
        .collect();
    if active.is_empty() {
        return active;
    }
    let edges: Vec<_> = rules
        .iter()
        .map(|r| {
            let mut deps = BTreeSet::new();
            for (_, e) in &r.logic.bindings {
                calls(e, &mut deps);
            }
            calls(&r.logic.value, &mut deps);
            (r.name.clone(), deps)
        })
        .collect();
    loop {
        let before = active.len();
        for (name, deps) in &edges {
            if active.contains(name) || !active.is_disjoint(deps) {
                active.insert(name.clone());
                active.extend(deps.iter().cloned());
            }
        }
        if before == active.len() {
            return active;
        }
    }
}

pub fn active_rules(p: &Program) -> BTreeSet<String> {
    participating(
        &p.items
            .iter()
            .filter_map(|i| match i {
                Item::Rule(r) => Some(r),
                _ => None,
            })
            .collect::<Vec<_>>(),
    )
}

#[derive(Clone, Copy, Debug)]
enum Value {
    Number(i64, i64),
    Bool,
}
impl Value {
    fn number(self) -> Result<(i64, i64), String> {
        match self {
            Self::Number(a, b) => Ok((a, b)),
            _ => Err("expected number, found bool".into()),
        }
    }
    fn boolean(self) -> Result<(), String> {
        match self {
            Self::Bool => Ok(()),
            _ => Err("expected bool, found number".into()),
        }
    }
    fn ty(self) -> Type {
        match self {
            Self::Number(..) => Type::Number,
            Self::Bool => Type::Bool,
        }
    }
}

struct Check<'a> {
    rules: HashMap<&'a str, &'a Rule>,
    concepts: HashMap<&'a str, &'a Concept>,
    visiting: BTreeSet<String>,
    cache: HashMap<String, Value>,
    steps: usize,
}
impl<'a> Check<'a> {
    fn new(p: &'a Program) -> Self {
        Self {
            rules: p
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Rule(r) => Some((r.name.as_str(), r)),
                    _ => None,
                })
                .collect(),
            concepts: iter_all_concepts(&p.items)
                .map(|c| (c.name.as_str(), c))
                .collect(),
            visiting: BTreeSet::new(),
            cache: HashMap::new(),
            steps: 0,
        }
    }
    fn rule(&mut self, name: &str) -> Result<Value, String> {
        if let Some(v) = self.cache.get(name) {
            return Ok(*v);
        }
        if self.visiting.len() >= 128 {
            return Err("analysis exceeds 128 nested rule calls".into());
        }
        if !self.visiting.insert(name.into()) {
            return Err(format!("recursion through rule '{name}' is unsupported"));
        }
        let result = self.rule_inner(name);
        self.visiting.remove(name);
        if let Ok(v) = result {
            self.cache.insert(name.into(), v);
        }
        result.map_err(|e| format!("rule '{name}': {e}"))
    }
    fn rule_inner(&mut self, name: &str) -> Result<Value, String> {
        let r = *self.rules.get(name).ok_or("unknown callee")?;
        if r.context_name.is_some() || r.context_ty.is_some() {
            return Err("context inputs are unsupported".into());
        }
        if !matches!(r.output_ty, Type::Number | Type::Bool) {
            return Err("only number/bool outputs are supported".into());
        }
        let Type::Named(cname) = &r.input_ty else {
            return Err("a flat input concept is required".into());
        };
        let c = *self
            .concepts
            .get(cname.as_str())
            .ok_or("unknown input concept")?;
        if !c.variants.is_empty()
            || c.fields.is_empty()
            || c.fields
                .iter()
                .any(|f| !matches!(f.ty, Type::Number | Type::Text))
        {
            return Err("input must be a nonempty flat concept of numbers and text".into());
        }
        for f in &c.fields {
            if let Some((lo, hi)) = f.range {
                if lo > hi || (f.ty == Type::Text && hi < 0) {
                    return Err(format!(
                        "input field '{}' has invalid declared bounds",
                        f.name
                    ));
                }
            }
        }
        let mut env = HashMap::new();
        for (n, e) in &r.logic.bindings {
            let v = self
                .expr(e, r, c, &env, 0)
                .map_err(|e| format!("let '{n}': {e}"))?;
            env.insert(n.clone(), v);
        }
        let value = self
            .expr(&r.logic.value, r, c, &env, 0)
            .map_err(|e| format!("output '{}': {e}", r.output_name))?;
        if value.ty() != r.output_ty {
            return Err("computed output type differs from declaration".into());
        }
        if let Some(h) = r.hints.as_ref().and_then(|h| h.overflow.as_ref()) {
            if h.min > h.max {
                return Err(format!(
                    "invalid overflow bounds: min {} > max {}",
                    h.min, h.max
                ));
            }
            let (lo, hi) = value.number()?;
            if lo < h.min || hi > h.max {
                return Err(format!(
                    "computed range [{lo}, {hi}] exceeds declared [{}, {}]",
                    h.min, h.max
                ));
            }
            // Callers rely on the public declaration, not implementation details.
            return Ok(Value::Number(h.min, h.max));
        }
        Ok(value)
    }
    fn expr(
        &mut self,
        e: &Expr,
        r: &Rule,
        c: &Concept,
        env: &HashMap<String, Value>,
        depth: usize,
    ) -> Result<Value, String> {
        self.steps += 1;
        if self.steps > 100_000 {
            return Err("analysis exceeds 100000 expressions".into());
        }
        if depth >= 256 {
            return Err("analysis exceeds 256 expression levels".into());
        }
        let mut eval = |e: &Expr| self.expr(e, r, c, env, depth + 1);
        match e {
            Expr::Number(n) => Ok(Value::Number(*n, *n)),
            Expr::Ident(n) => env.get(n).copied().ok_or_else(|| format!("unknown scalar binding '{n}'")),
            Expr::Field(base, name) => {
                if !matches!(base.as_ref(), Expr::Ident(n) if n == &r.input_name && !env.contains_key(n)) {
                    return Err("only unshadowed numeric input fields are supported".into());
                }
                let f = c.fields.iter().find(|f| f.name == *name).ok_or("unknown input field")?;
                if f.ty != Type::Number { return Err(format!("field '{name}': only numeric field reads are supported")); }
                let (lo, hi) = f.range.unwrap_or((i64::MIN, i64::MAX));
                Ok(Value::Number(lo, hi))
            }
            Expr::If(cond, a, b) => {
                eval(cond).map_err(|e| format!("if condition: {e}"))?.boolean()?;
                let a = eval(a).map_err(|e| format!("then branch: {e}"))?;
                let b = eval(b).map_err(|e| format!("else branch: {e}"))?;
                match (a, b) {
                    (Value::Number(a, b), Value::Number(c, d)) => Ok(Value::Number(a.min(c), b.max(d))),
                    (Value::Bool, Value::Bool) => Ok(Value::Bool),
                    _ => Err("conditional branches have different types".into()),
                }
            }
            Expr::Not(v) => { eval(v)?.boolean()?; Ok(Value::Bool) }
            Expr::Neg(v) | Expr::Abs(v) => {
                let (lo, hi) = eval(v)?.number()?;
                if lo == i64::MIN { return Err("unary arithmetic may overflow i64 at MIN".into()); }
                if matches!(e, Expr::Neg(_)) { Ok(Value::Number(-hi, -lo)) }
                else { Ok(Value::Number(if lo <= 0 && hi >= 0 { 0 } else { lo.abs().min(hi.abs()) }, lo.abs().max(hi.abs()))) }
            }
            Expr::Min(a, b) | Expr::Max(a, b) => {
                let (a, bnd) = eval(a)?.number()?;
                let (c, d) = eval(b)?.number()?;
                Ok(if matches!(e, Expr::Min(..)) { Value::Number(a.min(c), bnd.min(d)) } else { Value::Number(a.max(c), bnd.max(d)) })
            }
            Expr::Binary(op, a, b) => {
                let a = eval(a).map_err(|e| format!("left operand of {op:?}: {e}"))?;
                let b = eval(b).map_err(|e| format!("right operand of {op:?}: {e}"))?;
                match op {
                    BinOp::And | BinOp::Or => { a.boolean()?; b.boolean()?; Ok(Value::Bool) }
                    BinOp::Eq | BinOp::NotEq if a.ty() == b.ty() => Ok(Value::Bool),
                    BinOp::Gt | BinOp::GtEq | BinOp::Lt | BinOp::LtEq => { a.number()?; b.number()?; Ok(Value::Bool) }
                    _ => {
                        let (lo, hi) = a.number()?;
                        let (rl, rh) = b.number()?;
                        arithmetic(*op, (lo, hi), (rl, rh))
                    }
                }
            }
            Expr::Call(name, args) => {
                let callee = self.rules.get(name.as_str()).ok_or("unknown callee")?;
                if args.len() != 1 || !matches!(&args[0], Expr::Ident(n) if n == &r.input_name && !env.contains_key(n)) || callee.input_ty != r.input_ty {
                    return Err(format!("call '{name}': requires callee(input) with the same input concept; input bounds must be preserved"));
                }
                self.rule(name)
            }
            _ => Err("unknown overflow analysis: unsupported expression; use pure scalar arithmetic, lets, conditions and same-input acyclic calls".into()),
        }
    }
}

fn arithmetic(op: BinOp, (a, b): (i64, i64), (c, d): (i64, i64)) -> Result<Value, String> {
    if matches!(op, BinOp::Div | BinOp::Mod) {
        if c <= 0 && d >= 0 {
            return Err(format!("{op:?}: divisor range [{c}, {d}] includes zero"));
        }
        if a == i64::MIN && c <= -1 && d >= -1 {
            return Err(format!("{op:?}: MIN / -1 or MIN % -1 may overflow i64"));
        }
    }
    let (lo, hi): (i128, i128) = match op {
        BinOp::Add => (a as i128 + c as i128, b as i128 + d as i128),
        BinOp::Sub => (a as i128 - d as i128, b as i128 - c as i128),
        BinOp::Mul | BinOp::Div => {
            let xs = [(a, c), (a, d), (b, c), (b, d)].map(|(x, y)| {
                if op == BinOp::Mul {
                    x as i128 * y as i128
                } else {
                    x as i128 / y as i128
                }
            });
            (*xs.iter().min().unwrap(), *xs.iter().max().unwrap())
        }
        BinOp::Mod => {
            let limit = (c as i128).abs().max((d as i128).abs()) - 1;
            (
                (a as i128).min(0).max(-limit),
                (b as i128).max(0).min(limit),
            )
        }
        _ => return Err("incompatible operands".into()),
    };
    if lo < i64::MIN as i128 || hi > i64::MAX as i128 {
        return Err(format!("{op:?}: intermediate range [{lo}, {hi}] may overflow i64; tighten enforced input bounds or rewrite the calculation"));
    }
    Ok(Value::Number(lo as i64, hi as i64))
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    let active = active_rules(p);
    if active.is_empty() {
        return vec![];
    }
    let mut check = Check::new(p);
    let mut errors = Vec::new();
    for name in &active {
        if let Err(message) = check.rule(name) {
            errors.push(VerifyError {
                context: format!("rule '{name}' / hints.overflow"),
                message,
            });
        }
    }
    for item in &p.items {
        let bad = match item {
            Item::Service(s) => {
                let mut names = BTreeSet::new();
                for log in &s.logs {
                    match &log.effect {
                        Effect::Print(es) => {
                            for e in es {
                                calls(e, &mut names);
                            }
                        }
                        Effect::AppendFile { content, .. } => calls(content, &mut names),
                    }
                }
                crate::text_bounds::service_uses_contract(s, &active) || !names.is_disjoint(&active)
            }
            Item::Reaction(rx) => {
                active.contains(&rx.trigger)
                    || rx.effects.iter().any(|effect| {
                        let mut names = BTreeSet::new();
                        match effect {
                            Effect::Print(es) => {
                                for e in es {
                                    calls(e, &mut names);
                                }
                            }
                            Effect::AppendFile { content, .. } => calls(content, &mut names),
                        }
                        !names.is_disjoint(&active)
                    })
            }
            _ => false,
        };
        if bad {
            errors.push(VerifyError {
                context: "hints.overflow".into(),
                message: "service/reaction contexts are unsupported by strict overflow contracts"
                    .into(),
            });
        }
    }
    errors
}

#[cfg(test)]
mod tests;
