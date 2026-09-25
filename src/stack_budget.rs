//! Source-selected ceilings for checked argv entries and bounded HTTP services.
//! Placement comes from the emitter, not an independent AST size estimate.
use crate::ast::*;
use crate::verifier::VerifyError;
use std::collections::BTreeSet;
pub(crate) mod http;

/// Possible pre-existing owners, counted once each, not extra frame storage.
/// Exclusive alternatives can share addresses while both contribute capacity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallStorage {
    pub callee: String,
    pub parent_call: Option<usize>,
    pub live_caller_buffer_capacity_bytes: usize,
    pub retained_caller_buffer_capacity_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextFrame {
    pub slot_bytes: usize,
    pub buffer_bytes: usize,
    pub saved_register_bytes: usize,
    pub calls: Vec<CallStorage>,
}

impl TextFrame {
    pub fn frame_bytes(&self) -> usize {
        self.slot_bytes + self.buffer_bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub rule: String,
    pub declared_bytes: Option<u32>,
    pub input_slot_bytes: usize,
    pub shared_slot_bytes: usize,
    pub bookkeeping_bytes: usize,
    pub frame_bytes: usize,
    pub saved_base_pointer_bytes: usize,
    pub input_stack_bytes: usize,
    pub expression_stack_bytes: usize,
    pub output_stack_bytes: usize,
    pub text_frame: Option<TextFrame>,
}

impl Report {
    pub fn transient_stack_bytes(&self) -> usize {
        let nested = self
            .text_frame
            .as_ref()
            .map_or(0, |t| t.frame_bytes() + t.saved_register_bytes);
        self.input_stack_bytes
            .max(nested + self.expression_stack_bytes.max(self.output_stack_bytes))
    }

    pub fn stack_bound_bytes(&self) -> usize {
        // Entry guards finish before the nested text frame is opened. Text
        // construction and output keep that frame live; bool output uses no
        // scratch after closing it. Numeric entries have no nested frame.
        self.saved_base_pointer_bytes + self.frame_bytes + self.transient_stack_bytes()
    }

    pub fn json(&self) -> String {
        // Additive schema-1 extension. Numeric reports remain byte-identical.
        let text_frame = self.text_frame.as_ref().map_or(String::new(), |t| {
            let calls = if t.calls.is_empty() { String::new() } else {
                format!(",\"calls\":[{}]", t.calls.iter().enumerate().map(|(index, c)| format!(
                    "{{\"call\":{},\"callee\":\"{}\",\"parent_call\":{},\"live_caller_buffer_capacity_bytes\":{},\"retained_caller_buffer_capacity_bytes\":{}}}",
                    index + 1, c.callee, c.parent_call.map_or("null".into(), |n| n.to_string()),
                    c.live_caller_buffer_capacity_bytes, c.retained_caller_buffer_capacity_bytes
                )).collect::<Vec<_>>().join(","))
            };
            format!(",\"text_frame\":{{\"frame_bytes\":{},\"slot_bytes\":{},\"buffer_bytes\":{},\"saved_register_bytes\":{}{}}}",
                t.frame_bytes(), t.slot_bytes, t.buffer_bytes, t.saved_register_bytes, calls)
        });
        // Rule names are lexer identifiers, never arbitrary source strings.
        format!(
            concat!(
                "{{\"schema_version\":1,\"target\":\"x86_64-linux\",",
                "\"entry_mode\":\"argv\",\"scope\":\"additional_entry_stack\",",
                "\"rule\":\"{}\",\"declared_bytes\":{},\"stack_bound_bytes\":{},",
                "\"frame_bytes\":{},\"input_slot_bytes\":{},\"shared_slot_bytes\":{},",
                "\"bookkeeping_bytes\":{},\"saved_base_pointer_bytes\":{},",
                "\"input_stack_bytes\":{},",
                "\"expression_stack_bytes\":{},\"output_stack_bytes\":{}{}}}"
            ),
            self.rule,
            self.declared_bytes.map_or("null".into(), |n| n.to_string()),
            self.stack_bound_bytes(),
            self.frame_bytes,
            self.input_slot_bytes,
            self.shared_slot_bytes,
            self.bookkeeping_bytes,
            self.saved_base_pointer_bytes,
            self.input_stack_bytes,
            self.expression_stack_bytes,
            self.output_stack_bytes,
            text_frame,
        )
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "native stack: rule '{}' (x86_64-linux, argv)", self.rule)?;
        writeln!(
            f,
            "  additional stack bound: {} bytes",
            self.stack_bound_bytes()
        )?;
        if let Some(limit) = self.declared_bytes {
            writeln!(f, "  declared limit: {limit} bytes (verified)")?;
        }
        writeln!(
            f,
            "  fixed frame: {} bytes (inputs {}, shared locals/scratch {}, bookkeeping {})",
            self.frame_bytes, self.input_slot_bytes, self.shared_slot_bytes, self.bookkeeping_bytes
        )?;
        writeln!(f, "  saved base pointer: {} bytes; transient input/expression/output: {}/{}/{} bytes (maximum, not sum)",
            self.saved_base_pointer_bytes, self.input_stack_bytes, self.expression_stack_bytes, self.output_stack_bytes)?;
        if let Some(t) = &self.text_frame {
            writeln!(f, "  nested text frame: {} bytes (slots {}, placed buffers {}), saved registers {} bytes",
                t.frame_bytes(), t.slot_bytes, t.buffer_bytes, t.saved_register_bytes)?;
            writeln!(f, "  peak = entry frame + saved base pointer + max(input, nested frame + saved registers + max(expression, output))")?;
            if !t.calls.is_empty() {
                writeln!(f, "  expanded calls: possible caller buffer capacities (included in the frame, not additive)")?;
                for (index, call) in t.calls.iter().enumerate() {
                    writeln!(f, "    call {} '{}', parent {}: {} bytes live at entry, {} retained through return",
                        index + 1, call.callee, call.parent_call.map_or("entry".into(), |n| n.to_string()),
                        call.live_caller_buffer_capacity_bytes, call.retained_caller_buffer_capacity_bytes)?;
                }
            }
        }
        write!(
            f,
            "  excludes initial argv/environment storage, OS/kernel memory and interpreter storage"
        )
    }
}

/// A sequence is an execution selection, not a new source-level budget. Each
/// declaration still bounds its own argv phase, including expanded callees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SequenceReport {
    pub phases: Vec<Report>,
}

impl SequenceReport {
    pub fn stack_bound_bytes(&self) -> usize {
        self.phases
            .iter()
            .map(Report::stack_bound_bytes)
            .max()
            .unwrap_or(0)
    }

    pub fn json(&self) -> String {
        format!(
            concat!(
                "{{\"schema_version\":1,\"target\":\"x86_64-linux\",",
                "\"entry_mode\":\"argv\",\"scope\":\"additional_entry_stack\",",
                "\"composition\":\"sequential\",\"on_phase_failure\":\"stop\",",
                "\"retained_stack_bytes\":0,\"stack_bound_bytes\":{},\"phases\":[{}]}}"
            ),
            self.stack_bound_bytes(),
            self.phases
                .iter()
                .map(Report::json)
                .collect::<Vec<_>>()
                .join(",")
        )
    }
}

impl std::fmt::Display for SequenceReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "native stack: {} sequential argv phases (x86_64-linux)",
            self.phases.len()
        )?;
        writeln!(
            f,
            "  additional stack bound: {} bytes (maximum of phases)",
            self.stack_bound_bytes()
        )?;
        writeln!(
            f,
            "  retained stack between phases: 0 bytes; stop on phase failure"
        )?;
        for (index, phase) in self.phases.iter().enumerate() {
            writeln!(f, "phase {}:\n{phase}", index + 1)?;
        }
        Ok(())
    }
}

pub fn has_declarations(p: &Program) -> bool {
    p.items
        .iter()
        .any(|i| match i {
            Item::Rule(r) => r.proofs.native_stack.is_some(),
            Item::Service(s) => s.native_stack.is_some(),
            _ => false,
        })
}

fn calls(e: &Expr, out: &mut BTreeSet<String>) {
    if let Expr::Call(name, _) = e {
        out.insert(name.clone());
    }
    crate::verifier::walk_expr_children(e, &mut |child| calls(child, out));
}

/// Entries that reach a declaration. A helper's standalone argv budget does
/// not describe a transport wrapper's storage. Unrelated entries stay usable.
pub(crate) fn entry_rules(p: &Program) -> BTreeSet<String> {
    let mut active = BTreeSet::new();
    if !has_declarations(p) {
        return active;
    }
    let mut edges = Vec::new();
    for item in &p.items {
        if let Item::Rule(r) = item {
            if r.proofs.native_stack.is_some() {
                active.insert(r.name.clone());
            }
            let mut deps = BTreeSet::new();
            for (_, e) in &r.logic.bindings {
                calls(e, &mut deps);
            }
            calls(&r.logic.value, &mut deps);
            edges.push((&r.name, deps));
        }
    }
    loop {
        let before = active.len();
        for (name, deps) in &edges {
            if !active.is_disjoint(deps) {
                active.insert((*name).clone());
            }
        }
        if before == active.len() {
            return active;
        }
    }
}

fn context_errors(p: &Program) -> Vec<VerifyError> {
    let active = entry_rules(p);
    let mut errors = Vec::new();
    for item in &p.items {
        let mut names = BTreeSet::new();
        let (context, effects): (String, Vec<&Effect>) = match item {
            Item::Service(s) => {
                names.insert(s.handler.clone());
                for set in &s.after_sets {
                    calls(&set.value, &mut names);
                }
                (
                    format!("service '{}'", s.name),
                    s.logs.iter().map(|l| &l.effect).collect(),
                )
            }
            Item::Reaction(r) => {
                names.insert(r.trigger.clone());
                (format!("reaction '{}'", r.name), r.effects.iter().collect())
            }
            _ => continue,
        };
        for effect in effects {
            match effect {
                Effect::Print(args) => {
                    for e in args {
                        calls(e, &mut names);
                    }
                }
                Effect::AppendFile { content, .. } => calls(content, &mut names),
            }
        }
        if let Some(name) = active.intersection(&names).next() {
            errors.push(VerifyError {
                context: format!("{context} / proofs.native_stack"),
                message: format!("'{name}' reaches a native_stack declaration; this contract supports pure native argv entries only, not service or reaction contexts"),
            });
        }
    }
    errors
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    if !has_declarations(p) {
        return vec![];
    }
    let errors = context_errors(p);
    if !errors.is_empty() {
        return errors;
    }
    // Public backend APIs also use this gate, independently of the CLI verifier.
    // The emitter must never see recursion or unverified numeric expressions.
    let errors = crate::numeric_bounds::verify(p);
    if !errors.is_empty() {
        return errors;
    }
    let mut errors: Vec<_> = p.items.iter().filter_map(|i| {
        let Item::Rule(r) = i else { return None };
        let limit = r.proofs.native_stack?;
        let message = if !(1..=2_097_152).contains(&limit) {
            Some("native_stack must be in [1, 2097152] bytes".into())
        } else {
            match crate::native::stack_report(p, &r.name) {
                Ok(report) if report.stack_bound_bytes() <= limit as usize => None,
                Ok(report) => Some(format!(
                    "native argv stack bound {} bytes exceeds declared {limit} bytes (frame {}, saved base pointer {}, transient {}); includes expanded callees",
                    report.stack_bound_bytes(), report.frame_bytes,
                    report.saved_base_pointer_bytes,
                    report.transient_stack_bytes())),
                Err(e) => Some(format!("native stack analysis unavailable: {}", e.message)),
            }
        };
        message.map(|message| VerifyError {
            context: format!("rule '{}' / proofs.native_stack", r.name),
            message,
        })
    }).collect();
    errors.extend(http::verify(p));
    errors
}

#[cfg(test)]
mod tests;
