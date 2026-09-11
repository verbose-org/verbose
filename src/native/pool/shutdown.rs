//! Optional supervisor SIGTERM lifetime. No asynchronous handler/shared flag.
use super::*;

pub(super) const SIZE: i32 = 72;
const MASK: i32 = 0;
const DRAINING: i32 = 8;
const LIVE: i32 = 16;
const DEADLINE: i32 = 24;
const TIME: i32 = 32; // timespec
const WAIT: i32 = 48; // relative timespec
const SOCKET: i32 = 64; // int SO_ACCEPTCONN, socklen_t

fn slot(p: Pool, field: i32) -> i32 {
    p.base + 24 + 8 * p.workers as i32 + field
}

fn exit(a: &mut Asm<'_>, status: i32) {
    a.imm(DI, status);
    a.syscall(60);
}

pub(super) fn setup(code: &mut Vec<u8>, p: Pool) -> Vec<usize> {
    let mut a = Asm::new(code);
    a.imm(AX, 0);
    for field in (0..SIZE).step_by(8) {
        a.store(slot(p, field), AX);
    }
    a.imm(AX, p.workers as i32);
    a.store(slot(p, LIVE), AX);
    a.imm(AX, (1 << 14) | (1 << 16)); // SIGTERM | SIGCHLD
    a.store(slot(p, MASK), AX);
    a.imm(DI, 0); // SIG_BLOCK, preserving the rest of the inherited mask
    a.lea(SI, slot(p, MASK));
    a.imm(DX, 0);
    a.imm(R10, 8);
    a.syscall(14);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    // An inherited SIG_IGN must not silently discard the shutdown request.
    let after = a.label();
    a.jump(None, after);
    let action = a.code.len();
    a.bytes(&[0; 32]);
    a.mark(after);
    a.imm(DI, 15);
    a.rip(SI, action);
    a.imm(DX, 0);
    a.imm(R10, 8);
    a.syscall(13);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.finish()
}

fn remaining(a: &mut Asm<'_>, p: Pool) {
    a.now_ms(slot(p, TIME));
    a.load(DX, slot(p, DEADLINE));
    a.rr(0x29, DX, AX);
    a.cmp(DX, 0);
    a.jump(Some(0x8e), 0); // fixed deadline expired => force cleanup
}

pub(super) fn start(code: &mut Vec<u8>, p: Pool, seconds: u32) -> Vec<usize> {
    let mut a = Asm::new(code);
    let child = a.label();
    let reap = a.label();
    let check_children = a.label();
    let wait_signal = a.label();
    let timed_wait = a.label();
    let invoke_wait = a.label();
    let no_children = a.label();
    let known = a.label();
    let failure_exit = a.label();
    for n in 0..p.workers {
        a.syscall(57);
        a.cmp(AX, 0);
        a.jump(Some(0x84), child);
        a.jump(Some(0x88), 0);
        a.store(p.pid(n), AX);
    }
    // Retain the listener in the supervisor until shutdown. Never accept here.
    a.mark(reap);
    a.load(AX, slot(p, DRAINING));
    a.cmp(AX, 0);
    a.jump(Some(0x84), check_children);
    a.load(AX, slot(p, LIVE));
    a.cmp(AX, 0);
    let still_live = a.label();
    a.jump(Some(0x85), still_live);
    exit(&mut a, 0);
    a.mark(still_live);
    remaining(&mut a, p);
    a.mark(check_children);
    a.imm(DI, -1);
    a.lea(SI, p.status());
    a.imm(DX, 1); // WNOHANG
    a.imm(R10, 0);
    a.syscall(61);
    a.cmp(AX, -4);
    a.jump(Some(0x84), reap);
    a.cmp(AX, -10);
    a.jump(Some(0x84), no_children);
    a.cmp(AX, 0);
    a.jump(Some(0x84), wait_signal);
    a.jump(Some(0x88), 0);
    a.store(p.exited(), AX);
    a.load(AX, p.status());
    a.op(4, AX, 0x7f);
    a.cmp(AX, 0x7f);
    a.jump(Some(0x84), reap); // a traced stop is not an exit
    a.load(AX, p.exited());
    a.imm(DX, 0);
    for n in 0..p.workers {
        let next = a.label();
        a.load(CX, p.pid(n));
        a.rr(0x39, CX, AX);
        a.jump(Some(0x85), next);
        a.store(p.pid(n), DX); // remove before any failure can signal survivors
        a.jump(None, known);
        a.mark(next);
    }
    a.jump(None, 0); // wait returned an unowned PID
    a.mark(known);
    a.load(AX, slot(p, LIVE));
    a.op(5, AX, 1);
    a.store(slot(p, LIVE), AX);
    a.load(AX, slot(p, DRAINING));
    a.cmp(AX, 0);
    a.jump(Some(0x84), 0); // every exit while serving is unexpected
    a.load(AX, p.status());
    a.cmp(AX, 0);
    a.jump(Some(0x85), 0); // grace does not hide worker failure
    a.jump(None, reap);

    a.mark(wait_signal);
    a.load(AX, slot(p, DRAINING));
    a.cmp(AX, 0);
    a.jump(Some(0x85), timed_wait);
    a.imm(DX, 0); // Linux NULL timeout = indefinite signal wait
    a.jump(None, invoke_wait);
    a.mark(timed_wait);
    remaining(&mut a, p);
    a.mov(AX, DX);
    a.imm(DX, 0);
    a.imm(CX, 1000);
    a.bytes(&[0x48, 0xf7, 0xf1]); // div rcx => seconds, remainder ms
    a.store(slot(p, WAIT), AX);
    a.imm(CX, 1_000_000);
    a.bytes(&[0x48, 0x0f, 0xaf, 0xd1]); // imul rdx,rcx => nanoseconds
    a.store(slot(p, WAIT) + 8, DX);
    a.lea(DX, slot(p, WAIT));
    a.mark(invoke_wait);
    a.lea(DI, slot(p, MASK));
    a.imm(SI, 0); // no siginfo buffer needed
    a.imm(R10, 8);
    a.syscall(128); // rt_sigtimedwait
    for value in [-4, -11, 17] {
        // interruption, timeout, child notification
        a.cmp(AX, value);
        a.jump(Some(0x84), reap);
    }
    a.cmp(AX, 15);
    a.jump(Some(0x85), 0);
    a.load(AX, slot(p, DRAINING));
    a.cmp(AX, 0);
    a.jump(Some(0x85), reap); // repeated SIGTERM never resets the deadline
    a.now_ms(slot(p, TIME));
    a.op(0, AX, (seconds * 1000) as i32);
    a.store(slot(p, DEADLINE), AX);
    a.imm(AX, 1);
    a.store(slot(p, DRAINING), AX);
    a.mov(DI, LISTENER);
    a.imm(SI, 0); // SHUT_RD disables the shared TCP listener
    a.syscall(48);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.jump(None, reap);

    a.mark(no_children);
    // ECHILD means none of the recorded PIDs are safe to signal. LIVE is
    // decremented only by successful wait4; disagreement is an operational error.
    a.jump(None, failure_exit);
    a.mark(0); // parent-only failure boundary, including clock/syscall failures
    for n in 0..p.workers {
        let next = a.label();
        a.load(DI, p.pid(n));
        a.cmp(DI, 0);
        a.jump(Some(0x8e), next);
        a.imm(SI, 9);
        a.syscall(62);
        a.mark(next);
    }
    let drain = a.label();
    a.mark(drain);
    wait(&mut a, None);
    a.cmp(AX, -4);
    a.jump(Some(0x84), drain);
    a.cmp(AX, 0);
    a.jump(Some(0x8f), drain);
    a.mark(failure_exit);
    exit(&mut a, 1);
    a.mark(child);
    assert!(a.finish().is_empty());
    // A worker setup failure exits that worker. It must NOT run the parent's
    // PID cleanup using its inherited snapshot of sibling PIDs.
    let mut a = Asm::new(code);
    child_setup(&mut a, p);
    a.finish()
}

pub(super) fn accept_shutdown(a: &mut Asm<'_>, p: Pool) {
    let accepted = a.label();
    a.cmp(AX, -22); // Linux accept on a disabled listener
    a.jump(Some(0x85), accepted);
    a.imm(AX, 0);
    a.store(slot(p, SOCKET), AX);
    a.bytes(&[0xc7, 0x85]); // socklen_t is 32 bits
    a.i32(slot(p, SOCKET) + 4);
    a.i32(4);
    a.mov(DI, LISTENER);
    a.imm(SI, 1); // SOL_SOCKET
    a.imm(DX, 30); // SO_ACCEPTCONN
    a.lea(R10, slot(p, SOCKET));
    a.lea(R8, slot(p, SOCKET) + 4);
    a.syscall(55);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.bytes(&[0x8b, 0x85]); // zero-extended 32-bit option value
    a.i32(slot(p, SOCKET));
    a.cmp(AX, 0);
    a.jump(Some(0x85), 0); // EINVAL on a live listener remains fatal
    exit(a, 0);
    a.mark(accepted);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_emission_is_bounded_and_decodes() {
        assert_eq!(Pool::size(64, Some(3600)), 608);
        for workers in [1, 4, 64] {
            let p = Pool {
                base: -2048,
                workers,
                shutdown_timeout: Some(3600),
            };
            for emit in [
                super::super::setup,
                super::super::start,
                super::super::accept,
            ] {
                let mut code = vec![];
                assert!(!emit(&mut code, p).is_empty());
                crate::validate_x86::validate_code(&code).unwrap();
            }
        }
    }
}
