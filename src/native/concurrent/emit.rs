//! Raw syscall scheduler. No runtime call frames, TLS, heap or detached worker.
use super::*;
use crate::native::transport_asm::Asm;
const AX: u8 = 0;
const CX: u8 = 1;
const DX: u8 = 2;
const BX: u8 = 3;
const SP: u8 = 4;
const BP: u8 = 5;
const SI: u8 = 6;
const DI: u8 = 7;
const EMPTY: i32 = 0;
const READY: i32 = 1;
const DONE: i32 = 2;
const ERROR: i32 = 3;
const CANCEL: i32 = 4;
const CLONE_FLAGS: i32 = 0x0031_0d00; // VM|FILES|SIGHAND|THREAD|PARENT_SETTID|CHILD_CLEARTID
pub(super) struct Emission {
    pub code: Vec<u8>,
    #[cfg(test)]
    pub syscalls: Vec<(usize, i32)>,
    #[cfg(test)]
    pub workers: Vec<(usize, usize)>,
}
// Explicit operands, never rewritten by searching completed instruction bytes.
fn mem(a: &mut Asm<'_>, op: u8, r: u8, base: u8, offset: i32, wide: bool) {
    let rex = 0x40 | if wide { 8 } else { 0 } | ((r >> 3) << 2) | (base >> 3);
    if rex != 0x40 {
        a.bytes(&[rex]);
    }
    a.bytes(&[op, 0x80 | ((r & 7) << 3) | (base & 7)]);
    if base & 7 == 4 {
        a.bytes(&[0x24]);
    }
    a.i32(offset);
}
fn get(a: &mut Asm<'_>, r: u8, base: u8, off: usize) {
    mem(a, 0x8b, r, base, off as i32, true);
}
fn put(a: &mut Asm<'_>, base: u8, off: usize, r: u8) {
    mem(a, 0x89, r, base, off as i32, true);
}
fn addr(a: &mut Asm<'_>, r: u8, base: u8, off: usize) {
    mem(a, 0x8d, r, base, off as i32, true);
}
fn state(a: &mut Asm<'_>) {
    mem(a, 0x8b, AX, 15, 0, false);
}
fn set_state(a: &mut Asm<'_>, value: i32) {
    a.imm(AX, value);
    mem(a, 0x87, AX, 15, 0, false); // xchg memory is locked
}
fn syscall(a: &mut Asm<'_>, n: i32, sites: &mut Vec<(usize, i32)>) {
    sites.push((a.code.len() + 3, n));
    a.syscall(n);
}
fn wake(a: &mut Asm<'_>, sites: &mut Vec<(usize, i32)>) {
    addr(a, DI, 15, 0);
    a.imm(SI, 129);
    a.imm(DX, 1);
    syscall(a, 202, sites);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
}
fn wait(
    a: &mut Asm<'_>,
    sites: &mut Vec<(usize, i32)>,
    offset: usize,
    expected: i32,
    private: bool,
    retry: usize,
) {
    addr(a, DI, 15, offset);
    a.imm(SI, if private { 128 } else { 0 });
    if expected >= 0 {
        a.imm(DX, expected);
    } // otherwise expected TID already in edx
    a.imm(10, 0);
    a.imm(8, 0);
    a.imm(9, 0);
    syscall(a, 202, sites);
    a.cmp(AX, 0);
    a.jump(Some(0x89), retry);
    a.cmp(AX, -4);
    a.jump(Some(0x84), retry); // EINTR
    a.cmp(AX, -11);
    a.jump(Some(0x84), retry); // EAGAIN: state changed before sleep
    a.jump(None, 0);
}
fn publish(a: &mut Asm<'_>, sites: &mut Vec<(usize, i32)>, value: i32, exit: usize) {
    a.imm(AX, EMPTY);
    a.imm(DX, value);
    // lock cmpxchg dword [r15],edx; cancellation wins if it set CANCEL.
    a.bytes(&[0xf0, 0x41, 0x0f, 0xb1, 0x17]);
    a.jump(Some(0x85), exit);
    wake(a, sites);
}
fn protect(a: &mut Asm<'_>, sites: &mut Vec<(usize, i32)>, start: usize, size: usize, fail: usize) {
    addr(a, DI, BP, start);
    a.imm(SI, size as i32);
    a.imm(DX, 3);
    syscall(a, 10, sites);
    a.cmp(AX, 0);
    a.jump(Some(0x88), fail);
}
pub(super) fn compile(p: &Prepared<'_>) -> Result<Emission, NativeError> {
    let mut code = Vec::new();
    let mut sites = Vec::new();
    let mut worker_ranges = Vec::new();
    let mut a = Asm::new(&mut code);
    let cleanup = a.label();
    let mapped_failure = a.label();
    let worker_labels: Vec<_> = p.bodies.iter().map(|_| a.label()).collect();
    // Preserve the kernel entry stack; no coordinator push/call/sub rsp.
    a.mov(12, SP);
    a.imm(DI, 0);
    a.imm(SI, p.report.reserved_bytes as i32);
    a.imm(DX, 0);
    a.imm(10, 0x22);
    a.imm(8, -1);
    a.imm(9, 0);
    syscall(&mut a, 9, &mut sites);
    a.cmp(AX, -4095);
    a.jump(Some(0x83), 0);
    a.mov(BP, AX);
    // Header cannot be written until its protection succeeds. A failure here
    // has no live workers, so terminal exit_group releases the mapping.
    protect(&mut a, &mut sites, 0, p.report.control_reserved, 0);
    put(&mut a, BP, 0, 12);
    a.imm(AX, 0);
    put(&mut a, BP, 8, AX);
    a.imm(AX, 1 << 12);
    put(&mut a, BP, 16, AX); // SIGPIPE bit
    a.imm(DI, 0);
    addr(&mut a, SI, BP, 16);
    a.imm(DX, 0);
    a.imm(10, 8);
    syscall(&mut a, 14, &mut sites);
    a.cmp(AX, 0);
    a.jump(Some(0x88), mapped_failure);
    for l in &p.report.lanes {
        protect(
            &mut a,
            &mut sites,
            l.stack,
            l.stack_reserved,
            mapped_failure,
        );
        addr(&mut a, AX, BP, l.output);
        put(&mut a, BP, l.control + 24, AX);
        addr(&mut a, AX, BP, l.stack + l.stack_reserved);
        put(&mut a, BP, l.control + 32, AX);
    }
    for (wave, phases) in p.bodies.chunks(p.report.max_in_flight).enumerate() {
        let cancel = a.label();
        let join = a.label();
        let after = a.label();
        // Reset every lane before any clone, after the previous wave's joins.
        for l in p.report.lanes.iter().take(phases.len()) {
            addr(&mut a, 15, BP, l.control);
            set_state(&mut a, EMPTY);
        }
        for (lane, _) in phases.iter().enumerate() {
            let l = &p.report.lanes[lane];
            addr(&mut a, 15, BP, l.control);
            a.imm(DI, CLONE_FLAGS);
            get(&mut a, SI, 15, 32);
            addr(&mut a, DX, 15, 4);
            a.mov(10, DX);
            a.imm(8, 0);
            syscall(&mut a, 56, &mut sites);
            a.cmp(AX, 0);
            a.jump(Some(0x88), cancel);
            a.jump(
                Some(0x84),
                worker_labels[wave * p.report.max_in_flight + lane],
            );
        }
        // No publication before the last successful admission above.
        for (lane, _) in phases.iter().enumerate() {
            let l = &p.report.lanes[lane];
            addr(&mut a, 15, BP, l.control);
            let receive = a.label();
            let ready = a.label();
            let done = a.label();
            let next = a.label();
            a.mark(receive);
            state(&mut a);
            a.cmp(AX, READY);
            a.jump(Some(0x84), ready);
            a.cmp(AX, DONE);
            a.jump(Some(0x84), done);
            a.cmp(AX, EMPTY);
            a.jump(Some(0x85), cancel);
            wait(&mut a, &mut sites, 0, EMPTY, true, receive);
            a.mark(ready);
            get(&mut a, BX, 15, 8);
            a.cmp(BX, l.output_bytes as i32);
            a.jump(Some(0x87), cancel);
            get(&mut a, 14, 15, 24);
            let write = a.label();
            let consumed = a.label();
            a.mark(write);
            a.cmp(BX, 0);
            a.jump(Some(0x84), consumed);
            a.imm(DI, 1);
            a.mov(SI, 14);
            a.mov(DX, BX);
            syscall(&mut a, 1, &mut sites);
            a.cmp(AX, -4);
            a.jump(Some(0x84), write);
            a.cmp(AX, 0);
            a.jump(Some(0x8e), cancel);
            a.rr(0x29, BX, AX);
            a.rr(0x01, 14, AX);
            a.jump(None, write);
            a.mark(consumed);
            set_state(&mut a, EMPTY);
            wake(&mut a, &mut sites);
            a.jump(None, receive);
            a.mark(done);
            get(&mut a, AX, 15, 16);
            a.cmp(AX, 0);
            a.jump(Some(0x85), cancel);
            a.jump(None, next);
            a.mark(next);
        }
        a.jump(None, join);
        a.mark(cancel);
        a.imm(AX, 1);
        put(&mut a, BP, 8, AX);
        for l in p.report.lanes.iter().take(phases.len()) {
            addr(&mut a, 15, BP, l.control);
            set_state(&mut a, CANCEL);
            wake(&mut a, &mut sites);
        }
        a.mark(join);
        for l in p.report.lanes.iter().take(phases.len()) {
            addr(&mut a, 15, BP, l.control);
            let retry = a.label();
            let joined = a.label();
            a.mark(retry);
            mem(&mut a, 0x8b, DX, 15, 4, false);
            a.cmp(DX, 0);
            a.jump(Some(0x84), joined);
            // The kernel clear-TID wake uses the shared (non-private) futex key.
            wait(&mut a, &mut sites, 4, -1, false, retry);
            a.mark(joined);
        }
        get(&mut a, AX, BP, 8);
        a.cmp(AX, 0);
        a.jump(Some(0x85), cleanup);
        a.jump(None, after);
        a.mark(after);
    }
    a.jump(None, cleanup);
    a.mark(mapped_failure);
    a.imm(AX, 1);
    put(&mut a, BP, 8, AX);
    a.mark(cleanup);
    get(&mut a, 12, BP, 8);
    a.mov(DI, BP);
    a.imm(SI, p.report.reserved_bytes as i32);
    syscall(&mut a, 11, &mut sites);
    a.cmp(AX, 0);
    a.jump(Some(0x88), 0);
    a.mov(DI, 12);
    syscall(&mut a, 231, &mut sites);
    a.mark(0);
    a.imm(DI, 1);
    syscall(&mut a, 231, &mut sites);
    for (index, body) in p.bodies.iter().enumerate() {
        a.mark(worker_labels[index]);
        let start = a.code.len();
        worker(&mut a, &mut sites, body, p.concept);
        worker_ranges.push((start, a.code.len()));
    }
    assert!(a.finish().is_empty());
    if code.len() > 64 * 1024 * 1024 {
        return Err(error("combined code exceeds 64 MiB"));
    }
    validate_graph(&code)?;
    Ok(Emission {
        code,
        #[cfg(test)]
        syscalls: sites,
        #[cfg(test)]
        workers: worker_ranges,
    })
}
fn worker(a: &mut Asm<'_>, sites: &mut Vec<(usize, i32)>, body: &Body, concept: &Concept) {
    let exit = a.label();
    let fail = a.label();
    let done = a.label();
    let next = a.label();
    // clone supplies the distinct checked stack. r12 is the immutable original
    // entry stack; r15 is our lane. No return address or C ABI frame exists.
    a.mov(BP, SP);
    a.op(5, SP, body.frame_bytes as i32);
    a.mov(13, 12);
    a.op(0, 13, 8);
    get(a, 12, 12, 0);
    a.imm(14, 1);
    a.imm(AX, 0);
    a.store(-(body.frame_bytes as i32), AX);
    a.cmp(12, (concept.fields.len() + 1) as i32);
    a.jump(Some(0x8c), fail);
    a.mark(next);
    state(a);
    a.cmp(AX, CANCEL);
    a.jump(Some(0x84), exit);
    a.rr(0x39, 14, 12);
    a.jump(Some(0x8d), done);
    a.mov(AX, 14);
    a.op(0, AX, concept.fields.len() as i32);
    a.rr(0x39, AX, 12);
    a.jump(Some(0x8f), fail);
    let mut aborts = Vec::new();
    for (i, f) in concept.fields.iter().enumerate() {
        a.bytes(&[0x4b, 0x8b, 0x7c, 0xf5, 0]); // argv[r14]
        match f.ty {
            Type::Number => {
                emit_checked_atoi_inline(a.code, &mut aborts);
                if let Some((lo, hi)) = f.range {
                    emit_bounds_check(a.code, lo, hi, &mut aborts);
                }
                a.store(-((i as i32 + 1) * 8), AX);
            }
            Type::Text => {
                if let Some(max) = text_field_declared_max(f) {
                    emit_text_bound_check(a.code, max, &mut aborts);
                }
                a.store(-((i as i32 + 1) * 8), DI);
            }
            _ => unreachable!("closed phase input gate"),
        }
        a.op(0, 14, 1);
    }
    a.bytes(&body.code);
    publish(a, sites, READY, exit);
    let pending = a.label();
    a.mark(pending);
    state(a);
    a.cmp(AX, CANCEL);
    a.jump(Some(0x84), exit);
    a.cmp(AX, EMPTY);
    a.jump(Some(0x84), next);
    a.cmp(AX, READY);
    a.jump(Some(0x85), 0);
    wait(a, sites, 0, READY, true, pending);
    a.mark(done);
    a.load(AX, -(body.frame_bytes as i32));
    put(a, 15, 16, AX);
    publish(a, sites, DONE, exit);
    a.jump(None, exit);
    a.mark(fail);
    for at in aborts {
        let delta = a.code.len() as i32 - at as i32 - 4;
        a.code[at..at + 4].copy_from_slice(&delta.to_le_bytes());
    }
    publish(a, sites, ERROR, exit);
    a.mark(exit);
    a.imm(DI, 0);
    syscall(a, 60, sites);
}

// Follow both branch arms, skipping embedded literal data by control flow.
// This checks instruction boundaries in the closed, call-free worker/scheduler
// code. It is not a proof of x86 semantics or a general executable validator.
pub(super) fn validate_graph(code: &[u8]) -> Result<(), NativeError> {
    let mut visited = vec![0u8; code.len()];
    let mut work = vec![0usize];
    while let Some(pc) = work.pop() {
        if pc == code.len() {
            continue;
        }
        if pc > code.len() || visited[pc] == 2 {
            return Err(error("invalid instruction/branch boundary"));
        }
        if visited[pc] == 1 {
            continue;
        }
        let len = crate::validate_x86::decode_instruction_length(code, pc)
            .filter(|len| *len > 0 && *len <= code.len() - pc)
            .ok_or_else(|| error(format!("cannot decode instruction at {pc}")))?;
        if visited[pc + 1..pc + len].iter().any(|v| *v != 0) {
            return Err(error("overlapping instruction boundaries"));
        }
        visited[pc] = 1;
        visited[pc + 1..pc + len].fill(2);
        let ins = &code[pc..pc + len];
        if matches!(ins[0], 0xe8 | 0xc3) {
            return Err(error(
                "unexpected runtime call/return in fixed worker layout",
            ));
        }
        let next = pc + len;
        let branch = match ins {
            [0xeb, n] | [0x70..=0x7f, n] => Some(*n as i8 as i64),
            [0xe9, a, b, c, d] | [0x0f, 0x80..=0x8f, a, b, c, d] => {
                Some(i32::from_le_bytes([*a, *b, *c, *d]) as i64)
            }
            _ => None,
        };
        if let Some(d) = branch {
            let target = next as i64 + d;
            if target < 0 || target > code.len() as i64 {
                return Err(error("branch outside emitted code"));
            }
            work.push(target as usize);
        }
        if !matches!(ins[0], 0xe9 | 0xeb) {
            work.push(next);
        }
    }
    Ok(())
}
