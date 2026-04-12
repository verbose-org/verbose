/// Native x86-64 code generation — produces ELF binaries directly, no intermediate compiler.
///
/// How this works (for someone who hasn't done this before):
///
/// A compiled program is just a file full of bytes that the operating system knows how to run.
/// On Linux, that format is called ELF (Executable and Linkable Format). An ELF file has:
///   1. A header that says "I'm an ELF file, here's where the code starts"
///   2. The actual machine instructions (bytes that the CPU executes)
///   3. Some metadata about memory layout
///
/// Machine instructions are just numbers. For example, on x86-64:
///   - `cmp rdi, 10000` (compare a register to 10000) is the bytes: 48 81 FF 10 27 00 00
///   - `setg al` (set al=1 if greater) is the bytes: 0F 9F C0
///   - `ret` (return to caller) is the byte: C3
///
/// We emit these bytes directly. No assembler, no linker, no external tool.
/// The result is a tiny binary that does exactly what the Verbose rule says.

use crate::ast::*;
use std::io::Write;

#[derive(Debug)]
pub struct NativeError {
    pub message: String,
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native codegen error: {}", self.message)
    }
}

/// Compile a rule to a standalone x86-64 Linux ELF binary.
///
/// For the POC, this produces a minimal program that:
///   1. Reads numbers from command-line arguments
///   2. Evaluates the rule on each number
///   3. Prints "true" or "false" for each
///
/// The binary has ZERO runtime dependencies (no libc) — it uses Linux syscalls directly.
pub fn compile_native(
    rule: &Rule,
    concept: &Concept,
    output_path: &str,
) -> Result<(), NativeError> {
    let code = emit_program(rule, concept)?;
    let elf = build_elf(&code);

    let mut file = std::fs::File::create(output_path).map_err(|e| NativeError {
        message: format!("cannot create '{}': {}", output_path, e),
    })?;
    file.write_all(&elf).map_err(|e| NativeError {
        message: format!("cannot write '{}': {}", output_path, e),
    })?;

    // Make executable (chmod +x)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(output_path, perms).map_err(|e| NativeError {
            message: format!("cannot set permissions: {}", e),
        })?;
    }

    Ok(())
}

/// The full program as machine code.
///
/// Structure:
///   _start:
///     - for each argv[1..], parse number, evaluate rule, print result
///     - exit
///
/// We keep things simple: one field (number), one comparison, print "true\n" or "false\n".
fn emit_program(rule: &Rule, concept: &Concept) -> Result<Vec<u8>, NativeError> {
    if concept.fields.len() != 1 {
        return Err(NativeError {
            message: "native backend only supports single-field concepts for now".into(),
        });
    }
    let field = &concept.fields[0];
    if field.ty != Type::Number {
        return Err(NativeError {
            message: "native backend only supports 'number' fields for now".into(),
        });
    }

    let (op, threshold) = extract_comparison(&rule.logic.value, &rule.input_name)?;

    let mut code = Vec::new();

    // === _start ===
    // On Linux, when a static ELF starts:
    //   [rsp]     = argc
    //   [rsp+8]   = argv[0] (program name)
    //   [rsp+16]  = argv[1] (first arg)
    //   [rsp+24]  = argv[2] (second arg)
    //   ...

    // mov r12, [rsp]       — argc
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);
    // lea r13, [rsp+8]     — argv pointer
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]);
    // mov r14, 1           — index (start at 1 to skip argv[0])
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]);
    // mov r15, 0           — record counter for display
    code.extend_from_slice(&[0x49, 0xC7, 0xC7, 0x00, 0x00, 0x00, 0x00]);

    let loop_top = code.len();

    // cmp r14, r12         — if index >= argc, done
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]);
    // jge exit (placeholder, patch later)
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // 32-bit relative offset

    // mov rdi, [r13 + r14*8]  — argv[index]
    // REX = 0x4B: W=1 (64-bit), R=0 (rdi fits in 3 bits), X=1 (r14 index), B=1 (r13 base)
    code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]);

    // --- inline atoi: parse decimal string at rdi into rax ---
    // xor rax, rax          — result = 0
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);
    // xor rcx, rcx          — negative flag
    code.extend_from_slice(&[0x48, 0x31, 0xC9]);

    // Check for '-' sign
    // movzx rdx, byte [rdi]
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x17]);
    // cmp dl, '-'
    code.extend_from_slice(&[0x80, 0xFA, 0x2D]);
    // jne .parse_digits
    code.extend_from_slice(&[0x75, 0x05]);
    // mov rcx, 1
    code.extend_from_slice(&[0xB1, 0x01]);
    // inc rdi
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]);

    let parse_digits = code.len();
    // movzx rdx, byte [rdi]
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x17]);
    // test dl, dl           — null terminator?
    code.extend_from_slice(&[0x84, 0xD2]);
    // jz .done_parsing
    code.push(0x74);
    let done_parsing_patch = code.len();
    code.push(0x00); // 8-bit relative offset

    // sub dl, '0'
    code.extend_from_slice(&[0x80, 0xEA, 0x30]);
    // imul rax, 10
    code.extend_from_slice(&[0x48, 0x6B, 0xC0, 0x0A]);
    // add rax, rdx (zero-extended from dl)
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xD2]); // movzx rdx, dl
    code.extend_from_slice(&[0x48, 0x01, 0xD0]); // add rax, rdx
    // inc rdi
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]);
    // jmp .parse_digits
    let jmp_back = code.len();
    code.push(0xEB);
    let offset = parse_digits as i8 - (code.len() + 1) as i8;
    code.push(offset as u8);

    // .done_parsing:
    let done_parsing = code.len();
    code[done_parsing_patch] = (done_parsing - done_parsing_patch - 1) as u8;

    // if negative, negate
    // test cl, cl
    code.extend_from_slice(&[0x84, 0xC9]);
    // jz .not_neg
    code.extend_from_slice(&[0x74, 0x03]);
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    // .not_neg:

    // --- evaluate rule: cmp rax, threshold ---
    // rax = parsed number (the field value)
    emit_cmp_imm(&mut code, threshold);
    // set result based on comparison
    emit_setcc(&mut code, &op);
    // al = 0 or 1 now

    // --- print "[N] output_name = true/false\n" ---
    // We use a simplified approach: just print "true\n" or "false\n"
    // Save result in rbx
    // movzx rbx, al
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xD8]);

    // Print "true\n" or "false\n" using write syscall
    // test bl, bl
    code.extend_from_slice(&[0x84, 0xDB]);
    // jz .print_false
    code.push(0x74);
    let print_false_patch = code.len();
    code.push(0x00);

    // .print_true: write(1, "true\n", 5)
    emit_write_string(&mut code, b"true\n");
    // jmp .after_print
    code.push(0xEB);
    let after_print_patch = code.len();
    code.push(0x00);

    // .print_false:
    let print_false = code.len();
    code[print_false_patch] = (print_false - print_false_patch - 1) as u8;
    emit_write_string(&mut code, b"false\n");

    // .after_print:
    let after_print = code.len();
    code[after_print_patch] = (after_print - after_print_patch - 1) as u8;

    // inc r14 (index++)
    code.extend_from_slice(&[0x49, 0xFF, 0xC6]);
    // inc r15 (counter++)
    code.extend_from_slice(&[0x49, 0xFF, 0xC7]);

    // jmp loop_top
    code.push(0xE9);
    let loop_offset = loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&loop_offset.to_le_bytes());

    // exit:
    let exit_pos = code.len();
    let exit_offset = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_offset.to_le_bytes());

    // exit(0)
    // mov rax, 60 (sys_exit)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    // xor rdi, rdi (exit code 0)
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(code)
}

/// Extract the comparison operator and threshold from the rule's logic.
/// For now, only supports `field > N`, `field < N`, `field >= N`, `field <= N`.
fn extract_comparison(
    expr: &Expr,
    input_name: &str,
) -> Result<(BinOp, i64), NativeError> {
    match expr {
        Expr::Binary(op, left, right) => {
            // left should be a field access, right should be a number
            match (left.as_ref(), right.as_ref()) {
                (Expr::Field(base, _field), Expr::Number(n)) => {
                    if !matches!(base.as_ref(), Expr::Ident(name) if name == input_name) {
                        return Err(NativeError {
                            message: "expected field access on input binding".into(),
                        });
                    }
                    Ok((*op, *n))
                }
                _ => Err(NativeError {
                    message: "native backend requires pattern: field <op> number".into(),
                }),
            }
        }
        _ => Err(NativeError {
            message: "native backend requires a comparison expression".into(),
        }),
    }
}

/// Emit `cmp rax, imm32` or `cmp rax, imm8` depending on value size.
fn emit_cmp_imm(code: &mut Vec<u8>, value: i64) {
    if value >= -128 && value <= 127 {
        // cmp rax, imm8
        code.extend_from_slice(&[0x48, 0x83, 0xF8]);
        code.push(value as u8);
    } else {
        // cmp rax, imm32
        code.extend_from_slice(&[0x48, 0x3D]);
        code.extend_from_slice(&(value as i32).to_le_bytes());
    }
}

/// Emit the appropriate SETcc instruction into al.
fn emit_setcc(code: &mut Vec<u8>, op: &BinOp) {
    match op {
        BinOp::Gt => code.extend_from_slice(&[0x0F, 0x9F, 0xC0]),   // setg al
        BinOp::Lt => code.extend_from_slice(&[0x0F, 0x9C, 0xC0]),   // setl al
        BinOp::GtEq => code.extend_from_slice(&[0x0F, 0x9D, 0xC0]), // setge al
        BinOp::LtEq => code.extend_from_slice(&[0x0F, 0x9E, 0xC0]), // setle al
    }
}

/// Emit write(1, string, len) using Linux syscall.
/// The string is embedded inline in the code using a rip-relative lea.
fn emit_write_string(code: &mut Vec<u8>, s: &[u8]) {
    // We use a trick: jmp over the string data, then reference it with rip-relative addressing.
    //
    // Layout:
    //   jmp .after_data       (2 bytes: EB xx)
    //   .data: "true\n"       (N bytes)
    //   .after_data:
    //   lea rsi, [rip - N - 7]  (7 bytes, points back to .data)
    //   mov rdx, N            (7 bytes)
    //   mov rdi, 1            (7 bytes)
    //   mov rax, 1            (7 bytes, sys_write)
    //   syscall               (2 bytes)

    let len = s.len();

    // jmp over data
    code.push(0xEB);
    code.push(len as u8);

    // inline string data
    let data_offset = code.len();
    code.extend_from_slice(s);

    // lea rsi, [rip - offset_to_data]
    let after_lea = code.len() + 7; // lea is 7 bytes
    let rip_offset = data_offset as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rip_offset.to_le_bytes());

    // mov rdx, len
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(len as i32).to_le_bytes());

    // mov rdi, 1 (stdout)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);

    // mov rax, 1 (sys_write)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);

    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// Build a minimal ELF64 executable from raw machine code.
///
/// This creates the smallest valid ELF that Linux will run:
///   - ELF header (64 bytes)
///   - One program header (56 bytes)
///   - The code itself
///
/// Total overhead: 120 bytes. The rest is pure machine instructions.
fn build_elf(code: &[u8]) -> Vec<u8> {
    let entry_addr: u64 = 0x400000 + 120; // code starts right after headers
    let file_size = 120 + code.len();

    let mut elf = Vec::with_capacity(file_size);

    // --- ELF header (64 bytes) ---
    // e_ident: magic, class=64, data=LE, version=1, OS=Linux
    elf.extend_from_slice(&[
        0x7F, b'E', b'L', b'F', // magic
        0x02, // 64-bit
        0x01, // little-endian
        0x01, // ELF version 1
        0x03, // OS/ABI = Linux
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
    ]);
    elf.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    elf.extend_from_slice(&0x3Eu16.to_le_bytes()); // e_machine = x86-64
    elf.extend_from_slice(&1u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&entry_addr.to_le_bytes()); // e_entry
    elf.extend_from_slice(&64u64.to_le_bytes()); // e_phoff (program header offset)
    elf.extend_from_slice(&0u64.to_le_bytes()); // e_shoff (no section headers)
    elf.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&64u16.to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&56u16.to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    elf.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx

    // --- Program header (56 bytes) ---
    elf.extend_from_slice(&1u32.to_le_bytes()); // p_type = PT_LOAD
    elf.extend_from_slice(&5u32.to_le_bytes()); // p_flags = PF_R | PF_X
    elf.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    elf.extend_from_slice(&0x400000u64.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&0x400000u64.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&(file_size as u64).to_le_bytes()); // p_filesz
    elf.extend_from_slice(&(file_size as u64).to_le_bytes()); // p_memsz
    elf.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align

    // --- Code ---
    elf.extend_from_slice(code);

    elf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_gt_comparison() {
        let expr = Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::Field(
                Box::new(Expr::Ident("i".into())),
                "amount".into(),
            )),
            Box::new(Expr::Number(10000)),
        );
        let (op, val) = extract_comparison(&expr, "i").unwrap();
        assert_eq!(op, BinOp::Gt);
        assert_eq!(val, 10000);
    }

    #[test]
    fn elf_header_valid() {
        let code = vec![
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, // mov rax, 60
            0x48, 0x31, 0xFF, // xor rdi, rdi
            0x0F, 0x05, // syscall
        ];
        let elf = build_elf(&code);
        assert_eq!(&elf[0..4], &[0x7F, b'E', b'L', b'F']);
        assert_eq!(elf.len(), 120 + code.len());
    }

    #[test]
    fn cmp_small_value() {
        let mut code = Vec::new();
        emit_cmp_imm(&mut code, 42);
        // cmp rax, 42 → 48 83 F8 2A
        assert_eq!(code, vec![0x48, 0x83, 0xF8, 0x2A]);
    }

    #[test]
    fn cmp_large_value() {
        let mut code = Vec::new();
        emit_cmp_imm(&mut code, 10000);
        // cmp rax, 10000 → 48 3D 10 27 00 00
        assert_eq!(code, vec![0x48, 0x3D, 0x10, 0x27, 0x00, 0x00]);
    }
}
