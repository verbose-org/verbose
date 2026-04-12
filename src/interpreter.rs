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

pub fn eval_rule(
    rule: &Rule,
    all_rules: &[&Rule],
    input: &HashMap<String, Value>,
) -> Result<Value, RuntimeError> {
    let mut env: HashMap<String, Value> = HashMap::new();
    env.insert(rule.input_name.clone(), Value::Record(input.clone()));
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
                    writes: vec![],
                    calls: vec![],
                    verdict: PurityVerdict::Pure,
                },
                termination: Termination {
                    form: TerminationForm::ConstantBound,
                    bound: Some(1),
                },
                determinism: Determinism {
                    form: DeterminismForm::Total,
                },
            },
            hints: None,
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
}
