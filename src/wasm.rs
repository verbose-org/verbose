/// WebAssembly backend — produces .wasm modules from Verbose rules.
///
/// WASM is a stack-based bytecode that runs in browsers (Chrome, Firefox,
/// Safari) and server-side (Node.js, Deno, Cloudflare Workers).
///
/// # ABI
///
/// Per concept-field type, params encode as:
///   - `number` → 1 × i64
///   - `bool`   → 1 × i32  (only seen for inputs; not exposed yet for fields)
///   - `text`   → 2 × i32  (ptr, len) into the module's exported linear memory
///
/// Per output type, results encode as:
///   - `number` → 1 × i64
///   - `bool`   → 1 × i32
///   - `text`   → 2 × i32  (ptr, len) — multi-value return
///
/// Choice rationale (slice W3a, 2026-05-03): the (ptr, len) pair
/// mirrors native's BoundText slot convention exactly, so an auditor
/// reads either backend with the same mental model. The bound travels
/// alongside the data — never derived from a sentinel — so a
/// misbehaving caller cannot cause a read past the intended end.
///
/// # Memory
///
/// When any text I/O is present the module declares & exports a
/// linear memory of 1 page (64 KiB). Static text literals are placed
/// in the data section starting at offset `LITERAL_BASE` (1024) so
/// the first 1 KiB stays reserved for future runtime state (a bump
/// allocator for `concat` outputs lands in W3b).
///
/// The host calls into the module by writing input text bytes into
/// the exported memory, then passing the matching (ptr, len) as the
/// two i32 params. Output text bytes — for now only literals — live
/// in the data section and are read back by the host via
/// `(new TextDecoder()).decode(memory.subarray(ptr, ptr+len))`.

use crate::ast::*;
use std::collections::HashMap;

/// First memory offset where text literals are placed. The 0..1024
/// range is reserved for future runtime state (bump allocator
/// metadata, scratch buffers) so adding it later doesn't shift
/// existing literal offsets and break already-shipped modules.
const LITERAL_BASE: u32 = 1024;

/// How a concept field is represented in the WASM function's params
/// — sole source of truth for `Expr::Field` emission and for the
/// type-section signature.
#[derive(Debug, Clone, Copy)]
enum FieldShape {
    /// `number` field, single i64 param at this local index.
    Number(u32),
    /// `text` field, two i32 params: (ptr_local, len_local).
    Text { ptr: u32, len: u32 },
}

/// How a `let` binding is represented in the function's locals.
/// Number bindings sit in the i64 locals group, text bindings in
/// the i32 locals group as a (ptr, len) pair — same shape as a
/// text input field.
#[derive(Debug, Clone, Copy)]
enum BindingShape {
    Number(u32),
    Text { ptr: u32, len: u32 },
}

/// The five i32 locals reserved when a rule uses `concat(...)`.
/// `bump_ptr` persists across multiple sibling concats in the same
/// rule call (it's the allocator state); the others are per-concat
/// temporaries and are safe to reuse because we refuse nested concat.
#[derive(Debug, Clone, Copy)]
struct ConcatLocals {
    bump_ptr: u32,
    cursor: u32,
    result_ptr: u32,
    arg_ptr: u32,
    arg_len: u32,
}

/// Per-emit context bundle. Keeps the emit_wasm_expr signature
/// short as the backend grows; everything the recursive emitter
/// needs to look up sits here.
struct WasmCtx<'a> {
    rule: &'a Rule,
    concept: &'a Concept,
    all_rules: &'a std::collections::HashMap<&'a str, &'a Rule>,
    field_shapes: &'a HashMap<&'a str, FieldShape>,
    binding_shapes: &'a HashMap<&'a str, BindingShape>,
    text_literals: &'a HashMap<String, u32>,
    /// Local index just past the last i64 local — used when min/max/abs
    /// computes scratch_a / scratch_b on the fly.
    scratch_base_i64: u32,
    /// `Some` iff the rule uses `concat(...)`. Carries the five i32
    /// scratch locals reserved for the bump allocator and concat
    /// temporaries.
    concat: Option<ConcatLocals>,
    /// Function index of the inlined itoa helper, if the module
    /// emits one. `None` means concat with numeric args is refused
    /// (the caller has already verified this won't happen).
    itoa_func_idx: Option<u32>,
}

#[derive(Debug)]
pub struct WasmError {
    pub message: String,
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wasm error: {}", self.message)
    }
}

pub fn compile_wasm(
    program: &Program,
    rule_name: &str,
    output_path: &str,
) -> Result<(), WasmError> {
    let rules: std::collections::HashMap<&str, &Rule> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some((r.name.as_str(), r)),
            _ => None,
        })
        .collect();
    let concepts: Vec<&Concept> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Concept(c) => Some(c),
            _ => None,
        })
        .collect();

    let rule = rules.get(rule_name).ok_or_else(|| WasmError {
        message: format!("no rule named '{}'", rule_name),
    })?;

    let concept = match &rule.input_ty {
        Type::Named(n) => concepts.iter().find(|c| c.name == *n).ok_or_else(|| WasmError {
            message: format!("unknown concept '{}'", n),
        })?,
        _ => return Err(WasmError { message: "rule input must be a named concept".into() }),
    };

    // --- Param schema ---------------------------------------------
    // Walk concept fields once; each field becomes one or two WASM
    // params depending on its type. The map is the sole source of
    // truth for `Expr::Field` emission below; the type section is a
    // by-product of the same walk.
    let mut field_shapes: HashMap<&str, FieldShape> = HashMap::new();
    let mut param_types: Vec<u8> = Vec::new();
    let mut param_idx: u32 = 0;
    for f in &concept.fields {
        match &f.ty {
            Type::Number => {
                field_shapes.insert(f.name.as_str(), FieldShape::Number(param_idx));
                param_types.push(0x7E); // i64
                param_idx += 1;
            }
            Type::Bool => {
                // Bool input fields haven't been needed by any rule
                // we've shipped — keep refusing until there's a real
                // shape requirement to lock down.
                return Err(WasmError {
                    message: format!("bool input field '{}' is not yet supported in the WASM backend", f.name),
                });
            }
            Type::Text => {
                field_shapes.insert(f.name.as_str(), FieldShape::Text { ptr: param_idx, len: param_idx + 1 });
                param_types.push(0x7F); // i32 ptr
                param_types.push(0x7F); // i32 len
                param_idx += 2;
            }
            other => {
                return Err(WasmError {
                    message: format!("unsupported field type {:?} for '{}' in the WASM backend", other, f.name),
                });
            }
        }
    }
    let total_param_slots = param_idx;

    // --- Result schema --------------------------------------------
    // `text` output uses multi-value (i32, i32). Bool stays i32,
    // Number stays i64 — both 1-result.
    let result_types: Vec<u8> = match &rule.output_ty {
        Type::Bool => vec![0x7F],
        Type::Number => vec![0x7E],
        Type::Text => vec![0x7F, 0x7F],
        other => {
            return Err(WasmError {
                message: format!("unsupported output type {:?} in the WASM backend", other),
            });
        }
    };
    let is_bool = rule.output_ty == Type::Bool;
    let is_text_out = rule.output_ty == Type::Text;

    // --- Text literal collection + offset assignment ---------------
    // Walk the rule body and let-binding RHSes for every Expr::Text
    // occurrence; assign each unique literal an offset starting at
    // LITERAL_BASE. Identical literals are deduplicated.
    let mut text_literals: HashMap<String, u32> = HashMap::new();
    let mut literal_cursor: u32 = LITERAL_BASE;
    {
        let mut collect = |s: &str| {
            if !text_literals.contains_key(s) {
                text_literals.insert(s.to_string(), literal_cursor);
                literal_cursor += s.len() as u32;
            }
        };
        for (_, rhs) in &rule.logic.bindings {
            walk_text_literals(rhs, &mut collect);
        }
        walk_text_literals(&rule.logic.value, &mut collect);
    }

    // Memory section is required iff there's any text I/O. A rule that
    // returns `text` from a literal still needs memory because the host
    // reads bytes from the data section through it; a pure-number rule
    // skips memory entirely (keeps modules tiny).
    let needs_memory = is_text_out
        || field_shapes.values().any(|s| matches!(s, FieldShape::Text { .. }))
        || !text_literals.is_empty();

    // --- Binding shape classification ------------------------------
    // Walk bindings in source order, classifying each RHS as text
    // (yields ptr+len pair) or number (single i64). Text bindings
    // get two i32 slots; number bindings get one i64 slot. Build
    // the binding_shapes map incrementally so a later binding's
    // RHS can reference an earlier text binding via Ident.
    //
    // ALSO: refuse a text binding whose RHS is a `Concat(...)`
    // containing a nested concat — the nested concat would need
    // its own scratch set. The workaround is to bind the inner
    // concat to its own let first (which is now legal in W3b).
    let mut binding_shapes: HashMap<&str, BindingShape> = HashMap::new();
    let mut binding_assignments: Vec<(usize, BindingShape)> = Vec::new();
    let mut next_binding_i64 = total_param_slots;       // first i64 binding slot
    let mut n_i64_bindings: u32 = 0;
    let mut n_text_bindings: u32 = 0;
    // First pass: only count slots so we know where the i32 group
    // starts. We can't classify Ident-typed RHSes accurately without
    // the binding_shapes map being partially populated, but for now
    // the only "yields text" Idents would reference an earlier text
    // binding. We rely on source order during the actual classify
    // pass below.
    for (_, expr) in &rule.logic.bindings {
        if expr_yields_text(expr, &field_shapes, &binding_shapes) {
            n_text_bindings += 1;
        } else {
            n_i64_bindings += 1;
        }
        // Speculatively register the shape so subsequent ident
        // lookups during the same pass behave consistently. The
        // local indices are filler — they get overwritten in the
        // actual emit pass below.
        if expr_yields_text(expr, &field_shapes, &binding_shapes) {
            // unused at this point; populated below
        }
    }

    let needs_scratch = expr_uses_scratch(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_scratch(e));
    let n_scratch_i64 = if needs_scratch { 2 } else { 0 };

    // i64 group total: number bindings + scratch_a + scratch_b
    let n_i64_locals = n_i64_bindings + n_scratch_i64;
    let scratch_base_i64 = total_param_slots + n_i64_bindings;
    let i32_base = total_param_slots + n_i64_locals;

    let needs_concat = expr_uses_concat(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_concat(e));

    // Refuse nested concat upfront with a clear pointer to the
    // workaround.
    if expr_has_nested_concat(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_has_nested_concat(e))
    {
        return Err(WasmError {
            message: "nested concat(...) is not supported in the WASM backend; bind the inner concat to a `let` first".into(),
        });
    }

    // i32 group total: text-binding ptr/len pairs + concat scratches
    let n_concat_locals: u32 = if needs_concat { 5 } else { 0 };
    let n_i32_locals = n_text_bindings * 2 + n_concat_locals;

    // Concat scratch indices (just past the text bindings in the i32 group)
    let concat_locals: Option<ConcatLocals> = if needs_concat {
        let base = i32_base + n_text_bindings * 2;
        Some(ConcatLocals {
            bump_ptr: base,
            cursor: base + 1,
            result_ptr: base + 2,
            arg_ptr: base + 3,
            arg_len: base + 4,
        })
    } else {
        None
    };

    // Now do the real binding-shape classification pass and assign
    // local indices. Counters track separate i64 and i32 cursors.
    let mut i64_cursor = total_param_slots;
    let mut i32_cursor = i32_base;
    for (i, (name, expr)) in rule.logic.bindings.iter().enumerate() {
        if expr_yields_text(expr, &field_shapes, &binding_shapes) {
            let shape = BindingShape::Text { ptr: i32_cursor, len: i32_cursor + 1 };
            binding_shapes.insert(name.as_str(), shape);
            binding_assignments.push((i, shape));
            i32_cursor += 2;
        } else {
            let shape = BindingShape::Number(i64_cursor);
            binding_shapes.insert(name.as_str(), shape);
            binding_assignments.push((i, shape));
            i64_cursor += 1;
        }
    }
    // Place scratch_a / scratch_b right after the number bindings in
    // the i64 group; just past them is `scratch_base_i64`.
    debug_assert_eq!(i64_cursor, scratch_base_i64);

    // --- Detect itoa need -----------------------------------------
    let needs_itoa = expr_concat_has_number_arg(&rule.logic.value, &field_shapes, &binding_shapes)
        || rule.logic.bindings.iter().any(|(_, e)| {
            expr_concat_has_number_arg(e, &field_shapes, &binding_shapes)
        });

    // --- ALLOC_BASE for the bump allocator ------------------------
    // Pick the first 16-byte-aligned address past all literals. If
    // the rule uses no literals this collapses to LITERAL_BASE
    // (1024). The 0..LITERAL_BASE range stays reserved: bytes 16..48
    // back the itoa scratch region, bytes 0..16 stay free for future
    // allocator metadata.
    let alloc_base: u32 = (literal_cursor + 15) & !15;

    let mut module = Vec::new();

    // === WASM header ===
    module.extend_from_slice(b"\0asm");     // magic
    module.extend_from_slice(&1u32.to_le_bytes()); // version 1

    // === Type section (function signature) ===
    // 1 type if no itoa, 2 types otherwise. Itoa's signature is
    // (i64) -> (i32, i32) — the same shape every time, so we can
    // hard-code its type bytes.
    let mut type_section = Vec::new();
    let n_types: u32 = if needs_itoa { 2 } else { 1 };
    emit_leb128(&mut type_section, n_types as u64);
    type_section.push(0x60);                         // type 0: rule
    emit_leb128(&mut type_section, param_types.len() as u64);
    type_section.extend_from_slice(&param_types);
    emit_leb128(&mut type_section, result_types.len() as u64);
    type_section.extend_from_slice(&result_types);
    if needs_itoa {
        // type 1: itoa (i64) -> (i32, i32)
        type_section.extend_from_slice(&[0x60, 0x01, 0x7E, 0x02, 0x7F, 0x7F]);
    }
    emit_section(&mut module, 1, &type_section);

    // === Function section ===
    let func_section: Vec<u8> = if needs_itoa {
        vec![2, 0, 1] // 2 funcs: func 0 = type 0 (rule), func 1 = type 1 (itoa)
    } else {
        vec![1, 0]
    };
    emit_section(&mut module, 3, &func_section);

    // === Memory section (only if text I/O present) ===
    if needs_memory {
        // limits=0 means no max; min=1 page (64 KiB).
        let memory_section = vec![0x01, 0x00, 0x01];
        emit_section(&mut module, 5, &memory_section);
    }

    // === Export section ===
    // Always export the rule function. Also export "memory" when
    // declared so the host can read text bytes. Itoa stays internal.
    let mut export_section = Vec::new();
    let n_exports: u32 = 1 + if needs_memory { 1 } else { 0 };
    emit_leb128(&mut export_section, n_exports as u64);
    emit_name(&mut export_section, rule_name);
    export_section.push(0x00); // export kind: function
    emit_leb128(&mut export_section, 0); // function index 0 (rule)
    if needs_memory {
        emit_name(&mut export_section, "memory");
        export_section.push(0x02); // export kind: memory
        emit_leb128(&mut export_section, 0); // memory index 0
    }
    emit_section(&mut module, 7, &export_section);

    // === Code section ===
    //
    // Locals layout:
    //   - params 0..total_param_slots                 (concept fields)
    //   - i64 group: number bindings, then scratch_a/b if used
    //   - i32 group: text-binding ptr/len pairs, then concat scratches
    //                (bump_ptr, cursor, result_ptr, arg_ptr, arg_len)
    //
    // Locals declarations are emitted as `vec(localdecl)` where each
    // localdecl is `(count, valtype)`. We use up to two groups (one
    // per valtype) and skip any group whose count is zero.
    let mut body = Vec::new();
    let mut groups: Vec<(u32, u8)> = Vec::new();
    if n_i64_locals > 0 { groups.push((n_i64_locals, 0x7E)); }
    if n_i32_locals > 0 { groups.push((n_i32_locals, 0x7F)); }
    emit_leb128(&mut body, groups.len() as u64);
    for (count, ty) in &groups {
        emit_leb128(&mut body, *count as u64);
        body.push(*ty);
    }

    // Bump allocator init: $bump_ptr = ALLOC_BASE. Emitted before
    // any binding so a binding RHS that uses concat starts from a
    // fresh allocator state. Reset on every call → returned (ptr,
    // len) from a previous call is invalidated by the next call.
    if let Some(cl) = concat_locals {
        body.push(0x41);                              // i32.const
        emit_sleb128(&mut body, alloc_base as i64);
        body.push(0x21);                              // local.set
        emit_leb128(&mut body, cl.bump_ptr as u64);
    }

    let ctx = WasmCtx {
        rule,
        concept,
        all_rules: &rules,
        field_shapes: &field_shapes,
        binding_shapes: &binding_shapes,
        text_literals: &text_literals,
        scratch_base_i64,
        concat: concat_locals,
        itoa_func_idx: if needs_itoa { Some(1) } else { None },
    };

    // Emit let binding computations. Text bindings leave (ptr, len)
    // on the stack — pop into the two i32 slots in reverse order
    // (len first, then ptr). Number bindings store one i64.
    for (i, (_, expr)) in rule.logic.bindings.iter().enumerate() {
        emit_wasm_expr(&mut body, expr, &ctx)?;
        match binding_assignments[i].1 {
            BindingShape::Number(idx) => {
                body.push(0x21);
                emit_leb128(&mut body, idx as u64);
            }
            BindingShape::Text { ptr, len } => {
                body.push(0x21); emit_leb128(&mut body, len as u64);   // pop len
                body.push(0x21); emit_leb128(&mut body, ptr as u64);   // pop ptr
            }
        }
    }

    // Emit main expression.
    emit_wasm_expr(&mut body, &rule.logic.value, &ctx)?;

    // If rule returns bool but expr produces i64, wrap to i32. Text
    // outputs already leave (i32, i32) on the stack from Expr::Text /
    // text Expr::Field / Concat — no widening needed.
    if is_bool {
        body.push(0xA7); // i32.wrap_i64
    }

    body.push(0x0B); // end

    // Itoa function body (func 1) — only emitted when needs_itoa.
    let itoa_body: Vec<u8> = if needs_itoa { build_itoa_body() } else { Vec::new() };

    let mut code_section = Vec::new();
    let n_bodies: u32 = if needs_itoa { 2 } else { 1 };
    emit_leb128(&mut code_section, n_bodies as u64);
    emit_leb128(&mut code_section, body.len() as u64);
    code_section.extend_from_slice(&body);
    if needs_itoa {
        emit_leb128(&mut code_section, itoa_body.len() as u64);
        code_section.extend_from_slice(&itoa_body);
    }
    emit_section(&mut module, 10, &code_section);

    // === Data section (only if literals were collected) ===
    // One active segment per literal at its assigned offset. We could
    // pack adjacent literals into one segment, but per-literal segments
    // keep the byte-shape obvious to an auditor reading the module
    // dump and cost only a few bytes each.
    if !text_literals.is_empty() {
        // Sort by offset so the section bytes are deterministic — a
        // HashMap iteration order would otherwise reshuffle modules
        // between builds.
        let mut sorted: Vec<(&String, &u32)> = text_literals.iter().collect();
        sorted.sort_by_key(|(_, &off)| off);

        let mut data_section = Vec::new();
        emit_leb128(&mut data_section, sorted.len() as u64);
        for (s, &off) in &sorted {
            data_section.push(0x00);                       // mode: active in memory 0
            data_section.push(0x41);                       // i32.const
            emit_sleb128(&mut data_section, off as i64);
            data_section.push(0x0B);                       // end
            emit_leb128(&mut data_section, s.len() as u64);
            data_section.extend_from_slice(s.as_bytes());
        }
        emit_section(&mut module, 11, &data_section);
    }

    std::fs::write(output_path, &module).map_err(|e| WasmError {
        message: format!("cannot write '{}': {}", output_path, e),
    })?;

    Ok(())
}

/// Build the WASM body for the itoa helper function.
///
/// Signature: `(i64) -> (i32, i32)`. Writes the ASCII decimal of
/// the input value into linear memory at `[16, 48)` (writing
/// digits BACKWARDS from offset 47), returns `(ptr, len)` pointing
/// at the first written byte.
///
/// Locals (after the i64 input param at index 0):
///   1: $val      (i64) — working copy, mutated as we divide by 10
///   2: $cursor   (i32) — write position, starts at 47
///   3: $isNeg    (i32) — flag, 1 if input was negative
///
/// Why unsigned div/rem (`i64.div_u` / `i64.rem_u`): for `i64::MIN`,
/// the negation `0 - val` overflows in signed semantics but
/// produces the correct positive bit pattern when interpreted as
/// u64 (2^63). Signed div/rem on that pattern would treat it as
/// negative again, breaking the loop. Unsigned div/rem treats it
/// at face value. Validated against Node for the full i64 range.
///
/// Bytes match the prototype that was hand-validated against Node
/// before this slice was implemented — see the W3b commit message.
fn build_itoa_body() -> Vec<u8> {
    vec![
        // Locals: 2 groups
        0x02,
        0x01, 0x7E,                             // 1 × i64 ($val)
        0x02, 0x7F,                             // 2 × i32 ($cursor, $isNeg)
        // $val = param 0
        0x20, 0x00,                             // local.get 0
        0x21, 0x01,                             // local.set 1 ($val)
        // $isNeg = 0
        0x41, 0x00,                             // i32.const 0
        0x21, 0x03,                             // local.set 3
        // If $val < 0: $isNeg = 1; $val = 0 - $val (wraps for i64::MIN)
        0x20, 0x01, 0x42, 0x00, 0x53,           // local.get 1; i64.const 0; i64.lt_s
        0x04, 0x40,                             // if (no result)
            0x41, 0x01, 0x21, 0x03,             //   i32.const 1; local.set 3
            0x42, 0x00, 0x20, 0x01, 0x7D,       //   i64.const 0; local.get 1; i64.sub
            0x21, 0x01,                         //   local.set 1
        0x0B,                                    // end if
        // Special case: $val == 0 → write '0' to mem[16]; return (16, 1)
        0x20, 0x01, 0x50,                       // local.get 1; i64.eqz
        0x04, 0x40,                             // if
            0x41, 0x10, 0x41, 0x30,             //   i32.const 16; i32.const 48 ('0')
            0x3A, 0x00, 0x00,                   //   i32.store8 align=0 offset=0
            0x41, 0x10, 0x41, 0x01,             //   push 16, 1
            0x0F,                                //   return
        0x0B,                                    // end if
        // $cursor = 47
        0x41, 0x2F, 0x21, 0x02,                 // i32.const 47; local.set 2
        // loop: while $val != 0
        0x03, 0x40,                             // loop (no result)
            // mem[$cursor] = ($val rem_u 10) + '0'
            0x20, 0x02,                         // local.get 2 (dest)
            0x20, 0x01,                         // local.get 1 ($val)
            0x42, 0x0A,                         // i64.const 10
            0x82,                               // i64.rem_u
            0xA7,                               // i32.wrap_i64
            0x41, 0x30, 0x6A,                   // i32.const 48; i32.add
            0x3A, 0x00, 0x00,                   // i32.store8
            // $cursor -= 1
            0x20, 0x02, 0x41, 0x01, 0x6B, 0x21, 0x02,
            // $val = $val div_u 10
            0x20, 0x01, 0x42, 0x0A, 0x80, 0x21, 0x01,
            // continue if $val != 0
            0x20, 0x01, 0x42, 0x00, 0x52, 0x0D, 0x00,
        0x0B,                                    // end loop
        // If $isNeg: write '-' at $cursor; cursor -= 1
        0x20, 0x03,                             // local.get 3
        0x04, 0x40,                             // if
            0x20, 0x02, 0x41, 0x2D, 0x3A, 0x00, 0x00,   // store '-'
            0x20, 0x02, 0x41, 0x01, 0x6B, 0x21, 0x02,   // cursor -= 1
        0x0B,                                    // end if
        // Return (cursor + 1, 47 - cursor)
        0x20, 0x02, 0x41, 0x01, 0x6A,           // cursor + 1
        0x41, 0x2F, 0x20, 0x02, 0x6B,           // 47 - cursor
        0x0B,                                    // end func
    ]
}

/// Recursively enumerate every text literal occurring in `expr`. The
/// callback is invoked once per occurrence — dedup is the caller's
/// job (so the offset map stays in one place).
fn walk_text_literals<F: FnMut(&str)>(expr: &Expr, f: &mut F) {
    match expr {
        Expr::Text(s) => f(s),
        Expr::Binary(_, l, r) => { walk_text_literals(l, f); walk_text_literals(r, f); }
        Expr::If(c, t, e) => { walk_text_literals(c, f); walk_text_literals(t, f); walk_text_literals(e, f); }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::Abs(inner) => walk_text_literals(inner, f),
        Expr::Min(a, b) | Expr::Max(a, b) => { walk_text_literals(a, f); walk_text_literals(b, f); }
        Expr::Call(_, args) => { for a in args { walk_text_literals(a, f); } }
        Expr::Concat(args) => { for a in args { walk_text_literals(a, f); } }
        _ => {}
    }
}

/// Type-shape classifier for an expression: would emitting this
/// expression leave a text `(ptr, len)` pair on the stack (true) or
/// a single i64 / i32 value (false)?
///
/// Source of truth for:
///   - choosing the binding slot shape (i64 vs i32 ptr+len)
///   - dispatching concat-arg emission (text path vs number path)
///
/// Walks the Expr tree against the field/binding shape maps that
/// the compiler has already populated. Conservative: anything we
/// don't recognise is treated as non-text — the actual emitter
/// will either emit a number or raise a clear error.
fn expr_yields_text(
    expr: &Expr,
    field_shapes: &HashMap<&str, FieldShape>,
    binding_shapes: &HashMap<&str, BindingShape>,
) -> bool {
    match expr {
        Expr::Text(_) => true,
        Expr::Concat(_) => true,
        Expr::Field(base, name) => {
            matches!(base.as_ref(), Expr::Ident(_))
                && matches!(field_shapes.get(name.as_str()), Some(FieldShape::Text { .. }))
        }
        Expr::Ident(name) => {
            matches!(binding_shapes.get(name.as_str()), Some(BindingShape::Text { .. }))
        }
        // Conservative defaults below: treat unknown shapes as non-text.
        // If/Else with text branches isn't allowed in W3b (the type
        // system would forbid mixed branches) — refuse upstream if it
        // ever appears.
        _ => false,
    }
}

/// True iff `expr` (or any sub-expression) is a `Concat(...)` —
/// drives the decision to reserve concat scratches and the bump
/// allocator's local.
fn expr_uses_concat(expr: &Expr) -> bool {
    match expr {
        Expr::Concat(_) => true,
        Expr::Binary(_, l, r) => expr_uses_concat(l) || expr_uses_concat(r),
        Expr::If(c, t, e) => expr_uses_concat(c) || expr_uses_concat(t) || expr_uses_concat(e),
        Expr::Not(inner) | Expr::Neg(inner) | Expr::Abs(inner) => expr_uses_concat(inner),
        Expr::Min(a, b) | Expr::Max(a, b) => expr_uses_concat(a) || expr_uses_concat(b),
        Expr::Call(_, args) => args.iter().any(expr_uses_concat),
        _ => false,
    }
}

/// True iff a `Concat(...)` somewhere in `expr` has at least one
/// number-typed argument — drives the decision to emit the itoa
/// helper function. Walks recursively so a concat nested inside an
/// `if` arm still counts.
fn expr_concat_has_number_arg(
    expr: &Expr,
    field_shapes: &HashMap<&str, FieldShape>,
    binding_shapes: &HashMap<&str, BindingShape>,
) -> bool {
    match expr {
        Expr::Concat(args) => args.iter().any(|a| !expr_yields_text(a, field_shapes, binding_shapes)),
        Expr::Binary(_, l, r) => {
            expr_concat_has_number_arg(l, field_shapes, binding_shapes)
                || expr_concat_has_number_arg(r, field_shapes, binding_shapes)
        }
        Expr::If(c, t, e) => {
            expr_concat_has_number_arg(c, field_shapes, binding_shapes)
                || expr_concat_has_number_arg(t, field_shapes, binding_shapes)
                || expr_concat_has_number_arg(e, field_shapes, binding_shapes)
        }
        Expr::Not(inner) | Expr::Neg(inner) | Expr::Abs(inner) => {
            expr_concat_has_number_arg(inner, field_shapes, binding_shapes)
        }
        Expr::Min(a, b) | Expr::Max(a, b) => {
            expr_concat_has_number_arg(a, field_shapes, binding_shapes)
                || expr_concat_has_number_arg(b, field_shapes, binding_shapes)
        }
        Expr::Call(_, args) => args.iter().any(|a| expr_concat_has_number_arg(a, field_shapes, binding_shapes)),
        _ => false,
    }
}

/// True iff `expr` contains a Concat directly inside another Concat's
/// args. We refuse this in W3b because each concat call needs its
/// own private (cursor, result_ptr, arg_ptr, arg_len) — sharing them
/// across nesting would clobber. Workaround: bind the inner concat
/// to a `let` and reference the binding (which is now allowed since
/// W3b lifts the text-let-RHS refusal).
fn expr_has_nested_concat(expr: &Expr) -> bool {
    fn inside_concat(e: &Expr) -> bool {
        match e {
            Expr::Concat(_) => true,
            Expr::Binary(_, l, r) => inside_concat(l) || inside_concat(r),
            Expr::If(c, t, ee) => inside_concat(c) || inside_concat(t) || inside_concat(ee),
            Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) => inside_concat(i),
            Expr::Min(a, b) | Expr::Max(a, b) => inside_concat(a) || inside_concat(b),
            Expr::Call(_, args) => args.iter().any(inside_concat),
            _ => false,
        }
    }
    match expr {
        Expr::Concat(args) => args.iter().any(inside_concat),
        Expr::Binary(_, l, r) => expr_has_nested_concat(l) || expr_has_nested_concat(r),
        Expr::If(c, t, e) => expr_has_nested_concat(c) || expr_has_nested_concat(t) || expr_has_nested_concat(e),
        Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) => expr_has_nested_concat(i),
        Expr::Min(a, b) | Expr::Max(a, b) => expr_has_nested_concat(a) || expr_has_nested_concat(b),
        Expr::Call(_, args) => args.iter().any(expr_has_nested_concat),
        _ => false,
    }
}

fn emit_wasm_expr(
    code: &mut Vec<u8>,
    expr: &Expr,
    ctx: &WasmCtx,
) -> Result<(), WasmError> {
    match expr {
        Expr::Number(n) => {
            code.push(0x42); // i64.const
            emit_sleb128(code, *n);
            Ok(())
        }
        Expr::Text(s) => {
            // Push (ptr, len) — the offset was assigned by the compiler
            // pre-pass. Empty literals get an arbitrary offset; len=0
            // makes them safe regardless.
            let offset = *ctx.text_literals.get(s).ok_or_else(|| WasmError {
                message: format!("internal: text literal '{}' not in offset map", s),
            })?;
            code.push(0x41);                            // i32.const ptr
            emit_sleb128(code, offset as i64);
            code.push(0x41);                            // i32.const len
            emit_sleb128(code, s.len() as i64);
            Ok(())
        }
        Expr::Field(base, field_name) => {
            if !matches!(base.as_ref(), Expr::Ident(n) if n == &ctx.rule.input_name) {
                return Err(WasmError { message: "nested field access not supported".into() });
            }
            match ctx.field_shapes.get(field_name.as_str()) {
                Some(FieldShape::Number(idx)) => {
                    code.push(0x20); // local.get
                    emit_leb128(code, *idx as u64);
                    Ok(())
                }
                Some(FieldShape::Text { ptr, len }) => {
                    code.push(0x20); emit_leb128(code, *ptr as u64);   // local.get ptr
                    code.push(0x20); emit_leb128(code, *len as u64);   // local.get len
                    Ok(())
                }
                None => Err(WasmError { message: format!("unknown field '{}'", field_name) }),
            }
        }
        Expr::Ident(name) => {
            match ctx.binding_shapes.get(name.as_str()) {
                Some(BindingShape::Number(idx)) => {
                    code.push(0x20);
                    emit_leb128(code, *idx as u64);
                    Ok(())
                }
                Some(BindingShape::Text { ptr, len }) => {
                    code.push(0x20); emit_leb128(code, *ptr as u64);
                    code.push(0x20); emit_leb128(code, *len as u64);
                    Ok(())
                }
                None => Err(WasmError { message: format!("unresolved ident '{}'", name) }),
            }
        }
        Expr::Binary(op, left, right) => {
            emit_wasm_expr(code, left, ctx)?;
            emit_wasm_expr(code, right, ctx)?;
            match op {
                BinOp::Add => code.push(0x7C),    // i64.add
                BinOp::Sub => code.push(0x7D),    // i64.sub
                BinOp::Mul => code.push(0x7E),    // i64.mul
                BinOp::Div => code.push(0x7F),    // i64.div_s
                BinOp::Mod => code.push(0x81),    // i64.rem_s
                // Comparisons return i32 in WASM — extend to i64 for consistency
                BinOp::Gt => { code.push(0x55); code.push(0xAD); }     // i64.gt_s → i64.extend_i32_u
                BinOp::Lt => { code.push(0x53); code.push(0xAD); }     // i64.lt_s → i64.extend_i32_u
                BinOp::GtEq => { code.push(0x57); code.push(0xAD); }   // i64.ge_s → i64.extend_i32_u
                BinOp::LtEq => { code.push(0x55); code.push(0xAD); }   // i64.le_s → i64.extend_i32_u
                BinOp::Eq => { code.push(0x51); code.push(0xAD); }     // i64.eq → i64.extend_i32_u
                BinOp::NotEq => { code.push(0x52); code.push(0xAD); }  // i64.ne → i64.extend_i32_u
                BinOp::And => code.push(0x83),     // i64.and
                BinOp::Or => code.push(0x84),      // i64.or
            }
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            emit_wasm_expr(code, cond, ctx)?;
            code.push(0xA7); // i32.wrap_i64 (condition must be i32)
            code.push(0x04); // if
            code.push(0x7E); // result type: i64
            emit_wasm_expr(code, then_e, ctx)?;
            code.push(0x05); // else
            emit_wasm_expr(code, else_e, ctx)?;
            code.push(0x0B); // end
            Ok(())
        }
        Expr::Not(inner) => {
            emit_wasm_expr(code, inner, ctx)?;
            code.push(0x50); // i64.eqz
            Ok(())
        }
        Expr::Neg(inner) => {
            code.push(0x42); code.push(0x00); // i64.const 0
            emit_wasm_expr(code, inner, ctx)?;
            code.push(0x7D); // i64.sub (0 - x = -x)
            Ok(())
        }
        Expr::Abs(inner) => {
            // Branchless abs via select. Evaluate `inner` exactly ONCE,
            // park it in scratch_a, then build:
            //   stack: [x, -x, (x>=0)i32]   →   select   →   [abs(x)]
            // i64.ge_s already returns i32 so no extend is needed for the
            // select condition (unlike the rule-output path that uniformly
            // widens comparisons via 0xAD for i64 chaining).
            let scratch_a = ctx.scratch_base_i64 as u64;
            emit_wasm_expr(code, inner, ctx)?;
            code.push(0x22); emit_leb128(code, scratch_a);   // local.tee scratch_a
            code.push(0x42); code.push(0x00);                // i64.const 0
            code.push(0x20); emit_leb128(code, scratch_a);   // local.get scratch_a
            code.push(0x7D);                                  // i64.sub  -> -x
            code.push(0x20); emit_leb128(code, scratch_a);   // local.get scratch_a
            code.push(0x42); code.push(0x00);                // i64.const 0
            code.push(0x59);                                  // i64.ge_s -> i32 cond
            code.push(0x1B);                                  // select
            Ok(())
        }
        Expr::Min(a, b) | Expr::Max(a, b) => {
            // Branchless binary scalar reduction via select.
            //
            // Evaluation discipline: a NESTED min/max/abs in `b` would
            // clobber scratch_a/scratch_b mid-computation, so we MUST
            // finish evaluating BOTH sub-expressions onto the stack
            // before parking either into a scratch local. The pop order
            // is then RIGHT-then-LEFT (LIFO):
            //   stack after evals : [..., a, b]
            //   set scratch_b     : pops b, $sb = b
            //   set scratch_a     : pops a, $sa = a
            // From here, only `local.get`s — no sub-evals — so $sa and
            // $sb remain stable through the four reads + select.
            //
            //   stack: [a, b, (a OP b)i32]  →  select  →  [a if cond else b]
            // For min: cond = (a < b)  (i64.lt_s = 0x53)
            // For max: cond = (a > b)  (i64.gt_s = 0x55)
            let is_max = matches!(expr, Expr::Max(_, _));
            let scratch_a = ctx.scratch_base_i64 as u64;
            let scratch_b = scratch_a + 1;

            emit_wasm_expr(code, a, ctx)?;     // [a]
            emit_wasm_expr(code, b, ctx)?;     // [a, b]
            code.push(0x21); emit_leb128(code, scratch_b);           // pop b: $sb = b
            code.push(0x21); emit_leb128(code, scratch_a);           // pop a: $sa = a
            code.push(0x20); emit_leb128(code, scratch_a);           // [a]      val_1
            code.push(0x20); emit_leb128(code, scratch_b);           // [a, b]   val_2
            code.push(0x20); emit_leb128(code, scratch_a);           // [a, b, a]
            code.push(0x20); emit_leb128(code, scratch_b);           // [a, b, a, b]
            code.push(if is_max { 0x55 } else { 0x53 });             // i64.gt_s | i64.lt_s
            code.push(0x1B);                                          // select
            Ok(())
        }
        Expr::Concat(args) => {
            // Single-pass concat: write each arg to $cursor in source
            // order, advancing $cursor by arg_len after each memcpy.
            // No prior sizing pass — the result length falls out of
            // (cursor - result_ptr) at the end.
            //
            // Allocator state ($bump_ptr) advances ONCE at the end so
            // sibling concats in the same rule call get fresh regions
            // (a `let x = concat(a,b); let y = concat(c,d)` produces
            // two adjacent allocations rather than overlapping ones).
            //
            // Nested concat is refused at the compile_wasm gate: each
            // concat needs its own private (cursor, result_ptr,
            // arg_ptr, arg_len) and we have only one set per rule.
            let cl = ctx.concat.ok_or_else(|| WasmError {
                message: "internal: Expr::Concat reached emit without concat scratches reserved".into(),
            })?;

            // Init: $result_ptr = $cursor = $bump_ptr
            code.push(0x20); emit_leb128(code, cl.bump_ptr as u64);   // [bump]
            code.push(0x22); emit_leb128(code, cl.result_ptr as u64); // [bump], $result_ptr = bump
            code.push(0x21); emit_leb128(code, cl.cursor as u64);     // [], $cursor = bump

            for arg in args {
                let yields_text = expr_yields_text(arg, ctx.field_shapes, ctx.binding_shapes);
                if yields_text {
                    // Eval arg → [..., arg_ptr, arg_len]
                    emit_wasm_expr(code, arg, ctx)?;
                } else {
                    // Eval number arg → [..., i64], then call itoa to
                    // get [..., i32 ptr, i32 len].
                    let itoa_idx = ctx.itoa_func_idx.ok_or_else(|| WasmError {
                        message: "internal: number arg in concat without itoa wired up".into(),
                    })?;
                    emit_wasm_expr(code, arg, ctx)?;
                    code.push(0x10); emit_leb128(code, itoa_idx as u64);   // call itoa
                }
                // Stack now: [..., arg_ptr, arg_len].
                // Park into scratches, then memcpy from arg_ptr to
                // $cursor for arg_len bytes.
                code.push(0x21); emit_leb128(code, cl.arg_len as u64);   // pop len
                code.push(0x21); emit_leb128(code, cl.arg_ptr as u64);   // pop ptr
                // memory.copy(dest=$cursor, src=$arg_ptr, n=$arg_len)
                code.push(0x20); emit_leb128(code, cl.cursor as u64);
                code.push(0x20); emit_leb128(code, cl.arg_ptr as u64);
                code.push(0x20); emit_leb128(code, cl.arg_len as u64);
                code.extend_from_slice(&[0xFC, 0x0A, 0x00, 0x00]);       // memory.copy 0 0
                // $cursor += $arg_len
                code.push(0x20); emit_leb128(code, cl.cursor as u64);
                code.push(0x20); emit_leb128(code, cl.arg_len as u64);
                code.push(0x6A);                                          // i32.add
                code.push(0x21); emit_leb128(code, cl.cursor as u64);
            }

            // Update allocator state: $bump_ptr = $cursor (next concat
            // in this rule call starts past the bytes we just wrote).
            code.push(0x20); emit_leb128(code, cl.cursor as u64);
            code.push(0x21); emit_leb128(code, cl.bump_ptr as u64);

            // Push result: ($result_ptr, $cursor - $result_ptr)
            code.push(0x20); emit_leb128(code, cl.result_ptr as u64);  // ptr
            code.push(0x20); emit_leb128(code, cl.cursor as u64);
            code.push(0x20); emit_leb128(code, cl.result_ptr as u64);
            code.push(0x6B);                                            // i32.sub → len
            Ok(())
        }
        Expr::Call(name, args) => {
            if args.len() != 1 {
                return Err(WasmError { message: "call requires 1 argument".into() });
            }
            let called = ctx.all_rules.get(name.as_str()).ok_or_else(|| WasmError {
                message: format!("unknown rule '{}'", name),
            })?;
            // Inline the callee's logic against the caller's field
            // layout. The callee MUST share the caller's input concept
            // and have no bindings of its own — otherwise the caller's
            // binding_shapes / text_literals are the wrong tables to
            // resolve through. Both restrictions match what's been
            // shipped historically; lifting them is its own slice.
            if !called.logic.bindings.is_empty() {
                return Err(WasmError {
                    message: format!("call into rule '{}' with let bindings is not yet supported", name),
                });
            }
            // Swap rule for the recursion so the callee's `input_name`
            // resolves correctly (caller and callee may name the same
            // concept binding differently).
            let callee_ctx = WasmCtx {
                rule: called,
                concept: ctx.concept,
                all_rules: ctx.all_rules,
                field_shapes: ctx.field_shapes,
                binding_shapes: ctx.binding_shapes,
                text_literals: ctx.text_literals,
                scratch_base_i64: ctx.scratch_base_i64,
                concat: ctx.concat,
                itoa_func_idx: ctx.itoa_func_idx,
            };
            emit_wasm_expr(code, &called.logic.value, &callee_ctx)
        }
        _ => Err(WasmError {
            message: format!("unsupported expression in WASM backend"),
        }),
    }
}

/// True iff `expr` (or any sub-expression) needs the WASM scratch locals.
/// Today that means `Abs`, `Min`, or `Max` — primitives that must reuse a
/// sub-expression value but cannot DUP the stack.
///
/// Walked once before the locals declaration so the function header
/// reserves scratches up-front; missed cases would surface as
/// "unknown local" validation errors at module load.
fn expr_uses_scratch(expr: &Expr) -> bool {
    match expr {
        Expr::Abs(_) | Expr::Min(_, _) | Expr::Max(_, _) => true,
        // Recurse into compound shapes the WASM backend currently supports.
        Expr::Binary(_, l, r) => expr_uses_scratch(l) || expr_uses_scratch(r),
        Expr::If(c, t, e) => expr_uses_scratch(c) || expr_uses_scratch(t) || expr_uses_scratch(e),
        Expr::Not(inner) | Expr::Neg(inner) => expr_uses_scratch(inner),
        Expr::Call(_, args) => args.iter().any(expr_uses_scratch),
        // Leaves and shapes the backend doesn't yet emit.
        _ => false,
    }
}

fn emit_section(module: &mut Vec<u8>, id: u8, content: &[u8]) {
    module.push(id);
    emit_leb128(module, content.len() as u64);
    module.extend_from_slice(content);
}

fn emit_name(buf: &mut Vec<u8>, name: &str) {
    emit_leb128(buf, name.len() as u64);
    buf.extend_from_slice(name.as_bytes());
}

fn emit_leb128(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn emit_sleb128(buf: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7F) as u8;
        value >>= 7;
        let more = !(value == 0 && byte & 0x40 == 0) && !(value == -1 && byte & 0x40 != 0);
        if more {
            buf.push(byte | 0x80);
        } else {
            buf.push(byte);
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leb128_encoding() {
        let mut buf = Vec::new();
        emit_leb128(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        emit_leb128(&mut buf, 127);
        assert_eq!(buf, vec![0x7F]);

        buf.clear();
        emit_leb128(&mut buf, 128);
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn sleb128_encoding() {
        let mut buf = Vec::new();
        emit_sleb128(&mut buf, 0);
        assert_eq!(buf, vec![0x00]);

        buf.clear();
        emit_sleb128(&mut buf, -1);
        assert_eq!(buf, vec![0x7F]);

        buf.clear();
        emit_sleb128(&mut buf, 10000);
        // 10000 = 0x2710 → LEB128: 0x90 0xCE 0x00
        assert_eq!(buf, vec![0x90, 0xCE, 0x00]);
    }

    #[test]
    fn wasm_module_has_valid_header() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule test_rule
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : bool
  logic:
    r = t.x > 10
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
"#;
        let tokens = Lexer::new(src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let path = "/tmp/test_wasm_header.wasm";
        compile_wasm(&program, "test_rule", path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).ok();
        // WASM magic: \0asm
        assert_eq!(&bytes[0..4], b"\0asm");
        // Version 1
        assert_eq!(&bytes[4..8], &[1, 0, 0, 0]);
        // Module should be small (< 200 bytes)
        assert!(bytes.len() < 200, "module too large: {} bytes", bytes.len());
    }

    #[test]
    fn wasm_arithmetic_module() {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule add_them
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    r = t.a + t.b * 2
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 2
"#;
        let tokens = Lexer::new(src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let path = "/tmp/test_wasm_arith.wasm";
        compile_wasm(&program, "add_them", path).unwrap();
        let bytes = std::fs::read(path).unwrap();
        std::fs::remove_file(path).ok();
        assert_eq!(&bytes[0..4], b"\0asm");
        // Should export "add_them"
        assert!(bytes.windows(8).any(|w| w == b"add_them"));
    }

    /// Compile `src` (a Verbose source string) for `rule_name` and return
    /// the resulting WASM bytes. Centralised here so the W1 tests stay
    /// focused on bytecode shape.
    fn compile_to_bytes(src: &str, rule_name: &str, file_stem: &str) -> Vec<u8> {
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        let path = format!("/tmp/wasm_w1_{}.wasm", file_stem);
        compile_wasm(&program, rule_name, &path).expect("compile_wasm");
        let bytes = std::fs::read(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        bytes
    }

    /// W1: `abs(<number>)` is a single-eval branchless emission. The
    /// distinctive opcodes are `i64.ge_s` (0x59) immediately followed by
    /// `select` (0x1B) — together they pick `x` when non-negative and
    /// `-x` otherwise. Pinning that pair guards against silent regression
    /// to a re-eval-twice or branching shape.
    #[test]
    fn wasm_w1_abs_emits_select_with_ge_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule pick_abs
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = abs(t.x)
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_abs", "abs");
        assert_eq!(&bytes[0..4], b"\0asm");
        // i64.ge_s (0x59) immediately before select (0x1B) is the abs signature.
        assert!(
            bytes.windows(2).any(|w| w == [0x59, 0x1B]),
            "abs should emit `i64.ge_s; select`"
        );
        // Negative regressions: a min/max emission would have a different cond.
        assert!(!bytes.windows(2).any(|w| w == [0x53, 0x1B]), "abs must not emit min cond");
        assert!(!bytes.windows(2).any(|w| w == [0x55, 0x1B]), "abs must not emit max cond");
    }

    /// W1: binary `min(a, b)` emits `i64.lt_s; select`. Asymmetry pinned
    /// here — if min and max ever silently swapped we'd notice.
    #[test]
    fn wasm_w1_min_emits_select_with_lt_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule pick_min
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = min(t.a, t.b)
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_min", "min");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(2).any(|w| w == [0x53, 0x1B]),
            "min should emit `i64.lt_s; select`"
        );
        assert!(!bytes.windows(2).any(|w| w == [0x55, 0x1B]), "min must not emit max cond");
    }

    /// W1: binary `max(a, b)` emits `i64.gt_s; select`.
    #[test]
    fn wasm_w1_max_emits_select_with_gt_s_cond() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule pick_max
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = max(t.a, t.b)
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "pick_max", "max");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(2).any(|w| w == [0x55, 0x1B]),
            "max should emit `i64.gt_s; select`"
        );
        assert!(!bytes.windows(2).any(|w| w == [0x53, 0x1B]), "max must not emit min cond");
    }

    /// W1: a rule with only let-bindings (no scratch primitives) keeps
    /// the previous locals-section bytes byte-for-byte. Two bindings
    /// would have triggered the latent encoding bug fixed in this slice
    /// (the prior code emitted "n groups, group 1 has n items" then ran
    /// out of bytes), so this test pins the corrected shape.
    #[test]
    fn wasm_w1_two_let_bindings_encode_locals_correctly() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule double_then_inc
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    let doubled = t.x * 2
    let plus_one = doubled + 1
    n = plus_one
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 3
"#;
        let bytes = compile_to_bytes(src, "double_then_inc", "two_lets");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Look for the locals declaration "01 02 7E" = 1 group of 2 i64.
        // It must appear inside the code section; a bare scan of the
        // module bytes is sufficient because no other section embeds
        // that exact triple by coincidence in this minimal program.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x02, 0x7E]),
            "expected single-group locals decl `01 02 7E` (1 group, 2 i64) in module bytes:\n{:02X?}",
            bytes
        );
    }

    /// W1: a rule that mixes a let-binding AND a scratch-needing
    /// primitive should reserve `n_bindings + 2` locals. With one let
    /// and `min(...)`, that's `01 03 7E` (1 group of 3 i64). This pins
    /// the layout invariant that scratches sit AFTER bindings in the
    /// local-index space.
    #[test]
    fn wasm_w1_lets_plus_scratch_reserve_total_locals() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule clamp_low
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    let floor = 10
    n = max(floor, min(t.a, t.b))
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 3
"#;
        let bytes = compile_to_bytes(src, "clamp_low", "lets_scratch");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x03, 0x7E]),
            "expected locals decl `01 03 7E` (1 group, 3 i64 = 1 let + 2 scratch)"
        );
        // Both min and max signature pairs must appear (nested clamp).
        assert!(bytes.windows(2).any(|w| w == [0x53, 0x1B]), "missing min select");
        assert!(bytes.windows(2).any(|w| w == [0x55, 0x1B]), "missing max select");
    }

    /// W1: a rule with NO bindings and NO scratch primitives must still
    /// emit the zero-locals byte (`00`), not skip the field. The CLI
    /// has been relying on this implicit invariant — pinned for clarity.
    #[test]
    fn wasm_w1_no_locals_emits_zero_byte() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule trivial
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    n : number
  logic:
    n = t.x + 1
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "trivial", "no_locals");
        assert_eq!(&bytes[0..4], b"\0asm");
        // The locals byte sits at the start of the function body inside
        // the code section. We don't decode the section here; instead we
        // pin that the abs/min/max scratch markers are absent (so the
        // detector wouldn't have inflated the count) and that none of
        // the multi-group locals patterns we'd expect for >0 locals
        // (`01 0N 7E` for small N) sneak in.
        for n in 1u8..=3 {
            assert!(
                !bytes.windows(3).any(|w| w == [0x01, n, 0x7E]),
                "trivial rule should not allocate {} locals",
                n
            );
        }
    }

    /// W1 end-to-end: run the compiled WASM in Node and check actual
    /// numeric outputs across nested scratch-using primitives. The
    /// bytecode-shape tests above pin the *opcodes*, but a real engine
    /// is the only thing that catches semantic timing bugs (e.g. a
    /// nested min/max overwriting an outer's scratch local
    /// mid-computation — exactly the bug fixed when this slice landed).
    ///
    /// Skipped silently if `node` is absent so contributors without it
    /// can still run `cargo test`. CI environments are expected to
    /// have node available.
    #[test]
    fn wasm_w1_runtime_nested_clamp_and_abs() {
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM runtime test");
            return;
        }

        // Two rules over the same Pair concept, exercising:
        //   - max(literal, min(field, field))   — outer max + nested min
        //   - abs(field)                         — single-eval branchless
        // Both share scratch_a/scratch_b, so any cross-contamination
        // between outer and nested levels surfaces here.
        let src = r#"@verbose 0.1.0
concept Pair
  @intention: "two numbers"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule clamp_to_zero
  @intention: "max(0, min(a, b))"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = max(0, min(p.a, p.b))
  proofs:
    purity:
      reads : [p.a, p.b]
      calls : []
    termination:
      bound : 2
rule abs_of_a
  @intention: "abs(a)"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = abs(p.a)
  proofs:
    purity:
      reads : [p.a]
      calls : []
    termination:
      bound : 1
"#;
        let clamp_path = "/tmp/wasm_w1_runtime_clamp.wasm";
        let abs_path = "/tmp/wasm_w1_runtime_abs.wasm";

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "clamp_to_zero", clamp_path).expect("compile clamp");
        compile_wasm(&program, "abs_of_a", abs_path).expect("compile abs");

        // One Node run, one assertion: a JS harness loads both modules
        // and prints OK or FAIL lines for each case. We then assert no
        // FAIL appears in the output.
        let script = format!(
            r#"
const fs = require("fs");
async function check(path, fn, args, expected) {{
  const buf = fs.readFileSync(path);
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL: invalid module " + path); return; }}
  const m = await WebAssembly.instantiate(buf);
  const got = m.instance.exports[fn](...args);
  const gotStr = typeof got === "bigint" ? got.toString() : String(got);
  if (gotStr === String(expected)) {{
    console.log("OK " + fn + "(" + args.join(",") + ") = " + gotStr);
  }} else {{
    console.log("FAIL " + fn + "(" + args.join(",") + ") = " + gotStr + ", expected " + expected);
  }}
}}
(async () => {{
  // clamp_to_zero(a, b) = max(0, min(a, b))
  await check("{clamp_path}", "clamp_to_zero", [5n, 7n], 5);          // both positive, in-range
  await check("{clamp_path}", "clamp_to_zero", [-3n, 7n], 0);          // negative inner, clamped up
  await check("{clamp_path}", "clamp_to_zero", [-3n, -10n], 0);        // both negative
  await check("{clamp_path}", "clamp_to_zero", [100n, 50n], 50);       // pins the inner min — this case caught the bug
  await check("{clamp_path}", "clamp_to_zero", [0n, 100n], 0);
  // abs_of_a(a, _)
  await check("{abs_path}", "abs_of_a", [42n, 0n], 42);
  await check("{abs_path}", "abs_of_a", [-42n, 0n], 42);
  await check("{abs_path}", "abs_of_a", [0n, 0n], 0);
  await check("{abs_path}", "abs_of_a", [-9223372036854775807n, 0n], 9223372036854775807n);
}})();
"#
        );

        let out = Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);

        let _ = std::fs::remove_file(clamp_path);
        let _ = std::fs::remove_file(abs_path);

        assert!(
            !stdout.contains("FAIL"),
            "WASM runtime check failed; node stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        // Sanity: at least one OK line so we didn't silently no-op.
        assert!(stdout.contains("OK"), "no OK lines from node; stdout:\n{}", stdout);
    }

    /// W3a: a rule with `output: text` returning a literal compiles to
    /// a multi-value `(i32, i32)` function and bundles a memory + data
    /// section. Pinned shape: type entry has `0x02 0x7F 0x7F` for the
    /// results, and the literal bytes appear in the data section.
    #[test]
    fn wasm_w3a_text_literal_output_emits_memory_and_data() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule say_hi
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    out : text
  logic:
    out = "hello"
  proofs:
    purity:
      reads: []
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "say_hi", "text_lit_out");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Multi-value result triple: 0x02 0x7F 0x7F (2 results, both i32)
        // appears in the type section.
        assert!(
            bytes.windows(3).any(|w| w == [0x02, 0x7F, 0x7F]),
            "type section should declare 2 i32 results"
        );
        // Memory section (id 5) declares 1 page.
        // Pattern: 0x05 0x03 0x01 0x00 0x01 — section id, length 3,
        // 1 memory, limits=0 (no max), min=1.
        assert!(
            bytes.windows(5).any(|w| w == [0x05, 0x03, 0x01, 0x00, 0x01]),
            "memory section must declare 1 page"
        );
        // The literal "hello" bytes appear in the data section.
        assert!(
            bytes.windows(5).any(|w| w == b"hello"),
            "data section must embed the literal bytes"
        );
        // Memory is also exported (so the host can read it).
        assert!(bytes.windows(6).any(|w| w == b"memory"), "memory should be exported");
    }

    /// W3a: a rule that echoes a text input field declares two i32
    /// params (ptr, len) for that field and returns the same pair.
    /// Pinned: param triple `0x02 0x7F 0x7F` (2 params i32 i32) and
    /// matching result triple — function type effectively
    /// `(i32, i32) -> (i32, i32)`.
    #[test]
    fn wasm_w3a_text_input_field_passes_two_i32_params() {
        let src = r#"@verbose 0.1.0
concept Greet
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
rule echo_name
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : Greet
  output:
    out : text
  logic:
    out = g.name
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "echo_name", "text_echo");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Two consecutive `0x02 0x7F 0x7F` triples in the type section
        // = (i32, i32) params + (i32, i32) results. We don't slice the
        // section here; presence of TWO non-overlapping windows is
        // enough to pin the shape.
        let tag = [0x02u8, 0x7F, 0x7F];
        let occurrences: Vec<usize> = bytes
            .windows(3)
            .enumerate()
            .filter_map(|(i, w)| if w == tag { Some(i) } else { None })
            .collect();
        // First triple is params, second is results — they should be
        // at distinct, non-overlapping positions.
        assert!(
            occurrences.len() >= 2 && occurrences[1] >= occurrences[0] + 3,
            "expected two `02 7F 7F` triples (params + results), got positions {:?}",
            occurrences
        );
        // Memory is exported (host writes input bytes here before calling).
        assert!(bytes.windows(6).any(|w| w == b"memory"));
    }

    /// W3a: text input fields shift binding/scratch local indices by
    /// the additional i32 slot. Concept `[name: text, age: number]`
    /// → params (i32 ptr, i32 len, i64 age), and a let binding `let x
    /// = age + 1` lives at local index 3 (not 2). Pinned indirectly
    /// by checking the locals declaration plus an opcode that reads
    /// the age param at index 2.
    #[test]
    fn wasm_w3a_text_field_shifts_subsequent_param_indices() {
        let src = r#"@verbose 0.1.0
concept P
  @intention: "p"
  @source: invoices.intent:1
  fields:
    name : text
    age : number
rule age_plus_one
  @intention: "p"
  @source: invoices.intent:1
  input:
    p : P
  output:
    n : number
  logic:
    let bumped = p.age + 1
    n = bumped
  proofs:
    purity:
      reads: [p.age]
      calls: []
    termination:
      bound: 2
"#;
        let bytes = compile_to_bytes(src, "age_plus_one", "shift_idx");
        assert_eq!(&bytes[0..4], b"\0asm");
        // `local.get 2` (0x20 0x02) reads the age param — name took
        // slots 0 and 1. If the emitter were still using
        // concept.fields.len() instead of total_param_slots, this
        // would have read slot 1 (= len half of name) and produced
        // a type-mismatch validation failure.
        assert!(
            bytes.windows(2).any(|w| w == [0x20, 0x02]),
            "expected `local.get 2` to read the age param after the text field"
        );
        // Param signature is (i32, i32, i64) = 0x03 0x7F 0x7F 0x7E.
        assert!(
            bytes.windows(4).any(|w| w == [0x03, 0x7F, 0x7F, 0x7E]),
            "expected param triple `03 7F 7F 7E` (i32, i32, i64)"
        );
    }

    /// W3b: text-typed let bindings now compile (W3a refused them).
    /// `let alias = g.name` parks (ptr, len) into two i32 slots and
    /// `Ident("alias")` resolves to two `local.get`s. Pinned by
    /// presence of an i32 group AFTER the text-input-field params.
    #[test]
    fn wasm_w3b_text_let_binding_compiles() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
rule passthrough
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    let alias = g.name
    out = alias
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "passthrough", "text_let");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Locals declarations: a single i32 group of 2 (the text
        // binding's ptr+len) → bytes `01 02 7F`. There are no number
        // bindings and no scratches, so the i64 group is absent.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x02, 0x7F]),
            "expected single i32 locals group of 2 (text binding ptr+len)"
        );
    }

    /// W3a end-to-end: load the compiled module in node, exercise
    /// both literal-output and echo, and check the bytes round-trip
    /// through TextDecoder. This is the slice's true regression net
    /// — bytecode-shape tests pin the SECTIONS, only a real engine
    /// catches an offset-vs-length mistake or a multi-value ABI
    /// mismatch.
    ///
    /// Skipped silently if `node` is absent.
    #[test]
    fn wasm_w3a_runtime_text_literal_and_echo() {
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM W3a runtime test");
            return;
        }

        let src = r#"@verbose 0.1.0
concept G
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
rule say_hi
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = "hello, verbose"
  proofs:
    purity:
      reads: []
      calls: []
    termination:
      bound: 1
rule echo_name
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = g.name
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
"#;
        let lit_path = "/tmp/wasm_w3a_runtime_lit.wasm";
        let echo_path = "/tmp/wasm_w3a_runtime_echo.wasm";

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "say_hi", lit_path).expect("compile lit");
        compile_wasm(&program, "echo_name", echo_path).expect("compile echo");

        let script = format!(
            r#"
const fs = require("fs");
const decoder = new TextDecoder();
async function readResult(path, fn, args) {{
  const buf = fs.readFileSync(path);
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL invalid module"); return; }}
  const m = await WebAssembly.instantiate(buf);
  const [ptr, len] = fn(m, args);
  const mem = new Uint8Array(m.instance.exports.memory.buffer);
  return decoder.decode(mem.subarray(ptr, ptr+len));
}}

(async () => {{
  // Literal output: rule takes one input record (one i32 ptr +
  // one i32 len) and returns (i32, i32). We pass dummy 0,0 for the
  // unused name field — the rule ignores it.
  let s = await readResult("{lit_path}",
    (m, args) => m.instance.exports.say_hi(0, 0),
    null);
  console.log(s === "hello, verbose" ? "OK lit" : "FAIL lit got " + JSON.stringify(s));

  // Echo: write "Alice" into memory at offset 4096, pass (4096, 5),
  // expect the same bytes back.
  let buf = fs.readFileSync("{echo_path}");
  let m = await WebAssembly.instantiate(buf);
  let mem = new Uint8Array(m.instance.exports.memory.buffer);
  const inputBytes = new TextEncoder().encode("Alice");
  mem.set(inputBytes, 4096);
  const [ptr, len] = m.instance.exports.echo_name(4096, inputBytes.length);
  const got = decoder.decode(mem.subarray(ptr, ptr+len));
  console.log(got === "Alice" ? "OK echo" : "FAIL echo got " + JSON.stringify(got));

  // Echo with longer input — make sure ptr+len isn't tied to a
  // particular length.
  const long = "the quick brown fox jumps over the lazy dog";
  const longBytes = new TextEncoder().encode(long);
  mem.set(longBytes, 8192);
  const [p2, l2] = m.instance.exports.echo_name(8192, longBytes.length);
  const got2 = decoder.decode(mem.subarray(p2, p2+l2));
  console.log(got2 === long ? "OK echo-long" : "FAIL echo-long got " + JSON.stringify(got2));
}})();
"#
        );

        let out = Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);

        let _ = std::fs::remove_file(lit_path);
        let _ = std::fs::remove_file(echo_path);

        assert!(
            !stdout.contains("FAIL"),
            "WASM W3a runtime check failed; node stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.matches("OK").count() >= 3,
            "expected >=3 OK lines (lit + echo + echo-long); stdout:\n{}",
            stdout
        );
    }

    /// W3b: a rule that uses `concat(...)` reserves the bump allocator
    /// local + the four per-concat scratches. With ONE text input
    /// field (2 i32 params), no number bindings, no min/max/abs, and
    /// no text bindings, the i32 locals group should hold exactly 5
    /// entries: the concat scratches (bump_ptr, cursor, result_ptr,
    /// arg_ptr, arg_len). Encoded as `01 05 7F`.
    #[test]
    fn wasm_w3b_concat_reserves_five_i32_scratches() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
rule greet
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = concat("hello, ", g.name)
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "greet", "concat_scratches");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Exactly one i32 group of 5 entries (no i64 group present:
        // no number bindings, no scratches).
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x05, 0x7F]),
            "expected single i32 locals group of 5 (concat scratches): {:02X?}",
            bytes
        );
        // memory.copy bytes (0xFC 0x0A 0x00 0x00) should appear once
        // per concat arg — here, twice.
        let copy_pat = [0xFCu8, 0x0A, 0x00, 0x00];
        let copy_count = bytes.windows(4).filter(|w| **w == copy_pat).count();
        assert_eq!(copy_count, 2, "expected 2 memory.copy ops, found {}", copy_count);
    }

    /// W3b: a `concat(...)` containing a number arg makes the module
    /// emit a 2-function shape (rule + itoa helper). Pinned by the
    /// type section declaring 2 types (`02` after the section length)
    /// and by the itoa-distinctive byte sequence appearing once in
    /// the code section.
    #[test]
    fn wasm_w3b_concat_with_number_arg_emits_itoa_func() {
        let src = r#"@verbose 0.1.0
concept Q
  @intention: "q"
  @source: invoices.intent:1
  fields:
    n : number
rule format_n
  @intention: "q"
  @source: invoices.intent:1
  input:
    q : Q
  output:
    out : text
  logic:
    out = concat("value=", q.n)
  proofs:
    purity:
      reads: [q.n]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "format_n", "concat_with_num");
        assert_eq!(&bytes[0..4], b"\0asm");
        // The function section content should be `02 00 01` (2 funcs,
        // type 0 then type 1). Pinned by direct byte search.
        assert!(
            bytes.windows(3).any(|w| w == [0x02, 0x00, 0x01]),
            "expected function section content `02 00 01` (rule + itoa)"
        );
        // itoa contains the unsigned div/rem opcodes (0x80, 0x82) —
        // these don't appear in any other emission today, so their
        // joint presence pins itoa being there.
        assert!(bytes.contains(&0x80), "expected i64.div_u from itoa");
        assert!(bytes.contains(&0x82), "expected i64.rem_u from itoa");
        // `call 1` (0x10 0x01) for the itoa invocation in concat.
        assert!(
            bytes.windows(2).any(|w| w == [0x10, 0x01]),
            "expected `call 1` (itoa) from concat"
        );
    }

    /// W3b: nested concat is refused with a clear pointer to the
    /// `let` workaround (which is now legal because text-let RHSes
    /// were just lifted).
    #[test]
    fn wasm_w3b_nested_concat_is_refused_with_let_hint() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
rule bad
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = concat("x", concat("a", g.name), "y")
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 2
"#;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        let path = "/tmp/wasm_w3b_nested_concat_refused.wasm";
        let err = compile_wasm(&program, "bad", path).expect_err("nested concat must be refused");
        assert!(
            err.message.contains("nested concat") && err.message.contains("let"),
            "error should hint at the let workaround; got: {}",
            err.message
        );
        let _ = std::fs::remove_file(path);
    }

    /// W3b end-to-end in node: exercises (a) text-only concat with a
    /// text-input field, (b) concat with a number arg via itoa across
    /// boundary values including i64::MIN, (c) sibling concats in
    /// the same call so the bump allocator advances correctly, and
    /// (d) two CALLS of the same module to confirm the allocator
    /// resets on entry. Like W1's runtime test, this is the only
    /// thing that catches a wrong scratch index or memcpy mishap.
    #[test]
    fn wasm_w3b_runtime_concat_text_and_numbers() {
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM W3b runtime test");
            return;
        }

        let src = r#"@verbose 0.1.0
concept G
  @intention: "g"
  @source: invoices.intent:1
  fields:
    name : text
    n : number
rule greet
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = concat("hello, ", g.name, "!")
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
rule format_with_num
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = concat(g.name, "=", g.n)
  proofs:
    purity:
      reads: [g.name, g.n]
      calls: []
    termination:
      bound: 2
rule chain_lets
  @intention: "g"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    let prefix = concat("[", g.name, "] ")
    let suffix = concat("(n=", g.n, ")")
    out = concat(prefix, suffix)
  proofs:
    purity:
      reads: [g.name, g.n]
      calls: []
    termination:
      bound: 4
"#;
        let greet_path = "/tmp/wasm_w3b_runtime_greet.wasm";
        let fmt_path = "/tmp/wasm_w3b_runtime_fmt.wasm";
        let chain_path = "/tmp/wasm_w3b_runtime_chain.wasm";

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "greet", greet_path).expect("compile greet");
        compile_wasm(&program, "format_with_num", fmt_path).expect("compile fmt");
        compile_wasm(&program, "chain_lets", chain_path).expect("compile chain");

        let script = format!(
            r#"
const fs = require("fs");
const dec = new TextDecoder();
const enc = new TextEncoder();

async function load(path) {{
  const buf = fs.readFileSync(path);
  if (!WebAssembly.validate(buf)) {{
    console.log("FAIL invalid module " + path);
    return null;
  }}
  return WebAssembly.instantiate(buf);
}}

function writeStr(mem, off, s) {{
  const bytes = enc.encode(s);
  mem.set(bytes, off);
  return bytes.length;
}}

(async () => {{
  // --- (a) text concat: "hello, " + name + "!"
  let m = await load("{greet_path}");
  let mem = new Uint8Array(m.instance.exports.memory.buffer);
  let nameLen = writeStr(mem, 4096, "Alice");
  // greet takes 3 i32 params (name_ptr, name_len, n) — n is unused
  // here but the signature includes it because the concept has it.
  let [p, l] = m.instance.exports.greet(4096, nameLen, 0n);
  let got = dec.decode((new Uint8Array(m.instance.exports.memory.buffer)).subarray(p, p+l));
  console.log(got === "hello, Alice!" ? "OK greet" : "FAIL greet got " + JSON.stringify(got));

  // --- (b) concat with number, including i64::MIN edge case
  m = await load("{fmt_path}");
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  for (const [name, n, expected] of [
    ["amount", 42n, "amount=42"],
    ["amount", 0n, "amount=0"],
    ["amount", -1n, "amount=-1"],
    ["balance", 9223372036854775807n, "balance=9223372036854775807"],
    ["balance", -9223372036854775808n, "balance=-9223372036854775808"],
  ]) {{
    mem = new Uint8Array(m.instance.exports.memory.buffer);
    const len = writeStr(mem, 4096, name);
    const [p2, l2] = m.instance.exports.format_with_num(4096, len, n);
    mem = new Uint8Array(m.instance.exports.memory.buffer);
    const out = dec.decode(mem.subarray(p2, p2+l2));
    console.log(out === expected ? "OK fmt " + n : "FAIL fmt " + n + " got " + JSON.stringify(out) + " expected " + expected);
  }}

  // --- (c) chain_lets: prefix and suffix concat each, then a
  //         third concat over both lets. Validates sibling-concat
  //         allocator advancement.
  m = await load("{chain_path}");
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  let chainLen = writeStr(mem, 4096, "Bob");
  let [p3, l3] = m.instance.exports.chain_lets(4096, chainLen, 99n);
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  let chainOut = dec.decode(mem.subarray(p3, p3+l3));
  console.log(chainOut === "[Bob] (n=99)" ? "OK chain" : "FAIL chain got " + JSON.stringify(chainOut));

  // --- (d) allocator reset: call greet twice, second call's bytes
  //         must equal the second input — not the first input's bytes.
  m = await load("{greet_path}");
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  let first = writeStr(mem, 4096, "First");
  let [p4, l4] = m.instance.exports.greet(4096, first, 0n);
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  const firstOut = dec.decode(mem.subarray(p4, p4+l4));
  let second = writeStr(mem, 8192, "Second");
  let [p5, l5] = m.instance.exports.greet(8192, second, 0n);
  mem = new Uint8Array(m.instance.exports.memory.buffer);
  const secondOut = dec.decode(mem.subarray(p5, p5+l5));
  // The second call's result_ptr should equal the FIRST call's
  // result_ptr (allocator reset on entry). Pin both contents.
  console.log(firstOut === "hello, First!" ? "OK reset-1" : "FAIL reset-1 got " + JSON.stringify(firstOut));
  console.log(secondOut === "hello, Second!" ? "OK reset-2" : "FAIL reset-2 got " + JSON.stringify(secondOut));
  console.log(p4 === p5 ? "OK reset-ptr" : "FAIL reset-ptr first=" + p4 + " second=" + p5);
}})();
"#
        );

        let out = Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);

        let _ = std::fs::remove_file(greet_path);
        let _ = std::fs::remove_file(fmt_path);
        let _ = std::fs::remove_file(chain_path);

        assert!(
            !stdout.contains("FAIL"),
            "WASM W3b runtime check failed; node stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        // We expect: 1 OK greet + 5 OK fmt + 1 OK chain + 3 OK reset = 10.
        assert!(
            stdout.matches("OK").count() >= 10,
            "expected >=10 OK lines; stdout:\n{}",
            stdout
        );
    }
}
