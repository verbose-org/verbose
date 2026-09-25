//! Per-process explicit stack storage, from the same HTTP emitter as the binary.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub service: String,
    pub handler: String,
    pub concurrency: &'static str,
    pub declared_bytes: Option<u32>,
    pub frame_bytes: usize,
    pub request_buffer_bytes: usize,
    pub request_metadata_bytes: usize,
    pub io_bookkeeping_bytes: usize,
    pub dispatch_bookkeeping_bytes: usize,
    pub startup_stack_bytes: usize,
    pub handler_frame: TextFrame,
    pub expression_stack_bytes: usize,
    pub response_stack_bytes: usize,
}

impl Report {
    pub fn stack_bound_bytes(&self) -> usize {
        8 + self.frame_bytes + self.startup_stack_bytes.max(
            self.handler_frame.frame_bytes() + self.handler_frame.saved_register_bytes
                + self.expression_stack_bytes.max(self.response_stack_bytes))
    }
    pub fn json(&self) -> String {
        format!(concat!(
            "{{\"schema_version\":1,\"target\":\"x86_64-linux\",",
            "\"entry_mode\":\"http_1_0\",\"scope\":\"additional_service_stack_per_process\",",
            "\"service\":\"{}\",\"handler\":\"{}\",\"concurrency\":\"{}\",",
            "\"declared_bytes\":{},\"stack_bound_bytes\":{},\"frame_bytes\":{},",
            "\"request_buffer_bytes\":{},\"request_metadata_bytes\":{},",
            "\"io_bookkeeping_bytes\":{},\"dispatch_bookkeeping_bytes\":{},",
            "\"saved_base_pointer_bytes\":8,\"startup_stack_bytes\":{},",
            "\"expression_stack_bytes\":{},\"response_stack_bytes\":{},",
            "\"handler_frame\":{{\"frame_bytes\":{},\"slot_bytes\":{},\"buffer_bytes\":{},",
            "\"saved_register_bytes\":{}}},\"retained_response_storage\":\"included_in_handler_frame\"}}"
        ), self.service, self.handler, self.concurrency,
            self.declared_bytes.map_or("null".into(), |n| n.to_string()), self.stack_bound_bytes(),
            self.frame_bytes, self.request_buffer_bytes, self.request_metadata_bytes,
            self.io_bookkeeping_bytes, self.dispatch_bookkeeping_bytes, self.startup_stack_bytes,
            self.expression_stack_bytes, self.response_stack_bytes,
            self.handler_frame.frame_bytes(), self.handler_frame.slot_bytes, self.handler_frame.buffer_bytes,
            self.handler_frame.saved_register_bytes)
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "native stack: service '{}' (x86_64-linux, http_1_0, {})", self.service, self.concurrency)?;
        writeln!(f, "  additional stack bound per process: {} bytes", self.stack_bound_bytes())?;
        if let Some(limit) = self.declared_bytes { writeln!(f, "  declared limit: {limit} bytes")?; }
        writeln!(f, "  fixed service frame: {} bytes (request buffer {}, request/output metadata {}, I/O {}, dispatch {})",
            self.frame_bytes, self.request_buffer_bytes, self.request_metadata_bytes, self.io_bookkeeping_bytes,
            self.dispatch_bookkeeping_bytes)?;
        writeln!(f, "  handler '{}': {} placed bytes (slots {}, buffers {}), saved registers {} bytes; retained through send",
            self.handler, self.handler_frame.frame_bytes(), self.handler_frame.slot_bytes,
            self.handler_frame.buffer_bytes, self.handler_frame.saved_register_bytes)?;
        writeln!(f, "  saved base pointer: 8 bytes; startup/expression/response scratch: {}/{}/{} bytes",
            self.startup_stack_bytes, self.expression_stack_bytes, self.response_stack_bytes)?;
        writeln!(f, "  peak = 8 + service frame + max(startup, handler frame + saved registers + max(expression, response))")?;
        write!(f, "  excludes initial argv/environment, code/static data, OS/kernel and interpreter storage; not total RSS or a pool-wide sum")
    }
}

pub(crate) fn context(p: &Program, s: &Service) -> Result<(), String> {
    if s.protocol != Protocol::Http10 || s.request_timeout.is_none() || s.response_timeout.is_none() {
        return Err("service native_stack requires http_1_0 with both request_timeout and response_timeout".into());
    }
    if let Some(error) = s.http_io_error().or_else(|| s.admission_error()).or_else(|| s.pool_error()) {
        return Err(error.into());
    }
    if s.max_request < 64 { return Err("service native_stack requires max_request >= 64".into()); }
    if !s.logs.is_empty() || !s.state_fields.is_empty() || !s.after_sets.is_empty() {
        return Err("service native_stack does not yet cover logs, state or after mutations".into());
    }
    if s.shutdown_timeout.is_some() {
        return Err("service native_stack does not yet cover shutdown_timeout signal frames".into());
    }
    if !crate::text_bounds::active_rules(p).contains(&s.handler) {
        return Err("service native_stack requires a handler in the pure bounded-text call graph".into());
    }
    Ok(())
}

pub(super) fn verify(p: &Program) -> Vec<VerifyError> {
    p.items.iter().filter_map(|item| {
        let Item::Service(s) = item else { return None };
        let limit = s.native_stack?;
        let checked = if !(1..=2_097_152).contains(&limit) {
            Err("service native_stack must be in [1, 2097152] bytes".into())
        } else {
            crate::native::service_stack_report(p, &s.name).map_err(|e| e.message).and_then(|report| {
                if report.stack_bound_bytes() > limit as usize {
                    Err(format!("native HTTP stack bound {} bytes per process exceeds declared {limit} bytes; includes transport, expanded handler and response storage", report.stack_bound_bytes()))
                } else { Ok(()) }
            })
        };
        checked.err().map(|message| VerifyError { context: format!("service '{}' / native_stack", s.name), message })
    }).collect()
}
