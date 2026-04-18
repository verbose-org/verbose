/// x86-64 instruction validator — verifies that emitted machine code is well-formed.
///
/// This is the compiler verifying ITSELF. Just as the verifier checks the AI's proofs,
/// this module checks that the native backend emits valid instructions.
///
/// The validator walks the byte stream and decodes each instruction's structure:
///   - REX prefix (if present)
///   - Opcode (1-3 bytes)
///   - ModRM + SIB (if present)
///   - Displacement and immediate operands
///
/// If any instruction doesn't decode properly, it reports the byte offset and the
/// invalid bytes. This catches encoding bugs (like the REX.X incident) at compile
/// time instead of at runtime (crash or silent corruption).
///
/// EB-rel8 handling:
/// - The emitter uses `EB n` (forward) for two purposes: jmp-over-inline-data
///   (the string literal embedded after the jmp) and jmp-over-code (e.g.
///   skipping a "false" branch in bool output). For BOTH cases, advancing
///   `2 + n` bytes lands on a real instruction boundary — the emitter's
///   invariant is that every jmp targets the start of an instruction, so
///   skipping is safe whether the skipped bytes are data or code.
/// - The emitter also uses `EB n` for BACKWARD jumps (sign bit set in n,
///   read as i8) inside tight loops like `emit_atoi_inline`'s digit-parse
///   loop and `emit_itoa_inline`'s div loop. These must NOT be treated as
///   data skips — the displacement is negative, the byte sequence after the
///   jmp is real code, and unsigned-treating would skip 200+ bytes forward
///   and land mid-instruction (the original false-positive root cause).
///   Backward EB falls through to the general decoder, which handles it
///   correctly as a 2-byte short jump.
///
/// Crédo: a false positive (rejecting valid code) damages the verifier's
/// credibility. Any new opcode added to the decoder must be verified to
/// correctly decode the actual emitted bytes; a partial decoder that drifts
/// is worse than no decoder at all.

#[derive(Debug)]
pub struct ValidationError {
    pub offset: usize,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid x86-64 at offset {}: {}", self.offset, self.message)
    }
}

/// Validate a sequence of x86-64 machine code bytes.
/// Returns Ok(instruction_count) or Err(first_error).
pub fn validate_code(code: &[u8]) -> Result<usize, ValidationError> {
    let mut pos = 0;
    let mut count = 0;

    while pos < code.len() {
        let start = pos;

        // Detect jmp-over-data pattern (EB xx): the bytes after the jmp may be
        // inline string data, not instructions. Only applies to FORWARD jumps
        // (positive i8 displacement). Backward EB jumps (xx >= 0x80 when read
        // as i8) are real short jumps — e.g. the tight loops inside
        // emit_atoi_inline and emit_itoa_*. Falling through to the general
        // decoder treats those correctly as a 2-byte short jmp.
        //
        // For a forward EB, skipping `disp` bytes is safe even if the bytes
        // happen to be real code (not data): the emitter's invariant is that
        // every jmp targets the START of a real instruction, so the skip lands
        // on an instruction boundary regardless of what the skipped bytes are.
        if code[pos] == 0xEB && pos + 1 < code.len() {
            let disp = code[pos + 1] as i8;
            if disp >= 0 {
                pos += 2 + disp as usize;
                count += 1;
                continue;
            }
        }

        match decode_instruction_length(code, pos) {
            Some(len) if len > 0 && pos + len <= code.len() => {
                pos += len;
                count += 1;
            }
            Some(0) => {
                return Err(ValidationError {
                    offset: start,
                    message: format!("zero-length instruction at byte 0x{:02X}", code[start]),
                });
            }
            _ => {
                let end = (start + 8).min(code.len());
                let bytes: Vec<String> = code[start..end].iter().map(|b| format!("{:02X}", b)).collect();
                return Err(ValidationError {
                    offset: start,
                    message: format!("cannot decode instruction: {}", bytes.join(" ")),
                });
            }
        }
    }

    Ok(count)
}

/// Decode the length of one x86-64 instruction starting at `code[pos]`.
/// Returns None if the instruction can't be decoded.
fn decode_instruction_length(code: &[u8], pos: usize) -> Option<usize> {
    if pos >= code.len() {
        return None;
    }

    let mut i = pos;

    // Legacy prefixes (66, F2, F3, etc.)
    let mut has_66 = false;
    while i < code.len() {
        match code[i] {
            0x66 => { has_66 = true; i += 1; }
            0xF2 | 0xF3 | 0x26 | 0x2E | 0x36 | 0x3E | 0x64 | 0x65 | 0xF0 => i += 1,
            _ => break,
        }
    }
    if i >= code.len() {
        return None;
    }

    // REX prefix (0x40-0x4F)
    let has_rex = code[i] >= 0x40 && code[i] <= 0x4F;
    if has_rex {
        i += 1;
        if i >= code.len() {
            return None;
        }
    }

    // Opcode
    let opcode = code[i];
    i += 1;

    // 2-byte opcode (0F xx)
    if opcode == 0x0F {
        if i >= code.len() {
            return None;
        }
        let op2 = code[i];
        i += 1;

        // 3-byte opcode (0F 38 xx or 0F 3A xx)
        if op2 == 0x38 || op2 == 0x3A {
            if i >= code.len() {
                return None;
            }
            i += 1; // third opcode byte
            // These typically have ModRM
            i += modrm_length(code, i)?;
            return Some(i - pos);
        }

        // 2-byte opcodes with ModRM
        match op2 {
            // Conditional jumps (Jcc rel32)
            0x80..=0x8F => {
                if i + 4 > code.len() {
                    return None;
                }
                return Some(i + 4 - pos);
            }
            // SETcc (0F 9x /r)
            0x90..=0x9F => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // MOVZX, MOVSX
            0xB6 | 0xB7 | 0xBE | 0xBF => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // IMUL r, r/m
            0xAF => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // MOVMSKPD
            0x50 => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // MOVDQU (F3 0F 6F)
            0x6F => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // MOVQ (66 0F 6E), PUNPCKLQDQ (66 0F 6C)
            0x6E | 0x6C => {
                i += modrm_length(code, i)?;
                return Some(i - pos);
            }
            // SYSCALL
            0x05 => return Some(i - pos),
            _ => {
                // Unknown 2-byte opcode — try assuming ModRM
                if i < code.len() {
                    i += modrm_length(code, i)?;
                }
                return Some(i - pos);
            }
        }
    }

    // 1-byte opcodes
    match opcode {
        // NOP
        0x90 => Some(i - pos),

        // ADD r/m8, r8 (00 /r) + r/m, r (01 /r)
        0x00 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // ADD *AX, imm32 (05 id) — with REX.W this is ADD RAX, imm32 sign-extended
        0x05 => {
            if i + 4 > code.len() { return None; }
            Some(i + 4 - pos)
        }
        // String instructions (usually preceded by F3/F2 prefix for rep/repne):
        // MOVSB (A4), MOVSD (A5), CMPSB (A6), CMPSD (A7),
        // STOSB (AA), STOSD (AB), LODSB (AC), LODSD (AD),
        // SCASB (AE), SCASD (AF) — all are 1-byte opcodes with no operands
        // (they implicitly use RDI/RSI/RCX/RAX).
        0xA4 | 0xA5 | 0xA6 | 0xA7 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE => Some(i - pos),
        // CLD (FC) / STD (FD) — 1-byte, no operands
        0xFC | 0xFD => Some(i - pos),
        // Shift r/m8, imm8 (C0 /r ib)
        0xC0 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // Shift r/m, CL (D3 /r)
        0xD3 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }

        // PUSH r64 (50+r)
        0x50..=0x57 => Some(i - pos),
        // POP r64 (58+r)
        0x58..=0x5F => Some(i - pos),

        // Short jump (EB rel8)
        0xEB => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // Short conditional jumps (7x rel8)
        0x70..=0x7F => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // Near jump (E9 rel32)
        0xE9 => {
            if i + 4 > code.len() { return None; }
            Some(i + 4 - pos)
        }
        // CALL rel32
        0xE8 => {
            if i + 4 > code.len() { return None; }
            Some(i + 4 - pos)
        }
        // RET
        0xC3 => Some(i - pos),

        // MOV r/m, imm8 (C6 /0)
        0xC6 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos) // + imm8
        }
        // MOV r/m, imm32 (C7 /0) — with 66h prefix, imm16
        0xC7 => {
            i += modrm_length(code, i)?;
            let imm_size = if has_66 { 2 } else { 4 };
            if i + imm_size > code.len() { return None; }
            Some(i + imm_size - pos)
        }

        // ALU r/m, r (ADD=01, OR=09, AND=21, SUB=29, XOR=31, CMP=39)
        0x01 | 0x09 | 0x21 | 0x29 | 0x31 | 0x39 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // ALU r, r/m (ADD=03, SUB=2B, XOR=33, CMP=3B, MOV=8B)
        0x03 | 0x2B | 0x33 | 0x3B | 0x8B => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // MOV r/m, r (89)
        0x89 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // LEA (8D)
        0x8D => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // TEST r/m8, imm8 (F6 /0)
        0xF6 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // IDIV, MUL, etc (F7 /r)
        0xF7 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // INC/DEC/CALL/JMP/PUSH (FF /r)
        0xFF => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // CMP rax, imm32 (3D)
        0x3D => {
            if i + 4 > code.len() { return None; }
            Some(i + 4 - pos)
        }
        // TEST al, imm8 (A8)
        0xA8 => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // TEST eax, imm32 (A9)
        0xA9 => {
            if i + 4 > code.len() { return None; }
            Some(i + 4 - pos)
        }
        // MOV r64, imm64 (B8+r) — REX.W makes this 64-bit
        0xB8..=0xBF => {
            let imm_size = if has_rex { 8 } else { 4 };
            if i + imm_size > code.len() { return None; }
            Some(i + imm_size - pos)
        }
        // PUSH imm8 (6A)
        0x6A => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // ALU r/m, imm8 (83 /r)
        0x83 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // ALU r/m, imm32 (81 /r)
        // ALU r/m, imm32 (81 /r) — with 66h prefix, imm16
        0x81 => {
            i += modrm_length(code, i)?;
            let imm_size = if has_66 { 2 } else { 4 };
            if i + imm_size > code.len() { return None; }
            Some(i + imm_size - pos)
        }
        // Shift r/m, imm8 (C1 /r)
        0xC1 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // Shift r/m, 1 (D1 /r)
        0xD1 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // IMUL r, r/m, imm8 (6B /r)
        0x6B => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // CQO / CDQ (99)
        0x99 => Some(i - pos),
        // TEST r/m, r (84/85)
        0x84 | 0x85 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // CMP r/m8, imm8 (80 /7)
        0x80 => {
            i += modrm_length(code, i)?;
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // MOV r8, imm8 (B0+r)
        0xB0..=0xB7 => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }
        // MOV r/m8, r8 (88)
        0x88 => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // MOV r8, r/m8 (8A) — e.g. mov al, [rcx]
        0x8A => {
            i += modrm_length(code, i)?;
            Some(i - pos)
        }
        // CMP AL, imm8 (3C) — e.g. cmp al, 0x20 (whitespace check)
        0x3C => {
            if i >= code.len() { return None; }
            Some(i + 1 - pos)
        }

        _ => None, // Unknown opcode
    }
}

/// Calculate the length of a ModRM byte + optional SIB + displacement.
fn modrm_length(code: &[u8], pos: usize) -> Option<usize> {
    if pos >= code.len() {
        return None;
    }
    let modrm = code[pos];
    let md = (modrm >> 6) & 3;
    let rm = modrm & 7;
    let mut len = 1; // ModRM byte itself

    if md != 3 && rm == 4 {
        len += 1; // SIB byte
    }

    match md {
        0 => {
            if rm == 5 {
                len += 4; // RIP-relative (disp32)
            } else if rm == 4 {
                // SIB with mod=00: check SIB base
                if pos + 1 < code.len() {
                    let sib_base = code[pos + 1] & 7;
                    if sib_base == 5 {
                        len += 4; // disp32
                    }
                }
            }
        }
        1 => len += 1,  // disp8
        2 => len += 4,  // disp32
        3 => {}          // register direct, no displacement
        _ => {}
    }

    Some(len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_simple_program() {
        // mov rax, 60; xor rdi, rdi; syscall
        let code = vec![
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, // mov rax, 60
            0x48, 0x31, 0xFF, // xor rdi, rdi
            0x0F, 0x05, // syscall
        ];
        let count = validate_code(&code).unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn validates_push_pop() {
        let code = vec![0x50, 0x51, 0x59, 0x58]; // push rax, push rcx, pop rcx, pop rax
        assert_eq!(validate_code(&code).unwrap(), 4);
    }

    #[test]
    fn validates_conditional_jump() {
        // jz +5 (short)
        let code = vec![0x74, 0x05, 0x90, 0x90, 0x90, 0x90, 0x90];
        assert!(validate_code(&code).is_ok());
    }

    #[test]
    fn validates_simd_instructions() {
        let code = vec![
            0x66, 0x48, 0x0F, 0x6E, 0xC8, // movq xmm1, rax
            0x66, 0x0F, 0x6C, 0xC9,         // punpcklqdq xmm1, xmm1
            0x66, 0x0F, 0x50, 0xC0,         // movmskpd eax, xmm0
        ];
        assert!(validate_code(&code).is_ok());
    }

    #[test]
    fn backward_eb_is_a_real_jump_not_data_skip() {
        // Regression: the EB-over-data heuristic used to treat the
        // displacement byte as unsigned, so a backward jump like `EB E5`
        // (used inside emit_atoi_inline's digit-parse loop) was decoded as
        // "skip 229 bytes forward", landing mid-instruction much later and
        // producing a phantom "cannot decode" error. Backward EB must fall
        // through to the general decoder (2-byte short jump).
        //
        // Sequence: NOP (90) at offset 0, then `EB FE` at offset 1 — a tight
        // jmp-to-self (an infinite loop, but the validator must accept it
        // as valid code shape). Followed by another NOP so we have something
        // after the loop to ensure linear walk reaches the end.
        let code = vec![
            0x90,             // nop
            0xEB, 0xFE,       // jmp -2 (backward to self) — disp=-2 as i8
            0x90,             // nop (validator must reach this)
        ];
        let count = validate_code(&code).expect("backward EB must validate");
        assert_eq!(count, 3, "expected nop+jmp+nop, got {} instructions", count);
    }

    #[test]
    fn forward_eb_skips_inline_data() {
        // The classic jmp-over-data pattern: `EB 06`, then 6 bytes of
        // string data, then a real instruction. The validator must skip
        // the data (not try to decode "Hello!" as instructions).
        let code = vec![
            0xEB, 0x06,                                     // jmp +6
            b'H', b'e', b'l', b'l', b'o', b'!',             // 6 bytes of data
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,       // mov rax, 60
            0x0F, 0x05,                                      // syscall
        ];
        let count = validate_code(&code).expect("forward EB-over-data must validate");
        // jmp + mov + syscall = 3 instructions (data is not counted).
        assert_eq!(count, 3, "expected 3 instructions counted, got {}", count);
    }
}
