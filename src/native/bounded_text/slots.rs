//! Symbolic one-word slots, relocated after their last emitted use is known.
//! Slots never escape by address. Every body load/store registers its operand;
//! buffer addresses have a separate placement and are not slot operands.
use super::{error, NativeError, FIXED_SCRATCH, FRAME_LIMIT};
use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap};
use std::ops::{Deref, DerefMut};

pub(super) trait Buffer {
    fn bytes(&mut self) -> &mut Vec<u8>;
    fn operand(&mut self, slot: i32, site: usize);
}
impl Buffer for Vec<u8> {
    fn bytes(&mut self) -> &mut Vec<u8> {
        self
    }
    fn operand(&mut self, _slot: i32, _site: usize) {}
}

#[derive(Clone, Copy)]
struct Lifetime {
    first: usize,
    last: usize,
}

#[derive(Default)]
pub(super) struct Code {
    bytes: Vec<u8>,
    slots: Vec<Lifetime>,
    operands: Vec<(i32, usize)>,
}
impl Deref for Code {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}
impl DerefMut for Code {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}
impl Buffer for Code {
    fn bytes(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }
    fn operand(&mut self, slot: i32, site: usize) {
        self.operands.push((slot, site));
    }
}

#[derive(Debug)]
pub(super) struct Layout {
    offsets: Vec<i32>,
    pub bytes: usize,
}
fn index(slot: i32, count: usize) -> Result<usize, NativeError> {
    let index = slot
        .checked_neg()
        .filter(|n| *n > 0 && n % 8 == 0)
        .map(|n| (n / 8 - 1) as usize)
        .filter(|n| *n < count)
        .ok_or_else(|| error("unknown symbolic word slot"))?;
    Ok(index)
}
impl Layout {
    pub(super) fn offset(&self, slot: i32) -> Result<i32, NativeError> {
        Ok(self.offsets[index(slot, self.offsets.len())?])
    }
}
impl Code {
    pub(super) fn slot(&mut self) -> Result<i32, NativeError> {
        // Keep a compiler-work limit on symbolic slots independently of the
        // smaller runtime frame. The previous emitter used the same ceiling.
        if (self.slots.len() + 1) * 8 + FIXED_SCRATCH > FRAME_LIMIT {
            return Err(error(
                "symbolic slot analysis exceeds the 2 MiB pre-reuse limit",
            ));
        }
        self.slots.push(Lifetime {
            first: self.bytes.len(),
            last: self.bytes.len(),
        });
        Ok(-((self.slots.len() * 8) as i32))
    }
    pub(super) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub(super) fn layout(
        &mut self,
        inputs: &[i32],
        outputs: &[i32],
    ) -> Result<Layout, NativeError> {
        let mut spans = self.slots.clone();
        for &(slot, site) in &self.operands {
            let span = &mut spans[index(slot, self.slots.len())?];
            if self.bytes.get(site..site + 4) != Some(slot.to_le_bytes().as_slice()) {
                return Err(error("invalid symbolic slot relocation"));
            }
            span.last = span.last.max(site + 4);
        }
        // Input fields are initialized together by Fragment::begin, before
        // any body instruction, even when discovered lazily in a later branch.
        for &slot in inputs {
            spans[index(slot, self.slots.len())?].first = 0;
        }
        // CLI/HTTP/persistent-state consumers read the result after the body.
        for &slot in outputs {
            spans[index(slot, self.slots.len())?].last = self.bytes.len() + 1;
        }
        let mut order: Vec<_> = (0..spans.len()).collect();
        order.sort_unstable_by_key(|&id| (spans[id].first, id));
        let mut active = BinaryHeap::<Reverse<(usize, usize)>>::new();
        let mut free = BTreeSet::new();
        let mut peak = 0;
        let mut offsets = vec![0; spans.len()];
        for id in order {
            let span = spans[id];
            while active
                .peek()
                .is_some_and(|Reverse((last, _))| *last < span.first)
            {
                let Reverse((_, word)) = active.pop().unwrap();
                free.insert(word);
            }
            let word = free.pop_first().unwrap_or_else(|| {
                peak += 1;
                peak
            });
            offsets[id] = -(word as i32 * 8);
            active.push(Reverse((span.last, word)));
        }
        // No benefit means no renumbering, preserving old bytes for this case.
        if peak == spans.len() {
            for (id, offset) in offsets.iter_mut().enumerate() {
                *offset = -((id + 1) as i32 * 8);
            }
        }
        let layout = Layout {
            offsets,
            bytes: peak * 8,
        };
        for &(slot, site) in &self.operands {
            self.bytes[site..site + 4].copy_from_slice(&layout.offset(slot)?.to_le_bytes());
        }
        Ok(layout)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{load, store};
    use super::*;
    use std::collections::HashMap;

    fn number(code: &mut Code, n: i64) -> i32 {
        code.extend_from_slice(&[0x48, 0xb8]);
        code.extend_from_slice(&n.to_le_bytes());
        let slot = code.slot().unwrap();
        store(code, 0, slot);
        slot
    }

    // Execute the emitted word operations using their encoded displacements.
    // This oracle knows nothing about allocation spans or the placement heap.
    fn run(bytes: &[u8], mut memory: HashMap<i32, i64>) -> (Vec<i64>, HashMap<i32, i64>) {
        let mut reads = Vec::new();
        let mut rax = 0;
        let mut pc = 0;
        while pc < bytes.len() {
            match &bytes[pc..pc + 3] {
                [0x48, 0xb8, _] => {
                    rax = i64::from_le_bytes(bytes[pc + 2..pc + 10].try_into().unwrap());
                    pc += 10;
                }
                [0x48, op @ (0x89 | 0x8b), 0x85] => {
                    let slot = i32::from_le_bytes(bytes[pc + 3..pc + 7].try_into().unwrap());
                    if *op == 0x89 {
                        memory.insert(slot, rax);
                    } else {
                        rax = *memory.get(&slot).expect("read before initialization");
                        reads.push(rax);
                    }
                    pc += 7;
                }
                bytes => panic!("unexpected test instruction {bytes:?}"),
            }
        }
        (reads, memory)
    }

    #[test]
    fn relocation_preserves_late_inputs_and_every_read_and_retained_result() {
        let mut code = Code::default();
        let first = number(&mut code, 999);
        let late_input = code.slot().unwrap();
        let mut words = vec![first];
        for n in 1..256 {
            let word = number(&mut code, n * 37);
            words.push(word);
            load(&mut code, 0, words[(n as usize * 17) % words.len()]);
            load(&mut code, 0, word);
        }
        load(&mut code, 0, late_input);
        let outputs = [first, words[8], words[255], late_input];
        let before = run(&code.bytes, HashMap::from([(late_input, -123)]));
        let length = code.len();
        let layout = code.layout(&[late_input], &outputs).unwrap();
        assert!(layout.bytes < code.slots.len() * 8);
        assert_eq!(length, code.len());
        let after = run(
            &code.bytes,
            HashMap::from([(layout.offset(late_input).unwrap(), -123)]),
        );
        assert_eq!(before.0, after.0);
        for slot in outputs {
            assert_eq!(before.1[&slot], after.1[&layout.offset(slot).unwrap()]);
        }
    }

    #[test]
    fn dead_words_reuse_one_slot_but_simultaneously_live_words_keep_identity() {
        let mut code = Code::default();
        let mut last = 0;
        for n in 0..256 {
            last = number(&mut code, n);
            load(&mut code, 0, last);
        }
        let before = run(&code.bytes, HashMap::new());
        assert_eq!(code.layout(&[], &[last]).unwrap().bytes, 8);
        assert_eq!(before.0, run(&code.bytes, HashMap::new()).0);
        let mut code = Code::default();
        let outputs: Vec<_> = (0..128).map(|n| number(&mut code, n)).collect();
        let before = code.bytes.clone();
        assert_eq!(code.layout(&[], &outputs).unwrap().bytes, 128 * 8);
        assert_eq!(code.bytes, before);
    }

    #[test]
    fn only_registered_operands_are_relocated_and_unknown_operands_refuse() {
        let mut code = Code::default();
        for n in 0..4 {
            number(&mut code, n);
        }
        let at = code.len();
        let literal = [0x48, 0x89, 0x85, 0xf0, 0xff, 0xff, 0xff];
        code.extend_from_slice(&literal);
        code.layout(&[], &[]).unwrap();
        assert_eq!(&code.bytes[at..], &literal);
        let mut code = Code::default();
        load(&mut code, 0, -8);
        assert!(code
            .layout(&[], &[])
            .unwrap_err()
            .message
            .contains("unknown symbolic"));
    }

    #[test]
    fn http_status_facts_keep_value_identity_when_physical_slots_are_reused() {
        use crate::{lexer::Lexer, parser::Parser};
        let source = include_str!("../../../examples/http_bounded_text.verbose");
        for (status, expected) in [
            ("200", Some((200, 200))),
            ("999", Some((999, 999))),
            ("length(req.path)", None),
        ] {
            let source = source
                .replace(
                    "    resp = HttpResponse",
                    "    let dead = 999\n    resp = HttpResponse",
                )
                .replace("status: 200", &format!("status: {status}"))
                .replace("reads : [req]", "reads : [req, req.path]");
            let program = Parser::new(Lexer::new(&source).tokenize().unwrap())
                .parse_program()
                .unwrap();
            let concept = crate::native::http_request_builtin_concept_native(4096);
            let fragment = super::super::prepare(&program, "handle", &concept).unwrap();
            assert_eq!(fragment.http_status_range, expected);
            // The fixed HTTP slots belong to the caller frame, independently
            // of the fragment's relocated words.
            let offsets = HashMap::from([("path", -8)]);
            let text = HashMap::new();
            let mut code = Vec::new();
            let result = fragment.http(&mut code, &offsets, &text);
            if status == "999" {
                assert!(result.unwrap_err().message.contains("status 999"));
            } else {
                result.unwrap();
            }
        }
    }
}
