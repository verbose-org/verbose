//! Original-AST oracle: pass the evaluated value directly to the next rule.
use super::*;
use crate::interpreter::eval_rule_with_value;

pub(super) fn run(
    e: &Execution,
    rules: &[&Rule],
    concepts: &[&Concept],
    records: &[HashMap<String, Value>],
    emit: &mut impl FnMut(usize, &Rule, usize, Value) -> Result<(), RuntimeError>,
) -> Result<i32, RuntimeError> {
    let mut failed = false;
    for (record, input) in records.iter().enumerate() {
        let mut value = Value::Record(input.clone());
        for (phase, name) in e.phases.iter().enumerate() {
            let rule = *rules.iter().find(|r| r.name == *name).unwrap();
            let context = |err: RuntimeError| {
                error(format!(
                    "execution '{}', phase {} ('{name}'), record {record}: {}",
                    e.name,
                    phase + 1,
                    err.message
                ))
            };
            // Pipelines opt into strict inputs even without a text annotation.
            // Guard all declared fields, including fields the phase never reads.
            check_input(rule, concepts, &value).map_err(context)?;
            value = eval_rule_with_value(rule, rules, concepts, &[], value).map_err(context)?;
            if phase + 1 == e.phases.len() {
                failed |= matches!(value, Value::Bool(false));
                emit(phase + 1, rule, record, value).map_err(context)?;
                break;
            }
        }
    }
    Ok(i32::from(failed))
}

fn check_input(rule: &Rule, concepts: &[&Concept], value: &Value) -> Result<(), RuntimeError> {
    let concept = concepts
        .iter()
        .find(|c| rule.input_ty == Type::Named(c.name.clone()))
        .ok_or_else(|| error("pipeline requires a declared input concept"))?;
    let Value::Record(values) = value else {
        return Err(error("pipeline requires a record input"));
    };
    for field in &concept.fields {
        let valid = match (&field.ty, values.get(&field.name)) {
            (Type::Text, Some(Value::Text(s))) => {
                field.range.map_or(true, |(_, max)| s.len() as i64 <= max)
            }
            (Type::Number, Some(Value::Number(n))) => field
                .range
                .map_or(true, |(min, max)| min <= *n && *n <= max),
            _ => false,
        };
        if !valid {
            return Err(error(format!(
                "pipeline input field '{}' violates its declared type or bound",
                field.name
            )));
        }
    }
    Ok(())
}
