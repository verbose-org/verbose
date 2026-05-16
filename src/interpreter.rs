use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path as StdPath;

use crate::ast::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(i64),
    Bool(bool),
    Text(String),
    List(Vec<Value>),
    Record(HashMap<String, Value>),
    /// Ok(inner) — the success arm of a Result-typed output.
    Ok(Box<Value>),
    /// Err(inner) — the declared failure arm of a Result-typed output.
    Err(Box<Value>),
    /// Phase A slice 2: a value of a sum-type concept, tagged with its
    /// concept name + variant name and carrying the payload field values
    /// (empty when the variant has no payload).
    Variant {
        concept: String,
        variant: String,
        fields: HashMap<String, Value>,
    },
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Number(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Text(s) => write!(f, "{}", s),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, v) in items.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", v)?;
                }
                write!(f, "]")
            }
            Value::Record(fields) => {
                write!(f, "{{")?;
                let mut first = true;
                for (k, v) in fields {
                    if !first {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                    first = false;
                }
                write!(f, "}}")
            }
            Value::Ok(inner) => write!(f, "Ok({})", inner),
            Value::Err(inner) => write!(f, "Err({})", inner),
            Value::Variant { concept, variant, fields } => {
                write!(f, "{}::{}", concept, variant)?;
                if !fields.is_empty() {
                    write!(f, " {{")?;
                    let mut first = true;
                    for (k, v) in fields {
                        if !first {
                            write!(f, ", ")?;
                        }
                        write!(f, "{}: {}", k, v)?;
                        first = false;
                    }
                    write!(f, "}}")?;
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
pub struct RuntimeError {
    pub message: String,
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "runtime error: {}", self.message)
    }
}

pub fn load_json_input(path: &StdPath) -> Result<Vec<HashMap<String, Value>>, RuntimeError> {
    let content = fs::read_to_string(path).map_err(|e| RuntimeError {
        message: format!("cannot read '{}': {}", path.display(), e),
    })?;
    parse_json_array(&content)
}

fn parse_json_array(src: &str) -> Result<Vec<HashMap<String, Value>>, RuntimeError> {
    let trimmed = src.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(RuntimeError {
            message: "input must be a JSON array".into(),
        });
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut records = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                let chunk = inner[start..i].trim();
                if !chunk.is_empty() {
                    records.push(parse_json_object(chunk)?);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        records.push(parse_json_object(last)?);
    }
    Ok(records)
}

fn parse_json_object(src: &str) -> Result<HashMap<String, Value>, RuntimeError> {
    let trimmed = src.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return Err(RuntimeError {
            message: format!("expected JSON object, got: {}", &trimmed[..trimmed.len().min(40)]),
        });
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let mut fields = HashMap::new();
    for pair in split_json_top_level(inner) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let colon = pair.find(':').ok_or_else(|| RuntimeError {
            message: format!("invalid JSON field: {}", pair),
        })?;
        let key = pair[..colon].trim().trim_matches('"');
        let val_str = pair[colon + 1..].trim();
        let val = parse_json_value(val_str)?;
        fields.insert(key.to_string(), val);
    }
    Ok(fields)
}

fn parse_json_value(s: &str) -> Result<Value, RuntimeError> {
    let s = s.trim();
    if s == "true" {
        return Ok(Value::Bool(true));
    }
    if s == "false" {
        return Ok(Value::Bool(false));
    }
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        return Ok(Value::Text(s[1..s.len() - 1].to_string()));
    }
    if s.starts_with('[') && s.ends_with(']') {
        let inner = &s[1..s.len() - 1];
        let mut items = Vec::new();
        for chunk in split_json_top_level(inner) {
            let chunk = chunk.trim();
            if !chunk.is_empty() {
                items.push(parse_json_value(chunk)?);
            }
        }
        return Ok(Value::List(items));
    }
    if s.starts_with('{') && s.ends_with('}') {
        return Ok(Value::Record(parse_json_object(s)?));
    }
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Value::Number(n));
    }
    Err(RuntimeError {
        message: format!("unsupported JSON value: {}", s),
    })
}

fn split_json_top_level(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = &s[start..];
    if !last.trim().is_empty() {
        parts.push(last);
    }
    parts
}

/// Evaluate a single expression in the context of a rule and input record.
pub fn eval_rule_expr(
    expr: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    input: &HashMap<String, Value>,
) -> Result<Value, RuntimeError> {
    let mut env: HashMap<String, Value> = HashMap::new();
    env.insert(rule.input_name.clone(), Value::Record(input.clone()));
    eval_expr(expr, &env, all_rules)
}

pub fn eval_rule(
    rule: &Rule,
    all_rules: &[&Rule],
    input: &HashMap<String, Value>,
) -> Result<Value, RuntimeError> {
    let mut env: HashMap<String, Value> = HashMap::new();
    env.insert(rule.input_name.clone(), Value::Record(input.clone()));
    for (name, expr) in &rule.logic.bindings {
        let val = eval_expr(expr, &env, all_rules)?;
        env.insert(name.clone(), val);
    }
    eval_expr(&rule.logic.value, &env, all_rules)
}

fn eval_expr(
    expr: &Expr,
    env: &HashMap<String, Value>,
    all_rules: &[&Rule],
) -> Result<Value, RuntimeError> {
    match expr {
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Text(s) => Ok(Value::Text(s.clone())),
        Expr::Ident(name) => env.get(name).cloned().ok_or_else(|| RuntimeError {
            message: format!("undefined binding '{}'", name),
        }),
        Expr::Field(base, field) => {
            let base_val = eval_expr(base, env, all_rules)?;
            match base_val {
                Value::Record(fields) => {
                    fields.get(field).cloned().ok_or_else(|| RuntimeError {
                        message: format!("no field '{}' on record", field),
                    })
                }
                other => Err(RuntimeError {
                    message: format!("cannot access field '{}' on {}", field, other),
                }),
            }
        }
        Expr::Binary(op, left, right) => {
            let l = eval_expr(left, env, all_rules)?;
            let r = eval_expr(right, env, all_rules)?;
            match (op, &l, &r) {
                (BinOp::Eq, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a == b)),
                (BinOp::Eq, Value::Text(a), Value::Text(b)) => Ok(Value::Bool(a == b)),
                (BinOp::NotEq, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a != b)),
                (BinOp::NotEq, Value::Text(a), Value::Text(b)) => Ok(Value::Bool(a != b)),
                (BinOp::Add, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
                (BinOp::Sub, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a - b)),
                (BinOp::Mul, Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
                (BinOp::Div, Value::Number(a), Value::Number(b)) => {
                    if *b == 0 {
                        Err(RuntimeError { message: "division by zero".into() })
                    } else {
                        Ok(Value::Number(a / b))
                    }
                }
                (BinOp::Mod, Value::Number(a), Value::Number(b)) => {
                    if *b == 0 {
                        Err(RuntimeError { message: "modulo by zero".into() })
                    } else {
                        Ok(Value::Number(a % b))
                    }
                }
                (BinOp::Gt, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a > b)),
                (BinOp::Lt, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a < b)),
                (BinOp::GtEq, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a >= b)),
                (BinOp::LtEq, Value::Number(a), Value::Number(b)) => Ok(Value::Bool(a <= b)),
                (BinOp::And, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(*a && *b)),
                (BinOp::Or, Value::Bool(a), Value::Bool(b)) => Ok(Value::Bool(*a || *b)),
                _ => Err(RuntimeError {
                    message: format!("cannot apply {:?} to {} and {}", op, l, r),
                }),
            }
        }
        Expr::If(cond, then_expr, else_expr) => {
            match eval_expr(cond, env, all_rules)? {
                Value::Bool(true) => eval_expr(then_expr, env, all_rules),
                Value::Bool(false) => eval_expr(else_expr, env, all_rules),
                other => Err(RuntimeError {
                    message: format!("'if' condition must be bool, got {}", other),
                }),
            }
        }
        Expr::Not(inner) => {
            match eval_expr(inner, env, all_rules)? {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                other => Err(RuntimeError {
                    message: format!("'not' requires bool, got {}", other),
                }),
            }
        }
        Expr::Neg(inner) => {
            match eval_expr(inner, env, all_rules)? {
                Value::Number(n) => Ok(Value::Number(-n)),
                other => Err(RuntimeError {
                    message: format!("'-' requires number, got {}", other),
                }),
            }
        }
        Expr::Abs(inner) => {
            match eval_expr(inner, env, all_rules)? {
                Value::Number(n) => Ok(Value::Number(n.wrapping_abs())),
                other => Err(RuntimeError {
                    message: format!("'abs' requires number, got {}", other),
                }),
            }
        }
        Expr::Quantifier(kind, collection, var_name, predicate) => {
            let coll_val = eval_expr(collection, env, all_rules)?;
            let items = match coll_val {
                Value::List(items) => items,
                _ => {
                    return Err(RuntimeError {
                        message: "expected a collection for all/any".into(),
                    })
                }
            };
            let result = match kind {
                QuantifierKind::All => {
                    let mut ok = true;
                    for item in &items {
                        let mut inner_env = env.clone();
                        inner_env.insert(var_name.clone(), item.clone());
                        match eval_expr(predicate, &inner_env, all_rules)? {
                            Value::Bool(b) => {
                                if !b {
                                    ok = false;
                                    break;
                                }
                            }
                            _ => {
                                return Err(RuntimeError {
                                    message: "quantifier predicate must return bool".into(),
                                })
                            }
                        }
                    }
                    ok
                }
                QuantifierKind::Any => {
                    let mut ok = false;
                    for item in &items {
                        let mut inner_env = env.clone();
                        inner_env.insert(var_name.clone(), item.clone());
                        match eval_expr(predicate, &inner_env, all_rules)? {
                            Value::Bool(b) => {
                                if b {
                                    ok = true;
                                    break;
                                }
                            }
                            _ => {
                                return Err(RuntimeError {
                                    message: "quantifier predicate must return bool".into(),
                                })
                            }
                        }
                    }
                    ok
                }
            };
            Ok(Value::Bool(result))
        }
        Expr::Fold(collection, initial, acc_name, item_name, body) => {
            let coll_val = eval_expr(collection, env, all_rules)?;
            let items = match coll_val {
                Value::List(items) => items,
                _ => return Err(RuntimeError { message: "fold requires a collection".into() }),
            };
            let mut acc = eval_expr(initial, env, all_rules)?;
            for item in &items {
                let mut inner_env = env.clone();
                inner_env.insert(acc_name.clone(), acc);
                inner_env.insert(item_name.clone(), item.clone());
                acc = eval_expr(body, &inner_env, all_rules)?;
            }
            Ok(acc)
        }
        Expr::FoldBytes(text, initial, acc_name, byte_name, idx_name, body) => {
            // Iterate over the bytes of `text`, threading a Number-typed
            // accumulator through three bound names: acc, byte, idx. Body
            // returns the next accumulator value. Same shape as Fold but
            // the iteration source is a text's bytes, not a collection.
            let text_val = eval_expr(text, env, all_rules)?;
            let s = match text_val {
                Value::Text(s) => s,
                _ => return Err(RuntimeError { message: "fold_bytes requires text as first argument".into() }),
            };
            let init_val = eval_expr(initial, env, all_rules)?;
            let mut acc = match init_val {
                Value::Number(_) => init_val,
                _ => return Err(RuntimeError { message: "fold_bytes init must be a number".into() }),
            };
            for (i, &b) in s.as_bytes().iter().enumerate() {
                let mut inner_env = env.clone();
                inner_env.insert(acc_name.clone(), acc);
                inner_env.insert(byte_name.clone(), Value::Number(b as i64));
                inner_env.insert(idx_name.clone(), Value::Number(i as i64));
                acc = eval_expr(body, &inner_env, all_rules)?;
                if !matches!(acc, Value::Number(_)) {
                    return Err(RuntimeError {
                        message: "fold_bytes body must return a number".into(),
                    });
                }
            }
            Ok(acc)
        }
        Expr::Map(collection, var_name, body) => {
            let coll_val = eval_expr(collection, env, all_rules)?;
            let items = match coll_val {
                Value::List(items) => items,
                _ => return Err(RuntimeError { message: "map requires a collection".into() }),
            };
            let mut out = Vec::with_capacity(items.len());
            for item in &items {
                let mut inner_env = env.clone();
                inner_env.insert(var_name.clone(), item.clone());
                out.push(eval_expr(body, &inner_env, all_rules)?);
            }
            Ok(Value::List(out))
        }
        Expr::Filter(collection, var_name, predicate) => {
            let coll_val = eval_expr(collection, env, all_rules)?;
            let items = match coll_val {
                Value::List(items) => items,
                _ => return Err(RuntimeError { message: "filter requires a collection".into() }),
            };
            let mut out = Vec::new();
            for item in &items {
                let mut inner_env = env.clone();
                inner_env.insert(var_name.clone(), item.clone());
                match eval_expr(predicate, &inner_env, all_rules)? {
                    Value::Bool(true) => out.push(item.clone()),
                    Value::Bool(false) => {}
                    _ => return Err(RuntimeError {
                        message: "filter predicate must return bool".into(),
                    }),
                }
            }
            Ok(Value::List(out))
        }
        Expr::Ok(inner) => {
            // Pass-through: evaluate the inner expr and tag it as the success arm.
            let v = eval_expr(inner, env, all_rules)?;
            Ok(Value::Ok(Box::new(v)))
        }
        Expr::Err(inner) => {
            let v = eval_expr(inner, env, all_rules)?;
            Ok(Value::Err(Box::new(v)))
        }
        Expr::Record(_concept_name, fields) => {
            // Evaluate each field expression and assemble a Value::Record.
            // The verifier already cross-checked field set + types — at this
            // point we trust the structure.
            let mut map = HashMap::new();
            for (name, expr) in fields {
                let v = eval_expr(expr, env, all_rules)?;
                map.insert(name.clone(), v);
            }
            Ok(Value::Record(map))
        }
        // Phase A slice 2: variant construction. Evaluate each payload field
        // expression in source order, collect into a HashMap, and tag with
        // (concept, variant). No-payload variants produce an empty fields map.
        // The verifier already validated the concept/variant/field set, so
        // here we trust the structure.
        Expr::VariantConstruct(concept_name, variant_name, fields) => {
            let mut map = HashMap::new();
            for (name, expr) in fields {
                let v = eval_expr(expr, env, all_rules)?;
                map.insert(name.clone(), v);
            }
            Ok(Value::Variant {
                concept: concept_name.clone(),
                variant: variant_name.clone(),
                fields: map,
            })
        }
        Expr::Concat(args) => {
            // Variadic text builder. Each argument is converted to its text
            // form; non-scalar arguments trigger a runtime error (the verifier
            // should have caught them at compile time, but the interpreter
            // stays defensive — defence in depth).
            let mut out = String::new();
            for arg in args {
                match eval_expr(arg, env, all_rules)? {
                    Value::Text(s) => out.push_str(&s),
                    Value::Number(n) => out.push_str(&n.to_string()),
                    Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
                    other => {
                        return Err(RuntimeError {
                            message: format!(
                                "concat argument must be scalar (number/bool/text), got {}",
                                other
                            ),
                        });
                    }
                }
            }
            Ok(Value::Text(out))
        }
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            // Evaluate the target, dispatch on its Ok/Err tag. Exactly one
            // arm runs; the chosen arm's lambda variable is bound to the
            // inner value.
            match eval_expr(target, env, all_rules)? {
                Value::Ok(inner) => {
                    let mut new_env = env.clone();
                    new_env.insert(ok_var.clone(), *inner);
                    eval_expr(ok_body, &new_env, all_rules)
                }
                Value::Err(inner) => {
                    let mut new_env = env.clone();
                    new_env.insert(err_var.clone(), *inner);
                    eval_expr(err_body, &new_env, all_rules)
                }
                other => Err(RuntimeError {
                    message: format!(
                        "match_result requires a Result value, got {}",
                        other
                    ),
                }),
            }
        }
        Expr::Call(name, args) => {
            let called = all_rules
                .iter()
                .find(|r| r.name == *name)
                .ok_or_else(|| RuntimeError {
                    message: format!("unknown rule '{}'", name),
                })?;
            if args.len() != 1 {
                return Err(RuntimeError {
                    message: format!("rule call expects 1 argument, got {}", args.len()),
                });
            }
            let arg_val = eval_expr(&args[0], env, all_rules)?;
            match arg_val {
                Value::Record(fields) => eval_rule(called, all_rules, &fields),
                _ => Err(RuntimeError {
                    message: "call argument must be a record".into(),
                }),
            }
        }
        // Phase 9 slice 1: real read happens in native; interpreter returns empty placeholder for now.
        Expr::Read(_) => Ok(Value::Text("".into())),
        // Phase 11 slice 1: real fetch happens in native; interpreter
        // returns empty placeholder for now (same shape as Read).
        Expr::Fetch(_, _) => Ok(Value::Text("".into())),
        // Phase 12 (json_escape): pure transform — evaluate inner, then
        // escape the 5 JSON-significant bytes. Mirrors optimizer's
        // escape_json_string so the interpreter and the literal-folder
        // agree byte-for-byte.
        Expr::JsonEscape(inner) => {
            let v = eval_expr(inner, env, all_rules)?;
            match v {
                Value::Text(s) => {
                    let bytes = s.as_bytes();
                    let mut out = String::with_capacity(bytes.len() * 2);
                    for &b in bytes {
                        match b {
                            0x22 => { out.push('\\'); out.push('"'); }
                            0x5C => { out.push('\\'); out.push('\\'); }
                            0x0A => { out.push('\\'); out.push('n'); }
                            0x0D => { out.push('\\'); out.push('r'); }
                            0x09 => { out.push('\\'); out.push('t'); }
                            other => out.push(other as char),
                        }
                    }
                    Ok(Value::Text(out))
                }
                other => Err(RuntimeError {
                    message: format!(
                        "json_escape requires a text value, got {}",
                        other
                    ),
                }),
            }
        }
        // Phase 12 (parse_int): pure transform — evaluate inner, then
        // parse with strict semantics (trim whitespace, then i64 parse).
        // On parse failure, return an interpreter error in the same shape
        // as json_escape's type mismatch — fail-closed posture mirrors the
        // native sys_exit(1) abort.
        Expr::ParseInt(inner) => {
            let v = eval_expr(inner, env, all_rules)?;
            match v {
                Value::Text(s) => match s.trim().parse::<i64>() {
                    Ok(n) => Ok(Value::Number(n)),
                    Err(_) => Err(RuntimeError {
                        message: format!(
                            "parse_int could not parse {:?} as a number",
                            s
                        ),
                    }),
                },
                other => Err(RuntimeError {
                    message: format!(
                        "parse_int requires a text value, got {}",
                        other
                    ),
                }),
            }
        }
        // `now_unix()` — current Unix epoch seconds as a number. In the
        // interpreter we sample the host clock directly. Native captures
        // it once per rule invocation via clock_gettime; the verifier
        // requires the rule's `reads:` proof to declare `now`.
        Expr::NowUnix => {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            Ok(Value::Number(secs))
        }
        // `starts_with(<haystack>, <needle>)` — byte-level prefix test.
        // Both children must be text; empty needle is always true; needle
        // longer than haystack is false. Mirrors Rust's str::starts_with
        // on the byte slices, matching what native will emit.
        Expr::StartsWith(haystack, needle) => {
            let h = eval_expr(haystack, env, all_rules)?;
            let n = eval_expr(needle, env, all_rules)?;
            match (h, n) {
                (Value::Text(hs), Value::Text(ns)) => {
                    Ok(Value::Bool(hs.as_bytes().starts_with(ns.as_bytes())))
                }
                (h, n) => Err(RuntimeError {
                    message: format!(
                        "starts_with requires two text arguments, got {} and {}",
                        h, n
                    ),
                }),
            }
        }
        // `contains(<haystack>, <needle>)` — byte-level substring test.
        // Both children must be text; empty needle is always true (stdlib
        // convention via str::contains); needle longer than haystack is
        // false. Returns Value::Bool. Mirrors what native will emit
        // (naive O(N*M) substring search bounded by `max:` declarations).
        Expr::Contains(haystack, needle) => {
            let h = eval_expr(haystack, env, all_rules)?;
            let n = eval_expr(needle, env, all_rules)?;
            match (h, n) {
                (Value::Text(hs), Value::Text(ns)) => {
                    Ok(Value::Bool(hs.contains(&ns)))
                }
                (h, n) => Err(RuntimeError {
                    message: format!(
                        "contains requires two text arguments, got {} and {}",
                        h, n
                    ),
                }),
            }
        }
        // `ends_with(<haystack>, <needle>)` — byte-level suffix test.
        // Symmetric of starts_with: true iff haystack's last len(needle)
        // bytes match needle byte-for-byte. Empty needle is always true;
        // needle longer than haystack is false. Mirrors Rust's
        // str::ends_with on the byte slices, matching what native will emit.
        Expr::EndsWith(haystack, needle) => {
            let h = eval_expr(haystack, env, all_rules)?;
            let n = eval_expr(needle, env, all_rules)?;
            match (h, n) {
                (Value::Text(hs), Value::Text(ns)) => {
                    Ok(Value::Bool(hs.as_bytes().ends_with(ns.as_bytes())))
                }
                (h, n) => Err(RuntimeError {
                    message: format!(
                        "ends_with requires two text arguments, got {} and {}",
                        h, n
                    ),
                }),
            }
        }
        // `length(<text_expr>)` — byte count of inner text. Inner must
        // evaluate to Value::Text (else type error). Returns the byte
        // length as a Number, mirroring what native emits via the
        // strlen scan / len_slot load.
        Expr::Length(inner) => {
            let v = eval_expr(inner, env, all_rules)?;
            match v {
                Value::Text(s) => Ok(Value::Number(s.as_bytes().len() as i64)),
                other => Err(RuntimeError {
                    message: format!(
                        "length requires a text value, got {}",
                        other
                    ),
                }),
            }
        }
        // `min(<a>, <b>)` — binary scalar minimum. Both args must be
        // Number; returns the smaller of the two. Mirrors what native
        // emits via cmp + cmovg (branch-free).
        Expr::Min(left, right) => {
            let l = eval_expr(left, env, all_rules)?;
            let r = eval_expr(right, env, all_rules)?;
            match (l, r) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a.min(b))),
                (l, r) => Err(RuntimeError {
                    message: format!(
                        "min requires two number arguments, got {} and {}",
                        l, r
                    ),
                }),
            }
        }
        // `max(<a>, <b>)` — binary scalar maximum. Both args must be
        // Number; returns the larger of the two. Mirrors what native
        // emits via cmp + cmovl (branch-free).
        Expr::Max(left, right) => {
            let l = eval_expr(left, env, all_rules)?;
            let r = eval_expr(right, env, all_rules)?;
            match (l, r) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a.max(b))),
                (l, r) => Err(RuntimeError {
                    message: format!(
                        "max requires two number arguments, got {} and {}",
                        l, r
                    ),
                }),
            }
        }
        // `substring(<text>, <start>, <end>)` — byte-slice of inner text
        // over the half-open range [start, end). Bounds are enforced
        // fail-closed (same posture as the native abort path): end must
        // be <= length(text) and start must be <= end. Negative offsets
        // and out-of-range values produce a RuntimeError (mirrors what
        // native lowers to sys_exit(1)).
        Expr::Substring(text, start, end) => {
            let t = eval_expr(text, env, all_rules)?;
            let s = eval_expr(start, env, all_rules)?;
            let e = eval_expr(end, env, all_rules)?;
            match (t, s, e) {
                (Value::Text(text_val), Value::Number(start_n), Value::Number(end_n)) => {
                    let bytes = text_val.as_bytes();
                    let len = bytes.len() as i64;
                    if start_n < 0 || end_n < 0 || end_n > len || start_n > end_n {
                        return Err(RuntimeError {
                            message: format!(
                                "substring bounds out of range: start={}, end={}, length={}",
                                start_n, end_n, len
                            ),
                        });
                    }
                    let slice = &bytes[start_n as usize..end_n as usize];
                    match std::str::from_utf8(slice) {
                        Ok(s) => Ok(Value::Text(s.to_string())),
                        Err(_) => {
                            // Native treats the buffer as raw bytes; for the
                            // interpreter's Value::Text (which is a Rust String)
                            // we fall back to a lossy conversion. The native
                            // path doesn't reject non-UTF-8 slices either, so
                            // this preserves cross-backend agreement on valid
                            // ASCII input (the common case).
                            Ok(Value::Text(String::from_utf8_lossy(slice).into_owned()))
                        }
                    }
                }
                (t, s, e) => Err(RuntimeError {
                    message: format!(
                        "substring requires (text, number, number), got ({}, {}, {})",
                        t, s, e
                    ),
                }),
            }
        }
        // `byte_at(<text>, <index>)` — read the byte at the given offset of
        // the text expression, returning a Number in 0..256. Index is
        // zero-based. Bounds enforced fail-closed (same posture as the
        // native abort path): index must be < length(text). Negative
        // indices and out-of-range values produce a RuntimeError (mirrors
        // what native lowers to sys_exit(1)).
        Expr::ByteAt(text, index) => {
            let t = eval_expr(text, env, all_rules)?;
            let i = eval_expr(index, env, all_rules)?;
            match (t, i) {
                (Value::Text(text_val), Value::Number(idx)) => {
                    let bytes = text_val.as_bytes();
                    let len = bytes.len() as i64;
                    if idx < 0 || idx >= len {
                        return Err(RuntimeError {
                            message: format!(
                                "byte_at index out of range: index={}, length={}",
                                idx, len
                            ),
                        });
                    }
                    Ok(Value::Number(bytes[idx as usize] as i64))
                }
                (t, i) => Err(RuntimeError {
                    message: format!(
                        "byte_at requires (text, number), got ({}, {})",
                        t, i
                    ),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;

    fn make_rule() -> Rule {
        Rule {
            name: "test".into(),
            intention: "t".into(),
            source: SourceRef {
                file: "t.intent".into(),
                line: 1,
            },
            input_name: "i".into(),
            input_ty: Type::Named("Invoice".into()),
            output_name: "important".into(),
            output_ty: Type::Bool,
            logic: LogicStmt {
                bindings: vec![],
                target: "important".into(),
                value: Expr::Binary(
                    BinOp::Gt,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "amount".into())),
                    Box::new(Expr::Number(10000)),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path {
                        segments: vec!["i".into(), "amount".into()],
                    }],
                    calls: vec![],
                },
                termination: Termination {
                    bound: Some(1),
                },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        }
    }

    #[test]
    fn eval_above_threshold() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(15000));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_below_threshold() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(500));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_exact_boundary() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(10000));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_just_above() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(10001));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(true));
    }

    #[test]
    fn missing_field_fails() {
        let rule = make_rule();
        let input = HashMap::new();
        assert!(eval_rule(&rule, &[], &input).is_err());
    }

    #[test]
    fn json_parsing() {
        let json = r#"[{"amount": 100}, {"amount": 200}]"#;
        let records = parse_json_array(json).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["amount"], Value::Number(100));
        assert_eq!(records[1]["amount"], Value::Number(200));
    }

    #[test]
    fn json_nested_arrays() {
        let json = r#"[{"name": "A", "items": [{"x": 1}, {"x": 2}]}]"#;
        let records = parse_json_array(json).unwrap();
        assert_eq!(records.len(), 1);
        assert!(matches!(&records[0]["items"], Value::List(v) if v.len() == 2));
    }

    #[test]
    fn json_string_values() {
        let json = r#"[{"status": "active", "count": 5}]"#;
        let records = parse_json_array(json).unwrap();
        assert_eq!(records[0]["status"], Value::Text("active".into()));
        assert_eq!(records[0]["count"], Value::Number(5));
    }

    #[test]
    fn arithmetic_operations() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(100));
        // The rule tests > 10000, so with 100 it's false
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn division_by_zero_caught() {
        use crate::ast::*;
        let rule = Rule {
            name: "div_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Binary(
                    BinOp::Div,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                    Box::new(Expr::Number(0)),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(42));
        assert!(eval_rule(&rule, &[], &input).is_err());
    }

    #[test]
    fn modulo_operation() {
        use crate::ast::*;
        let rule = Rule {
            name: "mod_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Binary(
                    BinOp::Mod,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                    Box::new(Expr::Number(3)),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(10));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(1)); // 10 % 3 = 1
    }

    #[test]
    fn if_else_evaluation() {
        use crate::ast::*;
        let rule = Rule {
            name: "if_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::If(
                    Box::new(Expr::Binary(
                        BinOp::Gt,
                        Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                        Box::new(Expr::Number(10)),
                    )),
                    Box::new(Expr::Number(1)),
                    Box::new(Expr::Number(0)),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(3) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(15));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(1));

        input.insert("x".into(), Value::Number(5));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(0));
    }

    #[test]
    fn string_equality() {
        use crate::ast::*;
        let rule = Rule {
            name: "str_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Bool,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Binary(
                    BinOp::Eq,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "s".into())),
                    Box::new(Expr::Text("active".into())),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "s".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("s".into(), Value::Text("active".into()));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(true));

        input.insert("s".into(), Value::Text("blocked".into()));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn negative_numbers() {
        use crate::ast::*;
        let rule = Rule {
            name: "neg_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Neg(Box::new(Expr::Field(
                    Box::new(Expr::Ident("i".into())),
                    "x".into(),
                ))),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(42));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(-42));

        input.insert("x".into(), Value::Number(-10));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(10));

        input.insert("x".into(), Value::Number(0));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Number(0));
    }

    #[test]
    fn not_boolean() {
        use crate::ast::*;
        let rule = Rule {
            name: "not_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Bool,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Not(Box::new(Expr::Binary(
                    BinOp::Gt,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                    Box::new(Expr::Number(10)),
                ))),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(2) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(15));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(false)); // not (15 > 10) = not true = false

        input.insert("x".into(), Value::Number(5));
        assert_eq!(eval_rule(&rule, &[], &input).unwrap(), Value::Bool(true)); // not (5 > 10) = not false = true
    }

    #[test]
    fn modulo_by_zero_caught() {
        use crate::ast::*;
        let rule = Rule {
            name: "mod0_test".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Binary(
                    BinOp::Mod,
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                    Box::new(Expr::Number(0)),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(42));
        assert!(eval_rule(&rule, &[], &input).is_err());
    }

    #[test]
    fn map_doubles_each_element() {
        use crate::ast::*;
        let rule = Rule {
            name: "m".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Collection("number".into()),
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Map(
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "items".into())),
                    "x".into(),
                    Box::new(Expr::Binary(
                        BinOp::Mul,
                        Box::new(Expr::Ident("x".into())),
                        Box::new(Expr::Number(2)),
                    )),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "items".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(10) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert(
            "items".into(),
            Value::List(vec![Value::Number(1), Value::Number(2), Value::Number(3)]),
        );
        let result = eval_rule(&rule, &[], &input).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::Number(2), Value::Number(4), Value::Number(6)])
        );
    }

    #[test]
    fn filter_keeps_matching_elements() {
        use crate::ast::*;
        let rule = Rule {
            name: "f".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Collection("number".into()),
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Filter(
                    Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "items".into())),
                    "x".into(),
                    Box::new(Expr::Binary(
                        BinOp::Gt,
                        Box::new(Expr::Ident("x".into())),
                        Box::new(Expr::Number(10)),
                    )),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "items".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(10) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert(
            "items".into(),
            Value::List(vec![
                Value::Number(5),
                Value::Number(15),
                Value::Number(8),
                Value::Number(42),
            ]),
        );
        let result = eval_rule(&rule, &[], &input).unwrap();
        assert_eq!(
            result,
            Value::List(vec![Value::Number(15), Value::Number(42)])
        );
    }

    #[test]
    fn result_ok_value_returned() {
        use crate::ast::*;
        let rule = Rule {
            name: "validate".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Result(Box::new(Type::Number), Box::new(Type::Text)),
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::If(
                    Box::new(Expr::Binary(
                        BinOp::GtEq,
                        Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "age".into())),
                        Box::new(Expr::Number(18)),
                    )),
                    Box::new(Expr::Ok(Box::new(Expr::Field(
                        Box::new(Expr::Ident("i".into())),
                        "age".into(),
                    )))),
                    Box::new(Expr::Err(Box::new(Expr::Text("under 18".into())))),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "age".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(3) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let mut input = HashMap::new();
        input.insert("age".into(), Value::Number(25));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Ok(Box::new(Value::Number(25)))
        );

        input.insert("age".into(), Value::Number(15));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Err(Box::new(Value::Text("under 18".into())))
        );
    }

    #[test]
    fn match_result_dispatches_on_tag() {
        use crate::ast::*;
        // A rule that takes an input.x and produces Ok(x*2) if x>0, else Err("negative"),
        // then consumes that via match_result: Ok arm returns the doubled value + 1,
        // Err arm propagates the error unchanged.
        let rule = Rule {
            name: "chain".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Result(Box::new(Type::Number), Box::new(Type::Text)),
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                // match_result(
                //   if i.x > 0 then Ok(i.x * 2) else Err("negative"),
                //   v => Ok(v + 1),
                //   e => Err(e)
                // )
                value: Expr::MatchResult(
                    Box::new(Expr::If(
                        Box::new(Expr::Binary(
                            BinOp::Gt,
                            Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                            Box::new(Expr::Number(0)),
                        )),
                        Box::new(Expr::Ok(Box::new(Expr::Binary(
                            BinOp::Mul,
                            Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "x".into())),
                            Box::new(Expr::Number(2)),
                        )))),
                        Box::new(Expr::Err(Box::new(Expr::Text("negative".into())))),
                    )),
                    "v".into(),
                    Box::new(Expr::Ok(Box::new(Expr::Binary(
                        BinOp::Add,
                        Box::new(Expr::Ident("v".into())),
                        Box::new(Expr::Number(1)),
                    )))),
                    "e".into(),
                    Box::new(Expr::Err(Box::new(Expr::Ident("e".into())))),
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(20) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };

        // Ok path: i.x = 5 → Ok(5*2) → match binds v=10 → Ok(v+1) = Ok(11)
        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(5));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Ok(Box::new(Value::Number(11)))
        );

        // Err path: i.x = -3 → Err("negative") → match binds e="negative" → Err(e) unchanged
        input.insert("x".into(), Value::Number(-3));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Err(Box::new(Value::Text("negative".into())))
        );
    }

    #[test]
    fn concat_builds_dynamic_text() {
        use crate::ast::*;
        // logic: r = concat("age ", i.x, " years")
        let rule = Rule {
            name: "build".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("T".into()),
            output_name: "r".into(),
            output_ty: Type::Text,
            logic: LogicStmt {
                bindings: vec![],
                target: "r".into(),
                value: Expr::Concat(vec![
                    Expr::Text("age ".into()),
                    Expr::Field(Box::new(Expr::Ident("i".into())), "x".into()),
                    Expr::Text(" years".into()),
                ]),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "x".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(4) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };

        let mut input = HashMap::new();
        input.insert("x".into(), Value::Number(42));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Text("age 42 years".into())
        );

        input.insert("x".into(), Value::Number(-3));
        assert_eq!(
            eval_rule(&rule, &[], &input).unwrap(),
            Value::Text("age -3 years".into())
        );
    }

    #[test]
    fn empty_json_array() {
        let json = "[]";
        let records = parse_json_array(json).unwrap();
        assert_eq!(records.len(), 0);
    }

    #[test]
    fn json_boolean_values() {
        let json = r#"[{"active": true, "deleted": false}]"#;
        let records = parse_json_array(json).unwrap();
        assert_eq!(records[0]["active"], Value::Bool(true));
        assert_eq!(records[0]["deleted"], Value::Bool(false));
    }

    /// fold_bytes slice 1 — semantic surface. The interpreter handles
    /// the byte-loop end-to-end (native emit is a follow-up slice).
    /// This test pins the canonical "find first digit position" use
    /// case, which is the exemplar that motivated fold_bytes in the
    /// first place: variable-length scan with running accumulator,
    /// idx-aware so the position can be returned.
    ///
    /// Body grammar: standard if/else chain on acc/byte/idx, with
    /// short-circuit `and` (from PR #20) for the digit range check.
    #[test]
    fn fold_bytes_find_first_digit_position() {
        // Build the rule manually:
        //   fold_bytes(i.s, -1, acc, b, idx =>
        //     if acc >= 0 then acc
        //     else if b >= 48 and b <= 57 then idx
        //     else acc)
        let body = Expr::If(
            Box::new(Expr::Binary(
                BinOp::GtEq,
                Box::new(Expr::Ident("acc".into())),
                Box::new(Expr::Number(0)),
            )),
            Box::new(Expr::Ident("acc".into())),
            Box::new(Expr::If(
                Box::new(Expr::Binary(
                    BinOp::And,
                    Box::new(Expr::Binary(
                        BinOp::GtEq,
                        Box::new(Expr::Ident("b".into())),
                        Box::new(Expr::Number(48)),
                    )),
                    Box::new(Expr::Binary(
                        BinOp::LtEq,
                        Box::new(Expr::Ident("b".into())),
                        Box::new(Expr::Number(57)),
                    )),
                )),
                Box::new(Expr::Ident("idx".into())),
                Box::new(Expr::Ident("acc".into())),
            )),
        );
        let logic_value = Expr::FoldBytes(
            Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "s".into())),
            Box::new(Expr::Number(-1)),
            "acc".into(),
            "b".into(),
            "idx".into(),
            Box::new(body),
        );
        let rule = Rule {
            name: "first_digit_pos".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("Input".into()),
            output_name: "n".into(),
            output_ty: Type::Number,
            logic: LogicStmt {
                bindings: vec![],
                target: "n".into(),
                value: logic_value,
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "s".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(10) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };

        let run = |text: &str| -> Value {
            let mut input = HashMap::new();
            input.insert("s".into(), Value::Text(text.into()));
            eval_rule(&rule, &[], &input).unwrap()
        };
        // '  42' → 2 (first digit at position 2)
        assert_eq!(run("  42"), Value::Number(2));
        // '123' → 0 (first digit at position 0)
        assert_eq!(run("123"), Value::Number(0));
        // 'abc' → -1 (no digit found, initial acc preserved)
        assert_eq!(run("abc"), Value::Number(-1));
        // '' → -1 (no bytes to iterate)
        assert_eq!(run(""), Value::Number(-1));
        // 'a9' → 1 (digit at position 1)
        assert_eq!(run("a9"), Value::Number(1));
        // Long input: 'xxxxxx5' → 6 (digit at end)
        assert_eq!(run("xxxxxx5"), Value::Number(6));
    }

    /// Phase A slice 2 — variant construction in the interpreter.
    ///
    /// `Expr::VariantConstruct(concept, variant, fields)` evaluates to a
    /// `Value::Variant` tagged with the concept + variant name and carrying
    /// the evaluated payload. No-payload variants produce a Variant whose
    /// fields map is empty (NOT a Record or a Unit — the tag is what
    /// distinguishes the variant). This is the runtime semantics the
    /// later pattern-match slice (A.3) will dispatch on.
    #[test]
    fn phase_a2_variant_construct_runtime() {
        // Build a rule manually:
        //   rule wrap_id
        //     input:  i : Input  (id: number)
        //     output: t : Token
        //     logic:  t = Token::Int { value: i.id }
        //
        // We don't need the concept declarations at runtime — the
        // interpreter only sees the AST. The verifier validates the
        // shape; the interpreter trusts it.
        let payload_rule = Rule {
            name: "wrap_id".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("Input".into()),
            output_name: "t".into(),
            output_ty: Type::Named("Token".into()),
            logic: LogicStmt {
                bindings: vec![],
                target: "t".into(),
                value: Expr::VariantConstruct(
                    "Token".into(),
                    "Int".into(),
                    vec![(
                        "value".into(),
                        Expr::Field(Box::new(Expr::Ident("i".into())), "id".into()),
                    )],
                ),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![Path { segments: vec!["i".into(), "id".into()] }],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };

        let mut input = HashMap::new();
        input.insert("id".into(), Value::Number(42));
        let result = eval_rule(&payload_rule, &[], &input).unwrap();
        match result {
            Value::Variant { concept, variant, fields } => {
                assert_eq!(concept, "Token");
                assert_eq!(variant, "Int");
                assert_eq!(fields.len(), 1);
                assert_eq!(fields.get("value"), Some(&Value::Number(42)));
            }
            other => panic!("expected Value::Variant, got {:?}", other),
        }

        // No-payload variant: Token::Eof
        let eof_rule = Rule {
            name: "make_eof".into(),
            intention: "t".into(),
            source: SourceRef { file: "t.intent".into(), line: 1 },
            input_name: "i".into(),
            input_ty: Type::Named("Input".into()),
            output_name: "t".into(),
            output_ty: Type::Named("Token".into()),
            logic: LogicStmt {
                bindings: vec![],
                target: "t".into(),
                value: Expr::VariantConstruct("Token".into(), "Eof".into(), vec![]),
            },
            proofs: Proofs {
                purity: Purity {
                    reads: vec![],
                    calls: vec![],
                },
                termination: Termination { bound: Some(1) },
            },
            hints: None,
            layer: None,
            context_name: None,
            context_ty: None,
        };
        let result = eval_rule(&eof_rule, &[], &HashMap::new()).unwrap();
        match result {
            Value::Variant { concept, variant, fields } => {
                assert_eq!(concept, "Token");
                assert_eq!(variant, "Eof");
                assert!(fields.is_empty(), "no-payload variant has empty fields map");
            }
            other => panic!("expected Value::Variant, got {:?}", other),
        }
    }
}
