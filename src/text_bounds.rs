//! Opt-in result capacities. Unknown analysis is a diagnostic, never a proof.
use crate::ast::*;
use crate::verifier::{walk_expr_children, VerifyError};
use std::collections::{BTreeSet, HashMap};

fn calls(e: &Expr, out: &mut BTreeSet<String>) {
    if let Expr::Call(name, _) = e {
        out.insert(name.clone());
    }
    walk_expr_children(e, &mut |child| calls(child, out));
}

pub fn participating(rules: &[&Rule]) -> BTreeSet<String> {
    let mut active: BTreeSet<_> = rules
        .iter()
        .filter(|r| r.output_text_max.is_some())
        .map(|r| r.name.clone())
        .collect();
    if active.is_empty() {
        return active;
    }
    let edges: Vec<_> = rules
        .iter()
        .map(|r| {
            let mut names = BTreeSet::new();
            for (_, rhs) in &r.logic.bindings {
                calls(rhs, &mut names);
            }
            calls(&r.logic.value, &mut names);
            (r.name.clone(), names)
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

#[derive(Clone, Debug)]
enum Value {
    Number,
    Bool,
    Text(Option<u64>),
    Input(String),
    Record(String, HashMap<String, Value>),
}
impl Value {
    fn ty(&self) -> Type {
        match self {
            Self::Number => Type::Number,
            Self::Bool => Type::Bool,
            Self::Text(_) => Type::Text,
            Self::Input(n) | Self::Record(n, _) => Type::Named(n.clone()),
        }
    }
    fn require(&self, ty: &Type) -> Result<(), String> {
        if self.ty() == *ty {
            Ok(())
        } else {
            Err(format!("expected {ty:?}, got {:?}", self.ty()))
        }
    }
    fn capacity(&self) -> Result<Option<u64>, String> {
        match self {
            Self::Text(n) => Ok(*n),
            Self::Number => Ok(Some(20)),
            _ => Err("concat requires text or number arguments".into()),
        }
    }
}

struct Check<'a> {
    rules: HashMap<&'a str, &'a Rule>,
    concepts: HashMap<&'a str, &'a Concept>,
    visiting: BTreeSet<String>,
    cache: HashMap<String, Value>,
    steps: usize,
    native: bool,
}
impl Check<'_> {
    fn rule(&mut self, name: &str) -> Result<Value, String> {
        if let Some(v) = self.cache.get(name) {
            return Ok(v.clone());
        }
        if self.visiting.len() >= 128 {
            return Err("analysis exceeds 128 nested rule calls".into());
        }
        if !self.visiting.insert(name.to_string()) {
            return Err(format!("recursion through rule '{name}' is unsupported"));
        }
        let result = self.rule_inner(name);
        self.visiting.remove(name);
        if let Ok(v) = &result {
            self.cache.insert(name.into(), v.clone());
        }
        result.map_err(|e| format!("rule '{name}': {e}"))
    }

    fn rule_inner(&mut self, name: &str) -> Result<Value, String> {
        let r = *self
            .rules
            .get(name)
            .ok_or_else(|| format!("unknown rule '{name}'"))?;
        if r.context_ty.is_some() || r.context_name.is_some() {
            return Err("context inputs are unsupported in bounded text analysis".into());
        }
        if r.hints.is_some() {
            return Err("optimization hints are unsupported in bounded text analysis".into());
        }
        let Type::Named(input) = &r.input_ty else {
            return Err("input must be a flat named concept of numbers and text".into());
        };
        let c = *self
            .concepts
            .get(input.as_str())
            .ok_or("unknown input concept")?;
        if !c.variants.is_empty()
            || c.fields
                .iter()
                .any(|f| !matches!(f.ty, Type::Number | Type::Text))
        {
            return Err("input must be a flat named concept of numbers and text".into());
        }
        if self.native
            && c.fields.iter().any(|f| {
                f.ty == Type::Text && f.range.is_some_and(|(_, n)| !(0..=1_048_576).contains(&n))
            })
        {
            return Err(
                "bounded text native input field capacity must be at most 1048576 bytes".into(),
            );
        }
        let mut env = HashMap::from([(r.input_name.clone(), Value::Input(input.clone()))]);
        for (name, e) in &r.logic.bindings {
            let value = self.expr(e, r, &env, 0)?;
            env.insert(name.clone(), value);
        }
        let mut v = self.expr(&r.logic.value, r, &env, 0)?;
        v.require(&r.output_ty)?;
        if let Some(max) = r.output_text_max {
            if max > 1_048_576 || r.output_ty != Type::Text {
                return Err("output bound requires text [..N], 0 <= N <= 1048576".into());
            }
            match v {
                Value::Text(Some(actual)) if actual <= u64::from(max) => {
                    // Composition uses the checked public contract, not incidental precision.
                    v = Value::Text(Some(u64::from(max)));
                }
                Value::Text(Some(actual)) => {
                    return Err(format!(
                        "text output needs up to {actual} bytes, exceeds declared [..{max}]"
                    ))
                }
                _ => {
                    return Err(format!(
                        "cannot prove text output [..{max}]: byte capacity is unknown"
                    ))
                }
            }
        }
        Ok(v)
    }

    fn expr(
        &mut self,
        e: &Expr,
        r: &Rule,
        env: &HashMap<String, Value>,
        depth: usize,
    ) -> Result<Value, String> {
        self.steps += 1;
        if self.steps > 100_000 || depth > 256 {
            return Err(
                "bounded text analysis limit exceeded (100000 nodes / 256 expression levels)"
                    .into(),
            );
        }
        let mut sub = |e| self.expr(e, r, env, depth + 1);
        let value = match e {
            Expr::Text(s) => Value::Text(Some(s.len() as u64)),
            Expr::Number(_) => Value::Number,
            Expr::Neg(n) if matches!(n.as_ref(), Expr::Number(0..)) => Value::Number,
            Expr::Ident(n) => env
                .get(n)
                .cloned()
                .ok_or_else(|| format!("unknown binding '{n}'"))?,
            Expr::Field(base, name) => match sub(base)? {
                Value::Input(c) => {
                    let f = self.concepts[c.as_str()]
                        .fields
                        .iter()
                        .find(|f| f.name == *name)
                        .ok_or_else(|| format!("unknown field '{c}.{name}'"))?;
                    match f.ty {
                        Type::Number => Value::Number,
                        Type::Text => Value::Text(f.range.and_then(|(_, n)| u64::try_from(n).ok())),
                        _ => return Err("unsupported field type".into()),
                    }
                }
                Value::Record(_, fields) => {
                    fields.get(name).cloned().ok_or("unknown record field")?
                }
                _ => return Err("field access requires a record".into()),
            },
            Expr::Concat(args) => {
                let mut total = Some(0u64);
                for e in args {
                    let n = sub(e)?.capacity()?;
                    total = match (total, n) {
                        (Some(a), Some(b)) => Some(
                            a.checked_add(b)
                                .ok_or("text capacity arithmetic overflow")?,
                        ),
                        _ => None,
                    };
                }
                Value::Text(total)
            }
            Expr::If(cond, yes, no) => {
                sub(cond)?.require(&Type::Bool)?;
                let a = sub(yes)?;
                let b = sub(no)?;
                a.require(&b.ty())?;
                match (a, b) {
                    (Value::Text(a), Value::Text(b)) => {
                        Value::Text(a.zip(b).map(|(a, b)| a.max(b)))
                    }
                    (Value::Number, Value::Number) => Value::Number,
                    (Value::Bool, Value::Bool) => Value::Bool,
                    // Fieldwise joins need their own analysis; never keep one arm's capacity.
                    _ => {
                        return Err(
                            "conditional record values are unsupported in bounded text analysis"
                                .into(),
                        )
                    }
                }
            }
            Expr::Call(name, args) => {
                if args.len() != 1
                    || !matches!(&args[0], Expr::Ident(n) if *n == r.input_name)
                    || !matches!(env.get(&r.input_name), Some(Value::Input(_)))
                {
                    return Err(format!(
                        "call '{name}' requires callee(input) with the original input binding"
                    ));
                }
                let callee = self.rules.get(name.as_str()).ok_or("unknown callee")?;
                if callee.input_ty != r.input_ty {
                    return Err(format!("call '{name}' requires the same input concept"));
                }
                self.rule(name)?
            }
            Expr::Binary(op, left, right) => {
                let a = sub(left)?;
                let b = sub(right)?;
                match op {
                    BinOp::Eq | BinOp::NotEq => {
                        a.require(&b.ty())?;
                        if !matches!(a, Value::Number | Value::Bool | Value::Text(_)) {
                            return Err("comparison requires scalar values".into());
                        }
                        Value::Bool
                    }
                    BinOp::And | BinOp::Or => {
                        a.require(&Type::Bool)?; b.require(&Type::Bool)?; Value::Bool
                    }
                    BinOp::Gt | BinOp::Lt | BinOp::GtEq | BinOp::LtEq => {
                        a.require(&Type::Number)?; b.require(&Type::Number)?; Value::Bool
                    }
                    _ => return Err("numeric arithmetic is outside this bounded text slice; pass or format a number directly".into()),
                }
            }
            Expr::Not(e) => {
                sub(e)?.require(&Type::Bool)?;
                Value::Bool
            }
            Expr::Length(e) => {
                sub(e)?.require(&Type::Text)?;
                Value::Number
            }
            Expr::Record(name, fields) => {
                let c = *self
                    .concepts
                    .get(name.as_str())
                    .ok_or("unknown output concept")?;
                if !c.variants.is_empty() || fields.len() != c.fields.len() {
                    return Err("record fields must match the declared concept".into());
                }
                let mut values = HashMap::new();
                for (n, e) in fields {
                    let f = c
                        .fields
                        .iter()
                        .find(|f| f.name == *n)
                        .ok_or("unknown record field")?;
                    let v = self.expr(e, r, env, depth + 1)?;
                    v.require(&f.ty)?;
                    if !matches!(v, Value::Text(_) | Value::Number | Value::Bool) {
                        return Err(
                            "nested records are unsupported in bounded text analysis".into()
                        );
                    }
                    if values.insert(n.clone(), v).is_some() {
                        return Err("duplicate record field".into());
                    }
                }
                Value::Record(name.clone(), values)
            }
            _ => {
                return Err(format!(
                    "unsupported expression in bounded text analysis: {}",
                    shape(e)
                ))
            }
        };
        if self.native && matches!(value, Value::Text(None) | Value::Text(Some(1_048_577..))) {
            return Err("bounded text native lowering requires every text expression to have a known capacity of at most 1048576 bytes".into());
        }
        Ok(value)
    }
}

fn shape(e: &Expr) -> &'static str {
    match e {
        Expr::Read(_) | Expr::Fetch(_, _) | Expr::Random(_) | Expr::NowUnix => "effect",
        Expr::Ok(_) | Expr::Err(_) | Expr::MatchResult(..) | Expr::TryByteAt(..) => "Result",
        Expr::JsonEscape(_) => "json_escape",
        Expr::Substring(..) => "substring",
        Expr::Fold(..)
        | Expr::FoldBytes(..)
        | Expr::Map(..)
        | Expr::Filter(..)
        | Expr::Quantifier(..) => "collection or fold",
        _ => "form outside the supported subset",
    }
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    verify_mode(p, false)
}

fn verify_mode(p: &Program, native: bool) -> Vec<VerifyError> {
    let active = active_rules(p);
    if active.is_empty() {
        return Vec::new();
    }
    let body_max = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Service(s) if s.protocol == Protocol::Http10 => Some(i64::from(s.max_request)),
            _ => None,
        })
        .max();
    let builtins = body_max
        .map(|max| {
            vec![
                crate::verifier::builtin_http_request(max),
                crate::verifier::builtin_http_response(),
            ]
        })
        .unwrap_or_default();
    let mut check = Check {
        rules: p
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Rule(r) => Some((r.name.as_str(), r)),
                _ => None,
            })
            .collect(),
        concepts: iter_all_concepts(&p.items)
            .chain(builtins.iter())
            .map(|c| (c.name.as_str(), c))
            .collect(),
        visiting: BTreeSet::new(),
        cache: HashMap::new(),
        steps: 0,
        native,
    };
    let mut errors = Vec::new();
    for name in &active {
        if let Err(message) = check.rule(name) {
            errors.push(VerifyError {
                context: format!("rule '{name}' / bounded text output"),
                message,
            });
        }
    }
    for item in &p.items {
        let mut effect_calls = BTreeSet::new();
        let mut inspect_effect = |e: &Effect| match e {
            Effect::Print(args) => {
                for a in args {
                    calls(a, &mut effect_calls);
                }
            }
            Effect::AppendFile { content, .. } => calls(content, &mut effect_calls),
        };
        match item {
            Item::Reaction(rx) => {
                for e in &rx.effects {
                    inspect_effect(e);
                }
                if active.contains(&rx.trigger) {
                    effect_calls.insert(rx.trigger.clone());
                }
            }
            Item::Service(s) => {
                for log in &s.logs {
                    inspect_effect(&log.effect);
                }
                for set in &s.after_sets {
                    calls(&set.value, &mut effect_calls);
                }
            }
            _ => {}
        }
        if !active.is_disjoint(&effect_calls) {
            errors.push(VerifyError { context: "bounded text output / effect".into(), message: "participating rules cannot be called from reactions, logs or after mutations in this slice".into() });
        }
        if let Item::Service(s) = item {
            if active.contains(&s.handler)
                && (s.protocol != Protocol::Http10
                    || !s.logs.is_empty()
                    || !s.after_sets.is_empty()
                    || !s.state_fields.is_empty())
            {
                errors.push(VerifyError {
                    context: format!("service '{}' / bounded text output", s.name),
                    message:
                        "this slice supports an HTTP handler without state, logs or after mutations"
                            .into(),
                });
            }
        }
    }
    errors
}

/// Lower the checked, pure, total subset to the legacy text emitter's grammar.
/// Substitution can duplicate computation but cannot duplicate effects or hide
/// checked-operation failure: neither is accepted by this slice. Count expanded
/// nodes so an alias DAG cannot turn into an unbounded compiler allocation.
pub fn lower_native(p: &Program) -> Result<Program, String> {
    if let Some(e) = verify_mode(p, true).first() {
        return Err(e.to_string());
    }
    let active = active_rules(p);
    let mut out = p.clone();
    let mut budget = ExpansionBudget::new();
    for item in &mut out.items {
        if let Item::Rule(r) = item {
            if !active.contains(&r.name) {
                continue;
            }
            let mut env = HashMap::from([(
                r.input_name.clone(),
                Expr::Ident("__bounded_text_input".into()),
            )]);
            for (name, expr) in &r.logic.bindings {
                let value = expand(expr, &env, &HashMap::new(), &mut budget, 0)?;
                env.insert(name.clone(), value);
            }
            r.logic.value = expand(&r.logic.value, &env, &HashMap::new(), &mut budget, 0)?;
            r.logic.bindings.clear();
            r.input_name = "__bounded_text_input".into();
        }
    }
    // The legacy backend expands acyclic calls too. Bound that expansion,
    // including a branching call DAG whose rule summaries were memoized above.
    fn cost(name: &str, rules: &HashMap<&str, &Rule>, cache: &mut HashMap<String, usize>) -> usize {
        if let Some(n) = cache.get(name) {
            return *n;
        }
        fn node(
            e: &Expr,
            rules: &HashMap<&str, &Rule>,
            cache: &mut HashMap<String, usize>,
        ) -> usize {
            let mut n: usize = 1;
            if let Expr::Call(name, _) = e {
                n = n.saturating_add(cost(name, rules, cache));
            }
            walk_expr_children(e, &mut |e| n = n.saturating_add(node(e, rules, cache)));
            n
        }
        let n = node(&rules[name].logic.value, rules, cache);
        cache.insert(name.into(), n);
        n
    }
    let rules: HashMap<_, _> = out
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let mut cache = HashMap::new();
    for name in &active {
        if cost(name, &rules, &mut cache) > 100_000 {
            return Err(format!(
                "bounded text native call expansion limit exceeded in rule '{name}'"
            ));
        }
    }
    let bodies: Vec<_> = out
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    let inline_rules = bodies.iter().map(|r| (r.name.as_str(), r)).collect();
    let mut budget = ExpansionBudget::new();
    for item in &mut out.items {
        if let Item::Rule(r) = item {
            if active.contains(&r.name) {
                r.logic.value = expand(
                    &r.logic.value,
                    &HashMap::new(),
                    &inline_rules,
                    &mut budget,
                    0,
                )?;
            }
        }
    }
    Ok(out)
}

struct ExpansionBudget {
    nodes: usize,
    literal_bytes: usize,
}
impl ExpansionBudget {
    fn new() -> Self {
        Self {
            nodes: 100_000,
            literal_bytes: 16 * 1024 * 1024,
        }
    }
}

fn expand(
    e: &Expr,
    env: &HashMap<String, Expr>,
    rules: &HashMap<&str, &Rule>,
    budget: &mut ExpansionBudget,
    depth: usize,
) -> Result<Expr, String> {
    if budget.nodes == 0 || depth > 256 {
        return Err("bounded text native expansion limit exceeded".into());
    }
    budget.nodes -= 1;
    if let Expr::Text(s) = e {
        budget.literal_bytes = budget
            .literal_bytes
            .checked_sub(s.len())
            .ok_or("bounded text native literal expansion exceeds 16 MiB")?;
    }
    let mut sub = |e| expand(e, env, rules, budget, depth + 1);
    Ok(match e {
        Expr::Ident(n) if env.contains_key(n) => {
            // An already expanded binding must never see a later lexical binding.
            expand(&env[n], &HashMap::new(), rules, budget, depth + 1)?
        }
        Expr::Text(_) | Expr::Number(_) | Expr::Ident(_) => e.clone(),
        Expr::Field(a, n) => Expr::Field(Box::new(sub(a)?), n.clone()),
        Expr::Concat(args) => {
            let mut flat = Vec::new();
            for arg in args {
                match sub(arg)? {
                    Expr::Concat(inner) => flat.extend(inner),
                    other => flat.push(other),
                }
            }
            Expr::Concat(flat)
        }
        Expr::Call(n, _) if rules.contains_key(n.as_str()) => sub(&rules[n.as_str()].logic.value)?,
        Expr::Call(n, args) => Expr::Call(
            n.clone(),
            args.iter().map(&mut sub).collect::<Result<_, _>>()?,
        ),
        Expr::If(c, a, b) => Expr::If(Box::new(sub(c)?), Box::new(sub(a)?), Box::new(sub(b)?)),
        Expr::Binary(op, a, b) => Expr::Binary(*op, Box::new(sub(a)?), Box::new(sub(b)?)),
        Expr::Not(a) => Expr::Not(Box::new(sub(a)?)),
        Expr::Neg(a) => Expr::Neg(Box::new(sub(a)?)),
        Expr::Length(a) => Expr::Length(Box::new(sub(a)?)),
        Expr::Record(n, fields) => Expr::Record(
            n.clone(),
            fields
                .iter()
                .map(|(n, e)| Ok((n.clone(), sub(e)?)))
                .collect::<Result<_, String>>()?,
        ),
        _ => return Err("unsupported expression in bounded text native expansion".into()),
    })
}

#[cfg(test)]
mod tests;
