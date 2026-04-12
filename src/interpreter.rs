use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::Path as StdPath;

use crate::ast::*;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Number(i64),
    Bool(bool),
    Record(HashMap<String, Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Number(n) => write!(f, "{}", n),
            Value::Bool(b) => write!(f, "{}", b),
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
    for pair in inner.split(',') {
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
    if let Ok(n) = s.parse::<i64>() {
        return Ok(Value::Number(n));
    }
    Err(RuntimeError {
        message: format!("unsupported JSON value: {}", s),
    })
}

pub fn eval_rule(
    rule: &Rule,
    input: &HashMap<String, Value>,
) -> Result<Value, RuntimeError> {
    let mut env: HashMap<String, Value> = HashMap::new();
    env.insert(rule.input_name.clone(), Value::Record(input.clone()));
    let result = eval_expr(&rule.logic.value, &env)?;
    Ok(result)
}

fn eval_expr(expr: &Expr, env: &HashMap<String, Value>) -> Result<Value, RuntimeError> {
    match expr {
        Expr::Number(n) => Ok(Value::Number(*n)),
        Expr::Ident(name) => env.get(name).cloned().ok_or_else(|| RuntimeError {
            message: format!("undefined binding '{}'", name),
        }),
        Expr::Field(base, field) => {
            let base_val = eval_expr(base, env)?;
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
            let l = eval_expr(left, env)?;
            let r = eval_expr(right, env)?;
            match (op, &l, &r) {
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
        }
    }

    #[test]
    fn eval_above_threshold() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(15000));
        assert_eq!(eval_rule(&rule, &input).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_below_threshold() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(500));
        assert_eq!(eval_rule(&rule, &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_exact_boundary() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(10000));
        assert_eq!(eval_rule(&rule, &input).unwrap(), Value::Bool(false));
    }

    #[test]
    fn eval_just_above() {
        let rule = make_rule();
        let mut input = HashMap::new();
        input.insert("amount".into(), Value::Number(10001));
        assert_eq!(eval_rule(&rule, &input).unwrap(), Value::Bool(true));
    }

    #[test]
    fn missing_field_fails() {
        let rule = make_rule();
        let input = HashMap::new();
        assert!(eval_rule(&rule, &input).is_err());
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
