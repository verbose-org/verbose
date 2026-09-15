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
    Number(i64, i64),
    Bool,
    Text(Option<u64>),
    Input(String),
    Record(String, HashMap<String, Value>),
}
impl Value {
    fn ty(&self) -> Type {
        match self {
            Self::Number(..) => Type::Number,
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
            Self::Number(..) => Ok(Some(20)),
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
    // Rule bodies are checked against their declared input, not specialized to
    // one call site's narrower values. Prove the transfer before using that
    // public contract; a nominal record type alone does not prove its ranges.
    fn call_input(&self, name: &str, value: &Value) -> Result<(), String> {
        let callee = self.rules.get(name).ok_or("unknown callee")?;
        value.require(&callee.input_ty)?;
        if matches!(value, Value::Input(_)) {
            // Forwarding the original input (also through an alias) preserves
            // the same concept and the entry's existing input guards.
            return Ok(());
        }
        let Value::Record(concept, values) = value else {
            return Err(format!("call '{name}' requires one flat record input"));
        };
        let concept = self
            .concepts
            .get(concept.as_str())
            .ok_or("unknown input concept")?;
        for field in &concept.fields {
            let value = values.get(&field.name).ok_or("missing input field")?;
            value.require(&field.ty)?;
            let context = format!("call '{name}' input field '{}'", field.name);
            match value {
                Value::Text(actual) => {
                    let max = field
                        .range
                        .and_then(|(_, n)| u64::try_from(n).ok())
                        .ok_or_else(|| format!("{context}: declared byte capacity is unknown"))?;
                    let actual = actual
                        .ok_or_else(|| format!("{context}: argument byte capacity is unknown"))?;
                    if actual > max {
                        return Err(format!("{context}: argument needs up to {actual} bytes, exceeds declared [..{max}]"));
                    }
                }
                Value::Number(lo, hi) => {
                    if let Some((min, max)) = field.range {
                        if *lo < min || *hi > max {
                            return Err(format!("{context}: cannot prove argument range [{lo}, {hi}] fits [{min}, {max}]"));
                        }
                    }
                }
                _ => {
                    return Err(format!(
                        "{context}: input must be a flat concept of numbers and text"
                    ))
                }
            }
        }
        Ok(())
    }

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
            Expr::Number(n) => Value::Number(*n, *n),
            Expr::Neg(n) if matches!(n.as_ref(), Expr::Number(0..)) => {
                let Expr::Number(n) = n.as_ref() else {
                    unreachable!()
                };
                Value::Number(-*n, -*n)
            }
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
                        Type::Number => {
                            let (min, max) = f.range.unwrap_or((i64::MIN, i64::MAX));
                            Value::Number(min, max)
                        }
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
                    (Value::Number(a, b), Value::Number(c, d)) => Value::Number(a.min(c), b.max(d)),
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
                if args.len() != 1 {
                    return Err(format!("call '{name}' requires exactly one record input"));
                }
                let input = sub(&args[0])?;
                self.call_input(name, &input)?;
                self.rule(name)?
            }
            Expr::Binary(op, left, right) => {
                let a = sub(left)?;
                let b = sub(right)?;
                match op {
                    BinOp::Eq | BinOp::NotEq => {
                        a.require(&b.ty())?;
                        if !matches!(a, Value::Number(..) | Value::Bool | Value::Text(_)) {
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
                let value = sub(e)?;
                value.require(&Type::Text)?;
                let Value::Text(cap) = value else {
                    unreachable!()
                };
                Value::Number(
                    0,
                    cap.and_then(|n| i64::try_from(n).ok()).unwrap_or(i64::MAX),
                )
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
                    if !matches!(v, Value::Text(_) | Value::Number(..) | Value::Bool) {
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

/// The only new escape from invocation storage: a complete, explicitly bounded
/// text call copied into a sequential HTTP service's existing owned state.
/// Used by the service capacity gate and native preparation too. Callee bodies
/// remain subject to the ordinary bounded-text analysis above.
pub(crate) fn state_call<'a>(
    service: &Service,
    handler: &Rule,
    set: &StateSet,
    rules: &HashMap<&str, &'a Rule>,
) -> Result<Option<&'a Rule>, String> {
    let Expr::Call(name, args) = &set.value else {
        return Ok(None);
    };
    let Some(callee) = rules.get(name.as_str()).copied() else {
        return Ok(None);
    };
    let Some(cap) = callee.output_text_max else {
        return Ok(None);
    };
    if service.protocol != Protocol::Http10 || service.concurrency != ConcurrencyMode::Sequential {
        return Err("bounded text state transfer requires a sequential HTTP service".into());
    }
    if args.len() != 1
        || !matches!(&args[0], Expr::Ident(n) if *n == handler.input_name)
        || handler.input_ty != Type::Named("HttpRequest".into())
        || callee.input_ty != handler.input_ty
    {
        return Err("bounded text state transfer requires callee(input) with the original HTTP input binding".into());
    }
    let field = service
        .state_fields
        .iter()
        .find(|f| f.name == set.field_name)
        .ok_or("bounded text state transfer requires a declared state field")?;
    if field.ty != Type::Text || callee.output_ty != Type::Text {
        return Err(
            "bounded text state transfer requires a text result and a text state field".into(),
        );
    }
    let max = field
        .max_bytes
        .filter(|n| (1..=65536).contains(n))
        .ok_or("bounded text state transfer requires a state capacity in 1..=65536")?;
    if i64::from(cap) > max {
        return Err(format!(
            "bounded text result [..{cap}] exceeds state field '{}' capacity [..{max}]",
            field.name
        ));
    }
    Ok(Some(callee))
}

pub(crate) fn service_uses_contract(s: &Service, active: &BTreeSet<String>) -> bool {
    if active.contains(&s.handler) {
        return true;
    }
    let mut names = BTreeSet::new();
    for set in &s.after_sets {
        calls(&set.value, &mut names);
    }
    !active.is_disjoint(&names)
}

pub(crate) fn verify_mode(p: &Program, native: bool) -> Vec<VerifyError> {
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
                    let mut names = BTreeSet::new();
                    calls(&set.value, &mut names);
                    if !active.is_disjoint(&names) {
                        let result = check.rules.get(s.handler.as_str())
                            .ok_or_else(|| "missing service handler".to_string())
                            .and_then(|handler| state_call(s, handler, set, &check.rules))
                            .and_then(|callee| callee.ok_or_else(|| "bounded text after mutation requires a complete call to a rule with an explicit text [..N] result".into()));
                        if let Err(message) = result {
                            errors.push(VerifyError {
                                context: format!(
                                    "service '{}' / after / set {}",
                                    s.name, set.field_name
                                ),
                                message,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        if !active.is_disjoint(&effect_calls) {
            errors.push(VerifyError {
                context: "bounded text output / effect".into(),
                message:
                    "participating rules cannot be called from reactions or logs in this slice"
                        .into(),
            });
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

#[cfg(test)]
mod tests;
