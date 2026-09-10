const AX: u8 = 0;
const BP: u8 = 5;

/// Small local assembler: rel32 labels and the few register operations used by
/// service transport. Label zero is the failure boundary chosen by the caller.
pub(super) struct Asm<'a> {
    pub(super) code: &'a mut Vec<u8>,
    labels: Vec<Option<usize>>,
    jumps: Vec<(usize, usize)>,
}
impl<'a> Asm<'a> {
    pub(super) fn new(code: &'a mut Vec<u8>) -> Self {
        Self {
            code,
            labels: vec![None],
            jumps: vec![],
        }
    }
    pub(super) fn bytes(&mut self, b: &[u8]) {
        self.code.extend_from_slice(b);
    }
    pub(super) fn i32(&mut self, n: i32) {
        self.bytes(&n.to_le_bytes());
    }
    pub(super) fn label(&mut self) -> usize {
        self.labels.push(None);
        self.labels.len() - 1
    }
    pub(super) fn mark(&mut self, l: usize) {
        assert!(self.labels[l].is_none());
        self.labels[l] = Some(self.code.len());
    }
    pub(super) fn jump(&mut self, cond: Option<u8>, l: usize) {
        if let Some(c) = cond {
            self.bytes(&[0x0f, c]);
        } else {
            self.bytes(&[0xe9]);
        }
        self.jumps.push((self.code.len(), l));
        self.i32(0);
    }
    pub(super) fn finish(self) -> Vec<usize> {
        let mut fail = vec![];
        for (at, label) in self.jumps {
            if label == 0 {
                fail.push(at);
            } else {
                let target = self.labels[label].expect("unbound transport label");
                let rel = i32::try_from(target as i64 - at as i64 - 4).expect("transport rel32");
                self.code[at..at + 4].copy_from_slice(&rel.to_le_bytes());
            }
        }
        fail
    }
    pub(super) fn rex(&mut self, r: u8, b: u8) {
        self.bytes(&[0x48 | ((r >> 3) << 2) | (b >> 3)]);
    }
    pub(super) fn rr(&mut self, op: u8, dst: u8, src: u8) {
        self.rex(src, dst);
        self.bytes(&[op, 0xc0 | ((src & 7) << 3) | (dst & 7)]);
    }
    pub(super) fn mov(&mut self, dst: u8, src: u8) {
        self.rr(0x89, dst, src);
    }
    pub(super) fn imm(&mut self, dst: u8, value: i32) {
        self.rex(0, dst);
        self.bytes(&[0xc7, 0xc0 | (dst & 7)]);
        self.i32(value);
    }
    pub(super) fn op(&mut self, group: u8, dst: u8, value: i32) {
        self.rex(0, dst);
        self.bytes(&[0x81, 0xc0 | (group << 3) | (dst & 7)]);
        self.i32(value);
    }
    pub(super) fn cmp(&mut self, r: u8, v: i32) {
        self.op(7, r, v);
    }
    pub(super) fn load(&mut self, r: u8, offset: i32) {
        self.rex(r, BP);
        self.bytes(&[0x8b, 0x85 | ((r & 7) << 3)]);
        self.i32(offset);
    }
    pub(super) fn store(&mut self, offset: i32, r: u8) {
        self.rex(r, BP);
        self.bytes(&[0x89, 0x85 | ((r & 7) << 3)]);
        self.i32(offset);
    }
    pub(super) fn lea(&mut self, r: u8, offset: i32) {
        self.rex(r, BP);
        self.bytes(&[0x8d, 0x85 | ((r & 7) << 3)]);
        self.i32(offset);
    }
    pub(super) fn rip(&mut self, r: u8, target: usize) {
        self.rex(r, 0);
        self.bytes(&[0x8d, 0x05 | ((r & 7) << 3)]);
        self.i32(i32::try_from(target as i64 - self.code.len() as i64 - 4).unwrap());
    }
    pub(super) fn syscall(&mut self, n: i32) {
        self.imm(AX, n);
        self.bytes(&[0x0f, 0x05]);
    }
}
