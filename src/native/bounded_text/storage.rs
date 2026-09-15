//! Compile-time lifetimes and placement of writable text buffers.
//!
//! Pointer/length slots are never reused. A pointer join records both possible
//! owners; backwards propagation of its last use keeps either buffer alive.
//! Intervals follow emitted order, conservatively spanning conditional arms.
//! There are no runtime allocation/free operations.
use super::{address, error, NativeError, FIXED_SCRATCH, FRAME_LIMIT};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

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

#[derive(Default)]
pub(super) struct Storage {
    pointers: BTreeMap<i32, Pointer>,
    buffers: Vec<Buffer>,
    clock: usize,
}
impl Storage {
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
        self.propagate()?;
        let mut active = BinaryHeap::<Reverse<(usize, usize, usize)>>::new();
        let mut free = Free::default();
        let mut extent = 0usize;
        for buffer in &self.buffers {
            let size = buffer
                .capacity
                .ok_or_else(|| error(format!("{}: unknown buffer capacity", buffer.context)))?
                .checked_add(7)
                .map(|n| n & !7)
                .ok_or_else(|| error("buffer capacity overflow"))?;
            while active
                .peek()
                .is_some_and(|Reverse((last, _, _))| *last < buffer.first)
            {
                let Reverse((_, start, size)) = active.pop().unwrap();
                free.release(start, size);
            }
            let start = if size == 0 {
                0
            } else {
                free.take(size, extent)
            };
            let end = start
                .checked_add(size)
                .ok_or_else(|| error("buffer placement overflow"))?;
            extent = extent.max(end);
            let frame = slots
                .checked_add(extent)
                .and_then(|n| n.checked_add(FIXED_SCRATCH))
                .ok_or_else(|| error("frame size overflow"))?;
            if frame > FRAME_LIMIT {
                return Err(error(format!("{}: invocation frame exceeds {FRAME_LIMIT} bytes (needs {frame} including slots, buffer placement and fixed scratch)", buffer.context)));
            }
            let offset = -((slots + end) as i32);
            code[buffer.address..buffer.address + 4].copy_from_slice(&offset.to_le_bytes());
            if size != 0 {
                active.push(Reverse((buffer.last, start, size)));
            }
        }
        Ok(slots + extent)
    }
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
