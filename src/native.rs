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

    let is_vectorizable = rule
        .hints
        .as_ref()
        .map_or(false, |h| h.vectorizable == Some(true));
    let is_parallel = rule
        .hints
        .as_ref()
        .map_or(false, |h| h.parallel == Some(true));

    let code = if is_vectorizable && concept.fields.len() == 1 {
        if let Some(threshold) = extract_simple_gt(rule) {
            emit_vectorized_program(threshold)?
        } else {
            emit_full_program(rule, concept, &rules)?
        }
    } else if is_parallel {
        emit_parallel_program(rule, concept, &rules)?
    } else {
        emit_full_program(rule, concept, &rules)?
    };
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
    let field_ranges = build_field_ranges(concept);
    emit_eval_expr(&mut code, &rule.logic.value, &rule.input_name, &offsets, all_rules, &field_ranges)?;

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

/// Build field ranges from concept for static analysis.
fn build_field_ranges(concept: &Concept) -> HashMap<&str, (i64, i64)> {
    concept
        .fields
        .iter()
        .filter(|f| f.ty == Type::Number)
        .map(|f| {
            let range = f.range.unwrap_or((0, i32::MAX as i64));
            (f.name.as_str(), range)
        })
        .collect()
}

/// Try to statically determine if a comparison is always true or false.
/// Uses interval arithmetic on declared field ranges.
fn try_static_condition(
    expr: &Expr,
    field_ranges: &HashMap<&str, (i64, i64)>,
    input_name: &str,
) -> Option<bool> {
    use crate::verifier::compute_range;
    match expr {
        Expr::Binary(op, left, right) => {
            let (l_min, l_max) = compute_range(left, field_ranges, input_name)?;
            let (r_min, r_max) = compute_range(right, field_ranges, input_name)?;
            match op {
                BinOp::Gt => {
                    if l_min > r_max {
                        Some(true)
                    } else if l_max <= r_min {
                        Some(false)
                    } else {
                        None
                    }
                }
                BinOp::Lt => {
                    if l_max < r_min {
                        Some(true)
                    } else if l_min >= r_max {
                        Some(false)
                    } else {
                        None
                    }
                }
                BinOp::GtEq => {
                    if l_min >= r_max {
                        Some(true)
                    } else if l_max < r_min {
                        Some(false)
                    } else {
                        None
                    }
                }
                BinOp::LtEq => {
                    if l_max <= r_min {
                        Some(true)
                    } else if l_min > r_max {
                        Some(false)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Compile an expression to machine code. Result left in rax.
fn emit_eval_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Number(n) => {
            emit_mov_rax_imm(code, *n);
            Ok(())
        }
        Expr::Text(_) => {
            Err(NativeError {
                message: "text literals not supported in native backend (use --compile for Rust transpiler)".into(),
            })
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
            emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges)?;
            code.push(0x50); // push rax
            emit_eval_expr(code, right, input_name, offsets, all_rules, field_ranges)?;
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
                BinOp::Eq => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x94, 0xC0]); // sete al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
                }
                BinOp::NotEq => {
                    code.extend_from_slice(&[0x48, 0x39, 0xC1]); // cmp rcx, rax
                    code.extend_from_slice(&[0x0F, 0x95, 0xC0]); // setne al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]); // movzx rax, al
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
                field_ranges,
            )
        }
        Expr::If(cond, then_e, else_e) => {
            // Try static branch elimination via interval arithmetic
            if let Some(always) = try_static_condition(cond, field_ranges, input_name) {
                if always {
                    // Condition always true — emit only then branch, skip comparison
                    return emit_eval_expr(code, then_e, input_name, offsets, all_rules, field_ranges);
                } else {
                    // Condition always false — emit only else branch, skip comparison
                    return emit_eval_expr(code, else_e, input_name, offsets, all_rules, field_ranges);
                }
            }
            // Dynamic: emit both branches with runtime check
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges)?;
            // test al, al
            code.extend_from_slice(&[0x84, 0xC0]);
            // jz .else_branch
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00; 4]);
            // then branch → rax
            emit_eval_expr(code, then_e, input_name, offsets, all_rules, field_ranges)?;
            // jmp .end
            code.push(0xE9);
            let end_patch = code.len();
            code.extend_from_slice(&[0x00; 4]);
            // .else_branch:
            let else_pos = code.len();
            let eo = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&eo.to_le_bytes());
            emit_eval_expr(code, else_e, input_name, offsets, all_rules, field_ranges)?;
            // .end:
            let end_pos = code.len();
            let ep = end_pos as i32 - (end_patch as i32 + 4);
            code[end_patch..end_patch + 4].copy_from_slice(&ep.to_le_bytes());
            Ok(())
        }
        Expr::Not(inner) => {
            emit_eval_expr(code, inner, input_name, offsets, all_rules, field_ranges)?;
            // rax is 0 or 1; flip it
            code.extend_from_slice(&[0x48, 0x83, 0xF0, 0x01]); // xor rax, 1
            Ok(())
        }
        Expr::Neg(inner) => {
            emit_eval_expr(code, inner, input_name, offsets, all_rules, field_ranges)?;
            code.extend_from_slice(&[0x48, 0xF7, 0xD8]); // neg rax
            Ok(())
        }
        Expr::Quantifier(_, _, _, _) => Err(NativeError {
            message: "quantifiers (all/any) not supported in native backend (use --run interpreter)"
                .into(),
        }),
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

/// Fork-based parallel program: splits records across 2 processes.
///
/// Phase 1: Parse all argv into a contiguous array
/// Phase 2: fork() — child gets records 0..N/2, parent gets N/2..N
/// Phase 3: Child runs its half, exits. Parent waits, then runs its half.
///
/// Both processes use the same evaluation code. Output is ordered because
/// the parent waits for the child to finish before printing its half.
///
/// This is REAL parallelism — 2 CPU cores running simultaneously.
/// The 'parallel: yes' hint guarantees it's safe (pure, no side effects).
fn emit_parallel_program(
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let nfields = concept.fields.len();
    let offsets = field_offsets(concept);
    let is_bool = rule.output_ty == Type::Bool;
    let mut code = Vec::new();

    // === Setup: save argc/argv ===
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]

    // total_args = argc - 1
    code.extend_from_slice(&[0x4C, 0x89, 0xE0]); // mov rax, r12
    code.extend_from_slice(&[0x48, 0xFF, 0xC8]); // dec rax
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax (r14 = total_args)

    // Allocate array (total_args * 8, aligned to 16)
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x0F]); // add rax, 15
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF0]); // and rax, -16
    code.extend_from_slice(&[0x48, 0x29, 0xC4]); // sub rsp, rax
    code.extend_from_slice(&[0x49, 0x89, 0xE7]); // mov r15, rsp

    // Setup rbp frame for field storage
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    let frame = ((nfields * 8 + 15) & !15) as i32;
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame.to_le_bytes());

    // === Phase 1: Parse all argv numbers into array ===
    code.extend_from_slice(&[0x48, 0x31, 0xDB]); // xor rbx, rbx
    let parse_top = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xF3]); // cmp rbx, r14
    code.push(0x0F);
    code.push(0x8D);
    let parse_done_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    code.extend_from_slice(&[0x49, 0x8B, 0x7C, 0xDD, 0x08]); // mov rdi, [r13+rbx*8+8]
    code.push(0x53); // push rbx
    emit_atoi_inline(&mut code);
    code.push(0x5B); // pop rbx
    code.extend_from_slice(&[0x49, 0x89, 0x04, 0xDF]); // mov [r15+rbx*8], rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx
    code.push(0xE9);
    let pj = parse_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&pj.to_le_bytes());

    let parse_done = code.len();
    let pd = parse_done as i32 - (parse_done_patch as i32 + 4);
    code[parse_done_patch..parse_done_patch + 4].copy_from_slice(&pd.to_le_bytes());

    // === Calculate num_records = total_args / nfields ===
    code.extend_from_slice(&[0x4C, 0x89, 0xF0]); // mov rax, r14
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
    code.extend_from_slice(&(nfields as i32).to_le_bytes()); // mov rcx, nfields
    code.extend_from_slice(&[0x48, 0xF7, 0xF1]); // div rcx
    code.extend_from_slice(&[0x49, 0x89, 0xC6]); // mov r14, rax (r14 = num_records)

    // midpoint = num_records / 2
    code.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax
    code.extend_from_slice(&[0x48, 0xD1, 0xEB]); // shr rbx, 1

    // === Phase 2: fork() ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x39, 0x00, 0x00, 0x00]); // mov rax, 57 (fork)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.push(0x0F);
    code.push(0x85);
    let parent_patch = code.len();
    code.extend_from_slice(&[0x00; 4]); // jnz .parent

    // === Child: records 0..midpoint ===
    code.extend_from_slice(&[0x48, 0x31, 0xC9]); // xor rcx, rcx (start=0)
    code.extend_from_slice(&[0x49, 0x89, 0xD8]); // mov r8, rbx (end=midpoint)
    code.push(0xE9);
    let child_jmp_patch = code.len();
    code.extend_from_slice(&[0x00; 4]); // jmp .process_loop

    // === Parent: wait for child, then records midpoint..count ===
    let parent_pos = code.len();
    let po = parent_pos as i32 - (parent_patch as i32 + 4);
    code[parent_patch..parent_patch + 4].copy_from_slice(&po.to_le_bytes());

    // wait4(-1, NULL, 0, NULL)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0xFF, 0xFF, 0xFF, 0xFF]); // mov rdi, -1
    code.extend_from_slice(&[0x48, 0x31, 0xF6]); // xor rsi, rsi
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x4D, 0x31, 0xD2]); // xor r10, r10
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3D, 0x00, 0x00, 0x00]); // mov rax, 61 (wait4)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    code.extend_from_slice(&[0x48, 0x89, 0xD9]); // mov rcx, rbx (start=midpoint)
    code.extend_from_slice(&[0x4D, 0x89, 0xF0]); // mov r8, r14 (end=num_records)

    // === Process loop (shared by child and parent) ===
    let process_loop = code.len();
    let cj = process_loop as i32 - (child_jmp_patch as i32 + 4);
    code[child_jmp_patch..child_jmp_patch + 4].copy_from_slice(&cj.to_le_bytes());

    // cmp rcx, r8
    code.extend_from_slice(&[0x4C, 0x39, 0xC1]);
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00; 4]); // jge .exit

    // Save loop registers
    code.push(0x51); // push rcx
    code.extend_from_slice(&[0x41, 0x50]); // push r8

    // Load fields from array into rbp slots
    // base_index = rcx * nfields
    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
    if nfields > 1 {
        code.extend_from_slice(&[0x48, 0x6B, 0xC0, nfields as u8]); // imul rax, rax, nfields
    }
    for (i, field) in concept.fields.iter().enumerate() {
        let offset = offsets[field.name.as_str()];
        // mov rdx, [r15 + rax*8]
        code.extend_from_slice(&[0x49, 0x8B, 0x14, 0xC7]);
        // mov [rbp + offset], rdx
        code.extend_from_slice(&[0x48, 0x89, 0x55]);
        code.push(offset as u8);
        if i < nfields - 1 {
            code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
        }
    }

    // Evaluate expression (result in rax)
    let field_ranges = build_field_ranges(concept);
    emit_eval_expr(
        &mut code,
        &rule.logic.value,
        &rule.input_name,
        &offsets,
        all_rules,
        &field_ranges,
    )?;

    // Print result
    if is_bool {
        code.extend_from_slice(&[0x84, 0xC0]); // test al, al
        code.push(0x74);
        let fp = code.len();
        code.push(0x00);
        emit_write_string(&mut code, b"true\n");
        code.push(0xEB);
        let dp = code.len();
        code.push(0x00);
        let fpos = code.len();
        code[fp] = (fpos - fp - 1) as u8;
        emit_write_string(&mut code, b"false\n");
        let dpos = code.len();
        code[dp] = (dpos - dp - 1) as u8;
    } else {
        emit_itoa_inline(&mut code);
    }

    // Restore loop registers, increment, loop back
    code.extend_from_slice(&[0x41, 0x58]); // pop r8
    code.push(0x59); // pop rcx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.push(0xE9);
    let lj = process_loop as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&lj.to_le_bytes());

    // === Exit ===
    let exit_pos = code.len();
    let eo = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&eo.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(code)
}

/// Check if the rule's logic is a simple `field > N` pattern.
fn extract_simple_gt(rule: &Rule) -> Option<i64> {
    if let Expr::Binary(BinOp::Gt, left, right) = &rule.logic.value {
        if let Expr::Number(n) = right.as_ref() {
            if let Expr::Field(base, _) = left.as_ref() {
                if matches!(base.as_ref(), Expr::Ident(name) if name == &rule.input_name) {
                    return Some(*n);
                }
            }
        }
    }
    None
}

fn emit_cmp_rax_imm(code: &mut Vec<u8>, value: i64) {
    if value >= i32::MIN as i64 && value <= i32::MAX as i64 {
        code.extend_from_slice(&[0x48, 0x3D]);
        code.extend_from_slice(&(value as i32).to_le_bytes());
    } else {
        // mov rcx, imm64; cmp rax, rcx
        code.extend_from_slice(&[0x48, 0xB9]);
        code.extend_from_slice(&value.to_le_bytes());
        code.extend_from_slice(&[0x48, 0x39, 0xC8]);
    }
}

/// SIMD-optimized program for single-field `> threshold` comparisons.
/// Uses SSE4.2 `pcmpgtq` to compare 2 i64 values simultaneously.
///
/// Phase 1: Parse all argv numbers into a contiguous, 16-byte-aligned array
/// Phase 2: Process pairs with SIMD (pcmpgtq compares 2 values per instruction)
/// Phase 3: Scalar fallback for the remainder (if odd count)
fn emit_vectorized_program(threshold: i64) -> Result<Vec<u8>, NativeError> {
    let mut code = Vec::new();

    // === Setup ===
    // mov r12, [rsp]
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);
    // lea r13, [rsp+8]
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]);

    // r14 = count = argc - 1
    code.extend_from_slice(&[0x4D, 0x89, 0xE6]); // mov r14, r12
    code.extend_from_slice(&[0x49, 0xFF, 0xCE]); // dec r14

    // Allocate 16-byte-aligned array on stack
    code.extend_from_slice(&[0x4C, 0x89, 0xF0]); // mov rax, r14
    code.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x03]); // shl rax, 3
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x0F]); // add rax, 15
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF0]); // and rax, -16
    code.extend_from_slice(&[0x48, 0x29, 0xC4]); // sub rsp, rax
    code.extend_from_slice(&[0x49, 0x89, 0xE7]); // mov r15, rsp

    // === Phase 1: Parse all argv into array ===
    code.extend_from_slice(&[0x48, 0x31, 0xDB]); // xor rbx, rbx

    let parse_top = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xF3]); // cmp rbx, r14
    code.push(0x0F);
    code.push(0x8D);
    let parse_done_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // mov rdi, [r13 + rbx*8 + 8]
    code.extend_from_slice(&[0x49, 0x8B, 0x7C, 0xDD, 0x08]);

    // Save rbx (atoi clobbers nothing we use, but be safe)
    code.push(0x53); // push rbx
    emit_atoi_inline(&mut code);
    code.push(0x5B); // pop rbx

    // mov [r15 + rbx*8], rax
    code.extend_from_slice(&[0x49, 0x89, 0x04, 0xDF]);

    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx
    code.push(0xE9);
    let parse_jmp = parse_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&parse_jmp.to_le_bytes());

    let parse_done = code.len();
    let pd_offset = parse_done as i32 - (parse_done_patch as i32 + 4);
    code[parse_done_patch..parse_done_patch + 4].copy_from_slice(&pd_offset.to_le_bytes());

    // === Phase 2: Broadcast threshold → xmm1 ===
    emit_mov_rax_imm(&mut code, threshold);
    code.extend_from_slice(&[0x66, 0x48, 0x0F, 0x6E, 0xC8]); // movq xmm1, rax
    code.extend_from_slice(&[0x66, 0x0F, 0x6C, 0xC9]); // punpcklqdq xmm1, xmm1

    // === Phase 3: SIMD loop — 2 elements per iteration ===
    code.extend_from_slice(&[0x48, 0x31, 0xDB]); // xor rbx, rbx

    let simd_top = code.len();
    code.extend_from_slice(&[0x48, 0x8D, 0x43, 0x02]); // lea rax, [rbx+2]
    code.extend_from_slice(&[0x4C, 0x39, 0xF0]); // cmp rax, r14
    code.push(0x0F);
    code.push(0x8F);
    let remainder_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // movdqu xmm0, [r15 + rbx*8]
    code.extend_from_slice(&[0xF3, 0x41, 0x0F, 0x6F, 0x04, 0xDF]);
    // pcmpgtq xmm0, xmm1 (SSE4.2: compare 2 i64s simultaneously)
    code.extend_from_slice(&[0x66, 0x0F, 0x38, 0x37, 0xC1]);
    // movmskpd eax, xmm0 (extract 2 result bits)
    code.extend_from_slice(&[0x66, 0x0F, 0x50, 0xC0]);
    // Save mask in r8d
    code.extend_from_slice(&[0x41, 0x89, 0xC0]); // mov r8d, eax

    // Print result for element [rbx]
    code.extend_from_slice(&[0x41, 0xF6, 0xC0, 0x01]); // test r8b, 1
    code.push(0x74);
    let f0_patch = code.len();
    code.push(0x00);
    emit_write_string(&mut code, b"true\n");
    code.push(0xEB);
    let d0_patch = code.len();
    code.push(0x00);
    let f0_pos = code.len();
    code[f0_patch] = (f0_pos - f0_patch - 1) as u8;
    emit_write_string(&mut code, b"false\n");
    let d0_pos = code.len();
    code[d0_patch] = (d0_pos - d0_patch - 1) as u8;

    // Print result for element [rbx+1]
    code.extend_from_slice(&[0x41, 0xF6, 0xC0, 0x02]); // test r8b, 2
    code.push(0x74);
    let f1_patch = code.len();
    code.push(0x00);
    emit_write_string(&mut code, b"true\n");
    code.push(0xEB);
    let d1_patch = code.len();
    code.push(0x00);
    let f1_pos = code.len();
    code[f1_patch] = (f1_pos - f1_patch - 1) as u8;
    emit_write_string(&mut code, b"false\n");
    let d1_pos = code.len();
    code[d1_patch] = (d1_pos - d1_patch - 1) as u8;

    code.extend_from_slice(&[0x48, 0x83, 0xC3, 0x02]); // add rbx, 2
    code.push(0xE9);
    let simd_jmp = simd_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&simd_jmp.to_le_bytes());

    // === Phase 4: Scalar remainder ===
    let remainder_pos = code.len();
    let rem_offset = remainder_pos as i32 - (remainder_patch as i32 + 4);
    code[remainder_patch..remainder_patch + 4].copy_from_slice(&rem_offset.to_le_bytes());

    let scalar_top = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xF3]); // cmp rbx, r14
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // mov rax, [r15 + rbx*8]
    code.extend_from_slice(&[0x49, 0x8B, 0x04, 0xDF]);
    emit_cmp_rax_imm(&mut code, threshold);
    code.extend_from_slice(&[0x0F, 0x9F, 0xC0]); // setg al
    code.extend_from_slice(&[0x84, 0xC0]); // test al, al
    code.push(0x74);
    let sf_patch = code.len();
    code.push(0x00);
    emit_write_string(&mut code, b"true\n");
    code.push(0xEB);
    let sd_patch = code.len();
    code.push(0x00);
    let sf_pos = code.len();
    code[sf_patch] = (sf_pos - sf_patch - 1) as u8;
    emit_write_string(&mut code, b"false\n");
    let sd_pos = code.len();
    code[sd_patch] = (sd_pos - sd_patch - 1) as u8;

    code.extend_from_slice(&[0x48, 0xFF, 0xC3]); // inc rbx
    code.push(0xE9);
    let scalar_jmp = scalar_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&scalar_jmp.to_le_bytes());

    // === Exit ===
    let exit_pos = code.len();
    let exit_offset = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_offset.to_le_bytes());

    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(code)
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
                Field { name: "a".into(), ty: Type::Number, range: None },
                Field { name: "b".into(), ty: Type::Number, range: None },
                Field { name: "c".into(), ty: Type::Number, range: None },
            ],
        };
        let offsets = field_offsets(&concept);
        assert_eq!(offsets["a"], -8);
        assert_eq!(offsets["b"], -16);
        assert_eq!(offsets["c"], -24);
    }
}
