//! Stack bounds for the existing closed service-log emitter, not a new lowering.
use super::*;
use crate::stack_budget::http::Log;

pub(super) fn report(
    index: usize, policy: ErrorPolicy, content: &Expr, input_name: &str,
    concept: &Concept, text: &TextBindings<'_>, offsets: &HashMap<&str, i32>,
    response_capacity: usize,
) -> Result<Log, NativeError> {
    let error = || NativeError { message: format!("bounded HTTP log[{index}] stack layout is unknown") };
    let mut report = Log {
        index, on_error: match policy { ErrorPolicy::Abort => "abort", ErrorPolicy::Drop => "drop" },
        strategy: "literal", content_capacity_bytes: 0, buffer_bytes: 0,
        sizing_stack_bytes: 0, formatting_stack_bytes: 0,
    };
    if let Expr::Text(s) = content {
        report.content_capacity_bytes = s.len();
        return Ok(report);
    }
    let Expr::Concat(args) = content else { return Err(error()); };
    // This is the same classification/static allowance used by the emitter.
    // The bounded-text verifier has already closed the effect's grammar/types.
    let layout = concat_layout(args, input_name, concept, text, offsets, false)?;
    if layout.n_calls != 0 { return Err(error()); }
    report.strategy = if layout.has_dynamic { "dynamic" } else { "static" };
    let mut buffer = usize::try_from(layout.static_total).map_err(|_| error())?;
    for (arg, kind) in args.iter().zip(&layout.kinds) {
        let capacity = match (arg, kind) {
            (Expr::Text(s), ConcatArgKind::Text) => s.len(),
            (Expr::Number(_), ConcatArgKind::Number) => 20,
            (Expr::Field(base, field), ConcatArgKind::Number)
                if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                    && matches!(field.as_str(), "__resp_status" | "__req_timestamp") => 20,
            (Expr::Ident(n), ConcatArgKind::BoundText) if n == "__resp_body" => response_capacity,
            (Expr::Field(base, field), ConcatArgKind::Text | ConcatArgKind::BoundText)
                if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                    && matches!(field.as_str(), "method" | "path" | "body") => {
                concept.fields.iter().find(|f| f.name == *field && f.ty == Type::Text)
                    .and_then(|f| f.range).and_then(|(_, max)| usize::try_from(max).ok()).ok_or_else(error)?
            }
            _ => return Err(error()),
        };
        report.content_capacity_bytes = report.content_capacity_bytes.checked_add(capacity).ok_or_else(error)?;
        if *kind == ConcatArgKind::Number {
            // Saving rbx for the scalar load (8 bytes) ends before itoa's
            // larger staging buffer is opened. Numeric expressions are closed.
            report.formatting_stack_bytes = ITOA_STACK_BYTES as usize;
        }
        if layout.has_dynamic {
            match kind {
                ConcatArgKind::BoundText => buffer = buffer.checked_add(capacity).ok_or_else(error)?,
                ConcatArgKind::Text if matches!(arg, Expr::Field(..)) => {
                    // Existing dynamic sizing adds strlen even when the field
                    // was included in static_total. Preserve and count both.
                    buffer = buffer.checked_add(capacity).ok_or_else(error)?;
                    report.sizing_stack_bytes = 8;
                }
                _ => {}
            }
        }
    }
    report.buffer_bytes = buffer.checked_add(7).ok_or_else(error)? & !7;
    Ok(report)
}
