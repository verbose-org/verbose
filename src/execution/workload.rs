//! Predicted use and exact experiment-planning counts, separate from proofs.
use super::*;

pub(crate) fn shape_error(w: &Workload) -> Option<String> {
    if !(1..=16).contains(&w.cases.len()) {
        return Some("workload requires 1..=16 cases".into());
    }
    let mut seen = HashSet::new();
    for c in &w.cases {
        // Also close programmatically constructed ASTs; names appear in JSON.
        if !c
            .name
            .as_bytes()
            .first()
            .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
            || !c
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Some("workload case name must be an identifier".into());
        }
        if !seen.insert(&c.name) {
            return Some(format!("workload: duplicate case '{}'", c.name));
        }
        for (key, value, maximum) in [
            ("weight", c.weight, 1_000_000),
            ("records", c.records, 1_000_000),
            ("target_us", c.target_us.unwrap_or(1), 1_000_000_000),
        ] {
            if !(1..=maximum).contains(&value) {
                return Some(format!(
                    "workload case '{}': {key} must be in [1, {maximum}]",
                    c.name
                ));
            }
        }
    }
    None
}

impl WorkloadObjective {
    fn name(self) -> &'static str {
        match self {
            Self::Elapsed => "elapsed",
            Self::Cpu => "cpu",
        }
    }
    fn metric(self) -> &'static str {
        match self {
            Self::Elapsed => "wall_us",
            Self::Cpu => "cpu_us",
        }
    }
}

#[derive(Debug)]
pub struct Report {
    execution: String,
    input: String,
    input_fields: Vec<(String, &'static str)>,
    workload: Workload,
    phases: Vec<String>,
    batch: Option<u32>,
    native_budget_present: bool,
    native_json: String,
    native_text: String,
}

pub fn report(p: &Program, name: &str) -> Result<Report, NativeError> {
    // Validate every declaration before arithmetic or lowering, including
    // unselected profiles and callers that construct an AST directly.
    gate(p)?;
    let e = find(p, name).ok_or_else(|| NativeError {
        message: "--workload-report requires one source execution".into(),
    })?;
    let workload = e.workload.clone().ok_or_else(|| NativeError {
        message: format!("execution '{name}': --workload-report requires a declared workload"),
    })?;
    let (batch, native_budget_present, native_json, native_text) = match e.mode {
        ExecutionMode::Pipeline { .. } => return Err(NativeError {
            message: "pipeline execution does not support workload reports in this slice".into(),
        }),
        ExecutionMode::Sequential { .. } => {
            let r = super::report_one(p, e).map_err(|e| NativeError {
                message: e.to_string(),
            })?;
            (None, true, r.json(), r.to_string())
        }
        ExecutionMode::Concurrent {
            result_batch,
            native_memory,
            ..
        } => {
            let r = native::concurrent_memory_report(p, e)?;
            (
                Some(result_batch),
                native_memory.is_some(),
                r.json(),
                r.to_string(),
            )
        }
    };
    Ok(Report {
        execution: e.name.clone(),
        input: e.input.clone(),
        input_fields: iter_all_concepts(&p.items)
            .find(|c| c.name == e.input)
            .expect("gate validated the input concept")
            .fields
            .iter()
            .map(|f| {
                (
                    f.name.clone(),
                    match f.ty {
                        Type::Number => "number",
                        Type::Text => "text",
                        Type::Bool => "bool",
                        Type::Bytes => "bytes",
                        _ => "unsupported",
                    },
                )
            })
            .collect(),
        workload,
        phases: e.phases.clone(),
        batch,
        native_budget_present,
        native_json,
        native_text,
    })
}

fn ratio(n: u64, d: u64) -> String {
    format!("{{\"numerator\":{n},\"denominator\":{d}}}")
}
impl Report {
    // 16 cases * 10^6 weight * 10^6 records * 64 phases < 2^64.
    // gate() establishes these bounds before a Report can be constructed.
    fn total_weight(&self) -> u64 {
        self.workload.cases.iter().map(|c| c.weight as u64).sum()
    }
    fn weighted_records(&self) -> u64 {
        self.workload
            .cases
            .iter()
            .map(|c| c.weight as u64 * c.records as u64)
            .sum()
    }
    fn batches(&self, records: u32) -> Option<u64> {
        self.batch
            .map(|b| (records as u64).div_ceil(b as u64) * self.phases.len() as u64)
    }
    pub fn json(&self) -> String {
        let weight = self.total_weight();
        let records = self.weighted_records();
        let phase_count = self.phases.len() as u64;
        let cases = self
            .workload
            .cases
            .iter()
            .map(|c| {
                format!(
                    concat!(
            "{{\"name\":\"{}\",\"weight\":{},\"records\":{},\"target_us\":{},",
            "\"invocation_share\":{},\"record_volume_share\":{},",
            "\"full_success_phase_evaluations\":{},\"full_success_native_result_batches\":{}}}"),
                    c.name,
                    c.weight,
                    c.records,
                    c.target_us.map_or("null".into(), |n| n.to_string()),
                    ratio(c.weight as u64, weight),
                    ratio(c.weight as u64 * c.records as u64, records),
                    c.records as u64 * phase_count,
                    self.batches(c.records)
                        .map_or("null".into(), |n| n.to_string())
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let batches = if self.batch.is_some() {
            ratio(
                self.workload
                    .cases
                    .iter()
                    .map(|c| c.weight as u64 * self.batches(c.records).unwrap())
                    .sum(),
                weight,
            )
        } else {
            "null".into()
        };
        format!(concat!(
            "{{\"schema_version\":1,\"profile_kind\":\"prediction\",\"execution\":\"{}\",\"input_concept\":\"{}\",",
            "\"input_fields\":[{}],",
            "\"objective\":\"{}\",\"measurement_metric\":\"{}\",\"aggregation\":\"weighted_arithmetic_mean_per_invocation\",",
            "\"target_kind\":\"unverified_elapsed_goal\",\"phase_count\":{},\"total_weight\":{},\"cases\":[{}],",
            "\"expected_records_per_invocation\":{},\"expected_full_success_phase_evaluations\":{},",
            "\"expected_full_success_native_result_batches\":{},\"native_emission_budget_present\":{},",
            "\"native_resource\":{},\"measurements_available\":false}}"),
            self.execution, self.input,
            self.input_fields.iter().map(|(name, ty)| format!("{{\"name\":\"{name}\",\"type\":\"{ty}\"}}")).collect::<Vec<_>>().join(","),
            self.workload.objective.name(), self.workload.objective.metric(),
            phase_count, weight, cases, ratio(records, weight), ratio(records * phase_count, weight),
            batches, self.native_budget_present, self.native_json)
    }
}
impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "predicted workload: execution '{}' over '{}'",
            self.execution, self.input
        )?;
        writeln!(
            f,
            "objective: weighted mean {} per invocation; no measurements available",
            self.workload.objective.name()
        )?;
        writeln!(
            f,
            "targets are desired elapsed times, not verified deadlines or runtime timeouts"
        )?;
        for c in &self.workload.cases {
            writeln!(
                f,
                "  case '{}': {}/{} invocations; {} records; {}/{} record volume; target {}",
                c.name,
                c.weight,
                self.total_weight(),
                c.records,
                c.weight as u64 * c.records as u64,
                self.weighted_records(),
                c.target_us.map_or("none".into(), |n| format!("{n} us"))
            )?;
            writeln!(f, "    on full success: {} phase evaluations; native result batches {} (not syscall counts)",
                c.records as u64 * self.phases.len() as u64,
                self.batches(c.records).map_or("not applicable".into(), |n| n.to_string()))?;
        }
        writeln!(
            f,
            "expected records per invocation: {}/{}; no instruction/cycle cost inferred",
            self.weighted_records(),
            self.total_weight()
        )?;
        writeln!(
            f,
            "native emission budget present: {}",
            self.native_budget_present
        )?;
        write!(f, "{}", self.native_text)
    }
}

#[cfg(test)]
mod tests;
