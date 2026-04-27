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
}
