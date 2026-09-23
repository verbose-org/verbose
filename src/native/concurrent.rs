//! Fixed Linux thread lanes for source-declared pure concurrent argv phases.
//! The same prepared bodies and lane layout drive reporting and emission.
use super::*;
mod emit;
mod layout;
#[cfg(test)]
mod tests;
pub(crate) use layout::Report;

pub(super) struct Body {
    pub code: Vec<u8>,
    pub frame_bytes: usize,
    pub stack_bytes: usize,
    pub output_bytes: usize,
}
fn error(message: impl Into<String>) -> NativeError {
    NativeError {
        message: format!("native concurrent execution: {}", message.into()),
    }
}
struct Prepared<'a> {
    report: Report,
    bodies: Vec<Body>,
    concept: &'a Concept,
}
fn prepare<'a>(p: &'a Program, e: &Execution) -> Result<Prepared<'a>, NativeError> {
    let ExecutionMode::Concurrent {
        max_in_flight,
        native_memory,
    } = e.mode
    else {
        return Err(error("--memory-report requires a concurrent execution"));
    };
    if !(1..=64).contains(&max_in_flight) {
        return Err(error("max_in_flight must be in [1, 64]"));
    }
    if native_memory.is_some_and(|n| !(1..=268_435_456).contains(&n)) {
        return Err(error("native_memory must be in [1, 268435456] bytes"));
    }
    let names: Vec<_> = e.phases.iter().map(String::as_str).collect();
    // This gate retains the original closed source subset and standalone budgets.
    // Its sequential layout is not used as a concurrent memory estimate.
    sequential::report(p, &names)?;
    let concept = iter_all_concepts(&p.items)
        .find(|c| c.name == e.input)
        .ok_or_else(|| error("missing input concept"))?;
    if concept.fields.is_empty() {
        return Err(error("input requires at least one field"));
    }
    let text = crate::text_bounds::active_rules(p);
    let mut bodies = Vec::new();
    for name in &e.phases {
        let rule = p
            .items
            .iter()
            .find_map(|i| match i {
                Item::Rule(r) if r.name == *name => Some(r),
                _ => None,
            })
            .ok_or_else(|| error(format!("missing phase '{name}'")))?;
        let body = if text.contains(name) {
            bounded_text::worker_body(p, rule, concept)
        } else {
            bounded::worker_body(p, rule, concept)
        }
        .map_err(|cause| error(format!("phase '{name}': {}", cause.message)))?;
        bodies.push(body);
    }
    let report = layout::plan(e, &bodies)?;
    if native_memory.is_some_and(|n| report.reserved_bytes > n as usize) {
        return Err(error(format!(
            "reserved memory {} bytes exceeds declared {} bytes",
            report.reserved_bytes,
            native_memory.unwrap()
        )));
    }
    Ok(Prepared {
        report,
        bodies,
        concept,
    })
}
pub(super) fn report(p: &Program, e: &Execution) -> Result<Report, NativeError> {
    Ok(prepare(p, e)?.report)
}
pub(super) fn compile(p: &Program, e: &Execution) -> Result<Vec<u8>, NativeError> {
    let prepared = prepare(p, e)?;
    if prepared.report.declared_bytes.is_none() {
        return Err(error("native_memory is required for native emission; use --memory-report to inspect the reservation"));
    }
    Ok(emit::compile(&prepared)?.code)
}

// rbx is the bounded output cursor. The existing numeric formatter preserves
// r12-r15 and restores its 24-byte scratch before returning.
pub(super) fn literal(code: &mut Vec<u8>, bytes: &[u8]) {
    // Inline stores avoid a second copy of small punctuation. Long field names
    // remain bounded by the source/code limit, not an unbounded runtime write.
    for &b in bytes {
        code.extend_from_slice(&[0xc6, 0x03, b, 0x48, 0xff, 0xc3]);
    }
}
pub(super) fn output_start(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x49, 0x8b, 0x5f, 24]); // rbx = lane buffer
}
pub(super) fn output_end(code: &mut Vec<u8>) {
    code.extend_from_slice(&[0x48, 0x89, 0xd8, 0x49, 0x2b, 0x47, 24, 0x49, 0x89, 0x47, 8]);
}
pub(super) fn scalar_output(code: &mut Vec<u8>, ty: &Type, sticky: i32) {
    output_start(code);
    if *ty == Type::Bool {
        code.extend_from_slice(&[0x48, 0x85, 0xc0, 0x0f, 0x84]);
        let false_jump = code.len();
        code.extend_from_slice(&[0; 4]);
        literal(code, b"true\n");
        code.push(0xe9);
        let done_jump = code.len();
        code.extend_from_slice(&[0; 4]);
        let target = code.len();
        code[false_jump..false_jump + 4]
            .copy_from_slice(&((target - false_jump - 4) as i32).to_le_bytes());
        code.extend_from_slice(&[0x48, 0xc7, 0x85]);
        code.extend_from_slice(&sticky.to_le_bytes());
        code.extend_from_slice(&1i32.to_le_bytes());
        literal(code, b"false\n");
        let target = code.len();
        code[done_jump..done_jump + 4]
            .copy_from_slice(&((target - done_jump - 4) as i32).to_le_bytes());
    } else {
        emit_itoa_to_buffer(code);
        literal(code, b"\n");
    }
    output_end(code);
}
