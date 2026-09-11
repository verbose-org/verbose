//! Linux x86-64 bounded HTTP transport. All persistent state is frame-owned.
use super::transport_asm::Asm;
use crate::http_framing::{self, DIGIT, DONE, LENGTH, START, STATE, TARGET};

const AX: u8 = 0;
const CX: u8 = 1;
const DX: u8 = 2;
const BX: u8 = 3;
const SI: u8 = 6;
const DI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;

/// Scratch region after resource slots, above the receive buffer. No r12 writes.
#[derive(Clone, Copy)]
pub(super) struct Io {
    pub base: i32,
}
impl Io {
    pub const SIZE: i32 = 128;
    fn time(self) -> i32 {
        self.base
    }
    fn deadline(self) -> i32 {
        self.base + 16
    }
    fn poll(self) -> i32 {
        self.base + 24
    }
    fn have(self) -> i32 {
        self.base + 32
    }
    fn scan(self) -> i32 {
        self.base + 40
    }
    fn state(self) -> i32 {
        self.base + 48
    }
    fn length(self) -> i32 {
        self.base + 56
    }
    fn seen(self) -> i32 {
        self.base + 64
    }
    fn total(self) -> i32 {
        self.base + 72
    }
    fn path(self) -> i32 {
        self.base + 96
    }
    fn ptr(self) -> i32 {
        self.base + 80
    }
    fn left(self) -> i32 {
        self.base + 88
    }
    pub fn restore_length(self, code: &mut Vec<u8>) {
        let mut a = Asm::new(code);
        a.load(AX, self.total());
    }
    pub fn timespec(self) -> i32 {
        self.time()
    }
}

impl<'a> Asm<'a> {
    fn now(&mut self, io: Io) {
        self.now_ms(io.time());
    }
    fn start(&mut self, io: Io, seconds: u32) {
        self.now(io);
        self.op(0, AX, (seconds * 1000) as i32);
        self.store(io.deadline(), AX);
    }
    fn remaining(&mut self, io: Io) {
        self.now(io);
        self.load(DX, io.deadline());
        self.rr(0x29, DX, AX);
        self.cmp(DX, 0);
        self.jump(Some(0x8e), 0);
    }
    fn wait(&mut self, io: Io, event: i32, retry: usize) {
        self.remaining(io); // rdx = remaining milliseconds
        self.load(AX, -48);
        self.store(io.poll(), AX);
        // pollfd.events is a short at +4; zero revents together with it.
        self.bytes(&[0xc7, 0x85]);
        self.i32(io.poll() + 4);
        self.i32(event);
        self.lea(DI, io.poll());
        self.imm(SI, 1);
        self.syscall(7);
        self.cmp(AX, -4);
        self.jump(Some(0x84), retry); // EINTR: recompute deadline
        self.cmp(AX, 0);
        self.jump(Some(0x8e), 0);
        self.jump(None, retry); // recv/send decides EOF/error, including HUP
    }
}

pub(super) fn start_response(code: &mut Vec<u8>, io: Io, seconds: u32) -> Vec<usize> {
    let mut a = Asm::new(code);
    a.start(io, seconds);
    a.finish()
}

/// Inputs rsi/rdx = segment span; all six segments share the response deadline.
/// Failure jumps to service close/reset, even with an itoa buffer on the stack.
pub(super) fn send(code: &mut Vec<u8>, io: Io) -> Vec<usize> {
    let mut a = Asm::new(code);
    let retry = a.label();
    let blocked = a.label();
    let done = a.label();
    a.store(io.ptr(), SI);
    a.store(io.left(), DX);
    a.mark(retry);
    a.remaining(io);
    a.load(DX, io.left());
    a.cmp(DX, 0);
    a.jump(Some(0x84), done);
    a.load(DI, -48);
    a.load(SI, io.ptr());
    a.load(DX, io.left());
    a.imm(R10, 0x4040); // MSG_DONTWAIT | MSG_NOSIGNAL
    a.imm(R8, 0);
    a.imm(R9, 0);
    a.syscall(44);
    a.cmp(AX, -4);
    a.jump(Some(0x84), retry);
    a.cmp(AX, -11);
    a.jump(Some(0x84), blocked);
    a.cmp(AX, 0);
    a.jump(Some(0x8e), 0);
    a.load(SI, io.ptr());
    a.rr(0x01, SI, AX);
    a.store(io.ptr(), SI);
    a.load(DX, io.left());
    a.rr(0x29, DX, AX);
    a.store(io.left(), DX);
    a.jump(None, retry);
    a.mark(blocked);
    a.wait(io, 4, retry); // POLLOUT
    a.mark(done);
    a.finish()
}

/// Receive one fully framed request. Return exact request length in rax, so the
/// existing body binding ignores any coalesced bytes belonging to another one.
pub(super) fn receive(
    code: &mut Vec<u8>,
    io: Io,
    buffer: i32,
    capacity: u32,
    seconds: u32,
) -> Vec<usize> {
    let mut a = Asm::new(code);
    let begin = a.label();
    a.jump(None, begin);
    let table = a.code.len();
    for row in http_framing::table() {
        for entry in row {
            a.bytes(&entry.to_le_bytes());
        }
    }
    a.mark(begin);
    a.start(io, seconds);
    a.imm(AX, 0);
    for off in [
        io.have(),
        io.scan(),
        io.length(),
        io.seen(),
        io.total(),
        io.path(),
    ] {
        a.store(off, AX);
    }
    a.imm(AX, START as i32);
    a.store(io.state(), AX);
    let read = a.label();
    let blocked = a.label();
    let scan = a.label();
    let no_target = a.label();
    let no_length = a.label();
    let no_digit = a.label();
    let header_done = a.label();
    let check_body = a.label();
    let complete = a.label();
    a.mark(read);
    a.remaining(io);
    a.load(DI, -48);
    a.lea(SI, buffer);
    a.load(CX, io.have());
    a.rr(0x01, SI, CX);
    a.imm(DX, capacity as i32);
    a.rr(0x29, DX, CX);
    a.cmp(DX, 0);
    a.jump(Some(0x8e), 0);
    a.imm(R10, 0x40);
    a.imm(R8, 0);
    a.imm(R9, 0);
    a.syscall(45); // recvfrom DONTWAIT
    a.cmp(AX, -4);
    a.jump(Some(0x84), read);
    a.cmp(AX, -11);
    a.jump(Some(0x84), blocked);
    a.cmp(AX, 0);
    a.jump(Some(0x8e), 0);
    a.load(CX, io.have());
    a.rr(0x01, CX, AX);
    a.store(io.have(), CX);
    a.load(AX, io.total());
    a.cmp(AX, 0);
    a.jump(Some(0x85), check_body);
    a.mark(scan);
    a.load(CX, io.scan());
    a.load(DX, io.have());
    a.rr(0x39, CX, DX);
    a.jump(Some(0x83), read);
    a.lea(SI, buffer);
    a.rr(0x01, SI, CX);
    a.bytes(&[0x0f, 0xb6, 0x1e]); // movzx ebx, byte [rsi]
    a.op(0, CX, 1);
    a.store(io.scan(), CX);
    a.load(AX, io.state());
    a.bytes(&[0x48, 0xc1, 0xe0, 9]); // shl rax,9
    a.mov(DX, BX);
    a.rr(0x01, DX, DX);
    a.rr(0x01, AX, DX);
    a.rip(SI, table);
    a.rr(0x01, SI, AX);
    a.bytes(&[0x0f, 0xb7, 0x06]); // movzx eax, word [rsi]
    a.cmp(AX, 0);
    a.jump(Some(0x84), 0);
    a.mov(CX, AX);
    a.op(4, CX, STATE as i32);
    a.store(io.state(), CX);
    a.mov(DX, AX);
    a.op(4, DX, TARGET as i32);
    a.cmp(DX, 0);
    a.jump(Some(0x84), no_target);
    a.load(DX, io.path());
    a.op(0, DX, 1);
    a.cmp(DX, 256);
    a.jump(Some(0x87), 0);
    a.store(io.path(), DX);
    a.mark(no_target);
    a.mov(DX, AX);
    a.op(4, DX, LENGTH as i32);
    a.cmp(DX, 0);
    a.jump(Some(0x84), no_length);
    a.load(DX, io.seen());
    a.cmp(DX, 0);
    a.jump(Some(0x85), 0);
    a.imm(DX, 1);
    a.store(io.seen(), DX);
    a.mark(no_length);
    a.op(4, AX, DIGIT as i32);
    a.cmp(AX, 0);
    a.jump(Some(0x84), no_digit);
    a.load(AX, io.length());
    a.bytes(&[0x48, 0x6b, 0xc0, 10]); // imul rax,rax,10
    a.op(5, BX, 48);
    a.rr(0x01, AX, BX);
    a.cmp(AX, capacity as i32);
    a.jump(Some(0x87), 0);
    a.store(io.length(), AX);
    a.mark(no_digit);
    a.cmp(CX, DONE as i32);
    a.jump(Some(0x84), header_done);
    a.jump(None, scan);
    a.mark(header_done);
    a.load(AX, io.scan());
    a.load(DX, io.length());
    a.rr(0x01, AX, DX);
    a.cmp(AX, capacity as i32);
    a.jump(Some(0x87), 0);
    a.store(io.total(), AX);
    a.mark(check_body);
    a.load(CX, io.have());
    a.rr(0x39, CX, AX);
    a.jump(Some(0x83), complete);
    a.jump(None, read);
    a.mark(blocked);
    a.wait(io, 1, read); // POLLIN
    a.mark(complete);
    a.remaining(io);
    a.load(AX, io.total());
    a.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_http_instructions_validate_without_inline_table() {
        let io = Io { base: -256 };
        let mut code = vec![];
        let failures = receive(&mut code, io, -4352, 4096, 2);
        let skip = 5 + i32::from_le_bytes(code[1..5].try_into().unwrap()) as usize;
        assert!(!failures.is_empty());
        crate::validate_x86::validate_code(&code[skip..]).unwrap();
        code.clear();
        start_response(&mut code, io, 2);
        send(&mut code, io);
        crate::validate_x86::validate_code(&code).unwrap();
    }
}
