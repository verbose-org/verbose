//! Source-selected ceilings for the strict numeric native argv entry.
//! Placement comes from the emitter, not an independent AST size estimate.
use crate::ast::*;
use crate::verifier::VerifyError;

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
}

impl Report {
    pub fn transient_stack_bytes(&self) -> usize {
        self.input_stack_bytes
            .max(self.expression_stack_bytes)
            .max(self.output_stack_bytes)
    }

    pub fn stack_bound_bytes(&self) -> usize {
        // Input guards, expression spills and result formatting run separately.
        self.saved_base_pointer_bytes + self.frame_bytes + self.transient_stack_bytes()
    }

    pub fn json(&self) -> String {
        // Rule names are lexer identifiers, never arbitrary source strings.
        format!(
            concat!(
                "{{\"schema_version\":1,\"target\":\"x86_64-linux\",",
                "\"entry_mode\":\"argv\",\"scope\":\"additional_entry_stack\",",
                "\"rule\":\"{}\",\"declared_bytes\":{},\"stack_bound_bytes\":{},",
                "\"frame_bytes\":{},\"input_slot_bytes\":{},\"shared_slot_bytes\":{},",
                "\"bookkeeping_bytes\":{},\"saved_base_pointer_bytes\":{},",
                "\"input_stack_bytes\":{},",
                "\"expression_stack_bytes\":{},\"output_stack_bytes\":{}}}"
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
        write!(
            f,
            "  excludes initial argv/environment storage, OS/kernel memory and interpreter storage"
        )
    }
}

pub fn has_declarations(p: &Program) -> bool {
    p.items
        .iter()
        .any(|i| matches!(i, Item::Rule(r) if r.proofs.native_stack.is_some()))
}

pub fn verify(p: &Program) -> Vec<VerifyError> {
    if !has_declarations(p) {
        return vec![];
    }
    // Public backend APIs also use this gate, independently of the CLI verifier.
    // The emitter must never see recursion or unverified numeric expressions.
    let errors = crate::numeric_bounds::verify(p);
    if !errors.is_empty() {
        return errors;
    }
    p.items.iter().filter_map(|i| {
        let Item::Rule(r) = i else { return None };
        let limit = r.proofs.native_stack?;
        let message = if !(1..=2_097_152).contains(&limit) {
            Some("native_stack must be in [1, 2097152] bytes".into())
        } else {
            match crate::native::numeric_stack_report(p, &r.name) {
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
    }).collect()
}

#[cfg(test)]
mod tests;
