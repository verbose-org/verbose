//! Checked argv phases with no storage or values retained between phases.
//! Both compilation and reporting use this lowering and its structured exits.
use super::*;
use crate::stack_budget::SequenceReport;

const MAX_PHASES: usize = 64;
const MAX_CODE_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn uses_checked_entry(p: &Program, names: &[&str]) -> bool {
    let mut active = crate::stack_budget::entry_rules(p);
    active.extend(crate::numeric_bounds::active_rules(p));
    active.extend(crate::text_bounds::active_rules(p));
    names.iter().any(|n| active.contains(*n))
}

pub(super) fn compile(p: &Program, names: &[&str]) -> Result<Vec<u8>, NativeError> {
    Ok(prepare(p, names)?.0)
}

pub(super) fn report(p: &Program, names: &[&str]) -> Result<SequenceReport, NativeError> {
    Ok(prepare(p, names)?.1)
}

fn error(message: impl Into<String>) -> NativeError {
    NativeError {
        message: format!("native_stack sequential phases: {}", message.into()),
    }
}

fn prepare(p: &Program, names: &[&str]) -> Result<(Vec<u8>, SequenceReport), NativeError> {
    if !(2..=MAX_PHASES).contains(&names.len()) {
        return Err(error(format!("expected 2..={MAX_PHASES} phases")));
    }
    for errors in [
        crate::stack_budget::verify(p),
        crate::numeric_bounds::verify(p),
        crate::text_bounds::verify_mode(p, true),
    ] {
        if let Some(e) = errors.first() {
            return Err(error(e.to_string()));
        }
    }
    let rules: HashMap<_, _> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let text = crate::text_bounds::active_rules(p);
    let mut input = None;
    let mut code = Vec::new();
    let mut phases = Vec::new();
    for (index, name) in names.iter().enumerate() {
        let r = *rules
            .get(name)
            .ok_or_else(|| error(format!("phase {}: no rule named '{name}'", index + 1)))?;
        if input.as_ref().is_some_and(|ty| ty != &r.input_ty) {
            return Err(error(format!(
                "phase {} ('{name}'): phases must use the same input concept",
                index + 1
            )));
        }
        input = Some(r.input_ty.clone());
        let end = if index + 1 == names.len() {
            EntryEnd::Exit
        } else {
            EntryEnd::NextPhase
        };
        let phase = if text.contains(*name) {
            let concept = iter_all_concepts(&p.items)
                .find(|c| r.input_ty == Type::Named(c.name.clone()))
                .ok_or_else(|| {
                    error(format!(
                        "phase {} ('{name}'): missing declared input concept",
                        index + 1
                    ))
                })?;
            bounded_text::compile_phase(p, r, concept, end)
        } else {
            bounded::compile_phase(p, name, end)
        };
        let (bytes, report) =
            phase.map_err(|e| error(format!("phase {} ('{name}'): {}", index + 1, e.message)))?;
        if bytes.len() > MAX_CODE_BYTES - code.len() {
            return Err(error("combined code exceeds 64 MiB"));
        }
        code.extend_from_slice(&bytes);
        phases.push(report);
    }
    Ok((code, SequenceReport { phases }))
}
