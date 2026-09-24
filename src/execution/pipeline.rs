//! Per-record composition lowered to existing immutable calls and one frame.
use super::*;
use std::collections::{BTreeSet, HashMap};

/// Preserve the source AST of all pipeline stages and their callees, even when
/// another entry is selected. Legacy rewrites do not carry the frame contract.
pub(crate) fn participating(p: &Program) -> BTreeSet<String> {
    let mut active: BTreeSet<_> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Execution(e) if matches!(e.mode, ExecutionMode::Pipeline { .. }) => {
                Some(&e.phases)
            }
            _ => None,
        })
        .flatten()
        .cloned()
        .collect();
    if active.is_empty() {
        return active;
    }
    let rules: HashMap<_, _> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    fn calls(expr: &Expr, names: &mut Vec<String>) {
        if let Expr::Call(name, _) = expr {
            names.push(name.clone());
        }
        crate::verifier::walk_expr_children(expr, &mut |child| calls(child, names));
    }
    let mut pending: Vec<_> = active.iter().cloned().collect();
    while let Some(name) = pending.pop() {
        if let Some(rule) = rules.get(name.as_str()) {
            let mut names = Vec::new();
            for (_, expr) in &rule.logic.bindings {
                calls(expr, &mut names);
            }
            calls(&rule.logic.value, &mut names);
            for callee in names {
                if active.insert(callee.clone()) {
                    pending.push(callee);
                }
            }
        }
    }
    active
}

fn native_error(message: impl Into<String>) -> NativeError {
    NativeError {
        message: message.into(),
    }
}

/// Called only after execution shape checks. Internal orchestration is not an
/// author-supplied rule/proof; the original rule proofs are verified unchanged.
fn lower(p: &Program, e: &Execution) -> Program {
    let last = p
        .items
        .iter()
        .find_map(|item| match item {
            Item::Rule(r) if Some(&r.name) == e.phases.last() => Some(r),
            _ => None,
        })
        .expect("execution shape checks resolved every phase");
    let mut value = Expr::Ident("pipeline_input".into());
    let mut bindings = Vec::new();
    for (index, phase) in e.phases.iter().enumerate() {
        value = Expr::Call(phase.clone(), vec![value]);
        if index + 1 < e.phases.len() {
            let name = format!("pipeline_stage_{index}");
            bindings.push((name.clone(), value));
            value = Expr::Ident(name);
        }
    }
    let wrapper = Rule {
        name: e.name.clone(),
        intention: e.intention.clone(),
        source: e.source.clone(),
        input_name: "pipeline_input".into(),
        input_ty: Type::Named(e.input.clone()),
        output_name: "out".into(),
        output_ty: last.output_ty.clone(),
        output_text_max: last.output_text_max,
        logic: LogicStmt {
            bindings,
            target: "out".into(),
            value,
        },
        // No fabricated source proof. Verification below checks the actual
        // expanded composition, not these internal bookkeeping fields.
        proofs: Proofs {
            purity: Purity {
                reads: vec![],
                calls: vec![],
            },
            termination: Termination {
                bound: None,
                structural: None,
                decreasing: None,
                increasing: None,
            },
            native_stack: None,
        },
        hints: None,
        layer: None,
        context_name: None,
        context_ty: None,
    };
    let mut lowered = p.clone();
    lowered.items.retain(|i| !matches!(i, Item::Execution(_)));
    lowered.items.push(Item::Rule(wrapper));
    lowered
}

pub(crate) fn prepare(p: &Program, e: &Execution) -> Result<(Vec<u8>, Report), NativeError> {
    let ExecutionMode::Pipeline { native_stack } = e.mode else {
        return Err(native_error("expected pipeline execution"));
    };
    // Preserve all pre-existing source contracts, including independent budgets
    // on unselected rules. Never reinterpret a phase's standalone stack limit.
    for errors in [
        crate::stack_budget::verify(p),
        crate::numeric_bounds::verify(p),
        crate::text_bounds::verify_mode(p, true),
    ] {
        if let Some(error) = errors.first() {
            return Err(native_error(error.to_string()));
        }
    }
    let lowered = lower(p, e);
    crate::text_bounds::verify_entry(&lowered, &e.name)
        .map_err(|message| native_error(format!("pipeline composition unavailable: {message}")))?;
    let (code, invocation) = native::pipeline_layout(&lowered, &e.name)?;
    if code.len() > 64 * 1024 * 1024 {
        return Err(native_error("pipeline code exceeds 64 MiB"));
    }
    if invocation.stack_bound_bytes() > native_stack as usize {
        return Err(native_error(format!(
            "pipeline native argv stack bound {} bytes exceeds declared {native_stack} bytes (shared invocation including retained values)",
            invocation.stack_bound_bytes()
        )));
    }
    Ok((
        code,
        Report {
            name: e.name.clone(),
            input: e.input.clone(),
            phases: e.phases.clone(),
            declared_bytes: native_stack,
            invocation,
        },
    ))
}

#[derive(Debug)]
pub struct Report {
    pub name: String,
    pub input: String,
    pub phases: Vec<String>,
    pub declared_bytes: u32,
    pub invocation: crate::stack_budget::Report,
}
impl Report {
    pub fn json(&self) -> String {
        format!(
            concat!(
            "{{\"schema_version\":1,\"target\":\"x86_64-linux\",\"entry_mode\":\"argv\",",
            "\"scope\":\"additional_entry_stack\",\"composition\":\"pipeline\",",
            "\"execution\":\"{}\",\"input_concept\":\"{}\",\"declared_bytes\":{},",
            "\"stack_bound_bytes\":{},\"record_order\":\"input\",\"publication\":\"final_phase\",",
            "\"on_failure\":\"stop\",\"boolean_failure\":\"after_batch\",",
            "\"retained_storage\":\"included_in_invocation\",\"phases\":[{}],\"invocation\":{}}}"),
            self.name,
            self.input,
            self.declared_bytes,
            self.invocation.stack_bound_bytes(),
            self.phases
                .iter()
                .map(|n| format!("\"{n}\""))
                .collect::<Vec<_>>()
                .join(","),
            self.invocation.json()
        )
    }
}
impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "pipeline '{}' over '{}': {} (one record at a time, final result only)",
            self.name,
            self.input,
            self.phases.join(" -> ")
        )?;
        writeln!(f, "  declared stack limit: {} bytes (verified); retained values included in the shared invocation", self.declared_bytes)?;
        self.invocation.fmt(f)
    }
}

#[cfg(test)]
mod tests;
