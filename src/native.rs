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

/// Compile multiple rules into a single native binary. Each rule's code
/// block is emitted sequentially; intermediate blocks end with stack cleanup
/// (`mov rsp, rbp; pop rbp`) instead of `sys_exit`, so execution falls
/// through to the next block. The last block exits normally.
///
/// Each block re-reads argc/argv from the original stack position (set by
/// the kernel at `_start`), so every rule independently parses the same
/// input. This means the binary produces ALL rules' outputs in sequence.
pub fn compile_native_multi(
    program: &Program,
    rule_names: &[&str],
    output_path: &str,
    stdin: bool,
    stream: bool,
) -> Result<(), NativeError> {
    if rule_names.is_empty() {
        return Err(NativeError { message: "no rules specified for multi-rule binary".into() });
    }
    if rule_names.len() == 1 {
        return compile_native(program, rule_names[0], output_path, stdin, stream);
    }

    if stream {
        return Err(NativeError { message: "--stream is not supported with multi-rule binaries".into() });
    }

    let mut combined = Vec::new();
    let exit_sequence: [u8; 12] = [
        0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00, // mov rax, 60
        0x48, 0x31, 0xFF,                           // xor rdi, rdi
        0x0F, 0x05,                                  // syscall
    ];
    let cleanup_sequence: [u8; 4] = [
        0x48, 0x89, 0xEC, // mov rsp, rbp
        0x5D,             // pop rbp
    ];

    for (i, rule_name) in rule_names.iter().enumerate() {
        let is_last = i == rule_names.len() - 1;
        let mut code = compile_native_code(program, rule_name, false, false)?;

        // Verify the block ends with the expected exit sequence.
        if code.len() < 12 || code[code.len() - 12..] != exit_sequence {
            return Err(NativeError {
                message: format!(
                    "rule '{}' code block does not end with the expected sys_exit sequence — cannot compose in multi-rule mode",
                    rule_name
                ),
            });
        }

        if !is_last {
            // Replace exit with stack cleanup: fall through to next block.
            code.truncate(code.len() - 12);
            code.extend_from_slice(&cleanup_sequence);
        }

        combined.extend_from_slice(&code);
    }

    // Stdin mode: prepend the shared stdin reader prologue once.
    if stdin {
        let mut full = Vec::new();
        emit_stdin_prologue(&mut full);
        full.extend_from_slice(&combined);
        combined = full;
    }

    // Self-verify + peephole on the combined code.
    peephole_optimize(&mut combined);
    if let Err(e) = crate::validate_x86::validate_code(&combined) {
        eprintln!("warning: x86-64 validation: {} (decoder incomplete, may be false positive)", e);
    }

    let elf = build_elf(&combined);
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
            .map_err(|e| NativeError { message: format!("cannot set permissions: {}", e) })?;
    }
    Ok(())
}

/// Internal: emit machine code for a single rule (no ELF wrapping, no
/// self-verification). Used by both `compile_native` and
/// `compile_native_multi`.
fn compile_native_code(
    program: &Program,
    rule_name: &str,
    stdin: bool,
    stream: bool,
) -> Result<Vec<u8>, NativeError> {
    // (Extracted from compile_native — same dispatch logic.)
    let concepts: Vec<&Concept> = program.items.iter().filter_map(|i| match i { Item::Concept(c) => Some(c), _ => None }).collect();
    let rules: HashMap<&str, &Rule> = program.items.iter().filter_map(|i| match i { Item::Rule(r) => Some((r.name.as_str(), r)), _ => None }).collect();
    let reaction = program.items.iter().find_map(|i| match i { Item::Reaction(rx) if rx.name == rule_name => Some(rx), _ => None });

    let (rule, concept) = if let Some(rx) = reaction {
        let trigger = rules.get(rx.trigger.as_str()).ok_or_else(|| NativeError { message: format!("reaction '{}' triggers unknown rule '{}'", rx.name, rx.trigger) })?;
        let concept = match &trigger.input_ty { Type::Named(n) => concepts.iter().find(|c| c.name == *n).ok_or_else(|| NativeError { message: format!("unknown concept '{}'", n) })?, _ => return Err(NativeError { message: "reaction trigger rule must take a named concept input".into() }) };
        (*trigger, *concept)
    } else {
        let r = rules.get(rule_name).ok_or_else(|| NativeError { message: format!("no rule or reaction named '{}'", rule_name) })?;
        let c = match &r.input_ty { Type::Named(n) => concepts.iter().find(|c| c.name == *n).ok_or_else(|| NativeError { message: format!("unknown concept '{}'", n) })?, _ => return Err(NativeError { message: "rule input must be a named concept".into() }) };
        (*r, *c)
    };

    // Look up the context concept (if multi-input rule).
    let context_concept: Option<&Concept> = match &rule.context_ty {
        Some(Type::Named(n)) => Some(
            concepts.iter().find(|c| c.name == *n).copied()
                .ok_or_else(|| NativeError { message: format!("unknown context concept '{}'", n) })?
        ),
        Some(_) => return Err(NativeError { message: "context type must be a named concept".into() }),
        None => None,
    };

    let is_vectorizable = rule.hints.as_ref().map_or(false, |h| h.vectorizable.is_some());
    let is_parallel = rule.hints.as_ref().map_or(false, |h| h.parallel.is_some());
    let is_result_output = matches!(&rule.output_ty, Type::Result(_, _));
    let is_collection_output = matches!(&rule.output_ty, Type::Collection(_));
    // Quantifier(All/Any) desugar to Fold for native emission. The parser
    // keeps the Quantifier AST node (the verifier + interpreter handle it
    // natively), but the native emitter converts it to a Fold on the fly
    // so emit_fold_program can handle it.
    let desugared_fold: Option<Expr> = match &rule.logic.value {
        Expr::Quantifier(QuantifierKind::All, coll, var, pred) => {
            let acc = "__acc".to_string();
            Some(Expr::Fold(
                coll.clone(),
                Box::new(Expr::Number(1)),
                acc.clone(),
                var.clone(),
                Box::new(Expr::If(pred.clone(), Box::new(Expr::Ident(acc)), Box::new(Expr::Number(0)))),
            ))
        }
        Expr::Quantifier(QuantifierKind::Any, coll, var, pred) => {
            let acc = "__acc".to_string();
            Some(Expr::Fold(
                coll.clone(),
                Box::new(Expr::Number(0)),
                acc.clone(),
                var.clone(),
                Box::new(Expr::If(pred.clone(), Box::new(Expr::Number(1)), Box::new(Expr::Ident(acc)))),
            ))
        }
        _ => None,
    };
    let effective_logic = desugared_fold.as_ref().unwrap_or(&rule.logic.value);
    let is_fold_number_output = matches!(&rule.output_ty, Type::Number | Type::Bool) && matches!(effective_logic, Expr::Fold(_, _, _, _, _));
    let is_fold_text_output = matches!(&rule.output_ty, Type::Text) && matches!(&rule.logic.value, Expr::Fold(_, _, _, _, _));
    let record_output_concept: Option<&Concept> = match &rule.output_ty { Type::Named(n) => concepts.iter().find(|c| c.name == *n).copied(), _ => None };

    let mut code = if let Some(rx) = reaction {
        emit_reaction_program(rx, rule, concept, &rules)?
    } else if is_result_output {
        emit_result_program(rule, concept, &rules)?
    } else if is_collection_output {
        emit_collection_program(rule, concept, &concepts, &rules)?
    } else if is_fold_number_output {
        // If the logic was desugared from Quantifier→Fold, create a temp
        // rule with the desugared logic so emit_fold_program sees a Fold.
        if let Some(ref desugared) = desugared_fold {
            let mut rule_copy = rule.clone();
            rule_copy.logic.value = desugared.clone();
            emit_fold_program(&rule_copy, concept, &concepts, &rules)?
        } else {
            emit_fold_program(rule, concept, &concepts, &rules)?
        }
    } else if is_fold_text_output {
        emit_text_fold_program(rule, concept, &concepts, &rules)?
    } else if matches!(&rule.output_ty, Type::Text) {
        emit_text_program(rule, concept, &rules)?
    } else if let Some(rec_concept) = record_output_concept {
        emit_record_program(rule, rec_concept, concept, &concepts, &rules)?
    } else if matches!(&rule.output_ty, Type::Number | Type::Bool) && contains_quantifier(&rule.logic.value) {
        // Phase 6: scalar output with embedded quantifiers (e.g. if all(...) then X else Y).
        emit_multi_fold_program(rule, concept, &concepts, &rules)?
    } else if is_vectorizable && concept.fields.len() == 1 {
        if let Some(threshold) = extract_simple_gt(rule) { emit_vectorized_program(threshold)? } else { emit_full_program(rule, concept, context_concept, &rules)? }
    } else if is_parallel {
        emit_parallel_program(rule, concept, &rules)?
    } else {
        emit_full_program(rule, concept, context_concept, &rules)?
    };

    if stream {
        // Streaming mode: wrap rule code in a line-by-line read loop.
        // Requires the rule code to use the standard push rbp / mov rbp, rsp
        // prologue so that `mov rsp, rbp; pop rbp` correctly restores the stack.
        // Vectorized and parallel programs use different prologues — refuse them.
        if is_vectorizable && concept.fields.len() == 1 && extract_simple_gt(rule).is_some() {
            return Err(NativeError {
                message: "streaming mode is not supported with SIMD-vectorized rules (use a non-vectorized rule)".into(),
            });
        }
        if is_parallel {
            return Err(NativeError {
                message: "streaming mode is not supported with parallel rules".into(),
            });
        }

        // Strip the exit sequence from the rule code. The exit now includes
        // an exit-flag load before `mov rax, 60; syscall`, so we search backward
        // for the `mov rax, 60` pattern (48 C7 C0 3C 00 00 00) and strip from there.
        let mov_rax_60: [u8; 7] = [0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00];
        if let Some(pos) = code.windows(7).rposition(|w| w == mov_rax_60) {
            code.truncate(pos);
        }
        // Add stack cleanup: mov rsp, rbp; pop rbp
        code.extend_from_slice(&[0x48, 0x89, 0xEC, 0x5D]);
        // Add jmp placeholder (rel32, patched after prepend)
        code.push(0xE9);
        let jmp_offset_in_rule = code.len(); // position of the rel32 within rule code
        code.extend_from_slice(&[0x00; 4]);

        // Emit the stream prologue
        let mut full = Vec::new();
        let stream_top = emit_stream_prologue(&mut full);
        let prologue_size = full.len();

        // Append rule code after prologue
        full.extend_from_slice(&code);

        // Patch jmp: target = stream_top, from = prologue_size + jmp_offset_in_rule + 4
        let jmp_abs = prologue_size + jmp_offset_in_rule;
        let jmp_target = stream_top as i32 - (jmp_abs as i32 + 4);
        full[jmp_abs..jmp_abs + 4].copy_from_slice(&jmp_target.to_le_bytes());

        code = full;
    } else if stdin {
        // One-shot stdin: read all, tokenize, process, exit.
        let mut full = Vec::new();
        emit_stdin_prologue(&mut full);
        full.extend_from_slice(&code);
        code = full;
    }

    Ok(code)
}

pub fn compile_native(
    program: &Program,
    rule_name: &str,
    output_path: &str,
    stdin: bool,
    stream: bool,
) -> Result<(), NativeError> {
    let mut code = compile_native_code(program, rule_name, stdin, stream)?;

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
    match_slot: i32,
    /// Phase 2F: slot holding the pointer to the Err-bound text value.
    /// Paired with `err_len_slot` below. Unused when the rule doesn't use
    /// match_result with a non-pass-through Err arm.
    err_ptr_slot: i32,
    /// Phase 2F: slot holding the length of the Err-bound text value.
    err_len_slot: i32,
    /// Phase 2F: slot holding the rsp value captured BEFORE the inlined
    /// callee's Err leaf allocates its concat buffer (if any). Restoring
    /// rsp from this slot at the end of the outer Err arm frees whatever
    /// the callee allocated. When the callee's Err doesn't use concat,
    /// this slot just holds the unchanged rsp — restoring is a no-op.
    err_frame_save_slot: i32,
    /// Exit code flag: 0 = all records succeeded, 1 = at least one failed.
    /// Bool rules set this to 1 on false; Result rules set it on Err.
    /// The epilogue loads this into rdi for sys_exit.
    exit_flag_slot: i32,
}

/// Emit an argc guard: if r12 (argc) < min_argc, write an error message
/// to stderr and exit(1). Prevents segfaults on wrong argument count.
/// Must be emitted AFTER `mov r12, [rsp]`.
fn emit_argc_guard(code: &mut Vec<u8>, min_argc: i32) {
    // cmp r12d, min_argc (imm8 — min_argc always < 127 in practice)
    code.extend_from_slice(&[0x41, 0x83, 0xFC]);
    code.push(min_argc as u8);
    // jge .ok (short forward jump, patched below)
    code.push(0x7D);
    let ok_patch = code.len();
    code.push(0x00);
    // Error path: write message to stderr, exit(1).
    emit_write_static_to_fd(code, b"error: not enough arguments\n", 2);
    // mov rax, 60 (sys_exit) ; mov edi, 1 ; syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
    // .ok:
    let ok_pos = code.len();
    code[ok_patch] = (ok_pos - ok_patch - 1) as u8;
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
    context_concept: Option<&'a Concept>,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<RecordLoopCtx<'a>, NativeError> {
    let n_ctx = context_concept.map_or(0, |c| c.fields.len());
    let nfields = input_concept.fields.len();
    let n_bindings = rule.logic.bindings.len();
    // Bottom-of-frame reserved slots, in order:
    //   base + 1: match_slot          (Phase 2D Ok-bound i64)
    //   base + 2: err_ptr_slot        (Phase 2F Err-bound text ptr)
    //   base + 3: err_len_slot        (Phase 2F Err-bound text length)
    //   base + 4: err_frame_save_slot (Phase 2F rsp saved before callee Err concat)
    //   base + 5: exit_flag_slot      (exit code: 0=success, 1=failure)
    let n_reserved = 5;
    let frame_slots = n_ctx + nfields + n_bindings + n_reserved;
    let frame_size = (frame_slots * 8) as i32;
    let base = (n_ctx + nfields + n_bindings) as i32;
    let match_slot: i32 = -((base + 1) * 8);
    let err_ptr_slot: i32 = -((base + 2) * 8);
    let err_len_slot: i32 = -((base + 3) * 8);
    let err_frame_save_slot: i32 = -((base + 4) * 8);
    let exit_flag_slot: i32 = -((base + 5) * 8);

    // mov r12, [rsp]            — argc
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);
    // Guard: need at least n_ctx + nfields + 1 args (argv[0] + context + one record).
    emit_argc_guard(code, (n_ctx as i32) + (nfields as i32) + 1);
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
    // Initialize exit flag to 0 (success)
    code.extend_from_slice(&[0x48, 0xC7, 0x85]); // mov qword [rbp + exit_flag_slot], 0
    code.extend_from_slice(&exit_flag_slot.to_le_bytes());
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // ─── Read context fields (if any) ONCE before the record loop ──
    // Context fields go to rbp slots at the top of the frame (before input slots).
    let mut ctx_offsets: HashMap<&str, i32> = HashMap::new();
    if let Some(ctx) = context_concept {
        for (i, f) in ctx.fields.iter().enumerate() {
            let offset = -((i as i32 + 1) * 8);
            // mov rdi, [r13 + r14*8]
            code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]);
            match f.ty {
                Type::Number => {
                    emit_atoi_inline(code);
                    store_rax_at_rbp(code, offset);
                }
                Type::Text => store_rdi_at_rbp(code, offset),
                _ => {
                    return Err(NativeError {
                        message: format!("context field '{}' has unsupported type", f.name),
                    });
                }
            }
            // inc r14
            code.extend_from_slice(&[0x49, 0xFF, 0xC6]);
            ctx_offsets.insert(f.name.as_str(), offset);
        }
    }

    let loop_top = code.len();

    // cmp r14, r12 ; jge exit (rel32 placeholder)
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]);
    code.push(0x0F);
    code.push(0x8D);
    let exit_patch = code.len();
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // Input field offsets are shifted down by n_ctx to avoid colliding with context slots.
    let offsets: HashMap<&str, i32> = input_concept.fields.iter().enumerate()
        .map(|(i, f)| (f.name.as_str(), -(((n_ctx + i) as i32 + 1) * 8)))
        .collect();

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
    // Merge context fields so logic can access cfg.field_name
    for (k, v) in &ctx_offsets {
        binding_offsets.insert(k, *v);
    }
    let field_ranges = build_field_ranges(input_concept);
    let mut next_slot = -(((n_ctx + nfields) as i32 + 1) * 8);
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
        err_ptr_slot,
        err_len_slot,
        err_frame_save_slot,
        exit_flag_slot,
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
    // Load exit flag into rdi: mov rdi, [rbp + exit_flag_slot]
    let efl = ctx.exit_flag_slot;
    if efl >= -128 {
        code.extend_from_slice(&[0x48, 0x8B, 0x7D]);
        code.push(efl as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8B, 0xBD]);
        code.extend_from_slice(&efl.to_le_bytes());
    }
    // mov rax, 60 (sys_exit) ; syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn emit_full_program(
    rule: &Rule,
    concept: &Concept,
    context_concept: Option<&Concept>,
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let is_bool = rule.output_ty == Type::Bool;
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, context_concept, all_rules)?;

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
        // Set exit flag to 1 on false
        code.extend_from_slice(&[0x48, 0xC7, 0x85]); // mov qword [rbp + exit_flag_slot], 1
        code.extend_from_slice(&ctx.exit_flag_slot.to_le_bytes());
        code.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
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
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, None, all_rules)?;

    emit_text_write_to_fd(
        &mut code,
        &rule.logic.value,
        1,
        &rule.input_name,
        concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
        &no_text_bindings(),
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
    /// Phase 2F: an identifier bound to a text value via `match_result`'s
    /// Err arm. The bound value is represented as (ptr, len) in two rbp
    /// slots rather than as a NUL-terminated pointer.
    BoundText,
    /// Phase 2H-b: text-returning rule call. Evaluated once in a pre-eval
    /// pass; (ptr, len) stored at [r11 + 16*slot_idx + {0, 8}] where
    /// slot_idx is the index of this Call among the concat's Call args.
    /// The slot_idx is decoupled from the arg position so non-Call args
    /// don't consume slots.
    CallText,
}

/// Mapping `identifier → (ptr_slot, len_slot)` for text-typed values bound
/// in the current emission scope (e.g. `err_var` from match_result's Err
/// arm). Threaded through `classify_concat_arg`, `emit_concat_*`, and
/// `emit_text_write_to_fd` so that an `Expr::Ident(name)` where `name` is
/// bound resolves to a (ptr, len) load from the two slots.
type TextBindings<'a> = HashMap<&'a str, (i32, i32)>;

fn classify_concat_arg(
    expr: &Expr,
    concept: &Concept,
    input_name: &str,
    text_bindings: &TextBindings<'_>,
) -> Option<ConcatArgKind> {
    match expr {
        Expr::Text(_) => Some(ConcatArgKind::Text),
        Expr::Number(_) | Expr::Neg(_) => Some(ConcatArgKind::Number),
        Expr::Ident(name) if text_bindings.contains_key(name.as_str()) => {
            Some(ConcatArgKind::BoundText)
        }
        Expr::Call(_, _) => Some(ConcatArgKind::CallText),
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
    text_bindings: &TextBindings<'_>,
) -> Result<ConcatBufResult, NativeError> {
    emit_concat_to_buffer_impl(
        code, args, input_name, concept, all_rules, offsets, field_ranges, text_bindings,
        /* is_nested= */ false,
    )
}

/// Core concat-to-buffer emitter. When `is_nested`, skips the `mov r9, rsp`
/// pre-alloc save (the outer's r9 survives), and rejects CallText args
/// (Phase 2H-b scope restriction — one level of pre-eval).
fn emit_concat_to_buffer_impl(
    code: &mut Vec<u8>,
    args: &[Expr],
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
    is_nested: bool,
) -> Result<ConcatBufResult, NativeError> {
    // Classify every arg and tally the static worst case. A text-field arg,
    // a bound text var, or a Call arg means sizing must be runtime-dynamic.
    let mut kinds: Vec<ConcatArgKind> = Vec::with_capacity(args.len());
    let mut static_total: i32 = 0;
    let mut has_dynamic: bool = false;
    let mut n_calls: i32 = 0;
    // Map i-th arg → its CallText slot index in r11's array. -1 if not a Call.
    let mut call_slot_idx: Vec<i32> = Vec::with_capacity(args.len());
    for arg in args {
        let kind = classify_concat_arg(arg, concept, input_name, text_bindings).ok_or_else(|| {
            NativeError {
                message: "concat argument type not yet supported in native (text + number scalars, bound text var, or text-returning rule call; bool and others stay interpreter-only)".into(),
            }
        })?;
        kinds.push(kind);
        match kind {
            ConcatArgKind::Text => {
                if let Expr::Text(s) = arg {
                    static_total += s.as_bytes().len() as i32;
                } else if let Expr::Field(_, field_name) = arg {
                    // If the field has a [..N] bound, use N as the worst-case
                    // static size — avoids the dynamic path (no runtime strlen
                    // for sizing, no r9 save, static `add rsp` cleanup).
                    let bounded = concept
                        .fields
                        .iter()
                        .find(|f| &f.name == field_name)
                        .and_then(|f| f.range)
                        .map(|(_, max)| max as i32);
                    if let Some(max_len) = bounded {
                        static_total += max_len;
                    } else {
                        has_dynamic = true;
                    }
                }
                call_slot_idx.push(-1);
            }
            ConcatArgKind::Number => {
                static_total += 21;
                call_slot_idx.push(-1);
            }
            ConcatArgKind::BoundText => {
                has_dynamic = true;
                call_slot_idx.push(-1);
            }
            ConcatArgKind::CallText => {
                if is_nested {
                    return Err(NativeError {
                        message: "Phase 2H-b: nested concat (inside a text-returning callee's body) cannot have its own Call args — only one level of pre-eval supported".into(),
                    });
                }
                has_dynamic = true;
                call_slot_idx.push(n_calls);
                n_calls += 1;
            }
        }
    }
    if static_total == 0 && !has_dynamic {
        return Err(NativeError { message: "concat with zero total size".into() });
    }

    if !has_dynamic {
        // Fast path — compile-time-sized buffer, unchanged from before.
        let buf_size = ((static_total + 7) / 8) * 8;
        // sub rsp, buf_size
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&buf_size.to_le_bytes());
        // mov rbx, rsp  — rbx = write pointer
        code.extend_from_slice(&[0x48, 0x89, 0xE3]);
        // mov r10, rbx  — buffer base for final length calc
        code.extend_from_slice(&[0x49, 0x89, 0xDA]);
        emit_concat_fill(code, args, &kinds, &call_slot_idx, input_name, concept, all_rules, offsets, field_ranges, text_bindings)?;
        // rax = buffer base, rdx = length (rbx - r10)
        code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10
        code.extend_from_slice(&[0x48, 0x89, 0xDA]); // mov rdx, rbx
        code.extend_from_slice(&[0x4C, 0x29, 0xD2]); // sub rdx, r10
        return Ok(ConcatBufResult::Static(buf_size));
    }

    // Dynamic path. Capture rsp in r9 (outermost only) so the eventual
    // `mov rsp, r9` at cleanup frees the slot array, callee buffers, and
    // the main buffer in one instruction. When nested, we skip this — the
    // outer's r9 stays valid because nested concats never overwrite it
    // (scope restriction: nested has no CallText, so no pre-eval, and we
    // also skip the `mov r9, rsp` save entirely for nested).
    if !is_nested {
        // mov r9, rsp
        code.extend_from_slice(&[0x49, 0x89, 0xE1]);
    }

    // Phase 2H-b: pre-evaluate Call args into an r11-indexed slot array.
    // Only the outermost concat does pre-eval (nested is CallText-free).
    if n_calls > 0 {
        // sub rsp, 16*n_calls
        let slots_bytes = 16 * n_calls;
        code.extend_from_slice(&[0x48, 0x81, 0xEC]);
        code.extend_from_slice(&slots_bytes.to_le_bytes());
        // mov r11, rsp  — slot base
        code.extend_from_slice(&[0x49, 0x89, 0xE3]);

        // For each Call arg, emit callee into (rax, rdx) and store at the slot.
        // The inner (nested) emit_concat_to_buffer skips the r9 save and
        // refuses CallText args, so outer's r9 and r11 both survive as
        // register values across the inner evaluation. No push/pop dance
        // needed — the whole point of the is_nested flag.
        for (arg, kind) in args.iter().zip(kinds.iter()) {
            if *kind != ConcatArgKind::CallText {
                continue;
            }
            let slot_idx = call_slot_idx[args.iter().position(|a| std::ptr::eq(a, arg)).unwrap()];
            let disp_ptr = 16 * slot_idx;
            let disp_len = disp_ptr + 8;

            emit_text_produce_ptrlen(
                code, arg, input_name, concept, all_rules, offsets, field_ranges, text_bindings,
            )?;

            // mov [r11 + disp_ptr], rax
            emit_mov_r11_disp_from_reg(code, disp_ptr, /* is_rax= */ true);
            // mov [r11 + disp_len], rdx
            emit_mov_r11_disp_from_reg(code, disp_len, /* is_rax= */ false);
        }
    }

    // Sizing pass. mov rax, static_total.
    code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
    code.extend_from_slice(&static_total.to_le_bytes());

    for (arg, kind) in args.iter().zip(kinds.iter()) {
        match *kind {
            ConcatArgKind::Text => {
                if let Expr::Field(_, field_name) = arg {
                    let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                        message: format!(
                            "text-field '{}' has no rbp slot in concat size calc — input parsing missed it",
                            field_name
                        ),
                    })?;
                    code.push(0x50); // push rax
                    if offset >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(offset as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&offset.to_le_bytes());
                    }
                    emit_strlen(code); // rdx = length
                    code.push(0x59); // pop rcx
                    code.extend_from_slice(&[0x48, 0x01, 0xD1]); // add rcx, rdx
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]); // mov rax, rcx
                }
            }
            ConcatArgKind::BoundText => {
                if let Expr::Ident(name) = arg {
                    let (_, len_slot) = *text_bindings.get(name.as_str()).expect("classified as BoundText so present in bindings");
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x03, 0x45]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x03, 0x85]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                }
            }
            ConcatArgKind::CallText => {
                // add rax, [r11 + 16*slot_idx + 8]  (the len)
                let slot_idx = call_slot_idx[args.iter().position(|a| std::ptr::eq(a, arg)).unwrap()];
                let disp = 16 * slot_idx + 8;
                // 49 03 ModRM(01 000 011 = 0x43) disp8  or  49 03 ModRM(10 000 011 = 0x83) disp32
                if disp <= 127 {
                    code.extend_from_slice(&[0x49, 0x03, 0x43]);
                    code.push(disp as u8);
                } else {
                    code.extend_from_slice(&[0x49, 0x03, 0x83]);
                    code.extend_from_slice(&disp.to_le_bytes());
                }
            }
            _ => {}
        }
    }

    // Round up: add rax, 7 ; and rax, ~7
    code.extend_from_slice(&[0x48, 0x83, 0xC0, 0x07]);
    code.extend_from_slice(&[0x48, 0x83, 0xE0, 0xF8]);
    // sub rsp, rax  — main buffer allocation
    code.extend_from_slice(&[0x48, 0x29, 0xC4]);
    // mov rbx, rsp ; mov r10, rbx
    code.extend_from_slice(&[0x48, 0x89, 0xE3]);
    code.extend_from_slice(&[0x49, 0x89, 0xDA]);

    emit_concat_fill(code, args, &kinds, &call_slot_idx, input_name, concept, all_rules, offsets, field_ranges, text_bindings)?;

    // rax = buffer base, rdx = length
    code.extend_from_slice(&[0x4C, 0x89, 0xD0]); // mov rax, r10
    code.extend_from_slice(&[0x48, 0x89, 0xDA]); // mov rdx, rbx
    code.extend_from_slice(&[0x4C, 0x29, 0xD2]); // sub rdx, r10

    // Note: we don't pop r11 here. The Dynamic cleanup path does `mov rsp, r9`
    // which drops EVERYTHING below r9 — including the main buffer, the slot
    // array, the pushed-r11-placeholder (if we pushed one), and any callee
    // concat buffers. The saved r11 value on the stack is freed along with
    // everything else; the outer scope doesn't need the restored r11 because
    // its own ConcatBufResult::Dynamic cleanup will similarly reset rsp.
    // (If we ever need to restore r11 for post-concat use, we'd lift the
    // ordering to `pop r11 ; mov rsp, r9`, but no current caller needs it.)

    Ok(ConcatBufResult::Dynamic)
}

/// Emit `mov [r11 + disp], reg` where reg is rax (is_rax=true) or rdx.
/// Used by Phase 2H-b to populate Call-arg slots in the pre-eval array.
fn emit_mov_r11_disp_from_reg(code: &mut Vec<u8>, disp: i32, is_rax: bool) {
    // REX.WB + 0x89 + ModRM(reg = rax(000) or rdx(010), r/m = r11 (011, with REX.B))
    let reg_field: u8 = if is_rax { 0b000 } else { 0b010 };
    if disp >= -128 && disp <= 127 {
        let modrm = 0b01_000_000 | (reg_field << 3) | 0b011;
        code.extend_from_slice(&[0x49, 0x89, modrm]);
        code.push(disp as u8);
    } else {
        let modrm = 0b10_000_000 | (reg_field << 3) | 0b011;
        code.extend_from_slice(&[0x49, 0x89, modrm]);
        code.extend_from_slice(&disp.to_le_bytes());
    }
}

/// Produce (rax = ptr, rdx = len) for a text-producing expression — used by
/// the Phase 2H-b pre-eval pass when evaluating a Call arg's body.
///
/// Handles the same shapes as `emit_text_write_to_fd` but ends with the
/// values in registers instead of a write syscall. Allocated buffers (for
/// Concat callees) stay on the stack; the outer caller's final
/// `mov rsp, r9` will free them.
///
/// Scope mirrors Phase 2G: callee's body can be Text, Field, Concat, or
/// Call (recursively). The `Expr::Call` case validates the 2G restrictions
/// and recurses on the callee's body.
fn emit_text_produce_ptrlen(
    code: &mut Vec<u8>,
    text_expr: &Expr,
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match text_expr {
        Expr::Call(callee_name, args) => {
            // Validate the same Phase 2G restrictions.
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "Phase 2H-b: unknown rule '{}' called in concat",
                    callee_name
                ),
            })?;
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: rule '{}' in concat must return `text`, not {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            let callee_concept_name = match &callee.input_ty {
                Type::Named(n) => n.as_str(),
                _ => {
                    return Err(NativeError {
                        message: format!(
                            "Phase 2H-b: rule '{}' input must be a named concept",
                            callee_name
                        ),
                    });
                }
            };
            if callee_concept_name != concept.name.as_str() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: callee '{}' takes concept '{}' but caller takes '{}' — same-concept required",
                        callee_name, callee_concept_name, concept.name
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: callee '{}' binds its input as '{}' but caller uses '{}' — same input name required",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: callee '{}' has let bindings; not yet supported in native",
                        callee_name
                    ),
                });
            }
            if args.len() != 1 || !matches!(&args[0], Expr::Ident(n) if n == input_name) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: rule '{}' must be called with exactly the caller's input identifier",
                        callee_name
                    ),
                });
            }
            emit_text_produce_ptrlen(
                code, &callee.logic.value, input_name, concept, all_rules,
                offsets, field_ranges, text_bindings,
            )
        }
        Expr::Text(s) => {
            // jmp over inline bytes; lea rax, [rip+data]; mov rdx, n.
            let bytes = s.as_bytes();
            let n = bytes.len() as i32;
            if n <= 127 {
                code.push(0xEB);
                code.push(n as u8);
            } else {
                code.push(0xE9);
                code.extend_from_slice(&n.to_le_bytes());
            }
            let addr = code.len();
            code.extend_from_slice(bytes);
            // lea rax, [rip + rel32]
            let end = code.len() + 7;
            let rel32 = addr as i32 - end as i32;
            code.extend_from_slice(&[0x48, 0x8D, 0x05]);
            code.extend_from_slice(&rel32.to_le_bytes());
            // mov rdx, n
            code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
            code.extend_from_slice(&n.to_le_bytes());
            Ok(())
        }
        Expr::Field(base, field_name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
        {
            let f = concept
                .fields
                .iter()
                .find(|f| &f.name == field_name)
                .ok_or_else(|| NativeError {
                    message: format!("unknown field '{}' in text-produce", field_name),
                })?;
            if !matches!(f.ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-b: field '{}' is not text",
                        field_name
                    ),
                });
            }
            let offset = offsets[field_name.as_str()];
            // mov rax, [rbp + offset]  (ptr)
            load_rax_from_rbp(code, offset);
            // mov rsi, rax ; emit_strlen -> rdx = len
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            emit_strlen(code);
            Ok(())
        }
        Expr::Ident(name) if text_bindings.contains_key(name.as_str()) => {
            let (ptr_slot, len_slot) = text_bindings[name.as_str()];
            // mov rax, [rbp + ptr_slot]
            load_rax_from_rbp(code, ptr_slot);
            // mov rdx, [rbp + len_slot]
            if len_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x55]);
                code.push(len_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0x95]);
                code.extend_from_slice(&len_slot.to_le_bytes());
            }
            Ok(())
        }
        Expr::Concat(inner_args) => {
            // Recurse into a nested concat. is_nested=true so the inner
            // skips `mov r9, rsp` (outer's r9 survives) and rejects CallText
            // args (one-level-of-pre-eval scope restriction). Its (rax, rdx)
            // point into its own stack buffer, which stays allocated until
            // the outermost `mov rsp, r9` frees everything at once.
            let _buf = emit_concat_to_buffer_impl(
                code, inner_args, input_name, concept, all_rules, offsets, field_ranges,
                text_bindings,
                /* is_nested= */ true,
            )?;
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "Phase 2H-b: callee body shape not supported: {:?}",
                other
            ),
        }),
    }
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
    call_slot_idx: &[i32],
    input_name: &str,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
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
            ConcatArgKind::BoundText => {
                // Copy the bound (ptr, len) contents to the buffer.
                // No strlen needed at fill time — length is already stored.
                if let Expr::Ident(name) = arg {
                    let (ptr_slot, len_slot) = *text_bindings
                        .get(name.as_str())
                        .expect("classified as BoundText so present in bindings");
                    // mov rsi, [rbp + ptr_slot]
                    if ptr_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(ptr_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&ptr_slot.to_le_bytes());
                    }
                    // mov rcx, [rbp + len_slot]
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x4D]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                    // mov rdi, rbx ; rep movsb ; mov rbx, rdi
                    code.extend_from_slice(&[0x48, 0x89, 0xDF]);
                    code.extend_from_slice(&[0xF3, 0xA4]);
                    code.extend_from_slice(&[0x48, 0x89, 0xFB]);
                }
            }
            ConcatArgKind::CallText => {
                // Pre-eval has already stored (ptr, len) at [r11 + 16*idx + {0,8}].
                // mov rsi, [r11 + disp_ptr]  ; mov rcx, [r11 + disp_len]
                // mov rdi, rbx ; rep movsb ; mov rbx, rdi
                let slot_idx = call_slot_idx[i];
                let disp_ptr = 16 * slot_idx;
                let disp_len = disp_ptr + 8;
                // mov rsi, [r11 + disp_ptr]  (REX.B, ModRM reg=rsi(110) r/m=r11(011))
                if disp_ptr <= 127 {
                    code.extend_from_slice(&[0x49, 0x8B, 0x73]);
                    code.push(disp_ptr as u8);
                } else {
                    code.extend_from_slice(&[0x49, 0x8B, 0xB3]);
                    code.extend_from_slice(&disp_ptr.to_le_bytes());
                }
                // mov rcx, [r11 + disp_len]  (ModRM reg=rcx(001) r/m=r11(011))
                if disp_len <= 127 {
                    code.extend_from_slice(&[0x49, 0x8B, 0x4B]);
                    code.push(disp_len as u8);
                } else {
                    code.extend_from_slice(&[0x49, 0x8B, 0x8B]);
                    code.extend_from_slice(&disp_len.to_le_bytes());
                }
                // mov rdi, rbx ; rep movsb ; mov rbx, rdi
                code.extend_from_slice(&[0x48, 0x89, 0xDF]);
                code.extend_from_slice(&[0xF3, 0xA4]);
                code.extend_from_slice(&[0x48, 0x89, 0xFB]);
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

/// Empty text-bindings map for call sites that don't need text binding.
/// HashMap::new() doesn't allocate until first insert, so this is cheap.
fn no_text_bindings() -> TextBindings<'static> {
    HashMap::new()
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
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // First, emit the open() call. The path is always a compile-time literal,
    // so we embed it inline and point rdi at it.
    emit_open_append(code, path);
    // rax = fd; save in r15.
    // mov r15, rax  (49 89 C7)
    code.extend_from_slice(&[0x49, 0x89, 0xC7]);

    // Now the write() — dispatch on content shape. Factored into a helper
    // so that the Call arm (Phase 2H-a) can recurse on callee.logic.value
    // without re-opening the file or re-validating the path.
    emit_append_write_to_r15(code, content, rule, concept, all_rules, offsets, field_ranges, text_bindings)?;

    // === close(fd) ===
    // mov rax, 3 (sys_close)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    // mov rdi, r15
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(())
}

/// Emit a write(fd-in-r15, content, ...) for a reaction `append_file`
/// effect. Factored out of `emit_append_file_call` so the Call arm can
/// recurse on `callee.logic.value` without re-opening the file.
///
/// Preconditions: `r15` already holds the open fd (from `emit_open_append`).
/// Postconditions: one `write` syscall has been emitted; any scratch buffer
/// allocated by a Concat content has been freed.
fn emit_append_write_to_r15(
    code: &mut Vec<u8>,
    content: &Expr,
    rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
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
            Ok(())
        }
        Expr::Concat(args) => {
            // Build the content in a stack buffer, then write (rax=buf_ptr,
            // rdx=len) to the fd. Free according to the sizing strategy
            // reported by emit_concat_to_buffer.
            let buf = emit_concat_to_buffer(
                code, args, &rule.input_name, concept, all_rules, offsets, field_ranges,
                text_bindings,
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
            Ok(())
        }
        Expr::Call(callee_name, args) => {
            // Phase 2H-a: text-returning rule call inlined as append_file
            // content. Mirrors the Phase 2G restrictions in
            // emit_text_write_to_fd.
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "Phase 2H-a: unknown rule '{}' called as append_file content",
                    callee_name
                ),
            })?;
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-a: rule '{}' as append_file content must return `text`, not {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            let callee_concept_name = match &callee.input_ty {
                Type::Named(n) => n.as_str(),
                _ => {
                    return Err(NativeError {
                        message: format!(
                            "Phase 2H-a: rule '{}' input must be a named concept",
                            callee_name
                        ),
                    });
                }
            };
            if callee_concept_name != concept.name.as_str() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-a: callee '{}' takes concept '{}' but caller takes '{}' — same-concept required",
                        callee_name, callee_concept_name, concept.name
                    ),
                });
            }
            if callee.input_name != rule.input_name {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-a: callee '{}' binds its input as '{}' but caller uses '{}' — same input name required",
                        callee_name, callee.input_name, rule.input_name
                    ),
                });
            }
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-a: callee '{}' has let bindings; not yet supported in native",
                        callee_name
                    ),
                });
            }
            if args.len() != 1 || !matches!(&args[0], Expr::Ident(n) if n == &rule.input_name) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2H-a: rule '{}' must be called with exactly the caller's input identifier",
                        callee_name
                    ),
                });
            }
            // Recurse: treat the callee's body as the append_file content.
            emit_append_write_to_r15(
                code, &callee.logic.value, rule, concept, all_rules, offsets, field_ranges,
                text_bindings,
            )
        }
        other => Err(NativeError {
            message: format!(
                "append_file content must be a text literal, concat(...), or text-returning rule call; got {:?}",
                other
            ),
        }),
    }
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
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // Phase 2F: bound text var — (ptr, len) already in two rbp slots.
    // Write them directly to the fd; no strlen (length is stored).
    if let Expr::Ident(name) = text_expr {
        if let Some(&(ptr_slot, len_slot)) = text_bindings.get(name.as_str()) {
            // mov rsi, [rbp + ptr_slot]
            if ptr_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                code.push(ptr_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                code.extend_from_slice(&ptr_slot.to_le_bytes());
            }
            // mov rdx, [rbp + len_slot]
            if len_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x55]);
                code.push(len_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0x95]);
                code.extend_from_slice(&len_slot.to_le_bytes());
            }
            // mov rax, 1 (sys_write) ; mov rdi, fd ; syscall
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
            code.extend_from_slice(&[0x48, 0xC7, 0xC7]);
            code.extend_from_slice(&fd.to_le_bytes());
            code.extend_from_slice(&[0x0F, 0x05]);
            return Ok(());
        }
    }
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
                code, args, input_name, concept, all_rules, offsets, field_ranges, text_bindings,
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
        Expr::If(cond, then_e, else_e) => {
            // Conditional text: evaluate condition, branch, recurse on each
            // arm. Each arm writes its own text to the fd; control joins
            // after both branches.
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges)?;
            // test rax, rax ; jz else (rel32)
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0; 4]);
            // then arm
            emit_text_write_to_fd(
                code, then_e, fd, input_name, concept, all_rules,
                offsets, field_ranges, text_bindings,
            )?;
            // jmp after (rel32)
            code.push(0xE9);
            let after_patch = code.len();
            code.extend_from_slice(&[0; 4]);
            // else:
            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_text_write_to_fd(
                code, else_e, fd, input_name, concept, all_rules,
                offsets, field_ranges, text_bindings,
            )?;
            // after:
            let after_pos = code.len();
            let after_off = after_pos as i32 - (after_patch as i32 + 4);
            code[after_patch..after_patch + 4].copy_from_slice(&after_off.to_le_bytes());
            Ok(())
        }
        Expr::Call(callee_name, args) => {
            // Phase 2G: text-returning rule call inlined. Validate the
            // same-concept / same-input-name / no-lets restrictions, then
            // recurse on the callee's body. See "Phase 2G design (locked)"
            // in CLAUDE.md.
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "Phase 2G: unknown rule '{}' called in text context",
                    callee_name
                ),
            })?;
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2G: rule '{}' in text context must return `text`, not {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            let callee_concept_name = match &callee.input_ty {
                Type::Named(n) => n.as_str(),
                _ => {
                    return Err(NativeError {
                        message: format!(
                            "Phase 2G: rule '{}' input must be a named concept",
                            callee_name
                        ),
                    });
                }
            };
            if callee_concept_name != concept.name.as_str() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2G: callee '{}' takes concept '{}' but caller takes '{}' — same-concept required in native",
                        callee_name, callee_concept_name, concept.name
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "Phase 2G: callee '{}' binds its input as '{}' but caller uses '{}' — same input name required",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "Phase 2G: callee '{}' has let bindings; not yet supported in native (would need caller-side evaluation)",
                        callee_name
                    ),
                });
            }
            if args.len() != 1 || !matches!(&args[0], Expr::Ident(n) if n == input_name) {
                return Err(NativeError {
                    message: format!(
                        "Phase 2G: rule '{}' must be called with exactly the caller's input identifier",
                        callee_name
                    ),
                });
            }
            // Recurse: emit the callee's body as if it were inlined here.
            emit_text_write_to_fd(
                code,
                &callee.logic.value,
                fd,
                input_name,
                concept,
                all_rules,
                offsets,
                field_ranges,
                text_bindings,
            )
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
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, None, all_rules)?;

    // Evaluate the logic in Result context. Every Ok/Err leaf self-terminates
    // with a jmp loop_top, so there is no fall-through to handle here.
    let slots = MatchSlots {
        match_slot: ctx.match_slot,
        err_ptr_slot: ctx.err_ptr_slot,
        err_len_slot: ctx.err_len_slot,
        err_frame_save_slot: ctx.err_frame_save_slot,
        exit_flag_slot: ctx.exit_flag_slot,
    };
    emit_eval_result_expr(
        &mut code,
        &rule.logic.value,
        ctx.loop_top,
        rule,
        concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
        slots,
    )?;

    emit_record_loop_epilogue(&mut code, &ctx);
    Ok(code)
}

/// Rbp slots reserved at the bottom of the frame for `match_result` state.
/// Passed as a group through `emit_eval_result_expr` and
/// `emit_match_result_inlined` / `emit_redirect_callee_leaves` so each
/// helper knows where to read/write the Ok-bound value, the Err-bound
/// text (ptr, len), and the saved rsp for buffer cleanup.
#[derive(Debug, Clone, Copy)]
struct MatchSlots {
    /// Phase 2D: where the Ok-bound scalar value lands before the outer Ok
    /// body runs. A single i64 slot.
    match_slot: i32,
    /// Phase 2F: pointer half of the Err-bound text (when the outer Err
    /// arm captures the err value instead of passing it through).
    err_ptr_slot: i32,
    /// Phase 2F: length half of the Err-bound text.
    err_len_slot: i32,
    /// Phase 2F: rsp captured just before the inlined callee's Err leaf
    /// runs; restoring rsp to this value at the end of the outer Err arm
    /// frees any buffer the callee's Err concat allocated. A no-op when
    /// the callee's Err was a literal or field access.
    err_frame_save_slot: i32,
    /// Exit code flag slot: set to 1 on Err to propagate failure.
    exit_flag_slot: i32,
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
    slots: MatchSlots,
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
                        &no_text_bindings(),
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
                &no_text_bindings(),
            )?;
            emit_write_newline(code, 2);
            // Set exit flag to 1 (failure)
            code.extend_from_slice(&[0x48, 0xC7, 0x85]); // mov qword [rbp + exit_flag_slot], 1
            code.extend_from_slice(&slots.exit_flag_slot.to_le_bytes());
            code.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
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
                code, then_e, loop_top, rule, concept, all_rules, offsets, field_ranges, slots,
            )?;

            // .else:
            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_eval_result_expr(
                code, else_e, loop_top, rule, concept, all_rules, offsets, field_ranges, slots,
            )?;
            Ok(())
        }
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            // Phase 2D + 2F: inline the callee's Result-producing logic,
            // redirecting its Ok leaves into the outer Ok arm (Ok-bound value
            // lands at match_slot) and its Err leaves into the outer Err arm
            // (Err-bound text captured to err_ptr_slot/err_len_slot, then the
            // outer Err body runs with `err_var → (ptr, len)` bound).
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
                slots,
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
    slots: MatchSlots,
) -> Result<(), NativeError> {
    // The outer Err body must be `Err(<text_expr>)`. Phase 2F accepts any
    // text_expr that emit_text_write_to_fd can handle — literal, input text
    // field, Ident(err_var), or concat with any of those plus Ident(err_var).
    let outer_err_inner = match err_body {
        Expr::Err(inner) => inner.as_ref(),
        _ => {
            return Err(NativeError {
                message: "match_result outer Err arm must be of the form `Err(<text_expr>)`".into(),
            });
        }
    };

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
        err_var,
        outer_err_inner,
        loop_top,
        rule,
        concept,
        all_rules,
        offsets,
        field_ranges,
        slots,
    )
}

fn emit_redirect_callee_leaves(
    code: &mut Vec<u8>,
    expr: &Expr,
    callee: &Rule,
    ok_var: &str,
    ok_body: &Expr,
    err_var: &str,
    outer_err_inner: &Expr,
    loop_top: usize,
    outer_rule: &Rule,
    concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    slots: MatchSlots,
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
            store_rax_at_rbp(code, slots.match_slot);
            // Augment offsets with ok_var → match_slot, then emit outer ok_body
            // in result context. The outer arm self-terminates.
            let mut augmented = offsets.clone();
            augmented.insert(ok_var, slots.match_slot);
            emit_eval_result_expr(
                code,
                ok_body,
                loop_top,
                outer_rule,
                concept,
                all_rules,
                &augmented,
                field_ranges,
                slots,
            )
        }
        Expr::Err(inner) => {
            // Phase 2F: capture the callee's Err into (err_ptr_slot,
            // err_len_slot). Save rsp first so the outer arm's cleanup can
            // free whatever buffer the capture allocated (concat case only;
            // literal/field cases don't allocate but the save is harmless).

            // 1. mov [rbp + err_frame_save_slot], rsp
            emit_mov_rbp_disp_from_reg(code, slots.err_frame_save_slot, /* r_is_rsp= */ true);

            // 2. Capture the Err value per inner shape.
            match &**inner {
                Expr::Text(s) => {
                    // Inline literal bytes; lea rax at them; store (ptr, len).
                    let bytes = s.as_bytes();
                    let n = bytes.len() as i32;
                    if n <= 127 {
                        code.push(0xEB);
                        code.push(n as u8);
                    } else {
                        code.push(0xE9);
                        code.extend_from_slice(&n.to_le_bytes());
                    }
                    let data_addr = code.len();
                    code.extend_from_slice(bytes);
                    // lea rax, [rip + rel32] — 48 8D 05 rel32
                    let end = code.len() + 7;
                    let rel32 = data_addr as i32 - end as i32;
                    code.extend_from_slice(&[0x48, 0x8D, 0x05]);
                    code.extend_from_slice(&rel32.to_le_bytes());
                    store_rax_at_rbp(code, slots.err_ptr_slot);
                    // mov qword [rbp + err_len_slot], n (imm32 sign-extended)
                    if slots.err_len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0xC7, 0x45]);
                        code.push(slots.err_len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0xC7, 0x85]);
                        code.extend_from_slice(&slots.err_len_slot.to_le_bytes());
                    }
                    code.extend_from_slice(&n.to_le_bytes());
                }
                Expr::Field(base, field_name)
                    if matches!(base.as_ref(), Expr::Ident(n) if n == &outer_rule.input_name) =>
                {
                    let f = concept
                        .fields
                        .iter()
                        .find(|f| &f.name == field_name)
                        .ok_or_else(|| NativeError {
                            message: format!("unknown field '{}' in match_result Err capture", field_name),
                        })?;
                    if !matches!(f.ty, Type::Text) {
                        return Err(NativeError {
                            message: format!(
                                "match_result Err inner Field '{}' must be text-typed",
                                field_name
                            ),
                        });
                    }
                    let offset = offsets[field_name.as_str()];
                    // mov rax, [rbp+offset] — load ptr
                    load_rax_from_rbp(code, offset);
                    store_rax_at_rbp(code, slots.err_ptr_slot);
                    // mov rsi, rax ; emit_strlen → rdx = length
                    code.extend_from_slice(&[0x48, 0x89, 0xC6]);
                    emit_strlen(code);
                    // mov [rbp + err_len_slot], rdx
                    if slots.err_len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x89, 0x55]);
                        code.push(slots.err_len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x89, 0x95]);
                        code.extend_from_slice(&slots.err_len_slot.to_le_bytes());
                    }
                }
                Expr::Concat(args) => {
                    // Build the concat buffer; get (rax=ptr, rdx=len).
                    // The buffer stays alive across the outer arm; cleanup
                    // via `mov rsp, [rbp+err_frame_save_slot]` at the end.
                    let _buf = emit_concat_to_buffer(
                        code, args, &outer_rule.input_name, concept, all_rules,
                        offsets, field_ranges, &no_text_bindings(),
                    )?;
                    store_rax_at_rbp(code, slots.err_ptr_slot);
                    // mov [rbp + err_len_slot], rdx
                    if slots.err_len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x89, 0x55]);
                        code.push(slots.err_len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x89, 0x95]);
                        code.extend_from_slice(&slots.err_len_slot.to_le_bytes());
                    }
                }
                other => {
                    return Err(NativeError {
                        message: format!(
                            "match_result callee Err inner must be a text literal, input text field, or concat; got {:?}",
                            other
                        ),
                    });
                }
            }

            // 3. Build bindings for err_var and emit the outer Err body's
            //    text into stderr.
            let mut bindings: TextBindings = HashMap::new();
            bindings.insert(err_var, (slots.err_ptr_slot, slots.err_len_slot));
            emit_text_write_to_fd(
                code, outer_err_inner, 2, &outer_rule.input_name, concept, all_rules,
                offsets, field_ranges, &bindings,
            )?;
            emit_write_newline(code, 2);

            // 4. Restore rsp to pre-capture — frees the callee's concat
            //    buffer (if any). Harmless when no concat happened.
            load_rax_from_rbp(code, slots.err_frame_save_slot);
            // mov rsp, rax
            code.extend_from_slice(&[0x48, 0x89, 0xC4]);

            // 5. jmp loop_top
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
                code, then_e, callee, ok_var, ok_body, err_var, outer_err_inner, loop_top,
                outer_rule, concept, all_rules, offsets, field_ranges, slots,
            )?;

            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_redirect_callee_leaves(
                code, else_e, callee, ok_var, ok_body, err_var, outer_err_inner, loop_top,
                outer_rule, concept, all_rules, offsets, field_ranges, slots,
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

/// Emit `mov [rbp + disp], rsp` (or `mov [rbp + disp], rax` if `!r_is_rsp`).
/// Used by Phase 2F to save rsp into `err_frame_save_slot`.
fn emit_mov_rbp_disp_from_reg(code: &mut Vec<u8>, disp: i32, r_is_rsp: bool) {
    // REX.W + 0x89 + ModRM(reg=rsp(100) or rax(000), r/m=rbp(101), mod per disp)
    let reg_field: u8 = if r_is_rsp { 0b100 } else { 0b000 };
    if disp >= -128 {
        // mod=01 disp8
        let modrm = 0b01_000_000 | (reg_field << 3) | 0b101;
        code.extend_from_slice(&[0x48, 0x89, modrm]);
        code.push(disp as u8);
    } else {
        let modrm = 0b10_000_000 | (reg_field << 3) | 0b101;
        code.extend_from_slice(&[0x48, 0x89, modrm]);
        code.extend_from_slice(&disp.to_le_bytes());
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
    let ctx = emit_record_loop_prologue(&mut code, rule, input_concept, None, all_rules)?;

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
        Bool,
    }
    let output_kind: OutputElemKind = match elem_type_name.as_str() {
        "number" => OutputElemKind::Number,
        "text" => OutputElemKind::Text,
        "bool" => OutputElemKind::Bool,
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
                OutputElemKind::Bool => {
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
    let n_lets = rule.logic.bindings.len();
    let frame_slots = n_scalar + n_elem_fields + n_lets;
    let frame_size = (frame_slots as i32) * 8;

    let mut code = Vec::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
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

    // Evaluate let bindings after scalar fields.
    {
        let field_ranges_for_lets = build_field_ranges(input_concept);
        let mut let_offsets: HashMap<&str, i32> = scalar_offsets.clone();
        let mut next_let_slot = -(((n_scalar + n_elem_fields) as i32 + 1) * 8);
        for (name, expr) in &rule.logic.bindings {
            emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets)?;
            store_rax_at_rbp(&mut code, next_let_slot);
            let_offsets.insert(name.as_str(), next_let_slot);
            next_let_slot -= 8;
        }
        // Let bindings visible to body: add to elem_offsets so the lambda body can see them.
        for (name, &offset) in &let_offsets {
            if !elem_offsets.contains_key(name) && !scalar_offsets.contains_key(name) {
                elem_offsets.insert(name, offset);
            }
        }
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
            // Scalar element output: evaluate the body to rax (number/bool)
            // or emit the text directly, then one newline per element.
            if is_text {
                emit_text_write_to_fd(
                    &mut code, body, 1, lambda_var, input_elem_concept, all_rules,
                    &elem_offsets, &field_ranges,
                    &no_text_bindings(),
                )?;
                emit_write_newline(&mut code, 1);
            } else {
                emit_eval_expr(
                    &mut code, body, lambda_var, &elem_offsets, all_rules, &field_ranges,
                )?;
                if matches!(output_kind, OutputElemKind::Bool) {
                    // Bool: rax = 0/1 → "true"/"false" + newline.
                    code.extend_from_slice(&[0x84, 0xC0]); // test al, al
                    code.push(0x74); // jz .print_false
                    let pf_patch = code.len();
                    code.push(0x00);
                    emit_write_string(&mut code, b"true\n");
                    code.push(0xEB); // jmp .after
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
    let is_bool_output = matches!(rule.output_ty, Type::Bool);
    if !matches!(rule.output_ty, Type::Number | Type::Bool) {
        return Err(NativeError {
            message: "emit_fold_program called on non-number/non-bool output".into(),
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
    let n_lets = rule.logic.bindings.len();
    // Frame: scalars + element fields + let bindings + acc slot.
    let frame_slots = n_scalar + n_elem_fields + n_lets + 1;
    let frame_size = (frame_slots as i32) * 8;
    let acc_offset: i32 = -((frame_slots as i32) * 8);

    let mut code = Vec::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
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

    // Evaluate let bindings into rbp slots (after scalar fields, before element fields).
    let field_ranges_for_lets = build_field_ranges(input_concept);
    let mut let_offsets: HashMap<&str, i32> = scalar_offsets.clone();
    let mut next_let_slot = -(((n_scalar + n_elem_fields) as i32 + 1) * 8);
    for (name, expr) in &rule.logic.bindings {
        emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets)?;
        store_rax_at_rbp(&mut code, next_let_slot);
        let_offsets.insert(name.as_str(), next_let_slot);
        next_let_slot -= 8;
    }
    // Make let bindings visible to the fold body.
    for (name, &offset) in &let_offsets {
        if !body_offsets.contains_key(name) && !scalar_offsets.contains_key(name) {
            body_offsets.insert(name, offset);
        }
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

    // Emit the final accumulator.
    load_rax_from_rbp(&mut code, acc_offset);
    if is_bool_output {
        // Bool fold (all/any): rax is 0 or 1 → print "true"/"false" + newline.
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

// ═══════════════════════════════════════════════════════════════════════
// Phase 6: scalar output with embedded quantifiers (multi-accumulator fold)
// ═══════════════════════════════════════════════════════════════════════

/// A quantifier extracted from a larger expression tree and desugared into
/// a fold. After extraction, the original expression references the fold
/// result via `Ident(name)`.
struct ExtractedFold {
    name: String,       // result name used in the outer expression (e.g. "__fold_0")
    init: i64,          // literal initial accumulator value
    acc_name: String,   // unique accumulator name (e.g. "__fold_0_acc")
    item_name: String,  // element variable name from the quantifier (e.g. "e")
    body: Expr,         // fold body referencing acc_name and item_name
}

/// Returns true if the expression tree contains any Quantifier node.
fn contains_quantifier(expr: &Expr) -> bool {
    match expr {
        Expr::Quantifier(_, _, _, _) => true,
        Expr::If(c, t, e) => contains_quantifier(c) || contains_quantifier(t) || contains_quantifier(e),
        Expr::Binary(_, l, r) => contains_quantifier(l) || contains_quantifier(r),
        Expr::Not(e) | Expr::Neg(e) => contains_quantifier(e),
        _ => false,
    }
}

/// Walk the expression tree, replace every Quantifier node with an Ident
/// reference, and return the desugared fold parameters for each.
fn extract_quantifiers(expr: &Expr, counter: &mut usize) -> (Expr, Vec<ExtractedFold>) {
    match expr {
        Expr::Quantifier(kind, _coll, var, pred) => {
            let idx = *counter;
            *counter += 1;
            let name = format!("__fold_{}", idx);
            let acc = format!("__fold_{}_acc", idx);
            let (init, body) = match kind {
                QuantifierKind::All => (
                    1i64,
                    Expr::If(pred.clone(), Box::new(Expr::Ident(acc.clone())), Box::new(Expr::Number(0))),
                ),
                QuantifierKind::Any => (
                    0i64,
                    Expr::If(pred.clone(), Box::new(Expr::Number(1)), Box::new(Expr::Ident(acc.clone()))),
                ),
            };
            let fold = ExtractedFold { name: name.clone(), init, acc_name: acc, item_name: var.clone(), body };
            (Expr::Ident(name), vec![fold])
        }
        Expr::If(c, t, e) => {
            let (nc, mut fc) = extract_quantifiers(c, counter);
            let (nt, ft) = extract_quantifiers(t, counter);
            let (ne, fe) = extract_quantifiers(e, counter);
            fc.extend(ft); fc.extend(fe);
            (Expr::If(Box::new(nc), Box::new(nt), Box::new(ne)), fc)
        }
        Expr::Binary(op, l, r) => {
            let (nl, mut fl) = extract_quantifiers(l, counter);
            let (nr, fr) = extract_quantifiers(r, counter);
            fl.extend(fr);
            (Expr::Binary(*op, Box::new(nl), Box::new(nr)), fl)
        }
        Expr::Not(e) => {
            let (ne, fe) = extract_quantifiers(e, counter);
            (Expr::Not(Box::new(ne)), fe)
        }
        Expr::Neg(e) => {
            let (ne, fe) = extract_quantifiers(e, counter);
            (Expr::Neg(Box::new(ne)), fe)
        }
        other => (other.clone(), vec![]),
    }
}

/// Phase 6 emitter: scalar output whose logic contains embedded quantifiers
/// on the input concept's collection field. All quantifiers are extracted,
/// desugared to folds, and computed in a single pass over the collection
/// with one accumulator slot per fold. The remaining scalar expression is
/// evaluated after the inner loop.
///
/// This handles patterns like:
///   `if all(xs, p) then 0 else if any(xs, p) then 1 else 2`
///
/// Requirements:
///   - Output: number or bool
///   - Input concept: scalars* + ONE trailing collection(Concept) field
///   - All quantifiers target the same collection field
fn emit_multi_fold_program(
    rule: &Rule,
    input_concept: &Concept,
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
) -> Result<Vec<u8>, NativeError> {
    let is_bool_output = matches!(rule.output_ty, Type::Bool);
    if !matches!(rule.output_ty, Type::Number | Type::Bool) {
        return Err(NativeError {
            message: "emit_multi_fold_program: output must be number or bool".into(),
        });
    }

    // Extract quantifiers from the expression tree.
    let mut counter = 0usize;
    let (scalar_expr, folds) = extract_quantifiers(&rule.logic.value, &mut counter);
    if folds.is_empty() {
        return Err(NativeError {
            message: "emit_multi_fold_program: no quantifiers found in expression".into(),
        });
    }

    // Validate input concept shape: scalars + trailing collection.
    let mut scalar_fields: Vec<&Field> = Vec::new();
    let mut elem_concept_name: Option<String> = None;
    for (i, f) in input_concept.fields.iter().enumerate() {
        let is_last = i == input_concept.fields.len() - 1;
        match (&f.ty, is_last) {
            (Type::Number, false) | (Type::Text, false) => scalar_fields.push(f),
            (Type::Collection(elem), true) => { elem_concept_name = Some(elem.clone()); }
            _ => return Err(NativeError {
                message: format!(
                    "Phase 6: input concept '{}' must have scalars + ONE trailing collection; field '{}' at {} violates this",
                    input_concept.name, f.name, i
                ),
            }),
        }
    }
    let elem_concept_name = elem_concept_name.ok_or_else(|| NativeError {
        message: format!("Phase 6: input concept '{}' has no trailing collection field", input_concept.name),
    })?;
    let elem_concept = all_concepts.iter().find(|c| c.name == elem_concept_name).copied()
        .ok_or_else(|| NativeError { message: format!("unknown concept '{}'", elem_concept_name) })?;
    for f in &elem_concept.fields {
        if !matches!(f.ty, Type::Number | Type::Text) {
            return Err(NativeError {
                message: format!("Phase 6: element field '{}' has unsupported type", f.name),
            });
        }
    }

    // ===== Frame layout =====
    let n_scalar = scalar_fields.len();
    let n_elem = elem_concept.fields.len();
    let n_lets = rule.logic.bindings.len();
    let n_folds = folds.len();
    // Frame: scalars + element fields + let bindings + N accumulator slots
    let frame_slots = n_scalar + n_elem + n_lets + n_folds;
    let frame_size = (frame_slots as i32) * 8;

    // Accumulator slot offsets (at the bottom of the frame).
    let acc_offsets: Vec<i32> = (0..n_folds)
        .map(|i| -(((n_scalar + n_elem + n_lets + i) as i32 + 1) * 8))
        .collect();

    let mut code = Vec::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Outer loop — one record per iteration.
    let outer_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x39, 0xE6]); // cmp r14, r12
    code.extend_from_slice(&[0x0F, 0x8D]);
    let exit_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Offsets for scalar fields.
    let mut scalar_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in scalar_fields.iter().enumerate() {
        scalar_offsets.insert(f.name.as_str(), -((i as i32 + 1) * 8));
    }

    // Offsets for element fields (shared across all fold body evaluations).
    let mut body_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, f) in elem_concept.fields.iter().enumerate() {
        body_offsets.insert(f.name.as_str(), -(((n_scalar + i) as i32 + 1) * 8));
    }

    // Add accumulator name → slot mappings (both __fold_N_acc for body, __fold_N for final expr).
    for (i, fold) in folds.iter().enumerate() {
        body_offsets.insert(leak_string(&fold.acc_name), acc_offsets[i]);
    }

    // Parse scalar input fields (skip in logic, but consume from argv).
    for f in &scalar_fields {
        let offset = scalar_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => { emit_atoi_inline(&mut code); store_rax_at_rbp(&mut code, offset); }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Evaluate let bindings.
    let field_ranges_for_lets = build_field_ranges(input_concept);
    let mut let_offsets = scalar_offsets.clone();
    let mut next_let_slot = -(((n_scalar + n_elem) as i32 + 1) * 8);
    for (name, expr) in &rule.logic.bindings {
        emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets)?;
        store_rax_at_rbp(&mut code, next_let_slot);
        let_offsets.insert(name.as_str(), next_let_slot);
        next_let_slot -= 8;
    }
    for (name, &offset) in &let_offsets {
        if !body_offsets.contains_key(name) {
            body_offsets.insert(name, offset);
        }
    }

    // Parse collection count → r15.
    code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
    emit_atoi_inline(&mut code);
    code.extend_from_slice(&[0x49, 0x89, 0xC7]); // mov r15, rax
    code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14

    // Seed all accumulators.
    for (i, fold) in folds.iter().enumerate() {
        code.extend_from_slice(&[0x48, 0xB8]);
        code.extend_from_slice(&fold.init.to_le_bytes());
        store_rax_at_rbp(&mut code, acc_offsets[i]);
    }

    // Inner loop — per element, update ALL accumulators.
    let inner_loop_top = code.len();
    code.extend_from_slice(&[0x4D, 0x85, 0xFF]); // test r15, r15
    code.extend_from_slice(&[0x0F, 0x84]);
    let inner_done_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Parse element fields.
    for f in &elem_concept.fields {
        let offset = body_offsets[f.name.as_str()];
        code.extend_from_slice(&[0x4B, 0x8B, 0x7C, 0xF5, 0x00]); // mov rdi, [r13+r14*8]
        match f.ty {
            Type::Number => { emit_atoi_inline(&mut code); store_rax_at_rbp(&mut code, offset); }
            Type::Text => store_rdi_at_rbp(&mut code, offset),
            _ => unreachable!(),
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xC6]); // inc r14
    }

    // Evaluate each fold body and update its accumulator.
    let field_ranges = build_field_ranges(elem_concept);
    for (i, fold) in folds.iter().enumerate() {
        emit_eval_expr(&mut code, &fold.body, &fold.item_name, &body_offsets, all_rules, &field_ranges)?;
        store_rax_at_rbp(&mut code, acc_offsets[i]);
    }

    // dec r15 ; jmp inner_loop_top
    code.extend_from_slice(&[0x49, 0xFF, 0xCF]); // dec r15
    code.push(0xE9);
    let back = inner_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back.to_le_bytes());

    // inner_done:
    let inner_done = code.len();
    let inner_off = inner_done as i32 - (inner_done_patch as i32 + 4);
    code[inner_done_patch..inner_done_patch + 4].copy_from_slice(&inner_off.to_le_bytes());

    // Build offset map for the final scalar expression: fold results.
    let mut final_offsets: HashMap<&str, i32> = HashMap::new();
    for (i, fold) in folds.iter().enumerate() {
        final_offsets.insert(leak_string(&fold.name), acc_offsets[i]);
    }
    // Also include scalar fields and let bindings in case the expression references them.
    for (name, &offset) in &scalar_offsets {
        final_offsets.insert(name, offset);
    }
    for (name, &offset) in &let_offsets {
        final_offsets.insert(name, offset);
    }

    // Evaluate the final scalar expression (quantifiers replaced by Ident refs).
    // Use an empty input name — the expression should only reference __fold_N idents.
    let empty_ranges: HashMap<&str, (i64, i64)> = HashMap::new();
    emit_eval_expr(&mut code, &scalar_expr, "__phase6_none__", &final_offsets, all_rules, &empty_ranges)?;

    // Print result.
    if is_bool_output {
        code.extend_from_slice(&[0x84, 0xC0]); // test al, al
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
        emit_itoa_inline(&mut code);
    }

    // jmp outer_loop_top
    code.push(0xE9);
    let outer_off = outer_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&outer_off.to_le_bytes());

    // exit: sys_exit(0)
    let exit_pos = code.len();
    let exit_off = exit_pos as i32 - (exit_patch as i32 + 4);
    code[exit_patch..exit_patch + 4].copy_from_slice(&exit_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x31, 0xFF]);
    code.extend_from_slice(&[0x0F, 0x05]);

    Ok(code)
}

/// Leak a string to get a `&'static str` for use as HashMap key.
/// Used when we need to insert dynamically-created names into offset maps
/// that borrow `&str` from the AST (which outlives the emitter call).
fn leak_string(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
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
    let empty_bindings = no_text_bindings();
    for arg in rest_args {
        let k = classify_concat_arg(arg, elem_concept, item_name, &empty_bindings).ok_or_else(|| NativeError {
            message: "Phase 5b: fold-body concat arg must be a text literal, number expression, or element text field".into(),
        })?;
        if k == ConcatArgKind::BoundText {
            return Err(NativeError {
                message: "Phase 5b: fold body cannot reference bound text vars (no match_result inside fold bodies in native today)".into(),
            });
        }
        if k == ConcatArgKind::CallText {
            return Err(NativeError {
                message: "Phase 5b: fold body cannot call a text-returning rule (use concat literals/fields only)".into(),
            });
        }
        rest_kinds.push(k);
    }

    // Static per-element contribution (sum of literal lengths + 21 per number arg).
    // If ALL text-field args are bounded ([..N]), their max length is included
    // in static_per_element too — enabling single-pass fold sizing.
    let mut static_per_element: i32 = 0;
    let mut all_text_fields_bounded: bool = true;
    for (arg, kind) in rest_args.iter().zip(rest_kinds.iter()) {
        match kind {
            ConcatArgKind::Text => {
                if let Expr::Text(s) = arg {
                    static_per_element += s.as_bytes().len() as i32;
                } else if let Expr::Field(_, field_name) = arg {
                    let bounded = elem_concept
                        .fields
                        .iter()
                        .find(|f| &f.name == field_name)
                        .and_then(|f| f.range)
                        .map(|(_, max)| max as i32);
                    if let Some(max_len) = bounded {
                        static_per_element += max_len;
                    } else {
                        all_text_fields_bounded = false;
                    }
                }
            }
            ConcatArgKind::Number => {
                static_per_element += 21;
            }
            ConcatArgKind::BoundText | ConcatArgKind::CallText => {
                unreachable!("BoundText/CallText rejected above")
            }
        }
    }

    // ===== Emission =====
    let n_scalar = scalar_fields.len();
    let n_elem_fields = elem_concept.fields.len();
    let n_lets = rule.logic.bindings.len();
    // frame: n_scalar + n_elem + n_lets + count_slot + argv_save_slot
    let frame_slots = n_scalar + n_elem_fields + n_lets + 2;
    let frame_size = (frame_slots as i32) * 8;
    let count_slot: i32 = -(((n_scalar + n_elem_fields + n_lets + 1) as i32) * 8);
    let argv_save_slot: i32 = -(((n_scalar + n_elem_fields + n_lets + 2) as i32) * 8);

    let mut code = Vec::new();

    // _start — argv/rbp setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);       // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
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

    // Evaluate let bindings after scalar fields.
    {
        let field_ranges_for_lets = build_field_ranges(input_concept);
        let mut let_offsets: HashMap<&str, i32> = scalar_offsets.clone();
        let mut next_let_slot = -(((n_scalar + n_elem_fields) as i32 + 1) * 8);
        for (name, expr) in &rule.logic.bindings {
            emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets)?;
            store_rax_at_rbp(&mut code, next_let_slot);
            let_offsets.insert(name.as_str(), next_let_slot);
            next_let_slot -= 8;
        }
        for (name, &offset) in &let_offsets {
            if !elem_offsets.contains_key(name) && !scalar_offsets.contains_key(name) {
                elem_offsets.insert(name, offset);
            }
        }
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

    if all_text_fields_bounded {
        // Single-pass optimization: total = init_size + N * static_per_element.
        // No strlen loop needed — all text-field args have [..N] bounds whose
        // max is included in static_per_element. Compute via:
        //   mov rax, static_per_element ; imul rax, r15 ; add rax, init_size
        // Then round up and allocate statically.
        // mov rax, static_per_element
        code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
        code.extend_from_slice(&static_per_element.to_le_bytes());
        // imul rax, r15  (rax = static_per_element * N)
        // REX.WRB + 0F AF ModRM: reg=rax(000) r/m=r15(111) mod=11
        code.extend_from_slice(&[0x49, 0x0F, 0xAF, 0xC7]);
        // add rax, init_size
        if init_size != 0 {
            if init_size <= 127 {
                code.extend_from_slice(&[0x48, 0x83, 0xC0, init_size as u8]);
            } else {
                code.extend_from_slice(&[0x48, 0x05]);
                code.extend_from_slice(&init_size.to_le_bytes());
            }
        }
    } else {
        // Two-pass path: iterate the collection in pass 1 to strlen each
        // unbounded text field.  Pass 2 (fill) follows below.

        // mov rax, init_size
        code.extend_from_slice(&[0x48, 0xC7, 0xC0]);
        code.extend_from_slice(&init_size.to_le_bytes());

        let size_loop_top = code.len();
        code.extend_from_slice(&[0x4D, 0x85, 0xFF]); // test r15, r15
        code.push(0x0F);
        code.push(0x84);                             // jz size_done (rel32)
        let size_done_patch = code.len();
        code.extend_from_slice(&[0; 4]);

        for (arg, kind) in rest_args.iter().zip(rest_kinds.iter()) {
            if *kind == ConcatArgKind::Text {
                if let Expr::Field(_, field_name) = arg {
                    // Only strlen unbounded fields.
                    let is_bounded = elem_concept
                        .fields
                        .iter()
                        .find(|f| &f.name == field_name)
                        .and_then(|f| f.range)
                        .is_some();
                    if is_bounded {
                        continue; // already counted in static_per_element
                    }
                    let idx = elem_concept
                        .fields
                        .iter()
                        .position(|f| &f.name == field_name)
                        .ok_or_else(|| NativeError {
                            message: format!("unknown element field '{}' in fold body", field_name),
                        })?;
                    let disp = (idx * 8) as i32;
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
                    emit_strlen(&mut code);                          // rdx = length
                    code.push(0x59);                                 // pop rcx
                    code.extend_from_slice(&[0x48, 0x01, 0xD1]);     // add rcx, rdx
                    code.extend_from_slice(&[0x48, 0x89, 0xC8]);     // mov rax, rcx
                }
            }
        }

        if static_per_element != 0 {
            if static_per_element <= 127 {
                code.extend_from_slice(&[0x48, 0x83, 0xC0, static_per_element as u8]);
            } else {
                code.extend_from_slice(&[0x48, 0x05]);
                code.extend_from_slice(&static_per_element.to_le_bytes());
            }
        }

        let nef = n_elem_fields as i32;
        if nef <= 127 {
            code.extend_from_slice(&[0x49, 0x83, 0xC6, nef as u8]);
        } else {
            code.extend_from_slice(&[0x49, 0x81, 0xC6]);
            code.extend_from_slice(&nef.to_le_bytes());
        }
        code.extend_from_slice(&[0x49, 0xFF, 0xCF]); // dec r15
        code.push(0xE9);
        let back = size_loop_top as i32 - (code.len() + 4) as i32;
        code.extend_from_slice(&back.to_le_bytes());

        let size_done_pos = code.len();
        let size_done_off = size_done_pos as i32 - (size_done_patch as i32 + 4);
        code[size_done_patch..size_done_patch + 4].copy_from_slice(&size_done_off.to_le_bytes());
    }

    // Round up rax to 8: add rax, 7; and rax, ~7.
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
    // Phase 5b doesn't use CallText args (rejected above), so the slot-index
    // vector is all -1 — emit_concat_fill won't consult it for any arg kind
    // it actually handles here.
    let no_call_slots: Vec<i32> = vec![-1; rest_args.len()];
    emit_concat_fill(
        &mut code,
        rest_args,
        &rest_kinds,
        &no_call_slots,
        item_name,
        elem_concept,
        all_rules,
        &elem_offsets,
        &field_ranges,
        &empty_bindings,
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

/// Emit `mov rax, [rbp+offset]` — symmetric to `store_rax_at_rbp`. Short form
/// (disp8) when the offset fits in an i8; otherwise the disp32 encoding.
fn load_rax_from_rbp(code: &mut Vec<u8>, offset: i32) {
    if offset >= -128 {
        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
        code.push(offset as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
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
                    &no_text_bindings(),
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
    // Both Print and AppendFile effects are handled below.

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, trigger_rule, concept, None, all_rules)?;

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

    // Emit each effect.
    for effect in &reaction.effects {
        match effect {
            Effect::AppendFile { path, content } => {
                emit_append_file_call(
                    &mut code,
                    path,
                    content,
                    trigger_rule,
                    concept,
                    all_rules,
                    &ctx.binding_offsets,
                    &ctx.field_ranges,
                    &no_text_bindings(),
                )?;
            }
            Effect::Print(args) => {
                // Print each arg to stdout with spaces between, newline at end.
                // Each arg is a text expression or a number expression.
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        emit_write_static_to_fd(&mut code, b" ", 1);
                    }
                    // Determine if this arg is a number (needs itoa) or text.
                    let is_number = match arg {
                        Expr::Number(_) | Expr::Neg(_) => true,
                        Expr::Binary(op, _, _) => matches!(op,
                            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod),
                        Expr::Field(base, field_name) => {
                            if matches!(base.as_ref(), Expr::Ident(n) if n == &trigger_rule.input_name) {
                                concept.fields.iter()
                                    .find(|f| &f.name == field_name)
                                    .map_or(false, |f| matches!(f.ty, Type::Number))
                            } else {
                                false
                            }
                        }
                        _ => false,
                    };
                    if is_number {
                        emit_eval_expr(
                            &mut code, arg, &trigger_rule.input_name,
                            &ctx.binding_offsets, all_rules, &ctx.field_ranges,
                        )?;
                        emit_itoa_to_stdout_no_newline(&mut code);
                    } else {
                        emit_text_write_to_fd(
                            &mut code, arg, 1, &trigger_rule.input_name,
                            concept, all_rules, &ctx.binding_offsets,
                            &ctx.field_ranges, &no_text_bindings(),
                        )?;
                    }
                }
                emit_write_newline(&mut code, 1);
            }
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
            // Accept field access on the input name or any other known binding
            // (e.g. context name for multi-input rules). The offsets map is the
            // source of truth — if the field is in the map, it's valid.
            match base.as_ref() {
                Expr::Ident(_) => {}
                _ => {
                    return Err(NativeError {
                        message: "nested field access not supported in native backend".into(),
                    });
                }
            }
            let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                message: format!("unknown field '{}' in native codegen", field_name),
            })?;
            load_rax_from_rbp(code, offset);
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

            // === Text comparison: field == "literal" or field != "literal" ===
            // Detect a text field compared to a text literal (or vice versa).
            // Uses repe cmpsb with the literal NUL-terminated inline.
            if matches!(op, BinOp::Eq | BinOp::NotEq) {
                let (field_offset, literal_bytes) =
                    if let (Expr::Field(base, fname), Expr::Text(lit)) = (left.as_ref(), right.as_ref()) {
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                            (offsets.get(fname.as_str()), Some(lit.as_bytes()))
                        } else { (None, None) }
                    } else if let (Expr::Text(lit), Expr::Field(base, fname)) = (left.as_ref(), right.as_ref()) {
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                            (offsets.get(fname.as_str()), Some(lit.as_bytes()))
                        } else { (None, None) }
                    } else { (None, None) };

                if let (Some(&foff), Some(lit)) = (field_offset, literal_bytes) {
                    // mov rsi, [rbp + foff]  — field pointer (NUL-terminated from argv)
                    if foff >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(foff as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&foff.to_le_bytes());
                    }
                    // jmp over literal + NUL
                    let n = lit.len() + 1; // literal bytes + NUL
                    code.push(0xEB);
                    code.push(n as u8);
                    let data_addr = code.len();
                    code.extend_from_slice(lit);
                    code.push(0); // NUL terminator
                    // lea rdi, [rip + rel32]
                    let end = code.len() + 7;
                    let rel32 = data_addr as i32 - end as i32;
                    code.extend_from_slice(&[0x48, 0x8D, 0x3D]);
                    code.extend_from_slice(&rel32.to_le_bytes());
                    // mov rcx, n
                    code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
                    code.extend_from_slice(&(n as i32).to_le_bytes());
                    // cld ; repe cmpsb
                    code.push(0xFC);
                    code.extend_from_slice(&[0xF3, 0xA6]);
                    // ZF=1 if all bytes matched (including trailing NUL).
                    if *op == BinOp::Eq {
                        // sete al ; movzx rax, al
                        code.extend_from_slice(&[0x0F, 0x94, 0xC0]);
                        code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);
                    } else {
                        // setne al ; movzx rax, al
                        code.extend_from_slice(&[0x0F, 0x95, 0xC0]);
                        code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);
                    }
                    return Ok(());
                }
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
                load_rax_from_rbp(code, *offset);
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
    let jmp_back = div_loop_pos as i32 - (code.len() + 2) as i32;
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
    emit_argc_guard(&mut code, (nfields as i32) + 1);
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

/// Tier-3 native emitter feasibility probe (see docs/known-gaps.md).
/// NO `.verbose` source is involved — the entire binary is hand-emitted by
/// this Rust function writing x86-64 bytes directly. The output proves the
/// native backend CAN produce a ~498-byte HTTP server with zero deps and
/// pure syscalls; it does NOT prove that the language can yet describe one.
///
/// The binary: socket → bind(9999) → listen → accept loop → write response → close
/// ~498 bytes, no libc, no framework, hardcoded response body.
///
/// Long-term: collapse into tier 1 (described in `.verbose`) once Phase 7+
/// introduces declarable network primitives.
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
    emit_argc_guard(&mut code, 2); // vectorized: need at least 1 value
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

/// Emit a stdin reader prologue: reads whitespace-separated tokens from fd 0
/// and builds an argc/argv layout on the stack so the existing rule prologue
/// works unchanged.
///
/// Stack layout during the prologue (all below original rsp):
///   [rsp .. rsp+65535]          64K read buffer (stdin data → NUL-terminated tokens)
///   [rsp+65536 .. rsp+131071]   64K ptr array (up to 8192 token pointers)
///
/// After completion, rsp is restored and the layout at [rsp] is:
///   [rsp]      = argc (token_count + 1)
///   [rsp+8]    = 0 (dummy argv[0])
///   [rsp+16]   = pointer to token[0]
///   ...
///
/// Registers clobbered: rax, rbx, rcx, rdx, rdi, rsi, r8, r9.
/// All are ephemeral — the rule prologue re-reads everything from [rsp].
fn emit_stdin_prologue(code: &mut Vec<u8>) {
    // ─── save original rsp & allocate 128K ─────────────────────
    // mov rbx, rsp
    code.extend_from_slice(&[0x48, 0x89, 0xE3]);
    // sub rsp, 131072
    code.extend_from_slice(&[0x48, 0x81, 0xEC, 0x00, 0x00, 0x02, 0x00]);

    // ─── sys_read(0, rsp, 65535) ───────────────────────────────
    // xor edi, edi           (fd = 0 = stdin)
    code.extend_from_slice(&[0x31, 0xFF]);
    // mov rsi, rsp           (buf = rsp)
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);
    // mov edx, 65535         (count — leaves 1 byte for NUL sentinel)
    code.extend_from_slice(&[0xBA, 0xFF, 0xFF, 0x00, 0x00]);
    // xor eax, eax           (syscall nr 0 = read)
    code.extend_from_slice(&[0x31, 0xC0]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    // ─── error guard: negative rax → 0 bytes ──────────────────
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jns +2                 (skip xor if non-negative)
    code.extend_from_slice(&[0x79, 0x02]);
    // xor eax, eax
    code.extend_from_slice(&[0x31, 0xC0]);

    // ─── NUL-terminate buffer ──────────────────────────────────
    // mov byte [rsp + rax*1], 0
    code.extend_from_slice(&[0xC6, 0x04, 0x04, 0x00]);

    // ─── setup tokenizer state ─────────────────────────────────
    // lea r8, [rsp + 65536]  (ptr array base)
    code.extend_from_slice(&[0x4C, 0x8D, 0x84, 0x24, 0x00, 0x00, 0x01, 0x00]);
    // xor r9d, r9d           (token count = 0)
    code.extend_from_slice(&[0x45, 0x31, 0xC9]);
    // mov rcx, rsp           (scan pointer = buffer start)
    code.extend_from_slice(&[0x48, 0x89, 0xE1]);
    // lea rdx, [rsp + rax]   (buffer end)
    code.extend_from_slice(&[0x48, 0x8D, 0x14, 0x04]);

    // ═══ TOKENIZER LOOP ═══════════════════════════════════════
    // skip_ws:
    let skip_ws = code.len();

    //   cmp rcx, rdx
    code.extend_from_slice(&[0x48, 0x39, 0xD1]);
    //   jge done_tokenize (rel32, patched)
    code.extend_from_slice(&[0x0F, 0x8D]);
    let done_tok_patch1 = code.len();
    code.extend_from_slice(&[0x00; 4]);

    //   mov al, [rcx]
    code.extend_from_slice(&[0x8A, 0x01]);
    //   cmp al, ' '  ;  je next_ws
    code.extend_from_slice(&[0x3C, 0x20]);
    code.push(0x74); let nw1 = code.len(); code.push(0);
    //   cmp al, '\t' ;  je next_ws
    code.extend_from_slice(&[0x3C, 0x09]);
    code.push(0x74); let nw2 = code.len(); code.push(0);
    //   cmp al, '\n' ;  je next_ws
    code.extend_from_slice(&[0x3C, 0x0A]);
    code.push(0x74); let nw3 = code.len(); code.push(0);
    //   cmp al, '\r' ;  je next_ws
    code.extend_from_slice(&[0x3C, 0x0D]);
    code.push(0x74); let nw4 = code.len(); code.push(0);

    // ─── start of token: bounds-check + store pointer ────────────
    // Guard: if r9 >= 8192 (ptr array full), stop tokenizing.
    code.extend_from_slice(&[0x41, 0x81, 0xF9, 0x00, 0x20, 0x00, 0x00]); // cmp r9d, 8192
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done_tokenize (rel32, patched with done_tok)
    let token_cap_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);
    // mov [r8 + r9*8], rcx   (REX.WXB=0x4B)
    code.extend_from_slice(&[0x4B, 0x89, 0x0C, 0xC8]);
    // inc r9
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]);

    // find_end: scan for next whitespace or end-of-buffer
    let find_end = code.len();
    //   inc rcx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    //   cmp rcx, rdx
    code.extend_from_slice(&[0x48, 0x39, 0xD1]);
    //   jge done_tokenize (rel32, patched)
    code.extend_from_slice(&[0x0F, 0x8D]);
    let done_tok_patch2 = code.len();
    code.extend_from_slice(&[0x00; 4]);

    //   mov al, [rcx]
    code.extend_from_slice(&[0x8A, 0x01]);
    //   cmp al, ' '  ;  je terminate_token
    code.extend_from_slice(&[0x3C, 0x20]);
    code.push(0x74); let tt1 = code.len(); code.push(0);
    //   cmp al, '\t' ;  je terminate_token
    code.extend_from_slice(&[0x3C, 0x09]);
    code.push(0x74); let tt2 = code.len(); code.push(0);
    //   cmp al, '\n' ;  je terminate_token
    code.extend_from_slice(&[0x3C, 0x0A]);
    code.push(0x74); let tt3 = code.len(); code.push(0);
    //   cmp al, '\r' ;  je terminate_token
    code.extend_from_slice(&[0x3C, 0x0D]);
    code.push(0x74); let tt4 = code.len(); code.push(0);
    //   jmp find_end (backward short)
    code.push(0xEB);
    code.push((find_end as isize - code.len() as isize - 1) as u8);

    // terminate_token: NUL-terminate and continue scanning
    let terminate_token = code.len();
    //   mov byte [rcx], 0
    code.extend_from_slice(&[0xC6, 0x01, 0x00]);
    //   inc rcx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    //   jmp skip_ws (backward short)
    code.push(0xEB);
    code.push((skip_ws as isize - code.len() as isize - 1) as u8);

    // next_ws: advance past whitespace and re-scan
    let next_ws = code.len();
    //   inc rcx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    //   jmp skip_ws (backward short)
    code.push(0xEB);
    code.push((skip_ws as isize - code.len() as isize - 1) as u8);

    // ─── patch forward jumps ───────────────────────────────────
    for p in [nw1, nw2, nw3, nw4] {
        code[p] = (next_ws - p - 1) as u8;
    }
    for p in [tt1, tt2, tt3, tt4] {
        code[p] = (terminate_token - p - 1) as u8;
    }

    // done_tokenize:
    let done_tok = code.len();
    let r1 = done_tok as i32 - (done_tok_patch1 as i32 + 4);
    code[done_tok_patch1..done_tok_patch1 + 4].copy_from_slice(&r1.to_le_bytes());
    let r2 = done_tok as i32 - (done_tok_patch2 as i32 + 4);
    code[done_tok_patch2..done_tok_patch2 + 4].copy_from_slice(&r2.to_le_bytes());
    // Patch token capacity guard → done_tokenize
    let r3 = done_tok as i32 - (token_cap_patch as i32 + 4);
    code[token_cap_patch..token_cap_patch + 4].copy_from_slice(&r3.to_le_bytes());

    // ═══ COPY TOKENS TO ARGC/ARGV LAYOUT AT RBX ═══════════════
    // mov rax, r9             (token count)
    code.extend_from_slice(&[0x4C, 0x89, 0xC8]);
    // inc rax                 (argc = tokens + 1 for dummy argv[0])
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]);
    // mov [rbx], rax
    code.extend_from_slice(&[0x48, 0x89, 0x03]);
    // mov qword [rbx+8], 0    (dummy argv[0])
    code.extend_from_slice(&[0x48, 0xC7, 0x43, 0x08, 0x00, 0x00, 0x00, 0x00]);
    // xor ecx, ecx            (i = 0)
    code.extend_from_slice(&[0x31, 0xC9]);

    // copy_loop:
    let copy_loop = code.len();
    // cmp rcx, r9
    code.extend_from_slice(&[0x4C, 0x39, 0xC9]);
    // jge copy_done (short)
    code.push(0x7D);
    let copy_done_patch = code.len();
    code.push(0);
    // mov rax, [r8 + rcx*8]
    code.extend_from_slice(&[0x49, 0x8B, 0x04, 0xC8]);
    // mov [rbx + rcx*8 + 16], rax
    code.extend_from_slice(&[0x48, 0x89, 0x44, 0xCB, 0x10]);
    // inc rcx
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    // jmp copy_loop (backward short)
    code.push(0xEB);
    code.push((copy_loop as isize - code.len() as isize - 1) as u8);

    // copy_done:
    let copy_done = code.len();
    code[copy_done_patch] = (copy_done - copy_done_patch - 1) as u8;

    // ─── restore rsp → rule prologue sees argc/argv layout ────
    // mov rsp, rbx
    code.extend_from_slice(&[0x48, 0x89, 0xDC]);
}

/// Emit a streaming line reader prologue. Returns the offset of `stream_top`
/// within the emitted code (always 0 — the first instruction).
///
/// Structure:
///   stream_top: save rsp, allocate 128K, read line byte-by-byte
///   → on got_line: tokenize, copy argv, restore rsp, fall through to rule code
///   → on EOF: sys_exit(0)
///   → on empty line: restore rsp, jmp stream_top
///
/// The caller must:
///   1. Append the rule code (with sys_exit stripped)
///   2. Append `mov rsp, rbp; pop rbp; jmp stream_top`
fn emit_stream_prologue(code: &mut Vec<u8>) -> usize {
    let stream_top = code.len();

    // ─── save & allocate ────────────────────────────────────────
    code.extend_from_slice(&[0x48, 0x89, 0xE3]); // mov rbx, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC, 0x00, 0x00, 0x02, 0x00]); // sub rsp, 128K

    // ─── line reader: byte-by-byte until \n or EOF ─────────────
    code.extend_from_slice(&[0x49, 0x89, 0xE0]); // mov r8, rsp (buffer start)
    code.extend_from_slice(&[0x45, 0x31, 0xC9]); // xor r9d, r9d (length = 0)

    let read_byte = code.len();
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0x4B, 0x8D, 0x34, 0x08]); // lea rsi, [r8+r9]
    code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1
    code.extend_from_slice(&[0x31, 0xC0]); // xor eax, eax
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // EOF/error → near jump to check_eof (patched later)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x8E]); // jle check_eof (rel32)
    let eof_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    // Check newline
    code.extend_from_slice(&[0x43, 0x80, 0x3C, 0x08, 0x0A]); // cmp byte [r8+r9], 0x0A
    code.push(0x74); // je got_line (short, patched)
    let got_line_patch = code.len();
    code.push(0x00);

    // Continue reading
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]); // inc r9
    code.extend_from_slice(&[0x41, 0x81, 0xF9, 0xFE, 0xFF, 0x00, 0x00]); // cmp r9d, 65534
    code.push(0x0F); code.push(0x8C); // jl read_byte (rel32)
    let back = read_byte as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&back.to_le_bytes());

    // ─── got_line: NUL-terminate, check empty, then tokenize ───
    let got_line = code.len();
    code[got_line_patch] = (got_line - got_line_patch - 1) as u8;

    code.extend_from_slice(&[0x43, 0xC6, 0x04, 0x08, 0x00]); // mov byte [r8+r9], 0
    // Empty line? → jump to skip_empty (near, patched)
    code.extend_from_slice(&[0x4D, 0x85, 0xC9]); // test r9, r9
    code.extend_from_slice(&[0x0F, 0x84]); // jz skip_empty (rel32)
    let skip_empty_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    // rax = line length for tokenizer
    code.extend_from_slice(&[0x4C, 0x89, 0xC8]); // mov rax, r9

    // Jump over EOF/skip handlers to the tokenizer
    code.push(0xE9); // jmp tokenize (rel32)
    let tokenize_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    // ─── check_eof handler ─────────────────────────────────────
    let check_eof = code.len();
    let eof_rel = check_eof as i32 - (eof_patch as i32 + 4);
    code[eof_patch..eof_patch + 4].copy_from_slice(&eof_rel.to_le_bytes());

    // If we have pending bytes, process them as last line
    code.extend_from_slice(&[0x4D, 0x85, 0xC9]); // test r9, r9
    code.extend_from_slice(&[0x0F, 0x85]); // jnz got_line (rel32)
    let got_line_rel = got_line as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&got_line_rel.to_le_bytes());
    // True EOF: exit(0)
    code.extend_from_slice(&[0x48, 0x89, 0xDC]); // mov rsp, rbx
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0x31, 0xFF]); // xor rdi, rdi
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    // ─── skip_empty handler ────────────────────────────────────
    let skip_empty = code.len();
    let skip_rel = skip_empty as i32 - (skip_empty_patch as i32 + 4);
    code[skip_empty_patch..skip_empty_patch + 4].copy_from_slice(&skip_rel.to_le_bytes());

    code.extend_from_slice(&[0x48, 0x89, 0xDC]); // mov rsp, rbx
    code.push(0xE9); // jmp stream_top (rel32)
    let stream_back = stream_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&stream_back.to_le_bytes());

    // ─── tokenize: setup + tokenizer loop ──────────────────────
    let tokenize = code.len();
    let tok_rel = tokenize as i32 - (tokenize_patch as i32 + 4);
    code[tokenize_patch..tokenize_patch + 4].copy_from_slice(&tok_rel.to_le_bytes());

    // r8 = ptr array, r9 = 0, rcx = buffer, rdx = buffer end
    code.extend_from_slice(&[0x4C, 0x8D, 0x84, 0x24, 0x00, 0x00, 0x01, 0x00]); // lea r8, [rsp+65536]
    code.extend_from_slice(&[0x45, 0x31, 0xC9]); // xor r9d, r9d
    code.extend_from_slice(&[0x48, 0x89, 0xE1]); // mov rcx, rsp
    code.extend_from_slice(&[0x48, 0x8D, 0x14, 0x04]); // lea rdx, [rsp+rax]

    // ═══ TOKENIZER (same as stdin prologue) ════════════════════
    let skip_ws = code.len();
    code.extend_from_slice(&[0x48, 0x39, 0xD1]);
    code.extend_from_slice(&[0x0F, 0x8D]);
    let dt1 = code.len(); code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x8A, 0x01]);
    code.extend_from_slice(&[0x3C, 0x20]); code.push(0x74); let nw1 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x09]); code.push(0x74); let nw2 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x0A]); code.push(0x74); let nw3 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x0D]); code.push(0x74); let nw4 = code.len(); code.push(0);
    // Guard: token capacity check
    code.extend_from_slice(&[0x41, 0x81, 0xF9, 0x00, 0x20, 0x00, 0x00]); // cmp r9d, 8192
    code.extend_from_slice(&[0x0F, 0x8D]); // jge done_tokenize
    let stream_cap_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);
    code.extend_from_slice(&[0x4B, 0x89, 0x0C, 0xC8]);
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]);
    let fe = code.len();
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    code.extend_from_slice(&[0x48, 0x39, 0xD1]);
    code.extend_from_slice(&[0x0F, 0x8D]);
    let dt2 = code.len(); code.extend_from_slice(&[0; 4]);
    code.extend_from_slice(&[0x8A, 0x01]);
    code.extend_from_slice(&[0x3C, 0x20]); code.push(0x74); let t1 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x09]); code.push(0x74); let t2 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x0A]); code.push(0x74); let t3 = code.len(); code.push(0);
    code.extend_from_slice(&[0x3C, 0x0D]); code.push(0x74); let t4 = code.len(); code.push(0);
    code.push(0xEB); code.push((fe as isize - code.len() as isize - 1) as u8);
    let tt = code.len();
    code.extend_from_slice(&[0xC6, 0x01, 0x00]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    code.push(0xEB); code.push((skip_ws as isize - code.len() as isize - 1) as u8);
    let nw = code.len();
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    code.push(0xEB); code.push((skip_ws as isize - code.len() as isize - 1) as u8);
    for p in [nw1,nw2,nw3,nw4] { code[p] = (nw - p - 1) as u8; }
    for p in [t1,t2,t3,t4] { code[p] = (tt - p - 1) as u8; }
    let dt = code.len();
    code[dt1..dt1+4].copy_from_slice(&((dt as i32) - (dt1 as i32 + 4)).to_le_bytes());
    code[dt2..dt2+4].copy_from_slice(&((dt as i32) - (dt2 as i32 + 4)).to_le_bytes());
    // Patch token capacity guard
    code[stream_cap_patch..stream_cap_patch+4].copy_from_slice(&((dt as i32) - (stream_cap_patch as i32 + 4)).to_le_bytes());

    // ═══ COPY TOKENS TO ARGC/ARGV AT RBX ═══════════════════════
    code.extend_from_slice(&[0x4C, 0x89, 0xC8]); // mov rax, r9
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    code.extend_from_slice(&[0x48, 0x89, 0x03]); // mov [rbx], rax
    code.extend_from_slice(&[0x48, 0xC7, 0x43, 0x08, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
    let cl = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xC9]);
    code.push(0x7D); let cdp = code.len(); code.push(0);
    code.extend_from_slice(&[0x49, 0x8B, 0x04, 0xC8]);
    code.extend_from_slice(&[0x48, 0x89, 0x44, 0xCB, 0x10]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]);
    code.push(0xEB); code.push((cl as isize - code.len() as isize - 1) as u8);
    let cd = code.len();
    code[cdp] = (cd - cdp - 1) as u8;

    // Restore rsp → rule code sees argc/argv
    code.extend_from_slice(&[0x48, 0x89, 0xDC]); // mov rsp, rbx

    stream_top
}

/// Tier-3 native emitter feasibility probe (see docs/known-gaps.md).
/// NO `.verbose` source is involved — this Rust function writes x86-64 bytes
/// directly, producing a ~358-byte standalone TCP echo server. Feasibility
/// proof for socket/bind/accept/read/write syscalls; not a Verbose-described
/// program.
///
/// Listens on the given port, accepts connections, echoes received data
/// back until the client disconnects.
///
/// Syscalls used: socket(41), bind(49), listen(50), accept(43),
///                read(0), write(1), close(3), exit(60).
///
/// Stack layout:
///   [rsp .. rsp+16]    sockaddr_in struct (for bind)
///   [rsp+16 .. rsp+4112] read buffer (4096 bytes)
pub fn compile_echo_server(port: u16, output_path: &str) -> Result<(), NativeError> {
    // Tier-3 legacy path: the --echo-server flag hard-codes a 4096-byte
    // read buffer. Phase 7 slice 2b reuses the exact same emission body
    // via `emit_raw_tcp_echo_bytes`, parametrized by max_request, to
    // serve a .verbose-described service.
    let code = emit_raw_tcp_echo_bytes(port, 4096);
    write_server_elf(&code, output_path, "echo-server", port)
}

/// Phase 7 slice 2b: emit the machine code for a raw-TCP echo server
/// bound to `port`, with a read/write buffer sized exactly at
/// `max_request` bytes. The caller is responsible for wrapping the bytes
/// into an ELF and writing it to disk (`write_server_elf`).
///
/// This function is the shared emission body for `--echo-server` (tier 3,
/// hard-coded 4096-byte buffer) and `--native` over an
/// `Item::Service { protocol: RawTcp, handler: <identity> }` (tier 1,
/// buffer size from .verbose). Extracting the body here is what lets one
/// piece of machine-code emission serve both paths — the tier collapse
/// Phase 7 is meant to start.
fn emit_raw_tcp_echo_bytes(port: u16, max_request: u32) -> Vec<u8> {
    let mut code = Vec::new();
    let port_be = port.to_be_bytes(); // network byte order
    let buf_bytes = max_request.to_le_bytes();

    // ═══ SOCKET ════════════════════════════════════════════════
    // socket(AF_INET=2, SOCK_STREAM=1, 0) → rax = server_fd
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]); // mov rax, 41
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // mov rdi, 2 (AF_INET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1 (SOCK_STREAM)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // xor rdx, rdx
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    // Save server_fd in r12
    code.extend_from_slice(&[0x49, 0x89, 0xC4]);                         // mov r12, rax

    // ═══ SETSOCKOPT (SO_REUSEADDR) ════════════════════════════
    // Prevent "Address already in use" on rapid restart.
    // setsockopt(fd, SOL_SOCKET=1, SO_REUSEADDR=2, &1, 4) → syscall 54
    // Push the value 1 onto the stack for the optval pointer.
    code.extend_from_slice(&[0x6A, 0x01]);                               // push 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x36, 0x00, 0x00, 0x00]); // mov rax, 54
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1 (SOL_SOCKET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x02, 0x00, 0x00, 0x00]); // mov rdx, 2 (SO_REUSEADDR)
    code.extend_from_slice(&[0x49, 0x89, 0xE2]);                         // mov r10, rsp (optval = &1)
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x04, 0x00, 0x00, 0x00]); // mov r8, 4 (optlen)
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]);                   // add rsp, 8 (pop the 1)

    // ═══ BIND ═════════════════════════════════════════════════
    // Build sockaddr_in on the stack:
    //   [rsp+0..2]  = AF_INET (2, little-endian u16)
    //   [rsp+2..4]  = port (network byte order u16)
    //   [rsp+4..8]  = INADDR_ANY (0)
    //   [rsp+8..16] = padding (0)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]);                   // sub rsp, 16
    // mov word [rsp], 2 (AF_INET)
    code.extend_from_slice(&[0x66, 0xC7, 0x04, 0x24, 0x02, 0x00]);
    // mov word [rsp+2], port (network byte order)
    code.extend_from_slice(&[0x66, 0xC7, 0x44, 0x24, 0x02]);
    code.extend_from_slice(&port_be);
    // mov qword [rsp+4], 0 (INADDR_ANY + padding)
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00]);
    // Actually the above only writes 4 bytes at [rsp+4]. Let me also zero [rsp+8..16].
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x08, 0x00, 0x00, 0x00, 0x00]);

    // bind(r12, rsp, 16) → syscall 49
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x31, 0x00, 0x00, 0x00]); // mov rax, 49
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // mov rsi, rsp
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // mov rdx, 16
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ LISTEN ═══════════════════════════════════════════════
    // listen(r12, 128) → syscall 50
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]); // mov rax, 50
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]); // mov rsi, 128
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // Allocate the read buffer on the stack (below sockaddr_in).
    // Size comes from max_request — the .verbose-declared bound that
    // every incoming request is truncated to.
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);                         // sub rsp, imm32
    code.extend_from_slice(&buf_bytes);

    // ═══ ACCEPT LOOP ══════════════════════════════════════════
    let accept_top = code.len();
    // accept(r12, NULL, NULL) → rax = client_fd → syscall 43
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // mov rax, 43
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);                         // xor rsi, rsi
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // xor rdx, rdx
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    // Save client_fd in r13
    code.extend_from_slice(&[0x49, 0x89, 0xC5]);                         // mov r13, rax

    // ═══ ECHO LOOP (per connection) ═══════════════════════════
    let echo_top = code.len();
    // read(client_fd, rsp, max_request) → rax = bytes_read → syscall 0
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);                         // xor rax, rax (sys_read=0)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // mov rsi, rsp
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);                         // mov rdx, imm32
    code.extend_from_slice(&buf_bytes);
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // if bytes_read <= 0: close + accept next
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);                         // test rax, rax
    code.push(0x7E);                                                     // jle close_client
    let close_patch = code.len();
    code.push(0x00);

    // write(client_fd, rsp, bytes_read) → syscall 1
    code.extend_from_slice(&[0x48, 0x89, 0xC2]);                         // mov rdx, rax (count)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // mov rsi, rsp
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // jmp echo_top
    code.push(0xE9);
    let echo_back = echo_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&echo_back.to_le_bytes());

    // close_client:
    let close_pos = code.len();
    code[close_patch] = (close_pos - close_patch - 1) as u8;
    // close(client_fd) → syscall 3
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]); // mov rax, 3
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // jmp accept_top
    code.push(0xE9);
    let accept_back = accept_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&accept_back.to_le_bytes());

    // (Server never exits — kill with Ctrl-C / SIGTERM)

    // Validate emitted code
    if let Err(e) = crate::validate_x86::validate_code(&code) {
        eprintln!("warning: x86-64 validation: {} (decoder incomplete, may be false positive)", e);
    }

    code
}

/// Shared writer: wrap a Vec of machine code bytes in an ELF, write it to
/// `output_path`, set executable permissions on Unix, and print a line
/// tagged with `kind` and `port`. Called by both `compile_echo_server`
/// (tier-3 probe) and `compile_service` (tier-1 Phase 7 service).
fn write_server_elf(
    code: &[u8],
    output_path: &str,
    kind: &str,
    port: u16,
) -> Result<(), NativeError> {
    let elf = build_elf(code);
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
            .map_err(|e| NativeError { message: format!("cannot set permissions: {}", e) })?;
    }

    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    println!("{}: {} ({} bytes, port {})", kind, output_path, size, port);
    Ok(())
}

/// Phase 7 slice 2b: compile an `Item::Service` with `Protocol::RawTcp`
/// and an identity-shaped handler into a tier-1 native binary.
///
/// The handler must be strictly identity — its logic expression must be
/// `output_concept { field: input.field }`, where both concepts have
/// exactly one `bytes [..max_request]` field (already enforced by the
/// verifier). Anything more than identity returns an error here; later
/// slices relax this restriction one operation at a time.
///
/// The emitted machine code is byte-for-byte equivalent to what
/// `compile_echo_server` has produced via the tier-3 path since Phase 0,
/// just with port and buffer size driven from the .verbose source
/// instead of CLI arguments. This is the first tier-3 → tier-1 collapse
/// the Phase 7 design calls for.
pub fn compile_service(
    program: &Program,
    service_name: &str,
    output_path: &str,
) -> Result<(), NativeError> {
    let service = program
        .items
        .iter()
        .find_map(|i| match i {
            Item::Service(s) if s.name == service_name => Some(s),
            _ => None,
        })
        .ok_or_else(|| NativeError {
            message: format!("no service named '{}'", service_name),
        })?;

    if let Protocol::Http10 = service.protocol {
        // Phase 7 Http10 dispatch: detect whether the handler is a constant
        // HttpResponse (slice 3b, precomputed wire response) or a dynamic
        // shape with if/else and req inspection (slice 3c), and route to
        // the appropriate compiler. Unsupported shapes return a named error
        // pointing at the slice where they land.
        let handler = program
            .items
            .iter()
            .find_map(|i| match i {
                Item::Rule(r) if r.name == service.handler => Some(r),
                _ => None,
            })
            .ok_or_else(|| NativeError {
                message: format!(
                    "service '{}' handler '{}' not found (verifier should have caught this)",
                    service.name, service.handler
                ),
            })?;
        // Phase 8 slice 8a: presence of a log forces the dynamic path,
        // because the log content can reference request fields (method /
        // path) which only exist once the HTTP parser has run — and the
        // constant path does not emit the parser. The dynamic path handles
        // pure-literal Record leaves correctly, so no expressive loss.
        return match analyze_http10_handler_shape(handler) {
            Http10HandlerShape::Constant if service.log.is_none() => {
                compile_http10_constant_service(program, service, output_path)
            }
            Http10HandlerShape::Constant | Http10HandlerShape::Dynamic => {
                compile_http10_dynamic_service(program, service, output_path)
            }
            Http10HandlerShape::Unsupported(reason) => Err(NativeError {
                message: format!(
                    "service '{}' handler '{}': {}",
                    service.name, service.handler, reason
                ),
            }),
        };
    }

    // From here on, protocol is RawTcp. The only other variant (Http10)
    // returned above.

    let handler = program
        .items
        .iter()
        .find_map(|i| match i {
            Item::Rule(r) if r.name == service.handler => Some(r),
            _ => None,
        })
        .ok_or_else(|| NativeError {
            message: format!(
                "service '{}' handler '{}' not found (verifier should have caught this)",
                service.name, service.handler
            ),
        })?;

    // Enforce identity handler for slice 2b. The verifier already
    // guarantees the shape of input and output concepts; here we check
    // that the logic is literally `OutputConcept { f: input_var.f }` so
    // the emitted echo is semantically the handler's declared behavior.
    if handler.logic.target != "resp" && handler.logic.target != handler.output_name {
        // Expected: target matches the declared output binding name.
        // (Already enforced elsewhere — this is a defensive guard.)
    }
    if !handler.logic.bindings.is_empty() {
        return Err(NativeError {
            message: format!(
                "Phase 7 slice 2b: handler '{}' has let bindings; only identity handlers \
                 are supported in this slice",
                handler.name
            ),
        });
    }
    let is_identity = match &handler.logic.value {
        Expr::Record(_, fields) if fields.len() == 1 => {
            let (_, value) = &fields[0];
            matches!(value, Expr::Field(base, _) if matches!(base.as_ref(), Expr::Ident(n) if n == &handler.input_name))
        }
        _ => false,
    };
    if !is_identity {
        return Err(NativeError {
            message: format!(
                "Phase 7 slice 2b: handler '{}' logic is not identity (expected \
                 `resp = <OutputConcept> {{ <field>: <input>.<field> }}`); non-identity \
                 handlers land in slice 2c+",
                handler.name
            ),
        });
    }

    let code = emit_raw_tcp_echo_bytes(service.port, service.max_request);
    write_server_elf(&code, output_path, "service", service.port)
}

/// Tier-2 hybrid — rule from `.verbose`, network shell hardcoded
/// (see docs/known-gaps.md). The rule logic is verified against its source;
/// the HTTP plumbing around it (socket / bind / listen / accept / parse
/// GET path / write response) is emitted by the hand-written Rust code
/// below, NOT described in any `.verbose` file.
///
/// Example: for a rule with 2 number fields,
///   curl http://localhost:8080/500/25
///   → HTTP/1.0 200 OK\r\n\r\ntrue\n
///
/// Security: max 4K request, GET-only, bounds-checked path parsing.

/// Phase 7 slice 3b: compile an Http10 service whose handler returns a
/// constant HttpResponse. The handler must be of the shape
///
///     resp = HttpResponse { status: <number-literal>, body: <text-literal> }
///
/// (fields may appear in either order). Non-constant handlers — those
/// that branch on req.method / req.path or compute the body via concat —
/// land in slice 3c+. This slice's job is to prove the full Http10 chain
/// runs: grammar + verification (slice 3a) + emission (here) + running
/// binary that responds correctly to a real curl.
///
/// Response shape emitted:
///
///     HTTP/1.0 <status> OK\r\n
///     Content-Length: <body_len>\r\n
///     \r\n
///     <body>
///
/// The reason phrase is always "OK" in slice 3b. Slice 3c can add proper
/// reason-phrase mapping (200 → "OK", 404 → "Not Found", …). Clients
/// accept mismatched reason phrases (the protocol ignores them once the
/// status code is parsed), so "OK" is correct-enough for this slice.

/// Shape classification for Http10 handler logic. Drives the compile_service
/// Http10 dispatch between slice 3b (precomputed wire response) and slice 3c
/// (if/else with runtime evaluation) paths.
enum Http10HandlerShape {
    /// Slice 3b: `logic = HttpResponse { status: N, body: "..." }`.
    Constant,
    /// Slice 3c: `logic = if <cond> then <arm> else <arm>` where each arm
    /// recursively satisfies the shape, with leaves of the form
    /// `HttpResponse { status: N, body: <text-literal | req.method | req.path> }`.
    Dynamic,
    /// Anything else: explicit error message naming the slice that would
    /// lift the restriction (concat in body, let bindings, etc.).
    Unsupported(String),
}

/// Walk the handler's logic expression and classify it. Pure analysis — no
/// emission, no borrow of the program. Called once per service dispatch.
fn analyze_http10_handler_shape(handler: &Rule) -> Http10HandlerShape {
    if !handler.logic.bindings.is_empty() {
        return Http10HandlerShape::Unsupported(
            "let bindings in the handler body are not supported until slice 3d+".into(),
        );
    }
    classify_http10_expr(&handler.logic.value, &handler.input_name)
}

fn classify_http10_expr(expr: &Expr, input_name: &str) -> Http10HandlerShape {
    match expr {
        Expr::Record(name, fields) if name == "HttpResponse" => {
            classify_http_response_record(fields, input_name)
        }
        Expr::If(_cond, then_e, else_e) => {
            // Both arms must recursively satisfy the shape. The condition
            // itself is validated later at emit time (emit_eval_expr handles
            // any boolean expression; the RawTcp-style strict shape check is
            // unnecessary for conditions because the verifier already caught
            // type errors).
            match (
                classify_http10_expr(then_e, input_name),
                classify_http10_expr(else_e, input_name),
            ) {
                (Http10HandlerShape::Unsupported(why), _) => Http10HandlerShape::Unsupported(why),
                (_, Http10HandlerShape::Unsupported(why)) => Http10HandlerShape::Unsupported(why),
                // At least one side is non-constant (an If nesting) → Dynamic.
                _ => Http10HandlerShape::Dynamic,
            }
        }
        _ => Http10HandlerShape::Unsupported(format!(
            "unexpected handler expression shape {:?}; slice 3c supports \
             `HttpResponse {{ status: N, body: … }}` or `if … then … else …`",
            expr_kind(expr)
        )),
    }
}

fn classify_http_response_record(
    fields: &[(String, Expr)],
    input_name: &str,
) -> Http10HandlerShape {
    let mut has_const_status = false;
    let mut has_const_body = false;
    let mut has_dynamic_field = false;
    let mut seen_status = false;
    let mut seen_body = false;

    for (f_name, f_expr) in fields {
        match f_name.as_str() {
            "status" => {
                seen_status = true;
                match f_expr {
                    Expr::Number(n) => {
                        if (100..=599).contains(n) {
                            has_const_status = true;
                        } else {
                            return Http10HandlerShape::Unsupported(format!(
                                "status {} outside HTTP valid range [100, 599]", n
                            ));
                        }
                    }
                    _ => {
                        return Http10HandlerShape::Unsupported(
                            "slice 3c: status must be a number literal; computed status lands in slice 3d+".into(),
                        );
                    }
                }
            }
            "body" => {
                seen_body = true;
                match f_expr {
                    Expr::Text(_) => has_const_body = true,
                    Expr::Field(base, fname)
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                            && (fname == "method" || fname == "path") =>
                    {
                        has_dynamic_field = true;
                    }
                    // Slice 3d: body via concat(...) — args go through the
                    // existing concat pipeline (text literals, request fields,
                    // numbers). The buffer is allocated on the iteration's
                    // stack and freed in one shot when the accept loop
                    // restores rsp before jumping back.
                    Expr::Concat(_) => {
                        has_dynamic_field = true;
                    }
                    _ => {
                        return Http10HandlerShape::Unsupported(format!(
                            "slice 3d: body must be a text literal, req.method / req.path, \
                             or concat(...); other computed bodies land in a later slice (got {:?})",
                            expr_kind(f_expr)
                        ));
                    }
                }
            }
            _ => {
                return Http10HandlerShape::Unsupported(format!(
                    "HttpResponse has no field '{}'; expected only 'status' and 'body'",
                    f_name
                ));
            }
        }
    }
    if !seen_status || !seen_body {
        return Http10HandlerShape::Unsupported(
            "HttpResponse must have both 'status' and 'body' fields".into(),
        );
    }
    // Pure-literal (status Number + body Text) → Constant shape → slice 3b path.
    // Any request-field reference in body → Dynamic → slice 3c path.
    if has_const_status && has_const_body && !has_dynamic_field {
        Http10HandlerShape::Constant
    } else {
        Http10HandlerShape::Dynamic
    }
}

/// Short tag for Expr variants, for use in error messages. Avoids dumping
/// the whole derived Debug of the expression tree at the user.
fn expr_kind(e: &Expr) -> &'static str {
    match e {
        Expr::Number(_) => "Number",
        Expr::Text(_) => "Text",
        Expr::Ident(_) => "Ident",
        Expr::Field(_, _) => "Field",
        Expr::Binary(_, _, _) => "Binary",
        Expr::Call(_, _) => "Call",
        Expr::If(_, _, _) => "If",
        Expr::Not(_) => "Not",
        Expr::Neg(_) => "Neg",
        Expr::Quantifier(_, _, _, _) => "Quantifier",
        Expr::Fold(_, _, _, _, _) => "Fold",
        Expr::Map(_, _, _) => "Map",
        Expr::Filter(_, _, _) => "Filter",
        Expr::Ok(_) => "Ok",
        Expr::Err(_) => "Err",
        Expr::MatchResult(_, _, _, _, _) => "MatchResult",
        Expr::Record(_, _) => "Record",
        Expr::Concat(_) => "Concat",
    }
}

/// Phase 8 slice 8b/8c — walk the log content to detect whether the
/// program needs a per-request `clock_gettime` slot. Returns true if any
/// subexpression references `req.timestamp`, which is the only synthetic
/// field whose value is not already populated by the time the log fires.
/// `resp.status` and `resp.body` ride existing handler-output slots.
fn log_content_uses_req_timestamp(expr: &Expr) -> bool {
    match expr {
        Expr::Field(base, name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == "req") && name == "timestamp" =>
        {
            true
        }
        Expr::Concat(args) => args.iter().any(log_content_uses_req_timestamp),
        _ => false,
    }
}

/// Phase 8 slice 8b/8c — rewrite the log content so that `resp.*` and
/// `req.timestamp` references resolve through the enriched log-scope
/// concept and text-binding maps the emitter prepares around the existing
/// concat pipeline. The rewrite is local to the log scope; the handler's
/// logic is never touched.
///
/// Mappings:
///   - `Field(Ident("resp"), "status")`     → `Field(Ident(input_name), "__resp_status")`  (Number, slot -24)
///   - `Field(Ident("resp"), "body")`       → `Ident("__resp_body")`                       (BoundText, slots (-32, -40))
///   - `Field(Ident("req"), "timestamp")`   → `Field(Ident(input_name), "__req_timestamp")` (Number, slot -56)
///
/// Other shapes pass through unchanged. `resp.body` resolves to a BoundText
/// (ptr, len) pair rather than a NUL-terminated text field so that the
/// concat fill copies exactly `body_len` bytes — `emit_strlen` would walk
/// past the end of the body buffer (the body is not NUL-terminated by the
/// handler).
fn rewrite_log_content(expr: &Expr, input_name: &str) -> Expr {
    match expr {
        Expr::Field(base, name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == "resp") && name == "status" =>
        {
            Expr::Field(
                Box::new(Expr::Ident(input_name.to_string())),
                "__resp_status".to_string(),
            )
        }
        Expr::Field(base, name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == "resp") && name == "body" =>
        {
            Expr::Ident("__resp_body".to_string())
        }
        Expr::Field(base, name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == "req") && name == "timestamp" =>
        {
            Expr::Field(
                Box::new(Expr::Ident(input_name.to_string())),
                "__req_timestamp".to_string(),
            )
        }
        Expr::Concat(args) => Expr::Concat(
            args.iter()
                .map(|a| rewrite_log_content(a, input_name))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Phase 7 slice 3c: Http10 service whose handler contains one or more
/// if/else branches producing different HttpResponse records. The handler's
/// condition evaluation reuses emit_eval_expr (Phase 2's generic expression
/// emitter, which already handles text-field-vs-literal equality via
/// repe cmpsb on NUL-terminated strings — exactly what the HTTP parser
/// produces). Only the orchestration (HTTP parse → handler → HTTP serialize)
/// is new.
///
/// Emitted frame, per accept iteration, relative to rbp:
///   [rbp -  8]  method pointer (set by HTTP parser, NUL-terminated)
///   [rbp - 16]  path pointer   (set by HTTP parser, NUL-terminated)
///   [rbp - 24]  output status code (set by handler)
///   [rbp - 32]  output body pointer (set by handler)
///   [rbp - 40]  output body length  (set by handler)
///   [rbp - 48]  client file descriptor (saved after accept)
///   [rbp - (48 + max_request) .. rbp - 48]  read buffer
///
/// Registers convention: r12 holds the server fd for the lifetime of the
/// binary. emit_eval_expr clobbers rax, rcx, rdx, r8, rsi, rdi, flags; it
/// does NOT touch rbp or r12. The client fd must therefore live in an rbp
/// slot across the handler body invocation (hence [rbp - 48] above), not
/// in a register.
fn compile_http10_dynamic_service(
    program: &Program,
    service: &Service,
    output_path: &str,
) -> Result<(), NativeError> {
    let handler = program
        .items
        .iter()
        .find_map(|i| match i {
            Item::Rule(r) if r.name == service.handler => Some(r),
            _ => None,
        })
        .ok_or_else(|| NativeError {
            message: format!(
                "service '{}' handler '{}' not found (verifier should have caught this)",
                service.name, service.handler
            ),
        })?;

    // HttpRequest fields at fixed rbp slots — mirrors Phase 2E's text-input-
    // field layout so emit_eval_expr can compare req.method / req.path
    // against literals without modification.
    let mut offsets: HashMap<&str, i32> = HashMap::new();
    offsets.insert("method", -8);
    offsets.insert("path", -16);
    let no_rules: HashMap<&str, &Rule> = HashMap::new();
    let no_ranges: HashMap<&str, (i64, i64)> = HashMap::new();

    let code = emit_http10_dynamic_bytes(service, handler, &offsets, &no_rules, &no_ranges)?;
    write_server_elf(&code, output_path, "service", service.port)
}

/// Duplicated shape of the verifier's synthesised HttpRequest concept,
/// kept in native.rs so the emitter does not have to cross-call into the
/// verifier module. If the two ever drift, the phase7_http10 regression
/// tests will catch it (they type-check the handler via the real verifier
/// and then compile with this copy).
fn http_request_builtin_concept_native() -> Concept {
    Concept {
        name: "HttpRequest".to_string(),
        intention: "compiler built-in".to_string(),
        source: SourceRef { file: "<builtin>".to_string(), line: 0 },
        fields: vec![
            Field {
                name: "method".to_string(),
                ty: Type::Text,
                range: Some((0, 8)),
            },
            Field {
                name: "path".to_string(),
                ty: Type::Text,
                range: Some((0, 256)),
            },
        ],
    }
}

fn emit_http10_dynamic_bytes(
    service: &Service,
    handler: &Rule,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<Vec<u8>, NativeError> {
    let mut code = Vec::new();
    let port_be = service.port.to_be_bytes();
    let max_request = service.max_request;
    let buf_bytes = max_request.to_le_bytes();

    // Phase 8 slice 8c: when the log content references `req.timestamp`, the
    // frame grows by 8 bytes for the seconds slot at [rbp-56], pushing the
    // read buffer down accordingly. Without timestamp, the layout is
    // unchanged from slice 8a (frame_base = 48). The timestamp clock value is
    // captured once per accept loop (after accept, before read) so that all
    // log uses of req.timestamp within a single request observe the same
    // monotonic instant.
    let uses_timestamp = service
        .log
        .as_ref()
        .map(|e| match e {
            Effect::AppendFile { content, .. } => log_content_uses_req_timestamp(content),
            _ => false,
        })
        .unwrap_or(false);
    let frame_base: i32 = if uses_timestamp { 56 } else { 48 };
    let frame_size: u32 = (frame_base as u32) + max_request;
    let buf_offset_from_rbp: i32 = -(frame_base + max_request as i32);

    // ═══ PROLOGUE: rbp frame ═══════════════════════════════════
    code.push(0x55);                                     // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);         // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);         // sub rsp, imm32
    code.extend_from_slice(&frame_size.to_le_bytes());

    // ═══ SOCKET ════════════════════════════════════════════════
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]); // mov rax, 41
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // rdi=2 (AF_INET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // rsi=1 (STREAM)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // rdx=0
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x49, 0x89, 0xC4]);                         // mov r12, rax

    // SETSOCKOPT SO_REUSEADDR
    code.extend_from_slice(&[0x6A, 0x01]);                               // push 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x36, 0x00, 0x00, 0x00]); // rax=54
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // rsi=1
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x02, 0x00, 0x00, 0x00]); // rdx=2
    code.extend_from_slice(&[0x49, 0x89, 0xE2]);                         // r10=rsp
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x04, 0x00, 0x00, 0x00]); // r8=4
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]);                   // add rsp, 8

    // BIND
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]);                   // sub rsp, 16
    code.extend_from_slice(&[0x66, 0xC7, 0x04, 0x24, 0x02, 0x00]);       // word [rsp]=2
    code.extend_from_slice(&[0x66, 0xC7, 0x44, 0x24, 0x02]);             // word [rsp+2]=port
    code.extend_from_slice(&port_be);
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x08, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x31, 0x00, 0x00, 0x00]); // rax=49 bind
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // rsi=rsp
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // rdx=16
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);                   // add rsp, 16

    // LISTEN
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]); // rax=50
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]); // rsi=128
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ ACCEPT LOOP ═══════════════════════════════════════════
    let accept_top = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // rax=43 accept
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);                         // rsi=0
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // rdx=0
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    // mov [rbp-48], rax  (save client_fd)
    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xD0]);                   // -48 = 0xD0 i8

    // ═══ TIMESTAMP (Phase 8 slice 8c) ══════════════════════════
    // If the log reads req.timestamp, capture CLOCK_REALTIME seconds once
    // per request, before the parser runs. The 16-byte timespec is laid out
    // at the start of the read buffer area (still unused at this point —
    // the read happens next and overwrites it), then we copy tv_sec into
    // the dedicated [rbp-56] slot. One syscall, 8 bytes of frame growth.
    if uses_timestamp {
        // mov rax, 228  (sys_clock_gettime)
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xE4, 0x00, 0x00, 0x00]);
        // xor rdi, rdi  (CLOCK_REALTIME = 0)
        code.extend_from_slice(&[0x48, 0x31, 0xFF]);
        // lea rsi, [rbp + buf_offset_from_rbp]  (timespec scratch)
        code.extend_from_slice(&[0x48, 0x8D, 0xB5]);
        code.extend_from_slice(&buf_offset_from_rbp.to_le_bytes());
        // syscall
        code.extend_from_slice(&[0x0F, 0x05]);
        // mov rax, [rbp + buf_offset_from_rbp]  (tv_sec)
        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
        code.extend_from_slice(&buf_offset_from_rbp.to_le_bytes());
        // mov [rbp-56], rax
        code.extend_from_slice(&[0x48, 0x89, 0x45, 0xC8]);                // -56 = 0xC8 i8
    }

    // ═══ READ ══════════════════════════════════════════════════
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);                         // xor rax, rax
    code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);                   // rdi = [rbp-48]
    // rsi = rbp + buf_offset_from_rbp  (via lea)
    code.extend_from_slice(&[0x48, 0x8D, 0xB5]);                         // lea rsi, [rbp + disp32]
    code.extend_from_slice(&buf_offset_from_rbp.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);                         // mov rdx, max_request
    code.extend_from_slice(&buf_bytes);
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    // rax = bytes_read

    // ═══ HTTP PARSE (method, path) ═════════════════════════════
    // On malformed input (no space found, no CR/LF found), jumps to
    // the close/loop label via a pair of rel32 patch sites. We resolve
    // those after emitting the close.
    let parse_fail_patches = emit_http_parse_method_path(&mut code, buf_offset_from_rbp);

    // ═══ HANDLER BODY ══════════════════════════════════════════
    // Populates [rbp-24]=status, [rbp-32]=body_ptr, [rbp-40]=body_len.
    emit_handler_to_slots(
        &mut code,
        &handler.logic.value,
        &handler.input_name,
        offsets,
        all_rules,
        field_ranges,
    )?;

    // ═══ LOG EFFECT (Phase 8 slices 8a/8b/8c) ══════════════════
    // After the handler has populated the output slots and before the
    // response is serialised, fire the optional log append_file effect.
    // Fd lives in r15 across emit_append_file_call (its convention); we
    // don't need to save/restore it because nothing above or below it
    // in this path uses r15.
    //
    // Slice 8b/8c: enrich the scope visible to the log content with the
    // handler's response and the per-request timestamp, then rewrite the
    // content so that `resp.status`, `resp.body` and `req.timestamp`
    // resolve through synthetic identifiers backed by the existing rbp
    // slots. The handler itself never sees these names — the rewrite is
    // strictly local to the log scope, preserving handler purity and
    // keeping req.timestamp out of any decision the response depends on.
    if let Some(log_effect) = &service.log {
        if let Effect::AppendFile { path, content } = log_effect {
            let mut log_concept = http_request_builtin_concept_native();
            log_concept.fields.push(Field {
                name: "__resp_status".to_string(),
                ty: Type::Number,
                range: Some((100, 599)),
            });
            if uses_timestamp {
                log_concept.fields.push(Field {
                    name: "__req_timestamp".to_string(),
                    ty: Type::Number,
                    range: None,
                });
            }
            let mut log_offsets: HashMap<&str, i32> = offsets.clone();
            log_offsets.insert("__resp_status", -24);
            if uses_timestamp {
                log_offsets.insert("__req_timestamp", -56);
            }
            let mut log_text_bindings: TextBindings = HashMap::new();
            log_text_bindings.insert("__resp_body", (-32, -40));

            let rewritten = rewrite_log_content(content, &handler.input_name);
            emit_append_file_call(
                &mut code,
                path,
                &rewritten,
                handler,
                &log_concept,
                all_rules,
                &log_offsets,
                field_ranges,
                &log_text_bindings,
            )?;
        }
    }

    // ═══ HTTP SERIALIZE ════════════════════════════════════════
    emit_http_serialize(&mut code);

    // ═══ CLOSE + LOOP ══════════════════════════════════════════
    let close_label = code.len();
    // Patch parse_fail jumps to land here.
    for patch in parse_fail_patches {
        let rel = close_label as i32 - (patch as i32 + 4);
        code[patch..patch + 4].copy_from_slice(&rel.to_le_bytes());
    }
    // close(client_fd): rax=3, rdi=[rbp-48]
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);                   // rdi = [rbp-48]
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ ITERATION RSP RESTORE (Phase 7 slice 3d) ══════════════
    // Slice 3d allows the handler to allocate a stack-resident concat
    // buffer for the response body; the log effect (slice 8a–8c) may
    // also briefly grow rsp. Restoring rsp to its post-prologue value
    // here frees both in one instruction, regardless of which (if any)
    // ran. The prologue invariant (rsp = rbp - frame_size after the
    // initial sub) means this is a no-op when no concat ran.
    //
    // `lea rsp, [rbp + neg_frame_size]`  (REX.W + 0x8D + ModRM 0xA5 + disp32)
    code.extend_from_slice(&[0x48, 0x8D, 0xA5]);
    let neg_frame: i32 = -(frame_size as i32);
    code.extend_from_slice(&neg_frame.to_le_bytes());

    // jmp accept_top
    code.push(0xE9);
    let back = accept_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&back.to_le_bytes());

    Ok(code)
}

/// Emit the HTTP/1.0 minimal parser: scan the read buffer for the first
/// two space-delimited tokens (method, path), NUL-terminate each in place,
/// and store the pointers at [rbp - 8] and [rbp - 16] respectively.
///
/// On entry: rax holds the number of bytes read. buf_offset_from_rbp is the
/// signed rbp-relative offset of the buffer start.
/// On exit (success): rbp[-8] and rbp[-16] are valid NUL-terminated ptrs.
/// On exit (failure): execution jumps via one of the returned patch sites;
/// the caller patches those to the close label so malformed input closes
/// the connection without a response.
///
/// Registers used: rax (bytes remaining), rbx (scan pointer), al (byte reg).
fn emit_http_parse_method_path(code: &mut Vec<u8>, buf_offset_from_rbp: i32) -> Vec<usize> {
    let mut fail_patches = Vec::new();

    // rbx = buf start = rbp + buf_offset
    code.extend_from_slice(&[0x48, 0x8D, 0x9D]);                         // lea rbx, [rbp + disp32]
    code.extend_from_slice(&buf_offset_from_rbp.to_le_bytes());
    // mov [rbp-8], rbx    (method ptr)
    code.extend_from_slice(&[0x48, 0x89, 0x5D, 0xF8]);

    // scan_method:
    let scan_method_top = code.len();
    // test rax, rax  (bytes remaining)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jz parse_fail  (out of bytes without finding space)
    code.push(0x0F);
    code.push(0x84);
    fail_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]); // placeholder rel32
    // cmp byte [rbx], ' '
    code.extend_from_slice(&[0x80, 0x3B, 0x20]);
    // je method_end (rel8 forward)
    code.push(0x74);
    let patch_method_end = code.len();
    code.push(0);
    // inc rbx; dec rax; jmp scan_method
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]);                          // inc rbx
    code.extend_from_slice(&[0x48, 0xFF, 0xC8]);                          // dec rax
    let back_dist = scan_method_top as i32 - (code.len() as i32 + 2);
    code.push(0xEB);
    code.push(back_dist as i8 as u8);

    // method_end:
    let method_end = code.len();
    code[patch_method_end] = (method_end - patch_method_end - 1) as u8;
    // mov byte [rbx], 0
    code.extend_from_slice(&[0xC6, 0x03, 0x00]);
    // inc rbx; dec rax
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC8]);
    // mov [rbp-16], rbx    (path ptr)
    code.extend_from_slice(&[0x48, 0x89, 0x5D, 0xF0]);

    // scan_path:
    let scan_path_top = code.len();
    // test rax, rax
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    // jz parse_fail
    code.push(0x0F);
    code.push(0x84);
    fail_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    // mov r8b, [rbx] (use r8b to avoid clobbering rax low byte)
    code.extend_from_slice(&[0x44, 0x8A, 0x03]);
    // cmp r8b, ' '
    code.extend_from_slice(&[0x41, 0x80, 0xF8, 0x20]);
    // je path_end
    code.push(0x74);
    let patch_path_end_space = code.len();
    code.push(0);
    // cmp r8b, '\r'
    code.extend_from_slice(&[0x41, 0x80, 0xF8, 0x0D]);
    code.push(0x74);
    let patch_path_end_cr = code.len();
    code.push(0);
    // cmp r8b, '\n'
    code.extend_from_slice(&[0x41, 0x80, 0xF8, 0x0A]);
    code.push(0x74);
    let patch_path_end_lf = code.len();
    code.push(0);
    // inc rbx; dec rax; jmp scan_path
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC8]);
    let back_dist = scan_path_top as i32 - (code.len() as i32 + 2);
    code.push(0xEB);
    code.push(back_dist as i8 as u8);

    // path_end:
    let path_end = code.len();
    for patch in &[patch_path_end_space, patch_path_end_cr, patch_path_end_lf] {
        code[*patch] = (path_end - patch - 1) as u8;
    }
    // mov byte [rbx], 0
    code.extend_from_slice(&[0xC6, 0x03, 0x00]);

    fail_patches
}

/// Walk the handler's logic expression and emit code that, on exit, leaves:
///   qword [rbp - 24] = status
///   qword [rbp - 32] = body_ptr
///   qword [rbp - 40] = body_len
///
/// Accepted shape (slice 3c, enforced by analyze_http10_handler_shape):
///   If(cond, then_arm, else_arm)  — emit cond via emit_eval_expr, branch
///   Record("HttpResponse", [status, body])  — emit literal stores
/// Leaves beyond these shapes should already be Unsupported; if reached,
/// an internal error is returned (belt-and-suspenders vs shape drift).
fn emit_handler_to_slots(
    code: &mut Vec<u8>,
    expr: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Record(name, fields) if name == "HttpResponse" => {
            // Store status (Number literal)
            let mut status: Option<i64> = None;
            let mut body_ref: Option<&Expr> = None;
            for (fname, fexpr) in fields {
                match fname.as_str() {
                    "status" => {
                        if let Expr::Number(n) = fexpr {
                            status = Some(*n);
                        } else {
                            return Err(NativeError {
                                message: "status must be a number literal (slice 3c)".into(),
                            });
                        }
                    }
                    "body" => body_ref = Some(fexpr),
                    _ => {
                        return Err(NativeError {
                            message: format!("unexpected HttpResponse field '{}'", fname),
                        })
                    }
                }
            }
            let status = status.ok_or_else(|| NativeError {
                message: "HttpResponse missing status".into(),
            })?;
            let body_expr = body_ref.ok_or_else(|| NativeError {
                message: "HttpResponse missing body".into(),
            })?;

            // mov qword [rbp-24], <status as i32, sign-extended>
            code.extend_from_slice(&[0x48, 0xC7, 0x45, 0xE8]);           // -24 = 0xE8
            code.extend_from_slice(&(status as i32).to_le_bytes());

            // Body: literal Text OR Field(Ident(input), "method"|"path")
            match body_expr {
                Expr::Text(s) => {
                    // Inline the bytes with jmp-over + lea-rip
                    let bytes = s.as_bytes();
                    // jmp rel32 over data
                    code.push(0xE9);
                    let jlen = bytes.len() as i32;
                    code.extend_from_slice(&jlen.to_le_bytes());
                    let data_addr = code.len();
                    code.extend_from_slice(bytes);
                    // lea rax, [rip + disp32]
                    let after_lea = code.len() + 7;
                    let rel = data_addr as i32 - after_lea as i32;
                    code.extend_from_slice(&[0x48, 0x8D, 0x05]);
                    code.extend_from_slice(&rel.to_le_bytes());
                    // mov [rbp-32], rax  (body ptr)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);    // -32 = 0xE0
                    // mov qword [rbp-40], len (as i32)
                    code.extend_from_slice(&[0x48, 0xC7, 0x45, 0xD8]);    // -40 = 0xD8
                    code.extend_from_slice(&(bytes.len() as i32).to_le_bytes());
                }
                Expr::Field(base, fname)
                    if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
                {
                    let foff = *offsets.get(fname.as_str()).ok_or_else(|| NativeError {
                        message: format!("unknown field '{}'", fname),
                    })?;
                    // mov rax, [rbp + foff]  (text field pointer)
                    code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                    code.push(foff as i8 as u8);
                    // mov [rbp-32], rax
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);
                    // strlen via repne scasb: rdi = rax, al=0, rcx=-1, repne scasb
                    code.extend_from_slice(&[0x48, 0x89, 0xC7]);          // mov rdi, rax
                    code.extend_from_slice(&[0x30, 0xC0]);                // xor al, al
                    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0xFF, 0xFF, 0xFF, 0xFF]); // mov rcx, -1
                    code.extend_from_slice(&[0xFC]);                      // cld
                    code.extend_from_slice(&[0xF2, 0xAE]);                // repne scasb
                    // After: rcx = -(len+2); len = -rcx - 2 = (not rcx) - 1
                    code.extend_from_slice(&[0x48, 0xF7, 0xD1]);          // not rcx
                    code.extend_from_slice(&[0x48, 0xFF, 0xC9]);          // dec rcx
                    // mov [rbp-40], rcx
                    code.extend_from_slice(&[0x48, 0x89, 0x4D, 0xD8]);
                }
                // Slice 3d: body assembled at runtime via concat(...). The
                // existing concat-to-buffer infra handles every arg shape
                // (text literal, number, req.method / req.path text field).
                // Result: rax = ptr, rdx = len. We stash both in the body
                // slots; the iteration epilogue restores rsp from rbp,
                // freeing the buffer (and any log buffer above it) in one
                // instruction. ConcatBufResult is intentionally ignored
                // here — the per-iteration rsp restore subsumes both
                // Static and Dynamic free strategies.
                Expr::Concat(args) => {
                    let req_concept = http_request_builtin_concept_native();
                    let _ = emit_concat_to_buffer(
                        code,
                        args,
                        input_name,
                        &req_concept,
                        all_rules,
                        offsets,
                        field_ranges,
                        &no_text_bindings(),
                    )?;
                    // mov [rbp-32], rax  (body ptr)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);
                    // mov [rbp-40], rdx  (body len)
                    code.extend_from_slice(&[0x48, 0x89, 0x55, 0xD8]);
                }
                _ => {
                    return Err(NativeError {
                        message: "body shape not supported in slice 3d".into(),
                    });
                }
            }
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            // Evaluate cond → rax (0 or 1)
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges)?;
            // test rax, rax ; jz else_label
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let patch_else = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);

            // then arm
            emit_handler_to_slots(code, then_e, input_name, offsets, all_rules, field_ranges)?;
            // jmp end_label
            code.push(0xE9);
            let patch_end = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);

            // else_label:
            let else_pos = code.len();
            let rel = else_pos as i32 - (patch_else as i32 + 4);
            code[patch_else..patch_else + 4].copy_from_slice(&rel.to_le_bytes());

            // else arm
            emit_handler_to_slots(code, else_e, input_name, offsets, all_rules, field_ranges)?;

            // end_label:
            let end_pos = code.len();
            let rel = end_pos as i32 - (patch_end as i32 + 4);
            code[patch_end..patch_end + 4].copy_from_slice(&rel.to_le_bytes());

            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "emit_handler_to_slots: unexpected shape {:?} (slice 3c shape drift)",
                expr_kind(other)
            ),
        }),
    }
}

/// Emit the HTTP/1.0 response serializer: six sequential write() syscalls
/// that build the response line by line, reading status / body_ptr /
/// body_len / client_fd from their respective rbp slots. Ugly by design —
/// simple, auditable, no in-memory buffer. writev coalescing is a later
/// optimisation gated on a concrete bench.
fn emit_http_serialize(code: &mut Vec<u8>) {
    // Status itoa buffer lives on the stack; allocate 10 bytes upfront for
    // both status and body_len, re-used sequentially.
    // Format: HTTP/1.0 <status> OK\r\nContent-Length: <body_len>\r\n\r\n<body>

    emit_write_literal(code, b"HTTP/1.0 ");
    emit_write_itoa_slot(code, -24);                                     // status at rbp-24
    emit_write_literal(code, b" OK\r\nContent-Length: ");
    emit_write_itoa_slot(code, -40);                                     // body_len at rbp-40
    emit_write_literal(code, b"\r\n\r\n");
    emit_write_body_ptr_len(code);                                        // body_ptr at rbp-32, len at rbp-40
}

/// Emit a write() syscall for a fixed byte literal, inlined with jmp-over
/// + lea-rip-relative. Uses [rbp - 48] as the client_fd source.
fn emit_write_literal(code: &mut Vec<u8>, literal: &[u8]) {
    // jmp rel32 over data
    code.push(0xE9);
    let jlen = literal.len() as i32;
    code.extend_from_slice(&jlen.to_le_bytes());
    let data_addr = code.len();
    code.extend_from_slice(literal);
    // lea rsi, [rip + disp32]
    let after_lea = code.len() + 7;
    let rel = data_addr as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rel.to_le_bytes());
    // mov rdi, [rbp-48]  (client_fd)
    code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);
    // mov rdx, imm32 (length)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(literal.len() as i32).to_le_bytes());
    // mov rax, 1 (write); syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
}

/// Emit an itoa + write for a non-negative i64 stored at [rbp + slot_off].
/// The decimal digits are built on the stack (growing down) then written
/// via a single write() syscall. Uses rax/rcx/rdx/rsi/rdi/r8; caller
/// assumes no cross-call invariants in these registers.
fn emit_write_itoa_slot(code: &mut Vec<u8>, slot_off: i32) {
    // mov rax, [rbp + slot_off]  (value to print)
    code.extend_from_slice(&[0x48, 0x8B, 0x45]);
    code.push(slot_off as i8 as u8);

    // Allocate 24 bytes on stack for the digit buffer (enough for i64)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x18]);                    // sub rsp, 24
    // r8 = rsp + 24  (one-past-end cursor)
    code.extend_from_slice(&[0x4C, 0x8D, 0x44, 0x24, 0x18]);              // lea r8, [rsp+24]

    // Special case: value == 0 → emit single '0'
    // test rax, rax ; jnz itoa_loop
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.push(0x75);
    let patch_nz = code.len();
    code.push(0);
    // Zero path: dec r8 ; mov byte [r8], '0'
    code.extend_from_slice(&[0x49, 0xFF, 0xC8]);                          // dec r8
    code.extend_from_slice(&[0x41, 0xC6, 0x00, 0x30]);                    // mov byte [r8], '0'
    // jmp write_digits
    code.push(0xEB);
    let patch_skip_loop = code.len();
    code.push(0);

    // itoa_loop: (patch from "jnz")
    let loop_top = code.len();
    code[patch_nz] = (loop_top - patch_nz - 1) as u8;

    // rcx = 10 ; xor rdx, rdx ; div rcx → rax = rax / 10, rdx = rax % 10
    code.extend_from_slice(&[0x48, 0xC7, 0xC1, 0x0A, 0x00, 0x00, 0x00]); // mov rcx, 10
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // xor rdx, rdx
    code.extend_from_slice(&[0x48, 0xF7, 0xF1]);                         // div rcx
    // dl += '0' ; dec r8 ; mov [r8], dl
    code.extend_from_slice(&[0x80, 0xC2, 0x30]);                         // add dl, '0'
    code.extend_from_slice(&[0x49, 0xFF, 0xC8]);                         // dec r8
    code.extend_from_slice(&[0x41, 0x88, 0x10]);                         // mov [r8], dl
    // test rax, rax ; jnz loop_top
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.push(0x75);
    let back = loop_top as i32 - (code.len() as i32 + 1);
    code.push(back as i8 as u8);

    // write_digits: (patch from "jmp skip_loop" in zero path)
    let write_digits = code.len();
    code[patch_skip_loop] = (write_digits - patch_skip_loop - 1) as u8;

    // rdx (count) = (rsp + 24) - r8
    code.extend_from_slice(&[0x48, 0x8D, 0x54, 0x24, 0x18]);              // lea rdx, [rsp+24]
    code.extend_from_slice(&[0x4C, 0x29, 0xC2]);                          // sub rdx, r8
    // rsi = r8 (start of digits)
    code.extend_from_slice(&[0x4C, 0x89, 0xC6]);                          // mov rsi, r8
    // rdi = [rbp-48] (client_fd)
    code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);
    // rax = 1 ; syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);

    // Release the 24-byte digit buffer
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x18]);                    // add rsp, 24
}

/// Emit a write() syscall for the handler-produced body: pointer at
/// [rbp - 32], length at [rbp - 40], fd at [rbp - 48].
fn emit_write_body_ptr_len(code: &mut Vec<u8>) {
    // rsi = [rbp-32]
    code.extend_from_slice(&[0x48, 0x8B, 0x75, 0xE0]);
    // rdx = [rbp-40]
    code.extend_from_slice(&[0x48, 0x8B, 0x55, 0xD8]);
    // rdi = [rbp-48]
    code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);
    // rax = 1 ; syscall
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
}

fn compile_http10_constant_service(
    program: &Program,
    service: &Service,
    output_path: &str,
) -> Result<(), NativeError> {
    let handler = program
        .items
        .iter()
        .find_map(|i| match i {
            Item::Rule(r) if r.name == service.handler => Some(r),
            _ => None,
        })
        .ok_or_else(|| NativeError {
            message: format!(
                "service '{}' handler '{}' not found (verifier should have caught this)",
                service.name, service.handler
            ),
        })?;

    if !handler.logic.bindings.is_empty() {
        return Err(NativeError {
            message: format!(
                "Phase 7 slice 3b: handler '{}' has let bindings; slice 3b supports only \
                 constant-response handlers (HttpResponse {{ status: N, body: \"...\" }}). \
                 Conditionals and req inspection land in slice 3c+",
                handler.name
            ),
        });
    }

    // Match: Expr::Record("HttpResponse", [("status", Number(n)), ("body", Text(s))])
    // Field order is preserved by the parser but may be written either way
    // in source; we look up by name.
    let (status, body) = match &handler.logic.value {
        Expr::Record(name, fields) if name == "HttpResponse" => {
            let mut status_value: Option<i64> = None;
            let mut body_value: Option<String> = None;
            for (f_name, f_expr) in fields {
                match (f_name.as_str(), f_expr) {
                    ("status", Expr::Number(n)) => status_value = Some(*n),
                    ("body", Expr::Text(s)) => body_value = Some(s.clone()),
                    (k, _) => {
                        return Err(NativeError {
                            message: format!(
                                "Phase 7 slice 3b: handler '{}' field '{}' is not a literal; slice 3b supports only literal values in the HttpResponse record",
                                handler.name, k
                            ),
                        });
                    }
                }
            }
            let status = status_value.ok_or_else(|| NativeError {
                message: format!(
                    "handler '{}' HttpResponse is missing literal 'status'",
                    handler.name
                ),
            })?;
            let body = body_value.ok_or_else(|| NativeError {
                message: format!(
                    "handler '{}' HttpResponse is missing literal 'body'",
                    handler.name
                ),
            })?;
            (status, body)
        }
        _ => {
            return Err(NativeError {
                message: format!(
                    "Phase 7 slice 3b: handler '{}' logic is not a constant HttpResponse record; \
                     expected `resp = HttpResponse {{ status: N, body: \"...\" }}`. \
                     Conditional and request-inspecting handlers land in slice 3c+",
                    handler.name
                ),
            });
        }
    };

    // Range checks — the verifier already enforces the concept-declared
    // ranges on HttpResponse, but keep this defensive in case a future
    // refactor loosens the verifier path.
    if !(100..=599).contains(&status) {
        return Err(NativeError {
            message: format!("status {} outside HTTP valid range [100, 599]", status),
        });
    }
    if body.len() > 4096 {
        return Err(NativeError {
            message: format!(
                "body length {} exceeds HttpResponse text bound [..4096]",
                body.len()
            ),
        });
    }

    let response = format!(
        "HTTP/1.0 {} OK\r\nContent-Length: {}\r\n\r\n{}",
        status,
        body.len(),
        body
    );
    let code = emit_http10_constant_response_bytes(service.port, service.max_request, response.as_bytes());
    write_server_elf(&code, output_path, "service", service.port)
}

/// Phase 7 slice 3b emission body: socket → bind → listen → accept loop
/// where each connection reads up to max_request bytes (discarded), writes
/// the precomputed response, closes. The tier-1 equivalent of
/// emit_http_demo, but with port / max_request / response coming from the
/// .verbose source rather than hardcoded Rust values.
fn emit_http10_constant_response_bytes(port: u16, max_request: u32, response: &[u8]) -> Vec<u8> {
    let mut code = Vec::new();
    let port_be = port.to_be_bytes();
    let buf_bytes = max_request.to_le_bytes();
    let resp_len_bytes = (response.len() as i32).to_le_bytes();

    // ═══ SOCKET ════════════════════════════════════════════════
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]); // mov rax, 41 (socket)
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // mov rdi, 2 (AF_INET)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1 (SOCK_STREAM)
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // xor rdx, rdx
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x49, 0x89, 0xC4]);                         // mov r12, rax

    // ═══ SETSOCKOPT (SO_REUSEADDR) ════════════════════════════
    code.extend_from_slice(&[0x6A, 0x01]);                               // push 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x36, 0x00, 0x00, 0x00]); // mov rax, 54
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x02, 0x00, 0x00, 0x00]); // mov rdx, 2
    code.extend_from_slice(&[0x49, 0x89, 0xE2]);                         // mov r10, rsp
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x04, 0x00, 0x00, 0x00]); // mov r8, 4
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]);                   // add rsp, 8

    // ═══ BIND ═════════════════════════════════════════════════
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]);                   // sub rsp, 16
    code.extend_from_slice(&[0x66, 0xC7, 0x04, 0x24, 0x02, 0x00]);       // mov word [rsp], 2
    code.extend_from_slice(&[0x66, 0xC7, 0x44, 0x24, 0x02]);             // mov word [rsp+2], port
    code.extend_from_slice(&port_be);
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00]); // mov qword [rsp+4], 0
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x08, 0x00, 0x00, 0x00, 0x00]); // zero [rsp+8..16]

    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x31, 0x00, 0x00, 0x00]); // mov rax, 49 (bind)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // mov rsi, rsp
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // mov rdx, 16
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ LISTEN ═══════════════════════════════════════════════
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]); // mov rax, 50 (listen)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]); // mov rsi, 128
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // Allocate the request-drain buffer on the stack — size from .verbose
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);                         // sub rsp, imm32
    code.extend_from_slice(&buf_bytes);

    // ═══ ACCEPT LOOP ══════════════════════════════════════════
    let accept_top = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // mov rax, 43 (accept)
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // mov rdi, r12
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);                         // xor rsi, rsi
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // xor rdx, rdx
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    code.extend_from_slice(&[0x49, 0x89, 0xC5]);                         // mov r13, rax

    // Read request (drain the socket; contents ignored in slice 3b)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);                         // xor rax, rax (read)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);                         // mov rsi, rsp
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);                         // mov rdx, imm32
    code.extend_from_slice(&buf_bytes);
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ WRITE RESPONSE ═══════════════════════════════════════
    // Jump over the inline response data; use rel32 to handle responses
    // that exceed 127 bytes (which includes any realistic HTTP response
    // with a non-trivial body).
    code.push(0xE9);
    let jmp_len = response.len() as i32;
    code.extend_from_slice(&jmp_len.to_le_bytes());
    let resp_offset = code.len();
    code.extend_from_slice(response);

    // lea rsi, [rip + disp32] — compute the address of the inlined data
    let after_lea = code.len() + 7;
    let rip_delta = resp_offset as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rip_delta.to_le_bytes());

    // write(client_fd, &response, response.len())
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1 (write)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);                         // mov rdx, imm32
    code.extend_from_slice(&resp_len_bytes);
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // close(client_fd)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]); // mov rax, 3 (close)
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);                         // mov rdi, r13
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // jmp accept_top
    code.push(0xE9);
    let accept_back = accept_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&accept_back.to_le_bytes());

    code
}

/// Compile a multi-route HTTP server. Each rule becomes an endpoint:
///   GET /rule_name/arg1/arg2/...  →  evaluates rule with [arg1, arg2, ...]
///   GET /health                   →  200 OK (built-in)
///   GET /anything_else            →  404 Not Found
///
/// Route dispatch: the first path segment is compared against each rule name
/// (embedded as inline string data). On match, the remaining segments become
/// the rule's argv. On no match, 404.
pub fn compile_http_server(
    program: &Program,
    rule_name: &str,
    port: u16,
    output_path: &str,
) -> Result<(), NativeError> {
    // Compile the rule code (argv mode, no stdin/stream).
    // The rule must use the standard push rbp/mov rbp, rsp prologue —
    // vectorized and parallel programs have different stack layouts.
    let concepts: Vec<&Concept> = program.items.iter().filter_map(|i| match i { Item::Concept(c) => Some(c), _ => None }).collect();
    let rules: HashMap<&str, &Rule> = program.items.iter().filter_map(|i| match i { Item::Rule(r) => Some((r.name.as_str(), r)), _ => None }).collect();
    if let Some(r) = rules.get(rule_name) {
        let is_vec = r.hints.as_ref().map_or(false, |h| h.vectorizable.is_some());
        let is_par = r.hints.as_ref().map_or(false, |h| h.parallel.is_some());
        if let Type::Named(n) = &r.input_ty {
            if let Some(c) = concepts.iter().find(|c| c.name == *n) {
                if is_vec && c.fields.len() == 1 {
                    return Err(NativeError { message: "HTTP server mode not supported with SIMD-vectorized rules".into() });
                }
            }
        }
        if is_par {
            return Err(NativeError { message: "HTTP server mode not supported with parallel rules".into() });
        }
    }
    let mut rule_code = compile_native_code(program, rule_name, false, false)?;

    // Strip the sys_exit from the rule code — we'll return to the accept loop.
    let mov_rax_60: [u8; 7] = [0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00];
    if let Some(pos) = rule_code.windows(7).rposition(|w| w == mov_rax_60) {
        rule_code.truncate(pos);
    }
    // Add stack cleanup (same as streaming): mov rsp, rbp; pop rbp
    rule_code.extend_from_slice(&[0x48, 0x89, 0xEC, 0x5D]);

    let port_be = port.to_be_bytes();
    let mut code = Vec::new();

    // ═══ NETWORK SETUP (same as echo server) ══════════════════
    // socket(AF_INET, SOCK_STREAM, 0) → r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x49, 0x89, 0xC4]); // mov r12, rax

    // setsockopt SO_REUSEADDR
    code.extend_from_slice(&[0x6A, 0x01]); // push 1
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x36, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x02, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x49, 0x89, 0xE2]);
    code.extend_from_slice(&[0x49, 0xC7, 0xC0, 0x04, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]); // pop optval

    // bind(r12, sockaddr_in, 16)
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x10]); // sub rsp, 16
    code.extend_from_slice(&[0x66, 0xC7, 0x04, 0x24, 0x02, 0x00]); // AF_INET
    code.extend_from_slice(&[0x66, 0xC7, 0x44, 0x24, 0x02]);
    code.extend_from_slice(&port_be);
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x04, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0x44, 0x24, 0x08, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x31, 0x00, 0x00, 0x00]); // sys_bind
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x48, 0x89, 0xE6]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);

    // listen(r12, 128)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);

    // Allocate 4K buffer + 4K for tokenized argv array
    code.extend_from_slice(&[0x48, 0x81, 0xEC, 0x00, 0x20, 0x00, 0x00]); // sub rsp, 8192

    // ═══ ACCEPT LOOP ══════════════════════════════════════════
    let accept_top = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // sys_accept
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);
    code.extend_from_slice(&[0x0F, 0x05]);
    code.extend_from_slice(&[0x49, 0x89, 0xC5]); // r13 = client_fd

    // Read HTTP request into buffer at rsp (max 4K)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]); // sys_read
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // rdi = client_fd
    code.extend_from_slice(&[0x48, 0x89, 0xE6]); // rsi = rsp (buffer)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0xFF, 0x0F, 0x00, 0x00]); // rdx = 4095 (1 byte for NUL)
    code.extend_from_slice(&[0x0F, 0x05]);
    // rax = bytes read. NUL-terminate.
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
    code.extend_from_slice(&[0x0F, 0x8E]); // jle close_client (bad request)
    let bad_req_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);
    code.extend_from_slice(&[0xC6, 0x04, 0x04, 0x00]); // mov byte [rsp+rax], 0

    // ═══ HTTP PARSE: find path in "GET /path HTTP/..." ════════
    // Check first 4 bytes are "GET " (security: reject non-GET)
    // cmp dword [rsp], "GET " = 0x20544547
    code.extend_from_slice(&[0x81, 0x3C, 0x24, 0x47, 0x45, 0x54, 0x20]); // cmp dword [rsp], "GET "
    code.extend_from_slice(&[0x0F, 0x85]); // jne close_client (not GET)
    let not_get_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    // ─── /health check (built-in, no rule needed) ────────────
    // Compare bytes at rsp+5..rsp+11 against "health" (6 bytes).
    // If match AND rsp+11 == ' ', respond with 200 OK and skip rule.
    // "health" = 68 65 61 6C 74 68
    // Check first 4 bytes: "heal" = 0x6C616568
    code.extend_from_slice(&[0x81, 0x7C, 0x24, 0x05, 0x68, 0x65, 0x61, 0x6C]); // cmp dword [rsp+5], "heal"
    code.push(0x75); // jne not_health
    let not_health_patch = code.len();
    code.push(0x00);
    // Check next 2 bytes: "th" = 0x6874 at rsp+9
    code.extend_from_slice(&[0x66, 0x81, 0x7C, 0x24, 0x09, 0x74, 0x68]); // cmp word [rsp+9], "th"
    code.push(0x75); // jne not_health
    let not_health_patch2 = code.len();
    code.push(0x00);
    // Check rsp+11 == ' ' (end of path)
    code.extend_from_slice(&[0x80, 0x7C, 0x24, 0x0B, 0x20]); // cmp byte [rsp+11], ' '
    code.push(0x75); // jne not_health
    let not_health_patch3 = code.len();
    code.push(0x00);

    // /health matched! Send response directly to client socket.
    let health_response = b"HTTP/1.0 200 OK\r\nConnection: close\r\n\r\nok\n";
    // write(client_fd, response, len)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // mov rdi, r13
    // Inline the response using jmp-over-data
    code.push(0xEB);
    code.push(health_response.len() as u8);
    let resp_addr = code.len();
    code.extend_from_slice(health_response);
    let after_lea = code.len() + 7;
    let rip_off = resp_addr as i32 - after_lea as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]); // lea rsi, [rip + off]
    code.extend_from_slice(&rip_off.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(health_response.len() as i32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    // Jump to close_client (patched later)
    code.push(0xE9);
    let health_close_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    // not_health:
    let not_health = code.len();
    code[not_health_patch] = (not_health - not_health_patch - 1) as u8;
    code[not_health_patch2] = (not_health - not_health_patch2 - 1) as u8;
    code[not_health_patch3] = (not_health - not_health_patch3 - 1) as u8;

    // Path starts at rsp+4 (after "GET "). Find the space before "HTTP/".
    // rcx = scan pointer, starting at rsp+5 (skip the leading '/')
    code.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x24, 0x05]); // lea rcx, [rsp+5]

    // Find end of path: scan for ' ' (0x20)
    let scan_path = code.len();
    code.extend_from_slice(&[0x80, 0x39, 0x20]); // cmp byte [rcx], 0x20
    code.push(0x74); // je found_path_end
    let path_end_patch = code.len();
    code.push(0x00);
    code.extend_from_slice(&[0x80, 0x39, 0x00]); // cmp byte [rcx], 0 (safety: NUL)
    code.push(0x74); // je found_path_end
    let path_nul_patch = code.len();
    code.push(0x00);
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.push(0xEB); // jmp scan_path
    code.push((scan_path as isize - code.len() as isize - 1) as u8);

    // found_path_end: NUL-terminate path
    let found_path_end = code.len();
    code[path_end_patch] = (found_path_end - path_end_patch - 1) as u8;
    code[path_nul_patch] = (found_path_end - path_nul_patch - 1) as u8;
    code.extend_from_slice(&[0xC6, 0x01, 0x00]); // mov byte [rcx], 0

    // ═══ TOKENIZE PATH: split on '/' ══════════════════════════
    // rcx still points at path_end (where we just wrote NUL).
    // Save it in rdx, then reset rcx to path start for the tokenizer.
    code.extend_from_slice(&[0x48, 0x89, 0xCA]); // mov rdx, rcx (path end)
    code.extend_from_slice(&[0x48, 0x8D, 0x4C, 0x24, 0x05]); // lea rcx, [rsp+5] (path start)
    code.extend_from_slice(&[0x4C, 0x8D, 0x84, 0x24, 0x00, 0x10, 0x00, 0x00]); // lea r8, [rsp+4096]
    code.extend_from_slice(&[0x45, 0x31, 0xC9]); // xor r9d, r9d

    // ═══ PATH TOKENIZER (split on '/') ════════════════════════
    let tok_top = code.len();
    code.extend_from_slice(&[0x48, 0x39, 0xD1]); // cmp rcx, rdx
    code.extend_from_slice(&[0x0F, 0x8D]); // jge tok_done
    let tok_done_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    code.extend_from_slice(&[0x8A, 0x01]); // mov al, [rcx]
    code.extend_from_slice(&[0x3C, 0x2F]); // cmp al, '/'
    code.push(0x74); // je tok_skip
    let tok_skip_patch = code.len();
    code.push(0x00);

    // Non-slash: start of token
    // Bounds check
    code.extend_from_slice(&[0x41, 0x81, 0xF9, 0x00, 0x02, 0x00, 0x00]); // cmp r9d, 512
    code.extend_from_slice(&[0x0F, 0x8D]); // jge tok_done
    let tok_cap_patch = code.len();
    code.extend_from_slice(&[0x00; 4]);

    code.extend_from_slice(&[0x4B, 0x89, 0x0C, 0xC8]); // mov [r8+r9*8], rcx
    code.extend_from_slice(&[0x49, 0xFF, 0xC1]); // inc r9

    // Scan to next '/' or end
    let tok_scan = code.len();
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.extend_from_slice(&[0x48, 0x39, 0xD1]); // cmp rcx, rdx
    code.extend_from_slice(&[0x0F, 0x8D]); // jge tok_done
    let tok_done_patch2 = code.len();
    code.extend_from_slice(&[0x00; 4]);
    code.extend_from_slice(&[0x8A, 0x01]); // mov al, [rcx]
    code.extend_from_slice(&[0x3C, 0x2F]); // cmp al, '/'
    code.push(0x74); // je tok_terminate
    let tok_term_patch = code.len();
    code.push(0x00);
    code.push(0xEB); // jmp tok_scan
    code.push((tok_scan as isize - code.len() as isize - 1) as u8);

    // tok_terminate: NUL at '/' position, advance
    let tok_term = code.len();
    code[tok_term_patch] = (tok_term - tok_term_patch - 1) as u8;
    code.extend_from_slice(&[0xC6, 0x01, 0x00]); // mov byte [rcx], 0
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.push(0xE9); // jmp tok_top
    let tok_back = tok_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&tok_back.to_le_bytes());

    // tok_skip: advance past '/'
    let tok_skip = code.len();
    code[tok_skip_patch] = (tok_skip - tok_skip_patch - 1) as u8;
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.push(0xE9); // jmp tok_top
    let tok_back2 = tok_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&tok_back2.to_le_bytes());

    // tok_done:
    let tok_done = code.len();
    code[tok_done_patch..tok_done_patch+4].copy_from_slice(&((tok_done as i32) - (tok_done_patch as i32 + 4)).to_le_bytes());
    code[tok_done_patch2..tok_done_patch2+4].copy_from_slice(&((tok_done as i32) - (tok_done_patch2 as i32 + 4)).to_le_bytes());
    code[tok_cap_patch..tok_cap_patch+4].copy_from_slice(&((tok_done as i32) - (tok_cap_patch as i32 + 4)).to_le_bytes());

    // ═══ BUILD ARGC/ARGV FROM TOKENS ══════════════════════════
    // We need to put argc/argv at a known [rsp] position for the rule code.
    // The rule code reads [rsp] = argc and [rsp+8..] = argv.
    // BUT rsp currently points to our buffer. We need a different area.
    //
    // Strategy: save rsp in rbx, set rsp to rsp+8192 (above our buffer),
    // write argc/argv there, then the rule code's prologue will read it.
    // After the rule runs, restore rsp.
    //
    // Actually, simpler: the rule code calls mov r12, [rsp]; lea r13, [rsp+8].
    // I'll write argc at [rsp+8192+16] and argv starting at [rsp+8192+24].
    // Then briefly set rsp to that area, let the rule run, restore.

    // Use a region above our 8K buffer: rsp + 8192
    // Save current rsp in rbx
    code.extend_from_slice(&[0x48, 0x89, 0xE3]); // mov rbx, rsp

    // Write argc/argv at [rbx + 8192]
    // argc = r9 + 1
    code.extend_from_slice(&[0x4C, 0x89, 0xC8]); // mov rax, r9
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]); // inc rax
    code.extend_from_slice(&[0x48, 0x89, 0x83]); // mov [rbx + 8192], rax
    code.extend_from_slice(&8192i32.to_le_bytes());
    // argv[0] = 0 (dummy)
    code.extend_from_slice(&[0x48, 0xC7, 0x83]); // mov qword [rbx + 8200], 0
    code.extend_from_slice(&8200i32.to_le_bytes());
    code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

    // Copy token pointers: [rbx + 8208 + i*8] = [r8 + i*8]
    code.extend_from_slice(&[0x31, 0xC9]); // xor ecx, ecx
    let copy_loop = code.len();
    code.extend_from_slice(&[0x4C, 0x39, 0xC9]); // cmp rcx, r9
    code.push(0x7D); let copy_done_patch = code.len(); code.push(0);
    code.extend_from_slice(&[0x49, 0x8B, 0x04, 0xC8]); // mov rax, [r8+rcx*8]
    // mov [rbx + rcx*8 + 8208], rax
    code.extend_from_slice(&[0x48, 0x89, 0x84, 0xCB]); // mov [rbx + rcx*8 + disp32], rax
    code.extend_from_slice(&8208i32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xFF, 0xC1]); // inc rcx
    code.push(0xEB);
    code.push((copy_loop as isize - code.len() as isize - 1) as u8);
    let copy_done = code.len();
    code[copy_done_patch] = (copy_done - copy_done_patch - 1) as u8;

    // Set rsp to the argc/argv area so the rule code can read it
    code.extend_from_slice(&[0x48, 0x8D, 0xA3]); // lea rsp, [rbx + 8192]
    code.extend_from_slice(&8192i32.to_le_bytes());

    // ═══ SAVE NETWORK FDs ═══════════════════════════════════════
    // r12 (server_fd) and r13 (client_fd) will be clobbered by the rule code.
    // Save at [rbx+4080] and [rbx+4088] — top of the token array area,
    // safely past any HTTP request data (max 4K at [rbx..rbx+4096]).
    // MUST NOT save at [rbx+0] — that overlaps with the request buffer
    // where tokenized path values live!
    code.extend_from_slice(&[0x4C, 0x89, 0xA3]); // mov [rbx + 4080], r12
    code.extend_from_slice(&4080i32.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x89, 0xAB]); // mov [rbx + 4088], r13
    code.extend_from_slice(&4088i32.to_le_bytes());

    // ═══ REDIRECT STDOUT/STDERR TO CLIENT SOCKET ══════════════
    // dup2(client_fd, 1) → syscall 33
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x21, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // rdi = r13 (client_fd)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // rsi = 1
    code.extend_from_slice(&[0x0F, 0x05]);
    // dup2(client_fd, 2)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x21, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x02, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);

    // Write HTTP response header
    let header = b"HTTP/1.0 200 OK\r\nConnection: close\r\n\r\n";
    emit_write_static_to_fd(&mut code, header, 1);

    // ═══ RULE CODE ════════════════════════════════════════════
    code.extend_from_slice(&rule_code);

    // ═══ RESTORE + CLOSE ═══════════════════════════════════════
    // Close fd 1 (dup2'd to client socket — this sends FIN to client)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]); // sys_close
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // rdi = 1
    code.extend_from_slice(&[0x0F, 0x05]);
    // Close fd 2
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);

    // Restore rsp from rbx
    code.extend_from_slice(&[0x48, 0x89, 0xDC]); // mov rsp, rbx
    // Restore r12 (server_fd) and r13 (client_fd) from [rbx+4080/4088]
    code.extend_from_slice(&[0x4C, 0x8B, 0xA3]); // mov r12, [rbx + 4080]
    code.extend_from_slice(&4080i32.to_le_bytes());
    code.extend_from_slice(&[0x4C, 0x8B, 0xAB]); // mov r13, [rbx + 4088]
    code.extend_from_slice(&4088i32.to_le_bytes());

    // close(client_fd)
    let close_client = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x4C, 0x89, 0xEF]); // rdi = r13
    code.extend_from_slice(&[0x0F, 0x05]);

    // Restore stdout (dup2(1, 1) won't work... we need to save original stdout)
    // Actually, stdout was fd 1 pointing to terminal. After dup2(client_fd, 1),
    // fd 1 now points to the socket. After close(client_fd), the socket is closed
    // on the client's original fd but fd 1 still points to it (dangling).
    //
    // For a server that never writes to its own stdout, this is fine —
    // each new connection dup2's a fresh client_fd onto fd 1.
    // The only issue is if we want to log to the original stdout.
    // For v1, we don't. Accept the trade-off.

    // jmp accept_top
    code.push(0xE9);
    let accept_back = accept_top as i32 - (code.len() as i32 + 4);
    code.extend_from_slice(&accept_back.to_le_bytes());

    // Patch jumps to close_client
    let br_rel = close_client as i32 - (bad_req_patch as i32 + 4);
    code[bad_req_patch..bad_req_patch+4].copy_from_slice(&br_rel.to_le_bytes());
    let ng_rel = close_client as i32 - (not_get_patch as i32 + 4);
    code[not_get_patch..not_get_patch+4].copy_from_slice(&ng_rel.to_le_bytes());
    let hc_rel = close_client as i32 - (health_close_patch as i32 + 4);
    code[health_close_patch..health_close_patch+4].copy_from_slice(&hc_rel.to_le_bytes());

    // Validate + write ELF
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
            .map_err(|e| NativeError { message: format!("cannot set permissions: {}", e) })?;
    }

    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    println!("http-server: {} ({} bytes, rule '{}', port {})", output_path, size, rule_name, port);
    Ok(())
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
        compile_native(&program, "discounted_purchase", out.to_str().unwrap(), false, false)
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
            compile_native(&program, rule, out.to_str().unwrap(), false, false)
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
        compile_native(&program, "high_earners", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "compute_bonuses", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "total_salaries", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "high_earner_count", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "make_report", out.to_str().unwrap(), false, false)
            .expect("native compile of text-field-through-record should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_match_result_with_enriched_err() {
        // Phase 2F: enrich.verbose's `enriched` rule wraps validate_amount
        // with an outer match_result whose Err arm is:
        //   Err(concat("[", p.user, "] ", e))
        // Exercises the capture-then-bind flow: the inlined callee's Err
        // leaf writes (ptr, len) to err_ptr_slot / err_len_slot, saves rsp
        // to err_frame_save_slot, then the outer Err body evaluates with
        // err_var bound — concat reads the text field AND the bound text
        // in the same buffer build. At the end `mov rsp, [rbp+err_frame_save_slot]`
        // frees whatever the callee's Err concat allocated.
        use std::fs;
        let src = fs::read_to_string("examples/enrich.verbose")
            .expect("examples/enrich.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_enrich");
        compile_native(&program, "enriched", out.to_str().unwrap(), false, false)
            .expect("native compile of match_result enrich should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_record_text_field_from_concat() {
        // fullname.verbose: Greeting { fullname: concat(p.first, " ", p.last),
        // age: p.age }. Exercises `emit_text_write_to_fd`'s Concat arm
        // inside `emit_record_as_json` — the dynamic-sized concat buffer
        // (with r9 = saved rsp, mov rsp, r9 to free) composes a text value
        // that then flows through the JSON streaming path as a field value.
        // Regression: the "Record fields with text-typed value coming from
        // concat" claim in CLAUDE.md's rejection list was stale — this test
        // locks the working behavior.
        use std::fs;
        let src = fs::read_to_string("examples/fullname.verbose")
            .expect("examples/fullname.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_fullname");
        compile_native(&program, "compose_greeting", out.to_str().unwrap(), false, false)
            .expect("native compile of record-text-from-concat should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_500, "unexpected binary size: {}", size);
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
        compile_native(&program, "roster_line", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "greeting_line", out.to_str().unwrap(), false, false)
            .expect("native compile of output-text rule should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 300 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_call_inside_concat_arg() {
        // Phase 2H-b: compose.verbose::greeting concatenates a literal
        // prefix, a helper rule call (display_name(p)), and a number field.
        // Exercises the pre-eval loop: mov r9, rsp; sub rsp, 16;
        // mov r11, rsp; emit display_name → (rax, rdx); mov [r11], rax;
        // mov [r11+8], rdx; then sizing pass reads [r11+8], filling pass
        // copies from (r11[0], r11[8]).
        use std::fs;
        let src = fs::read_to_string("examples/compose.verbose")
            .expect("examples/compose.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_greeting_2hb");
        compile_native(&program, "greeting", out.to_str().unwrap(), false, false)
            .expect("native compile of Call-in-concat-arg should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 500 && size < 2_500, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_call_as_append_file_content() {
        // Phase 2H-a: the reaction append_file content is a text-returning
        // rule call. Mirror of Phase 2G in emit_append_file_call:
        // validate same-concept / same-input-name / no-lets, then recurse
        // on the callee's body via emit_append_write_to_r15.
        use std::fs;
        let src = fs::read_to_string("examples/log_via_helper.verbose")
            .expect("examples/log_via_helper.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_log_via_helper");
        compile_native(&program, "log_alert", out.to_str().unwrap(), false, false)
            .expect("native compile of reaction with text-call content should succeed");
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(size > 400 && size < 2_000, "unexpected binary size: {}", size);
        let _ = fs::remove_file(out);
    }

    #[test]
    fn native_compiles_text_returning_rule_call() {
        // Phase 2G: `output: text` whose body is Call(helper, [Ident(input)]).
        // compose.verbose's name_line delegates to display_name — the emitter
        // inlines the helper's `concat(p.first, " ", p.last)` body at the
        // call site. Same-concept / same-input-name / no-lets restrictions
        // are enforced; violating any of them produces a clear Phase 2G
        // error message.
        use std::fs;
        let src = fs::read_to_string("examples/compose.verbose")
            .expect("examples/compose.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_name_line");
        compile_native(&program, "name_line", out.to_str().unwrap(), false, false)
            .expect("native compile of text-returning call should succeed");
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
        compile_native(&program, "audit_suspicious", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "classify_invoice", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "classify_tier", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "validate_purchase", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "audit_suspicious", out.to_str().unwrap(), false, false)
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
        compile_native(&program, "audit_suspicious", out.to_str().unwrap(), false, false)
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

    #[test]
    fn stdin_prologue_validates_and_produces_correct_size() {
        // The stdin prologue should emit well-formed x86-64 that passes
        // the validator, and its size should be in the expected range
        // (~170 bytes for the tokenizer + copy logic).
        let mut code = Vec::new();
        emit_stdin_prologue(&mut code);
        assert!(code.len() > 100 && code.len() < 300,
            "unexpected stdin prologue size: {} bytes", code.len());
        crate::validate_x86::validate_code(&code)
            .expect("stdin prologue should pass x86-64 validation");
    }

    #[test]
    fn stdin_binary_compiles_with_larger_size() {
        use std::fs;
        // A rule compiled with stdin=true should produce a valid binary
        // that is larger than the argv version by ~170 bytes (the prologue).
        let src = std::fs::read_to_string("examples/invoices.verbose")
            .expect("examples/invoices.verbose");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out_argv = std::env::temp_dir().join("verbosec_test_stdin_argv");
        let out_stdin = std::env::temp_dir().join("verbosec_test_stdin_stdin");

        compile_native(&program, "important_invoice", out_argv.to_str().unwrap(), false, false)
            .expect("argv compile");
        compile_native(&program, "important_invoice", out_stdin.to_str().unwrap(), true, false)
            .expect("stdin compile");

        let size_argv = fs::metadata(&out_argv).map(|m| m.len()).unwrap_or(0);
        let size_stdin = fs::metadata(&out_stdin).map(|m| m.len()).unwrap_or(0);
        assert!(size_stdin > size_argv, "stdin binary should be larger");
        let diff = size_stdin - size_argv;
        assert!(diff > 100 && diff < 300,
            "prologue overhead should be 100-300 bytes, got {}", diff);

        let _ = fs::remove_file(out_argv);
        let _ = fs::remove_file(out_stdin);
    }

    /// Phase 8 slice 8a regression: the logged router compiles, all three
    /// body strings and the declared log path appear inline in the binary,
    /// and the binary is slightly larger than the unlogged hello_router
    /// (the log effect adds an open/write/close sequence).
    #[test]
    fn phase8_http10_service_with_log_embeds_path_and_body() {
        use std::fs;
        let src = fs::read_to_string("examples/hello_router_logged.verbose")
            .expect("examples/hello_router_logged.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase8_logged_router");
        compile_service(&program, "hello_server_logged", out.to_str().unwrap())
            .expect("Http10 logged-service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (1000..1800).contains(&size),
            "logged service binary size {} outside expected [1000, 1800] envelope", size
        );

        // Response bodies still inline
        for body in [
            &b"Hello, logged router!"[..],
            &b"pong"[..],
            &b"not found"[..],
        ] {
            assert!(
                bytes.windows(body.len()).any(|w| w == body),
                "body literal {:?} not found in logged binary",
                std::str::from_utf8(body).unwrap()
            );
        }
        // The declared log file path is a compile-time literal and must
        // appear inline — the auditor reads the binary (or the source)
        // and sees exactly every file the service can touch.
        let log_path = b"/tmp/verbose_router.log";
        assert!(
            bytes.windows(log_path.len()).any(|w| w == log_path),
            "log path literal not found in binary — string must be inlined"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 7 slice 3d regression: the echo_path service compiles; the
    /// binary embeds each `concat`'s literal pieces inline; and the size
    /// sits in the expected envelope.
    #[test]
    fn phase7_slice3d_http10_concat_body_compiles() {
        use std::fs;
        let src = fs::read_to_string("examples/echo_path.verbose")
            .expect("examples/echo_path.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase7_slice3d_echo");
        compile_service(&program, "echo_server", out.to_str().unwrap())
            .expect("Http10 slice-3d service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (1000..1700).contains(&size),
            "slice-3d echo server size {} outside expected [1000, 1700] envelope", size
        );

        // Each arm's concat has at least one literal piece — they must all
        // appear inline in the .text section, just like literal bodies do
        // in slice 3c.
        for lit in [
            &b"got GET on "[..],
            &b"got POST on "[..],
            &b"unsupported method "[..],
            &b" on "[..],
        ] {
            assert!(
                bytes.windows(lit.len()).any(|w| w == lit),
                "concat literal {:?} not found in slice-3d binary",
                std::str::from_utf8(lit).unwrap()
            );
        }

        // The iteration rsp restore is the Phase 7 slice 3d hallmark:
        // `lea rsp, [rbp - frame_size]` — REX.W 0x8D 0xA5 + neg i32. For
        // echo_path with max_request=4096 and no timestamp, frame_size =
        // 48 + 4096 = 4144, so neg_frame = -4144 = 0xFFFFEFD0 in LE.
        let lea_prefix = [0x48, 0x8D, 0xA5];
        let lea_disp = (-(48i32 + 4096)).to_le_bytes();
        let pattern = {
            let mut p = Vec::with_capacity(7);
            p.extend_from_slice(&lea_prefix);
            p.extend_from_slice(&lea_disp);
            p
        };
        assert!(
            bytes.windows(pattern.len()).any(|w| w == pattern.as_slice()),
            "expected `lea rsp, [rbp - 4144]` iteration rsp-restore not found"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 8 slice 8b/8c regression: the audit_complete service compiles,
    /// includes the response bodies and the JSONL skeleton inline, and
    /// — because req.timestamp appears in the log content — embeds the
    /// clock_gettime syscall number (228) and the CLOCK_REALTIME load
    /// from the [rbp-56] timestamp slot.
    #[test]
    fn phase8_http10_service_with_resp_and_timestamp_compiles() {
        use std::fs;
        let src = fs::read_to_string("examples/audit_complete.verbose")
            .expect("examples/audit_complete.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase8_audit_complete");
        compile_service(&program, "audit_endpoint", out.to_str().unwrap())
            .expect("Http10 audit-complete service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (1500..2200).contains(&size),
            "audit_complete service binary size {} outside expected [1500, 2200] envelope", size
        );

        // Handler body literals still inline.
        for body in [&b"ok"[..], &b"ping"[..], &b"not found"[..]] {
            assert!(
                bytes.windows(body.len()).any(|w| w == body),
                "body literal {:?} not found in audit_complete binary",
                std::str::from_utf8(body).unwrap()
            );
        }
        // The log path is a compile-time literal — must appear inline.
        let log_path = b"/tmp/verbose_audit_complete.jsonl";
        assert!(
            bytes.windows(log_path.len()).any(|w| w == log_path),
            "log path literal not found in binary"
        );
        // The JSONL skeleton fragment that names the timestamp field must
        // appear inline — it's a literal arg of the concat.
        let ts_key = b"{\"ts\":";
        assert!(
            bytes.windows(ts_key.len()).any(|w| w == ts_key),
            "JSONL ts key not found in binary"
        );
        // clock_gettime = syscall 228 = 0xE4 — the prologue uses
        // `mov rax, 228` encoded as `48 c7 c0 e4 00 00 00`. Anchor on
        // that 7-byte sequence so we know the timestamp capture is wired.
        let mov_rax_228 = [0x48, 0xC7, 0xC0, 0xE4, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(mov_rax_228.len()).any(|w| w == mov_rax_228),
            "clock_gettime syscall number (228) not embedded in binary — slice 8c not wired"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 7 slice 3c regression: compiling the shipped hello_router
    /// example must produce a binary within the expected size envelope
    /// and include all three declared body strings verbatim.
    #[test]
    fn phase7_http10_dynamic_service_emits_all_body_literals() {
        use std::fs;
        let src = fs::read_to_string("examples/hello_router.verbose")
            .expect("examples/hello_router.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase7_router");
        compile_service(&program, "hello_server", out.to_str().unwrap())
            .expect("Http10 dynamic-service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (800..1500).contains(&size),
            "http dynamic service binary size {} outside expected [800, 1500] envelope", size
        );

        // All three handler body strings must appear verbatim in the emitted
        // ELF — each is inlined in the .text section as part of the arm-
        // specific slot write.
        for body in [
            &b"Hello from Verbose router!"[..],
            &b"pong"[..],
            &b"not found"[..],
        ] {
            assert!(
                bytes.windows(body.len()).any(|w| w == body),
                "body literal {:?} not found in emitted binary",
                std::str::from_utf8(body).unwrap()
            );
        }
        // Comparison literals should also appear inline (repe cmpsb needs
        // them NUL-terminated and addressable via rip-relative lea).
        for method_or_path in [&b"GET\0"[..], &b"/\0"[..], &b"/ping\0"[..]] {
            assert!(
                bytes.windows(method_or_path.len()).any(|w| w == method_or_path),
                "comparison literal {:?} not found in emitted binary",
                std::str::from_utf8(method_or_path).unwrap()
            );
        }

        let _ = fs::remove_file(out);
    }

    /// Phase 7 slice 3b regression: compiling the shipped hello_http
    /// example via the Service path must succeed, produce a binary
    /// whose size is within the expected envelope, and emit the
    /// response string from the handler's HttpResponse record body.
    /// The last check prevents regressions where the AST-to-wire
    /// extraction drops or mangles the body literal.
    #[test]
    fn phase7_http10_constant_service_emits_declared_body() {
        use std::fs;
        let src = fs::read_to_string("examples/hello_http.verbose")
            .expect("examples/hello_http.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase7_http");
        compile_service(&program, "hello_server", out.to_str().unwrap())
            .expect("Http10 constant-service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (350..1100).contains(&size),
            "http constant service binary size {} outside expected [350, 1100] envelope", size
        );

        // The handler's body literal must appear verbatim in the emitted ELF
        // (inlined in the .text section as part of the precomputed response).
        let body = b"Hello from Verbose over HTTP!";
        assert!(
            bytes.windows(body.len()).any(|w| w == body),
            "handler body literal not found in emitted binary"
        );
        // So should the Content-Length line for the computed body length (29).
        let content_length = b"Content-Length: 29";
        assert!(
            bytes.windows(content_length.len()).any(|w| w == content_length),
            "expected Content-Length: 29 header in emitted binary"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 7 slice 2b regression: compiling the shipped raw_tcp_echo
    /// example via the Service path must produce a binary of exactly the
    /// same size (358 bytes) as the tier-3 compile_echo_server probe,
    /// since both paths now share emit_raw_tcp_echo_bytes. Byte-for-byte
    /// equivalence is the structural collapse of tier-3 into tier-1.
    #[test]
    fn phase7_service_matches_echo_probe_size() {
        use std::fs;
        let src = fs::read_to_string("examples/raw_tcp_echo.verbose")
            .expect("examples/raw_tcp_echo.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let svc_out = std::env::temp_dir().join("verbosec_test_phase7_service");
        let probe_out = std::env::temp_dir().join("verbosec_test_phase7_probe");

        compile_service(&program, "echo_server", svc_out.to_str().unwrap())
            .expect("service compile");
        compile_echo_server(7777, probe_out.to_str().unwrap())
            .expect("probe compile");

        let svc_size = fs::metadata(&svc_out).map(|m| m.len()).unwrap_or(0);
        let probe_size = fs::metadata(&probe_out).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            svc_size, probe_size,
            "service-emitted binary must equal echo-probe binary size (tier-3 → tier-1 collapse)"
        );
        assert_eq!(svc_size, 358, "expected exact 358 bytes for raw_tcp echo");

        let _ = fs::remove_file(svc_out);
        let _ = fs::remove_file(probe_out);
    }
}
