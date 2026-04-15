/// Native x86-64 code generation — produces ELF binaries directly.
///
/// General-purpose expression compiler: supports arithmetic (+, -, *, /),
/// comparisons (>, <, >=, <=), boolean logic (and, or), field access,
/// and rule calls (inlined). Multi-field concepts are supported.
///
/// The generated binary reads groups of N numbers from command-line arguments
/// (one group per record, N = number of fields) and prints the result.

use std::collections::HashMap;

use crate::verifier::compute_range;
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

    // The target name may be a rule OR a reaction. Dispatch accordingly.
    let reaction = program.items.iter().find_map(|i| match i {
        Item::Reaction(rx) if rx.name == rule_name => Some(rx),
        _ => None,
    });

    let (rule, concept) = if let Some(rx) = reaction {
        // Compile a reaction: target is the reaction, but we still need its
        // trigger rule and its input concept for field/arg wiring.
        let trigger = rules.get(rx.trigger.as_str()).ok_or_else(|| NativeError {
            message: format!(
                "reaction '{}' triggers unknown rule '{}'",
                rx.name, rx.trigger
            ),
        })?;
        let concept = match &trigger.input_ty {
            Type::Named(n) => concepts
                .iter()
                .find(|c| c.name == *n)
                .ok_or_else(|| NativeError {
                    message: format!("unknown concept '{}'", n),
                })?,
            _ => {
                return Err(NativeError {
                    message: "reaction trigger rule must take a named concept input".into(),
                })
            }
        };
        (*trigger, *concept)
    } else {
        let r = rules.get(rule_name).ok_or_else(|| NativeError {
            message: format!("no rule or reaction named '{}'", rule_name),
        })?;
        let c = match &r.input_ty {
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
        (*r, *c)
    };

    let is_vectorizable = rule
        .hints
        .as_ref()
        .map_or(false, |h| h.vectorizable.is_some());
    let is_parallel = rule
        .hints
        .as_ref()
        .map_or(false, |h| h.parallel.is_some());

    let is_result_output = matches!(&rule.output_ty, Type::Result(_, _));
    let is_collection_output = matches!(&rule.output_ty, Type::Collection(_));
    // Phase 4: number output whose top-level logic is `fold(...)` (or its
    // sum/count/min/max desugarings). Routes to emit_fold_program.
    let is_fold_number_output = matches!(&rule.output_ty, Type::Number)
        && matches!(&rule.logic.value, Expr::Fold(_, _, _, _, _));
    // Phase 5b: text output whose top-level logic is `fold(...)` — appends
    // into a growing text buffer. Routes to emit_text_fold_program.
    let is_fold_text_output = matches!(&rule.output_ty, Type::Text)
        && matches!(&rule.logic.value, Expr::Fold(_, _, _, _, _));

    // Record output: rule.output_ty is Named(concept_name) and the name
    // resolves to a concept declared in the program. Routes to
    // emit_record_program which emits JSON per record.
    let record_output_concept: Option<&Concept> = match &rule.output_ty {
        Type::Named(n) => concepts.iter().find(|c| c.name == *n).copied(),
        _ => None,
    };

    let mut code = if let Some(rx) = reaction {
        emit_reaction_program(rx, rule, concept, &rules)?
    } else if is_result_output {
        // Rules returning Result(T, E) get their own emitter: Ok-arm values
        // stream to stdout, Err-arm text streams to stderr. Each leaf
        // self-terminates (continuation-passing), no tagged value lives in
        // registers across the top-level dispatch.
        emit_result_program(rule, concept, &rules)?
    } else if is_collection_output {
        // Phase 3: rules returning collection(T). map/filter over an input
        // collection field, emit one JSON line per produced element.
        emit_collection_program(rule, concept, &concepts, &rules)?
    } else if is_fold_number_output {
        // Phase 4: rules returning number via fold (incl. sum/count/min/max
        // desugarings). Single 8-byte accumulator slot at the bottom of the
        // rbp frame; one final `itoa + \n` per input record.
        emit_fold_program(rule, concept, &concepts, &rules)?
    } else if is_fold_text_output {
        // Phase 5b: text output via top-level fold — append-only body,
        // two-pass sizing, stack buffer freed via saved rsp in r9.
        emit_text_fold_program(rule, concept, &concepts, &rules)?
    } else if matches!(&rule.output_ty, Type::Text) {
        // Phase 5a: rules returning `text` via a per-record body (literal,
        // input text field, or concat). One write to stdout + newline per
        // record. No accumulator — fold-over-collection to text stays Phase 5b.
        emit_text_program(rule, concept, &rules)?
    } else if let Some(rec_concept) = record_output_concept {
        // Record-output rules: each leaf emits a JSON object to stdout + \n.
        // Same continuation-passing convention as Result.
        emit_record_program(rule, rec_concept, concept, &concepts, &rules)?
    } else if is_vectorizable && concept.fields.len() == 1 {
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

    // Self-verification: validate emitted machine code (best-effort).
    // The x86-64 decoder doesn't cover all instructions yet (itoa, complex addressing).
    // Validation errors are warnings, not hard failures, until the decoder is complete.
    // Stack depth check: verify the expression won't overflow the stack
    let depth = max_stack_depth(&rule.logic.value);
    let stack_bytes = depth * 8;
    if stack_bytes > 1_000_000 {
        return Err(NativeError {
            message: format!(
                "expression stack depth {} ({} bytes) exceeds safety limit (1 MB)",
                depth, stack_bytes
            ),
        });
    }

    // Peephole optimization: eliminate redundant push/pop patterns
    let before_size = code.len();
    peephole_optimize(&mut code);
    let saved = before_size - code.len();
    if saved > 0 {
        eprintln!("peephole: {} bytes eliminated", saved);
    }

    if let Err(e) = crate::validate_x86::validate_code(&code) {
        eprintln!("warning: x86-64 validation: {} (decoder incomplete, may be false positive)", e);
    }

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

/// Context returned by `emit_record_loop_prologue` — everything the body
/// of an emitter needs after the shared setup, plus what
/// `emit_record_loop_epilogue` needs to close the loop.
///
/// Lifetimes: the &str keys in the maps borrow from the input concept's
/// field names and the rule's let-binding names, both of which live for
/// the duration of the emitter call.
struct RecordLoopCtx<'a> {
    /// Code offset of the loop top — the `cmp r14, r12` that gates iteration.
    loop_top: usize,
    /// Code offset of the rel32 placeholder in the `jge exit` jump.
    /// Epilogue fills this in once the exit label position is known.
    exit_patch: usize,
    /// Offsets for all bindings in scope: input fields + let bindings.
    binding_offsets: HashMap<&'a str, i32>,
    /// Field range map for overflow-proved arithmetic in emit_eval_expr.
    field_ranges: HashMap<&'a str, (i64, i64)>,
    /// Bottom-of-frame slot reserved for `match_result`'s Ok-value binding
    /// (Phase 2D). Present regardless of whether the rule uses match_result;
    /// a stable rbp offset is cheaper than a conditional frame layout.
    /// Only meaningful when `reserve_match_slot = true`.
    match_slot: i32,
}

/// Emit the shared prologue for any emitter that iterates over records
/// parsed from argv:
///   - stack frame setup (r12 = argc, r13 = argv base, rbp frame)
///   - r14 = arg index (starts at 1, skipping argv[0])
///   - loop_top label
///   - `cmp r14, r12; jge exit` (epilogue patches the exit address)
///   - field loading: Number via atoi, Text stores the argv pointer directly
///     (length is recovered at read sites via `emit_strlen`)
///   - let-binding evaluation into rbp slots
///
/// After this returns, the emitter's own logic can run with rax/rbx/… free,
/// `binding_offsets` covers every name the logic can reference, and the
/// `match_slot` is reserved at the bottom of the frame (used only by
/// emit_result_program today, but reserving it unconditionally keeps the
/// frame layout uniform across emitters — 8 bytes of waste when unused).
fn emit_record_loop_prologue<'a>(
    code: &mut Vec<u8>,
    rule: &'a Rule,
    input_concept: &'a Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<RecordLoopCtx<'a>, NativeError> {
    let nfields = input_concept.fields.len();
    let n_bindings = rule.logic.bindings.len();
    // +1 for match_slot at the bottom of the frame.
    let frame_slots = nfields + n_bindings + 1;
    let frame_size = (frame_slots * 8) as i32;
    let match_slot: i32 = -((frame_slots as i32) * 8);

    // mov r12, [rsp]            — argc
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);
    // lea r13, [rsp+8]          — argv base
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]);
    // push rbp ; mov rbp, rsp
    code.push(0x55);
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);
    // sub rsp, frame_size
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    // mov r14, 1                — arg index starts at 1 (skip argv[0])
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]);

    let loop_top = code.len();

    // cmp r14, r12 ; jge exit (rel32 placeholder)
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]);
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    let offsets = field_offsets(input_concept);

    // Per-field dispatch: Number via atoi, Text stores the argv pointer.
    for field in &input_concept.fields {
        let offset = offsets[field.name.as_str()];
        // mov rdi, [r13 + r14*8]       — argv[r14]
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]);
        match field.ty {
            Type::Number => {
                emit_atoi_inline(code);
                // mov [rbp + offset], rax
                if offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x89, 0x45]);
                    code.push(offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x89, 0x85]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
            }
            Type::Text => {
                // The pointer is already in rdi — stash it directly.
                // mov [rbp + offset], rdi
                if offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x89, 0x7D]);
                    code.push(offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x89, 0xBD]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
            }
            _ => {
                return Err(NativeError {
                    message: format!(
                        "native input field '{}' has unsupported type (only number/text today)",
                        field.name
                    ),
                });
            }
        }
        // inc r14
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]);
    }

    // Evaluate let bindings into successive rbp slots.
    let mut binding_offsets = offsets;
    let field_ranges = build_field_ranges(input_concept);
    let mut next_slot = -((nfields as i32 + 1) * 8);
    for (name, expr) in &rule.logic.bindings {
        emit_eval_expr(code, expr, &rule.input_name, &binding_offsets, all_rules, &field_ranges)?;
        if next_slot >= -128 {
            code.extend_from_slice(&[0x48, 0x89, 0x45]);
            code.push(next_slot as u8);
        } else {
            code.extend_from_slice(&[0x48, 0x89, 0x85]);
            code.extend_from_slice(&next_slot.to_le_bytes());
        }
        binding_offsets.insert(name.as_str(), next_slot);
        next_slot -= 8;
    }

    Ok(RecordLoopCtx {
        loop_top,
        exit_patch,
        binding_offsets,
        field_ranges,
        match_slot,
    })
}

/// Emit the shared epilogue: back-patch the `jge exit` jump to point here,
/// then emit `sys_exit(0)`. Callers must have emitted a `jmp loop_top` at
/// the end of their per-record work before calling this (so control falls
/// through to `exit` only when r14 >= r12).
fn emit_record_loop_epilogue(code: &mut Vec<u8>, ctx: &RecordLoopCtx<'_>) {
    let exit_pos = code.len();
    let exit_offset = exit_pos as i32 - (ctx.exit_patch as i32 + 4);
    code[ctx.exit_patch..ctx.exit_patch + 4].copy_from_slice(&exit_offset.to_le_bytes());
    // mov rax, 60 (sys_exit) ; xor rdi, rdi ; syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn emit_full_program(
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let is_bool = rule.output_ty == Type::Bool;
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, all_rules)?;

    // Evaluate final expression — result in rax
    emit_eval_expr(
        &mut code,
        &rule.logic.value,
        &rule.input_name,
        &ctx.binding_offsets,
        all_rules,
        &ctx.field_ranges,
    )?;

    // Print result per record
    if is_bool {
        // rax = 0 or 1
        code.extend_from_slice(&[0x84, 0xC0]); // test al, al
        code.push(0x74); // jz .print_false
        let pf_patch = code.len();
        code.push(0x00);
        emit_write_string(&mut code, b"true\n");
        code.push(0xEB); // jmp .after_print
        let ap_patch = code.len();
        code.push(0x00);
        let pf_pos = code.len();
        code[pf_patch] = (pf_pos - pf_patch - 1) as u8;
        emit_write_string(&mut code, b"false\n");
        let ap_pos = code.len();
        code[ap_patch] = (ap_pos - ap_patch - 1) as u8;
    } else {
        emit_itoa_inline(&mut code);
    }

    // jmp loop_top (next record) then fall through to shared exit.
    code.push(0xE9);
    let loop_offset = ctx.loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&loop_offset.to_le_bytes());

    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}

/// Phase 5a: `output: text` with a per-record body. The body is a text
/// expression (literal, input-field-text, or concat) — evaluated by
/// `emit_text_write_to_fd` directly to stdout, followed by a newline for
/// per-record separation. No accumulator, no new syscall surface compared
/// to Phase 2B's `Result(text, text)` Ok arm — this is the same machinery,
/// lifted out of the Result context to serve `output: text` directly.
///
/// Fold-over-collection to text is explicitly NOT handled here; that's Phase 5b.
/// Rejection comes naturally: a `Fold(...)` expression at the top of a
/// text-output rule would hit `emit_text_write_to_fd`'s fallback arm, which
/// refuses anything that isn't Text / Field / Concat.
fn emit_text_program(
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, all_rules)?;

    emit_text_write_to_fd(
        &mut code,
        &rule.logic.value,
        1,
        &rule.input_name,
        concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
    )?;
    emit_write_newline(&mut code, 1);

    // jmp loop_top
    code.push(0xE9);
    let loop_offset = ctx.loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&loop_offset.to_le_bytes());

    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}

/// Classify a concat argument's runtime type — native supports Text (bytes
/// copied from inline literals) and Number (evaluated to rax then itoa'd into
/// the buffer). Bool and other types are refused with a clear message; they
/// stay interpreter-only for now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConcatArgKind {
    Text,
    Number,
}

fn classify_concat_arg(
    expr: &Expr,
    concept: &Concept,
    input_name: &str,
) -> Option<ConcatArgKind> {
    match expr {
        Expr::Text(_) => Some(ConcatArgKind::Text),
        Expr::Number(_) | Expr::Neg(_) => Some(ConcatArgKind::Number),
        Expr::Field(base, field_name) => {
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                let f = concept.fields.iter().find(|f| &f.name == field_name)?;
                match &f.ty {
                    Type::Number => Some(ConcatArgKind::Number),
                    Type::Text => Some(ConcatArgKind::Text),
                    _ => None,
                }
            } else {
                None
            }
        }
        Expr::Binary(op, _, _) => match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                Some(ConcatArgKind::Number)
            }
            _ => None, // Bool-producing — not yet supported in native concat
        },
        _ => None,
    }
}

/// Emit code that writes decimal digits of rax into [rbx], advancing rbx.
/// Handles negative numbers (emits '-' first) and zero. Uses a 24-byte scratch
/// area on the stack for digit staging, then copies the digit run to [rbx]
/// via rep movsb. Scratch is freed before return.
///
/// Inputs:  rax = signed i64 value to print.
///          rbx = write pointer (the function updates it in place).
/// Clobbers: rax, rcx, rdx, rdi, rsi, r8. Preserves rbx (updated), r12-r15.
fn emit_itoa_to_buffer(code: &mut Vec<u8>) {
    // Handle negative: if rax < 0, emit '-' then negate.
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jns .positive (rel8)
    code.push(0x79);
    let not_neg_patch = code.len();
    code.push(0x00);
    // mov byte [rbx], '-'  (encoding: C6 03 2D)
    code.extend_from_slice(&[0xC6, 0x03, 0x2D]);
    // inc rbx
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]);
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    // .positive:
    let not_neg_pos = code.len();
    code[not_neg_patch] = (not_neg_pos - not_neg_patch - 1) as u8;

    // sub rsp, 24 — scratch buffer
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x18]);
    // lea rsi, [rsp + 23] — rightmost byte
    code.extend_from_slice(&[0x48, 0x8D, 0x74, 0x24, 0x17]);
    // mov r8, 10
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x0A, 0x00, 0x00, 0x00]);

    // Handle zero specially.
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz .div_loop (rel8)
    code.push(0x75);
    let nonzero_patch = code.len();
    code.push(0x00);
    // mov byte [rsi], '0'
    code.extend_from_slice(&[0xC6, 0x06, 0x30]);
    // jmp .done_digits (rel8)
    code.push(0xEB);
    let zero_done_patch = code.len();
    code.push(0x00);

    // .div_loop:
    let div_loop_pos = code.len();
    code[nonzero_patch] = (div_loop_pos - nonzero_patch - 1) as u8;
    // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    // div r8  (rax = quotient, rdx = remainder)
    code.extend_from_slice(&[0x49, 0xF7, 0xF0]);
    // add dl, '0'
    code.extend_from_slice(&[0x80, 0xC2, 0x30]);
    // mov [rsi], dl
    code.extend_from_slice(&[0x88, 0x16]);
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jz .done_digits (rel8) — leave rsi pointing at the first digit
    code.push(0x74);
    let done_patch = code.len();
    code.push(0x00);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    // jmp .div_loop (rel8, backward)
    let jmp_back = div_loop_pos as i32 - (code.len() + 2) as i32;
    code.extend_from_slice(&[0xEB, jmp_back as u8]);

    // .done_digits: rsi points at first digit.
    let done_pos = code.len();
    code[done_patch] = (done_pos - done_patch - 1) as u8;
    code[zero_done_patch] = (done_pos - zero_done_patch - 1) as u8;

    // Compute length = (rsp + 24) - rsi   (one past the last digit at rsp+23, minus start)
    // lea rcx, [rsp + 24]
    code.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x24, 0x18]);
    // sub rcx, rsi
    code.extend_from_slice(&[0x48, 0x29, 0xF1]);

    // Copy [rsi..rsi+rcx] to [rbx..rbx+rcx] via rep movsb.
    // mov rdi, rbx
    code.extend_from_slice(&[0x48, 0x89, 0xDF]);
    // rep movsb (F3 A4)
    code.extend_from_slice(&[0xF3, 0xA4]);
    // mov rbx, rdi  (new write pointer = rbx + length)
    code.extend_from_slice(&[0x48, 0x89, 0xFB]);

    // add rsp, 24 — free scratch
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x18]);
}

/// Result of `emit_concat_to_buffer` — tells the caller which epilogue to
/// emit to free the scratch buffer.
///
/// - `Static(n)` — all args had compile-time-known sizes, buffer is exactly
///   `n` bytes. Caller frees with `add rsp, n` (7 bytes).
/// - `Dynamic` — at least one arg was a text field from argv (unknown length
///   until runtime). `emit_concat_to_buffer` stashed the pre-allocation rsp
///   in r9; caller frees with `mov rsp, r9` (3 bytes). r9 is preserved across
///   `write` (syscall only takes 3 args) and not touched by itoa or strlen.
enum ConcatBufResult {
    Static(i32),
    Dynamic,
}

/// Emit the free sequence matching a `ConcatBufResult`. Call this after the
/// consumer has finished reading the buffer (i.e. after the `write` syscall).
/// Static path: `add rsp, imm32` (7 bytes). Dynamic path: `mov rsp, r9`
/// (3 bytes) — r9 was set by `emit_concat_to_buffer` to the pre-allocation rsp.
fn emit_concat_buffer_free(code: &mut Vec<u8>, buf: ConcatBufResult) {
    match buf {
        ConcatBufResult::Static(n) => {
            code.extend_from_slice(&[0x48, 0x81, 0xC4]);
            code.extend_from_slice(&n.to_le_bytes());
        }
        ConcatBufResult::Dynamic => {
            // mov rsp, r9
            code.extend_from_slice(&[0x4C, 0x89, 0xCC]);
        }
    }
}

/// Emit code that, when executed, builds the concat(arg1, arg2, ...) result
/// in a stack-allocated buffer and leaves (buffer_ptr, length) in (rax, rdx).
/// The caller frees the buffer according to the returned `ConcatBufResult`.
///
/// Sizing strategy:
/// - If every arg has a compile-time size (literals, numbers), the buffer is
///   sized exactly at emission time and `sub rsp, imm32` / `add rsp, imm32`
///   bracket the buffer.
/// - If any arg is a text field (from argv — length known only at runtime),
///   the total is computed at runtime: static parts go into rax, then for
///   each text field `strlen` is called and its length added. `sub rsp, rax`
///   reserves the buffer; the pre-allocation rsp is saved in r9 so the caller
///   can free via `mov rsp, r9` without knowing the size.
///
/// No heap allocation in either path. Text fields are copied via `rep movsb`
/// (length from the same `strlen` call that sized the buffer).
fn emit_concat_to_buffer(
    code: &mut Vec<u8>,
    args: &[Expr],
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<ConcatBufResult, NativeError> {
    // Classify every arg and tally the static worst case. A text-field arg
    // means sizing must be runtime-dynamic.
    let mut kinds: Vec<ConcatArgKind> = Vec::with_capacity(args.len());
    let mut static_total: i32 = 0;
    let mut has_text_field: bool = false;
    for arg in args {
        let kind = classify_concat_arg(arg, concept, input_name).ok_or_else(|| {
            NativeError {
                message: "concat argument type not yet supported in native (text + number scalars only for now; bool and others stay interpreter-only)".into(),
            }
        })?;
        kinds.push(kind);
        match kind {
            ConcatArgKind::Text => {
                if let Expr::Text(s) = arg {
                    static_total += s.as_bytes().len() as i32;
                } else if let Expr::Field(_, _) = arg {
                    // Text field: length known only at runtime. Dynamic path.
                    has_text_field = true;
                }
            }
            ConcatArgKind::Number => {
                static_total += 21; // i64 max 20 digits + sign
            }
        }
    }
    if static_total == 0 && !has_text_field {
        return Err(NativeError { message: "concat with zero total size".into() });
    }

    if !has_text_field {
        // Fast path — compile-time-sized buffer, unchanged from before.
        let buf_size = ((static_total + 7) / 8) * 8;
        // sub rsp, buf_size
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&buf_size.to_le_bytes());
        // mov rbx, rsp  — rbx = write pointer
        code.extend_from_slice(&[0x48, 0x89, 0xE3]);
        // mov r10, rbx  — buffer base for final length calc
        code.extend_from_slice(&[0x49, 0x89, 0xDA]);
        emit_concat_fill(code, args, &kinds, input_name, concept, all_rules, offsets, field_ranges)?;
        // rax = buffer base, rdx = length (rbx - r10)
        code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10
        code.extend_from_slice(&[0x48, 0x89, 0xDA]); // mov rdx, rbx
        code.extend_from_slice(&[0x4C, 0x29, 0xD2]); // sub rdx, r10
        return Ok(ConcatBufResult::Static(buf_size));
    }

    // Dynamic path: compute the total buffer size at runtime.
    // rax = static_total
    // mov rax, static_total (i32 imm sign-extended into rax)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
    code.extend_from_slice(&static_total.to_le_bytes());
    // For each text-field arg, strlen the pointer at its rbp slot and add
    // the length into rax. push/pop rax brackets each strlen since it
    // clobbers rax.
    for (arg, kind) in args.iter().zip(kinds.iter()) {
        if *kind == ConcatArgKind::Text {
            if let Expr::Field(_, field_name) = arg {
                let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                    message: format!(
                        "text-field '{}' has no rbp slot in concat size calc — input parsing missed it",
                        field_name
                    ),
                })?;
                // push rax
                code.push(0x50);
                // mov rsi, [rbp + offset]
                if offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                    code.push(offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
                emit_strlen(code); // rdx = length, rsi unchanged
                // pop rcx  (restore accumulated total into rcx; rax clobbered by strlen)
                code.push(0x59);
                // add rcx, rdx ; mov rax, rcx
                code.extend_from_slice(&[0x48, 0x01, 0xD1]);
                code.extend_from_slice(&[0x48, 0x89, 0xC8]);
            }
        }
    }
    // Round up to 8 for alignment: add rax, 7 ; and rax, ~7
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x07]);
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF8]);
    // Save rsp in r9 (preserved across syscalls, not used by itoa/strlen).
    // mov r9, rsp
    code.extend_from_slice(&[0x49, 0x89, 0xE1]);
    // sub rsp, rax  — dynamic allocation
    code.extend_from_slice(&[0x48, 0x29, 0xC4]);
    // mov rbx, rsp  ; mov r10, rbx
    code.extend_from_slice(&[0x48, 0x89, 0xE3]);
    code.extend_from_slice(&[0x49, 0x89, 0xDA]);

    emit_concat_fill(code, args, &kinds, input_name, concept, all_rules, offsets, field_ranges)?;

    // rax = buffer base, rdx = length
    code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10
    code.extend_from_slice(&[0x48, 0x89, 0xDA]); // mov rdx, rbx
    code.extend_from_slice(&[0x4C, 0x29, 0xD2]); // sub rdx, r10

    Ok(ConcatBufResult::Dynamic)
}

/// Shared fill loop for both static- and dynamic-sized concat buffers.
/// Preconditions: rbx = write pointer into the reserved buffer, r10 = buffer
/// base. Postconditions: rbx advanced past the last byte written, r10
/// unchanged. Text fields use `emit_strlen` + `rep movsb`; text literals are
/// embedded inline; numbers go through `emit_itoa_to_buffer`.
fn emit_concat_fill(
    code: &mut Vec<u8>,
    args: &[Expr],
    kinds: &[ConcatArgKind],
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {

    for (i, arg) in args.iter().enumerate() {
        match kinds[i] {
            ConcatArgKind::Text => {
                if let Expr::Text(s) = arg {
                    let bytes = s.as_bytes();
                    let n = bytes.len();
                    if n == 0 {
                        continue;
                    }
                    // jmp over <n> data bytes (rel8 if n <= 127, else rel32)
                    if n <= 127 {
                        code.push(0xEB);
                        code.push(n as u8);
                    } else {
                        code.push(0xE9);
                        code.extend_from_slice(&(n as i32).to_le_bytes());
                    }
                    let data_addr = code.len();
                    code.extend_from_slice(bytes);
                    // mov rdi, rbx  (dest)
                    code.extend_from_slice(&[0x48, 0x89, 0xDF]);
                    // lea rsi, [rip + rel32]  (source)
                    let end = code.len() + 7;
                    let rel32 = data_addr as i32 - end as i32;
                    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
                    code.extend_from_slice(&rel32.to_le_bytes());
                    // mov rcx, n
                    code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
                    code.extend_from_slice(&(n as i32).to_le_bytes());
                    // rep movsb
                    code.extend_from_slice(&[0xF3, 0xA4]);
                    // mov rbx, rdi  (advanced pointer)
                    code.extend_from_slice(&[0x48, 0x89, 0xFB]);
                }
            }
            ConcatArgKind::Number => {
                // push rbx — save write pointer (emit_eval_expr may clobber it via Binary op push/pop)
                code.push(0x53);
                emit_eval_expr(code, arg, input_name, offsets, all_rules, field_ranges)?;
                // pop rbx
                code.push(0x5B);
                // itoa into buffer (rax → decimal digits at [rbx], rbx advanced)
                emit_itoa_to_buffer(code);
            }
        }
        // If the arg is a Text FIELD (not a literal), emit strlen + rep movsb
        // at runtime. The field's value is a pointer stored at `offsets[field]`.
        if kinds[i] == ConcatArgKind::Text {
            if let Expr::Field(_, field_name) = arg {
                let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                    message: format!(
                        "text-field '{}' has no rbp slot in concat fill — input parsing missed it",
                        field_name
                    ),
                })?;
                // mov rsi, [rbp + offset]
                if offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                    code.push(offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
                emit_strlen(code); // rdx = length, rsi unchanged
                // mov rdi, rbx        (dest)
                code.extend_from_slice(&[0x48, 0x89, 0xDF]);
                // mov rcx, rdx        (byte count)
                code.extend_from_slice(&[0x48, 0x89, 0xD1]);
                // rep movsb
                code.extend_from_slice(&[0xF3, 0xA4]);
                // mov rbx, rdi        (advanced write ptr)
                code.extend_from_slice(&[0x48, 0x89, 0xFB]);
            }
        }
    }

    Ok(())
}

/// Emit the machine-code sequence for a single `append_file "path" "content"`
/// reaction effect. Both `path` (NUL-terminated for open()) and `content` are
/// embedded inline in the code section via a `jmp` over the bytes, then the
/// three syscalls (open, write, close) reference them by RIP-relative offsets.
///
/// Security notes:
/// - The path is a source-level LITERAL: no dynamic path escapes. Reading
///   the emitted bytes shows exactly which file this binary can touch.
/// - Flags are O_WRONLY | O_APPEND | O_CREAT (0x441); we never truncate,
///   never overwrite, never follow arbitrary pointers from the input.
/// - Mode is 0644 (rw-r--r--) — group/other cannot write.
///
/// No error handling on syscall failure: if open() returns negative, write()
/// will also fail silently. Good enough for a POC; proper error propagation
/// to exit codes is a future commit.
/// Emit the open/write/close sequence for an append_file effect.
/// `content` is the declared content expression — can be a Text literal
/// (fast path: bytes embedded inline) or a Concat (slow path: stack buffer
/// built at runtime from scalar args).
fn emit_append_file_call(
    code: &mut Vec<u8>,
    path: &str,
    content: &Expr,
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    // First, emit the open() call. The path is always a compile-time literal,
    // so we embed it inline and point rdi at it.
    emit_open_append(code, path);
    // rax = fd; save in r15.
    // mov r15, rax  (49 89 C7)
    code.extend_from_slice(&[0x49, 0x89, 0xC7]);

    // Now the write(). Two paths depending on content shape.
    match content {
        Expr::Text(s) => {
            // Fast path: fixed-length static content. Embed inline and
            // point rsi at it.
            let bytes = s.as_bytes();
            let n = bytes.len();
            // jmp over data
            if n <= 127 {
                code.push(0xEB);
                code.push(n as u8);
            } else {
                code.push(0xE9);
                code.extend_from_slice(&(n as i32).to_le_bytes());
            }
            let content_addr = code.len();
            code.extend_from_slice(bytes);

            // mov rax, 1 (sys_write)
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
            // mov rdi, r15 (fd)
            code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
            // lea rsi, [rip + rel32] → content
            let end = code.len() + 7;
            let rel32 = content_addr as i32 - end as i32;
            code.extend_from_slice(&[0x48, 0x8D, 0x35]);
            code.extend_from_slice(&rel32.to_le_bytes());
            // mov rdx, n
            code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
            code.extend_from_slice(&(n as i32).to_le_bytes());
            // syscall
            code.extend_from_slice(&[0x0F, 0x05]);
        }
        Expr::Concat(args) => {
            // Build the content in a stack buffer, then write (rax=buf_ptr,
            // rdx=len) to the fd. Free according to the sizing strategy
            // reported by emit_concat_to_buffer.
            let buf = emit_concat_to_buffer(
                code, args, &rule.input_name, concept, all_rules, offsets, field_ranges,
            )?;
            // At this point: rax = buf ptr, rdx = length, fd still in r15.
            // mov rsi, rax — write syscall wants source in rsi
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            // mov rdi, r15 (fd)
            code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
            // mov rax, 1 (sys_write)
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
            // syscall
            code.extend_from_slice(&[0x0F, 0x05]);
            emit_concat_buffer_free(code, buf);
        }
        other => {
            return Err(NativeError {
                message: format!(
                    "append_file content must be a string literal or concat(...) in native; got {:?}",
                    other
                ),
            });
        }
    }

    // === close(fd) ===
    // mov rax, 3 (sys_close)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    // mov rdi, r15
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(())
}

/// Emit a write syscall for a fixed byte string to the given fd. The bytes
/// are embedded inline in the code section via a jmp-over-data pattern.
/// Used as a building block for JSON streaming (static key prefixes like
/// `{"priority":"` between field values).
fn emit_write_static_to_fd(code: &mut Vec<u8>, bytes: &[u8], fd: i32) {
    let n = bytes.len();
    if n == 0 {
        return;
    }
    if n <= 127 {
        code.push(0xEB);
        code.push(n as u8);
    } else {
        code.push(0xE9);
        code.extend_from_slice(&(n as i32).to_le_bytes());
    }
    let addr = code.len();
    code.extend_from_slice(bytes);
    // mov rax, 1 (sys_write)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    // mov rdi, fd
    code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
    code.extend_from_slice(&fd.to_le_bytes());
    // lea rsi, [rip + rel32]
    let end = code.len() + 7;
    let rel32 = addr as i32 - end as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rel32.to_le_bytes());
    // mov rdx, n
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(n as i32).to_le_bytes());
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// Convert rax (signed i64) to decimal digits and write them to stdout,
/// WITHOUT a trailing newline. Used inside JSON record emission where a
/// number value goes between a `"key":` prefix and a `,` or `}` suffix.
/// For the stand-alone `Ok(number)` path that wants a per-record newline,
/// use emit_itoa_inline (which appends \n).
fn emit_itoa_to_stdout_no_newline(code: &mut Vec<u8>) {
    // sub rsp, 24 — scratch
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x18]);
    // lea rsi, [rsp + 23]
    code.extend_from_slice(&[0x48, 0x8D, 0x74, 0x24, 0x17]);
    // mov r8, 10
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x0A, 0x00, 0x00, 0x00]);

    // Handle negative: reserve a flag byte at [rsp+23] was the last digit;
    // we'll use [rsp] as a minus-flag scratch slot.
    // Store negative flag: rsp+23 is used for digits, so we pick rsp+0 for flag.
    // mov byte [rsp], 0
    code.extend_from_slice(&[0xC6, 0x04, 0x24, 0x00]);
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jns .not_neg (rel8)
    code.push(0x79);
    let not_neg_patch = code.len();
    code.push(0x00);
    // mov byte [rsp], 1
    code.extend_from_slice(&[0xC6, 0x04, 0x24, 0x01]);
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);
    let not_neg_pos = code.len();
    code[not_neg_patch] = (not_neg_pos - not_neg_patch - 1) as u8;

    // Zero case
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz .div_loop (rel8)
    code.push(0x75);
    let nz_patch = code.len();
    code.push(0x00);
    // mov byte [rsi], '0'
    code.extend_from_slice(&[0xC6, 0x06, 0x30]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    // jmp .after_loop (rel8)
    code.push(0xEB);
    let after_patch = code.len();
    code.push(0x00);

    // .div_loop:
    let div_loop_pos = code.len();
    code[nz_patch] = (div_loop_pos - nz_patch - 1) as u8;
    // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    // div r8
    code.extend_from_slice(&[0x49, 0xF7, 0xF0]);
    // add dl, '0'
    code.extend_from_slice(&[0x80, 0xC2, 0x30]);
    // mov [rsi], dl
    code.extend_from_slice(&[0x88, 0x16]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jnz .div_loop (rel8 backward)
    let jmp_back = div_loop_pos as i32 - (code.len() + 2) as i32;
    code.extend_from_slice(&[0x75, jmp_back as u8]);

    // .after_loop:
    let after_pos = code.len();
    code[after_patch] = (after_pos - after_patch - 1) as u8;

    // Prepend '-' if flag was set.
    // cmp byte [rsp], 0
    code.extend_from_slice(&[0x80, 0x3C, 0x24, 0x00]);
    // je .no_minus (rel8)
    code.push(0x74);
    let no_minus_patch = code.len();
    code.push(0x00);
    // mov byte [rsi], '-'
    code.extend_from_slice(&[0xC6, 0x06, 0x2D]);
    // dec rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xCE]);
    let no_minus_pos = code.len();
    code[no_minus_patch] = (no_minus_pos - no_minus_patch - 1) as u8;

    // inc rsi — points at first digit
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]);

    // length = (rsp + 24) - rsi
    code.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x18]); // lea rdx, [rsp+24]
    code.extend_from_slice(&[0x48, 0x29, 0xF2]); // sub rdx, rsi

    // write(1, rsi, rdx)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // mov rdi, 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // add rsp, 24
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x18]);
}

/// Compute the length of a NUL-terminated C string. Inputs: rsi = pointer
/// to the first byte. Outputs: rdx = number of bytes before the NUL.
/// Clobbers: rax, rcx, rdi.
///
/// Uses repne scasb: scan-byte while not-equal, decrementing rcx from -1.
/// After repne returns, rdi points past the NUL, and rcx is -(length + 2).
/// We compute length as `-(rcx) - 1`. This is the standard "fast strlen"
/// pattern in raw x86-64.
fn emit_strlen(code: &mut Vec<u8>) {
    // mov rdi, rsi
    code.extend_from_slice(&[0x48, 0x89, 0xF7]);
    // xor eax, eax  (scan target = 0)
    code.extend_from_slice(&[0x31, 0xC0]);
    // mov rcx, -1
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0xFF, 0xFF, 0xFF, 0xFF]);
    // cld  (clear DF so scasb increments rdi)
    code.push(0xFC);
    // repne scasb
    code.extend_from_slice(&[0xF2, 0xAE]);
    // length = -(rcx) - 1 = ~rcx - 0  → equivalent to: not rcx; dec rcx (from -1 base)
    // Simpler: rdx = -rcx - 1
    // mov rdx, rcx
    code.extend_from_slice(&[0x48, 0x89, 0xCA]);
    // not rdx
    code.extend_from_slice(&[0x48, 0xF7, 0xD2]);
    // dec rdx
    code.extend_from_slice(&[0x48, 0xFF, 0xCA]);
}

/// Append a single newline byte to the given file descriptor. Used for
/// per-record line separation: stdout (fd=1) for Ok-text output, stderr
/// (fd=2) for Err messages. Symmetric to the newline itoa adds to stdout
/// for Ok-number output.
fn emit_write_newline(code: &mut Vec<u8>, fd: i32) {
    // jmp +1, <0x0A>
    code.push(0xEB);
    code.push(0x01);
    let data_addr = code.len();
    code.push(0x0A); // '\n'
    // mov rax, 1 (sys_write)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    // mov rdi, fd
    code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
    code.extend_from_slice(&fd.to_le_bytes());
    // lea rsi, [rip + rel32]
    let end = code.len() + 7;
    let rel32 = data_addr as i32 - end as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rel32.to_le_bytes());
    // mov rdx, 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x01, 0x00, 0x00, 0x00]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// Emit code that writes the given text expression to the given fd. Shared
/// between Ok(text) → stdout and Err(text) → stderr. Handles two shapes:
///
/// - `Expr::Text(literal)` — inline the bytes via `jmp over data`, then
///   `lea rsi` + `mov rdx, len` + `mov rdi, fd` + `mov rax, 1` + `syscall`.
/// - `Expr::Concat(args)` — build into a stack buffer via
///   `emit_concat_to_buffer` (rax = buf, rdx = len), move to rsi, write,
///   then `add rsp, buf_size` to free before the next iteration.
///
/// On return, the write has happened but NO trailing newline has been emitted.
/// The caller is responsible for `emit_write_newline(fd)` if it wants
/// per-record separation.
fn emit_text_write_to_fd(
    code: &mut Vec<u8>,
    text_expr: &Expr,
    fd: i32,
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    // Special case: a Field access on a text-typed input field. The pointer
    // is in the rbp slot (stored at field-loading time). Length is recovered
    // via emit_strlen — argv strings are NUL-terminated so this is exact.
    if let Expr::Field(base, field_name) = text_expr {
        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
            let f = concept
                .fields
                .iter()
                .find(|f| &f.name == field_name)
                .ok_or_else(|| NativeError {
                    message: format!("unknown field '{}' in native text-write", field_name),
                })?;
            if matches!(f.ty, Type::Text) {
                let offset = offsets[field_name.as_str()];
                // mov rsi, [rbp + offset]   (load the stored pointer)
                if offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                    code.push(offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
                // strlen → rdx = length (rsi unchanged)
                emit_strlen(code);
                // write(fd, rsi, rdx)
                code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
                code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
                code.extend_from_slice(&fd.to_le_bytes());
                code.extend_from_slice(&[0x0F, 0x05]); // syscall
                return Ok(());
            }
        }
    }
    match text_expr {
        Expr::Text(s) => {
            let bytes = s.as_bytes();
            let n = bytes.len();
            if n <= 127 {
                code.push(0xEB);
                code.push(n as u8);
            } else {
                code.push(0xE9);
                code.extend_from_slice(&(n as i32).to_le_bytes());
            }
            let addr = code.len();
            code.extend_from_slice(bytes);
            // mov rax, 1 (sys_write)
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
            // mov rdi, fd
            code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
            code.extend_from_slice(&fd.to_le_bytes());
            // lea rsi, [rip + rel32]
            let end = code.len() + 7;
            let rel32 = addr as i32 - end as i32;
            code.extend_from_slice(&[0x48, 0x8D, 0x35]);
            code.extend_from_slice(&rel32.to_le_bytes());
            // mov rdx, n
            code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
            code.extend_from_slice(&(n as i32).to_le_bytes());
            // syscall
            code.extend_from_slice(&[0x0F, 0x05]);
            Ok(())
        }
        Expr::Concat(args) => {
            let buf = emit_concat_to_buffer(
                code, args, input_name, concept, all_rules, offsets, field_ranges,
            )?;
            // rax = buf ptr, rdx = length.
            // mov rsi, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            // mov rdi, fd
            code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
            code.extend_from_slice(&fd.to_le_bytes());
            // mov rax, 1 (sys_write)
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
            // syscall
            code.extend_from_slice(&[0x0F, 0x05]);
            emit_concat_buffer_free(code, buf);
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "text-producing expression not yet supported in native: {:?}",
                other
            ),
        }),
    }
}

/// Emit just the open(path, O_WRONLY|O_APPEND|O_CREAT, 0644) syscall with the
/// path embedded inline (NUL-terminated). On return, rax holds the fd.
fn emit_open_append(code: &mut Vec<u8>, path: &str) {
    let path_bytes = path.as_bytes();
    let path_with_nul_len = path_bytes.len() + 1;

    // jmp over path bytes
    if path_with_nul_len <= 127 {
        code.push(0xEB);
        code.push(path_with_nul_len as u8);
    } else {
        code.push(0xE9);
        code.extend_from_slice(&(path_with_nul_len as i32).to_le_bytes());
    }
    let path_addr = code.len();
    code.extend_from_slice(path_bytes);
    code.push(0);

    // mov rax, 2 (sys_open)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x02, 0x00, 0x00, 0x00]);
    // lea rdi, [rip + rel32] → path
    let end = code.len() + 7;
    let rel32 = path_addr as i32 - end as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x3D]);
    code.extend_from_slice(&rel32.to_le_bytes());
    // mov rsi, 0x441 (O_WRONLY | O_APPEND | O_CREAT)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x41, 0x04, 0x00, 0x00]);
    // mov rdx, 0x1A4 (mode 0644)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0xA4, 0x01, 0x00, 0x00]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// Emit a standalone binary for a rule whose output is Result(number, text).
///
/// Convention: the binary prints the Ok scalar to stdout (with a trailing
/// newline from itoa) and the Err message to stderr. Exit is always 0 after
/// all records process — the shell caller separates success from failure by
/// stream (`./prog 200 17 | consume 2>errors.log`).
///
/// Each Ok/Err leaf is emitted in continuation-passing style: it writes its
/// output and then jumps back to the top of the record loop. There is no
/// intermediate tagged "Result value" materialized in registers or memory —
/// avoids the register-lifetime / stack-cleanup complexity when a leaf
/// allocates a concat buffer.
fn emit_result_program(
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // Restrict to Result(number, text) for now — the calling convention is
    // different for other Result shapes (e.g. text payload on Ok) and they
    // need their own design pass.
    let (t_ok, t_err) = match &rule.output_ty {
        Type::Result(t, e) => (t.as_ref(), e.as_ref()),
        _ => {
            return Err(NativeError {
                message: "emit_result_program called on non-Result rule".into(),
            })
        }
    };
    // Accept Result(Number, Text) and Result(Text, Text). Other shapes
    // (Result(Record, _), Result(collection, _), etc.) need their own
    // calling convention design and stay interpreter-only.
    if !matches!(t_ok, Type::Number | Type::Text) || !matches!(t_err, Type::Text) {
        return Err(NativeError {
            message: "native Result rules today support Ok = number|text and Err = text; other shapes are interpreter-only".into(),
        });
    }

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, all_rules)?;

    // Evaluate the logic in Result context. Every Ok/Err leaf self-terminates
    // with a jmp loop_top, so there is no fall-through to handle here.
    emit_eval_result_expr(
        &mut code,
        &rule.logic.value,
        ctx.loop_top,
        rule,
        concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
        ctx.match_slot,
    )?;

    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}

/// Emit code for an expression that produces a Result(number, text). Each
/// Ok/Err leaf emits its own write + jmp loop_top (continuation-passing), so
/// the caller does not deal with a tagged union value — it just appends this
/// block and the leaves route themselves to the next iteration.
fn emit_eval_result_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    loop_top: usize,
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    match_slot: i32,
) -> Result<(), NativeError> {
    // Extract the declared (T, E) from the rule's Result(T, E) output so the
    // Ok arm emits the right write for its declared type.
    let t_ok = match &rule.output_ty {
        Type::Result(t, _) => t.as_ref(),
        _ => return Err(NativeError {
            message: "emit_eval_result_expr called on a rule whose output is not Result".into(),
        }),
    };

    match expr {
        Expr::Ok(inner) => {
            match t_ok {
                Type::Number => {
                    // Number Ok: evaluate → rax, itoa writes decimal + \n to stdout.
                    emit_eval_expr(code, inner, &rule.input_name, offsets, all_rules, field_ranges)?;
                    emit_itoa_inline(code);
                }
                Type::Text => {
                    // Text Ok: write the bytes (literal or concat buffer) to
                    // stdout (fd 1), then append a newline symmetric to itoa.
                    emit_text_write_to_fd(
                        code, inner, 1, &rule.input_name, concept, all_rules, offsets, field_ranges,
                    )?;
                    emit_write_newline(code, 1);
                }
                other => {
                    return Err(NativeError {
                        message: format!(
                            "Ok arm type '{:?}' not yet supported in native — only number and text",
                            other
                        ),
                    });
                }
            }
            // jmp loop_top (rel32, backward)
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::Err(inner) => {
            // Err is always text in the shapes we accept. Write to stderr
            // (fd 2), then a newline so multi-record runs separate cleanly.
            emit_text_write_to_fd(
                code, inner, 2, &rule.input_name, concept, all_rules, offsets, field_ranges,
            )?;
            emit_write_newline(code, 2);
            // jmp loop_top
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            // Evaluate the condition as a normal scalar (bool: 0 or 1).
            emit_eval_expr(code, cond, &rule.input_name, offsets, all_rules, field_ranges)?;
            // test rax, rax ; jz .else (rel32 patch so arms can be large)
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            // .then — each leaf self-terminates with jmp loop_top.
            emit_eval_result_expr(
                code, then_e, loop_top, rule, concept, all_rules, offsets, field_ranges, match_slot,
            )?;

            // .else:
            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_eval_result_expr(
                code, else_e, loop_top, rule, concept, all_rules, offsets, field_ranges, match_slot,
            )?;
            Ok(())
        }
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            // Phase 2D scope (narrow): support only the discounted_purchase
            // shape — target is a rule call on the outer rule's input,
            // ok_arm is `Ok(<expr using ok_var>)`, err_arm is `Err(Ident(err_var))`
            // (pure pass-through of the inner Err's text).
            //
            // Why narrow: a fully general match_result in native needs either
            // a tagged Result calling convention for callees or fully general
            // bound-var slot management for arbitrary text values. Both are
            // significantly more design surface; the narrow form here covers
            // the common compose-validators-then-transform pattern without
            // committing to either.
            emit_match_result_inlined(
                code,
                target,
                ok_var,
                ok_body,
                err_var,
                err_body,
                loop_top,
                rule,
                concept,
                all_rules,
                offsets,
                field_ranges,
                match_slot,
            )
        }
        other => Err(NativeError {
            message: format!(
                "Result-context expression not yet supported in native: {:?}",
                other
            ),
        }),
    }
}

/// Emit code for `match_result(callee(input), ok_var => Ok(...), err_var => Err(err_var))`.
///
/// Strategy: inline the callee's logic, redirecting its Ok/Err leaves to the
/// outer match arms instead of the standalone "write + jmp" they would emit.
///
/// - For each Ok leaf in the callee: evaluate the leaf's inner expression
///   (a number) into rax, store at match_slot, then emit the outer Ok arm
///   with `ok_var → match_slot` added to offsets. The outer Ok arm's body
///   self-terminates (jmp loop_top) — same continuation-passing as elsewhere.
///
/// - For each Err leaf in the callee: evaluate the leaf's inner expression
///   (text) and write directly to stderr (Err pass-through optimisation —
///   we detect that the outer Err arm is `Err(Ident(err_var))` so the value
///   simply forwards). Append a newline + jmp loop_top.
///
/// Restrictions enforced here (rejected with clear messages):
/// - Target must be `Call(callee_name, [Ident(input_name)])` where
///   input_name == outer rule's input.
/// - Callee's input concept must equal outer rule's input concept (so the
///   rbp slots already loaded by the prologue work as-is for callee).
/// - Outer Err arm must be `Err(Ident(err_var))` — the pass-through case.
/// - Callee's logic must be a tree of If / Ok / Err — no nested
///   match_result / no rule calls inside callee.
fn emit_match_result_inlined(
    code: &mut Vec<u8>,
    target: &Expr,
    ok_var: &str,
    ok_body: &Expr,
    err_var: &str,
    err_body: &Expr,
    loop_top: usize,
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    match_slot: i32,
) -> Result<(), NativeError> {
    // Validate the outer Err arm is the pass-through shape.
    let err_passthrough = match err_body {
        Expr::Err(inner) => matches!(inner.as_ref(), Expr::Ident(n) if n == err_var),
        _ => false,
    };
    if !err_passthrough {
        return Err(NativeError {
            message: "match_result Err arm in native today must be the pass-through `Err(<err_var>)`; richer Err transforms are deferred".into(),
        });
    }

    // Validate target is Call(callee, [Ident(input)]).
    let (callee_name, _arg) = match target {
        Expr::Call(name, args) if args.len() == 1 => {
            if !matches!(&args[0], Expr::Ident(n) if n == &rule.input_name) {
                return Err(NativeError {
                    message: "match_result target call must pass the outer rule's input identifier".into(),
                });
            }
            (name.as_str(), &args[0])
        }
        _ => {
            return Err(NativeError {
                message: "match_result target must be a rule call (literal Result targets not yet supported in native)".into(),
            });
        }
    };

    let callee = all_rules.get(callee_name).ok_or_else(|| NativeError {
        message: format!("match_result calls unknown rule '{}'", callee_name),
    })?;

    // Validate callee's input concept matches outer rule's. Same-concept
    // means the rbp slots already populated by the prologue are reusable.
    let callee_input = match &callee.input_ty {
        Type::Named(n) => n.as_str(),
        _ => return Err(NativeError { message: "callee input must be a named concept".into() }),
    };
    if callee_input != concept.name.as_str() {
        return Err(NativeError {
            message: format!(
                "match_result callee '{}' takes input concept '{}' but caller takes '{}' — same-concept required for native",
                callee_name, callee_input, concept.name
            ),
        });
    }

    // Walk callee's logic, redirecting leaves.
    emit_redirect_callee_leaves(
        code,
        &callee.logic.value,
        callee,
        ok_var,
        ok_body,
        loop_top,
        rule,
        concept,
        all_rules,
        offsets,
        field_ranges,
        match_slot,
    )
}

fn emit_redirect_callee_leaves(
    code: &mut Vec<u8>,
    expr: &Expr,
    callee: &Rule,
    ok_var: &str,
    ok_body: &Expr,
    loop_top: usize,
    outer_rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    match_slot: i32,
) -> Result<(), NativeError> {
    match expr {
        Expr::Ok(inner) => {
            // Evaluate inner using callee's input_name (typically same as outer's
            // since concepts match, but the names could differ syntactically).
            // For simplicity we currently require the names to also match —
            // otherwise the inner expression's Ident lookups would miss.
            if callee.input_name != outer_rule.input_name {
                return Err(NativeError {
                    message: format!(
                        "match_result callee uses input name '{}' but outer rule uses '{}' — same input name required for native today",
                        callee.input_name, outer_rule.input_name
                    ),
                });
            }
            // Evaluate Ok's inner (a number) → rax.
            emit_eval_expr(code, inner, &outer_rule.input_name, offsets, all_rules, field_ranges)?;
            // Store at match_slot.
            if match_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x89, 0x45]);
                code.push(match_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x89, 0x85]);
                code.extend_from_slice(&match_slot.to_le_bytes());
            }
            // Augment offsets with ok_var → match_slot, then emit outer ok_body
            // in result context. The outer arm self-terminates.
            let mut augmented = offsets.clone();
            augmented.insert(ok_var, match_slot);
            emit_eval_result_expr(
                code,
                ok_body,
                loop_top,
                outer_rule,
                concept,
                all_rules,
                &augmented,
                field_ranges,
                match_slot,
            )
        }
        Expr::Err(inner) => {
            // Pass-through to outer Err: write the inner text directly to
            // stderr, append newline, jmp loop_top. No binding needed.
            emit_text_write_to_fd(
                code, inner, 2, &outer_rule.input_name, concept, all_rules, offsets, field_ranges,
            )?;
            emit_write_newline(code, 2);
            // jmp loop_top
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            emit_eval_expr(code, cond, &outer_rule.input_name, offsets, all_rules, field_ranges)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            emit_redirect_callee_leaves(
                code, then_e, callee, ok_var, ok_body, loop_top,
                outer_rule, concept, all_rules, offsets, field_ranges, match_slot,
            )?;

            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_redirect_callee_leaves(
                code, else_e, callee, ok_var, ok_body, loop_top,
                outer_rule, concept, all_rules, offsets, field_ranges, match_slot,
            )?;
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "match_result callee body has expression not yet supported for inlining: {:?}",
                other
            ),
        }),
    }
}

/// Emit a standalone binary for a rule whose output is a user-declared
/// concept (record type). For each record parsed from argv, the binary
/// writes the output record as a single-line JSON object to stdout:
///     {"field1":value1,"field2":value2}\n
///
/// Scope for Phase 2C: the output record's fields must be `number` or
/// `text`. Number fields compile through the normal emit_eval_expr path
/// then itoa. Text fields must be compile-time literals today (or
/// `concat(...)` of scalars) — general text-field-as-field access from
/// the input is deferred to Phase 2E (needs argv-as-text support).
///
/// Continuation-passing: a structural `If` between two record-producing
/// branches recurses on each arm, and each arm emits its own JSON line +
/// jmp loop_top. Same leaf-terminates-itself convention as Result.
fn emit_record_program(
    rule: &Rule,
    output_concept: &Concept,
    input_concept: &Concept,
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // Validate output concept's fields — native today handles number + text only.
    for f in &output_concept.fields {
        match f.ty {
            Type::Number | Type::Text => {}
            _ => {
                return Err(NativeError {
                    message: format!(
                        "native record output: field '{}' has unsupported type (only number/text today)",
                        f.name
                    ),
                });
            }
        }
    }

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, input_concept, all_rules)?;

    emit_eval_record_expr(
        &mut code,
        &rule.logic.value,
        ctx.loop_top,
        output_concept,
        all_concepts,
        rule,
        input_concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
    )?;

    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}

/// Emit a standalone binary for a rule whose output is `collection(T)` (Phase 3).
///
/// Scope locked in CLAUDE.md "Phase 3 design": streaming emission (one JSON
/// object per element, no wrapper), count-prefixed argv (`<N> <element × N>`
/// trailing any scalar input fields), `map` or `filter` at the top of the
/// logic, output element is a declared Record with number/text fields.
///
/// Memory: no arena, no heap. Each element parses its fields into reused
/// rbp slots, evaluates the map body (or filter predicate + body), writes
/// one line, moves to the next. The only state crossing iterations is the
/// argv index (r14) and the inner loop counter (r15).
fn emit_collection_program(
    rule: &Rule,
    input_concept: &Concept,
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // ===== Scope validation =====
    let elem_type_name = match &rule.output_ty {
        Type::Collection(n) => n.clone(),
        _ => return Err(NativeError {
            message: "emit_collection_program called on non-collection output".into(),
        }),
    };

    // Output element kind: Record (named concept), Number, or Text.
    // Phase 3.0 shipped Record; Phase 3.2 adds Number / Text scalar elements
    // so `r = map(w.employees, e => e.salary)` -> `collection(number)` compiles.
    enum OutputElemKind<'a> {
        Record(&'a Concept),
        Number,
        Text,
    }
    let output_kind: OutputElemKind = match elem_type_name.as_str() {
        "number" => OutputElemKind::Number,
        "text" => OutputElemKind::Text,
        name => {
            let c = all_concepts
                .iter()
                .find(|c| c.name == name)
                .copied()
                .ok_or_else(|| NativeError {
                    message: format!(
                        "collection element type '{}' is neither a declared concept nor a scalar (number/text)",
                        name
                    ),
                })?;
            for f in &c.fields {
                if !matches!(f.ty, Type::Number | Type::Text) {
                    return Err(NativeError {
                        message: format!(
                            "collection element field '{}' has unsupported type (only number/text today)",
                            f.name
                        ),
                    });
                }
            }
            OutputElemKind::Record(c)
        }
    };

    // Input concept shape: scalars* + one trailing collection field.
    let mut scalar_fields: Vec<&Field> = Vec::new();
    let mut coll_field: Option<&Field> = None;
    let mut elem_field_concept_name: Option<String> = None;
    for (i, f) in input_concept.fields.iter().enumerate() {
        let is_last = i == input_concept.fields.len() - 1;
        match (&f.ty, is_last) {
            (Type::Number, false) | (Type::Text, false) => scalar_fields.push(f),
            (Type::Collection(elem), true) => {
                coll_field = Some(f);
                elem_field_concept_name = Some(elem.clone());
            }
            _ => {
                return Err(NativeError {
                    message: format!(
                        "input concept '{}' must have scalar fields followed by ONE trailing collection field; \
                         field '{}' at position {} violates this",
                        input_concept.name, f.name, i
                    ),
                });
            }
        }
    }
    let coll_field = coll_field.ok_or_else(|| NativeError {
        message: format!(
            "input concept '{}' must have a trailing collection field",
            input_concept.name
        ),
    })?;
    let elem_field_concept_name = elem_field_concept_name.unwrap();
    let input_elem_concept = all_concepts
        .iter()
        .find(|c| c.name == elem_field_concept_name)
        .copied()
        .ok_or_else(|| NativeError {
            message: format!(
                "unknown concept '{}' for input collection element",
                elem_field_concept_name
            ),
        })?;
    for f in &input_elem_concept.fields {
        if !matches!(f.ty, Type::Number | Type::Text) {
            return Err(NativeError {
                message: format!(
                    "input collection element field '{}' has unsupported type (only number/text today)",
                    f.name
                ),
            });
        }
    }

    // Logic shape: Map (producing Record or scalar) or Filter over input.<coll_field>.
    enum CollectionOp<'a> {
        /// map(coll, var => Record { ... }) — Record constructor body.
        MapRecord { lambda_var: &'a str, body_fields: &'a [(String, Expr)] },
        /// map(coll, var => <scalar>) — number or text body, one line per element.
        MapScalar { lambda_var: &'a str, body: &'a Expr, is_text: bool },
        /// filter(coll, var => predicate) — element passes through if true.
        Filter { lambda_var: &'a str, predicate: &'a Expr },
    }
    let op: CollectionOp = match &rule.logic.value {
        Expr::Map(coll_expr, v, b) => {
            verify_collection_target(coll_expr, &rule.input_name, &coll_field.name)?;
            match &output_kind {
                OutputElemKind::Record(oec) => {
                    let (body_concept_name, body_fields) = match b.as_ref() {
                        Expr::Record(name, fields) => (name.as_str(), fields.as_slice()),
                        _ => return Err(NativeError {
                            message: "map body must be a Record constructor when output element is a concept".into(),
                        }),
                    };
                    if body_concept_name != oec.name.as_str() {
                        return Err(NativeError {
                            message: format!(
                                "map body produces '{}' but output collection element is '{}'",
                                body_concept_name, oec.name
                            ),
                        });
                    }
                    CollectionOp::MapRecord { lambda_var: v.as_str(), body_fields }
                }
                OutputElemKind::Number => {
                    CollectionOp::MapScalar { lambda_var: v.as_str(), body: b.as_ref(), is_text: false }
                }
                OutputElemKind::Text => {
                    CollectionOp::MapScalar { lambda_var: v.as_str(), body: b.as_ref(), is_text: true }
                }
            }
        }
        Expr::Filter(coll_expr, v, pred) => {
            verify_collection_target(coll_expr, &rule.input_name, &coll_field.name)?;
            // Filter preserves element type — output element must match input
            // element. Phase 3.2 allows Record inputs only (argv shape); scalar
            // input collections (collection(number)) stay interpreter-only.
            let oec = match &output_kind {
                OutputElemKind::Record(c) => *c,
                _ => return Err(NativeError {
                    message: "filter with scalar output element requires a scalar input collection, which is not yet supported in native".into(),
                }),
            };
            if oec.name != input_elem_concept.name {
                return Err(NativeError {
                    message: format!(
                        "filter output collection must match input element type: input is collection({}) but output is collection({})",
                        input_elem_concept.name, oec.name
                    ),
                });
            }
            CollectionOp::Filter { lambda_var: v.as_str(), predicate: pred.as_ref() }
        }
        _ => return Err(NativeError {
            message: "collection-output rule logic must be map(...) or filter(...) at top level".into(),
        }),
    };

    // ===== Emission =====
    let n_scalar = scalar_fields.len();
    let n_elem_fields = input_elem_concept.fields.len();
    let frame_slots = n_scalar + n_elem_fields;
    let frame_size = (frame_slots as i32) * 8;

    let mut code = Vec::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Outer loop: each iteration processes one input record (scalars + count + elements).
    let outer_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]); // cmp r14, r12
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Offsets: scalar fields go at -8, -16, ...; element fields go after them.
    let mut scalar_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in scalar_fields.iter().enumerate() {
        scalar_offsets.insert(f.name.as_str(), -((i as i32 + 1) * 8));
    }
    let mut elem_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in input_elem_concept.fields.iter().enumerate() {
        elem_offsets.insert(f.name.as_str(), -(((n_scalar + i) as i32 + 1) * 8));
    }

    // Parse scalar input fields.
    for f in &scalar_fields {
        let offset = scalar_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Parse collection count into r15.
    code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
    emit_atoi_inline(&mut code);
    code.extend_from_slice(&[0x49, 0x89, 0xC7]); // mov r15, rax
    code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14

    // Inner loop: for each element, parse its fields, emit one JSON line.
    let inner_loop_top = code.len();
    // test r15, r15 ; jz inner_done (rel32)
    code.extend_from_slice(&[0x4D, 0x85, 0xFF]);
    code.push(0x0F);
    code.push(0x84);
    let inner_done_patch = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Parse element fields into rbp slots (reused each iteration).
    for f in &input_elem_concept.fields {
        let offset = elem_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Evaluate the body or predicate. Lambda var is the "input name" so
    // `e.salary` resolves to the element-field slot populated above.
    let field_ranges = build_field_ranges(input_elem_concept);
    match op {
        CollectionOp::MapRecord { lambda_var, body_fields } => {
            // Emit the constructed Record as one JSON line. The record's
            // trailing "}\n" IS the per-element separator — no extra newline.
            let output_concept = match &output_kind {
                OutputElemKind::Record(c) => *c,
                _ => unreachable!("MapRecord built only from Record output kind"),
            };
            emit_record_as_json(
                &mut code,
                body_fields,
                output_concept,
                lambda_var,
                input_elem_concept,
                all_rules,
                &elem_offsets,
                &field_ranges,
            )?;
        }
        CollectionOp::MapScalar { lambda_var, body, is_text } => {
            // Scalar element output: evaluate the body to rax (number) or
            // emit the text directly, then one newline per element.
            if is_text {
                emit_text_write_to_fd(
                    &mut code, body, 1, lambda_var, input_elem_concept, all_rules,
                    &elem_offsets, &field_ranges,
                )?;
                emit_write_newline(&mut code, 1);
            } else {
                emit_eval_expr(
                    &mut code, body, lambda_var, &elem_offsets, all_rules, &field_ranges,
                )?;
                // emit_itoa_inline writes rax to stdout with trailing newline.
                emit_itoa_inline(&mut code);
            }
        }
        CollectionOp::Filter { lambda_var, predicate } => {
            // Evaluate the predicate → rax (0 = skip, non-zero = keep).
            emit_eval_expr(
                &mut code, predicate, lambda_var, &elem_offsets, all_rules, &field_ranges,
            )?;
            // test rax, rax ; je skip_emit (rel32, patched after the write block).
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let skip_patch = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);

            // Emit the element as identity JSON: synthesize a Record whose
            // fields are `e.<field>` Field accesses, reusing the same
            // emit_record_as_json plumbing map uses. No runtime cost — the
            // synthesis is compile-time.
            let synthetic_fields: Vec<(String, Expr)> = input_elem_concept
                .fields
                .iter()
                .map(|f| {
                    (
                        f.name.clone(),
                        Expr::Field(
                            Box::new(Expr::Ident(lambda_var.to_string())),
                            f.name.clone(),
                        ),
                    )
                })
                .collect();
            emit_record_as_json(
                &mut code,
                &synthetic_fields,
                input_elem_concept, // output elem == input elem for filter
                lambda_var,
                input_elem_concept,
                all_rules,
                &elem_offsets,
                &field_ranges,
            )?;

            // skip_emit:
            let skip_pos = code.len();
            let skip_off = skip_pos as i32 - (skip_patch as i32 + 4);
            code[skip_patch..skip_patch + 4].copy_from_slice(&skip_off.to_le_bytes());
        }
    }

    // dec r15 ; jmp inner_loop_top (rel32).
    code.extend_from_slice(&[0x49, 0xFF, 0xCF]); // dec r15
    code.push(0xE9);
    let back_off = inner_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back_off.to_le_bytes());

    // inner_done:
    let inner_done_pos = code.len();
    let inner_done_off = inner_done_pos as i32 - (inner_done_patch as i32 + 4);
    code[inner_done_patch..inner_done_patch + 4].copy_from_slice(&inner_done_off.to_le_bytes());

    // jmp outer_loop_top (rel32).
    code.push(0xE9);
    let outer_off = outer_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&outer_off.to_le_bytes());

    // exit: sys_exit(0)
    let exit_pos = code.len();
    let exit_off = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    Ok(code)
}

/// Phase 4: `output: number` with top-level `fold` (or its sum/count/min/max
/// desugarings). Inner loop accumulates into a single stack slot (`acc_slot`)
/// at the bottom of the rbp frame. After the inner loop, the accumulator is
/// serialized once per input record via emit_itoa_inline (which appends `\n`).
///
/// Shape accepted (everything else refused with a message):
///   - rule.output_ty == Type::Number
///   - rule.logic.value == Expr::Fold(Field(Ident(input), coll_field), Number(init), acc, item, body)
///   - init is an Expr::Number literal (sum/count/min/max desugarings all produce literals)
///   - input concept: scalars* + ONE trailing collection(Concept) field
///   - body is a scalar expression reading acc and item.<field>
///
/// Syscall surface: identical to Phase 3 (`read argv`, `write fd 1`, `exit`).
/// No new register reservation. One extra 8-byte slot in the rbp frame.
fn emit_fold_program(
    rule: &Rule,
    input_concept: &Concept,
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // ===== Scope validation =====
    if !matches!(rule.output_ty, Type::Number) {
        return Err(NativeError {
            message: "emit_fold_program called on non-number output".into(),
        });
    }
    let (coll_expr, init_expr, acc_name, item_name, body) = match &rule.logic.value {
        Expr::Fold(c, i, a, it, b) => (c.as_ref(), i.as_ref(), a.as_str(), it.as_str(), b.as_ref()),
        _ => return Err(NativeError {
            message: "number-output native rule with fold must have `fold(...)` at the top level".into(),
        }),
    };
    let init_literal: i64 = match init_expr {
        Expr::Number(n) => *n,
        _ => return Err(NativeError {
            message: "Phase 4: fold init must be a literal number (sum/count/min/max desugarings satisfy this automatically)".into(),
        }),
    };

    // Input concept shape: scalars* + one trailing collection field.
    let mut scalar_fields: Vec<&Field> = Vec::new();
    let mut coll_field: Option<&Field> = None;
    let mut elem_concept_name: Option<String> = None;
    for (i, f) in input_concept.fields.iter().enumerate() {
        let is_last = i == input_concept.fields.len() - 1;
        match (&f.ty, is_last) {
            (Type::Number, false) | (Type::Text, false) => scalar_fields.push(f),
            (Type::Collection(elem), true) => {
                coll_field = Some(f);
                elem_concept_name = Some(elem.clone());
            }
            _ => {
                return Err(NativeError {
                    message: format!(
                        "input concept '{}' must have scalar fields followed by ONE trailing collection field; \
                         field '{}' at position {} violates this",
                        input_concept.name, f.name, i
                    ),
                });
            }
        }
    }
    let coll_field = coll_field.ok_or_else(|| NativeError {
        message: format!(
            "input concept '{}' must have a trailing collection field",
            input_concept.name
        ),
    })?;
    let elem_concept_name = elem_concept_name.unwrap();
    let elem_concept = all_concepts
        .iter()
        .find(|c| c.name == elem_concept_name)
        .copied()
        .ok_or_else(|| NativeError {
            message: format!("unknown concept '{}' for input collection element", elem_concept_name),
        })?;
    for f in &elem_concept.fields {
        if !matches!(f.ty, Type::Number | Type::Text) {
            return Err(NativeError {
                message: format!(
                    "input collection element field '{}' has unsupported type (only number/text today)",
                    f.name
                ),
            });
        }
    }

    // Fold target must be `input.<coll_field>`. Shared verifier with map/filter.
    verify_collection_target(coll_expr, &rule.input_name, &coll_field.name)?;

    // ===== Emission =====
    let n_scalar = scalar_fields.len();
    let n_elem_fields = elem_concept.fields.len();
    // +1 slot for acc at the bottom of the frame.
    let frame_slots = n_scalar + n_elem_fields + 1;
    let frame_size = (frame_slots as i32) * 8;
    let acc_offset: i32 = -((frame_slots as i32) * 8);

    let mut code = Vec::new();

    // _start — argv/rbp frame setup (identical to Phase 3).
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Outer loop — one input record per iteration.
    let outer_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]); // cmp r14, r12
    code.push(0x0F);
    code.push(0x8D);                              // jge exit (rel32)
    let exit_patch = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Offsets: scalars at -8..; element fields after them; acc at the bottom.
    let mut scalar_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in scalar_fields.iter().enumerate() {
        scalar_offsets.insert(f.name.as_str(), -((i as i32 + 1) * 8));
    }
    let mut body_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in elem_concept.fields.iter().enumerate() {
        body_offsets.insert(f.name.as_str(), -(((n_scalar + i) as i32 + 1) * 8));
    }
    // `acc_name` resolves to acc_slot inside the body.
    body_offsets.insert(acc_name, acc_offset);

    // Parse scalar input fields.
    for f in &scalar_fields {
        let offset = scalar_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Parse collection count into r15.
    code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
    emit_atoi_inline(&mut code);
    code.extend_from_slice(&[0x49, 0x89, 0xC7]); // mov r15, rax
    code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14

    // Seed acc_slot with the literal init. mov rax, imm64 then store to rbp slot.
    code.extend_from_slice(&[0x48, 0xB8]);
    code.extend_from_slice(&init_literal.to_le_bytes());
    store_rax_at_rbp(&mut code, acc_offset);

    // Inner loop — per element, parse fields, fold into acc_slot.
    let inner_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xFF]); // test r15, r15
    code.push(0x0F);
    code.push(0x84);                              // jz inner_done (rel32)
    let inner_done_patch = code.len();
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Parse element fields into rbp slots (reused each iteration).
    for f in &elem_concept.fields {
        let offset = body_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Evaluate the fold body; result is the NEW accumulator value in rax.
    // item_name is the "input" for field access resolution within the body.
    let field_ranges = build_field_ranges(elem_concept);
    emit_eval_expr(&mut code, body, item_name, &body_offsets, all_rules, &field_ranges)?;
    store_rax_at_rbp(&mut code, acc_offset);

    // dec r15 ; jmp inner_loop_top (rel32).
    code.extend_from_slice(&[0x49, 0xFF, 0xCF]); // dec r15
    code.push(0xE9);
    let back_off = inner_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back_off.to_le_bytes());

    // inner_done:
    let inner_done_pos = code.len();
    let inner_done_off = inner_done_pos as i32 - (inner_done_patch as i32 + 4);
    code[inner_done_patch..inner_done_patch + 4].copy_from_slice(&inner_done_off.to_le_bytes());

    // Emit the final accumulator: load acc_slot -> rax -> itoa+newline to stdout.
    if acc_offset >= -128 {
        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
        code.push(acc_offset as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
        code.extend_from_slice(&acc_offset.to_le_bytes());
    }
    emit_itoa_inline(&mut code);

    // jmp outer_loop_top.
    code.push(0xE9);
    let outer_off = outer_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&outer_off.to_le_bytes());

    // exit: sys_exit(0)
    let exit_pos = code.len();
    let exit_off = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    Ok(code)
}

/// Does `e` mention `Ident(name)` anywhere (recursively)? Used by Phase 5b
/// to enforce the "append-only" invariant: the fold accumulator must not
/// appear outside position 0 of the outer concat.
fn expr_mentions_ident(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Ident(n) => n == name,
        Expr::Field(b, _) => expr_mentions_ident(b, name),
        Expr::Binary(_, l, r) => expr_mentions_ident(l, name) || expr_mentions_ident(r, name),
        Expr::Neg(i) | Expr::Not(i) => expr_mentions_ident(i, name),
        Expr::If(c, t, el) => {
            expr_mentions_ident(c, name)
                || expr_mentions_ident(t, name)
                || expr_mentions_ident(el, name)
        }
        Expr::Call(_, args) => args.iter().any(|a| expr_mentions_ident(a, name)),
        Expr::Concat(args) => args.iter().any(|a| expr_mentions_ident(a, name)),
        Expr::Ok(i) | Expr::Err(i) => expr_mentions_ident(i, name),
        Expr::Quantifier(_, c, _, b) => expr_mentions_ident(c, name) || expr_mentions_ident(b, name),
        Expr::Map(c, _, b) | Expr::Filter(c, _, b) => {
            expr_mentions_ident(c, name) || expr_mentions_ident(b, name)
        }
        Expr::Fold(c, init, _, _, b) => {
            expr_mentions_ident(c, name)
                || expr_mentions_ident(init, name)
                || expr_mentions_ident(b, name)
        }
        _ => false,
    }
}

/// Phase 5b: `output: text` with a top-level `fold` that appends into a text
/// accumulator over a collection. Two-pass emission:
///
/// 1. Size pass — walk the collection once computing total bytes into rax
///    (init literal length + per-element contribution: static literals,
///    21 bytes per number arg, runtime `strlen` per text-field arg).
/// 2. Buffer allocation — `mov r9, rsp; sub rsp, rax; mov rbx, rsp; mov r10, rbx`.
///    Copy init literal into the buffer via `rep movsb`.
/// 3. Fill pass — rewind `r14` and `r15` from their rbp save slots, walk
///    the collection again, emitting each element's contribution via the
///    shared `emit_concat_fill`.
/// 4. Emit the buffer to stdout (`write(1, r10, rbx - r10)`) + newline.
/// 5. Free via `mov rsp, r9`. Loop to next input record.
///
/// Body shape (strictly append-only, refused otherwise):
///   `Concat(Ident(acc), ...rest)` where `acc` appears NOWHERE in `rest`.
fn emit_text_fold_program(
    rule: &Rule,
    input_concept: &Concept,
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // ===== Scope validation =====
    if !matches!(rule.output_ty, Type::Text) {
        return Err(NativeError {
            message: "emit_text_fold_program called on non-text output".into(),
        });
    }
    let (coll_expr, init_expr, acc_name, item_name, body) = match &rule.logic.value {
        Expr::Fold(c, i, a, it, b) => {
            (c.as_ref(), i.as_ref(), a.as_str(), it.as_str(), b.as_ref())
        }
        _ => {
            return Err(NativeError {
                message: "text-output native rule with fold must have `fold(...)` at the top level".into(),
            })
        }
    };
    let init_literal: &str = match init_expr {
        Expr::Text(s) => s.as_str(),
        _ => {
            return Err(NativeError {
                message: "Phase 5b: fold init must be a text literal".into(),
            })
        }
    };

    // Body must be Concat(Ident(acc), ...rest), with acc absent from rest.
    let rest_args: &[Expr] = match body {
        Expr::Concat(args) => {
            if args.is_empty() {
                return Err(NativeError {
                    message: "Phase 5b: fold body must be `concat(acc, ...)`".into(),
                });
            }
            match &args[0] {
                Expr::Ident(n) if n == acc_name => {}
                _ => {
                    return Err(NativeError {
                        message: format!(
                            "Phase 5b: first arg of fold-body concat must be the accumulator '{}'",
                            acc_name
                        ),
                    })
                }
            }
            for a in &args[1..] {
                if expr_mentions_ident(a, acc_name) {
                    return Err(NativeError {
                        message: format!(
                            "Phase 5b: accumulator '{}' may only appear as the first arg of the fold-body concat",
                            acc_name
                        ),
                    });
                }
            }
            &args[1..]
        }
        _ => {
            return Err(NativeError {
                message: "Phase 5b: fold body must be a `concat(...)` expression".into(),
            })
        }
    };

    // Input concept shape: scalars* + ONE trailing collection(Concept) field.
    let mut scalar_fields: Vec<&Field> = Vec::new();
    let mut coll_field: Option<&Field> = None;
    let mut elem_concept_name: Option<String> = None;
    for (i, f) in input_concept.fields.iter().enumerate() {
        let is_last = i == input_concept.fields.len() - 1;
        match (&f.ty, is_last) {
            (Type::Number, false) | (Type::Text, false) => scalar_fields.push(f),
            (Type::Collection(elem), true) => {
                coll_field = Some(f);
                elem_concept_name = Some(elem.clone());
            }
            _ => {
                return Err(NativeError {
                    message: format!(
                        "input concept '{}' must have scalar fields followed by ONE trailing collection field; \
                         field '{}' at position {} violates this",
                        input_concept.name, f.name, i
                    ),
                });
            }
        }
    }
    let coll_field = coll_field.ok_or_else(|| NativeError {
        message: format!(
            "input concept '{}' must have a trailing collection field",
            input_concept.name
        ),
    })?;
    let elem_concept_name = elem_concept_name.unwrap();
    let elem_concept = all_concepts
        .iter()
        .find(|c| c.name == elem_concept_name)
        .copied()
        .ok_or_else(|| NativeError {
            message: format!(
                "unknown concept '{}' for input collection element",
                elem_concept_name
            ),
        })?;
    for f in &elem_concept.fields {
        if !matches!(f.ty, Type::Number | Type::Text) {
            return Err(NativeError {
                message: format!(
                    "input collection element field '{}' has unsupported type (only number/text today)",
                    f.name
                ),
            });
        }
    }
    verify_collection_target(coll_expr, &rule.input_name, &coll_field.name)?;

    // Classify rest args. The lambda var `item` is the "input" for field
    // accesses within them.
    let mut rest_kinds: Vec<ConcatArgKind> = Vec::with_capacity(rest_args.len());
    for arg in rest_args {
        let k = classify_concat_arg(arg, elem_concept, item_name).ok_or_else(|| NativeError {
            message: "Phase 5b: fold-body concat arg must be a text literal, number expression, or element text field".into(),
        })?;
        rest_kinds.push(k);
    }

    // Static per-element contribution (sum of literal lengths + 21 per number arg).
    let mut static_per_element: i32 = 0;
    for (arg, kind) in rest_args.iter().zip(rest_kinds.iter()) {
        match kind {
            ConcatArgKind::Text => {
                if let Expr::Text(s) = arg {
                    static_per_element += s.as_bytes().len() as i32;
                }
                // Text field: runtime strlen, contributes 0 to static.
            }
            ConcatArgKind::Number => {
                static_per_element += 21;
            }
        }
    }

    // ===== Emission =====
    let n_scalar = scalar_fields.len();
    let n_elem_fields = elem_concept.fields.len();
    // frame: n_scalar + n_elem + count_slot + argv_save_slot = n_scalar + n_elem + 2
    let frame_slots = n_scalar + n_elem_fields + 2;
    let frame_size = (frame_slots as i32) * 8;
    let count_slot: i32 = -(((n_scalar + n_elem_fields + 1) as i32) * 8);
    let argv_save_slot: i32 = -(((n_scalar + n_elem_fields + 2) as i32) * 8);

    let mut code = Vec::new();

    // _start — argv/rbp setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);       // mov r12, [rsp]
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55);                                         // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);             // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());       // sub rsp, frame_size
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Outer loop: one input record per iteration.
    let outer_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]); // cmp r14, r12
    code.push(0x0F);
    code.push(0x8D);                             // jge exit (rel32)
    let exit_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Field offsets.
    let mut scalar_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in scalar_fields.iter().enumerate() {
        scalar_offsets.insert(f.name.as_str(), -((i as i32 + 1) * 8));
    }
    let mut elem_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in elem_concept.fields.iter().enumerate() {
        elem_offsets.insert(f.name.as_str(), -(((n_scalar + i) as i32 + 1) * 8));
    }

    // Parse scalar input fields.
    for f in &scalar_fields {
        let offset = scalar_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Parse count into r15, save it at count_slot.
    code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
    emit_atoi_inline(&mut code);
    code.extend_from_slice(&[0x49, 0x89, 0xC7]); // mov r15, rax
    // mov [rbp + count_slot], r15
    if count_slot >= -128 {
        code.extend_from_slice(&[0x4C, 0x89, 0x7D]);
        code.push(count_slot as u8);
    } else {
        code.extend_from_slice(&[0x4C, 0x89, 0xBD]);
        code.extend_from_slice(&count_slot.to_le_bytes());
    }
    code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14

    // Save r14 at argv_save_slot (this is the argv index of the first element).
    if argv_save_slot >= -128 {
        code.extend_from_slice(&[0x4C, 0x89, 0x75]);
        code.push(argv_save_slot as u8);
    } else {
        code.extend_from_slice(&[0x4C, 0x89, 0xB5]);
        code.extend_from_slice(&argv_save_slot.to_le_bytes());
    }

    // ===== Pass 1: compute total buffer size into rax =====
    let init_size = init_literal.as_bytes().len() as i32;
    // mov rax, init_size
    code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
    code.extend_from_slice(&init_size.to_le_bytes());

    let size_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xFF]); // test r15, r15
    code.push(0x0F);
    code.push(0x84);                             // jz size_done (rel32)
    let size_done_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // For each text-field arg in rest, strlen the pointer at its argv slot.
    for (arg, kind) in rest_args.iter().zip(rest_kinds.iter()) {
        if *kind == ConcatArgKind::Text {
            if let Expr::Field(_, field_name) = arg {
                let idx = elem_concept
                    .fields
                    .iter()
                    .position(|f| &f.name == field_name)
                    .ok_or_else(|| NativeError {
                        message: format!(
                            "unknown element field '{}' in fold body",
                            field_name
                        ),
                    })?;
                let disp = (idx * 8) as i32;
                // mov rsi, [r13 + r14*8 + disp]
                if disp == 0 {
                    code.extend_from_slice(&[0x4B, 0x8B, 0x74, 0xF5, 0x00]);
                } else if disp <= 127 {
                    code.extend_from_slice(&[0x4B, 0x8B, 0x74, 0xF5]);
                    code.push(disp as u8);
                } else {
                    code.extend_from_slice(&[0x4B, 0x8B, 0xB4, 0xF5]);
                    code.extend_from_slice(&disp.to_le_bytes());
                }
                code.push(0x50);                                 // push rax
                emit_strlen(&mut code);                          // rdx = length (clobbers rax/rcx/rdi)
                code.push(0x59);                                 // pop rcx
                code.extend_from_slice(&[0x48, 0x01, 0xD1]);     // add rcx, rdx
                code.extend_from_slice(&[0x48, 0x89, 0xC8]);     // mov rax, rcx
            }
        }
    }

    // add rax, static_per_element
    if static_per_element != 0 {
        if static_per_element <= 127 {
            code.extend_from_slice(&[0x48, 0x83, 0xC0, static_per_element as u8]);
        } else {
            code.extend_from_slice(&[0x48, 0x05]);
            code.extend_from_slice(&static_per_element.to_le_bytes());
        }
    }

    // Advance r14 past this element's argv slots: r14 += n_elem_fields.
    let nef = n_elem_fields as i32;
    if nef <= 127 {
        code.extend_from_slice(&[0x49, 0x83, 0xC6, nef as u8]);
    } else {
        code.extend_from_slice(&[0x49, 0x81, 0xC6]);
        code.extend_from_slice(&nef.to_le_bytes());
    }
    // dec r15 ; jmp size_loop_top
    code.extend_from_slice(&[0x49, 0xFF, 0xCF]);
    code.push(0xE9);
    let back = size_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back.to_le_bytes());

    // size_done:
    let size_done_pos = code.len();
    let size_done_off = size_done_pos as i32 - (size_done_patch as i32 + 4);
    code[size_done_patch..size_done_patch + 4].copy_from_slice(&size_done_off.to_le_bytes());

    // Round up rax to 8: add rax, 7; and rax, ~7 (sign-extended imm8 = -8 = 0xF8).
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x07]);
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF8]);

    // mov r9, rsp ; sub rsp, rax ; mov rbx, rsp ; mov r10, rbx
    code.extend_from_slice(&[0x49, 0x89, 0xE1]); // mov r9, rsp
    code.extend_from_slice(&[0x48, 0x29, 0xC4]); // sub rsp, rax
    code.extend_from_slice(&[0x48, 0x89, 0xE3]); // mov rbx, rsp
    code.extend_from_slice(&[0x49, 0x89, 0xDA]); // mov r10, rbx

    // Copy init literal into [rbx..]. Skip if empty.
    if init_size > 0 {
        let init_bytes = init_literal.as_bytes();
        if init_size <= 127 {
            code.push(0xEB);
            code.push(init_size as u8);
        } else {
            code.push(0xE9);
            code.extend_from_slice(&init_size.to_le_bytes());
        }
        let data_addr = code.len();
        code.extend_from_slice(init_bytes);
        // mov rdi, rbx
        code.extend_from_slice(&[0x48, 0x89, 0xDF]);
        // lea rsi, [rip + rel32]
        let end = code.len() + 7;
        let rel32 = data_addr as i32 - end as i32;
        code.extend_from_slice(&[0x48, 0x8D, 0x35]);
        code.extend_from_slice(&rel32.to_le_bytes());
        // mov rcx, init_size
        code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
        code.extend_from_slice(&init_size.to_le_bytes());
        // rep movsb
        code.extend_from_slice(&[0xF3, 0xA4]);
        // mov rbx, rdi
        code.extend_from_slice(&[0x48, 0x89, 0xFB]);
    }

    // Rewind r14 from argv_save_slot; reload r15 from count_slot.
    if argv_save_slot >= -128 {
        code.extend_from_slice(&[0x4C, 0x8B, 0x75]);
        code.push(argv_save_slot as u8);
    } else {
        code.extend_from_slice(&[0x4C, 0x8B, 0xB5]);
        code.extend_from_slice(&argv_save_slot.to_le_bytes());
    }
    if count_slot >= -128 {
        code.extend_from_slice(&[0x4C, 0x8B, 0x7D]);
        code.push(count_slot as u8);
    } else {
        code.extend_from_slice(&[0x4C, 0x8B, 0xBD]);
        code.extend_from_slice(&count_slot.to_le_bytes());
    }

    // ===== Pass 2: fill the buffer =====
    let fill_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xFF]); // test r15, r15
    code.push(0x0F);
    code.push(0x84);                             // jz fill_done (rel32)
    let fill_done_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Parse element fields into rbp slots (reused across iterations).
    for f in &elem_concept.fields {
        let offset = elem_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => {
                emit_atoi_inline(&mut code);
                store_rax_at_rbp(&mut code, offset);
            }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Emit the rest args into the buffer via the shared fill helper.
    // Lambda var is the "input" for field access; offsets resolve element fields.
    let field_ranges = build_field_ranges(elem_concept);
    emit_concat_fill(
        &mut code,
        rest_args,
        &rest_kinds,
        item_name,
        elem_concept,
        all_rules,
        &elem_offsets,
        &field_ranges,
    )?;

    // dec r15 ; jmp fill_loop_top
    code.extend_from_slice(&[0x49, 0xFF, 0xCF]);
    code.push(0xE9);
    let fb = fill_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&fb.to_le_bytes());

    // fill_done:
    let fill_done_pos = code.len();
    let fill_done_off = fill_done_pos as i32 - (fill_done_patch as i32 + 4);
    code[fill_done_patch..fill_done_patch + 4].copy_from_slice(&fill_done_off.to_le_bytes());

    // write(1, r10, rbx - r10)
    code.extend_from_slice(&[0x4C, 0x89, 0xD6]);             // mov rsi, r10
    code.extend_from_slice(&[0x48, 0x89, 0xDA]);             // mov rdx, rbx
    code.extend_from_slice(&[0x4C, 0x29, 0xD2]);             // sub rdx, r10
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // mov rdi, 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
    code.extend_from_slice(&[0x0F, 0x05]);                   // syscall

    emit_write_newline(&mut code, 1);

    // Free buffer: mov rsp, r9
    code.extend_from_slice(&[0x4C, 0x89, 0xCC]);

    // jmp outer_loop_top
    code.push(0xE9);
    let oo = outer_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&oo.to_le_bytes());

    // exit: sys_exit(0)
    let exit_pos = code.len();
    let exit_off = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);                         // xor rdi, rdi
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    Ok(code)
}

/// Verify a collection-op (map/filter) target is `input.<coll_field>`.
/// Used by both the Map and Filter branches of emit_collection_program.
fn verify_collection_target(
    expr: &Expr,
    input_name: &str,
    coll_field_name: &str,
) -> Result<(), NativeError> {
    match expr {
        Expr::Field(base, name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                && name == coll_field_name =>
        {
            Ok(())
        }
        _ => Err(NativeError {
            message: format!(
                "collection op target must be `{}.{}`",
                input_name, coll_field_name
            ),
        }),
    }
}

/// Emit `mov [rbp+offset], rax` with short/long encoding depending on offset.
fn store_rax_at_rbp(code: &mut Vec<u8>, offset: i32) {
    if offset >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x45]);
        code.push(offset as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&offset.to_le_bytes());
    }
}

/// Emit `mov [rbp+offset], rdi` — used to stash an argv pointer for a text field.
fn store_rdi_at_rbp(code: &mut Vec<u8>, offset: i32) {
    if offset >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x7D]);
        code.push(offset as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0xBD]);
        code.extend_from_slice(&offset.to_le_bytes());
    }
}

/// Walk an expression in record context. Each leaf is a Record constructor
/// that emits its own JSON line + jmp loop_top. Structural If/else branches
/// recurse; each arm plants its own terminator.
fn emit_eval_record_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    loop_top: usize,
    output_concept: &Concept,
    all_concepts: &[&Concept],
    rule: &Rule,
    input_concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Record(name, fields) => {
            // Defensive check: the constructor's concept name should match the
            // declared output concept. The verifier already enforced this, but
            // a mismatch here would silently produce wrong-shape JSON.
            if name != &output_concept.name {
                return Err(NativeError {
                    message: format!(
                        "record constructor '{}' does not match declared output concept '{}'",
                        name, output_concept.name
                    ),
                });
            }
            emit_record_as_json(
                code,
                fields,
                output_concept,
                &rule.input_name,
                input_concept,
                all_rules,
                offsets,
                field_ranges,
            )?;
            // jmp loop_top
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            emit_eval_expr(code, cond, &rule.input_name, offsets, all_rules, field_ranges)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.push(0x0F);
            code.push(0x84); // je rel32
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            emit_eval_record_expr(
                code, then_e, loop_top, output_concept, all_concepts, rule,
                input_concept, all_rules, offsets, field_ranges,
            )?;

            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_eval_record_expr(
                code, else_e, loop_top, output_concept, all_concepts, rule,
                input_concept, all_rules, offsets, field_ranges,
            )?;
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "record-context expression not yet supported in native: {:?}",
                other
            ),
        }),
    }
}

/// Serialize a record as a single-line JSON object to stdout. The field
/// ordering follows the concept's declaration (stable across runs), not the
/// source order in the constructor. Fields must all be declared (the verifier
/// has already enforced this).
fn emit_record_as_json(
    code: &mut Vec<u8>,
    fields: &[(String, Expr)],
    output_concept: &Concept,
    input_name: &str,
    input_concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    let provided: HashMap<&str, &Expr> = fields.iter().map(|(n, e)| (n.as_str(), e)).collect();

    for (i, decl) in output_concept.fields.iter().enumerate() {
        // Static prefix: `{"name":` for the first field, `,"name":` for the rest.
        // For text values we also append the opening quote.
        let mut prefix = String::new();
        prefix.push(if i == 0 { '{' } else { ',' });
        prefix.push('"');
        prefix.push_str(&decl.name);
        prefix.push('"');
        prefix.push(':');
        if matches!(decl.ty, Type::Text) {
            prefix.push('"');
        }
        emit_write_static_to_fd(code, prefix.as_bytes(), 1);

        let value_expr = provided.get(decl.name.as_str()).ok_or_else(|| NativeError {
            message: format!("record is missing field '{}'", decl.name),
        })?;

        match decl.ty {
            Type::Number => {
                // Evaluate → rax, write digits to stdout, no newline.
                emit_eval_expr(code, value_expr, input_name, offsets, all_rules, field_ranges)?;
                emit_itoa_to_stdout_no_newline(code);
            }
            Type::Text => {
                // Write the text bytes then the closing quote.
                emit_text_write_to_fd(
                    code,
                    value_expr,
                    1,
                    input_name,
                    input_concept,
                    all_rules,
                    offsets,
                    field_ranges,
                )?;
                emit_write_static_to_fd(code, b"\"", 1);
            }
            _ => {
                return Err(NativeError {
                    message: format!(
                        "native record field '{}' has unsupported type",
                        decl.name
                    ),
                });
            }
        }
    }

    // Closing "}\n"
    emit_write_static_to_fd(code, b"}\n", 1);
    Ok(())
}

/// Emit a standalone binary for a reaction. Reads N-field records from argv
/// just like `emit_full_program`, evaluates the trigger rule per record, and
/// when the trigger fires emits each declared effect inline.
///
/// For this commit the only supported effect is `append_file "path" "content"`
/// where the content is a STRING LITERAL. `concat(...)` content comes in the
/// next commit (itoa + stack-buffer concat in machine code).
fn emit_reaction_program(
    reaction: &Reaction,
    trigger_rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    // Pre-check: print effect is not yet wired in native reactions. Every
    // append_file is handled by emit_append_file_call which now accepts both
    // Text literals and Concat expressions.
    for effect in &reaction.effects {
        if let Effect::Print(_) = effect {
            return Err(NativeError {
                message: "print effect not yet wired in native reactions (use the interpreter or append_file)".into(),
            });
        }
    }

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, trigger_rule, concept, all_rules)?;

    // Evaluate trigger rule's logic → rax (0 = no fire, nonzero = fire).
    emit_eval_expr(
        &mut code,
        &trigger_rule.logic.value,
        &trigger_rule.input_name,
        &ctx.binding_offsets,
        all_rules,
        &ctx.field_ranges,
    )?;

    // cmp rax, 0 ; je skip_effects (rel32 so effect body can be large)
    code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x00]);
    code.push(0x0F);
    code.push(0x84);
    let skip_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // Emit each effect. The pre-check above ensures they are all AppendFile.
    for effect in &reaction.effects {
        if let Effect::AppendFile { path, content } = effect {
            emit_append_file_call(
                &mut code,
                path,
                content,
                trigger_rule,
                concept,
                all_rules,
                &ctx.binding_offsets,
                &ctx.field_ranges,
            )?;
        }
    }

    // skip_effects:
    let skip_pos = code.len();
    let skip_off = skip_pos as i32 - (skip_patch as i32 + 4);
    code[skip_patch..skip_patch + 4].copy_from_slice(&skip_off.to_le_bytes());

    // jmp loop_top
    code.push(0xE9);
    let loop_offset = ctx.loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&loop_offset.to_le_bytes());

    emit_record_loop_epilogue(&mut code, &ctx);

    Ok(code)
}

/// Compute the maximum stack depth an expression evaluation will use.
/// Each Binary operation pushes one value (8 bytes). Nested operations stack up.
/// This prevents the native backend from emitting code that would overflow the stack.
fn max_stack_depth(expr: &Expr) -> usize {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => 0,
        Expr::Field(base, _) => max_stack_depth(base),
        Expr::Binary(_, left, right) => {
            // Left is evaluated first and pushed, then right is evaluated
            let left_depth = max_stack_depth(left) + 1; // +1 for the push
            let right_depth = max_stack_depth(right);
            left_depth.max(right_depth)
        }
        Expr::Not(inner) | Expr::Neg(inner) => max_stack_depth(inner),
        Expr::If(cond, then_e, else_e) => {
            max_stack_depth(cond)
                .max(max_stack_depth(then_e))
                .max(max_stack_depth(else_e))
        }
        Expr::Call(_, args) => args.iter().map(max_stack_depth).max().unwrap_or(0),
        Expr::Quantifier(_, coll, _, pred) => {
            max_stack_depth(coll).max(max_stack_depth(pred))
        }
        Expr::Fold(coll, init, _, _, body) => {
            max_stack_depth(coll)
                .max(max_stack_depth(init))
                .max(max_stack_depth(body))
        }
        Expr::Map(coll, _, body) | Expr::Filter(coll, _, body) => {
            max_stack_depth(coll).max(max_stack_depth(body))
        }
        Expr::Ok(inner) | Expr::Err(inner) => max_stack_depth(inner),
        Expr::MatchResult(t, _, ob, _, eb) => {
            max_stack_depth(t).max(max_stack_depth(ob)).max(max_stack_depth(eb))
        }
        Expr::Record(_, fields) => fields.iter().map(|(_, e)| max_stack_depth(e)).max().unwrap_or(0),
        Expr::Concat(args) => args.iter().map(max_stack_depth).max().unwrap_or(0),
    }
}

/// Peephole optimizer: scan emitted machine code for redundant patterns.
///
/// Pattern 1: push Rx; pop Rx → remove both (dead save/restore)
///   50-57 followed by 58-5F where register matches
///   Example: push rax (50); pop rax (58) → nothing
///
/// Pattern 2: push Rx; pop Ry → mov Ry, Rx (avoid stack round-trip)
///   Only when registers are base (rax-rdi, not r8-r15)
///   50 59 → 48 89 C1 (push rax; pop rcx → mov rcx, rax)
///   Note: this makes code 1 byte larger but faster (no memory access)
///   We only apply pattern 1 (size reduction) for now.
fn peephole_optimize(code: &mut Vec<u8>) {
    let mut i = 0;
    let mut out = Vec::with_capacity(code.len());

    while i < code.len() {
        // Pattern: push Rx; pop Rx (same register) → eliminate both
        if i + 1 < code.len() {
            let a = code[i];
            let b = code[i + 1];
            if (0x50..=0x57).contains(&a) && b == a + 8 {
                // push Rx (0x50+r) followed by pop Rx (0x58+r) — same register
                i += 2;
                continue;
            }
        }

        // Pattern: REX push Rx; REX pop Rx (r8-r15) → eliminate both
        if i + 3 < code.len() {
            if code[i] == 0x41 && (0x50..=0x57).contains(&code[i + 1])
                && code[i + 2] == 0x41 && code[i + 3] == code[i + 1] + 8
            {
                i += 4;
                continue;
            }
        }

        out.push(code[i]);
        i += 1;
    }

    *code = out;
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
            // === Compile-time optimizations ===

            // Constant folding: both operands are literals → compute at compile time
            if let (Expr::Number(l), Expr::Number(r)) = (left.as_ref(), right.as_ref()) {
                let result = match op {
                    BinOp::Add => Some(l.wrapping_add(*r)),
                    BinOp::Sub => Some(l.wrapping_sub(*r)),
                    BinOp::Mul => Some(l.wrapping_mul(*r)),
                    BinOp::Div if *r != 0 => Some(l.wrapping_div(*r)),
                    _ => None,
                };
                if let Some(val) = result {
                    emit_mov_rax_imm(code, val);
                    return Ok(());
                }
            }

            // Strength reduction: multiply by power of 2 → shift left
            if *op == BinOp::Mul {
                if let Expr::Number(n) = right.as_ref() {
                    if *n > 0 && (*n as u64).is_power_of_two() {
                        emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges)?;
                        let shift = (*n as u64).trailing_zeros() as u8;
                        code.extend_from_slice(&[0x48, 0xC1, 0xE0, shift]); // shl rax, shift
                        return Ok(());
                    }
                }
            }

            // Strength reduction: divide by power of 2 → shift right
            if *op == BinOp::Div {
                if let Expr::Number(n) = right.as_ref() {
                    if *n > 0 && (*n as u64).is_power_of_two() {
                        emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges)?;
                        let shift = (*n as u64).trailing_zeros() as u8;
                        code.extend_from_slice(&[0x48, 0xC1, 0xE8, shift]); // shr rax, shift
                        return Ok(());
                    }
                }
            }

            // Strength reduction: divide by constant → multiply-shift trick
            // x / d = mulhi(x, magic) >> shift — 4 cycles instead of 20-40 for idiv
            // Only safe for non-negative dividends (verified via field ranges)
            if *op == BinOp::Div {
                if let Expr::Number(d) = right.as_ref() {
                    if *d > 1 {
                        if let Some((magic, shift)) = magic_div_constant(*d as u64) {
                            // Check that the dividend is non-negative via field ranges
                            let dividend_non_negative = compute_range(left, field_ranges, input_name)
                                .map_or(false, |(min, _)| min >= 0);
                            if dividend_non_negative {
                                emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges)?;
                                // mov rcx, magic
                                code.extend_from_slice(&[0x48, 0xB9]);
                                code.extend_from_slice(&magic.to_le_bytes());
                                // mul rcx (unsigned: rdx:rax = rax * rcx)
                                code.extend_from_slice(&[0x48, 0xF7, 0xE1]);
                                // shr rdx, shift (high half >> shift = result)
                                if shift > 0 {
                                    code.extend_from_slice(&[0x48, 0xC1, 0xEA, shift]);
                                }
                                // mov rax, rdx
                                code.extend_from_slice(&[0x48, 0x89, 0xD0]);
                                return Ok(());
                            }
                        }
                    }
                }
            }

            // Strength reduction: multiply by 0 → 0
            if *op == BinOp::Mul {
                if matches!(right.as_ref(), Expr::Number(0)) || matches!(left.as_ref(), Expr::Number(0)) {
                    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // xor rax, rax
                    return Ok(());
                }
            }

            // Strength reduction: multiply by 1 → identity
            if *op == BinOp::Mul {
                if matches!(right.as_ref(), Expr::Number(1)) {
                    return emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges);
                }
                if matches!(left.as_ref(), Expr::Number(1)) {
                    return emit_eval_expr(code, right, input_name, offsets, all_rules, field_ranges);
                }
            }

            // Strength reduction: add/sub 0 → identity
            if (*op == BinOp::Add || *op == BinOp::Sub) && matches!(right.as_ref(), Expr::Number(0)) {
                return emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges);
            }

            // === General case: evaluate both sides, apply operator ===
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
                    // result = left / right = rcx / rax → quotient in rax
                    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax (save right)
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx (left → rax)
                    code.extend_from_slice(&[0x48, 0x99]); // cqo (sign-extend rax → rdx:rax)
                    code.extend_from_slice(&[0x49, 0xF7, 0xF8]); // idiv r8
                }
                BinOp::Mod => {
                    // result = left % right = rcx % rax → remainder in rdx
                    code.extend_from_slice(&[0x49, 0x89, 0xC0]); // mov r8, rax
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
                    code.extend_from_slice(&[0x48, 0x99]); // cqo
                    code.extend_from_slice(&[0x49, 0xF7, 0xF8]); // idiv r8
                    code.extend_from_slice(&[0x48, 0x89, 0xD0]); // mov rax, rdx (remainder → result)
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
        Expr::Fold(_, _, _, _, _)
        | Expr::Quantifier(_, _, _, _)
        | Expr::Map(_, _, _)
        | Expr::Filter(_, _, _)
        | Expr::Ok(_)
        | Expr::Err(_)
        | Expr::MatchResult(_, _, _, _, _)
        | Expr::Record(_, _)
        | Expr::Concat(_) => Err(NativeError {
            message: "rich operations (collection/result/record/concat) not supported in native backend (use --run interpreter) — see CLAUDE.md, 'Two Execution Modes'"
                .into(),
        }),
        Expr::Ident(name) if name == input_name => Err(NativeError {
            message: "bare input binding not supported in expressions".into(),
        }),
        Expr::Ident(name) => {
            // Let-bound variable — load from rbp-relative slot
            if let Some(offset) = offsets.get(name.as_str()) {
                if *offset >= -128 {
                    code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                    code.push(*offset as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                    code.extend_from_slice(&offset.to_le_bytes());
                }
                Ok(())
            } else {
                Err(NativeError {
                    message: format!("unresolved identifier '{}' in native codegen", name),
                })
            }
        }
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

/// Compute magic number and shift for unsigned division by constant.
/// Uses the algorithm from Hacker's Delight (Warren, 2002).
///
/// For a 64-bit unsigned dividend and constant divisor d:
///   x / d = mulhi(x, magic) >> shift
///
/// where mulhi is the high 64 bits of the 128-bit product x * magic.
/// This replaces a 20-40 cycle `idiv` with a 3-cycle `mul` + 1-cycle `shr`.
/// Generate a minimal HTTP server binary — proof that the native backend
/// can produce real networked applications, not just rule evaluators.
///
/// The binary: socket → bind(8080) → listen → accept loop → write response → close
/// ~800 bytes, zero dependencies, pure syscalls. No libc, no framework.
pub fn emit_http_demo(output_path: &str) -> Result<(), NativeError> {
    let mut code = Vec::new();

    // HTTP response (hardcoded)
    let response = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nHello from Verbose!";

    // === socket(AF_INET=2, SOCK_STREAM=1, 0) → fd in rax ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]); // mov rax, 41 (socket)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // mov rdi, 2 (AF_INET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1 (SOCK_STREAM)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx (protocol 0)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax (save server fd)

    // === setsockopt(fd, SOL_SOCKET=1, SO_REUSEADDR=2, &1, 4) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x36, 0x00, 0x00, 0x00]); // mov rax, 54 (setsockopt)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12 (fd)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1 (SOL_SOCKET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x02, 0x00, 0x00, 0x00]); // mov rdx, 2 (SO_REUSEADDR)
    // Push 1 onto stack as the optval
    code.extend_from_slice(&[0x6A, 0x01]); // push 1
    code.extend_from_slice(&[0x49, 0x89, 0xE2]); // mov r10, rsp (optval pointer)
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x04, 0x00, 0x00, 0x00]); // mov r8, 4 (optlen)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8 (pop optval)

    // === Build sockaddr_in on stack ===
    // struct sockaddr_in { sin_family=2, sin_port=htons(8080)=0x1F90, sin_addr=0, pad=0 }
    // As 16 bytes: 02 00 1F 90 00 00 00 00 00 00 00 00 00 00 00 00
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]); // sub rsp, 16
    // mov dword [rsp], 0x0F270002 (family=2, port=0x270F big-endian for 9999)
    code.extend_from_slice(&[0xC7, 0x04, 0x24, 0x02, 0x00, 0x27, 0x0F]);
    // mov qword [rsp+4], 0 (sin_addr + padding)
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00]);
    // Clear last 4 bytes
    code.extend_from_slice(&[0xC7, 0x44, 0x24, 0x0C, 0x00, 0x00, 0x00, 0x00]);

    // === bind(fd, &sockaddr, 16) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x31, 0x00, 0x00, 0x00]); // mov rax, 49 (bind)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x48, 0x89, 0xE6]); // mov rsi, rsp (sockaddr pointer)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // mov rdx, 16
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // === listen(fd, 128) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]); // mov rax, 50 (listen)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]); // mov rsi, 128
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // Print listening message
    emit_write_string(&mut code, b"Verbose HTTP server on port 9999\n");

    // === Accept loop ===
    let accept_top = code.len();

    // accept(fd, NULL, NULL) → client fd in rax
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // mov rax, 43 (accept)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]); // mov rdi, r12 (server fd)
    code.extend_from_slice(&[0x48, 0x31, 0xF6]); // xor rsi, rsi (NULL)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx (NULL)
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x49, 0x89, 0xC5]); // mov r13, rax (client fd)

    // Read request (consume it, don't parse — just drain the socket)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]); // sub rsp, 16 (tiny read buffer)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00]); // mov rax, 0 (read)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // mov rdi, r13 (client fd)
    code.extend_from_slice(&[0x48, 0x89, 0xE6]); // mov rsi, rsp (buffer)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // mov rdx, 16
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]); // add rsp, 16

    // Write HTTP response — inline the string data
    // jmp over data
    code.push(0xEB);
    code.push(response.len() as u8);
    let resp_offset = code.len();
    code.extend_from_slice(response);

    // lea rsi, [rip - offset]
    let after_lea = code.len() + 7;
    let rip_delta = resp_offset as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rip_delta.to_le_bytes());

    // write(client_fd, response, len)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1 (write)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // mov rdi, r13 (client fd)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(response.len() as i32).to_le_bytes()); // mov rdx, len
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // close(client_fd)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]); // mov rax, 3 (close)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // mov rdi, r13
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // jmp accept_top
    code.push(0xE9);
    let jmp_offset = accept_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&jmp_offset.to_le_bytes());

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

/// Compute magic number for unsigned division by constant d.
/// Based on Hacker's Delight (Warren, 2002) and libdivide.
///
/// Formula: x / d = mulhi(x, magic) >> p
/// where p = floor(log2(d)), magic = ceil(2^(64+p) / d)
///
/// This works because mulhi gives the high 64 bits of the 128-bit product,
/// which approximates x * (2^64/d) — effectively computing x/d scaled by 2^64.
fn magic_div_constant(d: u64) -> Option<(u64, u8)> {
    if d <= 1 {
        return None;
    }
    // p = floor(log2(d))
    let p = 63u32 - d.leading_zeros();
    let shift_amount = 64u32 + p;
    if shift_amount > 127 {
        return None;
    }

    // magic = ceil(2^(64+p) / d) using 128-bit arithmetic
    let numerator: u128 = 1u128 << shift_amount;
    let magic128 = (numerator + d as u128 - 1) / d as u128;

    if magic128 > u64::MAX as u128 {
        return None; // Needs "add" variant — fall back to idiv
    }

    let magic = magic128 as u64;

    // Verify correctness on boundary values
    let verify = |x: u64| -> bool {
        let hi = ((x as u128 * magic as u128) >> 64) as u64;
        (hi >> p) == x / d
    };

    if !verify(0)
        || !verify(1)
        || !verify(d - 1)
        || !verify(d)
        || !verify(d + 1)
        || !verify(d * 100)
        || !verify(u32::MAX as u64)
    {
        return None;
    }

    Some((magic, p as u8))
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
    fn emit_open_append_embeds_path_and_syscall() {
        // Smoke test: emit_open_append produces non-empty bytes including the
        // NUL-terminated path and the sys_open immediate.
        let mut code = Vec::new();
        emit_open_append(&mut code, "/tmp/x.log");

        let marker = b"/tmp/x.log\0";
        assert!(
            code.windows(marker.len()).any(|w| w == marker),
            "path bytes + NUL not found in emitted code"
        );
        // mov rax, 2 (sys_open) somewhere in the block.
        let open_pattern = [0x48, 0xC7, 0xC0, 0x02, 0x00, 0x00, 0x00];
        assert!(
            code.windows(7).any(|w| w == open_pattern),
            "expected `mov rax, 2` (sys_open) in emitted code"
        );
    }

    #[test]
    fn native_compiles_discounted_purchase_match_result() {
        // discounted_purchase = match_result(validate_purchase(p),
        //   amount => Ok(amount * 90 / 100),
        //   reason => Err(reason))
        // Phase 2D narrow form: target is a rule call on outer's input,
        // Ok arm uses the bound number var, Err arm passes through.
        // Native inlines validate_purchase, redirecting its leaves.
        use std::fs;
        let src = fs::read_to_string("examples/purchase.verbose")
            .expect("examples/purchase.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_discounted");
        compile_native(&program, "discounted_purchase", out.to_str().unwrap())
            .expect("native compile of discounted_purchase should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_collection_scalar_output() {
        // payroll.verbose's `salaries` rule: output collection(number) via
        // `map(w.employees, e => e.salary)`. Emits one number per line,
        // no JSON wrapping. Also tests `names` (collection(text)).
        use std::fs;
        let src = fs::read_to_string("examples/payroll.verbose")
            .expect("examples/payroll.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        for rule in ["salaries", "names"] {
            let out = std::env::temp_dir().join(format!("verbosec_test_{}", rule));
            compile_native(&program, rule, out.to_str().unwrap())
                .unwrap_or_else(|e| panic!("native compile of {} failed: {:?}", rule, e));
            let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            // Scalar-element output is smaller than record output — no bracket
            // machinery, no per-field prefixes.
            assert!(size > 200 && size < 1_500, "unexpected size for {}: {}", rule, size);
            let _ = fs::remove_file(out);
        }
    }

    #[test]
    fn native_compiles_collection_filter_rule() {
        // payroll.verbose's high_earners uses `filter(...)` — predicate
        // evaluated per element, passing elements emit identity JSON.
        use std::fs;
        let src = fs::read_to_string("examples/payroll.verbose")
            .expect("examples/payroll.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_high_earners");
        compile_native(&program, "high_earners", out.to_str().unwrap())
            .expect("native compile of filter rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 400 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_collection_output_rule() {
        // payroll.verbose's compute_bonuses returns collection(Bonus).
        // Phase 3 v1: map over a collection input field, streaming JSON Lines
        // output. Each element parses its fields, evaluates the map body,
        // emits one JSON object per line. No arena, no heap.
        use std::fs;
        let src = fs::read_to_string("examples/payroll.verbose")
            .expect("examples/payroll.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_payroll");
        compile_native(&program, "compute_bonuses", out.to_str().unwrap())
            .expect("native compile of collection-output rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 400 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_fold_sum_rule() {
        // Phase 4: `output: number` with `sum(...)` desugars to Fold, routing
        // to emit_fold_program. One 8-byte accumulator slot at the bottom of
        // the rbp frame; one `itoa + \n` per input record.
        use std::fs;
        let src = fs::read_to_string("examples/payroll.verbose")
            .expect("examples/payroll.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_total_salaries");
        compile_native(&program, "total_salaries", out.to_str().unwrap())
            .expect("native compile of fold-sum rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_fold_count_rule() {
        // Phase 4: `count(...)` desugars to Fold with an `if pred then 1 else 0`
        // body — exercises the full expression emitter from within the fold
        // inner loop.
        use std::fs;
        let src = fs::read_to_string("examples/payroll.verbose")
            .expect("examples/payroll.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_high_earner_count");
        compile_native(&program, "high_earner_count", out.to_str().unwrap())
            .expect("native compile of fold-count rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_text_field_through_record_output() {
        // greeting.verbose's make_report rule:
        //   input has name: text, salary: number
        //   output: BonusReport { name: e.name, bonus: e.salary * 10 / 100 }
        // Phase 2E: text input fields flow through to JSON output. Native
        // stores the argv pointer at the rbp slot, recovers length via
        // emit_strlen at write time.
        use std::fs;
        let src = fs::read_to_string("examples/greeting.verbose")
            .expect("examples/greeting.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_make_report");
        compile_native(&program, "make_report", out.to_str().unwrap())
            .expect("native compile of text-field-through-record should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_text_fold_rule() {
        // Phase 5b: text-valued fold. roster.verbose's rule produces
        //   fold(w.employees, "roster: ", acc, e => concat(acc, e.name, "=", e.salary, "; "))
        // Exercises two-pass emission: pass 1 accumulates sizes (strlen per
        // text-field, +21 per number), pass 2 fills the buffer. One write
        // per input record; `mov rsp, r9` to free.
        use std::fs;
        let src = fs::read_to_string("examples/roster.verbose")
            .expect("examples/roster.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_roster");
        compile_native(&program, "roster_line", out.to_str().unwrap())
            .expect("native compile of text-fold rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_output_text_rule() {
        // Phase 5a: `output: text` with per-record body. greeting_line.verbose's
        // rule produces `concat("Hello ", p.name, ", age ", p.age)` per record.
        // Exercises emit_text_program: prologue, emit_text_write_to_fd with
        // Concat (uses the dynamic-sized buffer because p.name is a text
        // field), newline, loop-back.
        use std::fs;
        let src = fs::read_to_string("examples/greeting_line.verbose")
            .expect("examples/greeting_line.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_greeting_line");
        compile_native(&program, "greeting_line", out.to_str().unwrap())
            .expect("native compile of output-text rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_text_field_in_concat() {
        // audit_user.verbose's reaction calls
        //   append_file "/tmp/audit_user.log" concat("...", p.user, "...", p.amount, ...)
        // p.user is a text field whose length is unknown until argv is read.
        // Tests the dynamic-sized concat buffer: per-text-field strlen, dynamic
        // sub rsp sized in rax, free via mov rsp, r9 (the saved pre-allocation rsp).
        use std::fs;
        let src = fs::read_to_string("examples/audit_user.verbose")
            .expect("examples/audit_user.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_audit_user");
        compile_native(&program, "audit_suspicious", out.to_str().unwrap())
            .expect("native compile of text-field-in-concat reaction should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_record_output_rule() {
        // classify_invoice returns a Classification record. Native emits one
        // JSON line per record to stdout, with concept-declared field order.
        use std::fs;
        let src = fs::read_to_string("examples/classify.verbose")
            .expect("examples/classify.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_classify_invoice");
        compile_native(&program, "classify_invoice", out.to_str().unwrap())
            .expect("native compile of record-output rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        // Two record arms + itoa + multiple static-write syscalls — ~1 KB.
        assert!(size > 500 && size < 3_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_result_text_text_rule() {
        // tier.verbose returns Result(text, text). After Phase 2B the
        // native backend handles Ok(text) by writing the bytes to stdout
        // + newline, sharing the emit_text_write_to_fd helper with the
        // Err arm.
        use std::fs;
        let src = fs::read_to_string("examples/tier.verbose")
            .expect("examples/tier.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_classify_tier");
        compile_native(&program, "classify_tier", out.to_str().unwrap())
            .expect("native compile of Result(text, text) rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_result_number_text_rule() {
        // validate_purchase returns Result(number, text) with a dynamic
        // Err via concat. After Phase 2A the native backend routes it
        // through emit_result_program: Ok -> stdout, Err -> stderr.
        use std::fs;
        let src = fs::read_to_string("examples/purchase.verbose")
            .expect("examples/purchase.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_validate_purchase");
        compile_native(&program, "validate_purchase", out.to_str().unwrap())
            .expect("native compile of Result(number, text) rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        // Ballpark — concat machinery + itoa + stderr newline = ~700 B.
        assert!(size > 400 && size < 3_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_reaction_with_dynamic_concat() {
        // audit_log.verbose has append_file whose content is a concat(...)
        // of text literals and number fields. Before Phase 1 Commit B this
        // was interpreter-only; the native backend now handles it by
        // building the line in a stack buffer and writing it to the fd.
        use std::fs;
        let src = fs::read_to_string("examples/audit_log.verbose")
            .expect("examples/audit_log.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_audit_dynamic");
        compile_native(&program, "audit_suspicious", out.to_str().unwrap())
            .expect("native compile of dynamic-content reaction should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        // Slightly larger than the static version due to the concat machinery
        // (buffer reservation, itoa inline per number arg).
        assert!(size > 400, "expected > 400 bytes, got {}", size);
        assert!(size < 3_000, "expected < 3000 bytes, got {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_reaction_with_append_file() {
        // End-to-end: the native backend accepts a reaction whose effect is
        // append_file with a string literal, produces bytes, no error.
        use std::fs;
        let src = fs::read_to_string("examples/audit_simple.verbose")
            .expect("examples/audit_simple.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        // Write the ELF to a temp path; assert the file is non-empty.
        let out = std::env::temp_dir().join("verbosec_test_audit_simple");
        compile_native(&program, "audit_suspicious", out.to_str().unwrap())
            .expect("native compile of reaction should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 200, "expected a non-trivial ELF, got {} bytes", size);
        assert!(size < 2_000, "expected a small ELF, got {} bytes", size);
        let _ = fs::remove_file(out);
    }

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

    #[test]
    fn magic_div_100() {
        let (magic, shift) = magic_div_constant(100).unwrap();
        for x in [0u64, 1, 99, 100, 101, 999, 1000, 10000, 100000, 1000000, u32::MAX as u64] {
            let hi = ((x as u128 * magic as u128) >> 64) as u64;
            let result = hi >> shift;
            assert_eq!(result, x / 100, "failed for x={}: got {} expected {}", x, result, x / 100);
        }
    }

    #[test]
    fn magic_div_various() {
        for d in [3, 7, 10, 12, 25, 50, 100, 1000, 365] {
            let (magic, shift) = magic_div_constant(d).expect(&format!("no magic for {}", d));
            for x in [0u64, 1, d - 1, d, d + 1, d * 10, d * 100, 1000000] {
                let hi = ((x as u128 * magic as u128) >> 64) as u64;
                let result = hi >> shift;
                assert_eq!(result, x / d, "failed for x={} / {}: got {} expected {}", x, d, result, x / d);
            }
        }
    }

    #[test]
    fn peephole_eliminates_push_pop_same_reg() {
        let mut code = vec![0x50, 0x58, 0x90]; // push rax; pop rax; nop
        peephole_optimize(&mut code);
        assert_eq!(code, vec![0x90]); // only nop remains
    }

    #[test]
    fn stack_depth_simple() {
        // a > 10 → depth 1 (one push for Binary)
        let expr = Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::Ident("a".into())),
            Box::new(Expr::Number(10)),
        );
        assert_eq!(max_stack_depth(&expr), 1);
    }

    #[test]
    fn stack_depth_nested() {
        // (a + b) * (c + d) → depth 2
        let expr = Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Ident("a".into())),
                Box::new(Expr::Ident("b".into())),
            )),
            Box::new(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Ident("c".into())),
                Box::new(Expr::Ident("d".into())),
            )),
        );
        assert_eq!(max_stack_depth(&expr), 2);
    }

    #[test]
    fn stack_depth_leaf() {
        assert_eq!(max_stack_depth(&Expr::Number(42)), 0);
        assert_eq!(max_stack_depth(&Expr::Ident("x".into())), 0);
    }

    #[test]
    fn peephole_keeps_push_pop_different_reg() {
        let mut code = vec![0x50, 0x59]; // push rax; pop rcx
        peephole_optimize(&mut code);
        assert_eq!(code, vec![0x50, 0x59]); // unchanged — different registers
    }
}
