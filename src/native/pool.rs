//! Fixed isolated workers. The parent owns PIDs; each worker owns its request frame.
use super::transport_asm::Asm;
mod shutdown;
const AX: u8 = 0;
const CX: u8 = 1;
const DX: u8 = 2;
const SI: u8 = 6;
const DI: u8 = 7;
const R8: u8 = 8;
const R10: u8 = 10;
const LISTENER: u8 = 12;

#[derive(Clone, Copy)]
pub(super) struct Pool {
    pub base: i32,
    pub workers: u32,
    pub shutdown_timeout: Option<u32>,
}
impl Pool {
    pub fn size(workers: u32, timeout: Option<u32>) -> i32 {
        24 + 8 * workers as i32 + if timeout.is_some() { shutdown::SIZE } else { 0 }
    }
    fn parent(self) -> i32 {
        self.base
    }
    fn status(self) -> i32 {
        self.base + 8
    }
    fn exited(self) -> i32 {
        self.base + 16
    }
    fn pid(self, n: u32) -> i32 {
        self.base + 24 + 8 * n as i32
    }
}

pub(super) fn setup(code: &mut Vec<u8>, p: Pool) -> Vec<usize> {
    let mut a = Asm::new(code);
    a.imm(AX, 0);
    a.store(p.status(), AX);
    a.store(p.exited(), AX);
    for n in 0..p.workers {
        a.store(p.pid(n), AX);
    }
    a.syscall(39); // getpid, recorded before fork for the parent-death race check
    a.store(p.parent(), AX);
    let after = a.label();
    a.jump(None, after);
    let action = a.code.len();
    a.bytes(&[0; 32]);
    a.mark(after);
    a.imm(DI, 17);
    a.rip(SI, action);
    a.imm(DX, 0);
    a.imm(R10, 8);
    a.syscall(13); // SIGCHLD = SIG_DFL, including inherited SA_NOCLDWAIT reset
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    let mut failures = a.finish();
    if p.shutdown_timeout.is_some() { failures.extend(shutdown::setup(code, p)); }
    failures
}

fn wait(a: &mut Asm<'_>, status: Option<i32>) {
    a.imm(DI, -1);
    if let Some(slot) = status {
        a.lea(SI, slot);
    } else {
        a.imm(SI, 0);
    }
    a.imm(DX, 0);
    a.imm(R10, 0);
    a.syscall(61);
}

/// The parent never falls through. Every failure after the first fork drains
/// its owned children; only a worker with parent-death protection reaches accept.
pub(super) fn start(code: &mut Vec<u8>, p: Pool) -> Vec<usize> {
    if let Some(seconds) = p.shutdown_timeout { return shutdown::start(code, p, seconds); }
    let mut a = Asm::new(code);
    let child = a.label();
    let cleanup = a.label();
    let supervise = a.label();
    let drain = a.label();
    for n in 0..p.workers {
        a.syscall(57);
        a.cmp(AX, 0);
        a.jump(Some(0x84), child);
        a.jump(Some(0x88), cleanup);
        a.store(p.pid(n), AX);
    }
    a.mov(DI, LISTENER);
    a.syscall(3); // workers, not supervisor, own the accepting descriptors
    a.cmp(AX, 0);
    a.jump(Some(0x88), cleanup);
    a.mark(supervise);
    wait(&mut a, Some(p.status()));
    a.cmp(AX, -4);
    a.jump(Some(0x84), supervise);
    a.cmp(AX, -10);
    a.jump(Some(0x84), 0); // ECHILD: no owned PIDs remain safe to signal
    a.cmp(AX, 0);
    a.jump(Some(0x8e), cleanup);
    a.store(p.exited(), AX);
    a.load(AX, p.status());
    a.op(4, AX, 0x7f);
    a.cmp(AX, 0x7f);
    a.jump(Some(0x84), supervise); // a traced stop is not worker death
    a.load(AX, p.exited());
    a.imm(DX, 0);
    for n in 0..p.workers {
        let next = a.label();
        a.load(CX, p.pid(n));
        a.rr(0x39, CX, AX);
        a.jump(Some(0x85), next);
        a.store(p.pid(n), DX); // remove reaped PID before it could be reused
        a.mark(next);
    }
    a.mark(cleanup);
    for n in 0..p.workers {
        let next = a.label();
        a.load(DI, p.pid(n));
        a.cmp(DI, 0);
        a.jump(Some(0x8e), next); // never kill(0) or signal a reaped PID
        a.imm(SI, 9);
        a.syscall(62);
        a.mark(next);
    }
    a.mark(drain);
    wait(&mut a, None);
    a.cmp(AX, -4);
    a.jump(Some(0x84), drain);
    a.cmp(AX, 0);
    a.jump(Some(0x8f), drain);
    a.jump(None, 0); // exit(1), including unexpected drain errors
    a.mark(child);
    child_setup(&mut a, p);
    a.finish()
}

fn child_setup(a: &mut Asm<'_>, p: Pool) {
    a.imm(DI, 1); // PR_SET_PDEATHSIG
    a.imm(SI, 9);
    a.imm(DX, 0);
    a.imm(R10, 0);
    a.imm(R8, 0);
    a.syscall(157);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.syscall(110); // parent may have died before PR_SET_PDEATHSIG
    a.load(CX, p.parent());
    a.rr(0x39, CX, AX);
    a.jump(Some(0x85), 0);
}

pub(super) fn accept(code: &mut Vec<u8>, p: Pool) -> Vec<usize> {
    let mut a = Asm::new(code);
    let again = a.label();
    a.mark(again);
    a.mov(DI, LISTENER);
    a.imm(SI, 0);
    a.imm(DX, 0);
    a.syscall(43);
    for errno in [4, 11, 103, 100, 71, 92, 112, 64, 113, 95, 101] {
        a.cmp(AX, -errno);
        a.jump(Some(0x84), again);
    }
    if p.shutdown_timeout.is_some() { shutdown::accept_shutdown(&mut a, p); }
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0); // permanent accept failure kills this worker/pool
    a.store(-48, AX);
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pooled_worker_emission_has_fixed_storage_and_valid_instructions() {
        assert_eq!(Pool::size(64, None), 536);
        for workers in [1, 3, 64] {
            let mut code = vec![];
            assert!(!start(
                &mut code,
                Pool {
                    base: -1024,
                    workers,
                    shutdown_timeout: None,
                }
            )
            .is_empty());
            crate::validate_x86::validate_code(&code).unwrap();
            let mut accept_code = vec![];
            assert_eq!(accept(&mut accept_code, Pool { base: -1024, workers, shutdown_timeout: None }).len(), 1);
            crate::validate_x86::validate_code(&accept_code).unwrap();
        }
    }
}
