//! Compile-time lifetimes and placement of writable text buffers.
//!
//! Pointers retain symbolic identities even when physical word slots are reused.
//! A pointer join records both possible owners; backwards propagation of its
//! last use keeps either buffer alive.
//! Intervals follow emitted order. Structured alternatives can share a region;
//! that region stays live until the last use of either arm's buffers.
//! There are no runtime allocation/free operations.
use super::{address, error, NativeError, FIXED_SCRATCH, FRAME_LIMIT};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
mod calls;
use calls::Call;

#[derive(Default)]
struct Pointer {
    sources: Vec<i32>,
    buffer: Option<usize>,
    last: usize,
}
struct Buffer {
    capacity: Option<usize>,
    first: usize,
    last: usize,
    address: usize,
    context: String,
}

pub(super) struct Storage {
    pointers: BTreeMap<i32, Pointer>,
    buffers: Vec<Buffer>,
    clock: usize,
    scopes: Vec<Vec<Member>>,
    current: usize,
    calls: Vec<Call>,
    call_stack: Vec<usize>,
}
#[derive(Clone, Copy)]
enum Member {
    Buffer(usize),
    Choice(usize, usize),
}
#[derive(Clone, Copy)]
pub(super) struct Branch {
    parent: usize,
    left: usize,
    right: usize,
}
impl Default for Storage {
    fn default() -> Self {
        Self {
            pointers: BTreeMap::new(),
            buffers: Vec::new(),
            clock: 0,
            scopes: vec![Vec::new()],
            current: 0,
            calls: Vec::new(),
            call_stack: Vec::new(),
        }
    }
}
impl Storage {
    pub(super) fn begin_call(&mut self, callee: &str) -> usize {
        self.clock += 1;
        let id = self.calls.len();
        self.calls.push(Call {
            callee: callee.into(),
            parent: self.call_stack.last().map(|n| n + 1),
            first: self.clock,
            last: 0,
        });
        self.call_stack.push(id);
        id
    }
    pub(super) fn end_call(&mut self, id: usize) -> Result<(), NativeError> {
        if self.call_stack.pop() != Some(id) {
            return Err(error("unbalanced storage call"));
        }
        self.clock += 1;
        self.calls[id].last = self.clock;
        Ok(())
    }

    pub(super) fn call_report(&self) -> Result<Vec<crate::stack_budget::CallStorage>, NativeError> {
        if !self.call_stack.is_empty() {
            return Err(error("unclosed storage call"));
        }
        calls::report(&self.calls, &self.buffers)
    }

    // The condition is emitted before entering either arm. Only destinations
    // CREATED in these arms may overlap; existing owners remain in their scope.
    pub(super) fn branch(&mut self) -> Branch {
        let branch = Branch {
            parent: self.current,
            left: self.scopes.len(),
            right: self.scopes.len() + 1,
        };
        self.scopes[self.current].push(Member::Choice(branch.left, branch.right));
        self.scopes.extend([Vec::new(), Vec::new()]);
        self.current = branch.left;
        branch
    }
    pub(super) fn otherwise(&mut self, branch: Branch) -> Result<(), NativeError> {
        if self.current != branch.left {
            return Err(error("unbalanced storage branch"));
        }
        self.current = branch.right;
        Ok(())
    }
    pub(super) fn end_branch(&mut self, branch: Branch) -> Result<(), NativeError> {
        if self.current != branch.right {
            return Err(error("unbalanced storage branch"));
        }
        self.current = branch.parent;
        Ok(())
    }
    pub(super) fn pointer(&mut self, ptr: i32) {
        self.pointers.entry(ptr).or_default();
    }
    pub(super) fn touch(&mut self, ptr: i32) -> Result<(), NativeError> {
        self.clock += 1;
        self.pointers
            .get_mut(&ptr)
            .ok_or_else(|| error("unknown text storage provenance"))?
            .last = self.clock;
        Ok(())
    }
    pub(super) fn alias(&mut self, dest: i32, source: i32) -> Result<(), NativeError> {
        if !self.pointers.contains_key(&source) {
            return Err(error("unknown alias storage provenance"));
        }
        if dest == source {
            return Ok(());
        }
        let node = self
            .pointers
            .get_mut(&dest)
            .ok_or_else(|| error("unknown text join"))?;
        if node.buffer.is_some() {
            return Err(error("cannot replace an owned destination with an alias"));
        }
        if !node.sources.contains(&source) {
            node.sources.push(source);
        }
        Ok(())
    }
    pub(super) fn reserve(
        &mut self,
        ptr: i32,
        capacity: Option<usize>,
        context: String,
        code: &mut Vec<u8>,
    ) -> Result<(), NativeError> {
        let node = self
            .pointers
            .get_mut(&ptr)
            .ok_or_else(|| error("unknown destination"))?;
        if node.buffer.is_some() || !node.sources.is_empty() {
            return Err(error("destination storage already assigned"));
        }
        self.clock += 1;
        node.buffer = Some(self.buffers.len());
        self.scopes[self.current].push(Member::Buffer(self.buffers.len()));
        // The address exists from entry into this destination's producer, even
        // when its inferred capacity is learned only after emitting the body.
        address(code, 0, 0);
        self.buffers.push(Buffer {
            capacity,
            first: self.clock,
            last: self.clock,
            address: code.len() - 4,
            context,
        });
        Ok(())
    }
    pub(super) fn capacity(&mut self, ptr: i32, cap: usize) -> Result<(), NativeError> {
        let id = self
            .pointers
            .get(&ptr)
            .and_then(|n| n.buffer)
            .ok_or_else(|| error("missing destination owner"))?;
        self.buffers[id].capacity = Some(cap);
        Ok(())
    }
    fn propagate(&mut self) -> Result<(), NativeError> {
        // Edges point from a joined pointer to each possible source. The else
        // source can be created after its join, so creation order alone is not
        // a topological order. Each node/edge is processed once, without DFS or
        // expanding the set of owners at every alias use.
        let mut incoming: BTreeMap<_, usize> = self.pointers.keys().map(|p| (*p, 0)).collect();
        for node in self.pointers.values() {
            for source in &node.sources {
                *incoming
                    .get_mut(source)
                    .ok_or_else(|| error("unknown join source"))? += 1;
            }
        }
        let mut ready: BTreeSet<_> = incoming
            .iter()
            .filter_map(|(p, n)| (*n == 0).then_some(*p))
            .collect();
        let mut visited = 0;
        while let Some(ptr) = ready.pop_first() {
            visited += 1;
            let node = &self.pointers[&ptr];
            let last = node.last;
            if let Some(id) = node.buffer {
                self.buffers[id].last = self.buffers[id].last.max(last);
            }
            let sources = node.sources.clone();
            for source in sources {
                let node = self.pointers.get_mut(&source).unwrap();
                node.last = node.last.max(last);
                let count = incoming.get_mut(&source).unwrap();
                *count -= 1;
                if *count == 0 {
                    ready.insert(source);
                }
            }
        }
        if visited != self.pointers.len() {
            return Err(error("cyclic text storage provenance"));
        }
        Ok(())
    }
    pub(super) fn layout(&mut self, code: &mut [u8], slots: usize) -> Result<usize, NativeError> {
        if self.current != 0 {
            return Err(error("unclosed storage branch"));
        }
        self.propagate()?;
        let mut intervals = Vec::with_capacity(self.buffers.len());
        for buffer in &self.buffers {
            let size = buffer
                .capacity
                .ok_or_else(|| error(format!("{}: unknown buffer capacity", buffer.context)))?
                .checked_add(7)
                .map(|n| n & !7)
                .ok_or_else(|| error("buffer capacity overflow"))?;
            intervals.push(Interval {
                size,
                first: buffer.first,
                last: buffer.last,
            });
        }
        let (mut extent, mut starts) = place(&intervals)?;
        if self.scopes.len() > 1 {
            let (overlaid_extent, overlaid_starts) = self.overlay(&intervals)?;
            // Retaining a whole branch region can be more conservative than
            // individual last-use reuse. Never enlarge a frame for this choice;
            // ties retain the old placement and emitted bytes.
            if overlaid_extent < extent {
                extent = overlaid_extent;
                starts = overlaid_starts;
            }
        }
        let frame = slots
            .checked_add(extent)
            .and_then(|n| n.checked_add(FIXED_SCRATCH))
            .ok_or_else(|| error("frame size overflow"))?;
        if frame > FRAME_LIMIT {
            return Err(error(format!("invocation frame exceeds {FRAME_LIMIT} bytes (needs {frame} including slots, buffer placement and fixed scratch)")));
        }
        for (id, buffer) in self.buffers.iter().enumerate() {
            let offset = -((slots + starts[id] + intervals[id].size) as i32);
            code[buffer.address..buffer.address + 4].copy_from_slice(&offset.to_le_bytes());
        }
        Ok(slots + extent)
    }
    fn overlay(&self, buffers: &[Interval]) -> Result<(usize, Vec<usize>), NativeError> {
        // Children have larger scope indices. Build each arm independently,
        // then represent both arms by ONE region sized to their maximum. Its
        // lifetime spans all descendant owners, including aliases used after
        // the join. This deliberately avoids enumerating execution paths or
        // pairwise interference, and uses no runtime branch/ownership metadata.
        let mut regions = vec![
            Interval {
                size: 0,
                first: usize::MAX,
                last: 0
            };
            self.scopes.len()
        ];
        let mut offsets = vec![Vec::new(); self.scopes.len()];
        for (id, scope) in self.scopes.iter().enumerate().rev() {
            let members: Vec<_> = scope
                .iter()
                .map(|member| match *member {
                    Member::Buffer(b) => buffers[b],
                    Member::Choice(a, b) => Interval {
                        size: regions[a].size.max(regions[b].size),
                        first: regions[a].first.min(regions[b].first),
                        last: regions[a].last.max(regions[b].last),
                    },
                })
                .collect();
            let (size, starts) = place(&members)?;
            regions[id] = Interval {
                size,
                first: members.iter().map(|m| m.first).min().unwrap_or(usize::MAX),
                last: members.iter().map(|m| m.last).max().unwrap_or(0),
            };
            offsets[id] = starts;
        }
        let mut bases = vec![0usize; self.scopes.len()];
        let mut starts = vec![0; buffers.len()];
        for (id, scope) in self.scopes.iter().enumerate() {
            for (member, offset) in scope.iter().zip(&offsets[id]) {
                let start = bases[id]
                    .checked_add(*offset)
                    .ok_or_else(|| error("branch placement overflow"))?;
                match *member {
                    Member::Buffer(b) => starts[b] = start,
                    Member::Choice(a, b) => {
                        bases[a] = start;
                        bases[b] = start;
                    }
                }
            }
        }
        Ok((regions[0].size, starts))
    }
}

#[derive(Clone, Copy)]
struct Interval {
    size: usize,
    first: usize,
    last: usize,
}
fn place(intervals: &[Interval]) -> Result<(usize, Vec<usize>), NativeError> {
    let mut active = BinaryHeap::<Reverse<(usize, usize, usize)>>::new();
    let mut free = Free::default();
    let mut extent = 0usize;
    let mut starts = Vec::with_capacity(intervals.len());
    for &Interval { size, first, last } in intervals {
        // Empty arms have no lifetime. In particular, their sentinel must not
        // release live regions before the next real allocation in this scope.
        if size == 0 {
            starts.push(0);
            continue;
        }
        while active
            .peek()
            .is_some_and(|Reverse((end, _, _))| *end < first)
        {
            let Reverse((_, start, size)) = active.pop().unwrap();
            free.release(start, size);
        }
        let start = free.take(size, extent);
        let end = start
            .checked_add(size)
            .ok_or_else(|| error("buffer placement overflow"))?;
        extent = extent.max(end);
        starts.push(start);
        active.push(Reverse((last, start, size)));
    }
    Ok((extent, starts))
}

// Best fit with adjacent-hole coalescing. Both indices are ordered, making
// layout deterministic and avoiding quadratic scans of fragmented storage.
#[derive(Default)]
struct Free {
    by_start: BTreeMap<usize, usize>,
    by_size: BTreeSet<(usize, usize)>,
}
impl Free {
    fn remove(&mut self, start: usize, size: usize) {
        self.by_start.remove(&start);
        self.by_size.remove(&(size, start));
    }
    fn release(&mut self, mut start: usize, mut size: usize) {
        if let Some((&left, &len)) = self.by_start.range(..start).next_back() {
            if left + len == start {
                self.remove(left, len);
                start = left;
                size += len;
            }
        }
        if let Some(&right) = self.by_start.get(&(start + size)) {
            self.remove(start + size, right);
            size += right;
        }
        self.by_start.insert(start, size);
        self.by_size.insert((size, start));
    }
    fn take(&mut self, size: usize, extent: usize) -> usize {
        if let Some(&(len, start)) = self.by_size.range((size, 0)..).next() {
            self.remove(start, len);
            if len > size {
                self.release(start + size, len - size);
            }
            return start;
        }
        // A too-small free tail can grow, avoiding a stranded hole when
        // successive, non-overlapping values have increasing capacities.
        if let Some((&start, &len)) = self.by_start.last_key_value() {
            if start + len == extent {
                self.remove(start, len);
                return start;
            }
        }
        extent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve(s: &mut Storage, code: &mut Vec<u8>, ptr: i32, capacity: usize) {
        s.pointer(ptr);
        s.reserve(ptr, Some(capacity), "test buffer".into(), code)
            .unwrap();
    }
    fn span(s: &Storage, code: &[u8], id: usize) -> (i32, i32) {
        let b = &s.buffers[id];
        let at = b.address;
        let start = i32::from_le_bytes(code[at..at + 4].try_into().unwrap());
        (start, start + b.capacity.unwrap() as i32)
    }
    fn overlap(a: (i32, i32), b: (i32, i32)) -> bool {
        a.0 < b.1 && b.0 < a.1
    }

    #[test]
    fn call_retention_tracks_aliases_nested_calls_and_last_use_without_recounting_owners() {
        for keep_after in [false, true] {
            let mut s = Storage::default();
            let mut code = Vec::new();
            reserve(&mut s, &mut code, -8, 64); // caller result destination
            reserve(&mut s, &mut code, -16, 9); // immutable argument, rounded to 16
            s.pointer(-24);
            s.alias(-24, -16).unwrap();
            s.pointer(-32);
            s.alias(-32, -24).unwrap();
            let outer = s.begin_call("outer");
            reserve(&mut s, &mut code, -40, 24); // callee's own temporary
            let inner = s.begin_call("inner");
            s.touch(-24).unwrap();
            s.end_call(inner).unwrap();
            s.touch(-40).unwrap();
            s.end_call(outer).unwrap();
            if keep_after {
                s.touch(-32).unwrap();
            }
            s.touch(-8).unwrap();
            s.layout(&mut code, 40).unwrap();
            let calls = s.call_report().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].parent_call, None);
            assert_eq!(calls[1].parent_call, Some(1));
            assert_eq!(calls[0].live_caller_buffer_capacity_bytes, 80);
            assert_eq!(
                calls[0].retained_caller_buffer_capacity_bytes,
                if keep_after { 80 } else { 64 }
            );
            assert_eq!(calls[1].live_caller_buffer_capacity_bytes, 104);
            assert_eq!(
                calls[1].retained_caller_buffer_capacity_bytes,
                if keep_after { 104 } else { 88 }
            );
        }
    }

    #[test]
    fn call_retention_keeps_alternative_capacities_distinct_from_overlaid_frame_bytes() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        s.pointer(-8); // joined pointer
        let branch = s.branch();
        reserve(&mut s, &mut code, -16, 64);
        s.alias(-8, -16).unwrap();
        s.otherwise(branch).unwrap();
        reserve(&mut s, &mut code, -24, 64);
        s.alias(-8, -24).unwrap();
        s.end_branch(branch).unwrap();
        let call = s.begin_call("consume");
        s.touch(-8).unwrap();
        s.end_call(call).unwrap();
        s.touch(-8).unwrap();
        assert_eq!(s.layout(&mut code, 24).unwrap(), 24 + 64);
        assert_eq!(span(&s, &code, 0), span(&s, &code, 1));
        let calls = s.call_report().unwrap();
        // Two possible owners, not 128 bytes added to the 64-byte placement.
        assert_eq!(calls[0].live_caller_buffer_capacity_bytes, 128);
        assert_eq!(calls[0].retained_caller_buffer_capacity_bytes, 128);
    }

    #[test]
    fn joined_alias_keeps_both_owners_until_its_last_use() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        reserve(&mut s, &mut code, -8, 64);
        s.touch(-8).unwrap();
        s.pointer(-16);
        s.alias(-16, -8).unwrap();
        // The second owner is created AFTER the join, as in an else arm.
        reserve(&mut s, &mut code, -24, 64);
        s.touch(-24).unwrap();
        s.alias(-16, -24).unwrap();
        s.pointer(-32);
        s.alias(-32, -16).unwrap();
        reserve(&mut s, &mut code, -40, 64);
        s.touch(-40).unwrap();
        s.touch(-32).unwrap();
        reserve(&mut s, &mut code, -48, 128);
        s.touch(-48).unwrap();
        let frame = s.layout(&mut code, 64).unwrap();
        assert_eq!(frame, 64 + 192);
        for a in 0..3 {
            for b in a + 1..3 {
                assert!(!overlap(span(&s, &code, a), span(&s, &code, b)));
            }
        }
        // Once the join dies, its adjacent holes coalesce for the larger value.
        assert!(span(&s, &code, 3).0 >= -(frame as i32));
    }

    #[test]
    fn placement_never_overlaps_simultaneously_live_buffers() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        let mut seed = 7u32;
        for i in 0..512 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let capacity = ((seed >> 16) % 257) as usize;
            reserve(&mut s, &mut code, -(i + 1) * 8, capacity);
            // Fixed external intervals exercise fragmentation, simultaneous
            // endpoints, zero-length values and repeated increases in size.
            let b = s.buffers.last_mut().unwrap();
            b.last = b.first + (seed % 31) as usize;
        }
        let frame = s.layout(&mut code, 4096).unwrap();
        let emitted = code.clone();
        assert_eq!(s.layout(&mut code, 4096).unwrap(), frame);
        assert_eq!(code, emitted);
        for (i, a) in s.buffers.iter().enumerate() {
            let pa = span(&s, &code, i);
            assert_eq!(pa.0 % 8, 0);
            assert!(pa.0 >= -(frame as i32) && pa.1 <= -4096);
            for (j, b) in s.buffers.iter().enumerate().skip(i + 1) {
                if a.capacity != Some(0)
                    && b.capacity != Some(0)
                    && a.first <= b.last
                    && b.first <= a.last
                {
                    assert!(
                        !overlap(pa, span(&s, &code, j)),
                        "live buffers {i} and {j} overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn exclusive_owners_overlap_but_their_join_protects_the_region() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        reserve(&mut s, &mut code, -8, 32); // Outer owner.
        s.pointer(-16);
        let branch = s.branch();
        reserve(&mut s, &mut code, -24, 64);
        s.alias(-16, -24).unwrap();
        s.otherwise(branch).unwrap();
        reserve(&mut s, &mut code, -32, 128);
        s.alias(-16, -32).unwrap();
        s.end_branch(branch).unwrap();
        reserve(&mut s, &mut code, -40, 64); // Later work cannot destroy join.
        s.touch(-8).unwrap();
        s.touch(-16).unwrap();
        assert_eq!(s.layout(&mut code, 48).unwrap(), 48 + 32 + 128 + 64);
        assert!(overlap(span(&s, &code, 1), span(&s, &code, 2)));
        for a in [0, 3] {
            for b in [1, 2] {
                assert!(!overlap(span(&s, &code, a), span(&s, &code, b)));
            }
        }
    }

    #[test]
    fn branch_grouping_never_increases_the_legacy_frame() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        s.pointer(-8);
        let branch = s.branch();
        reserve(&mut s, &mut code, -16, 1024); // Dead temporary.
        reserve(&mut s, &mut code, -24, 8); // Escaping small result.
        s.alias(-8, -24).unwrap();
        s.otherwise(branch).unwrap();
        reserve(&mut s, &mut code, -32, 8);
        s.alias(-8, -32).unwrap();
        s.end_branch(branch).unwrap();
        reserve(&mut s, &mut code, -40, 512);
        s.touch(-8).unwrap();
        // Grouping would hold the whole 1024-byte region through the later
        // work. The original layout fits that work alongside the small owners.
        assert_eq!(s.layout(&mut code, 48).unwrap(), 48 + 1024);
        assert!(!overlap(span(&s, &code, 1), span(&s, &code, 2)));
    }

    #[test]
    fn structured_placement_preserves_every_compatible_live_pair() {
        fn generate(
            s: &mut Storage,
            code: &mut Vec<u8>,
            seed: &mut u32,
            path: &mut Vec<(usize, bool)>,
            paths: &mut Vec<Vec<(usize, bool)>>,
            depth: usize,
        ) {
            for _ in 0..4 {
                *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                if depth < 3 && *seed % 3 == 0 {
                    let branch = s.branch();
                    path.push((branch.left, false));
                    generate(s, code, seed, path, paths, depth + 1);
                    s.otherwise(branch).unwrap();
                    path.last_mut().unwrap().1 = true;
                    generate(s, code, seed, path, paths, depth + 1);
                    path.pop();
                    s.end_branch(branch).unwrap();
                } else {
                    let ptr = -(paths.len() as i32 + 1) * 8;
                    reserve(s, code, ptr, ((*seed >> 16) % 257) as usize);
                    s.buffers.last_mut().unwrap().last += (*seed % 511) as usize;
                    paths.push(path.clone());
                }
            }
        }
        for mut seed in [7, 42, 1234, 5678, 9000] {
            let mut s = Storage::default();
            let mut code = Vec::new();
            let mut paths = Vec::new();
            generate(&mut s, &mut code, &mut seed, &mut Vec::new(), &mut paths, 0);
            let slots = paths.len() * 8;
            let frame = s.layout(&mut code, slots).unwrap();
            let emitted = code.clone();
            assert_eq!(s.layout(&mut code, slots).unwrap(), frame);
            assert_eq!(code, emitted);
            for (i, a) in s.buffers.iter().enumerate() {
                assert!(span(&s, &code, i).0 >= -(frame as i32));
                assert!(span(&s, &code, i).1 <= -(slots as i32));
                for (j, b) in s.buffers.iter().enumerate().skip(i + 1) {
                    let exclusive = paths[i].iter().any(|(id, arm)| {
                        paths[j]
                            .iter()
                            .any(|(other, side)| id == other && arm != side)
                    });
                    if !exclusive
                        && a.capacity != Some(0)
                        && b.capacity != Some(0)
                        && a.first <= b.last
                        && b.first <= a.last
                    {
                        assert!(
                            !overlap(span(&s, &code, i), span(&s, &code, j)),
                            "compatible live owners {i}, {j} overlap (seed {seed})"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn empty_and_unbalanced_branches() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        reserve(&mut s, &mut code, -8, 64);
        let branch = s.branch();
        assert!(s.end_branch(branch).is_err());
        assert!(s.layout(&mut code, 16).is_err());
        s.otherwise(branch).unwrap();
        assert!(s.otherwise(branch).is_err());
        s.end_branch(branch).unwrap();
        reserve(&mut s, &mut code, -16, 64);
        s.touch(-8).unwrap();
        assert_eq!(s.layout(&mut code, 16).unwrap(), 144);
        assert!(!overlap(span(&s, &code, 0), span(&s, &code, 1)));
    }

    #[test]
    fn unknown_capacity_and_cyclic_provenance_fail_closed() {
        let mut s = Storage::default();
        let mut code = Vec::new();
        s.pointer(-8);
        s.reserve(-8, None, "unknown result".into(), &mut code)
            .unwrap();
        assert!(s
            .layout(&mut code, 16)
            .unwrap_err()
            .message
            .contains("unknown result: unknown buffer capacity"));
        let mut s = Storage::default();
        s.pointer(-8);
        s.pointer(-16);
        s.alias(-8, -16).unwrap();
        s.alias(-16, -8).unwrap();
        assert!(s
            .layout(&mut [], 16)
            .unwrap_err()
            .message
            .contains("cyclic text storage provenance"));
    }
}
