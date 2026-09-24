use super::*;
pub(super) const PAGE: usize = 4096;
pub(super) const HEADER: usize = 64;
pub(super) const CONTROL: usize = 64;
#[derive(Debug)]
pub(crate) struct Phase {
    pub rule: String,
    pub lane: usize,
    pub frame_bytes: usize,
    pub stack_bytes: usize,
    pub output_bytes: usize,
}
#[derive(Debug)]
pub(crate) struct Lane {
    pub control: usize,
    pub output: usize,
    pub output_bytes: usize,
    pub result_bytes: usize,
    pub stack: usize,
    pub stack_bytes: usize,
    pub stack_reserved: usize,
}
#[derive(Debug)]
pub(crate) struct Report {
    pub execution: String,
    pub input: String,
    pub max_in_flight: usize,
    pub result_batch: usize,
    pub declared_bytes: Option<u32>,
    pub control_reserved: usize,
    pub reserved_bytes: usize,
    pub lanes: Vec<Lane>,
    pub phases: Vec<Phase>,
}
fn align(n: usize, a: usize) -> Result<usize, NativeError> {
    n.checked_add(a - 1)
        .map(|v| v & !(a - 1))
        .ok_or_else(|| error("layout size overflow"))
}
pub(super) fn plan(e: &Execution, bodies: &[Body]) -> Result<Report, NativeError> {
    let ExecutionMode::Concurrent {
        max_in_flight,
        native_memory,
        result_batch,
    } = e.mode
    else {
        unreachable!()
    };
    let count = (max_in_flight as usize).min(bodies.len());
    let mut lanes: Vec<_> = (0..count)
        .map(|i| Lane {
            control: HEADER + i * CONTROL,
            output: 0,
            output_bytes: 0,
            result_bytes: 0,
            stack: 0,
            stack_bytes: 0,
            stack_reserved: 0,
        })
        .collect();
    let mut phases = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        let lane = &mut lanes[i % count];
        lane.result_bytes = lane.result_bytes.max(body.output_bytes);
        lane.stack_bytes = lane.stack_bytes.max(body.stack_bytes);
        phases.push(Phase {
            rule: e.phases[i].clone(),
            lane: i % count,
            frame_bytes: body.frame_bytes,
            stack_bytes: body.stack_bytes,
            output_bytes: body.output_bytes,
        });
    }
    let mut cursor = HEADER + count * CONTROL;
    for lane in &mut lanes {
        lane.output_bytes = lane
            .result_bytes
            .checked_mul(result_batch as usize)
            .ok_or_else(|| error("result batch capacity overflow"))?;
        lane.output = cursor;
        cursor = align(
            cursor
                .checked_add(lane.output_bytes)
                .ok_or_else(|| error("output size overflow"))?,
            64,
        )?;
    }
    let control_reserved = align(cursor, PAGE)?;
    cursor = control_reserved;
    for lane in &mut lanes {
        lane.stack = cursor
            .checked_add(PAGE)
            .ok_or_else(|| error("stack size overflow"))?;
        lane.stack_reserved = align(lane.stack_bytes, PAGE)?;
        cursor = lane
            .stack
            .checked_add(lane.stack_reserved)
            .ok_or_else(|| error("stack size overflow"))?;
    }
    if cursor > 268_435_456 {
        return Err(error("reservation exceeds 256 MiB implementation limit"));
    }
    Ok(Report {
        execution: e.name.clone(),
        input: e.input.clone(),
        max_in_flight: max_in_flight as usize,
        result_batch: result_batch as usize,
        declared_bytes: native_memory,
        control_reserved,
        reserved_bytes: cursor,
        lanes,
        phases,
    })
}
impl Report {
    pub fn json(&self) -> String {
        let lanes = self.lanes.iter().enumerate().map(|(i,l)| format!(
            "{{\"lane\":{},\"control_offset\":{},\"control_bytes\":64,\"output_offset\":{},\"output_capacity_bytes\":{},\"result_capacity_bytes\":{},\"stack_offset\":{},\"stack_bound_bytes\":{},\"stack_reserved_bytes\":{},\"guard_bytes\":4096}}",
            i+1,l.control,l.output,l.output_bytes,l.result_bytes,l.stack,l.stack_bytes,l.stack_reserved)).collect::<Vec<_>>().join(",");
        let phases = self.phases.iter().enumerate().map(|(i,p)| format!(
            "{{\"phase\":{},\"rule\":\"{}\",\"lane\":{},\"frame_bytes\":{},\"stack_bound_bytes\":{},\"output_capacity_bytes\":{}}}",
            i+1,p.rule,p.lane+1,p.frame_bytes,p.stack_bytes,p.output_bytes)).collect::<Vec<_>>().join(",");
        format!(concat!("{{\"schema_version\":1,\"target\":\"x86_64-linux\",\"entry_mode\":\"argv\",",
            "\"scope\":\"concurrent_execution_reservation\",\"composition\":\"concurrent\",",
            "\"execution\":\"{}\",\"input_concept\":\"{}\",\"max_in_flight\":{},\"result_batch\":{},\"declared_bytes\":{},",
            "\"reserved_bytes\":{},\"coordinator_stack_bytes\":0,\"header_bytes\":64,",
            "\"control_and_output_reserved_bytes\":{},\"lanes\":[{}],\"phases\":[{}],",
            "\"excludes\":[\"initial_argv_environment\",\"code_and_elf\",\"kernel_storage\",\"external_output\"]}}"),
            self.execution,self.input,self.max_in_flight,self.result_batch,self.declared_bytes.map_or("null".into(),|v|v.to_string()),
            self.reserved_bytes,self.control_reserved,lanes,phases)
    }
}
impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "execution '{}' over '{}': concurrent native reservation {} bytes; declared {}",
            self.execution,
            self.input,
            self.reserved_bytes,
            self.declared_bytes.map_or(
                "none (native emission requires native_memory)".into(),
                |v| format!("{v} bytes (verified)")
            )
        )?;
        writeln!(f,"{} reusable lanes; control/results {} bytes (page rounded); coordinator additional stack 0 bytes",self.lanes.len(),self.control_reserved)?;
        writeln!(
            f,
            "result_batch: {} results per lane; lane output capacities include the complete batch",
            self.result_batch
        )?;
        for (i, l) in self.lanes.iter().enumerate() {
            writeln!(
                f,
                "  lane {}: stack {} bytes ({} reserved), output {} bytes, guard {} bytes",
                i + 1,
                l.stack_bytes,
                l.stack_reserved,
                l.output_bytes,
                PAGE
            )?;
        }
        for (i, p) in self.phases.iter().enumerate() {
            writeln!(
                f,
                "  phase {} '{}': lane {}, stack {}, output {} bytes",
                i + 1,
                p.rule,
                p.lane + 1,
                p.stack_bytes,
                p.output_bytes
            )?;
        }
        write!(f,"Scope: reserved virtual address bytes, not RSS or total process memory; excludes initial argv/environment, code/ELF, kernel and external output storage.")
    }
}
