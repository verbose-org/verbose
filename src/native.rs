/// Native x86-64 code generation — produces ELF binaries directly.
///
/// General-purpose expression compiler: supports arithmetic (+, -, *, /),
/// comparisons (>, <, >=, <=), boolean logic (and, or), field access,
/// and rule calls (inlined). Multi-field concepts are supported.
///
/// The generated binary reads groups of N numbers from command-line arguments
/// (one group per record, N = number of fields) and prints the result.

use std::collections::HashMap;
use std::io::Write;

use crate::ast::*;

#[derive(Debug)]
pub struct NativeError {
    pub message: String,
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native codegen error: {}", self.message)
    }
}

pub fn compile_native(
    program: &Program,
    rule_name: &str,
    output_path: &str,
) -> Result<(), NativeError> {
    let concepts: Vec<&Concept> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Concept(c) => Some(c),
            _ => None,
        })
        .collect();
    let rules: HashMap<&str, &Rule> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();

    let rule = rules.get(rule_name).ok_or_else(|| NativeError {
        message: format!("no rule named '{}'", rule_name),
    })?;

    let concept = match &rule.input_ty {
        Type::Named(n) => concepts
            .iter()
            .find(|c| c.name == *n)
            .ok_or_else(|| NativeError {
                message: format!("unknown concept '{}'", n),
            })?,
        _ => {
            return Err(NativeError {
                message: "rule input must be a named concept".into(),
            })
        }
    };

    let code = emit_full_program(rule, concept, &rules)?;
    let elf = build_elf(&code);

    let mut file = std::fs::File::create(output_path).map_err(|e| NativeError {
        message: format!("cannot create '{}': {}", output_path, e),
    })?;
    file.write_all(&elf).map_err(|e| NativeError {
        message: format!("cannot write '{}': {}", output_path, e),
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(output_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| NativeError {
                message: format!("cannot set permissions: {}", e),
            })?;
    }

    Ok(())
}

/// Build field name → rbp offset mapping.
/// Fields are stored at [rbp-8], [rbp-16], etc.
fn field_offsets(concept: &Concept) -> HashMap<&str, i32> {
    concept
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), -((i as i32 + 1) * 8)))
        .collect()
}

fn emit_full_program(
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let nfields = concept.fields.len();
    let offsets = field_offsets(concept);
    let is_bool = rule.output_ty == Type::Bool;
    let mut code = Vec::new();

    // === _start ===
    // Stack at entry: [rsp]=argc, [rsp+8]=argv[0], [rsp+16]=argv[1], ...
    // mov r12, [rsp]           — argc
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);
    // lea r13, [rsp+8]         — argv base
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]);

    // Setup rbp frame for field storage
    // push rbp
    code.push(0x55);
    // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);
    // sub rsp, nfields*8 (reserve field slots)
    let frame_size = (nfields * 8) as i32;
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());

    // r14 = arg index (starts at 1, skip argv[0])
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]);

    let loop_top = code.len();

    // cmp r14, r12 — if index >= argc, done
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]);
    // jge exit (placeholder)
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // Parse N fields from argv into rbp-relative slots
    for (i, field) in concept.fields.iter().enumerate() {
        let offset = offsets[field.name.as_str()];

        // mov rdi, [r13 + r14*8] — argv[index]
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]);

        // inline atoi: rdi string → rax number
        emit_atoi_inline(&mut code);

        // mov [rbp + offset], rax — store parsed field
        if offset >= -128 {
            code.extend_from_slice(&[0x48, 0x89, 0x45]);
            code.push(offset as u8);
        } else {
            code.extend_from_slice(&[0x48, 0x89, 0x85]);
            code.extend_from_slice(&offset.to_le_bytes());
        }

        // inc r14
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]);
    }

    // Evaluate expression — result in rax
    emit_eval_expr(&mut code, &rule.logic.value, &rule.input_name, &offsets, all_rules)?;

    // Print result
    if is_bool {
        // rax = 0 or 1
        // test al, al
        code.extend_from_slice(&[0x84, 0xC0]);
        // jz .print_false
        code.push(0x74);
        let pf_patch = code.len();
        code.push(0x00);
        emit_write_string(&mut code, b"true\n");
        code.push(0xEB);
        let ap_patch = code.len();
        code.push(0x00);
        let pf_pos = code.len();
        code[pf_patch] = (pf_pos - pf_patch - 1) as u8;
        emit_write_string(&mut code, b"false\n");
        let ap_pos = code.len();
        code[ap_patch] = (ap_pos - ap_patch - 1) as u8;
    } else {
        // rax = number, print it
        emit_itoa_inline(&mut code);
    }

    // jmp loop_top
    code.push(0xE9);
    let loop_offset = loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&loop_offset.to_le_bytes());

    // exit:
    let exit_pos = code.len();
    let exit_offset = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_offset.to_le_bytes());

    // mov rax, 60 (sys_exit)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    // xor rdi, rdi (exit code 0)
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(code)
}

/// Compile an expression to machine code. Result left in rax.
fn emit_eval_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Number(n) => {
            emit_mov_rax_imm(code, *n);
            Ok(())
        }
        Expr::Field(base, field_name) => {
            if !matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                return Err(NativeError {
                    message: "nested field access not supported in native backend".into(),
                });
            }
            let offset = offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                message: format!("unknown field '{}' in native codegen", field_name),
            })?;
            // mov rax, [rbp + offset]
            if *offset >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                code.push(*offset as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                code.extend_from_slice(&offset.to_le_bytes());
            }
            Ok(())
        }
        Expr::Binary(op, left, right) => {
            // Evaluate left → rax, push, evaluate right → rax, pop left → rcx
            emit_eval_expr(code, left, input_name, offsets, all_rules)?;
            code.push(0x50); // push rax
            emit_eval_expr(code, right, input_name, offsets, all_rules)?;
            code.push(0x59); // pop rcx — now rcx=left, rax=right

            match op {
                BinOp::Add => {
                    // rax = left + right = rcx + rax
                    code.extend_from_slice(&[0x48, 0x01, 0xC8]); // add rax, rcx
                }
                BinOp::Sub => {
                    // result = left - right = rcx - rax
                    code.extend_from_slice(&[0x48, 0x29, 0xC1]); // sub rcx, rax
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
                }
                BinOp::Mul => {
                    // rax = left * right = rcx * rax
                    code.extend_from_slice(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
                }
                BinOp::Div => {
                    // result = left / right = rcx / rax
                    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (save right)
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (left → rax)
                    code.extend_from_slice(&[0x48, 0x99]); // cqo (sign-extend rax → rdx:rax)
                    code.extend_from_slice(&[0x49, 0xF7, 0xF8]); // idiv r8
                }
                BinOp::Gt => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x9F, 0xC0]); // setg al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                BinOp::Lt => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x9C, 0xC0]); // setl al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                BinOp::GtEq => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x9D, 0xC0]); // setge al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                BinOp::LtEq => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x9E, 0xC0]); // setle al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                BinOp::And => {
                    code.extend_from_slice(&[0x48, 0x21, 0xC8]); // and rax, rcx
                }
                BinOp::Or => {
                    code.extend_from_slice(&[0x48, 0x09, 0xC8]); // or rax, rcx
                }
            }
            Ok(())
        }
        Expr::Call(name, args) => {
            if args.len() != 1 {
                return Err(NativeError {
                    message: "native call requires exactly 1 argument".into(),
                });
            }
            let called = all_rules.get(name.as_str()).ok_or_else(|| NativeError {
                message: format!("unknown rule '{}' for native inlining", name),
            })?;
            // Inline: emit the called rule's logic with the same field layout
            emit_eval_expr(
                code,
                &called.logic.value,
                &called.input_name,
                offsets,
                all_rules,
            )
        }
        Expr::Ident(name) if name == input_name => Err(NativeError {
            message: "bare input binding not supported in expressions".into(),
        }),
        Expr::Ident(_) => Err(NativeError {
            message: "unresolved identifier in native codegen".into(),
        }),
    }
}

/// Inline atoi: parse null-terminated decimal string at rdi into rax.
fn emit_atoi_inline(code: &mut Vec<u8>) {
    // xor rax, rax
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);
    // xor rcx, rcx (negative flag)
    code.extend_from_slice(&[0x48, 0x31, 0xC9]);

    // Check for '-'
    // movzx rdx, byte [rdi]
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x17]);
    // cmp dl, '-'
    code.extend_from_slice(&[0x80, 0xFA, 0x2D]);
    // jne +5
    code.extend_from_slice(&[0x75, 0x05]);
    // mov cl, 1
    code.extend_from_slice(&[0xB1, 0x01]);
    // inc rdi
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]);

    let parse_top = code.len();
    // movzx rdx, byte [rdi]
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x17]);
    // test dl, dl
    code.extend_from_slice(&[0x84, 0xD2]);
    // jz done
    code.push(0x74);
    let done_patch = code.len();
    code.push(0x00);

    // sub dl, '0'
    code.extend_from_slice(&[0x80, 0xEA, 0x30]);
    // imul rax, 10
    code.extend_from_slice(&[0x48, 0x6B, 0xC0, 0x0A]);
    // movzx rdx, dl
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xD2]);
    // add rax, rdx
    code.extend_from_slice(&[0x48, 0x01, 0xD0]);
    // inc rdi
    code.extend_from_slice(&[0x48, 0xFF, 0xC7]);
    // jmp parse_top
    code.push(0xEB);
    let jmp_offset = (parse_top as isize).wrapping_sub(code.len() as isize + 1) as i8;
    code.push(jmp_offset as u8);

    // done:
    let done_pos = code.len();
    code[done_patch] = (done_pos - done_patch - 1) as u8;

    // if negative, negate
    // test cl, cl
    code.extend_from_slice(&[0x84, 0xC9]);
    // jz +3
    code.extend_from_slice(&[0x74, 0x03]);
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
}

/// Inline itoa: print rax as decimal string + newline to stdout.
fn emit_itoa_inline(code: &mut Vec<u8>) {
    // sub rsp, 24 — buffer on stack
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x18]);

    // lea rsi, [rsp + 22] — point to end of buffer
    code.extend_from_slice(&[0x48, 0x8D, 0x74, 0x24, 0x16]);
    // mov byte [rsi], 10 — newline
    code.extend_from_slice(&[0xC6, 0x06, 0x0A]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);

    // Handle negative
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jns .not_neg
    code.push(0x79);
    let not_neg_patch = code.len();
    code.push(0x00);
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    // Store '-' flag: push 1
    // mov byte [rsp+23], 1 — flag byte (we have space)
    code.extend_from_slice(&[0xC6, 0x44, 0x24, 0x17, 0x01]);
    code.push(0xEB); // jmp .after_neg
    let after_neg_patch = code.len();
    code.push(0x00);

    let not_neg_pos = code.len();
    code[not_neg_patch] = (not_neg_pos - not_neg_patch - 1) as u8;
    // mov byte [rsp+23], 0 — no negative flag
    code.extend_from_slice(&[0xC6, 0x44, 0x24, 0x17, 0x00]);

    let after_neg_pos = code.len();
    code[after_neg_patch] = (after_neg_pos - after_neg_patch - 1) as u8;

    // mov r8, 10
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x0A, 0x00, 0x00, 0x00]);

    // Handle zero
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz .div_loop
    code.push(0x75);
    let div_loop_patch = code.len();
    code.push(0x00);
    // mov byte [rsi], '0'
    code.extend_from_slice(&[0xC6, 0x06, 0x30]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    // jmp .write
    code.push(0xEB);
    let write_patch = code.len();
    code.push(0x00);

    // .div_loop:
    let div_loop_pos = code.len();
    code[div_loop_patch] = (div_loop_pos - div_loop_patch - 1) as u8;

    // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    // div r8 — rax=quotient, rdx=remainder
    code.extend_from_slice(&[0x49, 0xF7, 0xF0]);
    // add dl, '0'
    code.extend_from_slice(&[0x80, 0xC2, 0x30]);
    // mov [rsi], dl
    code.extend_from_slice(&[0x88, 0x16]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz .div_loop
    let jmp_back = div_loop_pos as i8 - (code.len() + 2) as i8;
    code.extend_from_slice(&[0x75, jmp_back as u8]);

    // .write:
    let write_pos = code.len();
    code[write_patch] = (write_pos - write_patch - 1) as u8;

    // Check negative flag
    // cmp byte [rsp+23], 0
    code.extend_from_slice(&[0x80, 0x7C, 0x24, 0x17, 0x00]);
    // je .no_minus
    code.push(0x74);
    let no_minus_patch = code.len();
    code.push(0x00);
    // mov byte [rsi], '-'
    code.extend_from_slice(&[0xC6, 0x06, 0x2D]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    let no_minus_pos = code.len();
    code[no_minus_patch] = (no_minus_pos - no_minus_patch - 1) as u8;

    // inc rsi — points to first char
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]);

    // rdx = length = (rsp + 23) - rsi
    code.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x17]); // lea rdx, [rsp+23]
    code.extend_from_slice(&[0x48, 0x29, 0xF2]); // sub rdx, rsi

    // mov rdi, 1 (stdout)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
    // mov rax, 1 (sys_write)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    // add rsp, 24
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x18]);
}

fn emit_mov_rax_imm(code: &mut Vec<u8>, value: i64) {
    if value >= i32::MIN as i64 && value <= i32::MAX as i64 {
        // mov rax, imm32 (sign-extended)
        code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
        code.extend_from_slice(&(value as i32).to_le_bytes());
    } else {
        // movabs rax, imm64
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&value.to_le_bytes());
    }
}

fn emit_write_string(code: &mut Vec<u8>, s: &[u8]) {
    let len = s.len();
    code.push(0xEB);
    code.push(len as u8);
    let data_offset = code.len();
    code.extend_from_slice(s);
    let after_lea = code.len() + 7;
    let rip_offset = data_offset as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rip_offset.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(len as i32).to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn build_elf(code: &[u8]) -> Vec<u8> {
    let entry_addr: u64 = 0x400000 + 120;
    let file_size = 120 + code.len();
    let mut elf = Vec::with_capacity(file_size);

    elf.extend_from_slice(&[
        0x7F, b'E', b'L', b'F', 0x02, 0x01, 0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ]);
    elf.extend_from_slice(&2u16.to_le_bytes());
    elf.extend_from_slice(&0x3Eu16.to_le_bytes());
    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&entry_addr.to_le_bytes());
    elf.extend_from_slice(&64u64.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0u32.to_le_bytes());
    elf.extend_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&56u16.to_le_bytes());
    elf.extend_from_slice(&1u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());
    elf.extend_from_slice(&0u16.to_le_bytes());

    elf.extend_from_slice(&1u32.to_le_bytes());
    elf.extend_from_slice(&5u32.to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf.extend_from_slice(&0x400000u64.to_le_bytes());
    elf.extend_from_slice(&0x400000u64.to_le_bytes());
    elf.extend_from_slice(&(file_size as u64).to_le_bytes());
    elf.extend_from_slice(&(file_size as u64).to_le_bytes());
    elf.extend_from_slice(&0x1000u64.to_le_bytes());

    elf.extend_from_slice(code);
    elf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_header_valid() {
        let code = vec![0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, 0x48, 0x31, 0xFF, 0x0F, 0x05];
        let elf = build_elf(&code);
        assert_eq!(&elf[0..4], &[0x7F, b'E', b'L', b'F']);
        assert_eq!(elf.len(), 120 + code.len());
    }

    #[test]
    fn mov_rax_small() {
        let mut code = Vec::new();
        emit_mov_rax_imm(&mut code, 42);
        assert_eq!(&code[0..3], &[0x48, 0xC7, 0xC0]);
        assert_eq!(i32::from_le_bytes([code[3], code[4], code[5], code[6]]), 42);
    }

    #[test]
    fn mov_rax_large() {
        let mut code = Vec::new();
        emit_mov_rax_imm(&mut code, 0x1_0000_0000);
        assert_eq!(&code[0..2], &[0x48, 0xB8]);
        assert_eq!(code.len(), 10);
    }

    #[test]
    fn field_offset_mapping() {
        let concept = Concept {
            name: "Test".into(),
            intention: "t".into(),
            source: SourceRef {
                file: "t.intent".into(),
                line: 1,
            },
            fields: vec![
                Field { name: "a".into(), ty: Type::Number },
                Field { name: "b".into(), ty: Type::Number },
                Field { name: "c".into(), ty: Type::Number },
            ],
        };
        let offsets = field_offsets(&concept);
        assert_eq!(offsets["a"], -8);
        assert_eq!(offsets["b"], -16);
        assert_eq!(offsets["c"], -24);
    }
}
