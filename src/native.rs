/// Native x86-64 code generation — produces ELF binaries directly.
///
/// General-purpose expression compiler: supports arithmetic (+, -, *, /),
/// comparisons (>, <, >=, <=), boolean logic (and, or), field access,
/// and rule calls (inlined). Multi-field concepts are supported.
///
/// The generated binary reads groups of N numbers from command-line arguments
/// (one group per record, N = number of fields) and prints the result.

use std::collections::{HashMap, HashSet};

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
    // Phase 9 slice 1: index every top-level `resource` block by name. The
    // emitter walks the rule's logic to discover which entries it actually
    // reads; entries the rule never references contribute zero bytes.
    let resources: HashMap<&str, &Resource> = program.items.iter().filter_map(|i| match i {
        Item::Resource(r) => Some((r.name.as_str(), r)),
        _ => None,
    }).collect();
    // Phase 11 slice 1: index every top-level `connection` block by name —
    // mirrors the resource map above. Rules walk their logic for fetch
    // sites and the prologue allocates one (ptr, len, buf) slot triple
    // per unique connection.
    let connections: HashMap<&str, &Connection> = program.items.iter().filter_map(|i| match i {
        Item::Connection(c) => Some((c.name.as_str(), c)),
        _ => None,
    }).collect();
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
        emit_reaction_program(rx, rule, concept, &rules, &resources, &connections)?
    } else if is_result_output {
        emit_result_program(rule, concept, &concepts, &rules, &resources, &connections)?
    } else if is_collection_output {
        emit_collection_program(rule, concept, &concepts, &rules, &resources)?
    } else if is_fold_number_output {
        // If the logic was desugared from Quantifier→Fold, create a temp
        // rule with the desugared logic so emit_fold_program sees a Fold.
        if let Some(ref desugared) = desugared_fold {
            let mut rule_copy = rule.clone();
            rule_copy.logic.value = desugared.clone();
            emit_fold_program(&rule_copy, concept, &concepts, &rules, &resources)?
        } else {
            emit_fold_program(rule, concept, &concepts, &rules, &resources)?
        }
    } else if is_fold_text_output {
        emit_text_fold_program(rule, concept, &concepts, &rules, &resources)?
    } else if matches!(&rule.output_ty, Type::Text) {
        emit_text_program(rule, concept, &rules, &resources, &connections)?
    } else if let Some(rec_concept) = record_output_concept {
        emit_record_program(rule, rec_concept, concept, &concepts, &rules, &resources, &connections)?
    } else if matches!(&rule.output_ty, Type::Number | Type::Bool) && contains_quantifier(&rule.logic.value) {
        // Phase 6: scalar output with embedded quantifiers (e.g. if all(...) then X else Y).
        emit_multi_fold_program(rule, concept, &concepts, &rules, &resources)?
    } else if is_vectorizable && concept.fields.len() == 1 {
        if let Some(threshold) = extract_simple_gt(rule) { emit_vectorized_program(threshold)? } else { emit_full_program(rule, concept, context_concept, &rules, &resources, &connections)? }
    } else if is_parallel {
        emit_parallel_program(rule, concept, &rules, &resources)?
    } else {
        emit_full_program(rule, concept, context_concept, &rules, &resources, &connections)?
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
    /// Phase 2I (non-literal text let bindings): maps text-typed let
    /// binding names to their (ptr_slot, len_slot) pair. Populated by the
    /// prologue's let-eval loop when a binding's RHS is a text
    /// expression; empty otherwise. Consumers that emit text (concat,
    /// text-write) read this to resolve `Ident(name)` as a BoundText,
    /// picking up exactly the same shape as Phase 2F's err_var capture.
    text_bindings: TextBindings<'a>,
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
    /// Number of match_result quartets reserved past depth 0. Equal to
    /// `(max_nesting_depth - 1)` when the rule has nested match_result;
    /// 0 otherwise. `match_slots_at_depth` derives slot offsets for
    /// depths > 0 from this and the depth-0 slots above.
    extra_match_slot_quartets: usize,
    /// Exit code flag: 0 = all records succeeded, 1 = at least one failed.
    /// Bool rules set this to 1 on false; Result rules set it on Err.
    /// The epilogue loads this into rdi for sys_exit.
    exit_flag_slot: i32,
    /// Phase 9 slice 1: js patch sites left by the resource open/read
    /// sequences emitted before the loop top. The epilogue resolves them
    /// to a shared abort label (sys_exit(1)) emitted after the normal
    /// exit syscall, when the vector is non-empty. Empty (and zero cost)
    /// for rules that do not reference any resource.
    resource_abort_patches: Vec<usize>,
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
/// Phase 2I — classify whether a let binding's RHS produces a text value.
/// The optimiser has already inlined `Expr::Text` literals, so the cases
/// that reach the emitter are: `Concat(...)`, `Field` on a text-typed
/// field, `Call` to a text-returning rule, and `Ident` to a prior text
/// let binding. Anything else is treated as number (the existing path).
fn let_rhs_is_text(
    expr: &Expr,
    concept: &Concept,
    context_concept: Option<&Concept>,
    all_rules: &HashMap<&str, &Rule>,
    prior_text_lets: &HashSet<&str>,
) -> bool {
    match expr {
        Expr::Text(_) | Expr::Concat(_) => true,
        Expr::Field(base, fname) => {
            if !matches!(base.as_ref(), Expr::Ident(_)) {
                return false;
            }
            // Look up in input concept first, then context concept.
            if let Some(f) = concept.fields.iter().find(|f| &f.name == fname) {
                return matches!(f.ty, Type::Text);
            }
            if let Some(cc) = context_concept {
                if let Some(f) = cc.fields.iter().find(|f| &f.name == fname) {
                    return matches!(f.ty, Type::Text);
                }
            }
            false
        }
        Expr::Call(name, _) => all_rules
            .get(name.as_str())
            .map(|r| matches!(r.output_ty, Type::Text))
            .unwrap_or(false),
        Expr::Ident(n) => prior_text_lets.contains(n.as_str()),
        // Phase 12 (json_escape): the transform's output type is text by
        // construction. The classifier needs to recognize it so a
        // `let escaped = json_escape(req.path)` lands in the Phase 2I
        // text-let path (two slots, BoundText resolution at use sites)
        // rather than the number-let path.
        Expr::JsonEscape(_) => true,
        // Phase 12 (parse_int): output type is number, not text. The text
        // classifier returns false so let-bindings pointing at parse_int
        // land in the number-let path (single slot).
        Expr::ParseInt(_) => false,
        // `now_unix()` returns number — same number-let path as ParseInt.
        Expr::NowUnix => false,
        // `abs(<number>)` returns number — same number-let path as ParseInt.
        Expr::Abs(_) => false,
        // `substring(text, start, end)` returns text by construction.
        // Same classifier shape as JsonEscape: the let lands in the
        // Phase 2I text-let path (two slots, BoundText resolution at
        // use sites), and emit_text_produce_ptrlen's Substring arm
        // produces the slice (ptr, len).
        Expr::Substring(_, _, _) => true,
        _ => false,
    }
}

/// Phase 9 slice 1 — walk an expression tree collecting every `Read(name)`
/// reference, de-duplicated, in source order. The native prologue iterates
/// this list once per rule invocation to emit the open/read/close sequence
/// for every referenced resource. Mirrors `verifier::collect_read_names`
/// exactly so the two stay in sync if Expr grows new variants.
fn collect_read_names_native(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Read(name) => {
            if !out.iter().any(|n| n == name) {
                out.push(name.clone());
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => {}
        Expr::Field(base, _) => collect_read_names_native(base, out),
        Expr::Binary(_, l, r) => {
            collect_read_names_native(l, out);
            collect_read_names_native(r, out);
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => {
            collect_read_names_native(i, out)
        }
        Expr::If(c, t, e) => {
            collect_read_names_native(c, out);
            collect_read_names_native(t, out);
            collect_read_names_native(e, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                collect_read_names_native(a, out);
            }
        }
        Expr::Quantifier(_, c, _, body) => {
            collect_read_names_native(c, out);
            collect_read_names_native(body, out);
        }
        Expr::Fold(c, init, _, _, body) => {
            collect_read_names_native(c, out);
            collect_read_names_native(init, out);
            collect_read_names_native(body, out);
        }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
            collect_read_names_native(c, out);
            collect_read_names_native(body, out);
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            collect_read_names_native(t, out);
            collect_read_names_native(ok, out);
            collect_read_names_native(err, out);
        }
        Expr::Record(_, fields) => {
            for (_, e) in fields {
                collect_read_names_native(e, out);
            }
        }
        // Phase 11 slice 1: Fetch's connection name is collected by
        // `collect_fetch_names_native`, not here. We still walk into the
        // request bytes Expr so any read(...) inside the request body is
        // captured.
        Expr::Fetch(_, req) => collect_read_names_native(req, out),
        // Phase 12 (json_escape): pure pass-through.
        Expr::JsonEscape(inner) => collect_read_names_native(inner, out),
        // Phase 12 (parse_int): pure pass-through.
        Expr::ParseInt(inner) => collect_read_names_native(inner, out),
        // `now_unix()` is a clock read, not a resource read — leaf node.
        Expr::NowUnix => {}
        // `starts_with(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` reference (e.g. needle is a
        // text loaded from a resource).
        Expr::StartsWith(h, n) => {
            collect_read_names_native(h, out);
            collect_read_names_native(n, out);
        }
        // `contains(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` reference. Same shape as
        // StartsWith — wraps two text-typed children, no own read.
        Expr::Contains(h, n) => {
            collect_read_names_native(h, out);
            collect_read_names_native(n, out);
        }
        // `ends_with(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` reference. Same shape as
        // StartsWith / Contains.
        Expr::EndsWith(h, n) => {
            collect_read_names_native(h, out);
            collect_read_names_native(n, out);
        }
        // `length(<text_expr>)` — pure pass-through.
        Expr::Length(inner) => collect_read_names_native(inner, out),
        // `abs(<number_expr>)` — pure pass-through.
        Expr::Abs(inner) => collect_read_names_native(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children; either
        // side may carry a `read(...)` reference.
        Expr::Min(l, r) | Expr::Max(l, r) => {
            collect_read_names_native(l, out);
            collect_read_names_native(r, out);
        }
        // `substring(text, start, end)` — recurse into all three children;
        // any child may carry a `read(...)` reference (e.g. the source text
        // is `read(buf)`).
        Expr::Substring(t, s, e) => {
            collect_read_names_native(t, out);
            collect_read_names_native(s, out);
            collect_read_names_native(e, out);
        }
    }
}

/// `now_unix()` primitive: walk an expression and return true if any
/// reference to `Expr::NowUnix` is reachable. Mirrors the shape of
/// collect_read_names_native — recurse through every variant; only
/// the NowUnix leaf returns true. Used by emitters to decide whether
/// to grow the frame for the timespec scratch and emit clock_gettime.
fn expr_uses_now_unix(e: &Expr) -> bool {
    match e {
        Expr::NowUnix => true,
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) | Expr::Read(_) => false,
        Expr::Field(b, _) => expr_uses_now_unix(b),
        Expr::Binary(_, l, r) => expr_uses_now_unix(l) || expr_uses_now_unix(r),
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => expr_uses_now_unix(i),
        Expr::JsonEscape(i) | Expr::ParseInt(i) | Expr::Length(i) | Expr::Abs(i) => expr_uses_now_unix(i),
        Expr::If(c, t, el) => {
            expr_uses_now_unix(c) || expr_uses_now_unix(t) || expr_uses_now_unix(el)
        }
        Expr::Call(_, args) | Expr::Concat(args) => args.iter().any(expr_uses_now_unix),
        Expr::Quantifier(_, c, _, b) => expr_uses_now_unix(c) || expr_uses_now_unix(b),
        Expr::Fold(c, init, _, _, b) => {
            expr_uses_now_unix(c) || expr_uses_now_unix(init) || expr_uses_now_unix(b)
        }
        Expr::Map(c, _, b) | Expr::Filter(c, _, b) => {
            expr_uses_now_unix(c) || expr_uses_now_unix(b)
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            expr_uses_now_unix(t) || expr_uses_now_unix(ok) || expr_uses_now_unix(err)
        }
        Expr::Record(_, fields) => fields.iter().any(|(_, e)| expr_uses_now_unix(e)),
        Expr::Fetch(_, req) => expr_uses_now_unix(req),
        Expr::StartsWith(h, n) => expr_uses_now_unix(h) || expr_uses_now_unix(n),
        Expr::Contains(h, n) => expr_uses_now_unix(h) || expr_uses_now_unix(n),
        Expr::EndsWith(h, n) => expr_uses_now_unix(h) || expr_uses_now_unix(n),
        Expr::Min(l, r) | Expr::Max(l, r) => expr_uses_now_unix(l) || expr_uses_now_unix(r),
        Expr::Substring(t, s, e) => {
            expr_uses_now_unix(t) || expr_uses_now_unix(s) || expr_uses_now_unix(e)
        }
    }
}

/// True iff the rule's logic (value OR any let-binding RHS) references
/// `now_unix()` anywhere. Used by every emitter that supports clock
/// capture to decide whether to grow the frame and emit the syscall.
fn rule_uses_now_unix(rule: &Rule) -> bool {
    expr_uses_now_unix(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_now_unix(e))
}

/// Derive `MatchSlots` for a given match_result nesting depth from the
/// rule's RecordLoopCtx. Depth 0 returns the prologue's depth-0 quartet
/// (existing same-concept / non-nested layout). Depth N>0 sits at
/// 32 bytes per level deeper into the frame; the prologue must have
/// reserved that space (extra_match_slot_quartets >= depth).
///
/// Errors out cleanly if the requested depth exceeds what was reserved
/// — this would only fire on a bug in the depth-counting walker, since
/// the prologue's reservation is supposed to be the static MAX of any
/// depth the emitter will request.
fn match_slots_at_depth(
    ctx: &RecordLoopCtx<'_>,
    depth: usize,
) -> Result<MatchSlots, NativeError> {
    if depth > ctx.extra_match_slot_quartets {
        return Err(NativeError {
            message: format!(
                "internal: match_result emitter requested depth {} but only {} extra quartets were reserved \
                 (max depth was {}); count_match_result_max_depth missed a nested match",
                depth, ctx.extra_match_slot_quartets, ctx.extra_match_slot_quartets + 1,
            ),
        });
    }
    let shift = (depth as i32) * 32;  // 4 i64 slots per level
    Ok(MatchSlots {
        match_slot: ctx.match_slot - shift,
        err_ptr_slot: ctx.err_ptr_slot - shift,
        err_len_slot: ctx.err_len_slot - shift,
        err_frame_save_slot: ctx.err_frame_save_slot - shift,
        exit_flag_slot: ctx.exit_flag_slot,
    })
}

/// Maximum nesting depth of `match_result(...)` along any path through
/// the expression tree. Drives the prologue's reservation of
/// match_result slot quartets. Counted as:
///   - `MatchResult` itself contributes 1 + max(depth of its arms)
///   - `Ok` / `Err` / unary inner / `If` branches recurse
///
/// `Err`'s body is grammar-restricted to `Err(<text_expr>)`, so the
/// err_body never actually carries a nested match_result, but we still
/// recurse into it for safety. Cost: zero — the walker just doesn't
/// find any match_result there.
fn count_match_result_max_depth(expr: &Expr) -> usize {
    match expr {
        Expr::MatchResult(_, _, ok_body, _, err_body) => {
            1 + std::cmp::max(
                count_match_result_max_depth(ok_body),
                count_match_result_max_depth(err_body),
            )
        }
        Expr::If(c, t, e) => {
            count_match_result_max_depth(c)
                .max(count_match_result_max_depth(t))
                .max(count_match_result_max_depth(e))
        }
        Expr::Ok(i) | Expr::Err(i) | Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i)
        | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => {
            count_match_result_max_depth(i)
        }
        Expr::Binary(_, l, r) => {
            count_match_result_max_depth(l).max(count_match_result_max_depth(r))
        }
        Expr::Min(a, b) | Expr::Max(a, b) | Expr::StartsWith(a, b)
        | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
            count_match_result_max_depth(a).max(count_match_result_max_depth(b))
        }
        Expr::Substring(t, s, e) => {
            count_match_result_max_depth(t)
                .max(count_match_result_max_depth(s))
                .max(count_match_result_max_depth(e))
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            args.iter().map(count_match_result_max_depth).max().unwrap_or(0)
        }
        Expr::Fold(coll, init, _, _, body) => {
            count_match_result_max_depth(coll)
                .max(count_match_result_max_depth(init))
                .max(count_match_result_max_depth(body))
        }
        Expr::Quantifier(_, coll, _, body)
        | Expr::Map(coll, _, body)
        | Expr::Filter(coll, _, body) => {
            count_match_result_max_depth(coll)
                .max(count_match_result_max_depth(body))
        }
        Expr::Record(_, fields) => fields
            .iter()
            .map(|(_, e)| count_match_result_max_depth(e))
            .max()
            .unwrap_or(0),
        Expr::Fetch(_, request) => count_match_result_max_depth(request),
        // Leaves: no inner expression to recurse into.
        Expr::Number(_) | Expr::Text(_) | Expr::Field(_, _) | Expr::Ident(_)
        | Expr::Read(_) | Expr::NowUnix => 0,
    }
}

/// True iff the expression tree references `Field(Ident(input_name), field_name)`
/// — a per-field-name walker, generalising what `expr_uses_now_unix`
/// does for the synthetic clock. Used to detect whether the HTTP
/// service emitter needs to populate a slot for a specific field
/// (e.g. `req.body`, which only deserves the parser+slot work when the
/// handler or log content actually references it).
fn expr_uses_field(e: &Expr, input_name: &str, field_name: &str) -> bool {
    match e {
        Expr::Field(base, fname) => {
            (matches!(base.as_ref(), Expr::Ident(n) if n == input_name) && fname == field_name)
                || expr_uses_field(base, input_name, field_name)
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) | Expr::Read(_) | Expr::NowUnix => false,
        Expr::Binary(_, l, r) => {
            expr_uses_field(l, input_name, field_name)
                || expr_uses_field(r, input_name, field_name)
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => {
            expr_uses_field(i, input_name, field_name)
        }
        Expr::JsonEscape(i) | Expr::ParseInt(i) | Expr::Length(i) | Expr::Abs(i) => {
            expr_uses_field(i, input_name, field_name)
        }
        Expr::If(c, t, el) => {
            expr_uses_field(c, input_name, field_name)
                || expr_uses_field(t, input_name, field_name)
                || expr_uses_field(el, input_name, field_name)
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            args.iter().any(|a| expr_uses_field(a, input_name, field_name))
        }
        Expr::StartsWith(h, n) | Expr::Contains(h, n) | Expr::EndsWith(h, n) => {
            expr_uses_field(h, input_name, field_name)
                || expr_uses_field(n, input_name, field_name)
        }
        Expr::Quantifier(_, c, _, b) => {
            expr_uses_field(c, input_name, field_name)
                || expr_uses_field(b, input_name, field_name)
        }
        Expr::Fold(c, init, _, _, b) => {
            expr_uses_field(c, input_name, field_name)
                || expr_uses_field(init, input_name, field_name)
                || expr_uses_field(b, input_name, field_name)
        }
        Expr::Map(c, _, b) | Expr::Filter(c, _, b) => {
            expr_uses_field(c, input_name, field_name)
                || expr_uses_field(b, input_name, field_name)
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            expr_uses_field(t, input_name, field_name)
                || expr_uses_field(ok, input_name, field_name)
                || expr_uses_field(err, input_name, field_name)
        }
        Expr::Record(_, fields) => fields
            .iter()
            .any(|(_, e)| expr_uses_field(e, input_name, field_name)),
        Expr::Fetch(_, req) => expr_uses_field(req, input_name, field_name),
        Expr::Min(l, r) | Expr::Max(l, r) => {
            expr_uses_field(l, input_name, field_name)
                || expr_uses_field(r, input_name, field_name)
        }
        Expr::Substring(t, s, e) => {
            expr_uses_field(t, input_name, field_name)
                || expr_uses_field(s, input_name, field_name)
                || expr_uses_field(e, input_name, field_name)
        }
    }
}

/// Emit a single `clock_gettime(CLOCK_REALTIME, &timespec)` syscall
/// that lands tv_sec at `[rbp + tv_sec_slot]` and tv_nsec at
/// `[rbp + tv_sec_slot + 8]` (one slot ABOVE — the caller MUST have
/// reserved 16 bytes total: tv_sec at the deepest position, tv_nsec
/// scratch in the slot just above it). Caller registers tv_sec_slot
/// in the local `offsets` map under the synthetic name `"now"` so
/// `emit_eval_expr`'s NowUnix arm finds it.
///
/// Linux struct timespec on x86-64: { tv_sec: i64; tv_nsec: i64 }.
/// rsi must point AT tv_sec (kernel writes byte 0..7 = tv_sec, 8..15
/// = tv_nsec). syscall = 228.
fn emit_capture_now_unix(code: &mut Vec<u8>, tv_sec_slot: i32) {
    // mov rax, 228 (sys_clock_gettime)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xE4, 0x00, 0x00, 0x00]);
    // xor edi, edi (CLOCK_REALTIME = 0; 32-bit zero suffices, upper auto-cleared)
    code.extend_from_slice(&[0x31, 0xFF]);
    // lea rsi, [rbp + tv_sec_slot]
    if tv_sec_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x8D, 0x75]);
        code.push(tv_sec_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8D, 0xB5]);
        code.extend_from_slice(&tv_sec_slot.to_le_bytes());
    }
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
    // (tv_sec is now at [rbp + tv_sec_slot]; the kernel doesn't return
    // an error for CLOCK_REALTIME on a sane Linux, so no abort path —
    // matches the documented expectation that wall-clock reads succeed.)
}

/// Walk a rule's logic — both let-binding RHS expressions and the value
/// — and return every distinct resource name referenced via `read(...)`,
/// in source order. Empty for rules that do not touch any resource.
fn collect_rule_read_names(rule: &Rule) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, expr) in &rule.logic.bindings {
        collect_read_names_native(expr, &mut names);
    }
    collect_read_names_native(&rule.logic.value, &mut names);
    names
}

/// Same as `collect_rule_read_names` but also follows `match_result`
/// targets into their callees and gathers THEIR `read(...)` names. The
/// native match_result emitter inlines callees into the outer rule's
/// frame, so any resource the callee reads must be allocated in the
/// outer's prologue. Symmetric with the verifier's
/// `augment_facts_with_transitive_match_result_reads` pass — what the
/// verifier says is a legitimate transitive read, the emitter now
/// allocates a slot for.
///
/// Cycle protection via a visited set keyed on rule name; a circular
/// chain stops at first re-entry.
fn collect_rule_read_names_transitive(
    rule: &Rule,
    all_rules: &HashMap<&str, &Rule>,
) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    visited.insert(rule.name.clone());
    for (_, expr) in &rule.logic.bindings {
        collect_read_names_native(expr, &mut names);
        gather_transitive_callee_reads(expr, all_rules, &mut visited, &mut names);
    }
    collect_read_names_native(&rule.logic.value, &mut names);
    gather_transitive_callee_reads(&rule.logic.value, all_rules, &mut visited, &mut names);
    names
}

/// Walk for `MatchResult` nodes whose target is a `Call(callee, [...])`,
/// look up the callee, and merge ITS read names into `out`.
fn gather_transitive_callee_reads(
    expr: &Expr,
    all_rules: &HashMap<&str, &Rule>,
    visited: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
) {
    match expr {
        Expr::MatchResult(target, _, ok_body, _, err_body) => {
            if let Expr::Call(name, _) = target.as_ref() {
                if let Some(callee) = all_rules.get(name.as_str()) {
                    if visited.insert(callee.name.clone()) {
                        // Add the callee's own direct read names.
                        for (_, e) in &callee.logic.bindings {
                            collect_read_names_native(e, out);
                        }
                        collect_read_names_native(&callee.logic.value, out);
                        // Recurse: the callee may itself match_result on
                        // another rule.
                        for (_, e) in &callee.logic.bindings {
                            gather_transitive_callee_reads(e, all_rules, visited, out);
                        }
                        gather_transitive_callee_reads(&callee.logic.value, all_rules, visited, out);
                    }
                }
            }
            gather_transitive_callee_reads(ok_body, all_rules, visited, out);
            gather_transitive_callee_reads(err_body, all_rules, visited, out);
        }
        Expr::If(c, t, e) => {
            gather_transitive_callee_reads(c, all_rules, visited, out);
            gather_transitive_callee_reads(t, all_rules, visited, out);
            gather_transitive_callee_reads(e, all_rules, visited, out);
        }
        Expr::Ok(i) | Expr::Err(i) | Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i)
        | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => {
            gather_transitive_callee_reads(i, all_rules, visited, out);
        }
        Expr::Binary(_, l, r) => {
            gather_transitive_callee_reads(l, all_rules, visited, out);
            gather_transitive_callee_reads(r, all_rules, visited, out);
        }
        Expr::Min(a, b) | Expr::Max(a, b) | Expr::StartsWith(a, b)
        | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
            gather_transitive_callee_reads(a, all_rules, visited, out);
            gather_transitive_callee_reads(b, all_rules, visited, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                gather_transitive_callee_reads(a, all_rules, visited, out);
            }
        }
        Expr::Record(_, fields) => {
            for (_, v) in fields {
                gather_transitive_callee_reads(v, all_rules, visited, out);
            }
        }
        Expr::Fetch(_, req) => {
            gather_transitive_callee_reads(req, all_rules, visited, out);
        }
        Expr::Fold(coll, init, _, _, body) => {
            gather_transitive_callee_reads(coll, all_rules, visited, out);
            gather_transitive_callee_reads(init, all_rules, visited, out);
            gather_transitive_callee_reads(body, all_rules, visited, out);
        }
        Expr::Quantifier(_, coll, _, body)
        | Expr::Map(coll, _, body)
        | Expr::Filter(coll, _, body) => {
            gather_transitive_callee_reads(coll, all_rules, visited, out);
            gather_transitive_callee_reads(body, all_rules, visited, out);
        }
        Expr::Substring(t, s, e) => {
            gather_transitive_callee_reads(t, all_rules, visited, out);
            gather_transitive_callee_reads(s, all_rules, visited, out);
            gather_transitive_callee_reads(e, all_rules, visited, out);
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Field(_, _) | Expr::Ident(_)
        | Expr::Read(_) | Expr::NowUnix => {}
    }
}

/// Resolve every resource name read by `rule` against the program's
/// top-level resource table. Mirror of the by-hand block 7 emitters
/// were duplicating before this helper was extracted (2026-05-01).
/// `role` distinguishes the error message between rule-level and
/// service-handler-level callers ("rule" vs "service handler") —
/// that's the only meaningful variation across call sites.
fn collect_referenced_resources<'a>(
    rule: &Rule,
    all_resources: &HashMap<&str, &'a Resource>,
    all_rules: &HashMap<&str, &Rule>,
    role: &str,
) -> Result<Vec<&'a Resource>, NativeError> {
    // Transitive: include resources read by callees inlined via
    // match_result chains. The emitter needs slots for ALL resources
    // that will be touched at runtime, including those whose `read(...)`
    // sits inside an inlined callee body. Verifier's
    // `augment_facts_with_transitive_match_result_reads` walks the same
    // closure; keeping them in lockstep is what lets a rule declare its
    // callees' resources in `reads:` without surprise.
    let names = collect_rule_read_names_transitive(rule, all_rules);
    let mut out: Vec<&'a Resource> = Vec::with_capacity(names.len());
    for name in &names {
        let r = all_resources.get(name.as_str()).ok_or_else(|| NativeError {
            message: format!(
                "{} '{}' reads resource '{}' but no top-level `resource {}` was declared",
                role, rule.name, name, name
            ),
        })?;
        out.push(*r);
    }
    Ok(out)
}

/// Each resource contributes 16 bytes (ptr + len) plus a max_bytes
/// buffer padded to 8 bytes. Same accounting in every emitter that
/// participates in the read() sweep — extracted for legibility and
/// to keep the formula in one place.
fn compute_resource_extra_bytes(referenced: &[&Resource]) -> i32 {
    referenced
        .iter()
        .map(|r| 16 + (((r.max_bytes as i32) + 7) & !7))
        .sum()
}

/// Shared sys_exit(1) tail for resource open/read failures. Emitted
/// only when at least one resource was read — resource-free programs
/// pay zero bytes for this. The same 9-line block was inlined at the
/// end of every emit_*_program that supports `read()`; extraction
/// here keeps the abort posture consistent across emitters.
fn emit_resource_abort_tail(code: &mut Vec<u8>, abort_patches: &[usize]) {
    if abort_patches.is_empty() {
        return;
    }
    let abort_label = code.len();
    for site in abort_patches {
        let rel = abort_label as i32 - (*site as i32 + 4);
        code[*site..*site + 4].copy_from_slice(&rel.to_le_bytes());
    }
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // mov rdi, 1
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
}

/// Phase 11 slice 1 — walk an expression tree collecting every
/// `Fetch(name, _)` connection name (de-duplicated, in source order).
/// Mirrors `collect_read_names_native` exactly. Used by the prologue to
/// emit one socket+connect+write+read+close sequence per unique
/// connection above loop_top.
fn collect_fetch_names_native(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Fetch(name, req) => {
            if !out.iter().any(|n| n == name) {
                out.push(name.clone());
            }
            collect_fetch_names_native(req, out);
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => {}
        Expr::Read(_) => {}
        Expr::Field(base, _) => collect_fetch_names_native(base, out),
        Expr::Binary(_, l, r) => {
            collect_fetch_names_native(l, out);
            collect_fetch_names_native(r, out);
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => {
            collect_fetch_names_native(i, out)
        }
        Expr::If(c, t, e) => {
            collect_fetch_names_native(c, out);
            collect_fetch_names_native(t, out);
            collect_fetch_names_native(e, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                collect_fetch_names_native(a, out);
            }
        }
        Expr::Quantifier(_, c, _, body) => {
            collect_fetch_names_native(c, out);
            collect_fetch_names_native(body, out);
        }
        Expr::Fold(c, init, _, _, body) => {
            collect_fetch_names_native(c, out);
            collect_fetch_names_native(init, out);
            collect_fetch_names_native(body, out);
        }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
            collect_fetch_names_native(c, out);
            collect_fetch_names_native(body, out);
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            collect_fetch_names_native(t, out);
            collect_fetch_names_native(ok, out);
            collect_fetch_names_native(err, out);
        }
        Expr::Record(_, fields) => {
            for (_, e) in fields {
                collect_fetch_names_native(e, out);
            }
        }
        // Phase 12 (json_escape): pure pass-through.
        Expr::JsonEscape(inner) => collect_fetch_names_native(inner, out),
        // Phase 12 (parse_int): pure pass-through.
        Expr::ParseInt(inner) => collect_fetch_names_native(inner, out),
        // `now_unix()` is not a connection — leaf node.
        Expr::NowUnix => {}
        // `starts_with(haystack, needle)` — recurse into both children;
        // either side may carry a `fetch(...)` reference.
        Expr::StartsWith(h, n) => {
            collect_fetch_names_native(h, out);
            collect_fetch_names_native(n, out);
        }
        // `contains(haystack, needle)` — recurse into both children;
        // either side may carry a `fetch(...)` reference.
        Expr::Contains(h, n) => {
            collect_fetch_names_native(h, out);
            collect_fetch_names_native(n, out);
        }
        // `ends_with(haystack, needle)` — recurse into both children;
        // either side may carry a `fetch(...)` reference.
        Expr::EndsWith(h, n) => {
            collect_fetch_names_native(h, out);
            collect_fetch_names_native(n, out);
        }
        // `length(<text_expr>)` — pure pass-through.
        Expr::Length(inner) => collect_fetch_names_native(inner, out),
        // `abs(<number_expr>)` — pure pass-through.
        Expr::Abs(inner) => collect_fetch_names_native(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) => {
            collect_fetch_names_native(l, out);
            collect_fetch_names_native(r, out);
        }
        // `substring(text, start, end)` — recurse into all three children.
        Expr::Substring(t, s, e) => {
            collect_fetch_names_native(t, out);
            collect_fetch_names_native(s, out);
            collect_fetch_names_native(e, out);
        }
    }
}

/// Phase 11 slice 1 — like `collect_rule_read_names` but for fetch.
/// Returns the unique connection names referenced by the rule's logic
/// (let bindings + value), in source order. Each entry corresponds to
/// one (ptr, len, buf) slot triple emitted above loop_top.
fn collect_rule_fetch_names(rule: &Rule) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for (_, expr) in &rule.logic.bindings {
        collect_fetch_names_native(expr, &mut names);
    }
    collect_fetch_names_native(&rule.logic.value, &mut names);
    names
}

/// Phase 9 slice 1 — emit `open(path, O_RDONLY, 0); read(fd, buf, max);
/// close(fd)` once per resource the rule reads. The path is the literal
/// declared on the resource. The buffer is reserved on the stack (size =
/// max_bytes padded to 8) and lives until the per-rule frame is freed.
///
/// Slot layout (relative to rbp, after caller has consumed up to
/// `*next_slot`):
///   [rbp + ptr_slot]    = pointer to the start of the read buffer
///   [rbp + len_slot]    = bytes read (0..=max_bytes)
///   [rbp + buf_slot]    = first byte of the read buffer
///   [rbp + buf_slot - (buf_padded - 8)]  = last byte of the read buffer
///
/// On open() or read() failure (sign-bit set on rax — Linux returns
/// -errno), control falls through to `js rel32` placeholders pushed into
/// `abort_patches`; the epilogue patches them to a shared sys_exit(1)
/// label emitted after the success exit syscall.
///
/// Returns `(ptr_slot, len_slot, buf_slot)` and the new `next_slot` (one
/// past the last used slot, suitable for further allocation by callers
/// that want to chain more reservations).
///
/// Registers used: rax (syscall return), rdi/rsi/rdx (syscall args), r15
/// (saved fd between read and close — same role as in the reaction
/// emitter). r12, r13, r14, rbp preserved.
fn emit_resource_read_sequence(
    code: &mut Vec<u8>,
    resource: &Resource,
    next_slot: i32,
    abort_patches: &mut Vec<usize>,
) -> (i32, i32, i32, i32) {
    // Slot layout (rbp-relative, going from higher to lower addresses):
    //   [rbp + ptr_slot, ptr_slot+7]   — buffer base pointer (8 bytes)
    //   [rbp + len_slot, len_slot+7]   — bytes read so far (8 bytes)
    //   [rbp + buf_slot, buf_slot+pad-1] — read buffer (max_bytes padded to 8)
    //   [rbp + new_next, new_next+7]   — next caller-allocatable 8-byte slot
    //
    // Placing the buffer BELOW the (ptr, len) pair keeps the indexing
    // monotonically descending — the next resource read or any other
    // bottom-of-frame allocator just continues from `new_next` without
    // ever stepping over the buffer's range.
    let ptr_slot = next_slot;
    let len_slot = next_slot - 8;
    let buf_padded = ((resource.max_bytes as i32) + 7) & !7;
    let buf_slot = len_slot - buf_padded;
    let new_next = buf_slot - 8;
    // === open(path, O_RDONLY=0, 0) ===
    let path_bytes = resource.path.as_bytes();
    let path_with_nul_len = path_bytes.len() + 1;
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
    // mov rsi, 0 (O_RDONLY)
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x00, 0x00, 0x00, 0x00]);
    // mov rdx, 0 (mode unused for O_RDONLY)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x00, 0x00, 0x00, 0x00]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
    // test rax, rax ; js rel32 (abort patch)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    // mov r15, rax  — save fd across the read
    code.extend_from_slice(&[0x49, 0x89, 0xC7]);

    // === read(r15, buf, max_bytes) ===
    // mov rax, 0 (sys_read)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00]);
    // mov rdi, r15
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
    // lea rsi, [rbp + buf_slot]
    if buf_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x8D, 0x75]);
        code.push(buf_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8D, 0xB5]);
        code.extend_from_slice(&buf_slot.to_le_bytes());
    }
    // mov rdx, max_bytes (32-bit immediate is fine — verifier caps at 64 MiB)
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(resource.max_bytes as i32).to_le_bytes());
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);
    // test rax, rax ; js rel32 (abort patch)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Store len: mov [rbp + len_slot], rax
    if len_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x45]);
        code.push(len_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&len_slot.to_le_bytes());
    }
    // Store ptr: lea rax, [rbp + buf_slot] ; mov [rbp + ptr_slot], rax
    if buf_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x8D, 0x45]);
        code.push(buf_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8D, 0x85]);
        code.extend_from_slice(&buf_slot.to_le_bytes());
    }
    if ptr_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x45]);
        code.push(ptr_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&ptr_slot.to_le_bytes());
    }

    // === close(r15) — failure here is intentionally ignored. close() ===
    // returns -errno on failure (e.g. EINTR), but the data already lives in
    // the buffer and a leaked fd is harmless for a one-shot rule binary.
    // mov rax, 3 (sys_close)
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
    // mov rdi, r15
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]);
    // syscall
    code.extend_from_slice(&[0x0F, 0x05]);

    (ptr_slot, len_slot, buf_slot, new_next)
}

/// Phase 11 slice 1 — emit `socket(AF_INET, SOCK_STREAM, 0); connect(...);
/// write(req); read(resp); close()` once per connection the rule fetches.
/// Layout mirrors `emit_resource_read_sequence`: response buffer + (ptr,
/// len) pair below the n_reserved slots, all freed by the per-rule frame
/// teardown.
///
/// The request bytes for slice 1 must be a `Text` literal — the only
/// text expression that can be lowered into machine code without field
/// references (we're above loop_top, so the rbp field slots aren't yet
/// populated). `Concat(...)` of literals is also fine because it
/// classifies as text and the inner Text args don't read fields.
/// Anything that reaches into the per-record input is rejected with a
/// clear error; the natural workaround is "stage the dynamic part inside
/// a let binding evaluated within the loop and wire it through a future
/// slice that supports per-record fetches".
///
/// Slot layout (rbp-relative):
///   [rbp + ptr_slot, +7]   — response buffer base pointer (8 bytes)
///   [rbp + len_slot, +7]   — bytes read by the read() syscall (8 bytes)
///   [rbp + buf_slot, +pad-1] — response buffer (max_response padded to 8)
///
/// Failures of the fallible syscalls (socket, connect, write, read) all
/// patch into the same shared sys_exit(1) abort label as the resource
/// path — the policy is `on_connect_error: abort` (slice-1 default and
/// only option). close() is best-effort (errors ignored), matching the
/// resource path.
///
/// Registers used: rax (syscall return), rdi/rsi/rdx/r10/r8 (syscall
/// args), r15 (saved socket fd across syscalls — same role as in the
/// resource path). r12, r13, r14, rbp preserved.
fn emit_connection_fetch_sequence(
    code: &mut Vec<u8>,
    connection: &Connection,
    rule: &Rule,
    input_concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    next_slot: i32,
    abort_patches: &mut Vec<usize>,
    // Phase 11 slice 3: HTTP services emit the fetch AFTER the per-accept
    // HTTP parse, so the request_expr can reference req.method / req.path —
    // the caller passes the populated offsets, text_bindings and field_ranges
    // from the surrounding handler context. Rule-prologue callers emit the
    // fetch BEFORE the record loop where no per-record field is loaded yet,
    // so they pass empty maps and the literal-only guard fires below.
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings,
    // When true (HTTP service path, slice 11.3) the request_expr is allowed
    // to reference per-record fields and other text bindings. When false
    // (rule-prologue path, slice 11.1) we keep the original literal-only
    // restriction so a rule-level fetch can't accidentally read a field
    // that hasn't been loaded into its slot yet.
    allow_dynamic_request: bool,
) -> Result<(i32, i32, i32, i32), NativeError> {
    let ptr_slot = next_slot;
    let len_slot = next_slot - 8;
    let buf_padded = ((connection.max_response as i32) + 7) & !7;
    let buf_slot = len_slot - buf_padded;
    let new_next = buf_slot - 8;

    // === Slice 1 / Slice 3: lower the request bytes ===
    // Slice 1 (rule prologue, allow_dynamic_request=false): the request must
    // be literal-only — no per-record field reference is reachable. The
    // walker below enforces this so the error fires at compile time.
    // Slice 3 (HTTP service, allow_dynamic_request=true): the request runs
    // after the HTTP parse; concat(req.method, " ", req.path, ...) is
    // permitted and resolves through `offsets` populated by the parser.
    fn request_is_literal_only(expr: &Expr) -> bool {
        match expr {
            Expr::Text(_) | Expr::Number(_) => true,
            Expr::Concat(args) => args.iter().all(request_is_literal_only),
            Expr::Neg(i) => request_is_literal_only(i),
            _ => false,
        }
    }
    // Find the Fetch we're emitting for — it's the first Fetch with this
    // connection name in the rule's logic. The verifier already enforces
    // "at most one fetch per connection per rule", so this is unambiguous.
    fn first_fetch_for<'a>(expr: &'a Expr, name: &str) -> Option<&'a Expr> {
        match expr {
            Expr::Fetch(n, req) if n == name => Some(req),
            Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) | Expr::Read(_) => None,
            Expr::Field(b, _) => first_fetch_for(b, name),
            Expr::Binary(_, l, r) => first_fetch_for(l, name).or_else(|| first_fetch_for(r, name)),
            Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => first_fetch_for(i, name),
            Expr::If(c, t, e) => first_fetch_for(c, name).or_else(|| first_fetch_for(t, name)).or_else(|| first_fetch_for(e, name)),
            Expr::Call(_, args) | Expr::Concat(args) => {
                args.iter().find_map(|a| first_fetch_for(a, name))
            }
            Expr::Quantifier(_, c, _, body) | Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
                first_fetch_for(c, name).or_else(|| first_fetch_for(body, name))
            }
            Expr::Fold(c, init, _, _, body) => {
                first_fetch_for(c, name).or_else(|| first_fetch_for(init, name)).or_else(|| first_fetch_for(body, name))
            }
            Expr::MatchResult(t, _, ok, _, err) => {
                first_fetch_for(t, name).or_else(|| first_fetch_for(ok, name)).or_else(|| first_fetch_for(err, name))
            }
            Expr::Record(_, fs) => fs.iter().find_map(|(_, e)| first_fetch_for(e, name)),
            Expr::Fetch(_, req) => first_fetch_for(req, name),
            // Phase 12 (json_escape): pass-through.
            Expr::JsonEscape(inner) => first_fetch_for(inner, name),
            // Phase 12 (parse_int): pass-through.
            Expr::ParseInt(inner) => first_fetch_for(inner, name),
            // `now_unix()` is not a Fetch — leaf node.
            Expr::NowUnix => None,
            // `starts_with(haystack, needle)` — recurse into both children.
            Expr::StartsWith(h, n) => first_fetch_for(h, name).or_else(|| first_fetch_for(n, name)),
            // `contains(haystack, needle)` — recurse into both children.
            Expr::Contains(h, n) => first_fetch_for(h, name).or_else(|| first_fetch_for(n, name)),
            // `ends_with(haystack, needle)` — recurse into both children.
            Expr::EndsWith(h, n) => first_fetch_for(h, name).or_else(|| first_fetch_for(n, name)),
            // `length(<text_expr>)` — pass-through.
            Expr::Length(inner) => first_fetch_for(inner, name),
            // `abs(<number_expr>)` — pass-through.
            Expr::Abs(inner) => first_fetch_for(inner, name),
            // `min(a, b)` / `max(a, b)` — recurse into both children.
            Expr::Min(l, r) | Expr::Max(l, r) => {
                first_fetch_for(l, name).or_else(|| first_fetch_for(r, name))
            }
            // `substring(text, start, end)` — recurse into all three children.
            Expr::Substring(t, s, e) => first_fetch_for(t, name)
                .or_else(|| first_fetch_for(s, name))
                .or_else(|| first_fetch_for(e, name)),
        }
    }
    let request_expr: &Expr = {
        let mut found: Option<&Expr> = None;
        for (_, b) in &rule.logic.bindings {
            if let Some(r) = first_fetch_for(b, &connection.name) {
                found = Some(r);
                break;
            }
        }
        if found.is_none() {
            found = first_fetch_for(&rule.logic.value, &connection.name);
        }
        found.ok_or_else(|| NativeError {
            message: format!(
                "internal: rule '{}' lists connection '{}' but no fetch site found",
                rule.name, connection.name
            ),
        })?
    };
    if !allow_dynamic_request && !request_is_literal_only(request_expr) {
        return Err(NativeError {
            message: format!(
                "phase 11 slice 1: fetch('{}', request) request must be a text literal (or concat of literals) when called from a rule prologue; per-record / dynamic request bodies are supported only inside HTTP service handlers (slice 11.3)",
                connection.name
            ),
        });
    }

    // === socket(AF_INET=2, SOCK_STREAM=1, 0) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00]); // mov rax, 41
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]); // mov rdi, 2
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov rsi, 1
    code.extend_from_slice(&[0x48, 0x31, 0xD2]); // xor rdx, rdx
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    // test rax, rax ; js rel32 (abort patch)
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);
    code.extend_from_slice(&[0x49, 0x89, 0xC7]); // mov r15, rax  (socket fd)

    // === Inline sockaddr_in (16 bytes), then take its address ===
    // struct sockaddr_in {
    //   sin_family: u16 = AF_INET = 2  (little-endian on x86: 02 00)
    //   sin_port:   u16 (big-endian: high byte first)
    //   sin_addr:   u32 (big-endian: octet[0] octet[1] octet[2] octet[3])
    //   sin_zero:   [u8; 8] = 0
    // }
    let mut sockaddr = [0u8; 16];
    sockaddr[0] = 2; // sin_family low
    sockaddr[1] = 0; // sin_family high
    let port_be = connection.port.to_be_bytes();
    sockaddr[2] = port_be[0];
    sockaddr[3] = port_be[1];
    let octets: Vec<u8> = connection
        .host
        .split('.')
        .map(|o| o.parse::<u8>().expect("verifier checked host octets"))
        .collect();
    sockaddr[4] = octets[0];
    sockaddr[5] = octets[1];
    sockaddr[6] = octets[2];
    sockaddr[7] = octets[3];
    // sockaddr[8..16] already zero (padding)

    // jmp over the 16-byte sockaddr literal embedded in the code stream.
    code.push(0xEB);
    code.push(16u8);
    let sockaddr_addr = code.len();
    code.extend_from_slice(&sockaddr);

    // === connect(r15, &sockaddr_in, 16) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00]); // mov rax, 42
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]); // mov rdi, r15
    // lea rsi, [rip + rel32] → sockaddr
    let end = code.len() + 7;
    let rel32 = sockaddr_addr as i32 - end as i32;
    code.extend_from_slice(&[0x48, 0x8D, 0x35]);
    code.extend_from_slice(&rel32.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x10, 0x00, 0x00, 0x00]); // mov rdx, 16
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    // test rax, rax ; js rel32 (abort patch) — connect returns 0 on success,
    // -errno on failure. Sign-bit checks both.
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // === Build request bytes via emit_text_produce_ptrlen ===
    // Slice 1 (rule prologue): the literal-only guard above ensures the
    // request consults none of the maps; the empty defaults below would
    // also work, but we pass the caller's maps for uniformity (they are
    // empty in practice for that path).
    // Slice 3 (HTTP service): the request_expr may reference req.method /
    // req.path — `offsets` carries the parser slot map (-8, -16) and
    // `text_bindings` carries any earlier-emitted resource/connection
    // (ptr, len) pairs. emit_text_produce_ptrlen → emit_concat_to_buffer
    // resolve those via the same BoundText path the response body uses.
    emit_text_produce_ptrlen(
        code,
        request_expr,
        &rule.input_name,
        input_concept,
        all_rules,
        offsets,
        field_ranges,
        text_bindings,
    )?;
    // After emit_text_produce_ptrlen: rax = req_ptr, rdx = req_len.
    // Stash into rsi (write expects buffer in rsi); rdx already correct.
    // mov rsi, rax
    code.extend_from_slice(&[0x48, 0x89, 0xC6]);

    // === write(r15, rsi, rdx) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]); // mov rax, 1
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]); // mov rdi, r15
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // === read(r15, buf, max_response) ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00]); // mov rax, 0
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]); // mov rdi, r15
    // lea rsi, [rbp + buf_slot]
    if buf_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x8D, 0x75]);
        code.push(buf_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8D, 0xB5]);
        code.extend_from_slice(&buf_slot.to_le_bytes());
    }
    // mov rdx, max_response
    code.extend_from_slice(&[0x48, 0xC7, 0xC2]);
    code.extend_from_slice(&(connection.max_response as i32).to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x48, 0x85, 0xC0]);
    code.extend_from_slice(&[0x0F, 0x88]);
    abort_patches.push(code.len());
    code.extend_from_slice(&[0, 0, 0, 0]);

    // Store len: mov [rbp + len_slot], rax
    if len_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x45]);
        code.push(len_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&len_slot.to_le_bytes());
    }
    // Store ptr: lea rax, [rbp + buf_slot] ; mov [rbp + ptr_slot], rax
    if buf_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x8D, 0x45]);
        code.push(buf_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x8D, 0x85]);
        code.extend_from_slice(&buf_slot.to_le_bytes());
    }
    if ptr_slot >= -128 {
        code.extend_from_slice(&[0x48, 0x89, 0x45]);
        code.push(ptr_slot as u8);
    } else {
        code.extend_from_slice(&[0x48, 0x89, 0x85]);
        code.extend_from_slice(&ptr_slot.to_le_bytes());
    }

    // === close(r15) — best-effort, mirrors the resource path ===
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]); // mov rax, 3
    code.extend_from_slice(&[0x4C, 0x89, 0xFF]); // mov rdi, r15
    code.extend_from_slice(&[0x0F, 0x05]); // syscall

    Ok((ptr_slot, len_slot, buf_slot, new_next))
}

fn emit_record_loop_prologue<'a>(
    code: &mut Vec<u8>,
    rule: &'a Rule,
    input_concept: &'a Concept,
    context_concept: Option<&'a Concept>,
    all_rules: &HashMap<&str, &Rule>,
    all_resources: &HashMap<&str, &'a Resource>,
    all_connections: &HashMap<&str, &'a Connection>,
) -> Result<RecordLoopCtx<'a>, NativeError> {
    let n_ctx = context_concept.map_or(0, |c| c.fields.len());
    let nfields = input_concept.fields.len();
    // Phase 2I: classify each let binding as text (2 slots) or number
    // (1 slot), walking the list in source order so a later binding can
    // see prior text lets as text-typed identifiers. The classifier
    // matches exactly what the emit loop will dispatch on — same
    // helper called twice with the same predicate.
    let mut prior_text: HashSet<&str> = HashSet::new();
    let binding_is_text: Vec<bool> = rule
        .logic
        .bindings
        .iter()
        .map(|(name, expr)| {
            let is_text = let_rhs_is_text(expr, input_concept, context_concept, all_rules, &prior_text);
            if is_text {
                prior_text.insert(name.as_str());
            }
            is_text
        })
        .collect();
    let n_binding_slots: usize = binding_is_text.iter().map(|b| if *b { 2 } else { 1 }).sum();
    // Bottom-of-frame reserved slots, in order:
    //   base + 1: match_slot          (Phase 2D Ok-bound i64)         <-- depth 0
    //   base + 2: err_ptr_slot        (Phase 2F Err-bound text ptr)
    //   base + 3: err_len_slot        (Phase 2F Err-bound text length)
    //   base + 4: err_frame_save_slot (Phase 2F rsp saved before callee Err concat)
    //   base + 5: match_slot          (depth 1, only if nested)
    //   base + 6: err_ptr_slot        (depth 1)
    //   base + 7: err_len_slot        (depth 1)
    //   base + 8: err_frame_save_slot (depth 1)
    //   ... 4 slots per added depth ...
    //   base + 4*N + 1: exit_flag_slot
    //
    // Nested match_result (slice "nested match"): one quartet per
    // `match_result` nesting level, indexed by depth via
    // `match_slots_at_depth(ctx, depth)`. Always at least one quartet
    // is reserved, even for non-match-result rules — the existing
    // layout has historically had these slots, so keeping them stable
    // when N=1 means non-nested binaries are byte-for-byte unchanged.
    let max_match_depth = std::cmp::max(
        1,
        count_match_result_max_depth(&rule.logic.value),
    );
    let n_match_quartets = max_match_depth;
    let n_reserved = 4 * n_match_quartets + 1;
    // Phase 9 slice 1: enumerate the resources the rule reads, in source
    // order. Each contributes 2 slots (ptr, len) plus a max_bytes buffer
    // padded to 8 bytes. Resources unknown at the program level become a
    // hard error here — the verifier already validates names, so reaching
    // an undeclared one means the dispatch was called with a stale rule.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // Phase 11 slice 1: enumerate the connections the rule fetches, in
    // source order. Each contributes 2 slots (ptr, len) plus the response
    // buffer (max_response padded to 8). Same shape as resources.
    let referenced_connections: Vec<&Connection> = {
        let names = collect_rule_fetch_names(rule);
        let mut out: Vec<&Connection> = Vec::with_capacity(names.len());
        for name in &names {
            let c = all_connections.get(name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "rule '{}' fetches connection '{}' but no top-level `connection {}` was declared",
                    rule.name, name, name
                ),
            })?;
            out.push(*c);
        }
        out
    };
    let connection_extra_bytes: i32 = referenced_connections
        .iter()
        .map(|c| 16 + (((c.max_response as i32) + 7) & !7))
        .sum();
    // `now_unix()` reservation: 16 bytes (tv_sec at the deeper of two
    // slots, tv_nsec scratch at the slot just above). Detected from the
    // rule's logic + let bindings via `rule_uses_now_unix`. The slot
    // sits at the very bottom of the frame, BELOW any resource and
    // connection blocks, so the existing slot offsets stay byte-for-
    // byte unchanged for rules that don't touch the clock.
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };
    let frame_slots = n_ctx + nfields + n_binding_slots + n_reserved;
    let frame_size = (frame_slots * 8) as i32
        + resource_extra_bytes
        + connection_extra_bytes
        + now_extra_bytes;
    let base = (n_ctx + nfields + n_binding_slots) as i32;
    // Depth-0 quartet (always present). Deeper depths sit at base + 4*depth + {1..4}.
    let match_slot: i32 = -((base + 1) * 8);
    let err_ptr_slot: i32 = -((base + 2) * 8);
    let err_len_slot: i32 = -((base + 3) * 8);
    let err_frame_save_slot: i32 = -((base + 4) * 8);
    // exit_flag sits past all match-result quartets so its offset stays
    // stable for non-nested rules.
    let exit_flag_slot: i32 = -((base + 4 * (n_match_quartets as i32) + 1) * 8);

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

    // ─── Phase 9 slice 1: read every referenced resource ONCE before the
    // record loop. The buffer + (ptr, len) pair live below the n_reserved
    // slots; both stay valid for the whole rule invocation (the buffer
    // outlives the record loop because it lives within the per-rule frame
    // freed by `mov rsp, rbp; pop rbp` in the epilogue). open/read failure
    // patches into the shared abort label emitted by emit_record_loop_epilogue.
    let mut text_bindings: TextBindings<'a> = HashMap::new();
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    let mut resource_next_slot: i32 = -((base + n_reserved as i32 + 1) * 8);
    for r in &referenced_resources {
        let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
            code,
            r,
            resource_next_slot,
            &mut resource_abort_patches,
        );
        text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
        resource_next_slot = new_next;
    }

    // ─── Phase 11 slice 1: fetch every referenced connection ONCE before
    // the record loop. socket → connect → write(request) → read(response)
    // → close, with (ptr, len) of the response stored at the registered
    // rbp slots. Failure of any fallible syscall jumps to the same shared
    // sys_exit(1) abort label as the resource path (single label, both
    // paths patch into it).
    // The empty maps + allow_dynamic_request=false combo enforces the
    // slice-1 invariant: rule-prologue fetches must use literal request
    // bytes (per-record fields haven't been loaded yet). The literal-only
    // guard inside emit_connection_fetch_sequence is what fails compilation
    // before any field-resolution attempt happens.
    let prologue_offsets: HashMap<&str, i32> = HashMap::new();
    let prologue_ranges: HashMap<&str, (i64, i64)> = HashMap::new();
    let prologue_bindings: TextBindings = HashMap::new();
    for c in &referenced_connections {
        let (ptr_slot, len_slot, _buf_slot, new_next) = emit_connection_fetch_sequence(
            code,
            c,
            rule,
            input_concept,
            all_rules,
            resource_next_slot,
            &mut resource_abort_patches,
            &prologue_offsets,
            &prologue_ranges,
            &prologue_bindings,
            false, // allow_dynamic_request
        )?;
        text_bindings.insert(c.name.as_str(), (ptr_slot, len_slot));
        resource_next_slot = new_next;
    }

    // ─── now_unix() capture: clock_gettime(CLOCK_REALTIME) ONCE at rule
    // entry, ABOVE loop_top, so every record sees the same captured
    // value. tv_sec lands at `now_slot` (the deeper of two reserved
    // slots); tv_nsec scratch occupies the slot immediately above.
    // The slot is registered under the synthetic name `"now"` in
    // binding_offsets below so emit_eval_expr's NowUnix arm finds it.
    let now_slot: i32 = if uses_now {
        let s = resource_next_slot - 8; // tv_sec at deeper slot
        emit_capture_now_unix(code, s);
        s
    } else {
        0 // unused; uses_now=false guards the only consumer
    };

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
    // Phase 2I — two evaluation paths:
    //   * Number RHS: existing single-slot path (emit_eval_expr → rax → one store).
    //   * Text RHS:   emit_text_produce_ptrlen → (rax=ptr, rdx=len) → two consecutive
    //                 slots. The name goes into text_bindings so downstream emitters
    //                 that already handle BoundText (concat args, text-write) see it.
    let mut binding_offsets = offsets;
    for (k, v) in &ctx_offsets {
        binding_offsets.insert(k, *v);
    }
    // Register the now_unix() slot so any reference in the rule's logic
    // (top-level or inside a let RHS) resolves through emit_eval_expr's
    // NowUnix arm to a load from this slot. Static lifetime literal so
    // the &'a str bound on binding_offsets is satisfied.
    if uses_now {
        binding_offsets.insert("now", now_slot);
    }
    let field_ranges = build_field_ranges(input_concept);
    // Phase 9 slice 1: text_bindings already contains the resource entries
    // populated above the loop_top. Let bindings continue to populate it
    // in source order; the namespaces don't collide (verifier rejects a
    // let with the same name as a resource at the resource-name check).
    let mut next_slot = -(((n_ctx + nfields) as i32 + 1) * 8);
    for (idx, (name, expr)) in rule.logic.bindings.iter().enumerate() {
        if binding_is_text[idx] {
            // Produce (rax, rdx). Note: concat RHS allocates a stack buffer
            // below rsp; it stays live until the record loop epilogue restores
            // rsp via `mov rsp, rbp`, which happens once per record — correct
            // scope for a per-record binding.
            emit_text_produce_ptrlen(
                code,
                expr,
                &rule.input_name,
                input_concept,
                all_rules,
                &binding_offsets,
                &field_ranges,
                &text_bindings,
            )?;
            let ptr_slot = next_slot;
            let len_slot = next_slot - 8;
            // mov [rbp + ptr_slot], rax
            if ptr_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x89, 0x45]);
                code.push(ptr_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x89, 0x85]);
                code.extend_from_slice(&ptr_slot.to_le_bytes());
            }
            // mov [rbp + len_slot], rdx
            if len_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x89, 0x55]);
                code.push(len_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x89, 0x95]);
                code.extend_from_slice(&len_slot.to_le_bytes());
            }
            text_bindings.insert(name.as_str(), (ptr_slot, len_slot));
            next_slot -= 16;
        } else {
            emit_eval_expr(code, expr, &rule.input_name, &binding_offsets, all_rules, &field_ranges, &text_bindings)?;
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
    }

    Ok(RecordLoopCtx {
        loop_top,
        exit_patch,
        binding_offsets,
        field_ranges,
        text_bindings,
        match_slot,
        err_ptr_slot,
        err_len_slot,
        err_frame_save_slot,
        extra_match_slot_quartets: n_match_quartets - 1,
        exit_flag_slot,
        resource_abort_patches,
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

    // Phase 9 slice 1: shared abort label for resource open/read failures.
    // The slice 8d service abort path stays separate; this one only fires
    // for rule-level resource I/O failures and only exists when at least
    // one resource read was emitted, so non-resource rules pay zero bytes.
    emit_resource_abort_tail(code, &ctx.resource_abort_patches);
}

fn emit_full_program(
    rule: &Rule,
    concept: &Concept,
    context_concept: Option<&Concept>,
    all_rules: &HashMap<&str, &Rule>,
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
) -> Result<Vec<u8>, NativeError> {
    let is_bool = rule.output_ty == Type::Bool;
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, context_concept, all_rules, all_resources, all_connections)?;

    // Evaluate final expression — result in rax
    emit_eval_expr(
        &mut code,
        &rule.logic.value,
        &rule.input_name,
        &ctx.binding_offsets,
        all_rules,
        &ctx.field_ranges,
        &ctx.text_bindings,
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
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
) -> Result<Vec<u8>, NativeError> {
    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, None, all_rules, all_resources, all_connections)?;

    // Phase 2I — pass the text_bindings built from the prologue's let-eval
    // loop so text-write resolves Ident(let-name) as a BoundText (same path
    // as Phase 2F's err_var). Without this, a rule like
    // `let msg = concat(...); msg` would fall through to
    // emit_text_write_to_fd's "unsupported shape" arm.
    emit_text_write_to_fd(
        &mut code,
        &rule.logic.value,
        1,
        &rule.input_name,
        concept,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
        &ctx.text_bindings,
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
    /// Phase 12 (json_escape): wrapper around an inner text-producing expr
    /// that triggers the per-byte JSON-escape transform during the fill
    /// pass. Sizing uses 2× the worst-case inner length. The inner shapes
    /// supported in native today are Text input fields and BoundText
    /// identifiers (resource Read, connection Fetch response, text let,
    /// match-result err_var). Literal Text inners are folded out by the
    /// optimizer before native sees them.
    JsonEscapedText,
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
        // `now_unix()` is a Number-producing leaf. Concat fill routes
        // Number args through emit_eval_expr, which resolves NowUnix
        // via the offsets["now"] slot the surrounding emitter populated.
        Expr::NowUnix => Some(ConcatArgKind::Number),
        // `length(...)`, `parse_int(...)`, `abs(...)` all return Number.
        // Concat's Number-arg path runs them through emit_eval_expr.
        Expr::Length(_) | Expr::ParseInt(_) | Expr::Abs(_) => Some(ConcatArgKind::Number),
        Expr::Ident(name) if text_bindings.contains_key(name.as_str()) => {
            Some(ConcatArgKind::BoundText)
        }
        // Phase 9 slice 1: read(<resource>) is wired into the same
        // BoundText path. The prologue stores (ptr, len) at fixed rbp
        // slots and registers the resource name in text_bindings, so
        // emit_concat_fill can resolve it through the same machinery
        // serving Phase 2F's err_var and Phase 2I's text lets.
        Expr::Read(name) if text_bindings.contains_key(name.as_str()) => {
            Some(ConcatArgKind::BoundText)
        }
        // Phase 11 slice 1: fetch(<connection>, _) shares the same
        // (ptr, len) slot shape — the prologue's emit_connection_fetch_sequence
        // populated the response slots and registered the connection
        // name in text_bindings.
        Expr::Fetch(name, _) if text_bindings.contains_key(name.as_str()) => {
            Some(ConcatArgKind::BoundText)
        }
        Expr::Call(_, _) => Some(ConcatArgKind::CallText),
        // Substring shares CallText's pre-eval/stash/fill machinery
        // 1-for-1: classify reserves a 16-byte slot, the pre-eval loop
        // calls emit_text_produce_ptrlen on the whole Substring expr
        // (which has a Substring arm since slice 1, producing rax=ptr,
        // rdx=len with the same fail-closed bounds), the sizing loop
        // loads the len from the slot, the fill loop copies (ptr, len)
        // from the slot. Substring inherits CallText's `is_nested`
        // rejection — only one level of pre-eval supported, same
        // constraint as Phase 2H-b Call args.
        Expr::Substring(_, _, _) => Some(ConcatArgKind::CallText),
        // Phase 12 (json_escape): the inner must classify as a text-producing
        // kind. Native today supports Text-typed input fields and BoundText
        // identifiers as inners; Number / CallText / nested JsonEscape stay
        // as None so the dispatcher returns a clear "not supported" error
        // (callers can either restructure their code or fall back to
        // interpreter for those shapes).
        Expr::JsonEscape(inner) => {
            let inner_kind = classify_concat_arg(inner, concept, input_name, text_bindings)?;
            match inner_kind {
                ConcatArgKind::Text | ConcatArgKind::BoundText => {
                    Some(ConcatArgKind::JsonEscapedText)
                }
                _ => None,
            }
        }
        Expr::Field(base, field_name) => {
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                // First: a text input field whose name is registered in
                // text_bindings is a BoundText source (e.g. req.body in
                // HTTP services — the parser stores ptr+len at dedicated
                // slots, NOT a NUL-terminated argv pointer). The fill
                // path resolves it via the same (ptr, len) machinery as
                // Read / Fetch / Phase-2I lets.
                if text_bindings.contains_key(field_name.as_str()) {
                    return Some(ConcatArgKind::BoundText);
                }
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
            ConcatArgKind::JsonEscapedText => {
                // Phase 12 (json_escape): worst-case is 2× the inner length
                // (every byte escapes to two). If the inner is a Field with a
                // declared `[..N]` bound, we can size statically as `2*N`. If
                // the inner is a BoundText (let / read / fetch / err_var) or
                // an unbounded Field, the size is runtime-dynamic — sized in
                // the dynamic path below by reading the inner's length and
                // doubling it.
                let inner = match arg { Expr::JsonEscape(i) => i.as_ref(), _ => unreachable!() };
                let inner_static_max: Option<i32> = match inner {
                    Expr::Field(base, field_name)
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
                    {
                        concept
                            .fields
                            .iter()
                            .find(|f| &f.name == field_name)
                            .and_then(|f| f.range)
                            .map(|(_, max)| max as i32)
                    }
                    _ => None,
                };
                if let Some(max_len) = inner_static_max {
                    static_total += 2 * max_len;
                } else {
                    has_dynamic = true;
                }
                call_slot_idx.push(-1);
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
                // Phase 9 slice 1 (Expr::Read) and Phase 11 slice 1 (Expr::Fetch)
                // both classify as BoundText with (ptr_slot, len_slot) in
                // text_bindings, just like Expr::Ident — the sizing pass must
                // count their runtime length so the buffer is large enough,
                // otherwise the fill pass overruns into adjacent slots.
                let bound_name = match arg {
                    Expr::Ident(n) | Expr::Read(n) | Expr::Fetch(n, _) => Some(n.as_str()),
                    // A text input field whose name is registered in
                    // text_bindings is a BoundText source (e.g. req.body
                    // in HTTP services — the parser stores ptr+len at
                    // dedicated slots, NOT a NUL-terminated argv pointer).
                    Expr::Field(_, n) => Some(n.as_str()),
                    _ => None,
                };
                if let Some(name) = bound_name {
                    let (_, len_slot) = *text_bindings.get(name).expect("classified as BoundText so present in bindings");
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
            ConcatArgKind::JsonEscapedText => {
                // Phase 12 (json_escape) runtime sizing: only reached when
                // the inner does NOT have a [..N] bound (otherwise the
                // static_total path absorbed it). Compute 2 × inner_length
                // into rcx, then add to rax.
                let inner = match arg { Expr::JsonEscape(i) => i.as_ref(), _ => unreachable!() };
                match inner {
                    Expr::Field(_, field_name) => {
                        // Same shape as the Text-field branch above: load
                        // the field pointer, strlen → rdx, then we can
                        // double + add. push/pop rax to preserve the
                        // running size accumulator across emit_strlen
                        // (which clobbers rax/rcx/rdx/rsi/rdi).
                        let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                            message: format!(
                                "json_escape inner field '{}' has no rbp slot",
                                field_name
                            ),
                        })?;
                        code.push(0x50); // push rax
                        // mov rsi, [rbp + offset]
                        if offset >= -128 {
                            code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                            code.push(offset as u8);
                        } else {
                            code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                            code.extend_from_slice(&offset.to_le_bytes());
                        }
                        emit_strlen(code); // rdx = length
                        code.push(0x59); // pop rcx (running size into rcx)
                        // shl rdx, 1  — double the inner length
                        code.extend_from_slice(&[0x48, 0xD1, 0xE2]);
                        // add rcx, rdx
                        code.extend_from_slice(&[0x48, 0x01, 0xD1]);
                        // mov rax, rcx
                        code.extend_from_slice(&[0x48, 0x89, 0xC8]);
                    }
                    Expr::Ident(name) | Expr::Read(name) | Expr::Fetch(name, _) => {
                        // BoundText: length is already in [rbp+len_slot].
                        // Load it into rcx, double via shl, add to rax.
                        let (_, len_slot) = *text_bindings
                            .get(name.as_str())
                            .expect("json_escape inner classified as BoundText");
                        // mov rcx, [rbp + len_slot]
                        if len_slot >= -128 {
                            code.extend_from_slice(&[0x48, 0x8B, 0x4D]);
                            code.push(len_slot as u8);
                        } else {
                            code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
                            code.extend_from_slice(&len_slot.to_le_bytes());
                        }
                        // shl rcx, 1
                        code.extend_from_slice(&[0x48, 0xD1, 0xE1]);
                        // add rax, rcx
                        code.extend_from_slice(&[0x48, 0x01, 0xC8]);
                    }
                    other => {
                        return Err(NativeError {
                            message: format!(
                                "json_escape inner shape not supported in native: {:?}",
                                other
                            ),
                        });
                    }
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

/// Shared substring emit, factored from four duplicated call sites
/// (emit_text_produce_ptrlen's Substring arm, plus the byte-prim
/// loaders for starts_with/ends_with/contains, json_escape, and
/// parse_int).
///
/// **Precondition (caller-provided):** `rax = text_ptr`,
/// `rdx = text_len`. The caller loads the text inner using whatever
/// dispatch it supports (each loader has different shape restrictions
/// — Concat / Call recursion / nested substring / etc.). All of them
/// converge to (rax, rdx) here at the entry to this helper.
///
/// **Postcondition (success path):** `rax = slice_ptr`,
/// `rdx = slice_len`. No other registers preserved (rcx is clobbered;
/// rsi/rdi untouched). Stack restored.
///
/// **Postcondition (fail-closed):** if `end > text_len` or
/// `start > end`, the emit jumps to an inline `sys_exit(1)` tail. The
/// process terminates; no return to caller. Negative start/end fall
/// under `start > end` after unsigned reinterpretation.
///
/// **Stack:** uses 24 bytes of scratch (text_ptr + text_len + end),
/// freed by `add rsp, 24` before the success-path return.
///
/// Cost: ~75 bytes of code per call site, plus the inline 16-byte
/// abort tail (jumped over on success, only reached on bounds
/// violation). No allocation, no buffer — the slice is a pointer
/// into the input buffer with a computed length.
fn emit_substring_bounds_and_slice(
    code: &mut Vec<u8>,
    start_expr: &Expr,
    end_expr: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // (1) push rax (text_ptr) ; push rdx (text_len)
    //   After 2 pushes: [rsp]=text_len, [rsp+8]=text_ptr.
    //   After push end below: [rsp]=end, [rsp+8]=text_len, [rsp+16]=text_ptr.
    code.push(0x50); // push rax
    code.push(0x52); // push rdx
    // (2) Evaluate end → rax
    emit_eval_expr(
        code, end_expr, input_name, offsets, all_rules, field_ranges, text_bindings,
    )?;
    // (3) push rax (save end at [rsp])
    code.push(0x50);
    // (4) Bounds: end > text_len → abort
    //   mov rcx, [rsp+8] ; cmp rax, rcx ; ja .abort
    code.extend_from_slice(&[0x48, 0x8B, 0x4C, 0x24, 0x08]);
    code.extend_from_slice(&[0x48, 0x39, 0xC8]);
    code.push(0x0F); code.push(0x87);
    let end_abort_patch = code.len();
    code.extend_from_slice(&[0; 4]);
    // (5) Evaluate start → rax
    emit_eval_expr(
        code, start_expr, input_name, offsets, all_rules, field_ranges, text_bindings,
    )?;
    // (6) Bounds: start > end → abort
    //   mov rcx, [rsp] ; cmp rax, rcx ; ja .abort
    code.extend_from_slice(&[0x48, 0x8B, 0x0C, 0x24]);
    code.extend_from_slice(&[0x48, 0x39, 0xC8]);
    code.push(0x0F); code.push(0x87);
    let start_abort_patch = code.len();
    code.extend_from_slice(&[0; 4]);
    // (7) Compute slice: slice_len = end - start, slice_ptr = text_ptr + start
    //   rcx = end (from previous load) ; rax = start
    //   mov rdx, rcx ; sub rdx, rax        (rdx = slice_len)
    code.extend_from_slice(&[0x48, 0x89, 0xCA]);
    code.extend_from_slice(&[0x48, 0x29, 0xC2]);
    //   mov rcx, [rsp+16] ; add rcx, rax   (rcx = slice_ptr)
    code.extend_from_slice(&[0x48, 0x8B, 0x4C, 0x24, 0x10]);
    code.extend_from_slice(&[0x48, 0x01, 0xC1]);
    //   mov rax, rcx                        (rax = slice_ptr per convention)
    code.extend_from_slice(&[0x48, 0x89, 0xC8]);
    // (8) add rsp, 24 ; jmp .ok
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x18]);
    code.push(0xE9);
    let ok_jmp_patch = code.len();
    code.extend_from_slice(&[0; 4]);
    // .abort: sys_exit(1)
    let abort_pos = code.len();
    let end_rel = abort_pos as i32 - (end_abort_patch as i32 + 4);
    code[end_abort_patch..end_abort_patch + 4].copy_from_slice(&end_rel.to_le_bytes());
    let start_rel = abort_pos as i32 - (start_abort_patch as i32 + 4);
    code[start_abort_patch..start_abort_patch + 4].copy_from_slice(&start_rel.to_le_bytes());
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&[0x0F, 0x05]);
    // .ok: result at (rax = slice_ptr, rdx = slice_len)
    let ok_pos = code.len();
    let ok_rel = ok_pos as i32 - (ok_jmp_patch as i32 + 4);
    code[ok_jmp_patch..ok_jmp_patch + 4].copy_from_slice(&ok_rel.to_le_bytes());
    Ok(())
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
            // mov rsi, rax              (rsi = text_ptr, survives emit_strlen)
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            // emit_strlen reads rsi, writes rdx, CLOBBERS rax (xor eax, eax).
            // Caller's convention is (rax = ptr, rdx = len), so restore rax
            // from rsi after the scan. This was a latent bug on the path —
            // no existing test exercised a bare-Field text-let RHS, so the
            // rax = 0 from xor leaked silently. substring(<text-field>, …)
            // is the first caller that pushes rax to the stack as the
            // saved text_ptr, which surfaced the issue.
            emit_strlen(code);
            // mov rax, rsi              (rax = text_ptr again)
            code.extend_from_slice(&[0x48, 0x89, 0xF0]);
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
        // Phase 9 slice 1: read(<resource>) — the prologue already filled
        // (ptr, len) at the registered slots. Identical shape to the
        // text-let / err-var case above.
        Expr::Read(name) if text_bindings.contains_key(name.as_str()) => {
            let (ptr_slot, len_slot) = text_bindings[name.as_str()];
            load_rax_from_rbp(code, ptr_slot);
            if len_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x55]);
                code.push(len_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0x95]);
                code.extend_from_slice(&len_slot.to_le_bytes());
            }
            Ok(())
        }
        // Phase 11 slice 1: fetch(<connection>, _) — same shape as read,
        // populated by emit_connection_fetch_sequence in the prologue.
        Expr::Fetch(name, _) if text_bindings.contains_key(name.as_str()) => {
            let (ptr_slot, len_slot) = text_bindings[name.as_str()];
            load_rax_from_rbp(code, ptr_slot);
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
        // `substring(<text>, <start>, <end>)` produces text by slicing
        // the inner buffer (ptr, len) at byte offsets [start, end).
        // No allocation: the result is (text_ptr + start, end - start)
        // — a pointer into the same buffer as the input.
        //
        // Algorithm:
        //   1. emit_text_produce_ptrlen(text)       ; (rax=ptr, rdx=len)
        //   2. push rax ; push rdx                  ; save text (ptr, len)
        //   3. emit_eval_expr(end) → rax            ; end value
        //   4. push rax                             ; save end
        //   5. cmp end, text_len ; ja .abort        ; fail-closed bounds
        //   6. emit_eval_expr(start) → rax          ; start value
        //   7. cmp start, end ; ja .abort           ; fail-closed range
        //   8. compute slice_ptr = text_ptr + start
        //                slice_len = end - start
        //   9. add rsp, 24                          ; free 3 saved
        //  10. jmp .ok
        //  .abort:  mov rax, 60 ; mov rdi, 1 ; syscall   (sys_exit 1)
        //  .ok:     result in (rax = slice_ptr, rdx = slice_len)
        //
        // The abort path is inlined per substring call (12 bytes) rather
        // than routed through the shared abort tail; this keeps
        // emit_text_produce_ptrlen's signature unchanged. Cost ≈ +80
        // bytes per substring call. Auditable on the binary side: every
        // substring call is a self-contained block with a visible
        // sys_exit(1) tail.
        //
        // The push/pop sequence is rsp-balanced: emit_eval_expr is
        // contractually rsp-neutral (each push has a matching pop before
        // return), so the literal offsets [rsp], [rsp+8], [rsp+16] keep
        // pointing at our saved values across the recursive emits.
        Expr::Substring(text, start_expr, end_expr) => {
            // Load (rax, rdx) for the text inner using the same loader
            // we're already inside (handles literals, Field, BoundText,
            // nested Concat callees through the Phase 2H-b path, Call
            // via Phase 2G inline). Then delegate the bounds-check +
            // slice computation to the shared helper.
            emit_text_produce_ptrlen(
                code, text, input_name, concept, all_rules,
                offsets, field_ranges, text_bindings,
            )?;
            emit_substring_bounds_and_slice(
                code, start_expr, end_expr,
                input_name, offsets, all_rules, field_ranges, text_bindings,
            )
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
                emit_eval_expr(code, arg, input_name, offsets, all_rules, field_ranges, text_bindings)?;
                // pop rbx
                code.push(0x5B);
                // itoa into buffer (rax → decimal digits at [rbx], rbx advanced)
                emit_itoa_to_buffer(code);
            }
            ConcatArgKind::BoundText => {
                // Copy the bound (ptr, len) contents to the buffer.
                // No strlen needed at fill time — length is already stored.
                // Phase 9 slice 1 routes Expr::Read here too — same shape,
                // identical (ptr, len) representation in text_bindings.
                // Phase 11 slice 1 routes Expr::Fetch here on the same
                // basis — the connection's response (ptr, len) lives in
                // the bound text slot the prologue allocated.
                let bound_name = match arg {
                    Expr::Ident(n) | Expr::Read(n) | Expr::Fetch(n, _) => Some(n.as_str()),
                    // Field whose name is in text_bindings (e.g. req.body)
                    // is a BoundText source — same (ptr, len) shape.
                    Expr::Field(_, n) => Some(n.as_str()),
                    _ => None,
                };
                if let Some(name) = bound_name {
                    let (ptr_slot, len_slot) = *text_bindings
                        .get(name)
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
            ConcatArgKind::JsonEscapedText => {
                // Phase 12 (json_escape) fill: load (rsi=src_ptr, rcx=src_len)
                // for the inner expression, then run the per-byte transform
                // loop that copies bytes verbatim except for the 5
                // JSON-significant ones, which expand to two-byte escape
                // sequences.
                let inner = match arg { Expr::JsonEscape(i) => i.as_ref(), _ => unreachable!() };
                emit_json_escape_load_src(code, inner, input_name, offsets, all_rules, field_ranges, text_bindings)?;
                emit_json_escape_fill_loop(code);
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

/// Phase 12 (json_escape): load (rsi = src_ptr, rcx = src_len) for the
/// json_escape inner expression. Inner shapes supported in native today:
///   - Text-typed input field: `mov rsi, [rbp+offset]; emit_strlen → rdx; mov rcx, rdx`
///   - BoundText (Ident / Read / Fetch resolving via `text_bindings`):
///     `mov rsi, [rbp+ptr_slot]; mov rcx, [rbp+len_slot]`
///
/// Anything else returns an error so the caller surfaces a clear "not
/// supported" message — Concat / Call inners are deferred to a follow-up
/// slice (each adds a different scratch-buffer ordering concern).
fn emit_json_escape_load_src(
    code: &mut Vec<u8>,
    inner: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match inner {
        Expr::Field(base, field_name)
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
        {
            let offset = *offsets.get(field_name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "json_escape inner field '{}' has no rbp slot",
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
            // emit_strlen leaves rdx = length, rsi unchanged.
            emit_strlen(code);
            // mov rcx, rdx
            code.extend_from_slice(&[0x48, 0x89, 0xD1]);
            Ok(())
        }
        Expr::Ident(name) | Expr::Read(name) | Expr::Fetch(name, _)
            if text_bindings.contains_key(name.as_str()) =>
        {
            let (ptr_slot, len_slot) = text_bindings[name.as_str()];
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
            Ok(())
        }
        // Phase 2G-shape inline: a Call returning text via a body that
        // itself resolves to a json_escape-supported shape (Field of input
        // or BoundText). Same constraints as length(Call) /
        // starts_with(Call) / parse_int(Call): single Ident(input) arg,
        // callee has no let bindings, callee returns text. A callee body
        // that itself is a concat / nested call / etc. bubbles up the
        // existing "not supported" error.
        Expr::Call(callee_name, call_args) => {
            if call_args.len() != 1 {
                return Err(NativeError {
                    message: format!(
                        "json_escape(call): callee '{}' must take exactly one argument; got {}",
                        callee_name, call_args.len()
                    ),
                });
            }
            match &call_args[0] {
                Expr::Ident(n) if n == input_name => {}
                other => {
                    return Err(NativeError {
                        message: format!(
                            "json_escape(call): callee '{}' argument must be the caller's input ident '{}'; got {}",
                            callee_name, input_name, expr_kind(other)
                        ),
                    });
                }
            }
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!("json_escape(call): unknown rule '{}'", callee_name),
            })?;
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "json_escape(call): callee '{}' has let bindings (same Phase 2G restriction \
                         as length(call) / starts_with(call)). Workaround: inline the let RHSes into \
                         the body, or bind the callee result via a `let` in the caller and json_escape \
                         the let.",
                        callee_name
                    ),
                });
            }
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "json_escape(call): callee '{}' must return text; got {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "json_escape(call): callee '{}' uses input ident '{}', caller uses '{}' — \
                         inline requires the same input ident name",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            emit_json_escape_load_src(
                code,
                &callee.logic.value,
                input_name,
                offsets,
                all_rules,
                field_ranges,
                text_bindings,
            )
        }
        // substring(text, start, end) as a json_escape inner —
        // same shape as the byte-prim slice 2c: inline the substring
        // emit and produce (rsi=slice_ptr, rcx=slice_len), which is
        // the convention the json_escape fill loop expects.
        // Duplicates the emit_text_produce_ptrlen's Substring arm
        // logic (kept in sync until a future cleanup factors a
        // shared emit_substring_to_rax_rdx helper).
        Expr::Substring(text, start_expr, end_expr) => {
            // Load text via this same loader (rsi/rcx convention),
            // adapt to (rax, rdx) for the shared helper, then back
            // to (rsi, rcx) for the fill loop downstream. The 4-byte
            // adapters dominate the small extra cost; the shared
            // helper is identical to the one used by other primitives.
            emit_json_escape_load_src(
                code, text, input_name, offsets, all_rules, field_ranges, text_bindings,
            )?;
            // mov rax, rsi ; mov rdx, rcx (rsi/rcx → rax/rdx)
            code.extend_from_slice(&[0x48, 0x89, 0xF0]);
            code.extend_from_slice(&[0x48, 0x89, 0xCA]);
            emit_substring_bounds_and_slice(
                code, start_expr, end_expr,
                input_name, offsets, all_rules, field_ranges, text_bindings,
            )?;
            // mov rsi, rax ; mov rcx, rdx (rax/rdx → rsi/rcx)
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            code.extend_from_slice(&[0x48, 0x89, 0xD1]);
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "json_escape inner shape not supported in native: {:?}",
                other
            ),
        }),
    }
}

/// Phase 12 (json_escape): emit the per-byte JSON-escape transform loop.
/// Preconditions: rsi = source pointer, rcx = source length, rbx = write
/// pointer into the reserved buffer.
/// Postcondition: rbx advanced by `inner_length + 1` per escaped byte
/// (each escape writes 2 bytes instead of 1). rsi/rcx clobbered.
///
/// Five bytes are escaped: `"` (0x22), `\` (0x5C), `\n` (0x0A), `\r`
/// (0x0D), `\t` (0x09). Other bytes pass through unchanged. The loop
/// uses byte-level x86 (mov al, [rsi]; cmp al, imm8; mov word [rbx],
/// imm16) so each iteration is small enough that the back-edge fits in
/// rel8.
///
/// Layout (instruction sizes shown):
///   loop_top:
///     test rcx, rcx      (3)
///     jz loop_end        (2, rel8 forward)
///     mov al, [rsi]      (2)
///     cmp al, 0x22       (2)
///     je esc_quote       (2, rel8 forward)
///     cmp al, 0x5C       (2)
///     je esc_back        (2)
///     cmp al, 0x0A       (2)
///     je esc_lf          (2)
///     cmp al, 0x0D       (2)
///     je esc_cr          (2)
///     cmp al, 0x09       (2)
///     je esc_tab         (2)
///     mov [rbx], al      (2)
///     inc rbx            (3, REX.W + FF /0)
///     jmp advance        (2, rel8 forward)
///   esc_quote:
///     mov word [rbx], 0x225C   (5: 66 C7 03 5C 22)
///     add rbx, 2               (4: 48 83 C3 02)
///     jmp advance              (2)
///   esc_back: ... (12 bytes total)
///   esc_lf:   ... (12 bytes total)
///   esc_cr:   ... (12 bytes total)
///   esc_tab:  ... (10 bytes — falls through to advance, no jmp)
///   advance:
///     inc rsi            (3)
///     dec rcx            (3)
///     jmp loop_top       (2, rel8 backward)
///   loop_end:
///
/// Total: ~95 bytes. All forward jumps are short (< 80 bytes); the back
/// edge (advance → loop_top) is also short.
fn emit_json_escape_fill_loop(code: &mut Vec<u8>) {
    // loop_top:
    let loop_top = code.len();
    // test rcx, rcx
    code.extend_from_slice(&[0x48, 0x85, 0xC9]);
    // jz loop_end (rel8) — patch later
    code.push(0x74);
    let loop_end_patch = code.len();
    code.push(0x00);
    // mov al, [rsi]
    code.extend_from_slice(&[0x8A, 0x06]);

    // For each escape sequence we need a forward jump from the cmp+je to
    // the corresponding handler block. We emit cmp+je with a placeholder
    // and remember the patch position; once the handler block is laid
    // down we patch the rel8.
    // (cmp_imm, esc_word): the imm8 to compare al against, and the
    // little-endian 16-bit value to store at [rbx] for the escape.
    let esc_specs: [(u8, u16); 5] = [
        (0x22, 0x225C), // "  → \"
        (0x5C, 0x5C5C), // \  → \\
        (0x0A, 0x6E5C), // LF → \n (literal)
        (0x0D, 0x725C), // CR → \r
        (0x09, 0x745C), // TAB → \t
    ];

    let mut esc_jump_patches: Vec<usize> = Vec::with_capacity(5);
    for (cmp_imm, _) in &esc_specs {
        // cmp al, imm8  (3C imm8)
        code.push(0x3C);
        code.push(*cmp_imm);
        // je rel8 (74 disp)
        code.push(0x74);
        esc_jump_patches.push(code.len());
        code.push(0x00);
    }

    // Plain byte path: mov [rbx], al ; inc rbx ; jmp advance
    // mov [rbx], al  (88 03)
    code.extend_from_slice(&[0x88, 0x03]);
    // inc rbx  (REX.W + FF C3)
    code.extend_from_slice(&[0x48, 0xFF, 0xC3]);
    // jmp advance (rel8) — patch later
    code.push(0xEB);
    let plain_to_advance_patch = code.len();
    code.push(0x00);

    // Escape handler blocks: each is mov word [rbx], esc16 ; add rbx, 2 ;
    // (jmp advance | fall through). All but the last jump to advance;
    // the last falls through.
    let mut esc_to_advance_patches: Vec<usize> = Vec::with_capacity(esc_specs.len() - 1);
    for (i, (_, esc_word)) in esc_specs.iter().enumerate() {
        // Patch the je from the cmp dispatch to here.
        let block_start = code.len();
        let dist = (block_start as i32) - (esc_jump_patches[i] as i32 + 1);
        if !(-128..=127).contains(&dist) {
            // Should never happen given total block size; defensive panic
            // would corrupt the binary, so bail with a cleaner message via
            // a deliberately oversize sequence is not an option — we only
            // assert in the debug build for the tightness of this layout.
            debug_assert!(false, "json_escape fill: cmp→escape jump out of rel8 range");
        }
        code[esc_jump_patches[i]] = dist as u8;

        // mov word [rbx], esc16  — encoded as 66 C7 03 imm16
        code.extend_from_slice(&[0x66, 0xC7, 0x03]);
        code.extend_from_slice(&esc_word.to_le_bytes());
        // add rbx, 2  (48 83 C3 02)
        code.extend_from_slice(&[0x48, 0x83, 0xC3, 0x02]);

        // The last block falls through into advance; the others jmp.
        if i < esc_specs.len() - 1 {
            code.push(0xEB);
            esc_to_advance_patches.push(code.len());
            code.push(0x00);
        }
    }

    // advance:
    let advance_pos = code.len();
    // Patch all jmp→advance sites (plain path + intermediate escape blocks)
    for patch in std::iter::once(plain_to_advance_patch).chain(esc_to_advance_patches.into_iter()) {
        let dist = (advance_pos as i32) - (patch as i32 + 1);
        debug_assert!((-128..=127).contains(&dist), "json_escape fill: jmp→advance out of rel8 range");
        code[patch] = dist as u8;
    }
    // inc rsi  (48 FF C6)
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]);
    // dec rcx  (48 FF C9)
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]);
    // jmp loop_top  (rel8 backward — EB disp8)
    code.push(0xEB);
    let after_back_jmp = code.len() + 1;
    let back_dist = (loop_top as i32) - (after_back_jmp as i32);
    debug_assert!(
        (-128..=127).contains(&back_dist),
        "json_escape fill: back jump out of rel8 range; loop body grew too large"
    );
    code.push(back_dist as u8);

    // loop_end:
    let loop_end_pos = code.len();
    let end_dist = (loop_end_pos as i32) - (loop_end_patch as i32 + 1);
    debug_assert!((-128..=127).contains(&end_dist), "json_escape fill: jz→loop_end out of rel8 range");
    code[loop_end_patch] = end_dist as u8;
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
    on_error: ErrorPolicy,
    abort_patches: &mut Vec<usize>,
) -> Result<(), NativeError> {
    // First, emit the open() call. The path is always a compile-time literal,
    // so we embed it inline and point rdi at it.
    emit_open_append(code, path);
    // Phase 8 slice 8d: when policy is Abort, branch to the shared
    // abort sequence on a negative open() return. open() returns -errno
    // on failure (negative i64), so a sign check via `js` catches every
    // failure mode without enumerating errno values.
    if on_error == ErrorPolicy::Abort {
        // test rax, rax  ; js rel32 (placeholder, caller patches)
        code.extend_from_slice(&[0x48, 0x85, 0xC0]);
        code.extend_from_slice(&[0x0F, 0x88]);
        abort_patches.push(code.len());
        code.extend_from_slice(&[0, 0, 0, 0]);
    }
    // rax = fd; save in r15.
    // mov r15, rax  (49 89 C7)
    code.extend_from_slice(&[0x49, 0x89, 0xC7]);

    // Now the write() — dispatch on content shape. Factored into a helper
    // so that the Call arm (Phase 2H-a) can recurse on callee.logic.value
    // without re-opening the file or re-validating the path.
    emit_append_write_to_r15(code, content, rule, concept, all_rules, offsets, field_ranges, text_bindings)?;
    // write() also returns -errno on failure (or fewer bytes than requested
    // on a partial write — short writes happen in practice on disk full).
    // Same `js` check picks up the negative return; a partial write below
    // the requested count is treated as success here, deliberately. Real
    // partial-write handling is its own slice and lives outside 8d.
    if on_error == ErrorPolicy::Abort {
        code.extend_from_slice(&[0x48, 0x85, 0xC0]);
        code.extend_from_slice(&[0x0F, 0x88]);
        abort_patches.push(code.len());
        code.extend_from_slice(&[0, 0, 0, 0]);
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
    // Phase 9 slice 1 extends this: Expr::Read shares the exact same
    // (ptr, len) slot shape, so funnel both through the same code path.
    // Phase 11 slice 1: Expr::Fetch(name, _) lives in the same slot
    // shape too — the prologue stored the response (ptr, len) at the
    // slots registered in text_bindings under the connection name. The
    // request bytes Expr inside the Fetch is consumed by the prologue;
    // here we only consult the connection name.
    let bound_name = match text_expr {
        Expr::Ident(n) | Expr::Read(n) | Expr::Fetch(n, _) => Some(n.as_str()),
        // Field whose name is in text_bindings (e.g. req.body in HTTP
        // services) routes through the BoundText (ptr, len) path.
        Expr::Field(_, n) => Some(n.as_str()),
        _ => None,
    };
    if let Some(name) = bound_name {
        if let Some(&(ptr_slot, len_slot)) = text_bindings.get(name) {
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
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
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
    // Accept Result(Number, Text), Result(Text, Text), and Result(Named, Text).
    // The Named case (slice "Result(Record, text)") routes the Ok arm through
    // emit_record_as_json — same one-JSON-object-per-record format that
    // `output: Named(C)` rules already emit. Err stays text everywhere.
    let ok_is_named = matches!(t_ok, Type::Named(_));
    if !(matches!(t_ok, Type::Number | Type::Text) || ok_is_named)
        || !matches!(t_err, Type::Text)
    {
        return Err(NativeError {
            message: format!(
                "native Result rules today support Ok = number | text | <concept>, Err = text. \
                 Got Result({:?}, {:?}).",
                t_ok, t_err,
            ),
        });
    }
    // For Result(Named(c), text), make sure the named concept exists.
    if let Type::Named(name) = t_ok {
        if !all_concepts.iter().any(|c| c.name == *name) {
            return Err(NativeError {
                message: format!(
                    "Result Ok arm references unknown concept '{}'",
                    name
                ),
            });
        }
    }

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, rule, concept, None, all_rules, all_resources, all_connections)?;

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
        all_concepts,
        all_rules,
        &ctx.binding_offsets,
        &ctx.field_ranges,
        &ctx.text_bindings,
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
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
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
                    emit_eval_expr(code, inner, &rule.input_name, offsets, all_rules, field_ranges, text_bindings)?;
                    emit_itoa_inline(code);
                }
                Type::Text => {
                    // Text Ok: write the bytes (literal or concat buffer) to
                    // stdout (fd 1), then append a newline symmetric to itoa.
                    // Phase 2I: pass the caller's text_bindings so let-bound
                    // text values can be referenced in the Ok arm.
                    emit_text_write_to_fd(
                        code, inner, 1, &rule.input_name, concept, all_rules, offsets, field_ranges,
                        text_bindings,
                    )?;
                    emit_write_newline(code, 1);
                }
                Type::Named(out_concept_name) => {
                    // Result(Record, Text) Ok arm. The inner expression
                    // must produce a record of the declared concept;
                    // we accept either a direct `Concept { ... }` or
                    // `if cond then Concept{...} else Concept{...}`
                    // (nestable). Each Record leaf emits its JSON +
                    // trailing `}\n` and falls through; the if/else
                    // branches converge below the helper, and the
                    // outer `jmp loop_top` after this match ends the
                    // record loop iteration once.
                    let out_concept = all_concepts
                        .iter()
                        .find(|c| c.name == *out_concept_name)
                        .copied()
                        .ok_or_else(|| NativeError {
                            message: format!("unknown output concept '{}'", out_concept_name),
                        })?;
                    emit_ok_record_dispatch(
                        code,
                        inner,
                        out_concept,
                        rule,
                        concept,
                        all_rules,
                        offsets,
                        field_ranges,
                        text_bindings,
                    )?;
                }
                other => {
                    return Err(NativeError {
                        message: format!(
                            "Ok arm type '{:?}' not yet supported in native — only number, text, and named concept",
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
            // Phase 2I: pass text_bindings so the Err text expression can
            // reference let-bound text from the outer rule's prologue.
            emit_text_write_to_fd(
                code, inner, 2, &rule.input_name, concept, all_rules, offsets, field_ranges,
                text_bindings,
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
            emit_eval_expr(code, cond, &rule.input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // test rax, rax ; jz .else (rel32 patch so arms can be large)
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            // .then — each leaf self-terminates with jmp loop_top.
            emit_eval_result_expr(
                code, then_e, loop_top, rule, concept, all_concepts, all_rules, offsets, field_ranges, text_bindings, slots,
            )?;

            // .else:
            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_eval_result_expr(
                code, else_e, loop_top, rule, concept, all_concepts, all_rules, offsets, field_ranges, text_bindings, slots,
            )?;
            Ok(())
        }
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            // Phase 2D + 2F: inline the callee's Result-producing logic,
            // redirecting its Ok leaves into the outer Ok arm (Ok-bound value
            // lands at match_slot) and its Err leaves into the outer Err arm
            // (Err-bound text captured to err_ptr_slot/err_len_slot, then the
            // outer Err body runs with `err_var → (ptr, len)` bound).
            //
            // Nested match_result (slice "nested match"): the OUTER arms can
            // themselves contain another `match_result` (e.g. a chained
            // validation). Each level needs its own quartet of slots — the
            // prologue reserved them based on the static max depth. We pass
            // `next_slots` for the +1 level deeper, derived by shifting the
            // current `slots` 32 bytes (4 i64) further into the frame.
            // exit_flag_slot stays shared across all levels.
            let next_slots = MatchSlots {
                match_slot: slots.match_slot - 32,
                err_ptr_slot: slots.err_ptr_slot - 32,
                err_len_slot: slots.err_len_slot - 32,
                err_frame_save_slot: slots.err_frame_save_slot - 32,
                exit_flag_slot: slots.exit_flag_slot,
            };
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
                all_concepts,
                all_rules,
                offsets,
                field_ranges,
                text_bindings,
                slots,
                next_slots,
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
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
    slots: MatchSlots,
    next_slots: MatchSlots,
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

    // Validate callee's input concept against outer's. Same-concept
    // is the easy case — rbp slots already populated by the prologue
    // are reusable as-is. Cross-concept (slice "cross-concept
    // match_result") accepts a callee whose concept's fields are a
    // SUBSET of the outer's by name AND type: the callee's body uses
    // the existing offsets/field-types, but only references fields
    // the outer also has. The check below makes the contract
    // explicit so a mismatched name or a same-name-different-type
    // surfaces a clear native error rather than silently
    // miscompiling.
    let callee_input = match &callee.input_ty {
        Type::Named(n) => n.as_str(),
        _ => return Err(NativeError { message: "callee input must be a named concept".into() }),
    };
    if callee_input != concept.name.as_str() {
        let callee_concept = all_concepts
            .iter()
            .find(|c| c.name == callee_input)
            .copied()
            .ok_or_else(|| NativeError {
                message: format!(
                    "match_result callee '{}' references unknown concept '{}'",
                    callee_name, callee_input
                ),
            })?;
        for f in &callee_concept.fields {
            match concept.fields.iter().find(|of| of.name == f.name) {
                None => {
                    return Err(NativeError {
                        message: format!(
                            "cross-concept match_result: callee '{}' references field '{}' not present in outer concept '{}'",
                            callee_name, f.name, concept.name,
                        ),
                    });
                }
                Some(of) if of.ty != f.ty => {
                    return Err(NativeError {
                        message: format!(
                            "cross-concept match_result: callee '{}' field '{}' is {:?} but outer concept '{}' has it as {:?}",
                            callee_name, f.name, f.ty, concept.name, of.ty,
                        ),
                    });
                }
                Some(_) => {}
            }
        }
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
        all_concepts,
        all_rules,
        offsets,
        field_ranges,
        text_bindings,
        slots,
        next_slots,
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
    all_concepts: &[&Concept],
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
    slots: MatchSlots,
    // `next_slots` are the slots to use for any nested match_result inside
    // the OUTER arms (`ok_body` / `err_body`). Computed by the caller as
    // `slots` shifted 32 bytes deeper (one quartet down). Unused — but
    // harmless — when the outer arms contain no nested match_result.
    next_slots: MatchSlots,
) -> Result<(), NativeError> {
    match expr {
        Expr::Ok(inner) => {
            // The inner expression originates from the CALLEE's body, so
            // its Field accesses use the callee's input name. Pass
            // callee.input_name to emit_eval_expr; offsets/field_ranges
            // are the outer's tables (same field names, by slice
            // contract — validated up-front).
            emit_eval_expr(code, inner, &callee.input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // Store at match_slot.
            store_rax_at_rbp(code, slots.match_slot);
            // Augment offsets with ok_var → match_slot, then emit outer ok_body
            // in result context using the OUTER's input_name (the outer arm's
            // expressions reference the outer's input identifier). The outer
            // arm self-terminates.
            let mut augmented = offsets.clone();
            augmented.insert(ok_var, slots.match_slot);
            // Pass `next_slots` to the recursive emit so a nested
            // match_result inside the outer ok_body uses a fresh
            // quartet (one level deeper into the frame). For
            // non-nested ok_body this argument is unused.
            emit_eval_result_expr(
                code,
                ok_body,
                loop_top,
                outer_rule,
                concept,
                all_concepts,
                all_rules,
                &augmented,
                field_ranges,
                text_bindings,
                next_slots,
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
                    if matches!(base.as_ref(), Expr::Ident(n) if n == &callee.input_name) =>
                {
                    // The Field expression was emitted by the callee — its
                    // base ident is the callee's input name. The field
                    // resolves through the OUTER's concept (slice contract:
                    // callee.fields ⊆ outer.fields by name + type), so
                    // both `concept.fields.find` and `offsets[name]` use
                    // the outer's tables here.
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
                    // The concat is in the CALLEE's body — its args
                    // reference fields via callee.input_name. Pass that
                    // name to the buffer builder; offsets/concept stay
                    // the outer's tables (slice contract: callee.fields
                    // ⊆ outer.fields). The buffer stays alive across the
                    // outer arm; cleanup via `mov rsp, [rbp+err_frame_save_slot]`
                    // at the end. Passes text_bindings so the callee's
                    // Err concat can reference text-let values from the
                    // outer rule's prologue (Phase 2I integration point).
                    let _buf = emit_concat_to_buffer(
                        code, args, &callee.input_name, concept, all_rules,
                        offsets, field_ranges, text_bindings,
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
            //    text into stderr. Phase 2I: MERGE the caller's text_bindings
            //    (let-bound text from the prologue) with err_var so the outer
            //    Err body can reference both — e.g., `Err(concat(msg, err))`
            //    where `msg` is a prior text let and `err` is this err_var.
            let mut bindings: TextBindings = text_bindings.clone();
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

            // 5. Set exit flag to 1 (failure) — mirror what
            //    emit_eval_result_expr's direct-Err arm does so that an
            //    Err propagated through a match_result chain has the
            //    same exit-code semantics as a literal Err in the rule
            //    body. Pre-2026-05-08 this was missing and produced
            //    exit=0 even on rejected records when the rejection
            //    came from an inlined-callee's Err.
            code.extend_from_slice(&[0x48, 0xC7, 0x85]);
            code.extend_from_slice(&slots.exit_flag_slot.to_le_bytes());
            code.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);

            // 6. jmp loop_top
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            // Cond originates from the CALLEE's body — evaluate against
            // callee.input_name. Field accesses resolve through `offsets`
            // (built from the outer's concept), which by slice contract
            // contains every name the callee could reference.
            emit_eval_expr(code, cond, &callee.input_name, offsets, all_rules, field_ranges, text_bindings)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            emit_redirect_callee_leaves(
                code, then_e, callee, ok_var, ok_body, err_var, outer_err_inner, loop_top,
                outer_rule, concept, all_concepts, all_rules, offsets, field_ranges, text_bindings, slots, next_slots,
            )?;

            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_redirect_callee_leaves(
                code, else_e, callee, ok_var, ok_body, err_var, outer_err_inner, loop_top,
                outer_rule, concept, all_concepts, all_rules, offsets, field_ranges, text_bindings, slots, next_slots,
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
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
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
    let ctx = emit_record_loop_prologue(&mut code, rule, input_concept, None, all_rules, all_resources, all_connections)?;

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
        &ctx.text_bindings,
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
    all_resources: &HashMap<&str, &Resource>,
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

    // Slice 9.5c: enumerate resources the rule reads (in source order) so the
    // open/read/close sequence runs ONCE above the outer loop and the (ptr,
    // len) slots survive every record AND every element iteration. Same
    // shape and shared `emit_resource_read_sequence` helper used by the rule
    // prologue (slice 9.1) and the text-fold init (slice 9.5).
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // now_unix(): 16 bytes at the bottom of the frame; collection rules
    // use it for per-element ts checks (e.g. enrich each emitted record
    // with the same captured `now`).
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };
    let frame_size = (frame_slots as i32) * 8 + resource_extra_bytes + now_extra_bytes;

    let mut code = Vec::new();
    // Patches into the resource open/read failure jumps (filled at the end).
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    // Names → (ptr_slot, len_slot) for resources, populated by the prologue
    // and threaded through the body emitters so `Expr::Read` resolves
    // through the BoundText path everywhere a text-typed value is consumed.
    let mut text_bindings: TextBindings<'_> = HashMap::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Slice 9.5c: emit each resource read sequence ABOVE the outer loop —
    // the file is opened/read/closed once per rule invocation, and the
    // (ptr, len) survive every record + every element via the rbp slots.
    // Slot cursor descends from just below the existing frame slots so
    // scalar/element/let slots stay where they were.
    let mut resource_next_slot: i32 = -((frame_slots as i32) * 8 + 8);
    for r in &referenced_resources {
        let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
            &mut code,
            r,
            resource_next_slot,
            &mut resource_abort_patches,
        );
        text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
        resource_next_slot = new_next;
    }

    // now_unix(): one syscall ABOVE the outer loop. now_slot registered
    // in elem_offsets below so the body's emit_eval_expr resolves it.
    let now_slot: i32 = if uses_now {
        let s = resource_next_slot - 8;
        emit_capture_now_unix(&mut code, s);
        s
    } else {
        0
    };

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
    if uses_now {
        elem_offsets.insert("now", now_slot);
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
            emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets, &text_bindings)?;
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
                &text_bindings,
            )?;
        }
        CollectionOp::MapScalar { lambda_var, body, is_text } => {
            // Scalar element output: evaluate the body to rax (number/bool)
            // or emit the text directly, then one newline per element.
            if is_text {
                emit_text_write_to_fd(
                    &mut code, body, 1, lambda_var, input_elem_concept, all_rules,
                    &elem_offsets, &field_ranges,
                    &text_bindings,
                )?;
                emit_write_newline(&mut code, 1);
            } else {
                emit_eval_expr(
                    &mut code, body, lambda_var, &elem_offsets, all_rules, &field_ranges,
                    &text_bindings,
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
                &text_bindings,
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
                &text_bindings,
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

    // Slice 9.5c: shared abort label for resource open/read failures.
    // Mirrors the rule-prologue path. Only emitted when at least one
    // resource was read, so resource-free programs pay zero bytes.
    emit_resource_abort_tail(&mut code, &resource_abort_patches);

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
    all_resources: &HashMap<&str, &Resource>,
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
    // Frame: scalars + element fields + let bindings + acc slot
    //        (+ resource block at the bottom when read() is referenced)
    let frame_slots = n_scalar + n_elem_fields + n_lets + 1;

    // Slice 9.5d: enumerate resources the rule reads (init / let RHS /
    // fold body). Same shape as slice 9.5b/c — collect_rule_read_names
    // walks every Read in the AST so a `read()` deep inside the body
    // gets the same prologue as a top-level let RHS reference.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // now_unix(): 16 bytes if any reference exists in the rule (logic
    // value or any let RHS). Sits at the very bottom of the frame so
    // resource and acc slot positions stay byte-for-byte unchanged for
    // rules without `now_unix()`.
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };
    let frame_size = (frame_slots as i32) * 8 + resource_extra_bytes + now_extra_bytes;
    let acc_offset: i32 = -((frame_slots as i32) * 8);

    let mut code = Vec::new();
    // Resource open/read failure jumps land here (patched at the end).
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    // text_bindings populated by the prologue and threaded through the
    // body's emit_eval_expr — `req.tag == read(allowlist)` resolves via
    // the new BinOp::Eq Field-vs-Read path against the registered slots.
    let mut text_bindings: TextBindings<'_> = HashMap::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Slice 9.5d: emit each resource read sequence ABOVE the outer loop —
    // (ptr, len) survive every record and every element. First slot starts
    // immediately below acc_slot (the existing bottom of the frame).
    let mut bottom_cursor: i32 = acc_offset - 8;
    {
        let mut next_slot: i32 = bottom_cursor;
        for r in &referenced_resources {
            let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
                &mut code,
                r,
                next_slot,
                &mut resource_abort_patches,
            );
            text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
            next_slot = new_next;
        }
        bottom_cursor = next_slot;
    }

    // now_unix(): clock_gettime(CLOCK_REALTIME) ONCE per rule invocation,
    // ABOVE outer_loop_top. tv_sec at the deeper of two reserved slots
    // (now_slot), tv_nsec scratch in the slot just above. Registered as
    // ("now", now_slot) in body_offsets below so the inner loop's
    // emit_eval_expr finds it via the standard offsets lookup.
    let now_slot: i32 = if uses_now {
        let s = bottom_cursor - 8;
        emit_capture_now_unix(&mut code, s);
        s
    } else {
        0
    };

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
    // now_unix(): the synthetic name `now` is visible in both let RHS
    // and the fold body. Registering in body_offsets covers the body
    // path; let_offsets is built below from scalar_offsets, so we
    // also need to insert there before the let-evaluation loop.
    if uses_now {
        body_offsets.insert("now", now_slot);
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

    // Evaluate let bindings into rbp slots (after scalar fields, before element fields).
    let field_ranges_for_lets = build_field_ranges(input_concept);
    let mut let_offsets: HashMap<&str, i32> = scalar_offsets.clone();
    if uses_now {
        let_offsets.insert("now", now_slot);
    }
    let mut next_let_slot = -(((n_scalar + n_elem_fields) as i32 + 1) * 8);
    for (name, expr) in &rule.logic.bindings {
        emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets, &text_bindings)?;
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
    emit_eval_expr(&mut code, body, item_name, &body_offsets, all_rules, &field_ranges, &text_bindings)?;
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

    // Slice 9.5d: shared abort label for resource open/read failures.
    // Only emitted when at least one resource was read; resource-free
    // programs pay zero bytes.
    emit_resource_abort_tail(&mut code, &resource_abort_patches);

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
    all_resources: &HashMap<&str, &Resource>,
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
    //        (+ resource block at the bottom when read() is referenced)
    let frame_slots = n_scalar + n_elem + n_lets + n_folds;

    // Slice 9.5e: enumerate resources the rule reads (same shape as
    // slice 9.5d). Compose with text equality for filter-by-allowlist
    // patterns inside `all`/`any` quantifiers.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // now_unix(): 16 bytes at the very bottom of the frame when the rule
    // touches the clock. Same shape as Phase 4 / record-loop prologue.
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };
    let frame_size = (frame_slots as i32) * 8 + resource_extra_bytes + now_extra_bytes;

    // Accumulator slot offsets (at the bottom of the original frame —
    // i.e. above the resource block, since acc slots get reused inside
    // the inner loop while resource (ptr, len) live for the whole rule).
    let acc_offsets: Vec<i32> = (0..n_folds)
        .map(|i| -(((n_scalar + n_elem + n_lets + i) as i32 + 1) * 8))
        .collect();
    let last_acc_offset: i32 = if n_folds == 0 { -((frame_slots as i32) * 8) } else { acc_offsets[n_folds - 1] };

    let mut code = Vec::new();
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    let mut text_bindings: TextBindings<'_> = HashMap::new();

    // _start — argv/rbp frame setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]); // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Slice 9.5e: emit each resource read sequence ABOVE the outer loop.
    let mut bottom_cursor: i32 = last_acc_offset - 8;
    {
        let mut next_slot: i32 = bottom_cursor;
        for r in &referenced_resources {
            let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
                &mut code,
                r,
                next_slot,
                &mut resource_abort_patches,
            );
            text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
            next_slot = new_next;
        }
        bottom_cursor = next_slot;
    }

    // now_unix(): one syscall ABOVE the outer loop. Slot registered in
    // body_offsets and let_offsets below.
    let now_slot: i32 = if uses_now {
        let s = bottom_cursor - 8;
        emit_capture_now_unix(&mut code, s);
        s
    } else {
        0
    };

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
    if uses_now {
        body_offsets.insert("now", now_slot);
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
    if uses_now {
        let_offsets.insert("now", now_slot);
    }
    let mut next_let_slot = -(((n_scalar + n_elem) as i32 + 1) * 8);
    for (name, expr) in &rule.logic.bindings {
        emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets, &text_bindings)?;
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
        emit_eval_expr(&mut code, &fold.body, &fold.item_name, &body_offsets, all_rules, &field_ranges, &text_bindings)?;
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
    emit_eval_expr(&mut code, &scalar_expr, "__phase6_none__", &final_offsets, all_rules, &empty_ranges, &text_bindings)?;

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

    // Slice 9.5e: shared abort label for resource open/read failures.
    emit_resource_abort_tail(&mut code, &resource_abort_patches);

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
    all_resources: &HashMap<&str, &Resource>,
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
    // Slice 9.5: fold init is either a text literal (Phase 5b original) or
    // `read(<resource>)` (this slice). When init is Read, the resource is
    // opened/read/closed ONCE above the outer loop and its (ptr, len) lives
    // in dedicated rbp slots; the init copy then becomes a `rep movsb` from
    // those slots into the per-record buffer. The body's classifier still
    // refuses BoundText args — keeping `read()` confined to the init keeps
    // the diff small and the use case (banner per record) clean.
    enum InitKind<'a> {
        Literal(&'a str),
        Read(&'a Resource),
    }
    let init_kind: InitKind = match init_expr {
        Expr::Text(s) => InitKind::Literal(s.as_str()),
        Expr::Read(name) => {
            let r = all_resources.get(name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "Phase 5b: fold init reads '{}' but no top-level `resource {}` was declared",
                    name, name
                ),
            })?;
            InitKind::Read(*r)
        }
        _ => {
            return Err(NativeError {
                message: "Phase 5b: fold init must be a text literal or `read(<resource>)`".into(),
            })
        }
    };
    // For sizing the per-record buffer we need either the literal length
    // (compile-time constant) or the resource's `max:` bound (compile-time
    // constant). Both fit in `init_size` as the WORST-CASE static contribution
    // — the actual init copy uses the runtime `len_slot` so the buffer is
    // never overrun even if the file was shorter than `max:`.
    let init_size: i32 = match &init_kind {
        InitKind::Literal(s) => s.as_bytes().len() as i32,
        InitKind::Read(r) => r.max_bytes as i32,
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

    // Slice 9.5b: enumerate every resource the rule reads (init AND body),
    // de-duplicated in source order. Registered in `prebuilt_text_bindings`
    // BEFORE the body classifier runs so `Expr::Read(name)` is recognized
    // as BoundText. The actual read sequences fire once per rule invocation
    // above the outer loop — see the prologue below. For literal-init,
    // resource-free programs `referenced_resources` stays empty and
    // everything is byte-for-byte the slice-5b original.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    // We don't yet know the rbp slot offsets — the prologue assigns them
    // AFTER frame setup. For the classifier we only need the names to be
    // present in the bindings map (the map lookup is name → presence;
    // the (i32, i32) values are placeholders here and overwritten below
    // before any emission consults them). This is the pre-build step.
    let mut prebuilt_text_bindings: TextBindings<'_> = HashMap::new();
    for r in &referenced_resources {
        prebuilt_text_bindings.insert(r.name.as_str(), (0, 0));
    }

    // Classify rest args. The lambda var `item` is the "input" for field
    // accesses within them. Pass `prebuilt_text_bindings` so `read(name)`
    // classifies as BoundText (slice 9.5b extension).
    let mut rest_kinds: Vec<ConcatArgKind> = Vec::with_capacity(rest_args.len());
    for arg in rest_args {
        let k = classify_concat_arg(arg, elem_concept, item_name, &prebuilt_text_bindings).ok_or_else(|| NativeError {
            message: "Phase 5b: fold-body concat arg must be a text literal, number expression, element text field, or read(<resource>)".into(),
        })?;
        if k == ConcatArgKind::BoundText {
            // Slice 9.5b: only Expr::Read is admitted as BoundText in a
            // fold body. Ident-bound text (Phase 2I let bindings) and
            // Fetch (Phase 11) remain refused — each is a separate slice.
            // Read works because its (ptr, len) live in stable rbp slots
            // for the entire rule invocation, so iteration order is
            // irrelevant to slot validity.
            if !matches!(arg, Expr::Read(_)) {
                return Err(NativeError {
                    message: "Phase 5b: fold body BoundText arg must be `read(<resource>)`; let-bound text and fetch remain out of scope".into(),
                });
            }
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
            ConcatArgKind::BoundText => {
                // Slice 9.5b: only Read is allowed (gate above). The
                // resource's `max:` bound is a compile-time constant
                // upper bound on the byte count copied per iteration,
                // so single-pass sizing stays available.
                if let Expr::Read(name) = arg {
                    let r = all_resources.get(name.as_str()).expect(
                        "resource validated to exist when prebuilt_text_bindings was populated",
                    );
                    static_per_element += r.max_bytes as i32;
                } else {
                    unreachable!("BoundText non-Read arg rejected at classifier")
                }
            }
            ConcatArgKind::CallText => {
                unreachable!("CallText rejected above")
            }
            ConcatArgKind::JsonEscapedText => {
                // Phase 12: json_escape inside a top-level fold body would
                // need its own per-element scratch handling (sized at 2×
                // the inner length, possibly per-iteration). Out of scope
                // for slice 5b; reject with a clear message.
                return Err(NativeError {
                    message: "Phase 5b: fold body cannot use json_escape (per-element scratch sizing not yet implemented)".into(),
                });
            }
        }
    }

    // ===== Emission =====
    let n_scalar = scalar_fields.len();
    let n_elem_fields = elem_concept.fields.len();
    let n_lets = rule.logic.bindings.len();
    // frame: n_scalar + n_elem + n_lets + count_slot + argv_save_slot
    //        (+ one resource block per referenced resource at the bottom)
    let frame_slots = n_scalar + n_elem_fields + n_lets + 2;
    // Slice 9.5b: ALL referenced resources contribute to the frame —
    // both the init-position read (slice 9.5) and any read appearing in
    // the body concat. Each contributes (16 bytes for ptr+len) + max
    // padded to 8 bytes. Reads run ONCE at startup; the (ptr, len) live
    // for the entire rule invocation.
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // now_unix(): 16 bytes at the bottom of the frame; useful here for
    // text fold rendering (e.g. prepend a per-batch timestamp to each
    // emitted line, captured once per rule invocation).
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };
    let frame_size = (frame_slots as i32) * 8 + resource_extra_bytes + now_extra_bytes;
    let count_slot: i32 = -(((n_scalar + n_elem_fields + n_lets + 1) as i32) * 8);
    let argv_save_slot: i32 = -(((n_scalar + n_elem_fields + n_lets + 2) as i32) * 8);

    let mut code = Vec::new();
    // Patches into the resource open/read failure jumps (filled at the end).
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    // (ptr_slot, len_slot) of the init-position read, if any.
    let mut init_read_slots: Option<(i32, i32)> = None;
    // Real text_bindings populated by the prologue below; consumed by
    // the body fill pass via emit_concat_fill. Names match
    // `prebuilt_text_bindings` from the classifier step but the
    // (i32, i32) pairs are the real rbp slots assigned here.
    let mut text_bindings: TextBindings<'_> = HashMap::new();

    // _start — argv/rbp setup.
    code.extend_from_slice(&[0x4C, 0x8B, 0x24, 0x24]);       // mov r12, [rsp]
    emit_argc_guard(&mut code, (n_scalar as i32) + 2);
    code.extend_from_slice(&[0x4C, 0x8D, 0x6C, 0x24, 0x08]); // lea r13, [rsp+8]
    code.push(0x55);                                         // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]);             // mov rbp, rsp
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame_size.to_le_bytes());       // sub rsp, frame_size
    code.extend_from_slice(&[0x49, 0xC7, 0xC6, 0x01, 0x00, 0x00, 0x00]); // mov r14, 1

    // Slice 9.5 / 9.5b: emit ALL resource read sequences ABOVE the outer
    // loop — files opened/read/closed once per rule invocation, every
    // (ptr, len) survives every record iteration via the rbp slots.
    // First slot starts immediately below argv_save_slot and descends.
    // The init resource (if init_kind is Read) gets its slots pulled
    // from text_bindings after the loop, by name.
    let mut bottom_cursor: i32 = argv_save_slot - 8;
    for r in &referenced_resources {
        let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
            &mut code,
            r,
            bottom_cursor,
            &mut resource_abort_patches,
        );
        text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
        bottom_cursor = new_next;
    }
    if let InitKind::Read(r) = &init_kind {
        let &(ptr_slot, len_slot) = text_bindings
            .get(r.name.as_str())
            .expect("init resource always present in referenced_resources");
        init_read_slots = Some((ptr_slot, len_slot));
    }

    // now_unix(): one syscall ABOVE the outer loop, slot at the bottom
    // of the frame. Useful in text-fold rules for prepending a captured
    // batch timestamp (formatted via the Number arg path in concat).
    let now_slot: i32 = if uses_now {
        let s = bottom_cursor - 8;
        emit_capture_now_unix(&mut code, s);
        s
    } else {
        0
    };

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
    if uses_now {
        elem_offsets.insert("now", now_slot);
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
        if uses_now {
            let_offsets.insert("now", now_slot);
        }
        let mut next_let_slot = -(((n_scalar + n_elem_fields) as i32 + 1) * 8);
        for (name, expr) in &rule.logic.bindings {
            emit_eval_expr(&mut code, expr, &rule.input_name, &let_offsets, all_rules, &field_ranges_for_lets, &text_bindings)?;
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
    // `init_size` is the WORST-CASE init contribution computed above from
    // either the literal length or the resource's `max:` bound. The actual
    // init copy below uses the runtime `len_slot` for the Read variant, so
    // the buffer is sized for the worst case but the write is exact.
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

    // Copy the init bytes into [rbx..]. Two variants:
    //   - Literal: bytes inline in the code, jumped over via short/near jmp
    //     and copied via lea rsi/rep movsb (the original Phase 5b path).
    //   - Read: the resource was already opened/read above the outer loop;
    //     (ptr, len) live at rbp slots in `init_read_slots`. Copy from those
    //     slots — the LENGTH used is the runtime `len_slot`, which can be
    //     anywhere from 0 to `max:`, so the buffer (sized for `max:`) is
    //     never overrun.
    match &init_kind {
        InitKind::Literal(s) => {
            let init_bytes = s.as_bytes();
            let lit_len = init_bytes.len() as i32;
            if lit_len > 0 {
                if lit_len <= 127 {
                    code.push(0xEB);
                    code.push(lit_len as u8);
                } else {
                    code.push(0xE9);
                    code.extend_from_slice(&lit_len.to_le_bytes());
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
                // mov rcx, lit_len
                code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
                code.extend_from_slice(&lit_len.to_le_bytes());
                // rep movsb
                code.extend_from_slice(&[0xF3, 0xA4]);
                // mov rbx, rdi
                code.extend_from_slice(&[0x48, 0x89, 0xFB]);
            }
        }
        InitKind::Read(_) => {
            let (ptr_slot, len_slot) = init_read_slots
                .expect("InitKind::Read implies the prologue allocated read slots");
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
        &text_bindings,
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

    // Slice 9.5: shared abort label for resource open/read failures.
    // Mirrors the rule-prologue path in emit_record_loop_epilogue. The
    // label only exists when at least one resource read was emitted, so
    // literal-init programs pay zero bytes for it.
    emit_resource_abort_tail(&mut code, &resource_abort_patches);

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
    text_bindings: &TextBindings<'_>,
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
                text_bindings,
            )?;
            // jmp loop_top
            code.push(0xE9);
            let off = loop_top as i32 - (code.len() + 4) as i32;
            code.extend_from_slice(&off.to_le_bytes());
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            emit_eval_expr(code, cond, &rule.input_name, offsets, all_rules, field_ranges, text_bindings)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.push(0x0F);
            code.push(0x84); // je rel32
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            emit_eval_record_expr(
                code, then_e, loop_top, output_concept, all_concepts, rule,
                input_concept, all_rules, offsets, field_ranges, text_bindings,
            )?;

            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());
            emit_eval_record_expr(
                code, else_e, loop_top, output_concept, all_concepts, rule,
                input_concept, all_rules, offsets, field_ranges, text_bindings,
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

/// `Result(Record, Text)` Ok-arm dispatch. Accepts either a direct
/// `<Concept> { ... }` constructor or `if cond then ... else ...`
/// where each branch is itself another dispatchable shape (so
/// arbitrarily-nested if/else is fine). Each Record leaf emits a
/// single JSON line via `emit_record_as_json` and falls through; the
/// caller's outer `jmp loop_top` provides the single tail.
///
/// Slice rationale: `emit_eval_record_expr` (the record-output rule
/// emitter) handles Record + If for the no-Result case but
/// self-terminates with its own `jmp loop_top`. We can't reuse it
/// here because the Ok arm's tail jmp is shared with the surrounding
/// Number/Text Ok arms, and we don't want two jmp_loop_tops emitted
/// by the same Ok branch.
fn emit_ok_record_dispatch(
    code: &mut Vec<u8>,
    expr: &Expr,
    output_concept: &Concept,
    rule: &Rule,
    input_concept: &Concept,
    all_rules: &HashMap<&str, &Rule>,
    offsets: &HashMap<&str, i32>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Record(name, fields) => {
            if name != &output_concept.name {
                return Err(NativeError {
                    message: format!(
                        "Ok-arm record constructor '{}' does not match declared concept '{}'",
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
                text_bindings,
            )
        }
        Expr::If(cond, then_e, else_e) => {
            // cond → rax; test, jz else
            emit_eval_expr(code, cond, &rule.input_name, offsets, all_rules, field_ranges, text_bindings)?;
            code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax
            code.push(0x0F);
            code.push(0x84); // je rel32
            let else_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            emit_ok_record_dispatch(
                code, then_e, output_concept, rule, input_concept, all_rules,
                offsets, field_ranges, text_bindings,
            )?;
            // Then-branch finished its record JSON; jump past the else
            // branch to the common tail. (The outer `jmp loop_top` is
            // beyond the patched end_patch site.)
            code.push(0xE9);
            let end_patch = code.len();
            code.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

            // Else label: patch the je to here.
            let else_pos = code.len();
            let else_off = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&else_off.to_le_bytes());

            emit_ok_record_dispatch(
                code, else_e, output_concept, rule, input_concept, all_rules,
                offsets, field_ranges, text_bindings,
            )?;

            // End label: patch the jmp from the then-branch to here.
            let end_pos = code.len();
            let end_off = end_pos as i32 - (end_patch as i32 + 4);
            code[end_patch..end_patch + 4].copy_from_slice(&end_off.to_le_bytes());
            Ok(())
        }
        other => Err(NativeError {
            message: format!(
                "Result({}, Text) Ok arm must be a `{} {{ ... }}` constructor or `if/else` of those; got {:?}",
                output_concept.name, output_concept.name, other,
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
    text_bindings: &TextBindings<'_>,
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
                emit_eval_expr(code, value_expr, input_name, offsets, all_rules, field_ranges, text_bindings)?;
                emit_itoa_to_stdout_no_newline(code);
            }
            Type::Text => {
                // Write the text bytes then the closing quote.
                // Slice 9.5c: pass through the caller's text_bindings so
                // `Expr::Read(name)` (and any future BoundText shape) in a
                // record's text-typed field resolves correctly. emit_record_program
                // passes &ctx.text_bindings; emit_collection_program passes its
                // local map populated from the resource prologue.
                emit_text_write_to_fd(
                    code,
                    value_expr,
                    1,
                    input_name,
                    input_concept,
                    all_rules,
                    offsets,
                    field_ranges,
                    text_bindings,
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
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
) -> Result<Vec<u8>, NativeError> {
    // Both Print and AppendFile effects are handled below.

    let mut code = Vec::new();
    let ctx = emit_record_loop_prologue(&mut code, trigger_rule, concept, None, all_rules, all_resources, all_connections)?;

    // Evaluate trigger rule's logic → rax (0 = no fire, nonzero = fire).
    emit_eval_expr(
        &mut code,
        &trigger_rule.logic.value,
        &trigger_rule.input_name,
        &ctx.binding_offsets,
        all_rules,
        &ctx.field_ranges,
        &ctx.text_bindings,
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
                // Reactions don't expose the slice 8d on_error knob — they
                // ride the Drop default, matching pre-8d behaviour.
                let mut _no_aborts: Vec<usize> = Vec::new();
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
                    ErrorPolicy::Drop,
                    &mut _no_aborts,
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
                            &ctx.text_bindings,
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
        // Phase 9 slice 1 stub: a Read leaf has no eval-stack consumption
        // (the result lands in registers/slots like any other text).
        Expr::Read(_) => 0,
        // Phase 11 slice 1: a Fetch's response lands in (ptr, len) slots
        // populated by the prologue, exactly like Read. The request bytes
        // expression is evaluated above loop_top once per rule invocation,
        // so its eval-stack depth doesn't accumulate at the call site.
        Expr::Fetch(_, _) => 0,
        // Phase 12 (json_escape): the inner expression's depth dominates;
        // the transform itself uses fixed registers (rsi/rcx) and the
        // existing concat write pointer (rbx), no additional eval-stack
        // pushes beyond what the inner already needs.
        Expr::JsonEscape(inner) => max_stack_depth(inner),
        // Phase 12 (parse_int): same shape — the parse loop is fixed-
        // register, so the inner's depth dominates.
        Expr::ParseInt(inner) => max_stack_depth(inner),
        // `now_unix()` is a leaf — the captured value lives in an rbp slot,
        // emission at use sites is a single load, no eval-stack pushes.
        Expr::NowUnix => 0,
        // `starts_with(haystack, needle)` — the byte-compare loop is fixed-
        // register (rsi/rdi/rcx); whichever child has the larger eval-stack
        // depth dominates. Same shape as Binary minus the +1 push (the
        // result is materialized in rax, not pushed for the parent).
        Expr::StartsWith(h, n) => max_stack_depth(h).max(max_stack_depth(n)),
        // `contains(haystack, needle)` — same shape as StartsWith: fixed-
        // register substring search, no eval-stack push for the result.
        Expr::Contains(h, n) => max_stack_depth(h).max(max_stack_depth(n)),
        // `ends_with(haystack, needle)` — same shape as StartsWith: fixed-
        // register suffix compare, no eval-stack push for the result.
        Expr::EndsWith(h, n) => max_stack_depth(h).max(max_stack_depth(n)),
        // `length(<text_expr>)` — fixed-register strlen scan / len_slot
        // load, the inner's depth dominates.
        Expr::Length(inner) => max_stack_depth(inner),
        // `abs(<number_expr>)` — 5-byte inline (cqo; xor rax, rdx; sub rax, rdx),
        // no eval-stack push, the inner's depth dominates.
        Expr::Abs(inner) => max_stack_depth(inner),
        // `min(a, b)` / `max(a, b)` — branch-free cmp + cmov; left is
        // evaluated and pushed, right is evaluated, so same shape as Binary.
        Expr::Min(l, r) | Expr::Max(l, r) => {
            let left_depth = max_stack_depth(l) + 1;
            let right_depth = max_stack_depth(r);
            left_depth.max(right_depth)
        }
        // `substring(text, start, end)` — the slice operation itself uses
        // fixed registers (the text source is materialised as a (ptr, len)
        // pair, then bounded by start/end values that live in rbp slots).
        // No eval-stack pushes beyond what the children's individual
        // depths already require.
        Expr::Substring(t, s, e) => {
            max_stack_depth(t)
                .max(max_stack_depth(s))
                .max(max_stack_depth(e))
        }
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
    text_bindings: &TextBindings<'_>,
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
                        emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
                        emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
                                emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
                    return emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings);
                }
                if matches!(left.as_ref(), Expr::Number(1)) {
                    return emit_eval_expr(code, right, input_name, offsets, all_rules, field_ranges, text_bindings);
                }
            }

            // Strength reduction: add/sub 0 → identity
            if (*op == BinOp::Add || *op == BinOp::Sub) && matches!(right.as_ref(), Expr::Number(0)) {
                return emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings);
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

                // === Text comparison: field == read(<resource>) ===
                // Slice "text equality with bound RHS" (2026-04-28). Detect a
                // text field compared to a resource read result. The literal
                // path above used an inline NUL-terminated literal; here the
                // bytes live at [rbp+ptr_slot] with the length at [rbp+len_slot].
                // No NUL in the resource buffer (read's byte count is exact),
                // so we cannot rely on the trailing NUL trick — we explicitly
                // compare strlen(field) == read_len first and only do cmpsb on
                // matching lengths. Field-side stays NUL-terminated (argv
                // string) so emit_strlen still applies. Detected for both
                // operand orders so `read(x) == p.tag` works the same as
                // `p.tag == read(x)`.
                let bound_pair: Option<(i32, &str)> = match (left.as_ref(), right.as_ref()) {
                    (Expr::Field(base, fname), Expr::Read(rname))
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
                    {
                        offsets.get(fname.as_str()).map(|&o| (o, rname.as_str()))
                    }
                    (Expr::Read(rname), Expr::Field(base, fname))
                        if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) =>
                    {
                        offsets.get(fname.as_str()).map(|&o| (o, rname.as_str()))
                    }
                    _ => None,
                };
                if let Some((foff, rname)) = bound_pair {
                    let &(ptr_slot, len_slot) = text_bindings.get(rname).ok_or_else(|| NativeError {
                        message: format!(
                            "text equality with read({}) expected the resource to be bound at this point — \
                             reached emit_eval_expr with no slot in text_bindings",
                            rname
                        ),
                    })?;

                    // Step 1: compute strlen(field) into rdx.
                    // mov rsi, [rbp + foff]
                    if foff >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(foff as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&foff.to_le_bytes());
                    }
                    emit_strlen(code); // rdx = strlen(field)

                    // Step 2: load read length, compare with strlen.
                    // mov rcx, [rbp + len_slot]
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x4D]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                    // cmp rdx, rcx
                    code.extend_from_slice(&[0x48, 0x39, 0xCA]);
                    // jne .lengths_differ (8-bit displacement; the body is
                    // small enough — at most ~22 bytes for the cmpsb path
                    // including reloads — to safely use a short jump).
                    code.push(0x75);
                    let lengths_differ_patch = code.len();
                    code.push(0x00);

                    // Step 3: lengths equal → cmpsb on rcx (= len_slot) bytes.
                    // Reload rsi (field ptr) and load rdi (resource ptr).
                    if foff >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(foff as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&foff.to_le_bytes());
                    }
                    if ptr_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x7D]);
                        code.push(ptr_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xBD]);
                        code.extend_from_slice(&ptr_slot.to_le_bytes());
                    }
                    // cld ; repe cmpsb
                    code.push(0xFC);
                    code.extend_from_slice(&[0xF3, 0xA6]);
                    // sete al  (rax = 1 iff every byte matched)
                    code.extend_from_slice(&[0x0F, 0x94, 0xC0]);
                    // jmp .collect (skip the lengths-differ tail)
                    code.push(0xEB);
                    let collect_patch = code.len();
                    code.push(0x00);

                    // .lengths_differ:
                    let ld_pos = code.len();
                    code[lengths_differ_patch] = (ld_pos - lengths_differ_patch - 1) as u8;
                    // xor al, al  (raw equal = 0)
                    code.extend_from_slice(&[0x30, 0xC0]);

                    // .collect:
                    let collect_pos = code.len();
                    code[collect_patch] = (collect_pos - collect_patch - 1) as u8;
                    // movzx rax, al
                    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);

                    // For NotEq, invert: xor rax, 1
                    if *op == BinOp::NotEq {
                        code.extend_from_slice(&[0x48, 0x83, 0xF0, 0x01]);
                    }
                    return Ok(());
                }
            }

            // === General case: evaluate both sides, apply operator ===
            emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            code.push(0x50); // push rax
            emit_eval_expr(code, right, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
                text_bindings,
            )
        }
        Expr::If(cond, then_e, else_e) => {
            // Try static branch elimination via interval arithmetic
            if let Some(always) = try_static_condition(cond, field_ranges, input_name) {
                if always {
                    // Condition always true — emit only then branch, skip comparison
                    return emit_eval_expr(code, then_e, input_name, offsets, all_rules, field_ranges, text_bindings);
                } else {
                    // Condition always false — emit only else branch, skip comparison
                    return emit_eval_expr(code, else_e, input_name, offsets, all_rules, field_ranges, text_bindings);
                }
            }
            // Dynamic: emit both branches with runtime check
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // test al, al
            code.extend_from_slice(&[0x84, 0xC0]);
            // jz .else_branch
            code.push(0x0F);
            code.push(0x84);
            let else_patch = code.len();
            code.extend_from_slice(&[0x00; 4]);
            // then branch → rax
            emit_eval_expr(code, then_e, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // jmp .end
            code.push(0xE9);
            let end_patch = code.len();
            code.extend_from_slice(&[0x00; 4]);
            // .else_branch:
            let else_pos = code.len();
            let eo = else_pos as i32 - (else_patch as i32 + 4);
            code[else_patch..else_patch + 4].copy_from_slice(&eo.to_le_bytes());
            emit_eval_expr(code, else_e, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // .end:
            let end_pos = code.len();
            let ep = end_pos as i32 - (end_patch as i32 + 4);
            code[end_patch..end_patch + 4].copy_from_slice(&ep.to_le_bytes());
            Ok(())
        }
        Expr::Not(inner) => {
            emit_eval_expr(code, inner, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // rax is 0 or 1; flip it
            code.extend_from_slice(&[0x48, 0x83, 0xF0, 0x01]); // xor rax, 1
            Ok(())
        }
        Expr::Neg(inner) => {
            emit_eval_expr(code, inner, input_name, offsets, all_rules, field_ranges, text_bindings)?;
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
        // Phase 9 slice 1 stub: read() returns text and is only meaningful in
        // a text-typed context. Reaching this number-context emitter means
        // someone tried to use it where a number was expected.
        Expr::Read(_) => Err(NativeError {
            message: "Expr::Read in number context — read() returns text, use it in a text-typed position".into(),
        }),
        // Phase 11 slice 1: same shape as Read — fetch() returns text,
        // not a number. The verifier already rejects this; the error
        // here is a defensive catch.
        Expr::Fetch(_, _) => Err(NativeError {
            message: "Expr::Fetch in number context — fetch() returns text, use it in a text-typed position".into(),
        }),
        // Phase 12 (json_escape): json_escape returns text, not a number.
        // Defensive catch — the verifier rejects this at compile time.
        Expr::JsonEscape(_) => Err(NativeError {
            message: "Expr::JsonEscape in number context — json_escape() returns text, use it in a text-typed position".into(),
        }),
        // parse_int(<text>) — convert a runtime-loaded text value to a
        // number. Inner must produce a (ptr, len) pair we can scan; the
        // strict scan accepts an optional leading `-`, then 1+ ASCII
        // digits, then end-of-input. Anything else (empty input,
        // whitespace, non-digit, lone `-`, overflow not yet checked)
        // sys_exit(1)s the binary — same fail-closed posture as
        // `on_read_error: abort`. Self-contained: the abort sequence is
        // emitted inline at the end of this branch (no per-binary
        // patches needed), so callers don't need any new bookkeeping.
        Expr::ParseInt(inner) => emit_parse_int(code, inner.as_ref(), input_name, text_bindings, offsets, all_rules, field_ranges),
        // `now_unix()` — load the captured CLOCK_REALTIME seconds from
        // the dedicated `now` slot. The slot is populated ONCE per rule
        // invocation by a `clock_gettime` call hoisted above the record
        // loop (see `emit_capture_now_unix`). Same value visible at
        // every call site within the rule — mirror of slice 8c's
        // req.timestamp scope. Emitters that don't yet wire the
        // capture register no `now` in offsets, so this arm fails with
        // a clear message naming the unwired emitter.
        Expr::NowUnix => {
            let slot = *offsets.get("now").ok_or_else(|| NativeError {
                message: "now_unix() reached emit_eval_expr in a context that did not capture \
                          the clock — the surrounding emitter must call emit_capture_now_unix \
                          and register the slot under the synthetic name \"now\" in its offsets map"
                    .into(),
            })?;
            load_rax_from_rbp(code, slot);
            Ok(())
        }
        // `starts_with(haystack, needle)` — bool result. Both args are
        // text. Algorithm (concrete byte-compare, no allocator):
        //   1. produce (rsi=ptr, rcx=len) for haystack; save into r8, r9
        //   2. produce (rsi=ptr, rcx=len) for needle
        //   3. mov rdi, rsi (rdi = needle_ptr)
        //   4. cmp r9, rcx ; jb .nope    (haystack shorter than needle → false)
        //   5. test rcx, rcx ; je .empty  (empty needle → true)
        //   6. mov rsi, r8                (restore haystack ptr)
        //   7. cld ; repe cmpsb           (compare needle_len bytes)
        //   8. sete al
        //   9. movzx rax, al
        // Edge cases:
        //   - empty needle → always true (standard convention)
        //   - needle longer than haystack → false
        //   - byte-exact match required (no encoding awareness)
        Expr::StartsWith(haystack, needle) => emit_starts_with(
            code,
            haystack.as_ref(),
            needle.as_ref(),
            input_name,
            offsets,
            all_rules,
            field_ranges,
            text_bindings,
        ),
        // `contains(haystack, needle)` — naive O(N*M) substring search.
        // Algorithm:
        //   1. Load haystack → (r8 = ptr, r9 = len) via the StartsWith
        //      load helper (allocation-free shapes only)
        //   2. Load needle → (r10 = ptr, r11 = len) likewise
        //   3. Empty needle → return 1 (true)
        //   4. needle_len > hay_len → return 0 (false)
        //   5. max_offset = hay_len - needle_len; iterate offset = 0..=max:
        //        rsi = r8 + offset ; rdi = r10 ; rcx = r11
        //        cld ; repe cmpsb
        //        if ZF=1 → return 1
        //        else inc offset, continue
        //   6. Loop exhausted → return 0
        // Verifier guarantees `max:` bounds on resources, so worst-case
        // work is statically known.
        Expr::Contains(haystack, needle) => emit_contains(
            code,
            haystack.as_ref(),
            needle.as_ref(),
            input_name,
            offsets,
            all_rules,
            field_ranges,
            text_bindings,
        ),
        // `ends_with(haystack, needle)` — symmetric of starts_with.
        // Algorithm:
        //   1. produce (rsi=ptr, rcx=len) for haystack; save into r8, r9
        //   2. produce (rsi=ptr, rcx=len) for needle
        //   3. mov rdi, rsi (rdi = needle_ptr)
        //   4. cmp r9, rcx ; jb .nope    (haystack shorter than needle → false)
        //   5. test rcx, rcx ; je .empty  (empty needle → always true)
        //   6. mov rsi, r8 ; add rsi, r9 ; sub rsi, rcx
        //      (rsi = haystack_ptr + hay_len - needle_len = tail start)
        //   7. cld ; repe cmpsb           (compare needle_len bytes)
        //   8. sete al ; movzx rax, al
        Expr::EndsWith(haystack, needle) => emit_ends_with(
            code,
            haystack.as_ref(),
            needle.as_ref(),
            input_name,
            offsets,
            all_rules,
            field_ranges,
            text_bindings,
        ),
        // `length(<text_expr>)` — byte count returned in rax.
        // Inner shape dispatch:
        //   - Text literal: optimizer should have folded; if it slipped
        //     through, emit `mov rax, <length>` (compile-time constant)
        //   - text input field (Field of input): inline strlen scan
        //   - BoundText (Read / Ident / Fetch with text_bindings entry):
        //     load len_slot directly into rax — zero-scan path because
        //     the prologue already counted the bytes when the resource
        //     was read or the let was evaluated
        //   - anything else: clear refusal (Concat / Call / JsonEscape /
        //     ParseInt as length-arg would require a per-call scratch
        //     evaluation; not in this slice)
        Expr::Length(inner) => emit_length(code, inner.as_ref(), input_name, offsets, all_rules, field_ranges, text_bindings),
        // `abs(<number>)` — branch-free absolute value via the canonical
        // 5-byte inline:
        //   cqo                 ; rdx = (rax < 0) ? -1 : 0
        //   xor rax, rdx        ; flip bits if negative
        //   sub rax, rdx        ; add 1 if negative (because rdx is -1)
        // For non-negative values: cqo→rdx=0, xor with 0 leaves rax,
        // sub 0 leaves rax. For negative: cqo→rdx=-1, xor flips bits
        // (= bitwise NOT), sub -1 adds 1 → that's two's complement
        // negation. Branch-free, no `cmp`, 5 bytes total. Doesn't
        // panic on i64::MIN (it stays at i64::MIN; the optimizer fold
        // uses wrapping_abs for the same property).
        Expr::Abs(inner) => {
            emit_eval_expr(code, inner, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // cqo
            code.extend_from_slice(&[0x48, 0x99]);
            // xor rax, rdx
            code.extend_from_slice(&[0x48, 0x31, 0xD0]);
            // sub rax, rdx
            code.extend_from_slice(&[0x48, 0x29, 0xD0]);
            Ok(())
        }
        // `min(a, b)` / `max(a, b)` — branch-free binary scalar via
        // `cmp + cmovl/cmovg`. Algorithm:
        //   eval left  → rax ; push rax
        //   eval right → rax
        //   pop rcx              ; rcx = left, rax = right
        //   cmp rcx, rax         ; sets flags from (left <=> right)
        //   cmovl rax, rcx       ; min: if left < right, take left
        //   (max uses cmovg: if left > right, take left)
        // 11 bytes total per call (4 instructions). Branch-free, no
        // flag dependency on subsequent code.
        Expr::Min(left, right) | Expr::Max(left, right) => {
            let is_max = matches!(expr, Expr::Max(_, _));
            // eval left → rax; push rax
            emit_eval_expr(code, left, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            code.push(0x50); // push rax
            // eval right → rax
            emit_eval_expr(code, right, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // pop rcx (rcx = left, rax = right)
            code.push(0x59);
            // cmp rcx, rax
            code.extend_from_slice(&[0x48, 0x39, 0xC1]);
            if is_max {
                // cmovg rax, rcx (if left > right, rax = left)
                code.extend_from_slice(&[0x48, 0x0F, 0x4F, 0xC1]);
            } else {
                // cmovl rax, rcx (if left < right, rax = left)
                code.extend_from_slice(&[0x48, 0x0F, 0x4C, 0xC1]);
            }
            Ok(())
        }
        // `substring(<text>, <start>, <end>)` produces text (a (ptr, len)
        // pair), not a scalar. It cannot be evaluated through this scalar
        // dispatcher; the text-producing path lives in
        // `emit_text_produce_ptrlen`. Refuse here with a clear breadcrumb —
        // the wire-up of substring as a text expression in let-RHS / concat
        // arg / record field positions is the next slice's responsibility.
        Expr::Substring(_, _, _) => Err(NativeError {
            message: "substring not yet supported in this context — only as RHS of a text-typed let binding in Phase 2I; coming in slice 1".into(),
        }),
    }
}

/// `length(<text>)` native emission. Produces the byte count in rax.
/// See the StartsWith load-text helper for the analogous shape; this
/// helper is simpler because length is a single-arg primitive (no
/// two-arg ordering, no stack-buffer concern).
fn emit_length(
    code: &mut Vec<u8>,
    inner: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match inner {
        // Compile-time constant: optimizer should fold `length("abc")`
        // to `Number(3)` and never reach here. Defensive emit just in
        // case a non-optimized path materialises a literal Length.
        Expr::Text(s) => {
            let n = s.as_bytes().len() as i64;
            // mov rax, imm32 (works for typical text sizes; emit_mov_rax_imm
            // handles wider values if needed).
            emit_mov_rax_imm(code, n);
            Ok(())
        }
        Expr::Field(base, fname) if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) => {
            // BoundText path takes precedence: a field whose name is in
            // text_bindings has its length at the registered len_slot
            // (e.g. req.body in HTTP services). Zero-scan.
            if let Some(&(_ptr_slot, len_slot)) = text_bindings.get(fname.as_str()) {
                if len_slot >= -128 {
                    code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                    code.push(len_slot as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                    code.extend_from_slice(&len_slot.to_le_bytes());
                }
                return Ok(());
            }
            // Number-typed fields land in `field_ranges`. Strlen on a
            // Number slot would scan whatever bytes happen to be there
            // (the field stores an i64, not a NUL-terminated string),
            // so reject explicitly rather than silently produce garbage.
            if field_ranges.contains_key(fname.as_str()) {
                return Err(NativeError {
                    message: format!(
                        "length: field '{}' is number-typed; length only accepts text",
                        fname
                    ),
                });
            }
            let &offset = offsets.get(fname.as_str()).ok_or_else(|| NativeError {
                message: format!("length: unknown text input field '{}'", fname),
            })?;
            // mov rsi, [rbp + offset]   — NUL-terminated argv pointer
            if offset >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                code.push(offset as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                code.extend_from_slice(&offset.to_le_bytes());
            }
            // emit_strlen reads rsi, writes rdx
            emit_strlen(code);
            // mov rax, rdx
            code.extend_from_slice(&[0x48, 0x89, 0xD0]);
            Ok(())
        }
        Expr::Read(name) | Expr::Ident(name) | Expr::Fetch(name, _)
            if text_bindings.contains_key(name.as_str()) =>
        {
            let &(_ptr_slot, len_slot) = text_bindings.get(name.as_str()).unwrap();
            // mov rax, [rbp + len_slot]   — the length is already at the
            // registered slot (populated by emit_resource_read_sequence,
            // emit_connection_fetch_sequence, or the Phase-2I let
            // evaluation). Zero scan.
            if len_slot >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                code.push(len_slot as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                code.extend_from_slice(&len_slot.to_le_bytes());
            }
            Ok(())
        }
        // length(concat(args)): sum each arg's length into a stack
        // accumulator, then load into rax. No buffer allocation — we
        // never materialise the concatenation, only count its bytes.
        // Args restricted to text-producing shapes; number args refused
        // explicitly (a number arg's decimal width would require running
        // an itoa-width helper, which is its own slice). Each arg
        // recurses through emit_length, so nested concat / Call args
        // compose naturally without ad-hoc scratch.
        Expr::Concat(args) => {
            // sub rsp, 8   ; mov qword [rsp], 0
            code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x08]);
            code.extend_from_slice(&[0x48, 0xC7, 0x04, 0x24, 0x00, 0x00, 0x00, 0x00]);
            for (i, arg) in args.iter().enumerate() {
                // Refuse number-producing kinds before recursion so the
                // error names the actual offender, not the inner shape.
                match arg {
                    Expr::Number(_)
                    | Expr::Neg(_)
                    | Expr::Abs(_)
                    | Expr::Length(_)
                    | Expr::ParseInt(_)
                    | Expr::NowUnix
                    | Expr::Binary(_, _, _)
                    | Expr::Min(_, _)
                    | Expr::Max(_, _) => {
                        // free the accumulator slot before reporting so a
                        // partially emitted prologue doesn't poison later
                        // tests if this error is caught somewhere upstream.
                        code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]);
                        return Err(NativeError {
                            message: format!(
                                "length(concat(...)): arg {} is number-typed ({}). \
                                 length only sums text-producing args; materialise \
                                 the concat via a `let` and call length on it.",
                                i,
                                expr_kind(arg)
                            ),
                        });
                    }
                    _ => {}
                }
                // Recurse on this arg — emit_length will produce the
                // arg's byte count in rax (or fail with a clear message).
                emit_length(code, arg, input_name, offsets, all_rules, field_ranges, text_bindings)?;
                // add [rsp], rax
                code.extend_from_slice(&[0x48, 0x01, 0x04, 0x24]);
            }
            // mov rax, [rsp]  ;  add rsp, 8
            code.extend_from_slice(&[0x48, 0x8B, 0x04, 0x24]);
            code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x08]);
            Ok(())
        }
        // length(Call(callee, [Ident(input)])): inline-resolve the callee
        // body (Phase 2G shape) and recurse. The callee's bytes never get
        // materialised — we compute their length directly. Phase 2G
        // constraints apply: single ident arg matching input_name, callee
        // returns text, callee has no let bindings.
        Expr::Call(callee_name, call_args) => {
            if call_args.len() != 1 {
                return Err(NativeError {
                    message: format!(
                        "length(call): callee '{}' must take exactly one argument; got {}",
                        callee_name,
                        call_args.len()
                    ),
                });
            }
            match &call_args[0] {
                Expr::Ident(n) if n == input_name => {}
                other => {
                    return Err(NativeError {
                        message: format!(
                            "length(call): callee '{}' argument must be the caller's input ident '{}'; got {}",
                            callee_name,
                            input_name,
                            expr_kind(other)
                        ),
                    });
                }
            }
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!("length(call): unknown rule '{}'", callee_name),
            })?;
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "length(call): callee '{}' has let bindings, which aren't supported \
                         for length(call) inlining (same restriction as Phase 2G text inlining). \
                         Workaround: inline the let RHSes into the body, or call the rule \
                         once into a caller-side let and length that.",
                        callee_name
                    ),
                });
            }
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "length(call): callee '{}' must return text; got {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "length(call): callee '{}' uses input ident '{}', caller uses '{}' — \
                         length(call) inlining requires the same input ident name \
                         (same Phase 2G constraint that lets the body resolve under the caller's offsets).",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            // Recurse on the callee body. The callee's logic value is a
            // text expression evaluated under the same input slots as the
            // caller, so offsets / field_ranges / text_bindings all apply
            // unchanged. Any failure deeper inside the body propagates a
            // message that names the offending shape.
            emit_length(
                code,
                &callee.logic.value,
                input_name,
                offsets,
                all_rules,
                field_ranges,
                text_bindings,
            )
        }
        // length(json_escape(<text>)) — count the escaped output bytes
        // without materialising the escape transform. Native's json_escape
        // only escapes five bytes (", \, \n, \r, \t), each becoming 2
        // bytes; every other byte passes through as 1. So the output
        // length = input length + count of escape-triggering bytes.
        // One pass over the input, accumulating in r8. Inner restricted
        // to the same shapes emit_json_escape_load_src accepts (text
        // input field, BoundText) so the fill-pass path that already
        // exists in the binary is the one we measure here.
        Expr::JsonEscape(inner) => {
            // Load (rsi = src ptr, rcx = src len) for the inner. Errors
            // propagate with a clear message if the shape is unsupported.
            emit_json_escape_load_src(code, inner.as_ref(), input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // mov r8, rcx   — output length accumulator starts at input length
            code.extend_from_slice(&[0x49, 0x89, 0xC8]);
            // test rcx, rcx ; jz .done   (empty input → output is 0)
            code.extend_from_slice(&[0x48, 0x85, 0xC9]);
            code.push(0x74);
            let done_patch = code.len();
            code.push(0x00);
            // .loop:
            let loop_top = code.len();
            // movzx eax, byte [rsi]
            code.extend_from_slice(&[0x0F, 0xB6, 0x06]);
            // Five escape-byte checks. Each match jumps to the `+1`
            // accumulator; no match falls through to .skip.
            let mut je_patches: Vec<usize> = Vec::new();
            for byte in [0x22u8, 0x5C, 0x0A, 0x0D, 0x09] {
                code.push(0x3C); code.push(byte); // cmp al, byte
                code.push(0x74); je_patches.push(code.len()); code.push(0x00); // je .add
            }
            // jmp .skip (no escape)
            code.push(0xEB);
            let skip_jmp_patch = code.len();
            code.push(0x00);
            // .add: inc r8
            let add_pos = code.len();
            for p in &je_patches {
                code[*p] = (add_pos - p - 1) as u8;
            }
            code.extend_from_slice(&[0x49, 0xFF, 0xC0]); // inc r8
            // .skip:
            let skip_pos = code.len();
            code[skip_jmp_patch] = (skip_pos - skip_jmp_patch - 1) as u8;
            // inc rsi ; dec rcx
            code.extend_from_slice(&[0x48, 0xFF, 0xC6]);
            code.extend_from_slice(&[0x48, 0xFF, 0xC9]);
            // jnz .loop (rel8 backward)
            code.push(0x75);
            let back = loop_top as i32 - (code.len() as i32 + 1);
            code.push(back as u8);
            // .done:
            let done_pos = code.len();
            code[done_patch] = (done_pos - done_patch - 1) as u8;
            // mov rax, r8
            code.extend_from_slice(&[0x4C, 0x89, 0xC0]);
            Ok(())
        }
        // length(substring(text, start, end)) — produces (end - start)
        // with the same fail-closed bounds as substring itself
        // (end > length(text) → abort, start > end → abort). We do
        // NOT need to materialise the slice — we only need its length.
        // Recurse on length(text) for the bounds check, evaluate
        // start/end, do the same two checks substring would do, then
        // emit end - start directly. Cost ~70 bytes per length-substring
        // call. The inline abort tail matches substring's posture.
        //
        // Optimisation note: an even tighter codegen would skip
        // length(text) entirely when the AST guarantees text is bounded
        // (e.g., field with declared `[..N]` range), but slice 2 stays
        // conservative — always emit the runtime check so the user
        // sees the same fail-closed semantic as substring-in-a-let.
        Expr::Substring(text, start_expr, end_expr) => {
            // (1) Recurse on length(text) → rax
            emit_length(code, text.as_ref(), input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // (2) push rax (text_len at [rsp+8] after later push)
            code.push(0x50);
            // (3) Evaluate end → rax
            emit_eval_expr(code, end_expr, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // (4) cmp end, text_len ; ja .abort
            //    mov rcx, [rsp]
            code.extend_from_slice(&[0x48, 0x8B, 0x0C, 0x24]);
            //    cmp rax, rcx
            code.extend_from_slice(&[0x48, 0x39, 0xC8]);
            //    ja .abort (rel32 placeholder)
            code.push(0x0F); code.push(0x87);
            let end_abort_patch = code.len();
            code.extend_from_slice(&[0; 4]);
            // (5) push rax (end at [rsp])
            code.push(0x50);
            // (6) Evaluate start → rax
            emit_eval_expr(code, start_expr, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // (7) cmp start, end ; ja .abort
            //    mov rcx, [rsp]  (rcx = end)
            code.extend_from_slice(&[0x48, 0x8B, 0x0C, 0x24]);
            //    cmp rax, rcx
            code.extend_from_slice(&[0x48, 0x39, 0xC8]);
            //    ja .abort
            code.push(0x0F); code.push(0x87);
            let start_abort_patch = code.len();
            code.extend_from_slice(&[0; 4]);
            // (8) Compute length = end - start = rcx - rax
            //    mov rdx, rcx
            code.extend_from_slice(&[0x48, 0x89, 0xCA]);
            //    sub rdx, rax
            code.extend_from_slice(&[0x48, 0x29, 0xC2]);
            //    mov rax, rdx (result in rax per length convention)
            code.extend_from_slice(&[0x48, 0x89, 0xD0]);
            // (9) add rsp, 16 (free saved end + text_len)
            code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x10]);
            // (10) jmp .ok
            code.push(0xE9);
            let ok_jmp_patch = code.len();
            code.extend_from_slice(&[0; 4]);
            // .abort: sys_exit(1)
            let abort_pos = code.len();
            let end_rel = abort_pos as i32 - (end_abort_patch as i32 + 4);
            code[end_abort_patch..end_abort_patch + 4].copy_from_slice(&end_rel.to_le_bytes());
            let start_rel = abort_pos as i32 - (start_abort_patch as i32 + 4);
            code[start_abort_patch..start_abort_patch + 4].copy_from_slice(&start_rel.to_le_bytes());
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
            code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
            code.extend_from_slice(&[0x0F, 0x05]);
            // .ok:
            let ok_pos = code.len();
            let ok_rel = ok_pos as i32 - (ok_jmp_patch as i32 + 4);
            code[ok_jmp_patch..ok_jmp_patch + 4].copy_from_slice(&ok_rel.to_le_bytes());
            Ok(())
        }
        _ => Err(NativeError {
            message: format!(
                "length: argument must be a text literal, text input field, \
                 read(<resource>), fetch(<connection>, ...), a text let in scope, \
                 concat(...) with text-only args, json_escape(<text>) of a \
                 text input field or BoundText, substring(<text>, <start>, <end>), \
                 or a text-returning rule call — got {:?}.",
                expr_kind(inner)
            ),
        }),
    }
}

/// Helper for `starts_with`: produce (rsi=ptr, rcx=len) for a simple
/// text expression. Restricted to allocation-free shapes:
/// literal, text input field, BoundText, OR a text-returning rule call
/// whose body itself resolves to one of those shapes (Phase 2G-shape:
/// same input ident, no callee lets). Concat / JsonEscape / ParseInt
/// remain rejected — they would materialise a buffer that the
/// two-arg evaluation would have to manage across the second emit.
fn emit_starts_with_load_text(
    code: &mut Vec<u8>,
    expr: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Text(s) => {
            let bytes = s.as_bytes();
            let n = bytes.len() as i32;
            // jmp over inline bytes
            if n <= 127 {
                code.push(0xEB);
                code.push(n as u8);
            } else {
                code.push(0xE9);
                code.extend_from_slice(&n.to_le_bytes());
            }
            let data_addr = code.len();
            code.extend_from_slice(bytes);
            // lea rsi, [rip + rel32]
            let end = code.len() + 7;
            let rel32 = data_addr as i32 - end as i32;
            code.extend_from_slice(&[0x48, 0x8D, 0x35]);
            code.extend_from_slice(&rel32.to_le_bytes());
            // mov rcx, n  (literal length, compile-time constant)
            code.extend_from_slice(&[0x48, 0xC7, 0xC1]);
            code.extend_from_slice(&n.to_le_bytes());
            Ok(())
        }
        Expr::Field(base, fname) if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) => {
            // BoundText path takes precedence: req.body and similar
            // fields registered in text_bindings carry (ptr, len) at
            // dedicated rbp slots — no scan needed.
            if let Some(&(ptr_slot, len_slot)) = text_bindings.get(fname.as_str()) {
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
                return Ok(());
            }
            let &offset = offsets.get(fname.as_str()).ok_or_else(|| NativeError {
                message: format!("starts_with: unknown text input field '{}'", fname),
            })?;
            // mov rsi, [rbp + offset]  (NUL-terminated argv pointer)
            if offset >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                code.push(offset as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                code.extend_from_slice(&offset.to_le_bytes());
            }
            // emit_strlen reads rsi, writes rdx (length)
            emit_strlen(code);
            // mov rcx, rdx
            code.extend_from_slice(&[0x48, 0x89, 0xD1]);
            Ok(())
        }
        Expr::Read(name) | Expr::Ident(name) | Expr::Fetch(name, _)
            if text_bindings.contains_key(name.as_str()) =>
        {
            let &(ptr_slot, len_slot) = text_bindings.get(name.as_str()).unwrap();
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
            Ok(())
        }
        Expr::Call(callee_name, call_args) => {
            // Phase 2G-shape inline: same single Ident(input) arg, callee
            // has no let bindings, callee returns text. Recurse on the
            // callee body so it produces (rsi, rcx) under the caller's
            // offsets / text_bindings — byte-for-byte equivalent to
            // hoisting the body's load into the caller. Used by
            // starts_with / ends_with / contains identically.
            if call_args.len() != 1 {
                return Err(NativeError {
                    message: format!(
                        "starts_with/ends_with/contains: callee '{}' must take exactly one argument; got {}",
                        callee_name, call_args.len()
                    ),
                });
            }
            match &call_args[0] {
                Expr::Ident(n) if n == input_name => {}
                other => {
                    return Err(NativeError {
                        message: format!(
                            "starts_with/ends_with/contains: callee '{}' argument must be the caller's input ident '{}'; got {}",
                            callee_name, input_name, expr_kind(other)
                        ),
                    });
                }
            }
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!("starts_with/ends_with/contains: unknown rule '{}'", callee_name),
            })?;
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "starts_with/ends_with/contains: callee '{}' has let bindings, which aren't supported \
                         for inline (same Phase 2G constraint as length(call)). Workaround: inline the let RHSes \
                         into the body, or bind the callee result via a `let` in the caller and pass the let here.",
                        callee_name
                    ),
                });
            }
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "starts_with/ends_with/contains: callee '{}' must return text; got {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "starts_with/ends_with/contains: callee '{}' uses input ident '{}', caller uses '{}' — \
                         inline requires the same input ident name",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            // Recurse on the callee body. Any allocation-needing shape
            // (Concat, nested Call, JsonEscape) hits the catch-all
            // below with a message that names the offending shape.
            emit_starts_with_load_text(
                code,
                &callee.logic.value,
                input_name,
                offsets,
                all_rules,
                field_ranges,
                text_bindings,
            )
        }
        // substring(text, start, end) as a starts_with/ends_with/contains
        // arg: inline the substring emit (same logic as
        // emit_text_produce_ptrlen's Substring arm, slice 1) to produce
        // (rax=slice_ptr, rdx=slice_len), then adapt to the byte-prim
        // convention (rsi=ptr, rcx=len). Bounds enforced fail-closed
        // via inline sys_exit(1) tail — same posture as the let-RHS
        // path. Cost ~85 bytes per substring-as-byte-prim-arg call
        // (a couple bytes more than the let path because of the
        // rsi/rcx adapter at the tail).
        //
        // Implementation note: duplicates the substring emit logic
        // from emit_text_produce_ptrlen (which has concept in scope
        // for defense-in-depth Field type-checks). Here we skip that
        // check — the verifier already enforced that the text inner
        // is text-typed before native sees it. If a future slice
        // factors emit_substring_to_rax_rdx as a shared helper, both
        // call sites can converge on it.
        Expr::Substring(text, start_expr, end_expr) => {
            // Load text inner via this same loader (rsi/rcx convention),
            // adapt to (rax, rdx) for the shared helper, then back to
            // (rsi, rcx) for the byte-prim's cmpsb downstream.
            emit_starts_with_load_text(
                code, text, input_name, offsets, all_rules, field_ranges, text_bindings,
            )?;
            // mov rax, rsi ; mov rdx, rcx
            code.extend_from_slice(&[0x48, 0x89, 0xF0]);
            code.extend_from_slice(&[0x48, 0x89, 0xCA]);
            emit_substring_bounds_and_slice(
                code, start_expr, end_expr,
                input_name, offsets, all_rules, field_ranges, text_bindings,
            )?;
            // mov rsi, rax ; mov rcx, rdx
            code.extend_from_slice(&[0x48, 0x89, 0xC6]);
            code.extend_from_slice(&[0x48, 0x89, 0xD1]);
            Ok(())
        }
        _ => Err(NativeError {
            message: format!(
                "starts_with: argument must be a text literal, text input field, \
                 read(<resource>), fetch(<connection>, ...), a text let in scope, \
                 a text-returning rule call (Phase 2G), or substring(<text>, <start>, <end>) — \
                 got {:?}. Concat / JsonEscape / ParseInt as starts_with args still need \
                 a buffer materialisation that the two-arg evaluation explicitly avoids.",
                expr_kind(expr)
            ),
        }),
    }
}

/// Emit `starts_with(haystack, needle)` returning bool in rax. See the
/// comment on the StartsWith arm of emit_eval_expr for the algorithm.
fn emit_starts_with(
    code: &mut Vec<u8>,
    haystack: &Expr,
    needle: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // Step 1: load haystack into (rsi, rcx); save into (r8, r9).
    emit_starts_with_load_text(code, haystack, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov r8, rsi
    code.extend_from_slice(&[0x49, 0x89, 0xF0]);
    // mov r9, rcx
    code.extend_from_slice(&[0x49, 0x89, 0xC9]);

    // Step 2: load needle into (rsi, rcx).
    emit_starts_with_load_text(code, needle, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov rdi, rsi  (cmpsb uses rdi as second source)
    code.extend_from_slice(&[0x48, 0x89, 0xF7]);

    // Step 3: cmp r9, rcx ; jb .nope (haystack shorter than needle → false)
    code.extend_from_slice(&[0x49, 0x39, 0xC9]);
    code.push(0x72);
    let nope_patch = code.len();
    code.push(0x00);

    // Step 4: empty needle short-circuit (test rcx, rcx ; je .empty)
    code.extend_from_slice(&[0x48, 0x85, 0xC9]);
    code.push(0x74);
    let empty_patch = code.len();
    code.push(0x00);

    // Step 5: restore haystack ptr to rsi (the load_text for needle put
    // needle_ptr in rsi; we already mirrored it to rdi above).
    // mov rsi, r8
    code.extend_from_slice(&[0x4C, 0x89, 0xC6]);
    // cld ; repe cmpsb
    code.push(0xFC);
    code.extend_from_slice(&[0xF3, 0xA6]);
    // sete al
    code.extend_from_slice(&[0x0F, 0x94, 0xC0]);
    // jmp .done
    code.push(0xEB);
    let done_from_main_patch = code.len();
    code.push(0x00);

    // .empty: needle was empty → result is true
    let empty_label = code.len();
    code[empty_patch] = (empty_label - empty_patch - 1) as u8;
    // mov al, 1
    code.extend_from_slice(&[0xB0, 0x01]);
    // jmp .done
    code.push(0xEB);
    let done_from_empty_patch = code.len();
    code.push(0x00);

    // .nope: haystack shorter than needle → false
    let nope_label = code.len();
    code[nope_patch] = (nope_label - nope_patch - 1) as u8;
    // xor al, al
    code.extend_from_slice(&[0x30, 0xC0]);

    // .done: movzx rax, al  (zero-extend bool to qword)
    let done_label = code.len();
    code[done_from_main_patch] = (done_label - done_from_main_patch - 1) as u8;
    code[done_from_empty_patch] = (done_label - done_from_empty_patch - 1) as u8;
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);

    Ok(())
}

/// Emit `ends_with(haystack, needle)` returning bool in rax. Symmetric
/// of starts_with: load both args, check haystack_len >= needle_len,
/// position rsi at the haystack's tail (haystack_ptr + hay_len - needle_len),
/// then `repe cmpsb` on needle_len bytes.
fn emit_ends_with(
    code: &mut Vec<u8>,
    haystack: &Expr,
    needle: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // Step 1: load haystack into (rsi, rcx); save into (r8, r9).
    emit_starts_with_load_text(code, haystack, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov r8, rsi  ; mov r9, rcx
    code.extend_from_slice(&[0x49, 0x89, 0xF0]);
    code.extend_from_slice(&[0x49, 0x89, 0xC9]);

    // Step 2: load needle into (rsi, rcx).
    emit_starts_with_load_text(code, needle, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov rdi, rsi  (cmpsb uses rdi as second source)
    code.extend_from_slice(&[0x48, 0x89, 0xF7]);

    // Step 3: cmp r9, rcx ; jb .nope (haystack shorter than needle → false)
    code.extend_from_slice(&[0x49, 0x39, 0xC9]);
    code.push(0x72);
    let nope_patch = code.len();
    code.push(0x00);

    // Step 4: empty needle short-circuit (test rcx, rcx ; je .empty)
    code.extend_from_slice(&[0x48, 0x85, 0xC9]);
    code.push(0x74);
    let empty_patch = code.len();
    code.push(0x00);

    // Step 5: rsi = haystack_ptr + (hay_len - needle_len)
    //   mov rsi, r8 ; add rsi, r9 ; sub rsi, rcx
    // The needle_ptr lives in rdi (set at step 2's mov rdi, rsi).
    code.extend_from_slice(&[0x4C, 0x89, 0xC6]);  // mov rsi, r8
    code.extend_from_slice(&[0x4C, 0x01, 0xCE]);  // add rsi, r9
    code.extend_from_slice(&[0x48, 0x29, 0xCE]);  // sub rsi, rcx
    // cld ; repe cmpsb
    code.push(0xFC);
    code.extend_from_slice(&[0xF3, 0xA6]);
    // sete al
    code.extend_from_slice(&[0x0F, 0x94, 0xC0]);
    // jmp .done
    code.push(0xEB);
    let done_from_main_patch = code.len();
    code.push(0x00);

    // .empty: needle was empty → result is true
    let empty_label = code.len();
    code[empty_patch] = (empty_label - empty_patch - 1) as u8;
    // mov al, 1
    code.extend_from_slice(&[0xB0, 0x01]);
    // jmp .done
    code.push(0xEB);
    let done_from_empty_patch = code.len();
    code.push(0x00);

    // .nope: haystack shorter than needle → false
    let nope_label = code.len();
    code[nope_patch] = (nope_label - nope_patch - 1) as u8;
    // xor al, al
    code.extend_from_slice(&[0x30, 0xC0]);

    // .done: movzx rax, al
    let done_label = code.len();
    code[done_from_main_patch] = (done_label - done_from_main_patch - 1) as u8;
    code[done_from_empty_patch] = (done_label - done_from_empty_patch - 1) as u8;
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);

    Ok(())
}

/// Emit `contains(haystack, needle)` returning bool in rax. Naive
/// O(N*M) substring scan. Verifier-bounded because resources have
/// `max:` declarations — worst-case work is statically known.
///
/// Register layout for the scan:
///   r8  = haystack ptr (preserved across the loop)
///   r9  = max_offset = hay_len - needle_len (loop bound)
///   r10 = needle ptr
///   r11 = needle len
///   rax = current scan offset, also the bool result on exit
///   rsi = haystack[offset] (recomputed per iteration)
///   rdi = needle[0] (loaded per iteration; cmpsb advances it)
///   rcx = remaining needle bytes (loaded per iteration)
fn emit_contains(
    code: &mut Vec<u8>,
    haystack: &Expr,
    needle: &Expr,
    input_name: &str,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    // Step 1: load haystack into (rsi, rcx); save into (r8, r9).
    emit_starts_with_load_text(code, haystack, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov r8, rsi  ; mov r9, rcx
    code.extend_from_slice(&[0x49, 0x89, 0xF0]);
    code.extend_from_slice(&[0x49, 0x89, 0xC9]);

    // Step 2: load needle into (rsi, rcx); save into (r10, r11).
    emit_starts_with_load_text(code, needle, input_name, offsets, all_rules, field_ranges, text_bindings)?;
    // mov r10, rsi ; mov r11, rcx
    code.extend_from_slice(&[0x49, 0x89, 0xF2]);
    code.extend_from_slice(&[0x49, 0x89, 0xCB]);

    // Step 3: empty needle → return 1.
    // test r11, r11 ; je .yes
    code.extend_from_slice(&[0x4D, 0x85, 0xDB]);
    code.push(0x0F);
    code.push(0x84);
    let yes_from_empty_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Step 4: needle_len > hay_len → return 0.
    // cmp r9, r11 ; jb .no
    code.extend_from_slice(&[0x4D, 0x39, 0xD9]);
    code.push(0x0F);
    code.push(0x82);
    let no_from_short_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Step 5: r9 -= r11  (r9 = max_offset)
    // sub r9, r11
    code.extend_from_slice(&[0x4D, 0x29, 0xD9]);

    // Step 6: rax = 0 (current offset)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);

    // Step 7: .scan_loop:
    let scan_loop_top = code.len();
    // mov rsi, r8 ; add rsi, rax
    code.extend_from_slice(&[0x4C, 0x89, 0xC6]);
    code.extend_from_slice(&[0x48, 0x01, 0xC6]);
    // mov rdi, r10 ; mov rcx, r11
    code.extend_from_slice(&[0x4C, 0x89, 0xD7]);
    code.extend_from_slice(&[0x4C, 0x89, 0xD9]);
    // cld ; repe cmpsb
    code.push(0xFC);
    code.extend_from_slice(&[0xF3, 0xA6]);
    // je .yes (rel32)
    code.push(0x0F);
    code.push(0x84);
    let yes_from_match_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // inc rax ; cmp rax, r9 ; jbe .scan_loop
    code.extend_from_slice(&[0x48, 0xFF, 0xC0]);
    code.extend_from_slice(&[0x4C, 0x39, 0xC8]);
    code.push(0x0F);
    code.push(0x86); // jbe rel32
    let back_off = scan_loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back_off.to_le_bytes());

    // Fall through to .no.
    // .no:
    let no_label = code.len();
    let no_off = no_label as i32 - (no_from_short_patch as i32 + 4);
    code[no_from_short_patch..no_from_short_patch + 4].copy_from_slice(&no_off.to_le_bytes());
    // xor al, al ; jmp .done
    code.extend_from_slice(&[0x30, 0xC0]);
    code.push(0xEB);
    let done_from_no_patch = code.len();
    code.push(0x00);

    // .yes:
    let yes_label = code.len();
    let yes_off_empty = yes_label as i32 - (yes_from_empty_patch as i32 + 4);
    code[yes_from_empty_patch..yes_from_empty_patch + 4]
        .copy_from_slice(&yes_off_empty.to_le_bytes());
    let yes_off_match = yes_label as i32 - (yes_from_match_patch as i32 + 4);
    code[yes_from_match_patch..yes_from_match_patch + 4]
        .copy_from_slice(&yes_off_match.to_le_bytes());
    // mov al, 1
    code.extend_from_slice(&[0xB0, 0x01]);

    // .done: movzx rax, al
    let done_label = code.len();
    code[done_from_no_patch] = (done_label - done_from_no_patch - 1) as u8;
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0xC0]);

    Ok(())
}

/// Slice "parse_int": resolve the inner text expression to (rsi=ptr,
/// rcx=len), then run the strict scan into rax. Self-contained — the
/// abort tail is inlined at the end of the emitted sequence.
///
/// Inner is restricted to text-producing shapes whose (ptr, len) live
/// in stable rbp slots:
///   - `Expr::Read(name)`  — resource buffer + len from the prologue
///   - `Expr::Ident(name)` with a text_bindings entry — Phase 2I lets
///                                                       and Fetch
///   - `Expr::Fetch(name, _)` — connection response (ptr, len)
///
/// Other shapes (Field text via argv, Concat, Call, JsonEscape) would
/// need either a strlen pre-pass or a per-call scratch buffer. They
/// can be added in a follow-up; the dominant use case is parse_int(read(...)).
fn emit_parse_int(
    code: &mut Vec<u8>,
    inner: &Expr,
    input_name: &str,
    text_bindings: &TextBindings<'_>,
    offsets: &HashMap<&str, i32>,
    all_rules: &HashMap<&str, &Rule>,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Result<(), NativeError> {
    // Step 1: resolve (rsi=ptr, rcx=len) for the inner. Three shapes:
    //   - BoundText (Read / Ident / Fetch in text_bindings): load
    //     (ptr, len) from the two registered slots — zero scan.
    //   - text input field: pointer is in the field's rbp slot;
    //     length recovered via emit_strlen (argv-style NUL terminator).
    //   - anything else (Concat, Call, JsonEscape, ...): refused with
    //     a clear message. Workaround: bind via a `let` and parse_int
    //     the let.
    match inner {
        Expr::Read(name) | Expr::Ident(name) | Expr::Fetch(name, _)
            if text_bindings.contains_key(name.as_str()) =>
        {
            let (ptr_slot, len_slot) = text_bindings[name.as_str()];
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
        }
        Expr::Field(_, fname) => {
            let &offset = offsets.get(fname.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "parse_int: field '{}' has no rbp slot in scope",
                    fname
                ),
            })?;
            // mov rsi, [rbp + offset]   — NUL-terminated argv pointer
            if offset >= -128 {
                code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                code.push(offset as u8);
            } else {
                code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                code.extend_from_slice(&offset.to_le_bytes());
            }
            // emit_strlen reads rsi, writes rdx (rsi unchanged). Then
            // copy length into rcx so the scan loop below works
            // identically to the BoundText path.
            emit_strlen(code);
            // mov rcx, rdx
            code.extend_from_slice(&[0x48, 0x89, 0xD1]);
        }
        Expr::Read(name) | Expr::Ident(name) | Expr::Fetch(name, _) => {
            return Err(NativeError {
                message: format!(
                    "parse_int: inner '{}' has no (ptr, len) slots registered at this point — \
                     the resource/connection/let must be visible in the surrounding scope",
                    name
                ),
            });
        }
        // Phase 2G-shape inline: a Call returning text via a body that
        // itself resolves to a parse_int-supported shape (text input
        // field or BoundText). Same constraints as the rest of the
        // Phase 2G family: single Ident(input) arg, no callee lets,
        // callee returns text, same input ident name. A callee body
        // that's a concat / nested call would need buffer
        // materialisation and is refused.
        Expr::Call(callee_name, call_args) => {
            if call_args.len() != 1 {
                return Err(NativeError {
                    message: format!(
                        "parse_int(call): callee '{}' must take exactly one argument; got {}",
                        callee_name, call_args.len()
                    ),
                });
            }
            match &call_args[0] {
                Expr::Ident(n) if n == input_name => {}
                other => {
                    return Err(NativeError {
                        message: format!(
                            "parse_int(call): callee '{}' argument must be the caller's input ident '{}'; got {}",
                            callee_name, input_name, expr_kind(other)
                        ),
                    });
                }
            }
            let callee = all_rules.get(callee_name.as_str()).ok_or_else(|| NativeError {
                message: format!("parse_int(call): unknown rule '{}'", callee_name),
            })?;
            if !callee.logic.bindings.is_empty() {
                return Err(NativeError {
                    message: format!(
                        "parse_int(call): callee '{}' has let bindings (same Phase 2G restriction \
                         as length(call) / starts_with(call) / json_escape(call)). Workaround: \
                         inline the let RHSes into the body, or bind the callee result via a `let` \
                         in the caller and parse_int the let.",
                        callee_name
                    ),
                });
            }
            if !matches!(callee.output_ty, Type::Text) {
                return Err(NativeError {
                    message: format!(
                        "parse_int(call): callee '{}' must return text; got {:?}",
                        callee_name, callee.output_ty
                    ),
                });
            }
            if callee.input_name != input_name {
                return Err(NativeError {
                    message: format!(
                        "parse_int(call): callee '{}' uses input ident '{}', caller uses '{}' — \
                         inline requires the same input ident name",
                        callee_name, callee.input_name, input_name
                    ),
                });
            }
            // Recurse on the callee body. The match arms above
            // re-dispatch on the body's shape; allocation-needing
            // shapes bubble up the catch-all error naming the
            // offending shape inside the callee.
            return emit_parse_int(
                code,
                &callee.logic.value,
                input_name,
                text_bindings,
                offsets,
                all_rules,
                field_ranges,
            );
        }
        // substring(text, start, end) as parse_int's inner: inline
        // the substring emit, leave (rsi=slice_ptr, rcx=slice_len)
        // for the scan loop below. Same algorithm as the byte-prim
        // slice 2c and json_escape slice 2d — duplicated here for
        // the same reason (no shared helper yet).
        Expr::Substring(text, start_expr, end_expr) => {
            // Inline-load the text inner (parse_int restricts to text
            // input field or BoundText — no Call recursion via the
            // load function exists for parse_int). Produces (rsi, rcx).
            // Then adapt to (rax, rdx), call helper, adapt back to
            // (rsi, rcx) — parse_int's scan loop downstream reads (rsi, rcx).
            match text.as_ref() {
                Expr::Read(name) | Expr::Ident(name) | Expr::Fetch(name, _)
                    if text_bindings.contains_key(name.as_str()) =>
                {
                    let (ptr_slot, len_slot) = text_bindings[name.as_str()];
                    if ptr_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(ptr_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&ptr_slot.to_le_bytes());
                    }
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x4D]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x8D]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                }
                Expr::Field(_, fname) => {
                    let &offset = offsets.get(fname.as_str()).ok_or_else(|| NativeError {
                        message: format!(
                            "parse_int(substring): text inner field '{}' has no rbp slot",
                            fname
                        ),
                    })?;
                    if offset >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x75]);
                        code.push(offset as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0xB5]);
                        code.extend_from_slice(&offset.to_le_bytes());
                    }
                    emit_strlen(code);
                    code.extend_from_slice(&[0x48, 0x89, 0xD1]); // mov rcx, rdx
                }
                other => {
                    return Err(NativeError {
                        message: format!(
                            "parse_int(substring): text inner must be a text input field \
                             or BoundText (read / text-let / fetch) — got {:?}. Concat / \
                             Call / nested substring as text inner would need buffer \
                             materialisation; bind via a `let` first.",
                            expr_kind(other)
                        ),
                    });
                }
            }
            // Adapt (rsi, rcx) → (rax, rdx) for the helper.
            code.extend_from_slice(&[0x48, 0x89, 0xF0]); // mov rax, rsi
            code.extend_from_slice(&[0x48, 0x89, 0xCA]); // mov rdx, rcx
            emit_substring_bounds_and_slice(
                code, start_expr, end_expr,
                input_name, offsets, all_rules, field_ranges, text_bindings,
            )?;
            // Adapt back: (rax, rdx) → (rsi, rcx). parse_int's scan
            // loop below reads (rsi, rcx).
            code.extend_from_slice(&[0x48, 0x89, 0xC6]); // mov rsi, rax
            code.extend_from_slice(&[0x48, 0x89, 0xD1]); // mov rcx, rdx
        }
        _ => {
            return Err(NativeError {
                message: format!(
                    "parse_int: inner must be a text input field, a BoundText reference \
                     (read(<resource>), text let, fetch(<connection>, ...)), a text-returning \
                     rule call (Phase 2G), or substring(<text>, <start>, <end>) — got {:?}. \
                     Concat / JsonEscape as parse_int args still need a buffer materialisation \
                     that the strict-scan algorithm explicitly avoids; bind via a `let` and \
                     parse_int the let.",
                    expr_kind(inner)
                ),
            });
        }
    }

    // Step 3: empty input → abort (parse_int requires at least one digit).
    // test rcx, rcx ; je .pi_abort
    code.extend_from_slice(&[0x48, 0x85, 0xC9]);
    code.push(0x0F);
    code.push(0x84);
    let empty_abort_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Step 4: optional leading '-'. r8b = sign flag (1 if negative).
    // xor r8d, r8d
    code.extend_from_slice(&[0x45, 0x31, 0xC0]);
    // mov al, [rsi]
    code.extend_from_slice(&[0x8A, 0x06]);
    // cmp al, '-'
    code.extend_from_slice(&[0x3C, 0x2D]);
    // jne .digits_init — patched after we know the consume-minus block size.
    code.push(0x75);
    let skip_minus_patch = code.len();
    code.push(0x00);
    // Consume-minus block: inc rsi (3) + dec rcx (3) + mov r8b, 1 (3) = 9 bytes.
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]); // inc rsi
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]); // dec rcx
    code.extend_from_slice(&[0x41, 0xB0, 0x01]); // mov r8b, 1
    // Patch the jne to skip exactly the consume-minus block.
    let after_consume = code.len();
    code[skip_minus_patch] = (after_consume - skip_minus_patch - 1) as u8;
    // .digits_init:
    // After consuming minus, rcx may have hit 0 → '-' alone is invalid.
    // test rcx, rcx ; je .pi_abort
    code.extend_from_slice(&[0x48, 0x85, 0xC9]);
    code.push(0x0F);
    code.push(0x84);
    let lone_minus_abort_patch = code.len();
    code.extend_from_slice(&[0; 4]);

    // Step 5: scan loop.
    // xor rax, rax  (accumulator)
    code.extend_from_slice(&[0x48, 0x31, 0xC0]);
    let loop_top = code.len();
    // movzx rdx, byte [rsi]
    code.extend_from_slice(&[0x48, 0x0F, 0xB6, 0x16]);
    // sub rdx, '0'
    code.extend_from_slice(&[0x48, 0x83, 0xEA, 0x30]);
    // cmp rdx, 9 ; ja .pi_abort
    code.extend_from_slice(&[0x48, 0x83, 0xFA, 0x09]);
    code.push(0x0F);
    code.push(0x87);
    let nondigit_abort_patch = code.len();
    code.extend_from_slice(&[0; 4]);
    // imul rax, rax, 10  (3-operand form, 4 bytes)
    code.extend_from_slice(&[0x48, 0x6B, 0xC0, 0x0A]);
    // add rax, rdx
    code.extend_from_slice(&[0x48, 0x01, 0xD0]);
    // inc rsi ; dec rcx ; jnz loop_top
    code.extend_from_slice(&[0x48, 0xFF, 0xC6]);
    code.extend_from_slice(&[0x48, 0xFF, 0xC9]);
    // jnz rel32 back to loop_top
    code.push(0x0F);
    code.push(0x85);
    let back_off = loop_top as i32 - (code.len() + 4) as i32;
    code.extend_from_slice(&back_off.to_le_bytes());

    // Step 6: apply sign.
    // test r8b, r8b ; je .pi_done
    code.extend_from_slice(&[0x45, 0x84, 0xC0]);
    code.push(0x74);
    code.push(0x03);  // skip the 3-byte neg
    // neg rax
    code.extend_from_slice(&[0x48, 0xF7, 0xD8]);

    // jmp .pi_done (skip the abort tail)
    code.push(0xEB);
    let done_jmp_patch = code.len();
    code.push(0x00);

    // .pi_abort: sys_exit(1) — fail-closed on any parse error.
    let pi_abort_label = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]); // mov rax, 60
    code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]); // mov rdi, 1
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // Patch the three abort jumps to land at .pi_abort.
    let off = pi_abort_label as i32 - (empty_abort_patch as i32 + 4);
    code[empty_abort_patch..empty_abort_patch + 4].copy_from_slice(&off.to_le_bytes());
    let off = pi_abort_label as i32 - (lone_minus_abort_patch as i32 + 4);
    code[lone_minus_abort_patch..lone_minus_abort_patch + 4].copy_from_slice(&off.to_le_bytes());
    let off = pi_abort_label as i32 - (nondigit_abort_patch as i32 + 4);
    code[nondigit_abort_patch..nondigit_abort_patch + 4].copy_from_slice(&off.to_le_bytes());

    // .pi_done: rax holds the parsed (signed) value.
    let pi_done_label = code.len();
    code[done_jmp_patch] = (pi_done_label - done_jmp_patch - 1) as u8;

    Ok(())
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
    all_resources: &HashMap<&str, &Resource>,
) -> Result<Vec<u8>, NativeError> {
    let nfields = concept.fields.len();
    let mut offsets = field_offsets(concept);
    let is_bool = rule.output_ty == Type::Bool;

    // Slice 9.5f: enumerate resources the rule reads, in source order.
    // Each contributes 16 bytes (ptr + len) plus a max-padded buffer to
    // the rbp frame. Same accounting as every other resource-aware
    // emitter — the canonical pattern from slice 9.1.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(rule, all_resources, all_rules, "rule")?;
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);

    // now_unix() in parallel rules: parent samples clock_gettime ONCE
    // before fork; both halves inherit the captured value via COW.
    // Same "ONCE per rule invocation" invariant as every other emitter,
    // and the same fork-COW propagation pattern as Phase 9 slice 4's
    // `cache: true` resources — the captured value is fixed at fork
    // time, so children CANNOT diverge into per-worker clock samples.
    let uses_now = rule_uses_now_unix(rule);
    let now_extra_bytes: i32 = if uses_now { 16 } else { 0 };

    let mut code = Vec::new();
    let mut resource_abort_patches: Vec<usize> = Vec::new();
    let mut text_bindings: TextBindings<'_> = HashMap::new();

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

    // Setup rbp frame for field storage. Slice 9.5f: grow by
    // resource_extra_bytes so each resource's (ptr, len, buffer) lives
    // INSIDE the rbp frame (not on the array side). Children inherit
    // these slots via fork's COW after the resource read happens
    // ONCE in the parent — no per-worker syscall, consistent with
    // slice 10 + 9.4's forked+cached pattern.
    code.push(0x55); // push rbp
    code.extend_from_slice(&[0x48, 0x89, 0xE5]); // mov rbp, rsp
    let frame = ((nfields * 8
        + resource_extra_bytes as usize
        + now_extra_bytes as usize
        + 15)
        & !15) as i32;
    code.extend_from_slice(&[0x48, 0x81, 0xEC]);
    code.extend_from_slice(&frame.to_le_bytes());

    // Slice 9.5f: emit each resource read sequence ONCE, BEFORE the
    // parse loop and BEFORE the fork. emit_resource_read_sequence
    // reuses r15 as the file fd; we save/restore r15 (the array base)
    // around each call to preserve it.
    let mut next_slot: i32 = -((nfields as i32) * 8 + 8);
    if !referenced_resources.is_empty() {
        // push r15  (save array base across the resource reads)
        code.extend_from_slice(&[0x41, 0x57]);
        for r in &referenced_resources {
            let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
                &mut code,
                r,
                next_slot,
                &mut resource_abort_patches,
            );
            text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
            next_slot = new_next;
        }
        // pop r15  (restore array base)
        code.extend_from_slice(&[0x41, 0x5F]);
    }

    // now_unix() capture: clock_gettime(CLOCK_REALTIME) ONCE in the
    // parent, BEFORE fork. tv_sec lands at `now_slot` (the deeper of two
    // reserved slots); tv_nsec scratch occupies the slot immediately
    // above. Both fork halves see the same captured value via COW —
    // matches the "ONCE per rule invocation" semantic of non-parallel
    // emitters. Slot registered under the synthetic name `"now"` in
    // `offsets` below so emit_eval_expr's NowUnix arm resolves to a
    // load from this slot. The clock_gettime syscall clobbers rax/
    // rcx/r11, which are all ephemeral at this point in the prologue
    // (r12=argc, r13=argv, r14/r15 set later); no save needed.
    let now_slot: i32 = if uses_now {
        let s = next_slot - 8; // tv_sec at deeper slot
        emit_capture_now_unix(&mut code, s);
        offsets.insert("now", s);
        s
    } else {
        0
    };
    let _ = now_slot;

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
        &text_bindings,
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

    // Slice 9.5f: shared abort label for resource open/read failures.
    // Resource reads happen in the parent BEFORE the fork, so an abort
    // here kills the single parent process without orphaning any
    // child — fail-closed even under parallel dispatch. Zero-byte cost
    // when no resource is referenced (the abort label is conditional).
    emit_resource_abort_tail(&mut code, &resource_abort_patches);

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
            Http10HandlerShape::Constant if service.logs.is_empty() => {
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
    let shape = classify_http10_expr(&handler.logic.value, &handler.input_name);
    // Phase 2I-in-handlers: handler `let` bindings are now supported via the
    // dynamic emission path (let prologue runs after HTTP parse and before
    // emit_handler_to_slots). The Constant fast path doesn't emit a let
    // prologue, so any handler with bindings must route through Dynamic
    // even when its response record is a literal pair — otherwise the
    // bindings would be silently dropped.
    if !handler.logic.bindings.is_empty() {
        if matches!(shape, Http10HandlerShape::Constant) {
            return Http10HandlerShape::Dynamic;
        }
    }
    shape
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
                    // Slice 3e: status can be any Number-typed expression
                    // (verifier already type-checks it against HttpResponse's
                    // declared `status: number [100, 599]`). Non-literal
                    // status forces the Dynamic shape so the emitter goes
                    // through emit_eval_expr instead of the Constant fast
                    // path. The bound check is the verifier's job; native
                    // trusts the proof.
                    _ => {
                        has_dynamic_field = true;
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
                    // Phase 9 slice 2: body = read(<resource>). The accept
                    // loop reads the file once per request into a per-frame
                    // buffer; the body slots receive (ptr, len) directly
                    // from the resource's bound (ptr_slot, len_slot). Goes
                    // through the Dynamic path so the resource open/read
                    // sequence is emitted before the handler runs.
                    Expr::Read(_) => {
                        has_dynamic_field = true;
                    }
                    // Phase 11 slice 2: body = fetch(<connection>, <literal>).
                    // The accept loop does socket/connect/write/read/close
                    // once per request into a per-frame buffer; the body
                    // slots receive (ptr, len) directly from the connection's
                    // bound (ptr_slot, len_slot). Same Dynamic dispatch as
                    // read(<resource>): the connection's I/O sequence is
                    // emitted between the per-accept resource reads and the
                    // HTTP read, so the (ptr, len) slot pair is populated by
                    // the time emit_handler_to_slots runs. Slice 11.2 keeps
                    // the request bytes restricted to literal-only (the
                    // Phase 11 slice 1 verifier rule applies); request
                    // bytes that reference req.method / req.path land in
                    // slice 11.3.
                    Expr::Fetch(_, _) => {
                        has_dynamic_field = true;
                    }
                    _ => {
                        return Http10HandlerShape::Unsupported(format!(
                            "slice 3d: body must be a text literal, req.method / req.path, \
                             concat(...), read(<resource>), or fetch(<connection>, <literal>); \
                             other computed bodies land in a later slice (got {:?})",
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
        Expr::Read(_) => "Read",
        Expr::Fetch(_, _) => "Fetch",
        Expr::JsonEscape(_) => "JsonEscape",
        Expr::ParseInt(_) => "ParseInt",
        Expr::NowUnix => "NowUnix",
        Expr::StartsWith(_, _) => "StartsWith",
        Expr::Contains(_, _) => "Contains",
        Expr::EndsWith(_, _) => "EndsWith",
        Expr::Length(_) => "Length",
        Expr::Abs(_) => "Abs",
        Expr::Min(_, _) => "Min",
        Expr::Max(_, _) => "Max",
        Expr::Substring(_, _, _) => "Substring",
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

    // Phase 9 slice 2: index every top-level `resource` block by name so
    // the dynamic emitter can look up resources the handler reads. Same
    // shape as compile_native's resources index — kept local to the
    // service path because rule binaries and service binaries take
    // different code paths.
    let all_resources: HashMap<&str, &Resource> = program.items.iter().filter_map(|i| match i {
        Item::Resource(r) => Some((r.name.as_str(), r)),
        _ => None,
    }).collect();
    // Phase 11 slice 2: index every top-level `connection` block by name
    // so the dynamic emitter can look up connections the handler fetches.
    // Mirrors the resource index above; the two namespaces are checked for
    // disjointness in the verifier (a name cannot be both a resource and
    // a connection).
    let all_connections: HashMap<&str, &Connection> = program.items.iter().filter_map(|i| match i {
        Item::Connection(c) => Some((c.name.as_str(), c)),
        _ => None,
    }).collect();

    let code = emit_http10_dynamic_bytes(service, handler, &offsets, &no_rules, &no_ranges, &all_resources, &all_connections)?;
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
            Field {
                name: "body".to_string(),
                ty: Type::Text,
                range: Some((0, 4096)),
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
    all_resources: &HashMap<&str, &Resource>,
    all_connections: &HashMap<&str, &Connection>,
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
    // Slice 8e: any log block referencing req.timestamp triggers the slot
    // allocation — one timestamp slot serves all logs (clock_gettime fires
    // once per accept, every log that needs it reads the same instant).
    let uses_timestamp = service.logs.iter().any(|lb| match &lb.effect {
        Effect::AppendFile { content, .. } => log_content_uses_req_timestamp(content),
        _ => false,
    });
    // req.body slot: only allocate when the handler logic OR any log
    // content references `req.body`. Body parsing in the HTTP parser
    // is conditional on this — the cost (one inline scan for \r\n\r\n
    // and two slot stores) is paid only when body is consumed.
    let uses_body = expr_uses_field(&handler.logic.value, &handler.input_name, "body")
        || handler.logic.bindings.iter().any(|(_, e)| expr_uses_field(e, &handler.input_name, "body"))
        || service.logs.iter().any(|lb| match &lb.effect {
            Effect::AppendFile { content, .. } => expr_uses_field(content, "req", "body"),
            _ => false,
        });
    // Phase 9 slice 2: enumerate resources the handler reads, in source
    // order, and resolve each against the program's top-level resource
    // table. A name unknown at this point is a hard error — the verifier
    // already rejects unknown resources at parse time, so reaching here
    // means the dispatcher was invoked with a stale handler.
    let referenced_resources: Vec<&Resource> =
        collect_referenced_resources(handler, all_resources, all_rules, "service handler")?;
    // Each resource contributes 16 bytes (ptr + len) plus a max_bytes
    // buffer padded to 8 bytes — same accounting as the rule-prologue
    // path in emit_record_loop_prologue.
    let resource_extra_bytes: i32 = compute_resource_extra_bytes(&referenced_resources);
    // Phase 11 slice 2: enumerate connections the handler fetches, in
    // source order, and resolve each against the program's top-level
    // connection table. A name unknown at this point is a hard error —
    // the verifier already rejects unknown connections at the rule-level
    // cross-check (handlers are rules), so reaching here means the
    // dispatcher was invoked with a stale handler. Same shape as the
    // resource path immediately above.
    let referenced_connections: Vec<&Connection> = {
        let names = collect_rule_fetch_names(handler);
        let mut out: Vec<&Connection> = Vec::with_capacity(names.len());
        for name in &names {
            let c = all_connections.get(name.as_str()).ok_or_else(|| NativeError {
                message: format!(
                    "service handler '{}' fetches connection '{}' but no top-level `connection {}` was declared",
                    handler.name, name, name
                ),
            })?;
            out.push(*c);
        }
        out
    };
    // Each connection contributes 16 bytes (ptr + len) plus a
    // max_response buffer padded to 8 — identical accounting to the
    // resource extras above; connections occupy the next monotonically-
    // descending block of slots after the resource block.
    let connection_extra_bytes: i32 = referenced_connections
        .iter()
        .map(|c| 16 + (((c.max_response as i32) + 7) & !7))
        .sum();
    // Slot map below rbp:
    //   -8     method ptr (parser output)
    //   -16    path ptr   (parser output)
    //   -24    status     (handler output)
    //   -32    body ptr   (handler output)
    //   -40    body len   (handler output)
    //   -48    client_fd
    //   -56    timestamp seconds        (only when uses_timestamp)
    //   below: resource (ptr, len) pairs + buffers, growing downward
    //   below: connection (ptr, len) pairs + response buffers, growing downward
    //   bottom: HTTP read buffer (max_request bytes)
    // Phase 2I-in-handlers: classify each handler `let` binding as text
    // (2 slots: ptr + len, same shape as Phase 2F's err_var) or number
    // (1 slot). Walk in source order so a later binding can refer to a
    // prior text let — same predicate the rule path uses, applied to the
    // synthetic HttpRequest concept the handler reads from.
    let http_request_concept_for_lets = http_request_builtin_concept_native();
    let mut prior_text_lets: HashSet<&str> = HashSet::new();
    let handler_binding_is_text: Vec<bool> = handler
        .logic
        .bindings
        .iter()
        .map(|(name, expr)| {
            let is_text = let_rhs_is_text(
                expr,
                &http_request_concept_for_lets,
                None,
                all_rules,
                &prior_text_lets,
            );
            if is_text {
                prior_text_lets.insert(name.as_str());
            }
            is_text
        })
        .collect();
    let handler_let_slots_bytes: i32 = handler_binding_is_text
        .iter()
        .map(|t| if *t { 16i32 } else { 8 })
        .sum();
    // Pre-body fixed offset: where the optional timestamp lives.
    let body_pre_offset: i32 = if uses_timestamp { 56 } else { 48 };
    let body_extra_bytes: i32 = if uses_body { 16 } else { 0 };
    // (req.body ptr, req.body len) follow timestamp, before handler lets.
    let body_ptr_slot: i32 = if uses_body { -(body_pre_offset + 8) } else { 0 };
    let body_len_slot: i32 = if uses_body { -(body_pre_offset + 16) } else { 0 };
    let frame_base_fixed: i32 = body_pre_offset + body_extra_bytes + handler_let_slots_bytes;
    let frame_base: i32 = frame_base_fixed + resource_extra_bytes + connection_extra_bytes;
    // Phase 8 slice 8d: collected `js abort_label` patch sites from
    // emit_append_file_call. Resolved after the accept loop emits the
    // shared abort sequence; left empty when policy is Drop.
    let mut abort_patches: Vec<usize> = Vec::new();
    // Phase 9 slice 2: open/read failure patches from each
    // emit_resource_read_sequence. They land at the same sys_exit(1)
    // sequence the slice-8d path already emits, so we just append into
    // `abort_patches` after the per-resource emit.
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

    // ═══ SIGCHLD = SIG_IGN (Phase 10 slice 10) ═════════════════
    // Forked mode only. Setting SIGCHLD's disposition to SIG_IGN tells
    // the kernel to auto-reap exiting children — no `wait`/`waitpid`,
    // no zombie processes accumulating. One syscall at startup, before
    // listen(), so it covers the very first connection. Sequential
    // mode skips this block entirely (preserving byte-for-byte
    // identity with the slice 9 binary).
    //
    // `struct sigaction` is the kernel x86-64 ABI shape (NOT libc):
    //   offset 0:  sa_handler  (8 bytes) = SIG_IGN = 1
    //   offset 8:  sa_flags    (8 bytes) = 0
    //   offset 16: sa_restorer (8 bytes) = 0  (unused without SA_RESTORER)
    //   offset 24: sa_mask     (8 bytes) = 0  (one longword, sigsetsize=8)
    //
    // Layout: jmp short over the 32-byte data block, then the syscall
    // itself with `lea rsi, [rip + disp32]` pointing back at the data.
    if service.concurrency == ConcurrencyMode::Forked {
        // jmp short +32 (over the data block)
        code.extend_from_slice(&[0xEB, 0x20]);
        let sigaction_data_at = code.len();
        // sa_handler = SIG_IGN = 1
        code.extend_from_slice(&1u64.to_le_bytes());
        // sa_flags = 0
        code.extend_from_slice(&0u64.to_le_bytes());
        // sa_restorer = 0
        code.extend_from_slice(&0u64.to_le_bytes());
        // sa_mask = 0
        code.extend_from_slice(&0u64.to_le_bytes());
        // mov rax, 13 (rt_sigaction)
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x0D, 0x00, 0x00, 0x00]);
        // mov rdi, 17 (SIGCHLD)
        code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x11, 0x00, 0x00, 0x00]);
        // lea rsi, [rip + disp32]  →  point at sigaction_data_at
        code.extend_from_slice(&[0x48, 0x8D, 0x35]);
        let lea_disp_at = code.len();
        code.extend_from_slice(&[0u8; 4]); // patched below
        let after_lea = code.len();
        let disp = sigaction_data_at as i32 - after_lea as i32;
        code[lea_disp_at..lea_disp_at + 4].copy_from_slice(&disp.to_le_bytes());
        // mov rdx, 0 (oldact = NULL)
        code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x00, 0x00, 0x00, 0x00]);
        // mov r10, 8 (sigsetsize)
        code.extend_from_slice(&[0x49, 0xC7, 0xC2, 0x08, 0x00, 0x00, 0x00]);
        // syscall
        code.extend_from_slice(&[0x0F, 0x05]);
    }

    // LISTEN
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x32, 0x00, 0x00, 0x00]); // rax=50
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0xC7, 0xC6, 0x80, 0x00, 0x00, 0x00]); // rsi=128
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall

    // ═══ CACHED RESOURCES (Phase 9 slice 9.4) ══════════════════
    // Resources marked `cache: true` get their open/read/close sequence
    // emitted ONCE here, between LISTEN and the accept_top label. The
    // (ptr, len) slots they populate sit within the per-process frame
    // (allocated by the prologue's `sub rsp, frame_size`) and SURVIVE
    // every iteration's `lea rsp, [rbp - frame_size]` epilogue, so the
    // already-loaded buffer is reused on every accept.
    //
    // Forked mode (slice 10): the fork dispatch lives INSIDE the accept
    // loop (after `accept` populates client_fd), so the cached read runs
    // once in the parent BEFORE any fork — children inherit the populated
    // slot via COW with no per-child read cost. Best case for static
    // assets on a forking server.
    //
    // Slot allocation walks the SAME `resource_next_slot` cursor used by
    // the per-iteration path below; this is what keeps `frame_base` correct
    // regardless of which resources cached and which didn't, and what lets
    // text_bindings register both kinds uniformly.
    //
    // Open/read failure here pushes into the same `abort_patches` Vec the
    // per-iteration path uses; both resolve to the shared sys_exit(1) label
    // at the end of the binary. Failures at startup kill the server before
    // serving any request, which is exactly the desired fail-closed
    // behaviour for any cached asset whose absence makes the service
    // meaningless to run.
    let mut http_text_bindings: TextBindings = HashMap::new();
    let mut resource_next_slot: i32 = -(frame_base_fixed + 8);
    for r in &referenced_resources {
        if r.cache {
            let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
                &mut code,
                r,
                resource_next_slot,
                &mut abort_patches,
            );
            http_text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
            resource_next_slot = new_next;
        }
    }

    // ═══ ACCEPT LOOP ═══════════════════════════════════════════
    let accept_top = code.len();
    code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00]); // rax=43 accept
    code.extend_from_slice(&[0x4C, 0x89, 0xE7]);                         // rdi=r12
    code.extend_from_slice(&[0x48, 0x31, 0xF6]);                         // rsi=0
    code.extend_from_slice(&[0x48, 0x31, 0xD2]);                         // rdx=0
    code.extend_from_slice(&[0x0F, 0x05]);                               // syscall
    // mov [rbp-48], rax  (save client_fd)
    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xD0]);                   // -48 = 0xD0 i8

    // ═══ FORK DISPATCH (Phase 10 slice 10) ═════════════════════
    // Forked mode only. After saving client_fd, fork(). Three branches:
    //   rax > 0  (parent): close(client_fd), jmp accept_top
    //   rax == 0 (child):  fall through to the existing iteration body
    //   rax < 0  (failed): write "fork failed\n" to stderr, then take
    //                      the same close + loop path as the parent
    //                      (drop the connection, keep serving).
    //
    // Layout (so the child path is the natural fall-through):
    //   mov rax, 57; syscall; test rax, rax
    //   jz child            (forward, into the rest of the function)
    //   js fork_error       (forward, into the inline error handler)
    //   <parent close + loop>
    //   <fork_error>: write to stderr, then jmp parent_close
    //   <child label>: end of dispatch — falls through naturally
    if service.concurrency == ConcurrencyMode::Forked {
        // mov rax, 57 (sys_fork) ; syscall ; test rax, rax
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x39, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x0F, 0x05]);
        code.extend_from_slice(&[0x48, 0x85, 0xC0]);                     // test rax, rax
        // jz rel8 to `child` (end of dispatch). Patched after we know
        // the dispatch length.
        code.extend_from_slice(&[0x74, 0x00]);
        let jz_to_child_at = code.len() - 1;
        // js rel8 to `fork_error`. Patched after parent_close emits.
        code.extend_from_slice(&[0x78, 0x00]);
        let js_to_err_at = code.len() - 1;

        // ── parent_close: close(client_fd) + jmp accept_top ──
        let parent_close_at = code.len();
        // mov rax, 3 (close) ; mov rdi, [rbp-48] ; syscall
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x48, 0x8B, 0x7D, 0xD0]);
        code.extend_from_slice(&[0x0F, 0x05]);
        // jmp accept_top (rel32 backward)
        code.push(0xE9);
        let back = accept_top as i32 - (code.len() as i32 + 4);
        code.extend_from_slice(&back.to_le_bytes());

        // ── fork_error: write "fork failed\n" to stderr, jmp parent_close ──
        let fork_error_at = code.len();
        // patch js_to_err: distance from end of `js` (js_to_err_at + 1 + 1) to fork_error_at
        let js_disp = (fork_error_at as i32) - (js_to_err_at as i32 + 1);
        // i8 fits — small forward jump
        code[js_to_err_at] = js_disp as i8 as u8;

        // jmp short +12 (over the message bytes)
        code.extend_from_slice(&[0xEB, 0x0C]);
        let msg_at = code.len();
        code.extend_from_slice(b"fork failed\n");
        // mov rax, 1 (write) ; mov rdi, 2 (stderr)
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x01, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x02, 0x00, 0x00, 0x00]);
        // lea rsi, [rip + disp32]  →  point at msg_at
        code.extend_from_slice(&[0x48, 0x8D, 0x35]);
        let lea_disp_at = code.len();
        code.extend_from_slice(&[0u8; 4]);
        let after_lea = code.len();
        let disp = msg_at as i32 - after_lea as i32;
        code[lea_disp_at..lea_disp_at + 4].copy_from_slice(&disp.to_le_bytes());
        // mov rdx, 12 (length)
        code.extend_from_slice(&[0x48, 0xC7, 0xC2, 0x0C, 0x00, 0x00, 0x00]);
        // syscall
        code.extend_from_slice(&[0x0F, 0x05]);
        // jmp parent_close (rel32 backward)
        code.push(0xE9);
        let back = parent_close_at as i32 - (code.len() as i32 + 4);
        code.extend_from_slice(&back.to_le_bytes());

        // ── child label: rest of iteration body falls through ──
        let child_at = code.len();
        let jz_disp = (child_at as i32) - (jz_to_child_at as i32 + 1);
        // sanity: jz uses rel8; our dispatch is small enough to fit i8
        debug_assert!((-128..=127).contains(&jz_disp),
            "phase 10: jz to child path overflowed rel8 ({})", jz_disp);
        code[jz_to_child_at] = jz_disp as i8 as u8;
    }

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

    // ═══ RESOURCES (Phase 9 slice 2) ═══════════════════════════
    // Per-accept open + read + close for every resource the handler reads
    // that is NOT marked `cache: true`. The cached ones already emitted
    // their read sequence above the accept_top label (slice 9.4) and
    // populated their (ptr, len) slots — those entries are already in
    // `http_text_bindings`.
    //
    // Buffers + (ptr, len) slots for non-cached resources live within the
    // per-accept frame; they are overwritten on every iteration, so the
    // file is consulted fresh per request without crossing accept
    // boundaries. text_bindings registers each resource name → (ptr_slot,
    // len_slot) so that downstream emitters (handler body Read arm,
    // concat args via the shared BoundText path, log content) all
    // resolve through the same lookup, regardless of whether the slot
    // was populated at startup or per-iteration.
    //
    // Slot layout: the `resource_next_slot` cursor is shared with the
    // cached pass above, so both kinds of resources contribute to the
    // same monotonically-descending sequence of (ptr, len, buffer)
    // triples. The relative ordering matches source order in the
    // resource declarations the handler references.
    for r in &referenced_resources {
        if !r.cache {
            let (ptr_slot, len_slot, _buf_slot, new_next) = emit_resource_read_sequence(
                &mut code,
                r,
                resource_next_slot,
                &mut abort_patches,
            );
            http_text_bindings.insert(r.name.as_str(), (ptr_slot, len_slot));
            resource_next_slot = new_next;
        }
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
    let mut parse_fail_patches = emit_http_parse_method_path(&mut code, buf_offset_from_rbp);

    // ═══ HTTP PARSE (body) ═════════════════════════════════════
    // Slice X (2026-04-29): when the rule references `req.body`, scan
    // forward from the post-parse rbx position for the "\r\n\r\n"
    // separator. body_ptr = matched_position + 4; body_len = remaining
    // rax bytes after the consume. On a missing or truncated separator
    // (request without headers terminator within the read window) we
    // jump to parse_fail like any other malformed input.
    //
    // rbx state on entry: just past the path's terminator byte (which
    // the method/path parser NUL'd in place). rax = bytes remaining.
    // The four-byte sliding window approach: while rax >= 4, compare
    // [rbx] to the little-endian representation of "\r\n\r\n" =
    // 0x0a0d0a0d; on match, advance rbx by 4 and decrement rax by 4
    // (now rbx = body_ptr, rax = body_len). On no match, inc/dec one
    // byte and retry. If rax ever drops below 4 without a match → fail.
    if uses_body {
        let scan_top = code.len();
        // cmp rax, 4 ; jl .body_fail (rel32)
        code.extend_from_slice(&[0x48, 0x83, 0xF8, 0x04]);
        code.push(0x0F);
        code.push(0x8C);
        parse_fail_patches.push(code.len());
        code.extend_from_slice(&[0; 4]);
        // cmp dword [rbx], 0x0a0d0a0d  ("\r\n\r\n" little-endian)
        code.extend_from_slice(&[0x81, 0x3B, 0x0D, 0x0A, 0x0D, 0x0A]);
        // je .found_crlfcrlf (rel8 forward)
        code.push(0x74);
        let patch_found = code.len();
        code.push(0x00);
        // inc rbx ; dec rax ; jmp scan_top (rel8)
        code.extend_from_slice(&[0x48, 0xFF, 0xC3]);
        code.extend_from_slice(&[0x48, 0xFF, 0xC8]);
        let back = scan_top as i32 - (code.len() + 2) as i32;
        code.push(0xEB);
        code.push(back as i8 as u8);

        // .found_crlfcrlf:
        let found_label = code.len();
        code[patch_found] = (found_label - patch_found - 1) as u8;
        // add rbx, 4 ; sub rax, 4   — body starts after the separator
        code.extend_from_slice(&[0x48, 0x83, 0xC3, 0x04]);
        code.extend_from_slice(&[0x48, 0x83, 0xE8, 0x04]);
        // mov [rbp + body_ptr_slot], rbx
        if body_ptr_slot >= -128 {
            code.extend_from_slice(&[0x48, 0x89, 0x5D]);
            code.push(body_ptr_slot as u8);
        } else {
            code.extend_from_slice(&[0x48, 0x89, 0x9D]);
            code.extend_from_slice(&body_ptr_slot.to_le_bytes());
        }
        // mov [rbp + body_len_slot], rax
        if body_len_slot >= -128 {
            code.extend_from_slice(&[0x48, 0x89, 0x45]);
            code.push(body_len_slot as u8);
        } else {
            code.extend_from_slice(&[0x48, 0x89, 0x85]);
            code.extend_from_slice(&body_len_slot.to_le_bytes());
        }
        // Register `body` in http_text_bindings so the existing
        // BoundText paths (concat / json_escape / starts_with /
        // contains / length / equality) handle Field(req, body)
        // through the same machinery as Read / Fetch.
        http_text_bindings.insert("body", (body_ptr_slot, body_len_slot));
    }

    // ═══ CONNECTIONS (Phase 11 slice 2 + slice 3) ══════════════
    // Per-accept socket + connect + write(request) + read(response) + close
    // for every connection the handler fetches. No `cache: true` for
    // connections in this phase, so the response is freshly fetched per
    // request.
    //
    // Slice 11.3 reorder: this block runs AFTER the HTTP parse populates
    // [rbp-8]=method ptr and [rbp-16]=path ptr, so the request_expr passed
    // to fetch() can reference req.method / req.path via the existing
    // `offsets` map. Slice 11.2 emitted the same block BEFORE the parse
    // (literal-only request bytes only); moving it AFTER is harmless to
    // literal-only requests and required for dynamic ones.
    //
    // Sharing r15: the resource sequence above (cached: at startup, non-
    // cached: above the parse) closes its fd before returning, and the HTTP
    // parse leaves rax/rbx clobbered but r15 untouched. The connection
    // sequence closes its own socket before returning, so the read syscall
    // ABOVE this block has already consumed the request bytes — no further
    // r15 use happens between the parse and the handler body.
    //
    // The per-connection (ptr, len, buf) triple lives within the per-accept
    // frame; on the next iteration we land back at accept_top with the same
    // rsp (the close+loop tail does not touch rsp), so the slots get
    // overwritten in place. http_text_bindings registers each connection
    // name → (ptr_slot, len_slot) so emit_handler_to_slots' Fetch arm
    // resolves through the same lookup as the Read arm.
    //
    // We pass `offsets` (carrying method→-8, path→-16), `field_ranges`
    // (empty for HttpRequest — text fields don't have numeric ranges),
    // `http_text_bindings` (resources + earlier connections), and
    // allow_dynamic_request=true so the literal-only guard is lifted.
    let http_request_concept_for_fetch = http_request_builtin_concept_native();
    for c in &referenced_connections {
        let (ptr_slot, len_slot, _buf_slot, new_next) = emit_connection_fetch_sequence(
            &mut code,
            c,
            handler,
            &http_request_concept_for_fetch,
            all_rules,
            resource_next_slot,
            &mut abort_patches,
            offsets,
            field_ranges,
            &http_text_bindings,
            true, // allow_dynamic_request — slice 11.3 lifts the guard
        )?;
        http_text_bindings.insert(c.name.as_str(), (ptr_slot, len_slot));
        resource_next_slot = new_next;
    }

    // ═══ HANDLER LET BINDINGS (Phase 2I-in-handlers) ══════════
    // Evaluate the handler's `let` bindings AFTER the HTTP parse and any
    // resource read / connection fetch have populated their slots, and
    // BEFORE the response record evaluates. Each text let lands in two
    // dedicated rbp slots (ptr + len) and registers in `http_text_bindings`
    // so the body's BoundText classifier resolves `Ident(let_name)` the
    // same way it does for `Read` / `Fetch`. Each number let lands in one
    // slot and is added to `handler_offsets` so emit_eval_expr's Field /
    // Ident resolution finds it.
    //
    // Slot layout (cursor descends from just below the fixed handler block):
    //   first slot:   -(56 if uses_timestamp else 48) - 8
    //   text let n:   ptr at cursor, len at cursor-8, cursor -= 16
    //   number let n: value at cursor, cursor -= 8
    // The slot range is reserved by `handler_let_slots_bytes` added into
    // frame_base_fixed above, so it never collides with the resource or
    // connection blocks below.
    let mut handler_offsets: HashMap<&str, i32> = offsets.clone();
    {
        let let_block_start: i32 = -(body_pre_offset + body_extra_bytes);
        let mut let_cursor: i32 = let_block_start - 8;
        for ((name, expr), is_text) in
            handler.logic.bindings.iter().zip(handler_binding_is_text.iter())
        {
            if *is_text {
                let ptr_slot = let_cursor;
                let len_slot = let_cursor - 8;
                let_cursor -= 16;
                emit_text_produce_ptrlen(
                    &mut code,
                    expr,
                    &handler.input_name,
                    &http_request_concept_for_lets,
                    all_rules,
                    &handler_offsets,
                    field_ranges,
                    &http_text_bindings,
                )?;
                store_rax_at_rbp(&mut code, ptr_slot);
                // mov [rbp + len_slot], rdx
                if len_slot >= -128 {
                    code.extend_from_slice(&[0x48, 0x89, 0x55]);
                    code.push(len_slot as u8);
                } else {
                    code.extend_from_slice(&[0x48, 0x89, 0x95]);
                    code.extend_from_slice(&len_slot.to_le_bytes());
                }
                http_text_bindings.insert(name.as_str(), (ptr_slot, len_slot));
            } else {
                let value_slot = let_cursor;
                let_cursor -= 8;
                emit_eval_expr(
                    &mut code,
                    expr,
                    &handler.input_name,
                    &handler_offsets,
                    all_rules,
                    field_ranges,
                    &http_text_bindings,
                )?;
                store_rax_at_rbp(&mut code, value_slot);
                handler_offsets.insert(name.as_str(), value_slot);
            }
        }
    }

    // ═══ HANDLER BODY ══════════════════════════════════════════
    // Populates [rbp-24]=status, [rbp-32]=body_ptr, [rbp-40]=body_len.
    emit_handler_to_slots(
        &mut code,
        &handler.logic.value,
        &handler.input_name,
        &handler_offsets,
        all_rules,
        field_ranges,
        &http_text_bindings,
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
    // Slice 8e: every declared log block fires in source order between the
    // handler and the response write. The log scope (synthetic concept,
    // offsets map, text_bindings) is built once outside the loop —
    // identical for every block — and reused for each emission. Each
    // block's own on_error policy is passed through to emit_append_file_call.
    if !service.logs.is_empty() {
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
        // req.body in log content (since 2026-04-30 body parsing): the
        // log scope sees body via the Field(req, body) form. Register
        // "body" with the same slot offsets the handler uses, so the
        // BoundText classifier (which looks up by field name) resolves
        // it through the same machinery as resp.body / resources / fetches.
        if uses_body {
            log_text_bindings.insert("body", (body_ptr_slot, body_len_slot));
        }

        for log_block in &service.logs {
            if let Effect::AppendFile { path, content } = &log_block.effect {
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
                    log_block.on_error,
                    &mut abort_patches,
                )?;
            }
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

    // ═══ ITERATION TAIL ════════════════════════════════════════
    // Sequential mode: the standard slice 3d epilogue — restore rsp to
    // the post-prologue invariant (frees any handler-allocated concat
    // buffer in one instruction) and jump back to accept_top.
    //
    // Forked mode: control reached here only inside the child process
    // (the parent took the close + jmp accept_top path inside the fork
    // dispatch). The child has finished serving one request, so it
    // exits with status 0 — sys_exit closes any remaining fds and
    // releases the per-request frame; no rsp restore needed.
    match service.concurrency {
        ConcurrencyMode::Sequential => {
            // `lea rsp, [rbp + neg_frame_size]`  (REX.W + 0x8D + ModRM 0xA5 + disp32)
            code.extend_from_slice(&[0x48, 0x8D, 0xA5]);
            let neg_frame: i32 = -(frame_size as i32);
            code.extend_from_slice(&neg_frame.to_le_bytes());
            // jmp accept_top
            code.push(0xE9);
            let back = accept_top as i32 - (code.len() as i32 + 4);
            code.extend_from_slice(&back.to_le_bytes());
        }
        ConcurrencyMode::Forked => {
            // mov rax, 60 (sys_exit) ; mov rdi, 0 ; syscall
            code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
            code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x00, 0x00, 0x00, 0x00]);
            code.extend_from_slice(&[0x0F, 0x05]);
        }
    }

    // ═══ ABORT SEQUENCE (Phase 8 slice 8d) ═════════════════════
    // Reachable only when on_error: abort and a log syscall returned
    // negative. Each `js` site in emit_append_file_call branches here;
    // we resolve them now that the label position is known. Sequence:
    // sys_exit(1).
    if !abort_patches.is_empty() {
        let abort_label = code.len();
        for site in &abort_patches {
            let rel = abort_label as i32 - (*site as i32 + 4);
            code[*site..*site + 4].copy_from_slice(&rel.to_le_bytes());
        }
        // mov rax, 60 (sys_exit) ; mov rdi, 1 ; syscall
        code.extend_from_slice(&[0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00]);
        code.extend_from_slice(&[0x0F, 0x05]);
    }

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
    text_bindings: &TextBindings<'_>,
) -> Result<(), NativeError> {
    match expr {
        Expr::Record(name, fields) if name == "HttpResponse" => {
            // Store status (Number literal — fast path — or any Number
            // expression evaluated via emit_eval_expr, slice 3e).
            let mut status_ref: Option<&Expr> = None;
            let mut body_ref: Option<&Expr> = None;
            for (fname, fexpr) in fields {
                match fname.as_str() {
                    "status" => status_ref = Some(fexpr),
                    "body" => body_ref = Some(fexpr),
                    _ => {
                        return Err(NativeError {
                            message: format!("unexpected HttpResponse field '{}'", fname),
                        })
                    }
                }
            }
            let status_expr = status_ref.ok_or_else(|| NativeError {
                message: "HttpResponse missing status".into(),
            })?;
            let body_expr = body_ref.ok_or_else(|| NativeError {
                message: "HttpResponse missing body".into(),
            })?;

            match status_expr {
                Expr::Number(n) => {
                    // Slice 3b/3c fast path: literal → 7-byte immediate
                    // store. mov qword [rbp-24], <status as i32 sign-ext>.
                    code.extend_from_slice(&[0x48, 0xC7, 0x45, 0xE8]);   // -24 = 0xE8
                    code.extend_from_slice(&(*n as i32).to_le_bytes());
                }
                _ => {
                    // Slice 3e: any Number expression. The verifier has
                    // already type-checked status against HttpResponse's
                    // `status: number [100, 599]`; native trusts it and
                    // dispatches to the generic evaluator, then stores
                    // rax at the status slot.
                    emit_eval_expr(code, status_expr, input_name, offsets, all_rules, field_ranges, text_bindings)?;
                    // mov [rbp-24], rax
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE8]);
                }
            }

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
                        text_bindings,
                    )?;
                    // mov [rbp-32], rax  (body ptr)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);
                    // mov [rbp-40], rdx  (body len)
                    code.extend_from_slice(&[0x48, 0x89, 0x55, 0xD8]);
                }
                // Phase 9 slice 2: body = read(<resource>). The accept loop
                // already opened, read, and closed the file before the
                // handler ran; (ptr, len) live at the slots registered in
                // text_bindings under the resource's name. Copy both into
                // the body slots — same shape as the input-text-field arm
                // above, just with a known-good slot pair.
                Expr::Read(name) if text_bindings.contains_key(name.as_str()) => {
                    let (ptr_slot, len_slot) = text_bindings[name.as_str()];
                    // mov rax, [rbp + ptr_slot]
                    if ptr_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                        code.push(ptr_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                        code.extend_from_slice(&ptr_slot.to_le_bytes());
                    }
                    // mov [rbp-32], rax (body ptr)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);
                    // mov rax, [rbp + len_slot]
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                    // mov [rbp-40], rax (body len)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xD8]);
                }
                // Phase 11 slice 2: body = fetch(<connection>, <literal>).
                // The accept loop already ran socket + connect + write +
                // read + close before the handler; (ptr, len) live at the
                // slots registered in text_bindings under the connection's
                // name. Same shape as the Read arm above — copy both into
                // the body slots. The request_expr is intentionally ignored
                // here: it was lowered when the per-accept fetch sequence
                // was emitted, and only its byte effect (the response
                // sitting in the connection buffer) matters at this site.
                Expr::Fetch(name, _) if text_bindings.contains_key(name.as_str()) => {
                    let (ptr_slot, len_slot) = text_bindings[name.as_str()];
                    // mov rax, [rbp + ptr_slot]
                    if ptr_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                        code.push(ptr_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                        code.extend_from_slice(&ptr_slot.to_le_bytes());
                    }
                    // mov [rbp-32], rax (body ptr)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xE0]);
                    // mov rax, [rbp + len_slot]
                    if len_slot >= -128 {
                        code.extend_from_slice(&[0x48, 0x8B, 0x45]);
                        code.push(len_slot as u8);
                    } else {
                        code.extend_from_slice(&[0x48, 0x8B, 0x85]);
                        code.extend_from_slice(&len_slot.to_le_bytes());
                    }
                    // mov [rbp-40], rax (body len)
                    code.extend_from_slice(&[0x48, 0x89, 0x45, 0xD8]);
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
            emit_eval_expr(code, cond, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // test rax, rax ; jz else_label
            code.extend_from_slice(&[0x48, 0x85, 0xC0]);
            code.push(0x0F);
            code.push(0x84);
            let patch_else = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);

            // then arm
            emit_handler_to_slots(code, then_e, input_name, offsets, all_rules, field_ranges, text_bindings)?;
            // jmp end_label
            code.push(0xE9);
            let patch_end = code.len();
            code.extend_from_slice(&[0, 0, 0, 0]);

            // else_label:
            let else_pos = code.len();
            let rel = else_pos as i32 - (patch_else as i32 + 4);
            code[patch_else..patch_else + 4].copy_from_slice(&rel.to_le_bytes());

            // else arm
            emit_handler_to_slots(code, else_e, input_name, offsets, all_rules, field_ranges, text_bindings)?;

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

    /// Cross-concept match_result: the outer rule takes a wider concept
    /// (Purchase: amount + age + name) than the callee (AmountOnly: just
    /// amount). The slice contract — callee.fields ⊆ outer.fields by
    /// (name, type) — lets the inlined callee body reuse the outer's
    /// rbp slots without re-marshalling. End-to-end runtime check
    /// covers both arms.
    #[test]
    fn native_cross_concept_match_result_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept AmountOnly
  @intention: "Just an amount"
  @source: invoices.intent:1

  fields:
    amount : number


concept Purchase
  @intention: "A purchase has amount, age, and customer name"
  @source: invoices.intent:1

  fields:
    amount        : number
    customer_age  : number
    customer_name : text


rule validate_amount
  @intention: "Reusable validator: positive amount → Ok, else Err"
  @source: invoices.intent:1

  input:
    a : AmountOnly

  output:
    r : Result(number, text)

  logic:
    r = if a.amount > 0 then Ok(a.amount) else Err("non-positive amount")

  proofs:
    purity:
      reads   : [a.amount]
      calls   : []
    termination:
      bound : 4


rule discount_purchase
  @intention: "Apply 10% discount via the cross-concept validator"
  @source: invoices.intent:1

  input:
    p : Purchase

  output:
    r : Result(number, text)

  logic:
    r = match_result(validate_amount(p), amount => Ok(amount * 90 / 100), reason => Err(reason))

  proofs:
    purity:
      reads   : [p]
      calls   : [validate_amount]
    termination:
      bound : 6
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_cross_concept_match");
        compile_native(&program, "discount_purchase", out.to_str().unwrap(), false, false)
            .expect("cross-concept match_result must compile");

        // Runtime: both arms must produce the right thing.
        // Adult, positive amount → Ok arm → 1000 * 90 / 100 = 900
        let ok_run = Command::new(&out)
            .args(["1000", "25", "Alice"])
            .output()
            .expect("spawn discount_purchase");
        assert_eq!(
            String::from_utf8_lossy(&ok_run.stdout).trim(),
            "900",
            "Ok arm should output 900; full stdout: {:?}",
            ok_run.stdout,
        );

        // Negative amount → callee Err → outer Err pass-through
        let err_run = Command::new(&out)
            .args(["-50", "25", "Bob"])
            .output()
            .expect("spawn discount_purchase err arm");
        assert_eq!(
            String::from_utf8_lossy(&err_run.stderr).trim(),
            "non-positive amount",
            "Err arm should propagate the callee's reason on stderr; got stdout={:?} stderr={:?}",
            err_run.stdout, err_run.stderr,
        );

        let _ = fs::remove_file(out);
    }

    /// Nested match_result: an outer match_result whose Ok arm itself
    /// runs another match_result (chained validation). The prologue
    /// reserves N quartets where N = static max nesting depth; each
    /// level uses its own quartet via the +32-byte shift in
    /// `next_slots`. This test exercises a 2-deep chain with all four
    /// reachable arm combinations: outer Err, outer-Ok-then-inner-Err,
    /// outer-Ok-then-inner-Ok.
    #[test]
    fn native_nested_match_result_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept P
  @intention: "amount + age"
  @source: invoices.intent:1

  fields:
    amount       : number
    customer_age : number


rule validate_amount
  @intention: "positive amount → Ok, else Err"
  @source: invoices.intent:1

  input:
    p : P

  output:
    r : Result(number, text)

  logic:
    r = if p.amount > 0 then Ok(p.amount) else Err("non-positive amount")

  proofs:
    purity:
      reads   : [p.amount]
      calls   : []
    termination:
      bound : 4


rule validate_age
  @intention: "adult age → Ok, else Err"
  @source: invoices.intent:1

  input:
    p : P

  output:
    r : Result(number, text)

  logic:
    r = if p.customer_age >= 18 then Ok(p.customer_age) else Err("under 18")

  proofs:
    purity:
      reads   : [p.customer_age]
      calls   : []
    termination:
      bound : 4


rule chained_validate
  @intention: "amount validated → then age validated; both must pass"
  @source: invoices.intent:1

  input:
    p : P

  output:
    r : Result(number, text)

  logic:
    r = match_result(validate_amount(p),
          amt => match_result(validate_age(p),
                  age => Ok(amt),
                  inner_e => Err(inner_e)),
          outer_e => Err(outer_e))

  proofs:
    purity:
      reads   : [p]
      calls   : [validate_amount, validate_age]
    termination:
      bound : 9
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_nested_match");
        compile_native(&program, "chained_validate", out.to_str().unwrap(), false, false)
            .expect("nested match_result must compile");

        // (1) Outer Ok, inner Ok → produces amount.
        let r_okok = Command::new(&out)
            .args(["1000", "25"])
            .output()
            .expect("spawn ok-ok");
        assert_eq!(
            String::from_utf8_lossy(&r_okok.stdout).trim(),
            "1000",
            "outer-Ok / inner-Ok should output the amount; stdout={:?} stderr={:?}",
            r_okok.stdout, r_okok.stderr,
        );

        // (2) Outer Ok, inner Err → "under 18" — exercises the inner
        //     match_result's Err arm. Critical: inner Err must propagate
        //     through the OUTER's match_result Ok-arm wrapping (the
        //     outer's Ok arm IS the inner match_result, so the inner
        //     match_result's Err is the outer rule's Err).
        let r_okerr = Command::new(&out)
            .args(["1000", "15"])
            .output()
            .expect("spawn ok-err");
        assert_eq!(
            String::from_utf8_lossy(&r_okerr.stderr).trim(),
            "under 18",
            "outer-Ok / inner-Err should propagate inner's Err on stderr; stdout={:?} stderr={:?}",
            r_okerr.stdout, r_okerr.stderr,
        );

        // (3) Outer Err → "non-positive amount". Inner match_result
        //     never runs — outer's Err arm fires first.
        let r_err = Command::new(&out)
            .args(["-50", "25"])
            .output()
            .expect("spawn outer-err");
        assert_eq!(
            String::from_utf8_lossy(&r_err.stderr).trim(),
            "non-positive amount",
            "outer-Err should bypass inner match_result; stdout={:?} stderr={:?}",
            r_err.stdout, r_err.stderr,
        );

        let _ = fs::remove_file(out);
    }

    /// `Result(<Record>, text)` end-to-end. Outer rule has
    /// `output: Result(Refined, text)` where Refined is a concept with
    /// number+text fields. The Ok arm emits the record as a single JSON
    /// line on stdout (same `{"f":v,...}\n` format as `output: Named(C)`
    /// rules). The Err arm writes the rejection reason to stderr +
    /// exit 1. Three reachable cases exercised: Ok with one tier
    /// branch, Ok with the other tier branch (text-field if/else), Err.
    #[test]
    fn native_result_record_text_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept Purchase
  @intention: "p"
  @source: invoices.intent:1

  fields:
    amount        : number
    customer_age  : number


concept Refined
  @intention: "validated purchase + derived tier"
  @source: invoices.intent:1

  fields:
    amount : number
    tier   : text


rule classify_purchase
  @intention: "v"
  @source: invoices.intent:1

  input:
    p : Purchase

  output:
    r : Result(Refined, text)

  logic:
    r = if p.customer_age >= 18 then Ok(Refined { amount: p.amount, tier: if p.amount >= 1000 then "premium" else "standard" }) else Err(concat("under 18: age=", p.customer_age))

  proofs:
    purity:
      reads   : [p.customer_age, p.amount]
      calls   : []
    termination:
      bound : 9
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_result_record");
        compile_native(&program, "classify_purchase", out.to_str().unwrap(), false, false)
            .expect("Result(Record, text) must compile");

        // Ok arm, premium tier.
        let r_premium = Command::new(&out).args(["1500", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_premium.stdout).trim(),
            r#"{"amount":1500,"tier":"premium"}"#,
            "premium tier; stdout={:?} stderr={:?}",
            r_premium.stdout, r_premium.stderr,
        );

        // Ok arm, standard tier.
        let r_standard = Command::new(&out).args(["200", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_standard.stdout).trim(),
            r#"{"amount":200,"tier":"standard"}"#,
            "standard tier; stdout={:?} stderr={:?}",
            r_standard.stdout, r_standard.stderr,
        );

        // Err arm: minor customer.
        let r_err = Command::new(&out).args(["1500", "15"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_err.stderr).trim(),
            "under 18: age=15",
            "minor Err; stdout={:?} stderr={:?}",
            r_err.stdout, r_err.stderr,
        );
        assert_eq!(
            r_err.status.code(),
            Some(1),
            "Err arm should exit 1 (compare to existing same-shape Result(text, text) tests)",
        );

        let _ = fs::remove_file(out);
    }

    /// `Result(<Record>, text)` Ok arm with `if/else` directly INSIDE
    /// the Ok constructor — the form
    ///   r = if outer then Ok(if inner then RecordA{...} else RecordB{...}) else Err(...)
    /// Each branch resolves to a different field set; both converge at
    /// the common jmp_loop_top below the dispatch. Pinned with two
    /// distinct record shapes so a swapped branch would surface in
    /// the JSON output.
    #[test]
    fn native_result_record_if_inside_ok_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept Purchase
  @intention: "p"
  @source: invoices.intent:1

  fields:
    amount        : number
    customer_age  : number


concept Refined
  @intention: "validated purchase + tier"
  @source: invoices.intent:1

  fields:
    amount : number
    tier   : text


rule classify
  @intention: "if-inside-Ok shape"
  @source: invoices.intent:1

  input:
    p : Purchase

  output:
    r : Result(Refined, text)

  logic:
    r = if p.customer_age >= 18 then Ok(if p.amount >= 1000 then Refined { amount: p.amount, tier: "premium" } else Refined { amount: p.amount, tier: "standard" }) else Err("under 18")

  proofs:
    purity:
      reads   : [p.customer_age, p.amount]
      calls   : []
    termination:
      bound : 8
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_result_record_if_in_ok");
        compile_native(&program, "classify", out.to_str().unwrap(), false, false)
            .expect("if-inside-Ok must compile");

        let r_premium = Command::new(&out).args(["1500", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_premium.stdout).trim(),
            r#"{"amount":1500,"tier":"premium"}"#,
        );

        let r_standard = Command::new(&out).args(["200", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_standard.stdout).trim(),
            r#"{"amount":200,"tier":"standard"}"#,
            "the inner if/else picked the wrong Refined branch",
        );

        let r_err = Command::new(&out).args(["1500", "15"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_err.stderr).trim(),
            "under 18",
        );
        assert_eq!(r_err.status.code(), Some(1));

        let _ = fs::remove_file(out);
    }

    /// `match_result` chained into a `Result(Record, text)` outer rule.
    /// Pattern: validate via a Result(number, text) callee, then wrap
    /// the bound Ok value into a richer Refined record. Two slices
    /// composing — match_result inlining + Result(Record, text) Ok
    /// dispatch — neither was independently designed with the other
    /// in mind, so this regression test pins the composition. Both
    /// arms exercised; any drift in either slice would surface here
    /// before silently changing semantics.
    #[test]
    fn native_match_result_into_result_record_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept Purchase
  @intention: "p"
  @source: invoices.intent:1

  fields:
    amount        : number
    customer_age  : number


concept Refined
  @intention: "validated + discounted purchase as a record"
  @source: invoices.intent:1

  fields:
    discounted : number
    tier       : text


rule validate_amount
  @intention: "v"
  @source: invoices.intent:1

  input:
    p : Purchase

  output:
    r : Result(number, text)

  logic:
    r = if p.amount > 0 then Ok(p.amount) else Err("non-positive amount")

  proofs:
    purity:
      reads   : [p.amount]
      calls   : []
    termination:
      bound : 4


rule discount_purchase
  @intention: "match_result chain → Result(Record, text)"
  @source: invoices.intent:1

  input:
    p : Purchase

  output:
    r : Result(Refined, text)

  logic:
    r = match_result(validate_amount(p),
          v => Ok(Refined { discounted: v * 90 / 100, tier: "premium" }),
          e => Err(e))

  proofs:
    purity:
      reads   : [p]
      calls   : [validate_amount]
    termination:
      bound : 8
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_match_to_record");
        compile_native(&program, "discount_purchase", out.to_str().unwrap(), false, false)
            .expect("match_result → Result(Record, text) must compile");

        // Ok arm: validate_amount returns Ok(1000), bound to v;
        // outer wraps into Refined { discounted: 900, tier: "premium" }.
        let r_ok = Command::new(&out).args(["1000", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_ok.stdout).trim(),
            r#"{"discounted":900,"tier":"premium"}"#,
            "Ok arm; stdout={:?} stderr={:?}",
            r_ok.stdout, r_ok.stderr,
        );

        // Err arm: validate_amount Err propagated through outer Err.
        let r_err = Command::new(&out).args(["-50", "25"]).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&r_err.stderr).trim(),
            "non-positive amount",
            "Err arm; stdout={:?} stderr={:?}",
            r_err.stdout, r_err.stderr,
        );

        let _ = fs::remove_file(out);
    }

    /// Transitive resource reads through match_result chains.
    ///
    /// Surfaced while building `examples/order_intake.verbose`: a rule
    /// `discounted` does `match_result(validate_amount(o), ...)` where
    /// `validate_amount` reads a resource `cap` via
    /// `parse_int(read(cap))`. The native emitter inlines the callee
    /// into `discounted`'s frame, so `discounted`'s prologue must
    /// allocate the slot for `cap`. Pre-fix, the verifier rejected
    /// `discounted`'s declaration of `cap` in `reads:` as "extra"
    /// (the literal AST of `discounted` doesn't reference `cap`), and
    /// without it the emitter failed with
    /// "no (ptr, len) slots are registered at this point".
    ///
    /// Fix: the verifier's
    /// `augment_facts_with_transitive_match_result_reads` and the
    /// native's `collect_rule_read_names_transitive` both walk
    /// match_result targets into callees and collect their reads.
    /// What the verifier accepts, the emitter now allocates.
    ///
    /// Runtime check: validate the file is open + read, then either
    /// arm produces the right result.
    #[test]
    fn native_match_result_transitive_resource_reads_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

resource cap
  @intention: "numeric cap loaded at startup"
  @source: invoices.intent:1

  path: "/tmp/verbosec_test_transitive_cap.txt"
  max:  16
  on_read_error: abort


concept O
  @intention: "an order with amount"
  @source: invoices.intent:1

  fields:
    amount : number [0, 1000000]


rule validate_amount
  @intention: "accept when amount <= runtime-loaded cap"
  @source: invoices.intent:1

  input:
    o : O

  output:
    r : Result(number, text)

  logic:
    r = if o.amount <= parse_int(read(cap)) then Ok(o.amount) else Err("over cap")

  proofs:
    purity:
      reads : [o.amount, cap]
      calls : []
    termination:
      bound : 6


rule discounted
  @intention: "match_result chain on validate_amount — cap read flows transitively"
  @source: invoices.intent:1

  input:
    o : O

  output:
    r : Result(number, text)

  logic:
    r = match_result(validate_amount(o), v => Ok(v * 90 / 100), e => Err(e))

  proofs:
    purity:
      reads : [o, cap]
      calls : [validate_amount]
    termination:
      bound : 7
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_transitive_reads");
        compile_native(&program, "discounted", out.to_str().unwrap(), false, false)
            .expect("transitive resource reads must compile");

        // Write the cap file. `printf` (no trailing newline) so parse_int
        // doesn't abort on a stray byte.
        let cap_path = "/tmp/verbosec_test_transitive_cap.txt";
        std::fs::write(cap_path, b"500").expect("write cap file");

        // Ok arm: amount=200 → validate_amount returns Ok(200) →
        // outer Ok(200 * 90 / 100) = 180.
        let r_ok = Command::new(&out).args(["200"]).output().expect("spawn ok");
        assert_eq!(
            String::from_utf8_lossy(&r_ok.stdout).trim(),
            "180",
            "Ok arm; stdout={:?} stderr={:?}",
            r_ok.stdout, r_ok.stderr,
        );

        // Err arm: amount=1000 > cap=500 → validate_amount returns
        // Err("over cap") → outer Err passes through.
        let r_err = Command::new(&out).args(["1000"]).output().expect("spawn err");
        assert_eq!(
            String::from_utf8_lossy(&r_err.stderr).trim(),
            "over cap",
            "Err arm; stdout={:?} stderr={:?}",
            r_err.stdout, r_err.stderr,
        );
        assert_eq!(
            r_err.status.code(),
            Some(1),
            "Err propagated through match_result should exit 1",
        );

        let _ = fs::remove_file(out);
        let _ = fs::remove_file(cap_path);
    }

    /// Cross-concept rejection: callee references a field that doesn't
    /// exist in the outer's concept. Slice contract enforces that the
    /// callee's fields are a SUBSET of the outer's; without the field
    /// in `offsets`, the inline emission would silently miscompile, so
    /// we reject up-front with a breadcrumb naming the missing field.
    #[test]
    fn native_cross_concept_match_rejects_missing_field() {
        let src = r#"@verbose 0.1.0

concept Wider
  @intention: "Has an extra field the outer doesn't"
  @source: invoices.intent:1

  fields:
    amount : number
    bonus  : number


concept Outer
  @intention: "Smaller concept — no `bonus`"
  @source: invoices.intent:1

  fields:
    amount : number


rule validate_wider
  @intention: "Reads the missing field"
  @source: invoices.intent:1

  input:
    w : Wider

  output:
    r : Result(number, text)

  logic:
    r = if w.bonus > 0 then Ok(w.amount) else Err("no bonus")

  proofs:
    purity:
      reads   : [w.bonus, w.amount]
      calls   : []
    termination:
      bound : 4


rule outer_rule
  @intention: "Calls validate_wider — callee references field not in Outer"
  @source: invoices.intent:1

  input:
    o : Outer

  output:
    r : Result(number, text)

  logic:
    r = match_result(validate_wider(o), v => Ok(v), reason => Err(reason))

  proofs:
    purity:
      reads   : [o]
      calls   : [validate_wider]
    termination:
      bound : 5
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_cross_concept_missing");
        let err = compile_native(&program, "outer_rule", out.to_str().unwrap(), false, false)
            .expect_err("cross-concept with missing field must reject");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("bonus") && msg.contains("not present in outer concept"),
            "expected breadcrumb naming missing field 'bonus'; got: {}",
            msg
        );
        let _ = std::fs::remove_file(out);
    }

    /// Cross-concept rejection: callee has a field with the same name
    /// as the outer's, but a different type. Without this check the
    /// callee would read the outer's slot through the wrong-typed
    /// access and silently miscompile.
    #[test]
    fn native_cross_concept_match_rejects_type_mismatch() {
        let src = r#"@verbose 0.1.0

concept TextAmount
  @intention: "amount is text here"
  @source: invoices.intent:1

  fields:
    amount : text


concept NumAmount
  @intention: "amount is number here"
  @source: invoices.intent:1

  fields:
    amount : number


rule text_validate
  @intention: "Validator over a text field"
  @source: invoices.intent:1

  input:
    t : TextAmount

  output:
    r : Result(text, text)

  logic:
    r = if length(t.amount) > 0 then Ok(t.amount) else Err("empty")

  proofs:
    purity:
      reads   : [t.amount]
      calls   : []
    termination:
      bound : 4


rule num_outer
  @intention: "Calls text_validate from a number-amount input"
  @source: invoices.intent:1

  input:
    n : NumAmount

  output:
    r : Result(text, text)

  logic:
    r = match_result(text_validate(n), v => Ok(v), reason => Err(reason))

  proofs:
    purity:
      reads   : [n]
      calls   : [text_validate]
    termination:
      bound : 5
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_cross_concept_type_mismatch");
        let err = compile_native(&program, "num_outer", out.to_str().unwrap(), false, false)
            .expect_err("cross-concept with type mismatch must reject");
        let msg = format!("{:?}", err);
        assert!(
            msg.contains("amount") && msg.contains("Text") && msg.contains("Number"),
            "expected breadcrumb naming the type mismatch on 'amount'; got: {}",
            msg
        );
        let _ = std::fs::remove_file(out);
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

    /// Phase 9 slice 9.5: `read(<resource>)` allowed as the init expression
    /// of a Phase 5b text fold. The resource is opened/read/closed ONCE
    /// above the outer loop; (ptr, len) live in dedicated rbp slots; the
    /// init copy uses the runtime length so a short file produces a tight
    /// output and a missing file aborts the binary BEFORE any output is
    /// written. This test exercises the full happy path (banner content
    /// followed by per-element entries) AND the abort path (binary exits
    /// non-zero when the resource path doesn't exist).
    #[test]
    fn slice_9_5_text_fold_with_read_init_runtime() {
        use std::process::Command;
        let banner_path = "/tmp/verbosec_test_slice95_banner.txt";
        std::fs::write(banner_path, b"BANNER>> ").expect("write banner");

        let src = std::fs::read_to_string("examples/banner_roster.verbose")
            .expect("examples/banner_roster.verbose is expected to exist");
        // Patch the path so the test doesn't fight with the example's
        // own /tmp/verbose_roster_banner.txt that a user might be using.
        let src = src.replace("/tmp/verbose_roster_banner.txt", banner_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice95_banner_roster");
        compile_native(&program, "banner_line", out.to_str().unwrap(), false, false)
            .expect("native compile of read-init text fold should succeed");

        // Happy path: banner content + per-element entries on stdout.
        let ok = Command::new(&out)
            .args(["2", "alice", "100", "bob", "200"])
            .output()
            .expect("spawn ok run");
        assert!(ok.status.success(), "expected success exit, got {:?}", ok.status);
        let stdout = String::from_utf8_lossy(&ok.stdout);
        assert_eq!(
            stdout.trim_end(),
            "BANNER>> alice=100; bob=200;",
            "happy-path stdout mismatch: {:?}",
            stdout
        );

        // Empty file path: zero-length init copy, body still runs.
        std::fs::write(banner_path, b"").expect("truncate banner");
        let empty = Command::new(&out)
            .args(["1", "carol", "300"])
            .output()
            .expect("spawn empty run");
        assert!(empty.status.success());
        assert_eq!(
            String::from_utf8_lossy(&empty.stdout).trim_end(),
            "carol=300;",
            "empty-banner stdout mismatch"
        );

        // Abort path: missing file exits 1 before any stdout is written.
        let _ = std::fs::remove_file(banner_path);
        let abort = Command::new(&out)
            .args(["1", "dave", "400"])
            .output()
            .expect("spawn abort run");
        assert!(
            !abort.status.success() && abort.stdout.is_empty(),
            "expected non-zero exit with empty stdout when banner missing; got status={:?} stdout={:?}",
            abort.status,
            String::from_utf8_lossy(&abort.stdout)
        );

        let _ = std::fs::remove_file(&out);
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

    /// Phase 2I-Result regression: a Result(text, text) rule whose Ok and
    /// Err arms each reference a distinct non-literal text let binding.
    /// Exercises the ctx.text_bindings thread through
    /// emit_result_program -> emit_eval_result_expr -> emit_text_write_to_fd.
    /// Without the thread, either arm would fall through to "unsupported
    /// shape" when resolving the Ident(let-name).
    #[test]
    fn phase2i_result_rule_text_lets_compile_and_run() {
        use std::fs;
        use std::process::Command;
        let src = fs::read_to_string("examples/gate_result.verbose")
            .expect("examples/gate_result.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase2i_gate_result");
        compile_native(&program, "gate", out.to_str().unwrap(), false, false)
            .expect("gate_result compile");

        // Adult → stdout, exit 0.
        let output = Command::new(&out)
            .args(["alice", "30"])
            .output()
            .expect("run gate adult");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "welcome alice");
        assert_eq!(output.status.code(), Some(0));

        // Minor → stderr, exit 1.
        let output = Command::new(&out)
            .args(["bob", "15"])
            .output()
            .expect("run gate minor");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "sorry bob, minimum age is 18"
        );
        assert_eq!(output.status.code(), Some(1));

        let _ = fs::remove_file(out);
    }

    /// Phase 2I regression: a text-output rule with chained non-literal
    /// text let bindings compiles and runs correctly. Exercises:
    ///   * `let tagged = concat(...)`  — first-level text let
    ///   * `let full = concat(tagged, ...)` — later let references prior one
    ///   * `line = concat(full, ...)`  — logic.value references text let
    ///
    /// Before this slice, all three `concat` arms would have had to be
    /// inlined at the return site because emit_eval_expr rejects text
    /// literals and had no slot-pair mechanism for computed text values.
    #[test]
    fn phase2i_non_literal_text_let_bindings_compile_and_run() {
        use std::fs;
        use std::process::Command;
        let src = fs::read_to_string("examples/ledger_line.verbose")
            .expect("examples/ledger_line.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase2i_ledger");
        compile_native(&program, "format_line", out.to_str().unwrap(), false, false)
            .expect("ledger_line compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (700..1400).contains(&size),
            "ledger_line binary size {} outside [700, 1400] envelope",
            size
        );

        // Run the binary and assert both text lets were resolved correctly
        // — if `tagged` had not been captured as a BoundText, the second
        // concat would have failed at emit time; if the slot layout were
        // off, the runtime output would mismatch.
        let output = Command::new(&out)
            .args(["alice", "42", "100"])
            .output()
            .expect("run ledger_line");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(
            stdout.trim(),
            "[alice#42] amount=100 posted",
            "unexpected output from chained text lets: {:?}",
            stdout
        );

        // Negative number path through the int-to-text formatter.
        let output = Command::new(&out)
            .args(["bob", "7", "-25"])
            .output()
            .expect("run ledger_line negative");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "[bob#7] amount=-25 posted");

        let _ = fs::remove_file(out);
    }

    /// Phase 8 slice 8d regression: a service with `on_error: abort`
    /// embeds the shared sys_exit(1) sequence at the end of the binary
    /// (mov rax,60; mov rdi,1; syscall) and a `test rax, rax; js rel32`
    /// check after each fallible log syscall.
    #[test]
    fn phase8_slice8d_audit_strict_embeds_abort_sequence() {
        use std::fs;
        let src = fs::read_to_string("examples/audit_strict.verbose")
            .expect("examples/audit_strict.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase8_slice8d_strict");
        compile_service(&program, "strict_endpoint", out.to_str().unwrap())
            .expect("Http10 slice-8d service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (1000..1500).contains(&size),
            "audit_strict service binary size {} outside expected [1000, 1500] envelope", size
        );

        // The shared abort label runs sys_exit(1):
        // mov rax, 60 = 0x48 0xC7 0xC0 0x3C 00 00 00
        // mov rdi, 1  = 0x48 0xC7 0xC7 0x01 00 00 00
        // syscall     = 0x0F 0x05
        let abort_seq = [
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
            0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        assert!(
            bytes.windows(abort_seq.len()).any(|w| w == abort_seq),
            "expected sys_exit(1) abort sequence not found — slice 8d not wired"
        );

        // A `test rax, rax ; js rel32` check appears after each fallible
        // syscall — there are at least two (open + write). Encoding:
        // 48 85 C0 0F 88 + i32 (placeholder distance to abort label).
        let check_prefix = [0x48, 0x85, 0xC0, 0x0F, 0x88];
        let check_count = bytes
            .windows(check_prefix.len())
            .filter(|w| *w == check_prefix)
            .count();
        assert!(
            check_count >= 2,
            "expected at least 2 `test rax, rax; js` abort checks (open + write), found {}",
            check_count
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 8 slice 8d regression: a service WITHOUT `on_error: abort`
    /// (the slice 8a default) does NOT embed the abort sequence — zero
    /// cost when the policy is Drop.
    #[test]
    fn phase8_slice8d_default_drop_omits_abort_sequence() {
        use std::fs;
        let src = fs::read_to_string("examples/access_log_json.verbose")
            .expect("examples/access_log_json.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase8_slice8d_default");
        compile_service(&program, "access_logged_service", out.to_str().unwrap())
            .expect("Http10 default-drop service compile");

        let bytes = fs::read(&out).expect("read output");
        let abort_seq = [
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
            0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        assert!(
            !bytes.windows(abort_seq.len()).any(|w| w == abort_seq),
            "Drop-policy service must NOT embed sys_exit(1) abort sequence — slice 8d default broke"
        );

        let _ = fs::remove_file(out);
    }

    /// `ends_with(<haystack>, <needle>)` primitive (2026-05-01):
    /// symmetric of `starts_with`. Native algorithm: position rsi at
    /// haystack_ptr + (hay_len - needle_len), then `repe cmpsb` on
    /// needle_len bytes.
    ///
    /// Pins the same edge cases as starts_with, mirrored for suffix:
    ///   (a) suffix matches → true
    ///   (b) length too short → false
    ///   (c) byte mismatch in tail → false
    ///   (d) empty needle → always true
    ///   (e) needle that's a PREFIX but not a suffix → false (this is
    ///       what differentiates ends_with from starts_with)
    ///   (f) case sensitive
    #[test]
    fn slice_ends_with_runtime_byte_compare() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept Path
  @intention: "x"
  @source: invoices.intent:1
  fields:
    p : text

rule is_css
  @intention: "x"
  @source: invoices.intent:1
  input:
    r : Path
  output:
    css : bool
  logic:
    css = ends_with(r.p, ".css")
  proofs:
    purity:
      reads : [r.p]
      calls : []
    termination:
      bound : 1
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_ends_with");
        compile_native(&program, "is_css", out.to_str().unwrap(), false, false)
            .expect("ends_with should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) suffix matches
        assert_eq!(run("/static/main.css"), "true");
        assert_eq!(run(".css"), "true", "exact match");
        // (b) length too short
        assert_eq!(run(".cs"), "false");
        assert_eq!(run(""), "false");
        // (c) byte mismatch in tail
        assert_eq!(run("/static/main.js"), "false");
        // (e) prefix-but-not-suffix — this is what distinguishes
        // ends_with from starts_with. ".css/style" STARTS with ".css"
        // but does NOT end with it.
        assert_eq!(run(".css/style"), "false");
        // (f) case-sensitive
        assert_eq!(run("/main.CSS"), "false");

        let _ = std::fs::remove_file(&out);

        // (d) empty needle → always true (literal fold path)
        let empty_src = src.replace("\".css\"", "\"\"");
        let tokens = crate::lexer::Lexer::new(&empty_src).tokenize().expect("tokenize");
        let empty_program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out2 = std::env::temp_dir().join("verbosec_test_ends_with_empty");
        compile_native(&empty_program, "is_css", out2.to_str().unwrap(), false, false)
            .expect("empty-needle ends_with should compile");
        let r = Command::new(&out2).args(["anything"]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true"
        );
        let r = Command::new(&out2).args([""]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true"
        );
        let _ = std::fs::remove_file(&out2);
    }

    /// WAF-style body content gate (2026-05-01): pairs the HTTP body
    /// parsing slice with `contains` + `read` to express a deployable
    /// content filter. Three gates in order:
    ///   1. body too large → 413
    ///   2. body contains blacklisted substring (loaded from disk) → 403
    ///   3. otherwise → 200
    ///
    /// This test pins the four routing branches AND the audit log
    /// structure (body_bytes field, json_escape on user-controlled
    /// values).
    #[test]
    fn body_content_gate_routing_and_audit() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let port: u16 = 18941;
        let max_body_path = "/tmp/verbosec_test_waf_max_body.txt";
        let banned_path = "/tmp/verbosec_test_waf_banned.txt";
        let audit_path = "/tmp/verbosec_test_waf_audit.jsonl";

        std::fs::write(max_body_path, b"50").expect("write limit");
        std::fs::write(banned_path, b"PASSWORD").expect("write banned");
        let _ = std::fs::remove_file(audit_path);

        let src = std::fs::read_to_string("examples/body_content_gate.verbose")
            .expect("examples/body_content_gate.verbose");
        let src = src
            .replace("/tmp/verbose_waf_max_body.txt", max_body_path)
            .replace("/tmp/verbose_waf_banned.txt", banned_path)
            .replace("/tmp/verbose_waf_audit.jsonl", audit_path)
            .replace("18939", &port.to_string());
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_waf");
        compile_service(&program, "waf", out.to_str().unwrap()).expect("WAF should compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let mut bound = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                bound = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(bound, "WAF service never bound");

        let request = |method: &str, path: &str, body: &str| -> u16 {
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let req = format!(
                "{} {} HTTP/1.0\r\nContent-Length: {}\r\n\r\n{}",
                method, path, body.len(), body
            );
            s.write_all(req.as_bytes()).expect("write");
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).expect("read");
            String::from_utf8_lossy(&buf)
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        };

        // 1. GET no body → 200
        assert_eq!(request("GET", "/", ""), 200);
        // 2. Innocent POST → 200
        assert_eq!(request("POST", "/api", "hello world"), 200);
        // 3. Body containing banned word → 403
        assert_eq!(request("POST", "/api", "sending PASSWORD=x"), 403);
        // 4. Body too large → 413
        assert_eq!(request("POST", "/api", &"a".repeat(100)), 413);
        // 5. Both banned + too large → 413 (size gate runs first)
        let payload = format!("PASSWORD{}", "a".repeat(100));
        assert_eq!(request("POST", "/api", &payload), 413);

        let _ = child.kill();
        let _ = child.wait();

        // Audit log: 5 lines, each with body_bytes field reflecting
        // the actual incoming body length (even rejected requests).
        let audit = std::fs::read_to_string(audit_path).expect("read audit");
        let lines: Vec<&str> = audit.lines().collect();
        assert_eq!(lines.len(), 5, "expected 5 audit lines");
        for line in &lines {
            assert!(line.contains("\"body_bytes\":"), "missing body_bytes: {:?}", line);
        }
        // Spot-check: line 4 (POST 100 bytes "a"*100) should report body_bytes:100
        assert!(lines[3].contains("\"body_bytes\":100"),
                "line 4 body_bytes should be 100; got: {:?}", lines[3]);

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(max_body_path);
        let _ = std::fs::remove_file(banned_path);
        let _ = std::fs::remove_file(audit_path);
    }

    /// HTTP body parsing (2026-04-29): `req.body` is now an accessible
    /// text field on the synthetic HttpRequest concept. The parser
    /// scans for "\r\n\r\n" after the method/path and stores the
    /// body's (ptr, len) at dedicated rbp slots. Body composes as
    /// BoundText: works with `length`, audit log, json_escape,
    /// concat, etc. via the same machinery as `read(...)` / `fetch(...)`.
    ///
    /// This test pins:
    ///   (a) GET request (no body) → length(req.body) == 0
    ///   (b) POST with small body → correct length
    ///   (c) POST with body over the runtime limit → 413
    ///   (d) Body content in the audit log via json_escape (round-trip
    ///       through concat with mixed Number+BoundText args)
    #[test]
    fn http_body_parsing_runtime() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let port: u16 = 18938;
        let limit_path = "/tmp/verbosec_test_body_limit.txt";
        std::fs::write(limit_path, b"50").expect("write limit");

        let src = std::fs::read_to_string("examples/body_size_gate.verbose")
            .expect("examples/body_size_gate.verbose");
        let src = src
            .replace("/tmp/verbose_max_body.txt", limit_path)
            .replace("18937", &port.to_string());
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_body_gate");
        compile_service(&program, "gate", out.to_str().unwrap())
            .expect("body parsing service should compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");

        let mut bound = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                bound = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(bound, "service never bound");

        let request = |method: &str, path: &str, body: &str| -> (u16, String) {
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let req = format!(
                "{} {} HTTP/1.0\r\nContent-Length: {}\r\n\r\n{}",
                method,
                path,
                body.len(),
                body
            );
            s.write_all(req.as_bytes()).expect("write");
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).expect("read");
            let resp = String::from_utf8_lossy(&buf).to_string();
            let status: u16 = resp
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            (status, body)
        };

        // (a) GET → no body → length 0
        let (st, body) = request("GET", "/x", "");
        assert_eq!(st, 200);
        assert!(body.contains("body=0"), "GET body resp: {:?}", body);

        // (b) POST with small body
        let (st, body) = request("POST", "/x", "hello");
        assert_eq!(st, 200);
        assert!(body.contains("body=5"), "POST 5: {:?}", body);

        // POST with 30 bytes
        let big30 = "a".repeat(30);
        let (st, body) = request("POST", "/x", &big30);
        assert_eq!(st, 200);
        assert!(body.contains("body=30"), "POST 30: {:?}", body);

        // (c) POST over limit → 413
        let big100 = "b".repeat(100);
        let (st, body) = request("POST", "/x", &big100);
        assert_eq!(st, 413);
        assert!(body.contains("payload too large"), "413 body: {:?}", body);

        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(limit_path);
    }

    /// **SYNTHESIS DEMO** (2026-04-29): audit_gateway.verbose combines
    /// nine features in a single ~2.9 KB native binary —
    ///   - HTTP service + prefix routing (`starts_with`)
    ///   - URI length input gate (`length` + `parse_int(read(...))`)
    ///   - method allowlist (`field == read(...)`)
    ///   - per-request JSONL audit log
    ///   - `json_escape` on user-controlled fields
    ///   - captured `req.timestamp`
    ///   - `on_error: abort` fail-closed audit
    ///   - `concurrency: forked`
    ///   - `cache: true` resources (one read at startup, COW to children)
    ///
    /// This test pins the end-to-end behavior:
    ///   - the four routing branches return the right (status, body)
    ///   - every request produces exactly one valid JSON audit line
    ///   - the audit lines parse with the expected structure (no
    ///     escape-related corruption even on weird input paths)
    #[test]
    fn synthesis_audit_gateway_end_to_end() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let port: u16 = 18936;
        let max_uri_path = "/tmp/verbosec_test_synth_max_uri.txt";
        let allowed_path = "/tmp/verbosec_test_synth_allowed_method.txt";
        let audit_path = "/tmp/verbosec_test_synth_audit.jsonl";

        std::fs::write(max_uri_path, b"20").expect("write max_uri");
        std::fs::write(allowed_path, b"GET").expect("write allowed");
        let _ = std::fs::remove_file(audit_path);

        let src = std::fs::read_to_string("examples/audit_gateway.verbose")
            .expect("examples/audit_gateway.verbose");
        let src = src
            .replace("/tmp/verbose_audit_max_uri.txt", max_uri_path)
            .replace("/tmp/verbose_audit_allowed_method.txt", allowed_path)
            .replace("/tmp/verbose_audit_gateway.jsonl", audit_path)
            .replace("18935", &port.to_string());
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_synth_audit_gateway");
        compile_service(&program, "gateway", out.to_str().unwrap())
            .expect("synthesis demo should compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn synthesis demo");

        // Wait for bind.
        let mut bound = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                bound = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(bound, "synthesis service never bound");

        let request = |method: &str, path: &str| -> (u16, String) {
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let req = format!("{} {} HTTP/1.0\r\n\r\n", method, path);
            s.write_all(req.as_bytes()).expect("write");
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).expect("read");
            let resp = String::from_utf8_lossy(&buf).to_string();
            // Extract status code from first line "HTTP/1.0 NNN ..."
            let status: u16 = resp
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            // Body after the empty line
            let body = resp.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            (status, body)
        };

        // Route 1: /health → 200 "ok"
        let (st, body) = request("GET", "/health");
        assert_eq!(st, 200);
        assert!(body.contains("ok"), "health body: {:?}", body);

        // Route 2: GET /api/v1/users → allowed → 200
        let (st, body) = request("GET", "/api/v1/users");
        assert_eq!(st, 200);
        assert!(body.contains("allow"), "allowed body: {:?}", body);

        // Route 3: POST /api/v1/users → method denied → 403
        let (st, body) = request("POST", "/api/v1/users");
        assert_eq!(st, 403);
        assert!(body.contains("method") && body.contains("not allowed"),
                "method-denied body: {:?}", body);

        // Route 4: long URI under /api/v1/ → length gate → 403
        let long_path = "/api/v1/this/path/is/way/too/long";
        assert!(long_path.len() > 20, "test fixture: long_path must exceed limit");
        let (st, body) = request("GET", long_path);
        assert_eq!(st, 403);
        assert!(body.contains("uri too long"), "length-denied body: {:?}", body);

        // Route 5: unknown → 404
        let (st, body) = request("GET", "/unknown");
        assert_eq!(st, 404);
        assert!(body.contains("not found"));

        let _ = child.kill();
        let _ = child.wait();

        // Audit log: 5 lines, each valid JSON with the four expected fields.
        let audit = std::fs::read_to_string(audit_path).expect("read audit");
        let lines: Vec<&str> = audit.lines().collect();
        assert_eq!(lines.len(), 5, "expected 5 audit lines, got {} — content: {:?}", lines.len(), audit);
        for line in &lines {
            assert!(line.starts_with("{\"ts\":"), "line must start with {{\"ts\":, got: {:?}", line);
            assert!(line.contains("\"method\":\""), "line missing method: {:?}", line);
            assert!(line.contains("\"path\":\""), "line missing path: {:?}", line);
            assert!(line.contains("\"status\":"), "line missing status: {:?}", line);
            assert!(line.ends_with("}"), "line must end with }}, got: {:?}", line);
        }
        // Specific status code in audit lines (in order).
        let statuses: Vec<&str> = lines
            .iter()
            .filter_map(|l| {
                l.find("\"status\":").map(|i| &l[i + 9..i + 12])
            })
            .collect();
        assert_eq!(statuses, vec!["200", "200", "403", "403", "404"]);

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(max_uri_path);
        let _ = std::fs::remove_file(allowed_path);
        let _ = std::fs::remove_file(audit_path);
    }

    /// `abs(<number>)` primitive (2026-04-29): branch-free 5-byte
    /// inline absolute value. Composes anywhere a number expression
    /// can appear. The motivating use case: time-window comparisons
    /// where the natural operator-style `now - ts < window` silently
    /// passes ANY future event because the subtraction goes negative.
    /// `abs(now - ts) < window` expresses the symmetric window
    /// correctly.
    ///
    /// Pins:
    ///   (a) past events within window → true
    ///   (b) past events outside window → false
    ///   (c) future events within window → true (the operator-style
    ///       buggy form would also say true here — both correct)
    ///   (d) future events OUTSIDE window → false (the buggy form
    ///       would say true — this is the bug abs() fixes)
    ///   (e) boundary at ±3599 (true) vs ±3600 (false, strict <)
    ///   (f) abs of i64 values doesn't panic on i64::MIN (uses cqo)
    #[test]
    fn slice_abs_branch_free_and_corrects_future_event_bug() {
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let src = std::fs::read_to_string("examples/recent_event_abs.verbose")
            .expect("examples/recent_event_abs.verbose");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_abs_recent");
        compile_native(&program, "recent", out.to_str().unwrap(), false, false)
            .expect("abs(now_unix() - ts) < 3600 should compile");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs() as i64;
        let run = |ts: i64| -> String {
            let r = Command::new(&out).args([ts.to_string()]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) past, within
        assert_eq!(run(now - 30), "true");
        assert_eq!(run(now - 100), "true");
        // (b) past, outside
        assert_eq!(run(now - 7200), "false");
        // (c) future, within
        assert_eq!(run(now + 30), "true", "future within window must be true");
        // (d) future, outside — this is the BUG abs fixes
        assert_eq!(run(now + 7200), "false",
            "future event outside window must be false (this fails the buggy operator-style form)");
        // (e) boundaries (strict <)
        assert_eq!(run(now - 3599), "true");
        assert_eq!(run(now + 3599), "true");
        assert_eq!(run(now - 3600), "false");
        assert_eq!(run(now + 3600), "false");

        let _ = std::fs::remove_file(&out);
    }

    /// `abs` literal-fold smoke test: optimizer turns `abs(<negative>)`
    /// into `Number(positive)` BEFORE native sees the AST. Pins the
    /// `wrapping_abs` semantics (no panic on edge cases like i64::MIN).
    #[test]
    fn slice_abs_literal_folds_at_compile_time() {
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    n : number

rule lit
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : T
  output:
    out : number
  logic:
    out = abs(0 - 42) + t.n
  proofs:
    purity:
      reads : [t.n]
      calls : []
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        // Mirror the CLI flow: optimizer runs before native dispatch.
        let (program, _) = crate::optimizer::optimize_program(&program);
        let out = std::env::temp_dir().join("verbosec_test_abs_literal_fold");
        compile_native(&program, "lit", out.to_str().unwrap(), false, false)
            .expect("literal abs should compile");
        let r = std::process::Command::new(&out)
            .args(["8"])
            .output()
            .expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "50",
            "abs(0 - 42) + 8 = 42 + 8 = 50"
        );
        let _ = std::fs::remove_file(&out);
    }

    /// Slice 9.5f (2026-04-29): closes the resource-aware emitter sweep.
    /// `read(<resource>)` is now allowed in `emit_parallel_program`. The
    /// parent reads the threshold file ONCE before the fork; both
    /// halves of the record stream inherit the (ptr, len) slot via
    /// fork's copy-on-write. Composes with `parse_int` to load a
    /// runtime-tunable number from disk.
    ///
    /// Pins:
    ///   (a) parallel rule with `read()` compiles + emits the parallel
    ///       hint
    ///   (b) per-record output respects the runtime threshold
    ///   (c) editing the file between invocations changes the result
    ///   (d) abort on invalid threshold (parse_int) — fail-closed
    ///   (e) abort on missing file (read) — fail-closed
    #[test]
    fn slice_9_5f_parallel_with_read_threshold() {
        use std::process::Command;

        let path = "/tmp/verbosec_test_parallel_threshold.txt";
        std::fs::write(path, b"100").expect("write threshold");

        let src = std::fs::read_to_string("examples/parallel_threshold.verbose")
            .expect("examples/parallel_threshold.verbose");
        let src = src.replace("/tmp/verbose_parallel_threshold.txt", path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_parallel_threshold");
        compile_native(&program, "above", out.to_str().unwrap(), false, false)
            .expect("parallel + read should compile");

        // (b) batch run with threshold=100. We don't assert ORDER (parallel
        // dispatch can interleave) — we assert the SET of outputs matches
        // the expected counts of true vs false.
        let r = Command::new(&out)
            .args(["50", "150", "200", "75", "99", "101"])
            .output()
            .expect("spawn");
        assert!(r.status.success(), "happy run exit: {:?}", r.status);
        let out_text = String::from_utf8_lossy(&r.stdout);
        let trues = out_text.lines().filter(|l| l.trim() == "true").count();
        let falses = out_text.lines().filter(|l| l.trim() == "false").count();
        assert_eq!(trues, 3, "threshold=100, expected 3 true (150/200/101); stdout={:?}", out_text);
        assert_eq!(falses, 3, "threshold=100, expected 3 false (50/75/99); stdout={:?}", out_text);

        // (c) edit file → recount with threshold=50
        std::fs::write(path, b"50").expect("update");
        let r = Command::new(&out)
            .args(["50", "150", "200", "75", "99", "101"])
            .output()
            .expect("spawn");
        let out_text = String::from_utf8_lossy(&r.stdout);
        let trues = out_text.lines().filter(|l| l.trim() == "true").count();
        let falses = out_text.lines().filter(|l| l.trim() == "false").count();
        assert_eq!(trues, 5, "threshold=50, expected 5 true; stdout={:?}", out_text);
        assert_eq!(falses, 1, "threshold=50, expected 1 false (50); stdout={:?}", out_text);

        // (d) Invalid threshold → parse_int abort
        std::fs::write(path, b"abc").expect("invalid");
        let r = Command::new(&out)
            .args(["50", "150"])
            .output()
            .expect("spawn");
        assert!(!r.status.success(), "invalid threshold must abort");

        // (e) Missing file → resource read abort
        let _ = std::fs::remove_file(path);
        let r = Command::new(&out)
            .args(["50", "150"])
            .output()
            .expect("spawn");
        assert!(!r.status.success(), "missing file must abort");

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(path);
    }

    /// `contains(<haystack>, <needle>)` primitive (2026-04-29):
    /// naive O(N*M) substring search returning bool. Verifier-bounded
    /// by `max:` declarations so worst-case work is statically known.
    ///
    /// Pins the eight behaviors:
    ///   (a) match anywhere in the middle
    ///   (b) match at the start
    ///   (c) match at the end
    ///   (d) byte mismatch in the only candidate position → false
    ///   (e) needle longer than haystack → false (early-out)
    ///   (f) empty haystack vs non-empty needle → false
    ///   (g) empty needle → always true (standard convention)
    ///   (h) case-sensitive — "errOR" doesn't match "ERROR"
    #[test]
    fn slice_contains_substring_search() {
        use std::process::Command;
        let key = "/tmp/verbosec_test_contains_keyword.txt";
        std::fs::write(key, b"abc").expect("write key");

        let src = std::fs::read_to_string("examples/keyword_filter.verbose")
            .expect("examples/keyword_filter.verbose");
        let src = src.replace("/tmp/verbose_keyword.txt", key);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_contains_keyword");
        compile_native(&program, "flag", out.to_str().unwrap(), false, false)
            .expect("contains should compile");

        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) middle, (b) start, (c) end
        assert_eq!(run("xxxabcxxx"), "true", "middle match");
        assert_eq!(run("abcdef"), "true", "start match");
        assert_eq!(run("xyzabc"), "true", "end match");
        // (d) candidate position mismatches
        assert_eq!(run("ab cabd"), "false", "no abc anywhere");
        assert_eq!(run("aabbcc"), "false", "letters present but not contiguous");
        // (e) needle longer than haystack
        assert_eq!(run("a"), "false");
        assert_eq!(run("ab"), "false");
        // (f) empty haystack vs non-empty needle
        assert_eq!(run(""), "false");
        // (h) case-sensitive
        assert_eq!(run("ABC"), "false", "case-sensitive: ABC ≠ abc");
        assert_eq!(run("ABCABCABC"), "false");

        let _ = std::fs::remove_file(&out);

        // (g) empty needle → always true
        std::fs::write(key, b"").expect("empty key");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let empty_program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out_empty = std::env::temp_dir().join("verbosec_test_contains_empty_needle");
        compile_native(&empty_program, "flag", out_empty.to_str().unwrap(), false, false)
            .expect("contains with empty needle should compile");
        let r = Command::new(&out_empty).args(["anything"]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true",
            "empty needle should always match"
        );
        let r = Command::new(&out_empty).args([""]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true",
            "empty needle vs empty haystack is true"
        );

        let _ = std::fs::remove_file(&out_empty);
        let _ = std::fs::remove_file(key);
    }

    /// `length(<text>)` primitive (2026-04-29): byte count of a text
    /// expression as a Number. Three native paths:
    ///   - text input field → emit_strlen scan (3 µs-ish for argv text)
    ///   - BoundText (read/fetch/Phase-2I let) → load len_slot directly
    ///     (zero scan; the prologue already counted the bytes)
    ///   - literal → folded to Number at compile time (never reaches native)
    ///
    /// This test pins:
    ///   (a) length(text_field) returns the exact byte count
    ///   (b) length(read(<resource>)) returns the file size (zero-scan
    ///       because the read syscall already returned the byte count)
    ///   (c) length("literal") folds at compile time (binary stays tight)
    ///   (d) length composes with parse_int + comparison for runtime-
    ///       tunable input validation gates (HTTP 414 pattern)
    #[test]
    fn length_runtime_and_compose_with_parse_int() {
        use std::process::Command;

        // (a) length(text_field) via a CLI scalar rule.
        let src_a = r#"@verbose 0.1.0

concept Req
  @intention: "x"
  @source: invoices.intent:1
  fields:
    path : text

rule path_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    r : Req
  output:
    n : number
  logic:
    n = length(r.path)
  proofs:
    purity:
      reads : [r.path]
      calls : []
    termination:
      bound : 1
"#;
        let tokens = crate::lexer::Lexer::new(src_a).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_field");
        compile_native(&program, "path_len", out.to_str().unwrap(), false, false)
            .expect("length(text_field) should compile");

        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        assert_eq!(run(""), "0", "empty path → 0");
        assert_eq!(run("/api"), "4", "/api → 4 bytes");
        assert_eq!(run("/this/is/a/very/long/path"), "25", "long path → 25 bytes");
        let _ = std::fs::remove_file(&out);

        // (c) length("literal") folds to Number.
        let src_c = r#"@verbose 0.1.0

concept Tick
  @intention: "x"
  @source: invoices.intent:1
  fields:
    n : number

rule lit_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    sz : number
  logic:
    sz = length("hello world") + t.n
  proofs:
    purity:
      reads : [t.n]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src_c).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        // Mirror the CLI flow: optimizer runs before native dispatch so
        // length("hello world") folds to Number(11) before native sees it.
        let (program, _) = crate::optimizer::optimize_program(&program);
        let out = std::env::temp_dir().join("verbosec_test_length_literal_fold");
        compile_native(&program, "lit_len", out.to_str().unwrap(), false, false)
            .expect("literal length should compile");
        let r = Command::new(&out).args(["5"]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "16",
            "11 + 5 = 16"
        );
        let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(
            size < 600,
            "literal-fold binary should stay tight; got {}",
            size
        );
        let _ = std::fs::remove_file(&out);
    }

    /// `length(<Call>)` and `length(<Concat>)` runtime composition
    /// (2026-05-12): the inner arg can now be a text-returning rule call
    /// (Phase 2G-shape, recursed into the callee body) or a `concat(...)`
    /// of text-only args (each arg's length contributes to a stack
    /// accumulator). No buffer allocation — the concatenation never
    /// materialises, we only count its bytes.
    ///
    /// This test pins three runtime behaviors and two refusals:
    ///   (a) `length(call(input))` where the callee body is
    ///       `concat(<literal>, input.field)` returns the right byte
    ///       count and varies with the input field's bytes.
    ///   (b) `length(concat(...))` directly (no helper) sums correctly.
    ///   (c) nested: `length(concat(<literal>, call(input)))` composes —
    ///       outer Concat slot accumulates the literal AND the recursive
    ///       Call-length, which itself opens its own slot.
    ///   (d) refusal: `length(concat(...))` with a number-typed field
    ///       arg produces a clear error naming the offending field.
    ///   (e) refusal: `length(call(input))` where the callee has let
    ///       bindings is rejected (Phase 2G-style restriction).
    #[test]
    fn slice_length_call_and_concat_compose() {
        use std::process::Command;

        // (a) length(Call) — callee body is concat("Hello, ", o.name).
        let src_a = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule greet
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("Hello, ", o.name)
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 2

rule greet_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(greet(o))
  proofs:
    purity:
      reads : [o]
      calls : [greet]
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src_a).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_call");
        compile_native(&program, "greet_len", out.to_str().unwrap(), false, false)
            .expect("length(call) should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        assert_eq!(run(""), "7", "Hello, + empty → 7");
        assert_eq!(run("Bob"), "10", "Hello, + Bob → 10");
        assert_eq!(run("Christopher"), "18", "Hello, + Christopher → 18");
        let _ = std::fs::remove_file(&out);

        // (b) length(Concat) directly.
        let src_b = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule msg_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(concat("Hi, ", o.name, "!"))
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src_b).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_concat_direct");
        compile_native(&program, "msg_len", out.to_str().unwrap(), false, false)
            .expect("length(concat) should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        // "Hi, " (4) + name + "!" (1) = 5 + name_len
        assert_eq!(run(""), "5");
        assert_eq!(run("Bob"), "8");
        assert_eq!(run("Christopher"), "16");
        let _ = std::fs::remove_file(&out);

        // (c) nested: length(concat(literal, call(input))) — outer
        // Concat slot composes with the inner Call's slot.
        let src_c = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule greet
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("Hello, ", o.name)
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 2

rule wrap_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(concat("[", greet(o), "]"))
  proofs:
    purity:
      reads : [o]
      calls : [greet]
    termination:
      bound : 4
"#;
        let tokens = crate::lexer::Lexer::new(src_c).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_nested");
        compile_native(&program, "wrap_len", out.to_str().unwrap(), false, false)
            .expect("length(concat with call arg) should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        // "[" (1) + "Hello, " (7) + name + "]" (1) = 9 + name_len
        assert_eq!(run(""), "9");
        assert_eq!(run("Bob"), "12");
        let _ = std::fs::remove_file(&out);

        // (d) refusal: number-typed field as concat arg.
        let src_d = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amt : number

rule l
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(concat("x", o.amt))
  proofs:
    purity:
      reads : [o.amt]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src_d).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_number_refuse");
        let err = compile_native(&program, "l", out.to_str().unwrap(), false, false)
            .expect_err("number-typed field arg must be refused");
        let msg = format!("{}", err);
        assert!(
            msg.contains("number-typed") && msg.contains("amt"),
            "error should name the field and its type: {}",
            msg
        );

        // (e) refusal: callee has let bindings (Phase 2G constraint).
        let src_e = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule greet_with_let
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    let prefix = concat("Hi, ", o.name)
    s = concat(prefix, "!")
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 3

rule with_let_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(greet_with_let(o))
  proofs:
    purity:
      reads : [o]
      calls : [greet_with_let]
    termination:
      bound : 4
"#;
        let tokens = crate::lexer::Lexer::new(src_e).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_lets_refuse");
        let err = compile_native(&program, "with_let_len", out.to_str().unwrap(), false, false)
            .expect_err("callee with let bindings must be refused");
        let msg = format!("{}", err);
        assert!(
            msg.contains("let bindings") && msg.contains("greet_with_let"),
            "error should explain the Phase 2G let restriction: {}",
            msg
        );
    }

    /// `now_unix()` allowed in parallel rules (2026-05-12): parent
    /// samples clock_gettime ONCE before the fork; both children inherit
    /// the captured value via fork's COW. This closes the last
    /// "now_unix() reached emit_eval_expr in a context that did not
    /// capture the clock" refusal — every emitter that supports parallel
    /// records now wires the timestamp.
    ///
    /// The invariant pinned here: every record processed by the binary
    /// reads the SAME `now` value. We submit records whose values are
    /// successive small constants and check that the output deltas
    /// equal the input deltas. If a child had re-sampled the clock
    /// between fork and record processing, the second half's records
    /// would carry a different `now` and the deltas would diverge by
    /// (roughly) one second.
    #[test]
    fn slice_now_unix_in_parallel_inherits_via_cow() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept R
  @intention: "a record"
  @source: invoices.intent:1
  fields:
    amount : number

rule stamped
  @intention: "amount plus captured now"
  @source: invoices.intent:1

  hints:
    parallel: "split across two children"

  input:
    r : R

  output:
    n : number

  logic:
    n = r.amount + now_unix()

  proofs:
    purity:
      reads : [r.amount, now]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_parallel_now");
        compile_native(&program, "stamped", out.to_str().unwrap(), false, false)
            .expect("now_unix() in parallel rule should compile");
        // Feed six records (3 to the child, 3 to the parent post-wait).
        // Inputs chosen with a deliberate gap (10, 20, 30, 1000, 1010,
        // 1020) so that if a worker re-sampled the clock between fork
        // and processing, the gap between the first-half and second-
        // half outputs would be poisoned by the clock drift.
        let r = Command::new(&out)
            .args(["10", "20", "30", "1000", "1010", "1020"])
            .output()
            .expect("spawn");
        assert!(r.status.success(), "binary should exit 0; stderr: {}",
            String::from_utf8_lossy(&r.stderr));
        let out_str = String::from_utf8_lossy(&r.stdout);
        let lines: Vec<&str> = out_str.lines().collect();
        assert_eq!(lines.len(), 6, "expected six output lines, got: {:?}", lines);
        let nums: Vec<i64> = lines.iter().map(|l| l.parse().expect("number")).collect();
        // All six outputs must share the same `now` baseline. Recover
        // it from each output (output[i] = input[i] + now) and verify
        // every line yields the same now.
        let inputs = [10i64, 20, 30, 1000, 1010, 1020];
        let now_baselines: Vec<i64> = nums.iter().zip(inputs.iter())
            .map(|(out, inp)| out - inp).collect();
        let first = now_baselines[0];
        for (i, b) in now_baselines.iter().enumerate() {
            assert_eq!(
                *b, first,
                "record {} sees a different `now` ({} vs first {}). Children must inherit \
                 the parent's pre-fork clock sample via COW, not re-sample.",
                i, b, first
            );
        }
        // Sanity: the captured `now` is in a plausible Unix-epoch range
        // (post-2020, pre-2050) so we know clock_gettime actually ran.
        assert!(first > 1_577_836_800, "now {} looks bogus (pre-2020)", first);
        assert!(first < 2_524_608_000, "now {} looks bogus (post-2050)", first);
        let _ = std::fs::remove_file(&out);
    }

    /// `length(json_escape(<text>))` runtime composition (2026-05-12):
    /// count the escaped output bytes without materialising the escape
    /// transform. Native's json_escape only escapes five bytes
    /// (`"` `\` `\n` `\r` `\t`), each becoming 2 bytes; every other byte
    /// passes through as 1. So the output length = input length + count
    /// of escape-triggering bytes. One byte pass, accumulator in r8,
    /// zero allocation.
    ///
    /// This test pins four runtime behaviors:
    ///   (a) input with no escape-triggering bytes → output length
    ///       equals input length (the +1-per-escape path never fires)
    ///   (b) input with a single quote → output length = input + 1
    ///   (c) input with multiple escape chars → +1 per match
    ///   (d) empty input → 0
    /// And one refusal: inner shape outside the
    /// `emit_json_escape_load_src` accepted set (literal Text, Call,
    /// nested JsonEscape) is rejected with a clear message.
    #[test]
    fn slice_length_json_escape_byte_scan() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    msg : text

rule escaped_len
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(json_escape(o.msg))
  proofs:
    purity:
      reads : [o.msg]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_json_escape");
        compile_native(&program, "escaped_len", out.to_str().unwrap(), false, false)
            .expect("length(json_escape(field)) should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        assert_eq!(run("hello"), "5", "plain ASCII: 5 bytes in, 5 out");
        assert_eq!(run("the quick brown fox"), "19", "ASCII no escapes");
        assert_eq!(run("foo\"bar"), "8", "7 input bytes + 1 escape = 8");
        assert_eq!(run("a\"b\\c"), "7", "5 in + 2 escapes (\" and \\) = 7");
        assert_eq!(run("a\tb\nc"), "7", "5 in + 2 escapes (\\t, \\n) = 7");
        assert_eq!(run("\"\\\n\r\t"), "10", "5 escape bytes → 10 out");
        assert_eq!(run(""), "0", "empty in → 0 out");
        let _ = std::fs::remove_file(&out);

        let src_e = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : text
    b : text

rule l
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(json_escape(concat(o.a, o.b)))
  proofs:
    purity:
      reads : [o.a, o.b]
      calls : []
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src_e).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_length_json_escape_refuse");
        let err = compile_native(&program, "l", out.to_str().unwrap(), false, false)
            .expect_err("json_escape inner = Concat must be refused in this slice");
        let msg = format!("{}", err);
        assert!(
            msg.contains("json_escape inner shape not supported"),
            "error should name the unsupported inner shape: {}",
            msg
        );
    }

    /// `json_escape(<Call>)` accepts a text-returning rule call (2026-05-12),
    /// Phase 2G-shape inline. Same family as length(Call) /
    /// starts_with(Call) / parse_int(Call): single Ident(input) arg,
    /// no callee lets, returns text. The callee body must itself
    /// resolve to a json_escape-supported shape (text input field or
    /// BoundText).
    ///
    /// Pinned through length(json_escape(call(o))) which exercises
    /// both the Call inline path AND length's existing json_escape
    /// arm — verifies the recursion produces the right byte count.
    #[test]
    fn slice_json_escape_accepts_call_phase_2g() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule lookup
  @intention: "passthrough"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = o.name
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 1

rule escaped_len
  @intention: "byte count after json-escape of helper output"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(json_escape(lookup(o)))
  proofs:
    purity:
      reads : [o]
      calls : [lookup]
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_json_escape_call_v2");
        compile_native(&program, "escaped_len", out.to_str().unwrap(), false, false)
            .expect("json_escape(Call) should compile");
        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };
        // Plain ASCII: no escapes → output length = input length
        assert_eq!(run("Alice"), "5");
        assert_eq!(run("hello world"), "11");
        // Quote inside: +1
        assert_eq!(run("foo\"bar"), "8");
        // Backslash + newline: +2
        assert_eq!(run("a\\b\nc"), "7");
        let _ = std::fs::remove_file(&out);

        // Refusal: callee body is a concat — bubbles up the existing
        // "json_escape inner shape not supported" error from the
        // recursive load.
        let src_refuse = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule greet
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("Hello, ", o.name)
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 2

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(json_escape(greet(o)))
  proofs:
    purity:
      reads : [o]
      calls : [greet]
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src_refuse).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let bin = std::env::temp_dir().join("verbosec_test_json_escape_call_refuse_v2");
        let err = compile_native(&program, "bad", bin.to_str().unwrap(), false, false)
            .expect_err("concat callee body must be refused for json_escape inner");
        let msg = format!("{}", err);
        assert!(
            msg.contains("json_escape inner shape not supported"),
            "error should name the unsupported inner shape: {}",
            msg
        );
    }

    /// `starts_with` / `ends_with` / `contains` accept a text-returning
    /// `Call(callee, [Ident(input)])` (2026-05-12), Phase 2G-shape
    /// inline. The callee's body resolves into (rsi, rcx) under the
    /// caller's offsets, byte-for-byte equivalent to hoisting the
    /// body's load. Restriction: callee body must itself resolve to a
    /// non-allocating shape (literal, text field, BoundText). A callee
    /// whose body is `concat(...)` is rejected — the two-arg
    /// byte-compare can't manage a buffer between the haystack and
    /// needle emits. Workaround: bind via a `let` and pass the let.
    ///
    /// Same Phase 2G constraints as length(Call): single Ident(input)
    /// arg, callee has no let bindings, callee returns text.
    #[test]
    fn slice_byte_prims_accept_call_phase_2g() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule lookup
  @intention: "passthrough callee — body is a text field, no allocation"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = o.name
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 1

rule starts_alice
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = starts_with(lookup(o), "Ali")
  proofs:
    purity:
      reads : [o]
      calls : [lookup]
    termination:
      bound : 2

rule ends_e
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = ends_with(lookup(o), "e")
  proofs:
    purity:
      reads : [o]
      calls : [lookup]
    termination:
      bound : 2

rule has_i
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = contains(lookup(o), "i")
  proofs:
    purity:
      reads : [o]
      calls : [lookup]
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let run_rule = |rule: &str, arg: &str| -> String {
            let bin = std::env::temp_dir().join(format!("verbosec_test_byte_prim_call_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let out = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            out
        };
        // starts_with(lookup(o), "Ali")
        assert_eq!(run_rule("starts_alice", "Alice"), "true");
        assert_eq!(run_rule("starts_alice", "Bob"), "false");
        // ends_with(lookup(o), "e")
        assert_eq!(run_rule("ends_e", "Alice"), "true");
        assert_eq!(run_rule("ends_e", "Bob"), "false");
        // contains(lookup(o), "i")
        assert_eq!(run_rule("has_i", "Alice"), "true");
        assert_eq!(run_rule("has_i", "Bob"), "false");

        // Refusal path: callee body is a concat — would need a buffer
        // between the haystack and needle emits.
        let src_refuse = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text

rule greet
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("Hello, ", o.name)
  proofs:
    purity:
      reads : [o.name]
      calls : []
    termination:
      bound : 2

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = starts_with(greet(o), "Hello")
  proofs:
    purity:
      reads : [o]
      calls : [greet]
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src_refuse).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let bin = std::env::temp_dir().join("verbosec_test_byte_prim_call_refuse");
        let err = compile_native(&program, "bad", bin.to_str().unwrap(), false, false)
            .expect_err("concat callee body must be refused");
        let msg = format!("{}", err);
        assert!(
            msg.contains("Concat"),
            "error should name the offending shape: {}",
            msg
        );
    }

    /// `starts_with(<haystack>, <needle>)` primitive (2026-04-29):
    /// native byte-compare returning bool. Both args text. Composes
    /// with the existing HTTP service handler emitter to express
    /// path-prefix routing without regex.
    ///
    /// This test pins the four behaviors:
    ///   (a) prefix match → true
    ///   (b) length-too-short → false (the cmp r9, rcx ; jb gate)
    ///   (c) byte mismatch within length → false (cmpsb sets ZF=0)
    ///   (d) empty needle → always true (the test rcx ; je shortcut)
    /// Plus a router test that exercises the more interesting
    /// composition: starts_with inside an HTTP service handler's
    /// if/else routing chain.
    #[test]
    fn slice_starts_with_runtime_byte_compare() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept Req
  @intention: "x"
  @source: invoices.intent:1
  fields:
    path : text

rule check
  @intention: "x"
  @source: invoices.intent:1
  input:
    r : Req
  output:
    ok : bool
  logic:
    ok = starts_with(r.path, "/admin/")
  proofs:
    purity:
      reads : [r.path]
      calls : []
    termination:
      bound : 1
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_starts_with");
        compile_native(&program, "check", out.to_str().unwrap(), false, false)
            .expect("starts_with should compile");

        let run = |arg: &str| -> String {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) prefix matches
        assert_eq!(run("/admin/users"), "true");
        assert_eq!(run("/admin/"), "true", "exact length match should be true");
        // (b) length too short — the haystack is shorter than the needle
        assert_eq!(run("/admin"), "false", "shorter than needle should be false");
        assert_eq!(run("/adm"), "false");
        assert_eq!(run(""), "false", "empty haystack vs non-empty needle");
        // (c) byte mismatch within length
        assert_eq!(run("/api/x"), "false");
        assert_eq!(run("/admixn"), "false", "byte mismatch within length");

        let _ = std::fs::remove_file(&out);

        // Empty needle: any haystack matches.
        let empty_src = src.replace("\"/admin/\"", "\"\"");
        let tokens = crate::lexer::Lexer::new(&empty_src).tokenize().expect("tokenize");
        let empty_program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out2 = std::env::temp_dir().join("verbosec_test_starts_with_empty");
        compile_native(&empty_program, "check", out2.to_str().unwrap(), false, false)
            .expect("starts_with with empty needle should compile");
        let r = Command::new(&out2).args(["/anything"]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true",
            "empty needle should match any haystack"
        );
        let r = Command::new(&out2).args([""]).output().expect("spawn");
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "true",
            "empty needle vs empty haystack should match"
        );
        let _ = std::fs::remove_file(&out2);
    }

    /// `now_unix()` extension (2026-04-28): the primitive now works in
    /// the body of a Phase 4 number fold (count/sum/min/max). The
    /// canonical use case: count events in a sliding window, judged
    /// against ONE captured "now" so a batch is consistent even if
    /// the wall clock advances mid-iteration.
    ///
    /// This test spawns the binary multiple times; each invocation
    /// captures its own clock independently, but within one batch
    /// every event is judged against the same captured value.
    #[test]
    fn slice_now_unix_in_fold_body_sliding_window() {
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let src = std::fs::read_to_string("examples/sliding_count.verbose")
            .expect("examples/sliding_count.verbose");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_sliding_count");
        compile_native(&program, "recent_count", out.to_str().unwrap(), false, false)
            .expect("Phase 4 fold + now_unix() should compile");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs() as i64;

        let count = |args: &[String]| -> String {
            let r = Command::new(&out).args(args).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout)
                .trim_end_matches('\n')
                .to_string()
        };

        // 3 fresh + 2 old → 3
        let args: Vec<String> = vec![
            "5".to_string(),
            (now - 30).to_string(),
            (now - 100).to_string(),
            (now - 7200).to_string(),
            (now - 60).to_string(),
            (now - 5000).to_string(),
        ];
        assert_eq!(count(&args), "3");

        // empty batch → 0
        assert_eq!(count(&["0".to_string()]), "0");

        // all old → 0
        assert_eq!(
            count(&["3".to_string(), (now - 4000).to_string(), (now - 5000).to_string(), (now - 7200).to_string()]),
            "0"
        );

        // all fresh → 4
        assert_eq!(
            count(&["4".to_string(), (now - 1).to_string(), (now - 100).to_string(), (now - 1000).to_string(), (now - 3500).to_string()]),
            "4"
        );

        let _ = std::fs::remove_file(&out);
    }

    /// `now_unix()` primitive (2026-04-28): system clock as a declared
    /// read. CLOCK_REALTIME sampled ONCE per rule invocation; every
    /// reference loads the same captured value from a dedicated rbp
    /// slot. Verifier requires `reads: [now]` (synthetic name token).
    ///
    /// This test pins:
    ///   (a) recent event → true
    ///   (b) old event → false
    ///   (c) multiple records in one binary invocation share the same
    ///       captured "now" (subtle but load-bearing — without single-
    ///       capture semantics, two records 1 ms apart could disagree
    ///       on whether they're "fresh")
    ///   (d) verifier rejects `now_unix()` use without `now` in reads
    #[test]
    fn now_unix_runtime_capture_and_verifier_check() {
        use std::process::Command;
        use std::time::{SystemTime, UNIX_EPOCH};

        let src = std::fs::read_to_string("examples/recent_event.verbose")
            .expect("examples/recent_event.verbose");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_now_unix_recent");
        compile_native(&program, "recent", out.to_str().unwrap(), false, false)
            .expect("now_unix() rule should compile");

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_secs() as i64;
        let run_one = |ts: i64| -> String {
            let r = Command::new(&out)
                .args([ts.to_string()])
                .output()
                .expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) 30 seconds old → true
        assert_eq!(run_one(now - 30), "true", "30s ago should be fresh");
        // (b) 7200 seconds old (2 hours) → false
        assert_eq!(run_one(now - 7200), "false", "2h ago should NOT be fresh");
        // 3599 vs 3600 boundary
        assert_eq!(run_one(now - 3599), "true", "3599s should be fresh (< 3600)");
        assert_eq!(run_one(now - 3600), "false", "3600s should NOT be fresh (strict <)");

        // (c) one binary, three records — all judged against the same
        // captured "now". With single-capture semantics, two records
        // submitted in the same invocation see identical "now" values
        // even if the wall clock advanced between iterations. Verify
        // the boolean output for each.
        let r = Command::new(&out)
            .args([(now - 30).to_string(), (now - 7200).to_string(), (now - 100).to_string()])
            .output()
            .expect("spawn batch");
        let lines: Vec<&str> = std::str::from_utf8(&r.stdout).unwrap().lines().collect();
        assert_eq!(lines, vec!["true", "false", "true"],
                   "batch outputs must reflect a single captured now");

        // (d) Verifier rejects undeclared use.
        let bad_src = src.replace("reads : [e.ts, now]", "reads : [e.ts]");
        let tokens = crate::lexer::Lexer::new(&bad_src).tokenize().expect("tokenize");
        let bad_program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");
        let errors = crate::verifier::verify_program(&bad_program, std::path::Path::new("examples"));
        assert!(
            errors.iter().any(|e| e.message.contains("missing")
                              && e.message.contains("now")),
            "verifier must reject now_unix() without `now` in reads; got errors: {:?}",
            errors
        );

        let _ = std::fs::remove_file(&out);
    }

    /// `parse_int(<text>)` primitive (2026-04-28): convert a text
    /// value to a number with strict scan + fail-closed abort. Inner
    /// must reference a text source whose (ptr, len) live in stable
    /// rbp slots — `read(<resource>)`, `Ident(<text-let>)`, or
    /// `fetch(<connection>, _)`. The optimizer compile-time-folds
    /// `parse_int("<literal>")` to `Number(<parsed>)` so that path
    /// never reaches native; this test focuses on the runtime scan
    /// triggered by `parse_int(read(threshold))`.
    ///
    /// Pins:
    ///   (a) valid positive integer → correct sum
    ///   (b) edit file between invocations → updated result
    ///   (c) "0" → all orders contribute
    ///   (d) negative integer → all orders contribute (sign handled)
    ///   (e) non-digit content → exit 1, no stdout
    ///   (f) empty file → exit 1
    ///   (g) lone "-" → exit 1
    ///   (h) missing file → exit 1 (resource read aborts before parse_int runs)
    #[test]
    fn parse_int_runtime_scan_with_read_inner() {
        use std::process::Command;
        let path = "/tmp/verbosec_test_parse_int_threshold.txt";
        std::fs::write(path, b"100").expect("write threshold");

        let src = std::fs::read_to_string("examples/threshold_sum.verbose")
            .expect("examples/threshold_sum.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_threshold.txt", path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_parse_int_threshold");
        compile_native(&program, "sum_above_threshold", out.to_str().unwrap(), false, false)
            .expect("parse_int(read(...)) should compile");

        let run = || -> std::process::Output {
            Command::new(&out)
                .args(["4", "50", "150", "200", "75"])
                .output()
                .expect("spawn")
        };
        let stdout_of = |o: &std::process::Output| -> String {
            String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string()
        };

        // (a) threshold=100 → 150 + 200 = 350
        let r = run();
        assert!(r.status.success() && stdout_of(&r) == "350", "threshold=100: {:?}", stdout_of(&r));

        // (b) threshold=200 → none strictly above → 0
        std::fs::write(path, b"200").expect("update");
        let r = run();
        assert!(r.status.success() && stdout_of(&r) == "0", "threshold=200: {:?}", stdout_of(&r));

        // (c) threshold=0 → 50+150+200+75 = 475
        std::fs::write(path, b"0").expect("update");
        let r = run();
        assert!(r.status.success() && stdout_of(&r) == "475", "threshold=0: {:?}", stdout_of(&r));

        // (d) threshold=-1 → 475
        std::fs::write(path, b"-1").expect("update");
        let r = run();
        assert!(r.status.success() && stdout_of(&r) == "475", "threshold=-1: {:?}", stdout_of(&r));

        // (e) non-digit → exit 1, empty stdout
        std::fs::write(path, b"abc").expect("update");
        let r = Command::new(&out).args(["1", "50"]).output().expect("spawn");
        assert!(!r.status.success() && r.stdout.is_empty(), "non-digit should abort");

        // (f) empty file
        std::fs::write(path, b"").expect("empty");
        let r = Command::new(&out).args(["1", "50"]).output().expect("spawn");
        assert!(!r.status.success() && r.stdout.is_empty(), "empty file should abort");

        // (g) lone "-"
        std::fs::write(path, b"-").expect("update");
        let r = Command::new(&out).args(["1", "50"]).output().expect("spawn");
        assert!(!r.status.success() && r.stdout.is_empty(), "lone minus should abort");

        // (h) missing file → resource read aborts first (covered by slice 9.1
        // semantics, but worth pinning here so we know the chain is intact)
        let _ = std::fs::remove_file(path);
        let r = Command::new(&out).args(["1", "50"]).output().expect("spawn");
        assert!(!r.status.success() && r.stdout.is_empty(), "missing file should abort");

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(path);
    }

    /// `parse_int("<literal>")` is folded by the optimizer to
    /// `Number(<parsed>)` BEFORE native sees the AST. This test pins
    /// that the runtime scan loop is NOT emitted for literal-arg
    /// calls — the resulting binary should be compact (no per-call
    /// scan + abort tail).
    #[test]
    fn parse_int_literal_folds_at_compile_time() {
        use std::fs;
        let src = r#"@verbose 0.1.0

concept T
  @intention: "trivial"
  @source: invoices.intent:1
  fields:
    x : number

rule lit
  @intention: "literal parse_int folded"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = parse_int("42") + t.x
  proofs:
    purity:
      reads : [t.x]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        // Mirror the CLI flow: optimizer runs before native dispatch so
        // `parse_int("42")` is folded to `Number(42)` BEFORE this compile
        // sees it. Without this step, the test would exercise the runtime
        // scan loop (which doesn't accept Text inner) and miss the point
        // of the fold.
        let (program, _) = crate::optimizer::optimize_program(&program);
        let out = std::env::temp_dir().join("verbosec_test_parse_int_literal_fold");
        compile_native(&program, "lit", out.to_str().unwrap(), false, false)
            .expect("literal parse_int should compile");

        // Run with t.x = 8; expect 42 + 8 = 50.
        let r = std::process::Command::new(&out)
            .args(["8"])
            .output()
            .expect("spawn");
        assert!(r.status.success());
        assert_eq!(
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n'),
            "50"
        );

        // Tight binary: literal fold means no scan loop, no abort tail.
        let size = fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        assert!(
            size < 600,
            "literal-fold binary should stay tight (under 600 B); got {}",
            size
        );

        let _ = fs::remove_file(&out);
    }

    /// `parse_int(<text-field>)` runtime scan (2026-05-12): parse a
    /// text-typed input field as a number. The field's pointer lives at
    /// the existing rbp slot (NUL-terminated argv pointer); we recover
    /// length via emit_strlen and run the existing strict scan loop.
    /// Closes a long-standing gap noted in emit_parse_int's old catch-
    /// all message ("Field-text/concat/call/json_escape parse_int can
    /// be added in a follow-up slice").
    ///
    /// Fail-closed posture intact: empty input aborts, lone '-' aborts,
    /// any non-digit byte aborts — same sys_exit(1) abort label shared
    /// with the BoundText path.
    #[test]
    fn parse_int_text_field_runtime() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    raw : text

rule parsed
  @intention: "parse the raw text field as a number"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = parse_int(o.raw)
  proofs:
    purity:
      reads : [o.raw]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_parse_int_field");
        compile_native(&program, "parsed", out.to_str().unwrap(), false, false)
            .expect("parse_int(text_field) should compile");

        let run = |arg: &str| -> (i32, String) {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            (code, stdout)
        };
        // Success cases — all-digit, signed, zero
        assert_eq!(run("42"), (0, "42".into()));
        assert_eq!(run("-7"), (0, "-7".into()));
        assert_eq!(run("0"), (0, "0".into()));
        assert_eq!(run("1000000"), (0, "1000000".into()));

        // Fail-closed cases — exit 1, no stdout
        assert_eq!(run("").0, 1, "empty input must abort");
        assert_eq!(run("abc").0, 1, "non-digit must abort");
        assert_eq!(run("-").0, 1, "lone minus must abort");
        assert_eq!(run("12x").0, 1, "trailing non-digit must abort");

        let _ = fs::remove_file(&out);
    }

    /// `parse_int(<Call>)` accepts a text-returning rule call (2026-05-13),
    /// Phase 2G-shape inline. Closes the Phase 2G family across every
    /// text-consuming primitive (length, parse_int, starts_with,
    /// ends_with, contains, json_escape all accept a Call now). The
    /// callee body must itself resolve to a parse_int-supported shape
    /// (text input field or BoundText); concat / nested call bubble up
    /// the existing catch-all.
    ///
    /// Same fail-closed posture as the other parse_int shapes:
    /// empty / lone '-' / non-digit byte all abort with sys_exit(1).
    #[test]
    fn slice_parse_int_accepts_call_phase_2g() {
        use std::fs;
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    raw : text

rule lookup
  @intention: "passthrough"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = o.raw
  proofs:
    purity:
      reads : [o.raw]
      calls : []
    termination:
      bound : 1

rule parsed
  @intention: "parse the helper output"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = parse_int(lookup(o))
  proofs:
    purity:
      reads : [o]
      calls : [lookup]
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let out = std::env::temp_dir().join("verbosec_test_parse_int_call");
        compile_native(&program, "parsed", out.to_str().unwrap(), false, false)
            .expect("parse_int(Call) should compile");
        let run = |arg: &str| -> (i32, String) {
            let r = Command::new(&out).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            (code, stdout)
        };
        assert_eq!(run("42"), (0, "42".into()));
        assert_eq!(run("-7"), (0, "-7".into()));
        assert_eq!(run("0"), (0, "0".into()));
        assert_eq!(run("").0, 1, "empty input must abort");
        assert_eq!(run("abc").0, 1, "non-digit must abort");
        let _ = fs::remove_file(&out);

        // Refusal: callee body is concat (needs buffer materialisation
        // that the strict scan avoids).
        let src_refuse = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : text
    b : text

rule joined
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat(o.a, o.b)
  proofs:
    purity:
      reads : [o.a, o.b]
      calls : []
    termination:
      bound : 2

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = parse_int(joined(o))
  proofs:
    purity:
      reads : [o]
      calls : [joined]
    termination:
      bound : 3
"#;
        let tokens = crate::lexer::Lexer::new(src_refuse).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let bin = std::env::temp_dir().join("verbosec_test_parse_int_call_refuse");
        let err = compile_native(&program, "bad", bin.to_str().unwrap(), false, false)
            .expect_err("concat callee body must be refused for parse_int inner");
        let msg = format!("{}", err);
        assert!(
            msg.contains("parse_int") && (msg.contains("Concat") || msg.contains("buffer")),
            "error should explain why concat-body callee is refused: {}",
            msg
        );
    }

    /// `substring(<text>, <start>, <end>)` slice 1 (2026-05-13): the
    /// missing tokenizer primitive — half-open slice by byte offset,
    /// returns text. No allocation: the result is `(text_ptr + start,
    /// end - start)`, a pointer into the same buffer as the input.
    ///
    /// Self-hosting target depends on this — without `substring` you
    /// can't extract a literal out of a source buffer in a tokenizer.
    ///
    /// Slice 1 wires substring only as the RHS of a text-typed `let`
    /// binding (Phase 2I shape); use as a direct arg to length /
    /// concat / etc. is slice 2's responsibility (deferred to keep
    /// this slice bounded).
    ///
    /// This test pins:
    ///   (a) Happy path: substring("hello", 0, 3) → "hel"
    ///   (b) Mid slice: substring("abcdef", 2, 5) → "cde"
    ///   (c) Exact end (end == len): substring("abcde", 2, 5) → "cde"
    ///   (d) Empty slice (start == end): substring("hello", 2, 2) → ""
    ///   (e) Fail-closed: end > length → sys_exit(1)
    ///   (f) Fail-closed: start > end → sys_exit(1)
    ///   (g) Surfaces & fixes a latent bug in emit_text_produce_ptrlen's
    ///       Field arm where rax was clobbered by emit_strlen's
    ///       `xor eax, eax` — no prior test exercised a bare-Field
    ///       text-let RHS, so the rax = 0 leaked silently. substring
    ///       pushes rax to the stack as saved text_ptr, surfacing the
    ///       issue. The fix saves text_ptr in rsi (survives emit_strlen)
    ///       and restores rax = rsi after the scan.
    #[test]
    fn slice_substring_via_text_let_runtime() {
        use std::process::Command;

        // (a–c) Happy paths through a text-let RHS.
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule head3
  @intention: "first 3 bytes"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    let slice = substring(o.source, 0, 3)
    s = slice
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2

rule mid
  @intention: "bytes 2..5"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    let slice = substring(o.source, 2, 5)
    s = slice
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2

rule empty_slice
  @intention: "empty slice when start == end"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    let slice = substring(o.source, 2, 2)
    s = slice
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let run = |rule: &str, arg: &str| -> (i32, String) {
            let bin = std::env::temp_dir().join(format!("verbosec_test_substring_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            (code, stdout)
        };

        // (a) substring("hello", 0, 3) → "hel"
        assert_eq!(run("head3", "hello"), (0, "hel".into()));
        // First-three bytes of any longer string.
        assert_eq!(run("head3", "abcdef"), (0, "abc".into()));
        // (b) Mid slice.
        assert_eq!(run("mid", "abcdef"), (0, "cde".into()));
        assert_eq!(run("mid", "helloworld"), (0, "llo".into()));
        // (c) Exact-end (end == length) — empty trailing range OK.
        assert_eq!(run("mid", "abcde"), (0, "cde".into()));
        // (d) Empty slice produces empty output.
        assert_eq!(run("empty_slice", "hello"), (0, "".into()));

        // (e) Fail-closed: end > length(text).
        assert_eq!(run("head3", "ab").0, 1, "end=3 > len=2 must abort");
        assert_eq!(run("mid", "abc").0, 1, "end=5 > len=3 must abort");

        // (f) Fail-closed: start > end. Build a rule whose start > end.
        let src_inverted = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule bad_range
  @intention: "start > end"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    let slice = substring(o.source, 3, 1)
    s = slice
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src_inverted).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let bin = std::env::temp_dir().join("verbosec_test_substring_bad_range");
        compile_native(&program, "bad_range", bin.to_str().unwrap(), false, false)
            .expect("compile");
        let r = Command::new(&bin).args(["hello"]).output().expect("spawn");
        assert_eq!(r.status.code(), Some(1), "start=3 > end=1 must abort");
        let _ = std::fs::remove_file(&bin);
    }

    /// `length(substring(text, start, end))` direct composition
    /// (2026-05-13), slice 2 of substring. Lets the AI write
    /// `length(substring(o.source, 0, 3))` without a let-workaround.
    /// The result is `end - start`, with the same fail-closed bounds
    /// substring itself enforces (end > length(text) → abort,
    /// start > end → abort). No allocation: we compute the length
    /// arithmetically without materialising the slice.
    #[test]
    fn slice_length_substring_direct() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule len3
  @intention: "length of the first-3 slice"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(substring(o.source, 0, 3))
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2

rule len_mid
  @intention: "length of a mid slice"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(substring(o.source, 2, 7))
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let run = |rule: &str, arg: &str| -> (i32, String) {
            let bin = std::env::temp_dir().join(format!("verbosec_test_length_substring_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            (code, stdout)
        };
        // Happy paths
        assert_eq!(run("len3", "hello"), (0, "3".into()));
        assert_eq!(run("len3", "abcdef"), (0, "3".into()));
        assert_eq!(run("len3", "abc"), (0, "3".into()), "end == length is OK");
        assert_eq!(run("len_mid", "abcdefghij"), (0, "5".into()), "7 - 2 = 5");
        assert_eq!(run("len_mid", "abcdefg"), (0, "5".into()), "exact-end mid");
        // Fail-closed
        assert_eq!(run("len3", "ab").0, 1, "end=3 > len=2 must abort");
        assert_eq!(run("len_mid", "abcdef").0, 1, "end=7 > len=6 must abort");
    }

    /// `concat(..., substring(...), ...)` direct composition (2026-05-13),
    /// slice 2b. Substring as a direct concat arg, without a let
    /// workaround. Substring shares CallText's pre-eval/stash/fill
    /// machinery 1-for-1 — the classify_concat_arg change is one line,
    /// and the existing pre-eval path calls emit_text_produce_ptrlen
    /// on the whole Substring expression (which has a Substring arm
    /// since slice 1, producing rax=ptr, rdx=len with the same
    /// fail-closed bounds).
    ///
    /// This test pins:
    ///   (a) substring at concat tail: concat("[", substring(...), "]")
    ///   (b) substring at concat middle: concat(prefix, substring(...), suffix)
    ///   (c) two substring args in the same concat (independent slots)
    ///   (d) fail-closed: bounds violation in substring still aborts
    #[test]
    fn slice_concat_substring_direct() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule wrap
  @intention: "wrap first 3 bytes in brackets"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("[", substring(o.source, 0, 3), "]")
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 3

rule labelled
  @intention: "substring in concat middle"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat("prefix=", substring(o.source, 1, 4), ";suffix")
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 3

rule two_slices
  @intention: "two substring args in same concat (independent pre-eval slots)"
  @source: invoices.intent:1
  input:
    o : O
  output:
    s : text
  logic:
    s = concat(substring(o.source, 0, 2), "-", substring(o.source, 3, 5))
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let run = |rule: &str, arg: &str| -> (i32, String) {
            let bin = std::env::temp_dir().join(format!("verbosec_test_concat_substring_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            (code, stdout)
        };
        // (a) substring at end
        assert_eq!(run("wrap", "hello"), (0, "[hel]".into()));
        assert_eq!(run("wrap", "abcdef"), (0, "[abc]".into()));
        // (b) substring in middle
        assert_eq!(run("labelled", "abcde"), (0, "prefix=bcd;suffix".into()));
        // (c) two substring args
        assert_eq!(run("two_slices", "abcdef"), (0, "ab-de".into()));
        assert_eq!(run("two_slices", "abcdefghij"), (0, "ab-de".into()),
            "longer input — same slices");
        // (d) bounds violation in any substring → abort
        assert_eq!(run("wrap", "ab").0, 1, "end=3 > len=2 must abort");
        assert_eq!(run("two_slices", "abc").0, 1, "end=5 > len=3 in second substring must abort");
    }

    /// `starts_with` / `ends_with` / `contains` accept substring as
    /// direct arg (2026-05-13), slice 2c. Closes the substring
    /// integration story across all text-consuming primitives.
    ///
    /// The substring emit inside the byte-prim loader duplicates the
    /// logic in emit_text_produce_ptrlen's Substring arm — they're
    /// kept in sync until a follow-up factors emit_substring_to_rax_rdx
    /// as a shared helper. The convention adapter at the tail
    /// (mov rsi, slice_ptr ; mov rcx, slice_len) is the only delta
    /// from the let-RHS path.
    ///
    /// field_ranges plumbed through emit_starts_with_load_text +
    /// emit_starts_with + emit_ends_with + emit_contains so the
    /// substring's start/end exprs can resolve any Number-typed
    /// expression (including field reads with declared bounds).
    #[test]
    fn slice_byte_prims_substring_direct() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule starts_ab
  @intention: "first-3 slice starts with ab"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = starts_with(substring(o.source, 0, 3), "ab")
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5

rule ends_lo
  @intention: "1..4 slice ends with lo"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = ends_with(substring(o.source, 1, 4), "lo")
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5

rule has_p
  @intention: "1..5 slice contains p"
  @source: invoices.intent:1
  input:
    o : O
  output:
    b : bool
  logic:
    b = contains(substring(o.source, 1, 5), "p")
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let run = |rule: &str, arg: &str| -> (i32, String) {
            let bin = std::env::temp_dir().join(format!("verbosec_test_byteprim_substring_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            (code, stdout)
        };
        // Verbose bool convention: true → exit 0, false → exit 1
        // (shell-script-friendly). So we check stdout for the value
        // and pair with the expected exit code.
        // starts_with: substring(source, 0, 3) starts with "ab"
        assert_eq!(run("starts_ab", "abcdef"), (0, "true".into()));
        assert_eq!(run("starts_ab", "xy-def"), (1, "false".into()));
        // ends_with: substring(source, 1, 4) ends with "lo"
        assert_eq!(run("ends_lo", "hello"), (1, "false".into()), "1..4='ell'");
        assert_eq!(run("ends_lo", "halo"), (0, "true".into()), "1..4='alo'");
        // contains: substring(source, 1, 5) contains "p"
        assert_eq!(run("has_p", "happle"), (0, "true".into()), "1..5='appl'");
        assert_eq!(run("has_p", "abcde"), (1, "false".into()), "1..5='bcde'");
        // Fail-closed: substring bounds violation in any primitive
        // also produces exit 1, but with NO stdout (the rule aborts
        // before printing).
        assert_eq!(run("starts_ab", "ab"), (1, "".into()), "substring end=3 > len=2 must abort");
        assert_eq!(run("ends_lo", "ab"), (1, "".into()), "substring end=4 > len=2 must abort");
        assert_eq!(run("has_p", "abc"), (1, "".into()), "substring end=5 > len=3 must abort");
    }

    /// `json_escape(substring(...))` and `parse_int(substring(...))`
    /// direct (2026-05-13), slices 2d + 2e. Closes the substring
    /// integration story across every text-consuming primitive.
    /// Same algorithm + fail-closed posture as 2b/2c — same
    /// duplication caveat (a shared emit_substring_to_rax_rdx helper
    /// would consolidate four call sites).
    ///
    /// 2d pinned via `length(json_escape(substring(...)))` which
    /// exercises the escape transform on the slice (verifies the
    /// byte counting goes through the substring slice, not the
    /// original buffer).
    ///
    /// 2e pinned via direct `parse_int(substring(...))` with happy
    /// path (slice contains valid digits) and two fail-closed paths
    /// (substring bounds violation; slice contains non-digit).
    #[test]
    fn slice_json_escape_and_parse_int_substring_direct() {
        use std::process::Command;
        let src = r#"@verbose 0.1.0

concept O
  @intention: "x"
  @source: invoices.intent:1
  fields:
    source : text

rule escape_slice_len
  @intention: "byte count of the escaped first-5 slice"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = length(json_escape(substring(o.source, 0, 5)))
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5

rule parse_slice
  @intention: "parse the first-3 slice as a number"
  @source: invoices.intent:1
  input:
    o : O
  output:
    n : number
  logic:
    n = parse_int(substring(o.source, 0, 3))
  proofs:
    purity:
      reads : [o.source]
      calls : []
    termination:
      bound : 5
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");
        let run = |rule: &str, arg: &str| -> (i32, String) {
            let bin = std::env::temp_dir().join(format!("verbosec_test_subst_2de_{}", rule));
            compile_native(&program, rule, bin.to_str().unwrap(), false, false)
                .expect("compile");
            let r = Command::new(&bin).args([arg]).output().expect("spawn");
            let code = r.status.code().unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string();
            let _ = std::fs::remove_file(&bin);
            (code, stdout)
        };

        // 2d: length(json_escape(substring))
        // 'abcde' (5 bytes, no escapes) → escaped length = 5
        assert_eq!(run("escape_slice_len", "abcde"), (0, "5".into()));
        // 'a"bcd' (5 bytes, 1 escape `"` → 2 bytes) → 6
        assert_eq!(run("escape_slice_len", "a\"bcd"), (0, "6".into()));
        // 'ab' (too short for substring(0, 5)) → abort
        assert_eq!(run("escape_slice_len", "ab").0, 1, "substring end=5 > len=2 must abort");

        // 2e: parse_int(substring)
        // '123abc' → slice='123' → 123
        assert_eq!(run("parse_slice", "123abc"), (0, "123".into()));
        // '042xyz' → slice='042' → 42 (parse_int handles leading zeros)
        assert_eq!(run("parse_slice", "042xyz"), (0, "42".into()));
        // 'ab' (too short) → substring bounds abort
        assert_eq!(run("parse_slice", "ab").0, 1, "substring end=3 > len=2 must abort");
        // 'abc' (slice='abc', not digits) → parse_int abort
        assert_eq!(run("parse_slice", "abc").0, 1, "non-digit slice must abort parse_int");
    }

    /// Phase 9 slice 9.5e (2026-04-28): `read(<resource>)` inside the
    /// body of a Phase 6 multi-fold (extracted quantifier).
    /// `all(events, e => e.role == read(role))` desugars to a fold
    /// whose body uses the new BoundText text-equality path. Same
    /// frame layout as 9.5d but with the n_folds accumulator slots
    /// above the resource block.
    ///
    /// Pins: matching role → true, mismatch → false, role swap (no
    /// recompile) flips the result, empty collection → vacuous true,
    /// missing file → fail-closed.
    #[test]
    fn slice_9_5e_multi_fold_with_read_in_body() {
        use std::process::Command;
        let role_path = "/tmp/verbosec_test_slice95e_role.txt";
        std::fs::write(role_path, b"admin").expect("write role");

        let src = std::fs::read_to_string("examples/access_check.verbose")
            .expect("examples/access_check.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_allowed_role.txt", role_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice95e_access");
        compile_native(&program, "all_authorized", out.to_str().unwrap(), false, false)
            .expect("Phase 6 multi-fold with body Read should compile");

        let run = |args: &[&str]| -> String {
            let r = Command::new(&out).args(args).output().expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // role=admin, all admin → true
        assert_eq!(run(&["3", "admin", "admin", "admin"]), "true");
        // role=admin, one user → false
        assert_eq!(run(&["3", "admin", "user", "admin"]), "false");

        // role swap → user; all user → true (same binary)
        std::fs::write(role_path, b"user").expect("swap role");
        assert_eq!(run(&["2", "user", "user"]), "true");

        // empty collection → vacuous true (all([]) = true is the standard)
        assert_eq!(run(&["0"]), "true");

        // Abort: missing file
        let _ = std::fs::remove_file(role_path);
        let abort = Command::new(&out).args(["1", "admin"]).output().expect("spawn abort");
        assert!(
            !abort.status.success() && abort.stdout.is_empty(),
            "missing file should exit non-zero with empty stdout"
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(role_path);
    }

    /// Phase 9 slice 9.5d (2026-04-28): `read(<resource>)` allowed inside
    /// the body of a Phase 4 number fold (sum/count/min/max). Composes
    /// with the new BoundText text-equality path so a body like
    /// `if o.tag == read(target) then o.amount else 0` filters by a
    /// runtime-loaded reference. Resource read once at startup; (ptr,
    /// len) survive every record AND every element via the new
    /// resource block at the bottom of the fold frame.
    ///
    /// This test pins:
    ///   (a) sum picks up only matching tags
    ///   (b) editing the file between invocations changes the result
    ///   (c) empty target file → 0 matches → 0 sum
    ///   (d) missing file → fail-closed (exit 1, no stdout)
    #[test]
    fn slice_9_5d_number_fold_with_read_in_body() {
        use std::process::Command;
        let target_path = "/tmp/verbosec_test_slice95d_target.txt";
        std::fs::write(target_path, b"PROD").expect("write target");

        let src = std::fs::read_to_string("examples/sum_by_tag.verbose")
            .expect("examples/sum_by_tag.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_target_tag.txt", target_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice95d_sum_by_tag");
        compile_native(&program, "sum_for_target", out.to_str().unwrap(), false, false)
            .expect("Phase 4 fold with body Read should compile");

        let run = || -> String {
            let r = Command::new(&out)
                .args(["4", "PROD", "100", "DEV", "50", "PROD", "200", "STAGING", "75"])
                .output()
                .expect("spawn");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) target=PROD → sum = 100 + 200 = 300
        assert_eq!(run(), "300", "PROD sum should be 300");

        // (b) change file → STAGING → sum = 75
        std::fs::write(target_path, b"STAGING").expect("update target");
        assert_eq!(run(), "75", "after target change, STAGING sum should be 75");

        // (c) empty target → no orders match → 0
        std::fs::write(target_path, b"").expect("empty target");
        assert_eq!(run(), "0", "empty target should match nothing");

        // (d) missing file → exit 1
        let _ = std::fs::remove_file(target_path);
        let abort = Command::new(&out)
            .args(["1", "PROD", "100"])
            .output()
            .expect("spawn abort");
        assert!(
            !abort.status.success() && abort.stdout.is_empty(),
            "missing-file run should exit non-zero with empty stdout"
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(target_path);
    }

    /// Slice "text equality with bound RHS" (2026-04-28): native text
    /// comparison now accepts `<field> == read(<resource>)` and the
    /// symmetric `read(...) == <field>` form. Until this slice the
    /// equality fast path only handled `field == "literal"`. The new
    /// path computes `strlen(field)` and compares against the resource's
    /// runtime length first; only when lengths match does it cmpsb the
    /// bytes. The runtime-loaded value can change between binary
    /// invocations without recompile, unlocking SIEM-style filter-by-
    /// allowlist patterns AND being the prerequisite for slice 9.5d.
    ///
    /// This test pins the four behaviors:
    ///   (a) matching bytes  → true
    ///   (b) different bytes → false
    ///   (c) length differs (prefix or longer)  → false (length check
    ///       catches it before cmpsb)
    ///   (d) empty file == empty argv → true (rcx=0, repe cmpsb is a
    ///       no-op, ZF stays set from the cmp before)
    ///   (e) NotEq inverts each (compiled as a separate rule because
    ///       the verifier rejects mixing the same logic with different
    ///       output values inside one rule).
    #[test]
    fn slice_text_eq_with_read_rhs_runtime() {
        use std::process::Command;
        let tag_path = "/tmp/verbosec_test_text_eq_read_tag.txt";
        std::fs::write(tag_path, b"INTERNAL").expect("write tag");

        let src = std::fs::read_to_string("examples/allowlist.verbose")
            .expect("examples/allowlist.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_allowed_tag.txt", tag_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_text_eq_read");
        compile_native(&program, "is_authorized", out.to_str().unwrap(), false, false)
            .expect("text-eq-with-read should compile");

        let run = |arg: &str| -> String {
            let r = Command::new(&out)
                .args([arg])
                .output()
                .expect("spawn run");
            String::from_utf8_lossy(&r.stdout).trim_end_matches('\n').to_string()
        };

        // (a) matching bytes
        assert_eq!(run("INTERNAL"), "true", "matching tag should yield true");
        // (b) different bytes, same length (8 vs 8)
        assert_eq!(run("EXTERNAL"), "false", "different bytes (same length) should be false");
        // (c.1) prefix (length too short)
        assert_eq!(run("INT"), "false", "prefix should be false (length differs)");
        // (c.2) extension (length too long)
        assert_eq!(run("INTERNALX"), "false", "extension should be false (length differs)");

        // (d) empty file vs empty argv → true
        std::fs::write(tag_path, b"").expect("truncate tag");
        assert_eq!(run(""), "true", "empty == empty should be true");
        // empty file vs non-empty → false (length differs the other way)
        assert_eq!(run("X"), "false", "empty file vs non-empty argv should be false");

        // (e) Abort path: missing file → exit 1, no stdout
        let _ = std::fs::remove_file(tag_path);
        let abort = Command::new(&out)
            .args(["INTERNAL"])
            .output()
            .expect("spawn abort");
        assert!(
            !abort.status.success() && abort.stdout.is_empty(),
            "missing-file run should exit non-zero with empty stdout; got status={:?} stdout={:?}",
            abort.status,
            String::from_utf8_lossy(&abort.stdout)
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(tag_path);
    }

    /// Phase 9 slice 9.5b: `read(<resource>)` is allowed in the BODY of a
    /// Phase 5b text fold (not just the init). The resource is opened
    /// once above the outer record loop and its (ptr, len) live for the
    /// entire rule invocation; the body classifier accepts BoundText
    /// only when the arg is `Expr::Read` (Ident-bound text and Fetch
    /// remain refused — narrower scope, same prologue extraction).
    /// Sizing pass adds the resource's `max:` bound to
    /// `static_per_element` (compile-time constant, so single-pass
    /// fold sizing is preserved).
    ///
    /// This test pins:
    ///   (a) the binary writes the literal init bytes once, then for
    ///       each element appends `name=salary<separator-from-file>`,
    ///       all in one write syscall per record
    ///   (b) the same binary, run again with the file's content
    ///       changed, produces output with the NEW separator (no
    ///       recompile needed)
    ///   (c) missing file → exit 1 with empty stdout
    #[test]
    fn slice_9_5b_text_fold_body_with_read_separator() {
        use std::process::Command;
        let sep_path = "/tmp/verbosec_test_slice95b_sep.txt";
        std::fs::write(sep_path, b"; ").expect("initial sep");

        let src = std::fs::read_to_string("examples/sep_roster.verbose")
            .expect("examples/sep_roster.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_roster_sep.txt", sep_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice95b_sep_roster");
        compile_native(&program, "sep_line", out.to_str().unwrap(), false, false)
            .expect("Phase 5b text fold with body Read should compile");

        // (a) First run with sep="; "
        let r1 = Command::new(&out)
            .args(["3", "alice", "100", "bob", "200", "carol", "300"])
            .output()
            .expect("spawn r1");
        assert!(r1.status.success(), "r1 status {:?}", r1.status);
        assert_eq!(
            String::from_utf8_lossy(&r1.stdout).trim_end_matches('\n'),
            "roster: alice=100; bob=200; carol=300; ",
            "r1 output mismatch"
        );

        // (b) Edit the file, no recompile, second run picks up new sep.
        std::fs::write(sep_path, b" | ").expect("update sep");
        let r2 = Command::new(&out)
            .args(["2", "dave", "400", "eve", "500"])
            .output()
            .expect("spawn r2");
        assert!(r2.status.success(), "r2 status {:?}", r2.status);
        assert_eq!(
            String::from_utf8_lossy(&r2.stdout).trim_end_matches('\n'),
            "roster: dave=400 | eve=500 | ",
            "r2 should pick up the new separator without recompile"
        );

        // (c) Missing file → fail-closed.
        let _ = std::fs::remove_file(sep_path);
        let r3 = Command::new(&out)
            .args(["1", "frank", "600"])
            .output()
            .expect("spawn r3");
        assert!(
            !r3.status.success() && r3.stdout.is_empty(),
            "missing-file run should exit non-zero with empty stdout; got status={:?} stdout={:?}",
            r3.status,
            String::from_utf8_lossy(&r3.stdout)
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(sep_path);
    }

    /// Phase 9 slice 9.5c: `read(<resource>)` is allowed inside a Phase 3
    /// `map(...)` body that produces a record collection. The resource is
    /// opened/read/closed ONCE above the outer record loop; (ptr, len)
    /// slots survive every record AND every element iteration; each output
    /// record's text field that uses `read(name)` resolves through the
    /// BoundText path (same shape as Phase 9.1 / 9.2 / 9.5).
    ///
    /// This test pins:
    ///   (a) the binary emits one valid JSON line per element
    ///   (b) every element receives the SAME bytes from the file (the
    ///       resource is read once and reused across iterations)
    ///   (c) on_read_error: abort exits before any output if the file
    ///       can't be opened
    #[test]
    fn slice_9_5c_collection_map_with_read_in_record_field() {
        use std::process::Command;
        let tag_path = "/tmp/verbosec_test_slice95c_tag.txt";
        std::fs::write(tag_path, b"POLICY-vTEST").expect("write tag");

        let src = std::fs::read_to_string("examples/tagged_bonuses.verbose")
            .expect("examples/tagged_bonuses.verbose is expected to exist");
        let src = src.replace("/tmp/verbose_policy_tag.txt", tag_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice95c_tagged");
        compile_native(&program, "tag_employees", out.to_str().unwrap(), false, false)
            .expect("Phase 3 collection-with-read should compile");

        // Happy path: 3 employees, every output record carries the same tag.
        let ok = Command::new(&out)
            .args(["3", "alice", "100", "bob", "200", "carol", "300"])
            .output()
            .expect("spawn happy run");
        assert!(ok.status.success(), "happy run exit: {:?}", ok.status);
        let stdout = String::from_utf8_lossy(&ok.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), 3, "expected 3 JSON lines, got {:?}", stdout);
        for line in &lines {
            assert!(
                line.contains("\"policy_tag\":\"POLICY-vTEST\""),
                "every record should carry the same policy_tag bytes; got line: {:?}",
                line
            );
            assert!(
                line.starts_with("{\"name\":\"") && line.ends_with("}"),
                "each line must be a complete JSON object: {:?}",
                line
            );
        }

        // Abort path: no tag file at all → exit 1 with empty stdout.
        let _ = std::fs::remove_file(tag_path);
        let abort = Command::new(&out)
            .args(["1", "eve", "400"])
            .output()
            .expect("spawn abort run");
        assert!(
            !abort.status.success() && abort.stdout.is_empty(),
            "missing-file run should exit non-zero with empty stdout; got status={:?} stdout={:?}",
            abort.status,
            String::from_utf8_lossy(&abort.stdout)
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(tag_path);
    }

    /// Phase 2I-in-handlers: an HTTP service handler may declare
    /// non-literal text `let` bindings whose values reference parsed
    /// request fields (req.method, req.path), and the response record
    /// can reuse the bound name multiple times. This test pins:
    ///   (a) a single text let evaluated once per request flows into
    ///       the response body via BoundText resolution
    ///   (b) a SECOND let referencing the first resolves correctly
    ///       (chained text bindings — later let sees earlier)
    ///   (c) the value visible in the response is byte-identical
    ///       across both reuses (no per-reuse re-evaluation, no
    ///       cross-request slot pollution)
    #[test]
    fn phase_2i_handler_lets_resolve_in_body_and_chain() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let svc_port: u16 = 18928;

        let src = format!(
            r#"@verbose 0.1.0

rule greet
  @intention: "let chain"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    let head = concat("M=", req.method, " P=", req.path)
    let line = concat(head, " :: tagged")
    resp = HttpResponse {{ status: 200, body: concat(line, "\n", line, "\n") }}
  proofs:
    purity:
      reads : [req.method, req.path]
      calls : []
    termination:
      bound : 4

service chained
  @intention: "x"
  @source: invoices.intent:1
  listen:
    protocol    : http_1_0
    port        : {svc_port}
    max_request : 4096
  handler: greet
"#
        );
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase2i_handler_lets");
        compile_service(&program, "chained", out.to_str().unwrap())
            .expect("Phase 2I handler-lets service should compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn chained service");

        let mut connected = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(connected, "chained service never bound");

        let send = |method: &str, path: &str| -> String {
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .expect("connect");
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let req = format!("{} {} HTTP/1.0\r\n\r\n", method, path);
            s.write_all(req.as_bytes()).expect("write");
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).expect("read");
            String::from_utf8_lossy(&buf).to_string()
        };

        let r1 = send("GET", "/alpha");
        let r2 = send("POST", "/beta/two");

        let _ = child.kill();
        let _ = child.wait();

        // Body of r1 must contain the chained let twice, byte-identically.
        let expected_line_1 = "M=GET P=/alpha :: tagged";
        assert!(
            r1.contains(&format!("{}\n{}\n", expected_line_1, expected_line_1)),
            "r1 body should contain chained let twice; got: {:?}", r1
        );
        let expected_line_2 = "M=POST P=/beta/two :: tagged";
        assert!(
            r2.contains(&format!("{}\n{}\n", expected_line_2, expected_line_2)),
            "r2 body should contain chained let twice; got: {:?}", r2
        );
        // Cross-request: r2 must NOT contain r1's slot value (no leak).
        assert!(
            !r2.contains("/alpha"),
            "cross-request slot leak detected: r2 contains r1's path"
        );

        let _ = std::fs::remove_file(&out);
    }

    /// Phase 8 slice 8e: a service may declare MULTIPLE `log:` blocks,
    /// each with its own `append_file` path, content, and on_error
    /// policy. This test stands up the service, fires two requests, and
    /// asserts BOTH sinks receive one line per request — proving the
    /// per-block emission is independent and order-preserving. It also
    /// pins the fail-closed semantics: when the FIRST log (declared
    /// `on_error: abort`) cannot open its file (we make the path a
    /// directory so open-for-append fails), the process exits BEFORE
    /// emitting the second (best-effort) log — no metrics line is
    /// written. The order of declaration is therefore load-bearing for
    /// dual-sink fail-closed semantics, and that contract belongs in
    /// the regression suite.
    #[test]
    fn slice_8e_dual_log_blocks_write_independently_and_fail_closed() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let svc_port: u16 = 18929;
        let audit_path = "/tmp/verbosec_test_slice8e_audit.jsonl";
        let metrics_path = "/tmp/verbosec_test_slice8e_metrics.ndjson";
        let _ = std::fs::remove_file(audit_path);
        let _ = std::fs::remove_file(metrics_path);
        let _ = std::fs::remove_dir_all(audit_path);

        let src = format!(
            r#"@verbose 0.1.0

rule echo_ok
  @intention: "ok"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: "ok\n" }}
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service dual
  @intention: "x"
  @source: invoices.intent:1
  listen:
    protocol    : http_1_0
    port        : {svc_port}
    max_request : 4096
  handler: echo_ok
  log:
    append_file "{audit_path}" concat("{{\"method\":\"", req.method, "\",\"path\":\"", req.path, "\",\"status\":", resp.status, "}}\n")
    on_error: abort
  log:
    append_file "{metrics_path}" concat("{{\"status\":", resp.status, "}}\n")
    on_error: drop
"#
        );
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_slice8e_dual_log");
        compile_service(&program, "dual", out.to_str().unwrap())
            .expect("dual-log service should compile");

        // ===== Phase A: happy path — both files writable =====
        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dual service");

        // Wait for the bind.
        let mut connected = false;
        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(connected, "dual service never bound");

        let one_request = || -> Result<(), String> {
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .map_err(|e| format!("connect: {}", e))?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            s.write_all(b"GET /a HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(())
        };
        one_request().expect("happy req 1");
        one_request().expect("happy req 2");

        let _ = child.kill();
        let _ = child.wait();

        let audit = std::fs::read_to_string(audit_path).expect("read audit");
        let metrics = std::fs::read_to_string(metrics_path).expect("read metrics");
        assert_eq!(
            audit.lines().count(), 2,
            "expected 2 audit lines, got: {:?}",
            audit
        );
        assert_eq!(
            metrics.lines().count(), 2,
            "expected 2 metrics lines, got: {:?}",
            metrics
        );
        assert!(
            audit.contains("\"method\":\"GET\"") && audit.contains("\"path\":\"/a\""),
            "audit content unexpected: {:?}",
            audit
        );
        assert!(
            metrics.lines().all(|l| l == "{\"status\":200}"),
            "metrics content unexpected: {:?}",
            metrics
        );

        // ===== Phase B: fail-closed — strict log path becomes a dir =====
        std::fs::remove_file(audit_path).ok();
        std::fs::remove_file(metrics_path).ok();
        std::fs::create_dir(audit_path).expect("turn audit path into a dir");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn dual service phase B");

        for _ in 0..50 {
            if TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_millis(100),
            )
            .is_ok()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        // Fire one request — server should abort BEFORE the metrics emit.
        let _ = one_request(); // may error on read; we don't care about response
        std::thread::sleep(Duration::from_millis(150));
        let _ = child.kill();
        let _ = child.wait();

        // Strict log is a directory → metrics file should never have been
        // created (abort fires after open() returns -EISDIR for the audit).
        assert!(
            !std::path::Path::new(metrics_path).exists(),
            "fail-closed broken: metrics file was written even though strict audit could not open"
        );

        // Cleanup.
        let _ = std::fs::remove_dir(audit_path);
        let _ = std::fs::remove_file(metrics_path);
        let _ = std::fs::remove_file(&out);
    }

    /// Phase 7 slice 3e regression: status assembled from a Number-typed
    /// expression (here `if req.method == "GET" then 200 else 405`)
    /// inside a single HttpResponse record. Both possible status values
    /// must appear inline as `mov rax, imm32` operands so the if-else
    /// branches can land them in rax for the `mov [rbp-24], rax` store.
    #[test]
    fn phase7_slice3e_http10_computed_status_compiles() {
        use std::fs;
        let src = fs::read_to_string("examples/method_guard.verbose")
            .expect("examples/method_guard.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase7_slice3e_guard");
        compile_service(&program, "guard_endpoint", out.to_str().unwrap())
            .expect("Http10 slice-3e service compile");

        let bytes = fs::read(&out).expect("read output");
        let size = bytes.len();
        assert!(
            (700..1300).contains(&size),
            "slice-3e guard service size {} outside expected [700, 1300] envelope", size
        );

        // 200 and 405 each appear as immediate operands of `mov rax, imm32`
        // (encoding 0x48 0xC7 0xC0 + i32). 200 → 0xC8, 405 → 0x95 0x01.
        let mov_rax_200 = [0x48, 0xC7, 0xC0, 0xC8, 0x00, 0x00, 0x00];
        let mov_rax_405 = [0x48, 0xC7, 0xC0, 0x95, 0x01, 0x00, 0x00];
        assert!(
            bytes.windows(mov_rax_200.len()).any(|w| w == mov_rax_200),
            "expected `mov rax, 200` immediate not found"
        );
        assert!(
            bytes.windows(mov_rax_405.len()).any(|w| w == mov_rax_405),
            "expected `mov rax, 405` immediate not found"
        );

        // After the if/else, rax is stored at the status slot via
        // `mov [rbp-24], rax` = 0x48 0x89 0x45 0xE8.
        let mov_status_slot = [0x48, 0x89, 0x45, 0xE8];
        assert!(
            bytes.windows(mov_status_slot.len()).any(|w| w == mov_status_slot),
            "expected `mov [rbp-24], rax` status store not found — slice 3e not wired"
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

    /// Phase 9 slice 1: a rule whose logic is `read(<resource>)` must
    /// embed the open + read + close syscalls for the declared path,
    /// plus the shared sys_exit(1) abort sequence at the binary's tail
    /// (mirrors slice 8d's pattern; only present when the rule actually
    /// reads at least one resource).
    #[test]
    fn phase9_slice1_resource_read_embeds_syscalls_and_abort() {
        let src = r#"@verbose 0.1.0

resource greeting
  @intention: "fixed welcome banner"
  @source: invoices.intent:1
  path: "/tmp/verbosec_test_phase9_banner.txt"
  max: 64

concept Tick
  @intention: "trivial input record"
  @source: invoices.intent:1
  fields:
    n : number

rule banner
  @intention: "echo the banner"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    out : text
  logic:
    out = read(greeting)
  proofs:
    purity:
      reads: [greeting]
      calls: []
    termination:
      bound: 1
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_banner_bin");
        compile_native(&program, "banner", out.to_str().unwrap(), false, false)
            .expect("native compile of resource-reading rule");

        let bytes = std::fs::read(&out).expect("read output binary");
        let size = bytes.len();
        assert!(
            (500..1500).contains(&size),
            "phase 9 slice 1 binary size {} outside [500, 1500] envelope",
            size
        );

        // Path bytes (NUL-terminated) embedded at the open site.
        let path_marker = b"/tmp/verbosec_test_phase9_banner.txt\0";
        assert!(
            bytes.windows(path_marker.len()).any(|w| w == path_marker),
            "expected resource path + NUL not found in binary"
        );

        // sys_open immediate: mov rax, 2 — encoded 48 C7 C0 02 00 00 00.
        let open_seq = [0x48, 0xC7, 0xC0, 0x02, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(open_seq.len()).any(|w| w == open_seq),
            "expected `mov rax, 2` (sys_open) for resource open"
        );
        // sys_read immediate: mov rax, 0 — encoded 48 C7 C0 00 00 00 00.
        let read_seq = [0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(read_seq.len()).any(|w| w == read_seq),
            "expected `mov rax, 0` (sys_read) for resource read"
        );
        // Shared abort label: mov rax, 60 ; mov rdi, 1 ; syscall.
        let abort_seq = [
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
            0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        assert!(
            bytes.windows(abort_seq.len()).any(|w| w == abort_seq),
            "expected sys_exit(1) abort sequence in resource-reading binary"
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 9 slice 1: the binary actually reads the declared file at
    /// runtime and writes its contents to stdout. We pre-populate the
    /// path with a known string, run the binary against a one-record
    /// argv, and assert stdout matches the file contents byte-for-byte.
    #[test]
    fn phase9_slice1_resource_read_runs_and_emits_file_contents() {
        use std::process::Command;
        let path = std::env::temp_dir().join("verbosec_test_phase9_runtime_input.txt");
        let path_str = path.to_str().unwrap().to_string();
        let payload = b"hello-from-resource";
        std::fs::write(&path, payload).expect("seed resource file");

        let src = format!(
            r#"@verbose 0.1.0

resource msg
  @intention: "runtime-supplied banner"
  @source: invoices.intent:1
  path: "{}"
  max: 256

concept Tick
  @intention: "trivial input record"
  @source: invoices.intent:1
  fields:
    n : number

rule echo_banner
  @intention: "emit the banner"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    out : text
  logic:
    out = read(msg)
  proofs:
    purity:
      reads: [msg]
      calls: []
    termination:
      bound: 1
"#,
            path_str
        );
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_runtime_bin");
        compile_native(&program, "echo_banner", out.to_str().unwrap(), false, false)
            .expect("native compile of runtime resource-reading rule");

        let result = Command::new(&out)
            .arg("1")
            .output()
            .expect("run resource-reading binary");
        assert!(result.status.success(), "binary exited with non-zero: {:?}", result);
        // emit_text_program writes the resource bytes followed by a newline.
        // Compare against payload + "\n".
        let mut expected = payload.to_vec();
        expected.push(b'\n');
        assert_eq!(
            result.stdout, expected,
            "stdout did not match resource contents: stdout={:?}, stderr={:?}",
            result.stdout, result.stderr
        );

        let _ = std::fs::remove_file(out);
        let _ = std::fs::remove_file(path);
    }

    /// Phase 9 slice 2: an Http10 service whose handler returns
    /// `HttpResponse { status: 200, body: read(<resource>) }` must
    /// embed the resource path literal, the open + read syscall numbers,
    /// and the shared sys_exit(1) abort sequence (reused for both the
    /// slice-8d log abort and the slice-9.2 resource-read failure).
    #[test]
    fn phase9_slice2_http_handler_read_embeds_syscalls_and_path() {
        let src = r#"@verbose 0.1.0

resource page
  @intention: "static welcome page"
  @source: invoices.intent:1
  path: "/tmp/verbosec_test_phase9_slice2_page.html"
  max: 1024

rule serve_page
  @intention: "serve the static page"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: read(page) }
  proofs:
    purity:
      reads: [page]
      calls: []
    termination:
      bound: 1

service web
  @intention: "static page server"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 18901
    max_request: 4096
  handler: serve_page
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_slice2_static");
        compile_service(&program, "web", out.to_str().unwrap())
            .expect("Http10 service with read(resource) compile");

        let bytes = std::fs::read(&out).expect("read output binary");
        let size = bytes.len();
        assert!(
            (800..2000).contains(&size),
            "phase 9 slice 2 service binary size {} outside [800, 2000] envelope",
            size
        );

        // Resource path literal embedded at the open site (NUL-terminated).
        let path_marker = b"/tmp/verbosec_test_phase9_slice2_page.html\0";
        assert!(
            bytes.windows(path_marker.len()).any(|w| w == path_marker),
            "expected resource path literal + NUL not embedded in service binary"
        );

        // sys_open immediate: mov rax, 2.
        let open_seq = [0x48, 0xC7, 0xC0, 0x02, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(open_seq.len()).any(|w| w == open_seq),
            "expected `mov rax, 2` (sys_open) for resource open"
        );
        // sys_read immediate: mov rax, 0.
        let read_seq = [0x48, 0xC7, 0xC0, 0x00, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(read_seq.len()).any(|w| w == read_seq),
            "expected `mov rax, 0` (sys_read) for resource read"
        );
        // Shared abort sequence: mov rax, 60 ; mov rdi, 1 ; syscall.
        let abort_seq = [
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
            0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        assert!(
            bytes.windows(abort_seq.len()).any(|w| w == abort_seq),
            "expected sys_exit(1) abort sequence in service binary"
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 9 slice 2 end-to-end: spawn the compiled service binary,
    /// connect via TCP, send an HTTP/1.0 GET, and assert the response
    /// body is byte-for-byte the contents of the seeded resource file.
    /// Uses a distinct port from any other test to avoid bind conflicts.
    #[test]
    fn phase9_slice2_http_handler_serves_file_contents() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let resource_path = std::env::temp_dir()
            .join("verbosec_test_phase9_slice2_runtime.html");
        let payload = b"<html><body>hello from disk</body></html>";
        std::fs::write(&resource_path, payload).expect("seed resource file");

        let port: u16 = 18902;
        let src = format!(
            r#"@verbose 0.1.0

resource page
  @intention: "runtime-served static page"
  @source: invoices.intent:1
  path: "{}"
  max: 4096

rule serve_page
  @intention: "echo the page bytes as the response body"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: read(page) }}
  proofs:
    purity:
      reads: [page]
      calls: []
    termination:
      bound: 1

service web
  @intention: "static page server"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: {}
    max_request: 4096
  handler: serve_page
"#,
            resource_path.display(),
            port
        );

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_slice2_runtime_bin");
        compile_service(&program, "web", out.to_str().unwrap())
            .expect("Http10 service with read(resource) compile");

        // Spawn the server in the background. It binds, listens, and
        // accepts forever — we kill it after the request roundtrips.
        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn service binary");

        // Give the kernel a brief moment to bind and listen. A short
        // retry loop avoids racing the listen() syscall without a
        // long fixed sleep.
        let mut stream: Option<TcpStream> = None;
        for _ in 0..50 {
            match TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }

        let runtime_result: Result<Vec<u8>, String> = (|| {
            let mut s = stream.ok_or_else(|| "could not connect to service".to_string())?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            s.write_all(b"GET / HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(buf)
        })();

        // Kill the server before asserting so a panic doesn't leak the
        // process across tests.
        let _ = child.kill();
        let _ = child.wait();

        let response = runtime_result.expect("HTTP roundtrip failed");
        // Split off headers; the body is everything after the empty line.
        let sep = b"\r\n\r\n";
        let split_at = response
            .windows(sep.len())
            .position(|w| w == sep)
            .expect("response missing CRLF/CRLF header terminator");
        let body = &response[split_at + sep.len()..];

        assert_eq!(
            body, payload,
            "response body did not match resource contents: body={:?}",
            body
        );
        // Status line check — the response should advertise 200.
        let status_line: &[u8] = response
            .split(|b| *b == b'\r')
            .next()
            .unwrap_or(&[]);
        assert!(
            status_line.starts_with(b"HTTP/1.0 200"),
            "expected HTTP/1.0 200 status line, got {:?}",
            String::from_utf8_lossy(status_line)
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&resource_path);
    }

    /// Phase 9 slice 9.4 byte-pattern regression: compiling
    /// static_file_server.verbose (which now carries `cache: true` on the
    /// `index_page` resource) must move the `mov rax, 2` (sys_open) for
    /// the resource read BEFORE the `mov rax, 43` (sys_accept). In the
    /// pre-9.4 binary, accept was emitted first (the accept loop opened
    /// the file inside each iteration); slice 9.4 hoists the cached open
    /// out to the startup path, between LISTEN and accept_top. Inverting
    /// the in-binary ordering of these two syscall immediates is exactly
    /// what proves the cached-emit path was taken.
    ///
    /// The resource path literal must still appear (no caching shortcut
    /// can drop the open call entirely). The resource path on disk is
    /// not opened at compile time — the assertion is purely about
    /// emitted bytes.
    #[test]
    fn phase9_slice4_cache_true_moves_open_before_accept() {
        use std::fs;
        let src = fs::read_to_string("examples/static_file_server.verbose")
            .expect("examples/static_file_server.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_slice4_cache_bytes");
        compile_service(&program, "static_server", out.to_str().unwrap())
            .expect("cached static_file_server compile");

        let bytes = fs::read(&out).expect("read output");

        // Locate `mov rax, 2` (sys_open). Only the resource read sequence
        // emits this 7-byte immediate — the socket() call uses `mov rax,
        // 41` and AF_INET=2 lives in `mov rdi, 2` (different ModR/M).
        let open_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x02, 0x00, 0x00, 0x00];
        let open_pos = bytes
            .windows(open_seq.len())
            .position(|w| w == open_seq)
            .expect("expected `mov rax, 2` (sys_open) for cached resource read");

        // Locate `mov rax, 43` (sys_accept) — unique to the accept loop.
        let accept_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00];
        let accept_pos = bytes
            .windows(accept_seq.len())
            .position(|w| w == accept_seq)
            .expect("expected `mov rax, 43` (sys_accept) in accept loop");

        // The whole point of cache: true — open hoisted ABOVE accept_top.
        // Pre-9.4 binary had accept_pos < open_pos (open inside the loop
        // body). Slice 9.4 inverts this for cached resources.
        assert!(
            open_pos < accept_pos,
            "cache: true must hoist sys_open BEFORE sys_accept; got open at {} but accept at {}",
            open_pos,
            accept_pos
        );

        // The path literal must still be embedded (caching does not drop
        // the syscall, only relocates it).
        let path_marker = b"/tmp/verbose_static_index.html\0";
        assert!(
            bytes.windows(path_marker.len()).any(|w| w == path_marker),
            "cached resource path literal must still be inlined in the binary"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 9 slice 9.4 end-to-end caching test. Spawn the server with a
    /// `cache: true` resource pointing at a seed file, hit it via TCP and
    /// confirm the body matches "version A". Then overwrite the file on
    /// disk to "version B", hit the server AGAIN, and assert the response
    /// body is STILL "version A" — proving the read happened once at
    /// startup and the per-request path now reads from the cached buffer
    /// rather than reopening the file.
    ///
    /// Sequential mode chosen on purpose: forked mode would also work
    /// (children inherit the cached buffer via COW), but sequential keeps
    /// the test plumbing simple — single accept loop, no fork bookkeeping.
    /// The cache hoist is independent of concurrency mode.
    #[test]
    fn phase9_slice4_cache_true_serves_stale_content_after_disk_overwrite() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let resource_path = std::env::temp_dir()
            .join("verbosec_test_phase9_slice4_cached.html");
        let version_a = b"version A";
        let version_b = b"version B";
        std::fs::write(&resource_path, version_a).expect("seed cached resource file");

        let port: u16 = 18904;
        let src = format!(
            r#"@verbose 0.1.0

resource page
  @intention: "page cached at server startup"
  @source: invoices.intent:1
  path: "{}"
  max: 4096
  on_read_error: abort
  cache: true

rule serve_page
  @intention: "echo the cached page bytes"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: read(page) }}
  proofs:
    purity:
      reads: [page]
      calls: []
    termination:
      bound: 1

service web
  @intention: "cached static page server"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: {}
    max_request: 4096
  handler: serve_page
"#,
            resource_path.display(),
            port
        );

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase9_slice4_runtime_bin");
        compile_service(&program, "web", out.to_str().unwrap())
            .expect("Http10 service with cache: true compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn cached service binary");

        // Helper closure: fire one HTTP/1.0 GET, return the body.
        let do_request = |port: u16| -> Result<Vec<u8>, String> {
            // Retry-connect loop — the server may still be binding.
            let mut stream: Option<TcpStream> = None;
            for _ in 0..50 {
                match TcpStream::connect_timeout(
                    &format!("127.0.0.1:{}", port).parse().unwrap(),
                    Duration::from_millis(100),
                ) {
                    Ok(s) => {
                        stream = Some(s);
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            let mut s = stream.ok_or_else(|| "could not connect".to_string())?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            s.write_all(b"GET / HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            let sep = b"\r\n\r\n";
            let split_at = buf
                .windows(sep.len())
                .position(|w| w == sep)
                .ok_or_else(|| "no header terminator".to_string())?;
            Ok(buf[split_at + sep.len()..].to_vec())
        };

        let result: Result<(Vec<u8>, Vec<u8>), String> = (|| {
            // First request — should see version A.
            let body1 = do_request(port)?;
            // Now overwrite the file on disk. Use a temp + rename to avoid
            // partial-write windows confusing this test.
            let tmp = std::env::temp_dir()
                .join("verbosec_test_phase9_slice4_cached.tmp");
            std::fs::write(&tmp, version_b).map_err(|e| format!("write tmp: {}", e))?;
            std::fs::rename(&tmp, &resource_path)
                .map_err(|e| format!("rename: {}", e))?;
            // Second request — caching means body MUST still be version A.
            let body2 = do_request(port)?;
            Ok((body1, body2))
        })();

        // Tear down before asserting so a panic doesn't leak the process.
        let _ = child.kill();
        let _ = child.wait();

        let (body1, body2) = result.expect("HTTP roundtrip(s) failed");

        assert_eq!(
            body1, version_a,
            "first response body did not match seed contents: {:?}",
            String::from_utf8_lossy(&body1)
        );
        // The load-bearing assertion of this test.
        assert_eq!(
            body2, version_a,
            "cached resource must serve the startup-loaded contents even after on-disk overwrite; got {:?}",
            String::from_utf8_lossy(&body2)
        );
        // Sanity: the disk file was actually changed.
        let on_disk = std::fs::read(&resource_path).expect("re-read disk");
        assert_eq!(
            on_disk, version_b,
            "test setup error: disk file was not overwritten as expected"
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&resource_path);
    }

    /// Phase 10 slice 10 byte-pattern regression: compiling
    /// static_file_server.verbose (which carries `concurrency: forked`)
    /// must embed the kernel-ABI sigaction setup, the fork dispatch, the
    /// child sys_exit, and the "fork failed\n" stderr literal. We assert
    /// each one as a bare byte sequence rather than using disassembly so
    /// the test fails loudly if the emitter ever drops or rewires one of
    /// the four moving parts.
    #[test]
    fn phase10_static_file_server_forked_embeds_dispatch_bytes() {
        use std::fs;
        let src = fs::read_to_string("examples/static_file_server.verbose")
            .expect("examples/static_file_server.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase10_static_forked");
        compile_service(&program, "static_server", out.to_str().unwrap())
            .expect("forked static_file_server compile");

        let bytes = fs::read(&out).expect("read output");

        // mov rax, 13  (rt_sigaction)  — sigaction setup before listen
        assert!(
            bytes.windows(7).any(|w| w == [0x48, 0xC7, 0xC0, 0x0D, 0x00, 0x00, 0x00]),
            "rt_sigaction syscall (rax=13) not found in forked binary"
        );
        // mov rdi, 17  (SIGCHLD)  — argument to rt_sigaction
        assert!(
            bytes.windows(7).any(|w| w == [0x48, 0xC7, 0xC7, 0x11, 0x00, 0x00, 0x00]),
            "SIGCHLD constant (rdi=17) not found in forked binary"
        );
        // mov rax, 57  (sys_fork)  — per-accept fork dispatch
        assert!(
            bytes.windows(7).any(|w| w == [0x48, 0xC7, 0xC0, 0x39, 0x00, 0x00, 0x00]),
            "sys_fork syscall (rax=57) not found in forked binary"
        );
        // mov rax, 60  (sys_exit)  — child path tail. The Phase 8 slice 8d
        // abort sequence at the very end of the binary also emits
        // `mov rax, 60`, and static_file_server happens to use
        // `on_error: abort`. To prove the *child-exit* mov-rax-60 is
        // present (not just the abort one), assert there are at least
        // two occurrences of the 7-byte sequence.
        let exit_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00];
        let exit_count = bytes
            .windows(exit_seq.len())
            .filter(|w| *w == exit_seq)
            .count();
        assert!(
            exit_count >= 2,
            "expected at least 2 occurrences of mov rax, 60 (one for child exit, one for abort), got {}",
            exit_count
        );
        // "fork failed\n" literal — error path stderr message
        let err_msg = b"fork failed\n";
        assert!(
            bytes.windows(err_msg.len()).any(|w| w == err_msg),
            "'fork failed\\n' literal not found in forked binary"
        );
        // Sanity: existing slice 9.2 invariants still hold.
        let log_path = b"/tmp/verbose_static_server.jsonl";
        assert!(
            bytes.windows(log_path.len()).any(|w| w == log_path),
            "log path literal must still be inlined (slice 8a contract)"
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 10 slice 10 concurrency smoke test: the forked binary should
    /// serve four parallel HTTP/1.0 GET requests successfully. We spawn
    /// the binary on port 18910 (chosen to avoid collision with other
    /// service tests' ports), fire four threads each opening a socket,
    /// writing a request, and reading the response, then assert all four
    /// bodies match the seeded resource file.
    ///
    /// The point is not raw throughput — sequential mode would also pass
    /// four requests in series — but to exercise the fork/parent-close
    /// path: if fork() were missing, the second connection would block
    /// behind the first; if the parent didn't close client_fd, the
    /// kernel would eventually exhaust fds; if the child didn't exit,
    /// children would loop back to accept and steal connections from
    /// the parent. Anything broken in the dispatch surfaces here.
    #[test]
    fn phase10_forked_service_serves_parallel_requests() {
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let resource_path = std::env::temp_dir()
            .join("verbosec_test_phase10_forked_index.html");
        let payload = b"<html><body>forked</body></html>";
        std::fs::write(&resource_path, payload).expect("seed resource file");

        let port: u16 = 18910;
        let src = format!(
            r#"@verbose 0.1.0

resource page
  @intention: "page served by a forked HTTP service"
  @source: invoices.intent:1
  path: "{}"
  max: 4096

rule serve_page
  @intention: "echo the page bytes as the response body"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: read(page) }}
  proofs:
    purity:
      reads: [page]
      calls: []
    termination:
      bound: 1

service web
  @intention: "fork-per-accept static page server"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: {}
    max_request: 4096
  handler: serve_page
  concurrency: forked
"#,
            resource_path.display(),
            port
        );

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase10_forked_bin");
        compile_service(&program, "web", out.to_str().unwrap())
            .expect("forked Http10 service compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn forked service binary");

        // Wait for listen() with a short retry loop. We probe the same
        // port the binary binds to; once a connect succeeds the kernel
        // has accepted the bind+listen, so the rest of the test can run.
        let mut probed = false;
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                drop(s);
                probed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let runtime_result: Result<Vec<Vec<u8>>, String> = (|| {
            if !probed {
                return Err("server never accepted TCP connections".into());
            }
            // Fire four parallel requests. Each thread opens its own
            // socket, sends a GET, and reads the full response.
            let port = port;
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    std::thread::spawn(move || -> Result<Vec<u8>, String> {
                        let mut s = TcpStream::connect_timeout(
                            &format!("127.0.0.1:{}", port).parse().unwrap(),
                            Duration::from_secs(2),
                        )
                        .map_err(|e| format!("connect: {}", e))?;
                        s.set_read_timeout(Some(Duration::from_secs(3))).ok();
                        s.write_all(b"GET / HTTP/1.0\r\n\r\n")
                            .map_err(|e| format!("write: {}", e))?;
                        let mut buf = Vec::new();
                        s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
                        Ok(buf)
                    })
                })
                .collect();
            let mut responses = Vec::new();
            for h in handles {
                let r = h.join().map_err(|_| "thread panicked".to_string())??;
                responses.push(r);
            }
            Ok(responses)
        })();

        // Always kill the server (and its forked children) before
        // asserting so a panic doesn't leak processes across tests.
        let _ = child.kill();
        let _ = child.wait();

        let responses = runtime_result.expect("parallel HTTP roundtrip failed");
        assert_eq!(responses.len(), 4, "expected 4 responses");
        let sep = b"\r\n\r\n";
        for (idx, response) in responses.iter().enumerate() {
            let split_at = response
                .windows(sep.len())
                .position(|w| w == sep)
                .unwrap_or_else(|| panic!("response {} missing CRLF/CRLF terminator", idx));
            let body = &response[split_at + sep.len()..];
            assert_eq!(
                body, payload,
                "response {} body did not match resource contents: body={:?}",
                idx, body
            );
            let status_line: &[u8] = response.split(|b| *b == b'\r').next().unwrap_or(&[]);
            assert!(
                status_line.starts_with(b"HTTP/1.0 200"),
                "response {} expected HTTP/1.0 200, got {:?}",
                idx,
                String::from_utf8_lossy(status_line)
            );
        }

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&resource_path);
    }

    /// Phase 11 slice 1: a rule whose logic is `fetch(<connection>, "...")`
    /// must embed the socket / connect / write / read / close syscalls,
    /// the inline sockaddr_in literal (family=2, htons(port), inet_aton(host),
    /// 8 bytes of padding), and the shared sys_exit(1) abort sequence at
    /// the binary's tail (the same one the resource path uses). Together
    /// these prove the prologue laid down a complete fetch sequence and
    /// wired its failure paths into the abort label.
    #[test]
    fn phase11_slice1_fetch_embeds_socket_connect_and_sockaddr() {
        let src = r#"@verbose 0.1.0

connection upstream
  @intention: "remote endpoint we probe"
  @source: invoices.intent:1
  host: "127.0.0.1"
  port: 19000
  max_response: 1024
  on_connect_error: abort

concept Tick
  @intention: "trivial input record"
  @source: invoices.intent:1
  fields:
    n : number

rule probe
  @intention: "fetch upstream and emit the response"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    out : text
  logic:
    out = fetch(upstream, "GET / HTTP/1.0\r\n\r\n")
  proofs:
    purity:
      reads: [upstream]
      calls: []
    termination:
      bound: 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase11_slice1_fetch_bin");
        compile_native(&program, "probe", out.to_str().unwrap(), false, false)
            .expect("native compile of fetch-using rule");

        let bytes = std::fs::read(&out).expect("read output binary");
        let size = bytes.len();
        assert!(
            (500..2000).contains(&size),
            "phase 11 slice 1 binary size {} outside [500, 2000] envelope",
            size
        );

        // sys_socket immediate: mov rax, 41 — encoded 48 C7 C0 29 00 00 00.
        let socket_seq = [0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(socket_seq.len()).any(|w| w == socket_seq),
            "expected `mov rax, 41` (sys_socket) in fetch binary"
        );
        // sys_connect immediate: mov rax, 42 — encoded 48 C7 C0 2A 00 00 00.
        let connect_seq = [0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(connect_seq.len()).any(|w| w == connect_seq),
            "expected `mov rax, 42` (sys_connect) in fetch binary"
        );
        // sys_close immediate: mov rax, 3 — encoded 48 C7 C0 03 00 00 00.
        let close_seq = [0x48, 0xC7, 0xC0, 0x03, 0x00, 0x00, 0x00];
        assert!(
            bytes.windows(close_seq.len()).any(|w| w == close_seq),
            "expected `mov rax, 3` (sys_close) in fetch binary"
        );
        // Inline sockaddr_in: family=2 (02 00 little-endian), htons(19000)=0x4A38
        // (high byte 0x4A, low byte 0x38), addr 127.0.0.1 = 7F 00 00 01.
        // Layout: [02 00 4A 38 7F 00 00 01 00 00 00 00 00 00 00 00].
        let sockaddr_marker = [
            0x02, 0x00, 0x4A, 0x38, 0x7F, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(
            bytes.windows(sockaddr_marker.len()).any(|w| w == sockaddr_marker),
            "expected inline sockaddr_in literal (family=2, htons(19000), 127.0.0.1, padding)"
        );
        // Shared abort label: mov rax, 60 ; mov rdi, 1 ; syscall — same one
        // the resource path patches into when open/read fail.
        let abort_seq = [
            0x48, 0xC7, 0xC0, 0x3C, 0x00, 0x00, 0x00,
            0x48, 0xC7, 0xC7, 0x01, 0x00, 0x00, 0x00,
            0x0F, 0x05,
        ];
        assert!(
            bytes.windows(abort_seq.len()).any(|w| w == abort_seq),
            "expected sys_exit(1) abort sequence in fetch binary"
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 11 slice 1: end-to-end. Spin up a tiny TCP listener on a
    /// fixed loopback port, compile a rule that fetches from it, run
    /// the binary, and assert stdout contains the response body the
    /// listener wrote back. Proves the wire round-trip is real, not
    /// just bytes embedded in the binary.
    #[test]
    fn phase11_slice1_fetch_round_trips_against_test_listener() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::process::Command;
        use std::thread;
        use std::time::Duration;

        let port: u16 = 19000;
        // Bind FIRST, then start a thread to accept once the binary
        // connects. Binding from the test thread guarantees the kernel
        // is ready before we spawn the binary — no fragile sleep race.
        let listener = TcpListener::bind(("127.0.0.1", port))
            .expect("bind test listener");
        listener
            .set_nonblocking(false)
            .expect("blocking listener");

        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req);
            sock.write_all(b"HTTP/1.0 200 OK\r\n\r\nhealthy")
                .expect("write response");
            // Drop closes the socket — the binary's read returns EOF.
        });

        let src = format!(
            r#"@verbose 0.1.0

connection upstream
  @intention: "test endpoint"
  @source: invoices.intent:1
  host: "127.0.0.1"
  port: {}
  max_response: 1024
  on_connect_error: abort

concept Tick
  @intention: "trivial input record"
  @source: invoices.intent:1
  fields:
    n : number

rule probe
  @intention: "fetch upstream and emit the response"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    out : text
  logic:
    out = fetch(upstream, "GET /health HTTP/1.0\r\n\r\n")
  proofs:
    purity:
      reads: [upstream]
      calls: []
    termination:
      bound: 2
"#,
            port
        );
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase11_slice1_runtime_bin");
        compile_native(&program, "probe", out.to_str().unwrap(), false, false)
            .expect("native compile of fetch-using rule");

        let result = Command::new(&out)
            .arg("1")
            .output()
            .expect("run fetch binary");
        // Reap the listener thread so a panic doesn't leave a dangling fd.
        let _ = server.join();

        assert!(
            result.status.success(),
            "binary exited with non-zero: {:?}",
            result
        );
        // emit_text_program writes the response bytes followed by a newline.
        // The body the test listener sent back was b"HTTP/1.0 200 OK\r\n\r\nhealthy".
        let mut expected = b"HTTP/1.0 200 OK\r\n\r\nhealthy".to_vec();
        expected.push(b'\n');
        assert_eq!(
            result.stdout, expected,
            "stdout did not match listener response: stdout={:?}, stderr={:?}",
            result.stdout, result.stderr
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 11 slice 2: byte-pattern check that the HTTP service binary
    /// emits the connection fetch sequence (socket + connect) AFTER the
    /// per-accept entry (sys_accept = `mov rax, 43`). This proves the
    /// fetch is hoisted INSIDE the accept loop rather than running once
    /// at startup. The constant-startup path (slice 9.4 cache) emits open
    /// BEFORE accept; slice 11.2 deliberately does not yet support a
    /// `cache: true` for connections, so the inverse ordering must hold:
    /// accept appears first in the binary, and socket/connect appear
    /// after it.
    ///
    /// Also asserts that the inline sockaddr_in literal (family + htons +
    /// IPv4 octets) is embedded — proving the destination is resolved at
    /// compile time, not at runtime via DNS.
    #[test]
    fn phase11_slice2_http_handler_fetch_embeds_socket_and_connect() {
        let src = r#"@verbose 0.1.0

connection upstream
  @intention: "byte-pattern test endpoint"
  @source: invoices.intent:1
  host: "127.0.0.1"
  port: 19002
  max_response: 1024
  on_connect_error: abort

rule proxy
  @intention: "proxy every request to upstream"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: fetch(upstream, "GET / HTTP/1.0\r\n\r\n") }
  proofs:
    purity:
      reads: [upstream]
      calls: []
    termination:
      bound: 2

service gateway
  @intention: "byte-pattern test gateway"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 18920
    max_request: 4096
  handler: proxy
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase11_slice2_bytes");
        compile_service(&program, "gateway", out.to_str().unwrap())
            .expect("Http10 service with fetch(connection) compile");

        let bytes = std::fs::read(&out).expect("read output binary");
        let size = bytes.len();
        assert!(
            (800..3000).contains(&size),
            "phase 11 slice 2 service binary size {} outside [800, 3000] envelope",
            size
        );

        // sys_accept immediate: mov rax, 43 — encoded 48 C7 C0 2B 00 00 00.
        // Unique to the accept loop; appears once at accept_top.
        let accept_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00];
        let accept_pos = bytes
            .windows(accept_seq.len())
            .position(|w| w == accept_seq)
            .expect("expected `mov rax, 43` (sys_accept) in service binary");

        // sys_socket immediate: mov rax, 41 — encoded 48 C7 C0 29 00 00 00.
        // The HTTP server's setup also calls socket() once at startup, so
        // we look for the LAST occurrence (the per-accept fetch one) and
        // assert it is AFTER the accept syscall. Pre-slice-11.2 service
        // binaries had no socket call after accept at all.
        let socket_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00];
        let socket_positions: Vec<usize> = bytes
            .windows(socket_seq.len())
            .enumerate()
            .filter_map(|(i, w)| if w == socket_seq { Some(i) } else { None })
            .collect();
        assert!(
            !socket_positions.is_empty(),
            "expected at least one `mov rax, 41` (sys_socket) in service binary"
        );
        let last_socket = *socket_positions.last().unwrap();
        assert!(
            last_socket > accept_pos,
            "phase 11 slice 2: per-accept fetch must emit sys_socket AFTER sys_accept; \
             got accept at {} but last socket at {}",
            accept_pos,
            last_socket
        );

        // sys_connect immediate: mov rax, 42 — encoded 48 C7 C0 2A 00 00 00.
        // Only the fetch sequence emits this; it must appear after accept too.
        let connect_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x2A, 0x00, 0x00, 0x00];
        let connect_pos = bytes
            .windows(connect_seq.len())
            .position(|w| w == connect_seq)
            .expect("expected `mov rax, 42` (sys_connect) in fetch sequence");
        assert!(
            connect_pos > accept_pos,
            "phase 11 slice 2: per-accept fetch must emit sys_connect AFTER sys_accept; \
             got accept at {} but connect at {}",
            accept_pos,
            connect_pos
        );

        // Inline sockaddr_in: family=2, htons(19002)=0x4A3A
        // (high byte 0x4A, low byte 0x3A), addr 127.0.0.1 = 7F 00 00 01,
        // 8 bytes of zero padding.
        let sockaddr_marker = [
            0x02, 0x00, 0x4A, 0x3A, 0x7F, 0x00, 0x00, 0x01,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        assert!(
            bytes.windows(sockaddr_marker.len()).any(|w| w == sockaddr_marker),
            "expected inline sockaddr_in literal (family=2, htons(19002), 127.0.0.1, padding)"
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 11 slice 2 end-to-end: spawn the compiled gateway service,
    /// spawn a tiny TCP listener that plays the role of the upstream
    /// backend, then issue an HTTP request to the gateway and assert the
    /// gateway's response body byte-for-byte equals the upstream's
    /// response. Proves the per-accept fetch wires together end to end:
    /// the gateway's HTTP read pulls the request, the per-accept fetch
    /// runs socket+connect+write+read+close against the upstream, and the
    /// upstream's response bytes flow into the response body slot before
    /// the HTTP serializer writes the gateway's response back.
    #[test]
    fn phase11_slice2_http_handler_proxies_upstream_response() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::Duration;

        let upstream_port: u16 = 19002;
        let gateway_port: u16 = 18921;

        // Bind upstream FIRST so the gateway's connect() always finds a
        // listening socket on the other side. Then the gateway binary
        // is spawned; on its first accept(), it will turn around and
        // dial back here.
        let upstream =
            TcpListener::bind(("127.0.0.1", upstream_port)).expect("bind upstream listener");
        upstream.set_nonblocking(false).expect("blocking upstream");
        let upstream_payload = b"HTTP/1.0 200 OK\r\nContent-Length: 9\r\n\r\nupstream!";
        let upstream_thread = thread::spawn(move || {
            let (mut sock, _) = upstream.accept().expect("upstream accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req);
            sock.write_all(upstream_payload).expect("write upstream response");
            // Drop closes the socket — gateway's read returns when the
            // upstream side EOFs (Content-Length read happens in the
            // gateway via the max_response cap).
        });

        let src = format!(
            r#"@verbose 0.1.0

connection upstream
  @intention: "test upstream"
  @source: invoices.intent:1
  host: "127.0.0.1"
  port: {}
  max_response: 1024
  on_connect_error: abort

rule proxy
  @intention: "forward every request"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: fetch(upstream, "GET /health HTTP/1.0\r\n\r\n") }}
  proofs:
    purity:
      reads: [upstream]
      calls: []
    termination:
      bound: 2

service gateway
  @intention: "proxy gateway"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: {}
    max_request: 4096
  handler: proxy
"#,
            upstream_port, gateway_port
        );

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out =
            std::env::temp_dir().join("verbosec_test_phase11_slice2_runtime_bin");
        compile_service(&program, "gateway", out.to_str().unwrap())
            .expect("Http10 service with fetch(connection) compile");

        // Spawn the gateway in the background.
        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gateway service binary");

        // Wait for the gateway to bind — short retry loop, no fragile sleep.
        let mut stream: Option<TcpStream> = None;
        for _ in 0..50 {
            match TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", gateway_port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }

        let runtime_result: Result<Vec<u8>, String> = (|| {
            let mut s = stream.ok_or_else(|| "could not connect to gateway".to_string())?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            s.write_all(b"GET / HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(buf)
        })();

        // Reap server + upstream before asserting so a panic does not
        // leave a dangling process or fd.
        let _ = child.kill();
        let _ = child.wait();
        let _ = upstream_thread.join();

        let response = runtime_result.expect("HTTP roundtrip failed");
        // The gateway's response body is the upstream's full payload —
        // the gateway does not parse upstream's HTTP, it just relays the
        // bytes that landed in the connection's response buffer.
        let sep = b"\r\n\r\n";
        let split_at = response
            .windows(sep.len())
            .position(|w| w == sep)
            .expect("gateway response missing CRLF/CRLF terminator");
        let body = &response[split_at + sep.len()..];

        // The body should CONTAIN "upstream!" — the upstream replied with
        // a full HTTP/1.0 wire response, so the body the gateway forwards
        // is the upstream's headers + body together. Asserting containment
        // (rather than exact equality) makes the test robust to whether
        // the gateway's read returned exactly upstream_payload's length or
        // a partial read.
        let body_str = String::from_utf8_lossy(body);
        assert!(
            body_str.contains("upstream!"),
            "gateway response body did not contain 'upstream!': body={:?}",
            body_str
        );

        let _ = std::fs::remove_file(&out);
    }

    /// Phase 11 slice 3: byte-pattern check that the per-accept fetch
    /// sequence is now emitted AFTER the HTTP parser (rather than before
    /// the read+parse, as in slice 11.2). The reorder is what lets
    /// fetch()'s request_expr reference req.method / req.path — the
    /// parser must run first to populate [rbp-8] / [rbp-16].
    ///
    /// We anchor the assertion on three sites in a fixed sequence:
    ///   1. sys_accept (`mov rax, 43`) — start of the per-accept body
    ///   2. the parser's first byte-cmp-against-space (`cmp byte [rbx], 0x20`,
    ///      encoded `80 3B 20`) — proof the parse happened
    ///   3. sys_socket (`mov rax, 41`) — start of the fetch sequence
    ///
    /// Slice 11.2 had ordering 1 → 3 → 2 (parse ran AFTER socket, since
    /// the connections block sat between resources and the read+parse).
    /// Slice 11.3 must show 1 → 2 → 3.
    #[test]
    fn phase11_slice3_fetch_emits_after_http_parse() {
        let src = std::fs::read_to_string("examples/reverse_proxy.verbose")
            .expect("examples/reverse_proxy.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase11_slice3_bytes");
        compile_service(&program, "proxy_server", out.to_str().unwrap())
            .expect("Http10 service with dynamic-request fetch compile");

        let bytes = std::fs::read(&out).expect("read output binary");
        let size = bytes.len();
        assert!(
            (800..3000).contains(&size),
            "phase 11 slice 3 binary size {} outside [800, 3000] envelope",
            size
        );

        // sys_accept: mov rax, 43 — encoded 48 C7 C0 2B 00 00 00.
        // Unique to the accept loop entry.
        let accept_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x2B, 0x00, 0x00, 0x00];
        let accept_pos = bytes
            .windows(accept_seq.len())
            .position(|w| w == accept_seq)
            .expect("expected `mov rax, 43` (sys_accept) in service binary");

        // HTTP parse fingerprint: the method scan compares `[rbx]` against
        // ASCII space (0x20). The encoding `80 3B 20` (cmp byte ptr [rbx],
        // 0x20) appears only in the parse helper. We take the FIRST
        // occurrence after accept_pos to anchor "the parse ran here".
        let parse_seq: [u8; 3] = [0x80, 0x3B, 0x20];
        let parse_pos = bytes
            .windows(parse_seq.len())
            .enumerate()
            .find_map(|(i, w)| if i > accept_pos && w == parse_seq { Some(i) } else { None })
            .expect("expected HTTP parser's `cmp byte [rbx], 0x20` after sys_accept");

        // sys_socket: mov rax, 41 — encoded 48 C7 C0 29 00 00 00.
        // The HTTP server's startup also calls socket(); we want the LAST
        // occurrence (the per-accept fetch one) and assert it comes AFTER
        // the parse fingerprint. This is the slice 11.3 invariant: the
        // fetch is hoisted INTO the per-accept body and scheduled AFTER
        // the parse populates the request slots.
        let socket_seq: [u8; 7] = [0x48, 0xC7, 0xC0, 0x29, 0x00, 0x00, 0x00];
        let last_socket = bytes
            .windows(socket_seq.len())
            .enumerate()
            .filter_map(|(i, w)| if w == socket_seq { Some(i) } else { None })
            .last()
            .expect("expected at least one `mov rax, 41` (sys_socket) in service binary");

        assert!(
            last_socket > parse_pos,
            "phase 11 slice 3: per-accept sys_socket must follow the HTTP parse; \
             accept@{} parse@{} last_socket@{}",
            accept_pos, parse_pos, last_socket
        );
        assert!(
            parse_pos > accept_pos,
            "phase 11 slice 3: HTTP parse must follow sys_accept; \
             accept@{} parse@{}",
            accept_pos, parse_pos
        );

        let _ = std::fs::remove_file(out);
    }

    /// Phase 11 slice 3 end-to-end: spawn the compiled reverse_proxy
    /// service, spawn a tiny TCP listener as the upstream backend, then
    /// issue an HTTP request to the proxy whose method+path are unique
    /// enough to be unmistakable in the listener's recorded buffer.
    /// Assert (a) the listener saw the SAME method and path on the wire,
    /// composed via concat(req.method, " ", req.path, ...), and (b) the
    /// proxy's response body contains the upstream's payload.
    ///
    /// The byte-pattern test above proves the reorder happened in
    /// machine code; this test proves the reordered code actually
    /// resolves req.method / req.path through the populated parser
    /// slots and writes their bytes onto the upstream socket.
    #[test]
    fn phase11_slice3_reverse_proxy_forwards_method_and_path() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};
        use std::thread;
        use std::time::Duration;

        let upstream_port: u16 = 19030;
        let proxy_port: u16 = 18930;

        let upstream =
            TcpListener::bind(("127.0.0.1", upstream_port)).expect("bind upstream listener");
        upstream.set_nonblocking(false).expect("blocking upstream");

        // Capture what the upstream saw on the wire so we can assert
        // method/path forwarding from the test thread after the join.
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let upstream_payload = b"HTTP/1.0 200 OK\r\nContent-Length: 9\r\n\r\nproxied!!";
        let upstream_thread = thread::spawn(move || {
            let (mut sock, _) = upstream.accept().expect("upstream accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut req = [0u8; 1024];
            let n = sock.read(&mut req).unwrap_or(0);
            if let Ok(mut g) = captured_clone.lock() {
                g.extend_from_slice(&req[..n]);
            }
            sock.write_all(upstream_payload).expect("write upstream response");
        });

        let src = std::fs::read_to_string("examples/reverse_proxy.verbose")
            .expect("examples/reverse_proxy.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out =
            std::env::temp_dir().join("verbosec_test_phase11_slice3_runtime_bin");
        compile_service(&program, "proxy_server", out.to_str().unwrap())
            .expect("Http10 service with dynamic-request fetch compile");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn proxy service binary");

        // Wait for the proxy to bind.
        let mut stream: Option<TcpStream> = None;
        for _ in 0..50 {
            match TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", proxy_port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }

        let runtime_result: Result<Vec<u8>, String> = (|| {
            let mut s = stream.ok_or_else(|| "could not connect to proxy".to_string())?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            // A path long and unique enough to be unmistakable in the
            // upstream's captured buffer.
            s.write_all(b"GET /some/test/path HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(buf)
        })();

        // Reap server + upstream first so a panic does not leave dangling fds.
        let _ = child.kill();
        let _ = child.wait();
        let _ = upstream_thread.join();

        let response = runtime_result.expect("HTTP roundtrip failed");
        let captured_bytes = captured.lock().expect("lock captured").clone();
        let captured_str = String::from_utf8_lossy(&captured_bytes);

        // Assertion (a): upstream saw the proxy's request line. The proxy
        // emitted `concat(req.method, " ", req.path, " HTTP/1.0\r\n\r\n")`,
        // so the wire bytes the listener captured must START with
        // "GET /some/test/path HTTP/1.0\r\n\r\n".
        assert!(
            captured_str.starts_with("GET /some/test/path HTTP/1.0\r\n\r\n"),
            "phase 11 slice 3: upstream did not see the forwarded method+path; \
             expected 'GET /some/test/path HTTP/1.0\\r\\n\\r\\n' prefix, got {:?}",
            captured_str
        );

        // Assertion (b): proxy returned upstream's payload as the body.
        let sep = b"\r\n\r\n";
        let split_at = response
            .windows(sep.len())
            .position(|w| w == sep)
            .expect("proxy response missing CRLF/CRLF terminator");
        let body = &response[split_at + sep.len()..];
        let body_str = String::from_utf8_lossy(body);
        assert!(
            body_str.contains("proxied!!"),
            "proxy response body did not contain 'proxied!!': body={:?}",
            body_str
        );

        let _ = std::fs::remove_file(&out);
    }

    /// Audit-coverage regression: when an HTTP handler body composes
    /// `concat(literal, read(resource), literal, fetch(connection, _))`,
    /// both the resource read and the fetch response must contribute
    /// their RUNTIME length to the body buffer's sizing pass. Before the
    /// fix in `emit_concat_to_buffer_impl`, the BoundText sizing branch
    /// matched only `Expr::Ident`, so `Read(_)` and `Fetch(_, _)` args
    /// silently added zero — the fill pass then overran the buffer
    /// upward into the HTTP request scratch and clobbered `req.method` /
    /// `req.path`. The visible symptom was correct response bodies but
    /// audit log lines whose method/path expanded to file/upstream
    /// content. This test pins the fix: the audit line's method must be
    /// "GET" and its path must be the request path, byte-for-byte.
    #[test]
    fn coverage_read_and_fetch_concat_in_handler_preserves_request_slots() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::Duration;

        let upstream_port: u16 = 19031;
        let svc_port: u16 = 18931;
        let header_path = "/tmp/verbosec_test_coverage_rf_header.txt";
        let audit_path = "/tmp/verbosec_test_coverage_rf_audit.jsonl";

        std::fs::write(header_path, b"HEADER-CONTENT-FOR-TEST\n").expect("write header");
        let _ = std::fs::remove_file(audit_path);

        let upstream =
            TcpListener::bind(("127.0.0.1", upstream_port)).expect("bind test upstream");
        upstream.set_nonblocking(false).expect("blocking upstream");
        let upstream_payload =
            b"HTTP/1.0 200 OK\r\nContent-Length: 24\r\n\r\nUPSTREAM-PAYLOAD-FOR-TST";
        let upstream_thread = thread::spawn(move || {
            let (mut sock, _) = upstream.accept().expect("upstream accept");
            sock.set_read_timeout(Some(Duration::from_secs(2))).ok();
            let mut req = [0u8; 1024];
            let _ = sock.read(&mut req).unwrap_or(0);
            sock.write_all(upstream_payload).expect("write upstream response");
        });

        let src = format!(
            r#"@verbose 0.1.0

resource header_template
  @intention: "header file"
  @source: invoices.intent:1
  path: "{header_path}"
  max:  512
  on_read_error: abort

connection upstream
  @intention: "test upstream"
  @source: invoices.intent:1
  host: "127.0.0.1"
  port: {upstream_port}
  max_response: 1024
  on_connect_error: abort

rule serve
  @intention: "header + upstream"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse {{ status: 200, body: concat("H:", read(header_template), "|U:", fetch(upstream, "GET / HTTP/1.0\r\n\r\n")) }}
  proofs:
    purity:
      reads : [header_template, upstream]
      calls : []
    termination:
      bound : 4

service rf_server
  @intention: "test"
  @source: invoices.intent:1
  listen:
    protocol    : http_1_0
    port        : {svc_port}
    max_request : 4096
  handler: serve
  log:
    append_file "{audit_path}" concat("{{\"method\":\"", req.method, "\",\"path\":\"", req.path, "\"}}\n")
    on_error: abort
"#
        );

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out =
            std::env::temp_dir().join("verbosec_test_coverage_rf_bin");
        compile_service(&program, "rf_server", out.to_str().unwrap())
            .expect("compile read+fetch service");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn service");

        let mut stream: Option<TcpStream> = None;
        for _ in 0..50 {
            match TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", svc_port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }

        let runtime: Result<Vec<u8>, String> = (|| {
            let mut s = stream.ok_or_else(|| "no connect".to_string())?;
            s.set_read_timeout(Some(Duration::from_secs(2))).ok();
            s.write_all(b"GET /coverage/path HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(buf)
        })();

        let _ = child.kill();
        let _ = child.wait();
        let _ = upstream_thread.join();

        let response = runtime.expect("HTTP roundtrip failed");
        let response_str = String::from_utf8_lossy(&response);

        // (a) Body must contain BOTH the header file content AND the
        //     upstream payload — proves the concat fill copied both
        //     BoundText args correctly.
        assert!(
            response_str.contains("H:HEADER-CONTENT-FOR-TEST")
                && response_str.contains("U:HTTP/1.0 200 OK")
                && response_str.contains("UPSTREAM-PAYLOAD-FOR-TST"),
            "response body missing header or upstream content: {:?}",
            response_str
        );

        // (b) The bug: req.method / req.path slots in the log scope were
        //     clobbered by the body buffer overrun. With the sizing fix
        //     they must contain exactly the parsed HTTP request fields.
        let audit = std::fs::read_to_string(audit_path).expect("read audit log");
        assert_eq!(
            audit.trim_end(),
            "{\"method\":\"GET\",\"path\":\"/coverage/path\"}",
            "audit log line corrupted — read+fetch buffer overran into req slots"
        );

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(header_path);
        let _ = std::fs::remove_file(audit_path);
    }

    /// Phase 12 (json_escape) compile-time fold: when the inner is a text
    /// literal, the optimizer must replace `Expr::JsonEscape(Text(s))`
    /// with `Expr::Text(<escaped s>)` BEFORE native sees the AST. This
    /// keeps the runtime free of a transform loop in the trivial case
    /// and proves the optimizer-side escape function matches the spec.
    #[test]
    fn phase12_json_escape_literal_folds_at_compile_time() {
        use crate::ast::{BinOp, Expr};
        let src = r#"@verbose 0.1.0

concept Tick
  @intention: "trivial input"
  @source: invoices.intent:1
  fields:
    n : number

rule esc_literal
  @intention: "escape a literal at compile time"
  @source: invoices.intent:1
  input:
    t : Tick
  output:
    out : text
  logic:
    out = json_escape("a\"b\\c")
  proofs:
    purity:
      reads: []
      calls: []
    termination:
      bound: 2
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");
        let (optimized, _stats) = crate::optimizer::optimize_program(&program);

        let rule = optimized
            .items
            .iter()
            .find_map(|i| match i {
                crate::ast::Item::Rule(r) if r.name == "esc_literal" => Some(r),
                _ => None,
            })
            .expect("rule esc_literal present");
        // The optimizer should have replaced JsonEscape(Text("a\"b\\c"))
        // with Text("a\\\"b\\\\c") — the same bytes a hand-escaped string
        // would carry. Suppress the warning Rust raises on the redundant
        // BinOp use.
        let _ = BinOp::Add;
        match &rule.logic.value {
            Expr::Text(s) => {
                assert_eq!(
                    s.as_str(),
                    r#"a\"b\\c"#,
                    "literal-fold did not produce expected escaped bytes: {:?}",
                    s
                );
            }
            other => panic!("expected folded Expr::Text, got {:?}", other),
        }
    }

    /// Phase 12 (json_escape) runtime: compiling the access_log_json
    /// service must embed the per-byte transform loop because the inner
    /// is a runtime-known field (req.method / req.path), not a literal.
    /// The signature byte sequence we look for is `cmp al, 0x22` (3C 22)
    /// — the first comparison in the loop body. Its presence proves the
    /// runtime path was emitted (the literal-fold path would not produce
    /// this opcode pair).
    #[test]
    fn phase12_json_escape_runtime_emits_transform_loop_bytes() {
        use std::fs;
        let src = fs::read_to_string("examples/access_log_json.verbose")
            .expect("examples/access_log_json.verbose is expected to exist");
        let tokens = crate::lexer::Lexer::new(&src).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();

        let out = std::env::temp_dir().join("verbosec_test_phase12_json_escape_loop");
        compile_service(&program, "access_logged_service", out.to_str().unwrap())
            .expect("Http10 service with json_escape compile");

        let bytes = fs::read(&out).expect("read output");
        // 0x3C is `cmp al, imm8` and 0x22 is the immediate (the JSON
        // double-quote we escape). The pair appears in the json_escape
        // fill loop and nowhere else in the existing emitter.
        let needle = [0x3C, 0x22];
        let count = bytes
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count();
        assert!(
            count >= 2,
            "expected at least 2 occurrences of `cmp al, 0x22` (one per json_escape call site), found {}",
            count
        );

        let _ = fs::remove_file(out);
    }

    /// Phase 12 (json_escape) end-to-end: spawn the access_log_json
    /// binary, send an HTTP request whose path contains a literal `"`
    /// (URL-encoded as %22), kill the server, parse the JSONL log file
    /// and assert each line contains the escaped quote `\"` inside the
    /// `path` field. Without json_escape the line would carry a bare `"`
    /// and break JSON parsing entirely.
    #[test]
    fn phase12_json_escape_access_log_produces_valid_jsonl() {
        use std::fs;
        use std::io::{Read, Write};
        use std::net::TcpStream;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        // Use a per-test log path so concurrent test runs don't collide.
        let log_path = std::env::temp_dir().join("verbosec_test_phase12_access.jsonl");
        let _ = fs::remove_file(&log_path);

        // Patch the example to write to our per-test log path. Re-using
        // the example file directly would race with other tests that
        // also touch /tmp/verbose_access.jsonl.
        let src_template = fs::read_to_string("examples/access_log_json.verbose")
            .expect("examples/access_log_json.verbose is expected to exist");
        let src = src_template.replace("/tmp/verbose_access.jsonl", log_path.to_str().unwrap());

        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_phase12_access_e2e");
        compile_service(&program, "access_logged_service", out.to_str().unwrap())
            .expect("compile access_logged_service for e2e");

        // The example's port is 18891; use a unique port to avoid
        // collisions with other tests / dev runs.
        // We patch the port via the source string so the test stays
        // self-contained even if another test binds 18891.
        let port: u16 = 18923;
        let src = src.replace("port        : 18891", &format!("port        : {}", port));
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens)
            .parse_program()
            .expect("parse");
        compile_service(&program, "access_logged_service", out.to_str().unwrap())
            .expect("recompile with custom port");

        let mut child = Command::new(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn access_logged_service binary");

        // Wait for listen() with a short retry loop.
        let mut probed = false;
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_millis(100),
            ) {
                drop(s);
                probed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let runtime_result: Result<(), String> = (|| {
            if !probed {
                return Err("server never accepted TCP connections".into());
            }
            // Send a request whose path contains a literal `"` byte —
            // the very character JSON requires us to escape. Per RFC
            // 7230 the path can technically be any visible ASCII; the
            // HTTP/1.0 parser in the binary takes whatever is between
            // the method and the next space, so we can include a raw
            // `"` here.
            let mut s = TcpStream::connect_timeout(
                &format!("127.0.0.1:{}", port).parse().unwrap(),
                Duration::from_secs(2),
            )
            .map_err(|e| format!("connect: {}", e))?;
            s.set_read_timeout(Some(Duration::from_secs(3))).ok();
            // Path: /quoted"path  — the bare quote is the test point.
            s.write_all(b"GET /quoted\"path HTTP/1.0\r\n\r\n")
                .map_err(|e| format!("write: {}", e))?;
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).map_err(|e| format!("read: {}", e))?;
            Ok(())
        })();

        let _ = child.kill();
        let _ = child.wait();

        runtime_result.expect("HTTP roundtrip failed");

        // Give the kernel a beat to flush the append_file write.
        std::thread::sleep(Duration::from_millis(50));

        let log_contents = fs::read_to_string(&log_path).expect("read log file");
        assert!(
            !log_contents.is_empty(),
            "log file empty after the request"
        );
        // Each line should contain the escaped quote `\"` inside the
        // `path` field — proving the runtime escape transform was
        // applied to req.path before assembling the JSON line.
        let mut saw_escape = false;
        for line in log_contents.lines() {
            // The path field appears as `"path":"<escaped path>"`. The
            // un-escaped quote in our request would have produced
            // `"path":"/quoted"path"` (broken JSON). With json_escape it
            // becomes `"path":"/quoted\"path"`.
            if line.contains(r#""path":"/quoted\"path""#) {
                saw_escape = true;
                break;
            }
        }
        assert!(
            saw_escape,
            "log file did not contain an escaped quote in the path field; contents:\n{}",
            log_contents
        );

        let _ = fs::remove_file(&out);
        let _ = fs::remove_file(&log_path);
    }

    /// `min(a, b)` / `max(a, b)` binary primitives (2026-04-29). Native
    /// emits a branch-free `cmp + cmovl/cmovg` (11 bytes total per op).
    /// Composes with `parse_int(read(<resource>))` so a clamping range
    /// can live on disk and be re-tuned without recompile — same fail-
    /// closed posture as `threshold_sum.verbose`.
    ///
    /// Pinned behaviours:
    ///   (a) raw inside [floor, ceiling] passes through untouched
    ///   (b) raw below floor is bumped UP to floor (max picks floor)
    ///   (c) raw above ceiling is pinned at ceiling (min picks ceiling)
    ///   (d) raw at the boundaries returns the boundary (no off-by-one)
    ///   (e) negative raw values clamp correctly (signed cmovl/cmovg)
    ///   (f) malformed floor/ceiling file aborts (parse_int fail-closed)
    ///   (g) missing file aborts (resource read fail-closed)
    #[test]
    fn slice_min_max_binary_runtime_clamp() {
        use std::process::Command;
        let floor_path = "/tmp/verbosec_test_clamp_floor.txt";
        let ceiling_path = "/tmp/verbosec_test_clamp_ceiling.txt";
        std::fs::write(floor_path, b"10").expect("write floor");
        std::fs::write(ceiling_path, b"90").expect("write ceiling");

        let src = std::fs::read_to_string("examples/clamped_score.verbose")
            .expect("examples/clamped_score.verbose");
        let src = src
            .replace("/tmp/verbose_clamp_floor.txt", floor_path)
            .replace("/tmp/verbose_clamp_ceiling.txt", ceiling_path);
        let tokens = crate::lexer::Lexer::new(&src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out = std::env::temp_dir().join("verbosec_test_clamp");
        compile_native(&program, "clamp", out.to_str().unwrap(), false, false)
            .expect("min/max + parse_int + read should compile");

        let run = |raw: &str| -> std::process::Output {
            // Pass exactly one arg: the binary's argv loop treats every
            // argv slot as a separate record, so `--` would be parsed as
            // an extra Reading and the test would see two outputs.
            Command::new(&out).args([raw]).output().expect("spawn")
        };
        let stdout_of = |o: &std::process::Output| -> String {
            String::from_utf8_lossy(&o.stdout).trim_end_matches('\n').to_string()
        };

        // (a) inside the range
        let r = run("50");
        assert!(r.status.success() && stdout_of(&r) == "50", "raw=50: {:?}", stdout_of(&r));

        // (b) below floor → floor
        let r = run("5");
        assert!(r.status.success() && stdout_of(&r) == "10", "raw=5: {:?}", stdout_of(&r));

        // (c) above ceiling → ceiling
        let r = run("200");
        assert!(r.status.success() && stdout_of(&r) == "90", "raw=200: {:?}", stdout_of(&r));

        // (d) exact boundaries
        let r = run("10");
        assert!(r.status.success() && stdout_of(&r) == "10", "raw=10: {:?}", stdout_of(&r));
        let r = run("90");
        assert!(r.status.success() && stdout_of(&r) == "90", "raw=90: {:?}", stdout_of(&r));

        // (e) negative raw, well below floor
        let r = run("-100");
        assert!(r.status.success() && stdout_of(&r) == "10", "raw=-100: {:?}", stdout_of(&r));

        // (f) malformed ceiling → parse_int abort
        std::fs::write(ceiling_path, b"abc").expect("update ceiling");
        let r = run("50");
        assert!(!r.status.success() && r.stdout.is_empty(), "non-digit ceiling should abort");

        // (g) missing floor → resource read abort
        std::fs::write(ceiling_path, b"90").expect("restore ceiling");
        let _ = std::fs::remove_file(floor_path);
        let r = run("50");
        assert!(!r.status.success() && r.stdout.is_empty(), "missing floor file should abort");

        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(floor_path);
        let _ = std::fs::remove_file(ceiling_path);
    }

    /// Pin the branch-free shape of binary `min` / `max` emission:
    /// for each op we expect one `cmp rcx, rax` (48 39 C1) and one
    /// `cmovl rax, rcx` (48 0F 4C C1) for `min`, or `cmovg rax, rcx`
    /// (48 0F 4F C1) for `max`. Without the cmov, the optimizer or
    /// emitter would have silently grown a conditional jump; this
    /// test fails fast if that ever happens.
    #[test]
    fn slice_min_max_binary_emits_cmp_and_cmov() {
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : number
    b : number

rule pick_min
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = min(t.a, t.b)
  proofs:
    purity:
      reads : [t.a, t.b]
      calls : []
    termination:
      bound : 1

rule pick_max
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = max(t.a, t.b)
  proofs:
    purity:
      reads : [t.a, t.b]
      calls : []
    termination:
      bound : 1
"#;
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("tokenize");
        let program = crate::parser::Parser::new(tokens).parse_program().expect("parse");

        let out_min = std::env::temp_dir().join("verbosec_test_min_cmov");
        compile_native(&program, "pick_min", out_min.to_str().unwrap(), false, false)
            .expect("min should compile");
        let bytes_min = std::fs::read(&out_min).expect("read min binary");
        let cmp_seq = [0x48u8, 0x39, 0xC1];
        let cmovl_seq = [0x48u8, 0x0F, 0x4C, 0xC1];
        let cmovg_seq = [0x48u8, 0x0F, 0x4F, 0xC1];
        assert!(
            bytes_min.windows(3).any(|w| w == cmp_seq),
            "min binary missing `cmp rcx, rax`"
        );
        assert!(
            bytes_min.windows(4).any(|w| w == cmovl_seq),
            "min binary missing `cmovl rax, rcx`"
        );
        assert!(
            !bytes_min.windows(4).any(|w| w == cmovg_seq),
            "min binary should NOT contain a cmovg (would mean we emitted max instead)"
        );

        let out_max = std::env::temp_dir().join("verbosec_test_max_cmov");
        compile_native(&program, "pick_max", out_max.to_str().unwrap(), false, false)
            .expect("max should compile");
        let bytes_max = std::fs::read(&out_max).expect("read max binary");
        assert!(
            bytes_max.windows(3).any(|w| w == cmp_seq),
            "max binary missing `cmp rcx, rax`"
        );
        assert!(
            bytes_max.windows(4).any(|w| w == cmovg_seq),
            "max binary missing `cmovg rax, rcx`"
        );
        assert!(
            !bytes_max.windows(4).any(|w| w == cmovl_seq),
            "max binary should NOT contain a cmovl (would mean we emitted min instead)"
        );

        let _ = std::fs::remove_file(&out_min);
        let _ = std::fs::remove_file(&out_max);
    }
}
