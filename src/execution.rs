//! Source-owned execution order and resource contracts for pure phases.
//! Sequential stack ceilings and concurrent fixed-memory reservations.
use crate::ast::*;
use crate::native::{self, NativeError};
use crate::stack_budget::SequenceReport;
use crate::verifier::VerifyError;
use std::collections::HashSet;

pub fn find<'a>(p: &'a Program, name: &str) -> Option<&'a Execution> {
    p.items.iter().find_map(|item| match item {
        Item::Execution(e) if e.name == name => Some(e),
        _ => None,
    })
}

pub fn has_declarations(p: &Program) -> bool {
    p.items.iter().any(|i| matches!(i, Item::Execution(_)))
}

pub fn default_entry(p: &Program) -> Option<String> {
    p.items.iter().rev().find_map(|item| match item {
        Item::Execution(e) => Some(e.name.clone()),
        Item::Service(s) => Some(s.name.clone()),
        _ => None,
    })
}

fn error(e: &Execution, message: impl Into<String>) -> VerifyError {
    VerifyError {
        context: format!("execution '{}'", e.name),
        message: message.into(),
    }
}

fn shape_errors(p: &Program) -> Vec<VerifyError> {
    let mut errors = Vec::new();
    let mut seen = HashSet::new();
    for e in p.items.iter().filter_map(|i| {
        if let Item::Execution(e) = i {
            Some(e)
        } else {
            None
        }
    }) {
        if !seen.insert(&e.name) {
            errors.push(error(e, "duplicate execution name"));
        }
        // A name must select the same kind in every CLI/backend entry path.
        let collision = p.items.iter().any(|item| match item {
            Item::Rule(r) => r.name == e.name,
            Item::Service(s) => s.name == e.name,
            Item::Reaction(r) => r.name == e.name,
            Item::Concept(c) => c.name == e.name,
            Item::ConceptGroup(g) => {
                g.name == e.name || g.concepts.iter().any(|c| c.name == e.name)
            }
            Item::Resource(r) => r.name == e.name,
            Item::Connection(c) => c.name == e.name,
            Item::Entropy(r) => r.name == e.name,
            Item::Execution(_) => false,
        });
        if collision || crate::parser::PRIMITIVE_CALL_NAMES.contains(&e.name.as_str()) {
            errors.push(error(
                e,
                "execution name collides with another declaration or a primitive",
            ));
        }
        if e.intention.trim().is_empty() {
            errors.push(error(e, "@intention must not be empty"));
        }
        if !(2..=64).contains(&e.phases.len()) {
            errors.push(error(e, "expected 2..=64 phases"));
        }
        match e.mode {
            ExecutionMode::Sequential { native_stack }
                if !(1..=2_097_152).contains(&native_stack) =>
            {
                errors.push(error(e, "native_stack must be in [1, 2097152] bytes"))
            }
            ExecutionMode::Concurrent { max_in_flight, .. }
                if !(1..=64).contains(&max_in_flight) =>
            {
                errors.push(error(e, "max_in_flight must be in [1, 64]"))
            }
            ExecutionMode::Concurrent { result_batch, .. }
                if !(1..=1024).contains(&result_batch) =>
            {
                errors.push(error(e, "result_batch must be in [1, 1024]"))
            }
            _ => {}
        }
        if !iter_all_concepts(&p.items).any(|c| c.name == e.input) {
            errors.push(error(e, format!("unknown input concept '{}'", e.input)));
        }
        for (index, name) in e.phases.iter().enumerate() {
            let rule = p.items.iter().find_map(|item| match item {
                Item::Rule(r) if r.name == *name => Some(r),
                _ => None,
            });
            match rule {
                None => errors.push(error(e, format!("phase {}: no rule named '{name}'; nested executions and services are not phases", index + 1))),
                Some(r) if r.input_ty != Type::Named(e.input.clone()) => errors.push(error(e,
                    format!("phase {} ('{name}'): input must be concept '{}'", index + 1, e.input))),
                _ => {}
            }
        }
    }
    errors
}

fn report_one(p: &Program, e: &Execution) -> Result<Report, VerifyError> {
    let ExecutionMode::Sequential { native_stack } = e.mode else {
        return Err(error(e, "concurrent execution has no native stack report: use --memory-report for its fixed reservation"));
    };
    let names: Vec<_> = e.phases.iter().map(String::as_str).collect();
    let sequence = native::sequential_stack_report(p, &names)
        .map_err(|cause| error(e, format!("stack analysis unavailable: {}", cause.message)))?;
    if sequence.stack_bound_bytes() > native_stack as usize {
        return Err(error(e, format!("native argv stack bound {} bytes exceeds declared {} bytes (maximum of sequential phases)",
            sequence.stack_bound_bytes(), native_stack)));
    }
    Ok(Report {
        name: e.name.clone(),
        input: e.input.clone(),
        declared_bytes: native_stack,
        sequence,
    })
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    if !has_declarations(p) {
        return Vec::new();
    }
    let errors = shape_errors(p);
    if !errors.is_empty() {
        return errors;
    }
    p.items
        .iter()
        .filter_map(|i| match i {
            Item::Execution(e) => match e.mode {
                ExecutionMode::Sequential { .. } => report_one(p, e).err(),
                ExecutionMode::Concurrent {
                    native_memory: Some(_),
                    ..
                } => native::concurrent_memory_report(p, e)
                    .err()
                    .map(|cause| error(e, cause.message)),
                ExecutionMode::Concurrent { .. } => {
                    // Reuse the existing closed pure phase analysis, including
                    // source rule budgets and the code/expansion limits. The
                    // sequential layout is NOT a concurrent memory estimate.
                    let names: Vec<_> = e.phases.iter().map(String::as_str).collect();
                    native::sequential_stack_report(p, &names)
                        .err()
                        .map(|cause| {
                            error(
                                e,
                                format!("concurrent phase analysis unavailable: {}", cause.message),
                            )
                        })
                }
            },
            _ => None,
        })
        .collect()
}

pub fn gate(p: &Program) -> Result<(), NativeError> {
    match verify(p).first() {
        Some(e) => Err(NativeError {
            message: e.to_string(),
        }),
        None => Ok(()),
    }
}

pub fn report(p: &Program, name: &str) -> Result<Report, NativeError> {
    // Check every declaration, including unselected executions, before reporting
    // success or writing an artifact. No recursive call to the source verifier.
    gate(p)?;
    let e = find(p, name).ok_or_else(|| NativeError {
        message: format!("no execution named '{name}'"),
    })?;
    report_one(p, e).map_err(|e| NativeError {
        message: e.to_string(),
    })
}

#[derive(Debug)]
pub struct Report {
    pub name: String,
    pub input: String,
    pub declared_bytes: u32,
    pub sequence: SequenceReport,
}
impl Report {
    pub fn json(&self) -> String {
        let mut base = self.sequence.json();
        base.pop();
        format!(
            "{base},\"execution\":\"{}\",\"input_concept\":\"{}\",\"declared_bytes\":{}}}",
            self.name, self.input, self.declared_bytes
        )
    }
}
impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "execution '{}' over '{}'; declared stack limit: {} bytes (verified)",
            self.name, self.input, self.declared_bytes
        )?;
        self.sequence.fmt(f)
    }
}

#[cfg(test)]
mod concurrent_tests;
#[cfg(test)]
mod tests;
