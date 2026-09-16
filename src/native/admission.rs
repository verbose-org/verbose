//! One parent owns admission and waitable children. No shared mutable state.
use super::transport_asm::Asm;
const AX: u8 = 0;
const CX: u8 = 1;
const DX: u8 = 2;
const SI: u8 = 6;
const DI: u8 = 7;
const R10: u8 = 10;
const LISTENER: u8 = 12;

#[derive(Clone, Copy)]
pub(super) struct Admission {
    pub base: i32,
}
impl Admission {
    pub const SIZE: i32 = 24;
    fn count(self) -> i32 {
        self.base
    }
    fn poll(self) -> i32 {
        self.base + 8
    }
    fn status(self) -> i32 {
        self.base + 16
    }
}

/// All failures here stop the current process via the existing exit(1) tail.
pub(super) fn check_syscall(code: &mut Vec<u8>) -> Vec<usize> {
    let mut a = Asm::new(code);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.finish()
}

pub(super) fn setup(code: &mut Vec<u8>, slots: Admission) -> Vec<usize> {
    let mut a = Asm::new(code);
    a.imm(AX, 0);
    a.store(slots.count(), AX);
    a.store(slots.status(), AX);
    // O_NONBLOCK belongs to the LISTENER; accept4 flags alone are insufficient.
    a.mov(DI, LISTENER);
    a.imm(SI, 3);
    a.syscall(72); // F_GETFL
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.mov(DX, AX);
    a.op(1, DX, 2048); // OR O_NONBLOCK
    a.mov(DI, LISTENER);
    a.imm(SI, 4);
    a.syscall(72); // F_SETFL
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    // Reset inherited SIG_IGN / SA_NOCLDWAIT, otherwise exits cannot be counted.
    let after = a.label();
    a.jump(None, after);
    let action = a.code.len();
    a.bytes(&[0; 32]);
    a.mark(after);
    a.imm(DI, 17);
    a.rip(SI, action);
    a.imm(DX, 0);
    a.imm(R10, 8);
    a.syscall(13); // rt_sigaction(SIGCHLD, SIG_DFL, NULL, 8)
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.finish()
}

fn reap(a: &mut Asm<'_>, slots: Admission) {
    let again = a.label();
    let no_children = a.label();
    let done = a.label();
    a.mark(again);
    a.imm(DI, -1);
    a.lea(SI, slots.status());
    a.imm(DX, 1);
    a.imm(R10, 0);
    a.syscall(61); // wait4(-1, &status, WNOHANG, NULL)
    a.cmp(AX, -4);
    a.jump(Some(0x84), again); // EINTR preserves accounting
    a.cmp(AX, -10);
    a.jump(Some(0x84), no_children); // ECHILD
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.jump(Some(0x84), done);
    // A ptraced child can report a stop even without WUNTRACED. A stop does
    // not free a slot. All other returned wait statuses here denote exit/death.
    a.load(AX, slots.status());
    a.op(4, AX, 0x7f);
    a.cmp(AX, 0x7f);
    a.jump(Some(0x84), again);
    a.load(CX, slots.count());
    a.cmp(CX, 0);
    a.jump(Some(0x8e), 0);
    a.op(5, CX, 1);
    a.store(slots.count(), CX);
    a.jump(None, again);
    a.mark(no_children);
    a.load(CX, slots.count());
    a.cmp(CX, 0);
    a.jump(Some(0x85), 0);
    a.mark(done);
}

/// Parent loops entirely within this block. Only an admitted child falls through.
/// No temporary stack allocations cross the fork or any failure edge.
pub(super) fn dispatch(code: &mut Vec<u8>, slots: Admission, limit: u32) -> Vec<usize> {
    let mut a = Asm::new(code);
    let top = a.label();
    let close = a.label();
    let child = a.label();
    a.mark(top);
    reap(&mut a, slots);
    a.mov(AX, LISTENER);
    a.store(slots.poll(), AX);
    a.bytes(&[0xc7, 0x85]);
    a.i32(slots.poll() + 4);
    a.i32(1); // POLLIN; clear revents
    a.lea(DI, slots.poll());
    a.imm(SI, 1);
    a.imm(DX, 100);
    a.syscall(7);
    a.cmp(AX, -4);
    a.jump(Some(0x84), top);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.jump(Some(0x84), top);
    a.bytes(&[0x0f, 0xb7, 0x85]);
    a.i32(slots.poll() + 6); // revents
    a.mov(CX, AX);
    a.op(4, CX, 0x38);
    a.cmp(CX, 0);
    a.jump(Some(0x85), 0);
    a.op(4, AX, 1);
    a.cmp(AX, 0);
    a.jump(Some(0x84), top);
    a.mov(DI, LISTENER);
    a.imm(SI, 0);
    a.imm(DX, 0);
    a.syscall(43);
    // Linux documents these pending network errors as retryable at accept.
    for errno in [4, 11, 103, 100, 71, 92, 112, 64, 113, 95, 101] {
        a.cmp(AX, -errno);
        a.jump(Some(0x84), top);
    }
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0); // never fork after a failed accept
    a.store(-48, AX);
    reap(&mut a, slots); // account for exits occurring during poll/accept
    a.load(AX, slots.count());
    a.cmp(AX, limit as i32);
    a.jump(Some(0x83), close); // full: no child, handler, or request effects
    a.syscall(57);
    a.cmp(AX, 0);
    a.jump(Some(0x84), child);
    a.jump(Some(0x88), close);
    // Increment only after the parent's successful fork return.
    a.load(AX, slots.count());
    a.op(0, AX, 1);
    a.store(slots.count(), AX);
    a.mark(close);
    a.load(DI, -48);
    a.syscall(3);
    a.jump(None, top);
    a.mark(child);
    // Parent is the sole listener owner; children need only their client fd.
    a.mov(DI, LISTENER);
    a.syscall(3);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_admission_instructions_and_failure_edges() {
        let mut code = vec![];
        let failures = dispatch(&mut code, Admission { base: -224 }, 3);
        assert!(failures.len() >= 6);
        crate::validate_x86::validate_code(&code).unwrap();
        // Two reaping loops, one nonblocking accept, and one fork site.
        let syscall = |n: i32| {
            [
                vec![0x48, 0xc7, 0xc0],
                n.to_le_bytes().to_vec(),
                vec![0x0f, 0x05],
            ]
            .concat()
        };
        for (n, expected) in [(61, 2), (43, 1), (57, 1), (7, 1)] {
            assert_eq!(
                code.windows(9).filter(|w| *w == syscall(n)).count(),
                expected
            );
        }
        for n in [9, 12, 25] {
            assert!(!code.windows(9).any(|w| w == syscall(n)));
        }
    }
}
