//! Reference semantics for source executions, using the original verified AST.
//! Input records remain available to every phase; completed results are streamed.
mod input;
#[cfg(test)]
mod tests;

use super::{eval_rule, RuntimeError, Value};
use crate::ast::*;
use std::collections::HashMap;
use std::io::{Read, Write};

fn error(message: impl Into<String>) -> RuntimeError {
    RuntimeError {
        message: message.into(),
    }
}

/// False is an ordinary result until the complete boolean phase has finished.
/// Evaluation and output errors instead return immediately, preserving the
/// already delivered prefix. Indices are one-based phases, zero-based records.
pub(super) fn run(
    program: &Program,
    name: &str,
    records: &[HashMap<String, Value>],
    mut emit: impl FnMut(usize, &Rule, usize, Value) -> Result<(), RuntimeError>,
) -> Result<i32, RuntimeError> {
    // This is the same closed contract as native execution, not a second,
    // more permissive interpretation of source budgets or unsupported phases.
    crate::execution::report(program, name).map_err(|e| error(e.message))?;
    let execution = crate::execution::find(program, name).unwrap();
    if records.is_empty() {
        return Err(error(format!(
            "execution '{name}': expected at least one input record"
        )));
    }
    let rules: Vec<_> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&program.items).collect();
    for (phase, rule_name) in execution.phases.iter().enumerate() {
        let rule = rules.iter().find(|r| r.name == *rule_name).unwrap();
        let mut failed = false;
        for (index, record) in records.iter().enumerate() {
            let context = |e: RuntimeError| {
                error(format!(
                    "execution '{name}', phase {} ('{rule_name}'), record {index}: {}",
                    phase + 1,
                    e.message
                ))
            };
            let value = eval_rule(rule, &rules, &concepts, &[], record).map_err(context)?;
            failed |= matches!(value, Value::Bool(false));
            emit(phase + 1, rule, index, value).map_err(context)?;
        }
        if failed {
            return Ok(1);
        }
    }
    Ok(0)
}

/// Unlike the legacy rule reader, this entry reads stdin directly and accepts
/// only a well-formed flat-record JSON document. No shared temporary file.
pub fn cli(program: &Program, name: &str, path: Option<&str>, stdin: bool, json: bool) -> i32 {
    let input = if stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map(|_| text)
    } else {
        let Some(path) = path else {
            eprintln!("--run requires --input <file> or --stdin");
            return 2;
        };
        std::fs::read_to_string(path)
    };
    let records = input
        .map_err(|e| error(format!("cannot read execution input: {e}")))
        .and_then(|s| input::parse(&s));
    let records = match records {
        Ok(records) => records,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match write(program, name, &records, json, &mut std::io::stdout().lock()) {
        Ok(status) => status,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn write(
    program: &Program,
    name: &str,
    records: &[HashMap<String, Value>],
    json: bool,
    output: &mut impl Write,
) -> Result<i32, RuntimeError> {
    let io_error = |e| error(format!("cannot write execution output: {e}"));
    // CLI source verification precedes this function. Delay the opening
    // bracket until the first event, then close even on runtime failure so
    // the completed prefix remains readable JSON.
    let mut first = true;
    let result = run(program, name, records, |phase, rule, record, value| {
        if json {
            write!(
                output,
                "{}{{\"phase\":{phase},\"rule\":\"{}\",\"record\":{record},\"value\":",
                if first { "[" } else { "," },
                rule.name
            )
            .map_err(io_error)?;
            json_value(output, &value).map_err(io_error)?;
            output.write_all(b"}").map_err(io_error)?;
        } else {
            native_value(output, program, &rule.output_ty, &value)?;
        }
        first = false;
        output.flush().map_err(io_error)
    });
    // CLI source verification precedes this function. A runtime error before
    // the first result has an empty completed prefix; it is not a success.
    if json {
        output
            .write_all(if first { b"[]\n" } else { b"]\n" })
            .map_err(io_error)?;
    }
    output.flush().map_err(io_error)?;
    result
}

fn native_value(
    out: &mut impl Write,
    p: &Program,
    ty: &Type,
    v: &Value,
) -> Result<(), RuntimeError> {
    let io_error = |e| error(format!("cannot write execution output: {e}"));
    match (ty, v) {
        (Type::Number, Value::Number(_))
        | (Type::Bool, Value::Bool(_))
        | (Type::Text, Value::Text(_)) => writeln!(out, "{v}").map_err(io_error),
        (Type::Named(name), Value::Record(fields)) => {
            let concept = iter_all_concepts(&p.items)
                .find(|c| c.name == *name)
                .ok_or_else(|| error("execution result concept is missing"))?;
            // Match existing native record output: declaration order and raw
            // text between quotes. --json below provides escaped JSON instead.
            for (index, field) in concept.fields.iter().enumerate() {
                write!(
                    out,
                    "{}\"{}\":",
                    if index == 0 { "{" } else { "," },
                    field.name
                )
                .map_err(io_error)?;
                match fields.get(&field.name) {
                    Some(Value::Number(n)) => write!(out, "{n}").map_err(io_error)?,
                    Some(Value::Text(s)) => write!(out, "\"{s}\"").map_err(io_error)?,
                    _ => return Err(error("unsupported execution result field")),
                }
            }
            out.write_all(b"}\n").map_err(io_error)
        }
        _ => Err(error("unsupported execution result type")),
    }
}

fn json_string(out: &mut impl Write, text: &str) -> std::io::Result<()> {
    out.write_all(b"\"")?;
    for c in text.chars() {
        match c {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\u{0}'..='\u{1f}' => write!(out, "\\u{:04x}", c as u32)?,
            _ => write!(out, "{c}")?,
        }
    }
    out.write_all(b"\"")
}

fn json_value(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    match value {
        Value::Number(n) => write!(out, "{n}"),
        Value::Bool(b) => write!(out, "{b}"),
        Value::Text(s) => json_string(out, s),
        Value::Record(fields) => {
            out.write_all(b"{")?;
            let mut keys: Vec<_> = fields.keys().collect();
            keys.sort();
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.write_all(b",")?;
                }
                json_string(out, key)?;
                out.write_all(b":")?;
                json_value(out, &fields[key])?;
            }
            out.write_all(b"}")
        }
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported execution result",
        )),
    }
}
