//! Summarize pre-existing buffer owners at expanded call boundaries.
//! Last uses have already propagated through all aliases and joins. A sweep
//! over reservations plus prefix sums over last uses avoids calls × buffers
//! scans and never enumerates branch paths or materializes owner sets per call.
use super::{error, Buffer, NativeError};
use crate::stack_budget::CallStorage;

pub(super) struct Call {
    pub callee: String,
    pub parent: Option<usize>,
    pub first: usize,
    pub last: usize,
}

pub(super) fn report(calls: &[Call], buffers: &[Buffer]) -> Result<Vec<CallStorage>, NativeError> {
    let mut ends: Vec<_> = buffers.iter().map(|b| b.last).collect();
    ends.sort_unstable();
    ends.dedup();
    let mut sums = PrefixSums(vec![0; ends.len() + 1]);
    let mut next_buffer = 0;
    let mut total = 0usize;
    let mut reports = Vec::with_capacity(calls.len());
    // Reservations and call entries each follow emitted order. Calls can nest;
    // their exits need not be sorted. Querying by last use handles both orders.
    for call in calls {
        if call.last < call.first {
            return Err(error("incomplete storage call"));
        }
        while let Some(buffer) = buffers.get(next_buffer).filter(|b| b.first < call.first) {
            let size = buffer
                .capacity
                .ok_or_else(|| error("call storage analysis requires known capacities"))?
                .checked_add(7)
                .map(|n| n & !7)
                .ok_or_else(|| error("call storage capacity overflow"))?;
            total = total
                .checked_add(size)
                .ok_or_else(|| error("call storage capacity overflow"))?;
            let index = ends.binary_search(&buffer.last).unwrap();
            sums.add(index, size)?;
            next_buffer += 1;
        }
        let live = total - sums.before(ends.partition_point(|last| *last < call.first));
        let retained = total - sums.before(ends.partition_point(|last| *last < call.last));
        reports.push(CallStorage {
            callee: call.callee.clone(),
            parent_call: call.parent,
            live_caller_buffer_capacity_bytes: live,
            retained_caller_buffer_capacity_bytes: retained,
        });
    }
    Ok(reports)
}

struct PrefixSums(Vec<usize>);
impl PrefixSums {
    fn add(&mut self, index: usize, bytes: usize) -> Result<(), NativeError> {
        let mut at = index + 1;
        while at < self.0.len() {
            self.0[at] = self.0[at]
                .checked_add(bytes)
                .ok_or_else(|| error("call storage capacity overflow"))?;
            at += at & at.wrapping_neg();
        }
        Ok(())
    }
    fn before(&self, mut count: usize) -> usize {
        let mut sum = 0;
        while count != 0 {
            sum += self.0[count];
            count &= count - 1;
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_matches_independent_owner_scan_for_nested_calls_and_dead_buffers() {
        let buffers: Vec<_> = (0..512)
            .map(|n| Buffer {
                capacity: Some((n * 37) % 129),
                first: n * 3,
                last: n * 3 + (n * 17) % 211,
                address: 0,
                context: "test".into(),
            })
            .collect();
        let calls: Vec<_> = (0..1024)
            .map(|n| Call {
                callee: format!("call_{n}"),
                parent: None,
                first: n * 2 + 1,
                last: n * 2 + 1 + (n * 31) % 373,
            })
            .collect();
        let reports = report(&calls, &buffers).unwrap();
        for (call, report) in calls.iter().zip(reports) {
            let count = |end| {
                buffers
                    .iter()
                    .filter(|b| b.first < call.first && b.last >= end)
                    .map(|b| (b.capacity.unwrap() + 7) & !7)
                    .sum::<usize>()
            };
            assert_eq!(report.live_caller_buffer_capacity_bytes, count(call.first));
            assert_eq!(
                report.retained_caller_buffer_capacity_bytes,
                count(call.last)
            );
        }
        assert!(report(&[], &[]).unwrap().is_empty());
        assert_eq!(
            report(&calls, &[]).unwrap()[0].live_caller_buffer_capacity_bytes,
            0
        );
        assert!(report(
            &[Call {
                callee: "incomplete".into(),
                parent: None,
                first: 1,
                last: 0
            }],
            &[]
        )
        .is_err());
    }
}
