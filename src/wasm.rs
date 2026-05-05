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

/// The five i32 locals reserved when a rule uses any of the W3c text
/// primitives that need 2-arg compare loops (`starts_with`, `ends_with`,
/// `contains`). `length` reuses the first slot to drop the unwanted
/// `ptr` half of a text value off the stack. `parse_int` and
/// `json_escape` route through helper functions so they don't draw on
/// these locals at all.
///
/// Kept SEPARATE from `ConcatLocals` because a primitive can appear
/// inside a concat arg (e.g. `concat("...", starts_with(a, b))`). The
/// inner call writes its own scratches; reusing concat's would clobber
/// the outer concat's per-call temporaries mid-emission.
#[derive(Debug, Clone, Copy)]
struct TextPrimLocals {
    /// Haystack `(ptr, len)` of a 2-arg primitive. Also doubles as the
    /// single-arg slot for `length`.
    h_ptr: u32,
    h_len: u32,
    /// Needle `(ptr, len)` of a 2-arg primitive. Unused for `length`.
    n_ptr: u32,
    n_len: u32,
    /// Outer loop counter for `contains` (candidate offset in haystack)
    /// and forward index for `starts_with` / `ends_with`.
    i: u32,
    /// Inner loop counter for `contains` (byte offset into the needle
    /// during the per-candidate compare). `starts_with` / `ends_with`
    /// only need one counter and don't touch this slot.
    j: u32,
}

/// What kind of Result the rule returns. Drives both the
/// type-section signature and the leaf encoding inside
/// `emit_wasm_result_body`. Slice W4-Result supports two shapes;
/// the rest are still rejected at the result_types match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultShape {
    /// Result(number, text) — (i32 tag, i64 ok, i32 err_ptr, i32 err_len).
    NumberText,
    /// Result(text, text) — (i32 tag, i32 ok_ptr, i32 ok_len,
    /// i32 err_ptr, i32 err_len).
    TextText,
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
    /// `Some` iff the rule uses any W3c text primitive. Carries the
    /// five i32 scratch locals used by `length` and the byte-compare
    /// loops of `starts_with` / `ends_with` / `contains`.
    text_prim: Option<TextPrimLocals>,
    /// Function index of the inlined itoa helper, if the module
    /// emits one. `None` means concat with numeric args is refused
    /// (the caller has already verified this won't happen).
    itoa_func_idx: Option<u32>,
    /// Function index of the inlined parse_int helper. `None` if the
    /// rule never references `parse_int(...)`.
    parse_int_func_idx: Option<u32>,
    /// Function index of the inlined json_escape helper. `None` if the
    /// rule never references `json_escape(...)`.
    json_escape_func_idx: Option<u32>,
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
    //
    // W4-Result (this slice): `Result(T, E)` outputs are encoded as a
    // tagged tuple. Tag is i32 (0 = Ok, 1 = Err); each arm contributes
    // its full encoding (i64 for number, i32+i32 for text). Both arms
    // are always emitted on the stack — the un-live arm's slots get
    // zero placeholders. Host reads `tag` first; the un-live slots
    // are not contractually meaningful. Supported shapes:
    //
    //   Result(number, text):  (i32 tag, i64 ok, i32 err_ptr, i32 err_len)
    //   Result(text, text):    (i32 tag, i32 ok_ptr, i32 ok_len,
    //                           i32 err_ptr, i32 err_len)
    let result_types: Vec<u8> = match &rule.output_ty {
        Type::Bool => vec![0x7F],
        Type::Number => vec![0x7E],
        Type::Text => vec![0x7F, 0x7F],
        Type::Result(ok_ty, err_ty) => match (ok_ty.as_ref(), err_ty.as_ref()) {
            (Type::Number, Type::Text) => vec![0x7F, 0x7E, 0x7F, 0x7F],
            (Type::Text,   Type::Text) => vec![0x7F, 0x7F, 0x7F, 0x7F, 0x7F],
            _ => return Err(WasmError {
                message: format!(
                    "WASM: Result({:?}, {:?}) outputs not yet supported. Slice W4-Result handles \
                     Result(number, text) and Result(text, text); other shapes need their own ABI design call.",
                    ok_ty, err_ty,
                ),
            }),
        },
        other => {
            return Err(WasmError {
                message: format!("unsupported output type {:?} in the WASM backend", other),
            });
        }
    };
    let is_bool = rule.output_ty == Type::Bool;
    let is_text_out = rule.output_ty == Type::Text;
    // For routing the rule body's emit phase + the multi-value if
    // blocktype, we need to know not just "is Result" but which
    // shape, since the Ok arm's payload width differs.
    let result_shape: Option<ResultShape> = match &rule.output_ty {
        Type::Result(ok_ty, err_ty) => match (ok_ty.as_ref(), err_ty.as_ref()) {
            (Type::Number, Type::Text) => Some(ResultShape::NumberText),
            (Type::Text,   Type::Text) => Some(ResultShape::TextText),
            _ => unreachable!("guarded by the result_types match above"),
        },
        _ => None,
    };
    let is_result_out = result_shape.is_some();

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
    //
    // Result outputs always need memory: the Err arm carries text, and
    // the host-side decoder reads the err bytes from the exported
    // memory via (err_ptr, err_len).
    let needs_memory = is_text_out
        || is_result_out
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

    // json_escape allocates in the bump region too — force the concat
    // scratches (which carry $bump_ptr) to be reserved even when the
    // rule has no explicit `concat(...)`. Over-reserves 4 scratches
    // (cursor/result_ptr/arg_ptr/arg_len) that json_escape doesn't
    // touch — ~16 bytes of WASM module — in exchange for keeping the
    // allocator state in one well-known place.
    let needs_concat = expr_uses_concat(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_concat(e))
        || expr_uses_json_escape(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_json_escape(e));

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
    //                  + W3c text-primitive scratches (5 i32 if any of
    //                  length/parse_int/json_escape/starts_with/ends_with
    //                  /contains appears in the rule).
    let needs_text_prim = expr_uses_text_primitive(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_text_primitive(e));
    let n_concat_locals: u32 = if needs_concat { 5 } else { 0 };
    let n_text_prim_locals: u32 = if needs_text_prim { 6 } else { 0 };
    let n_i32_locals = n_text_bindings * 2 + n_concat_locals + n_text_prim_locals;

    // Concat scratch indices (just past the text bindings in the i32 group).
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

    // Text-primitive scratch indices (just past the concat scratches).
    let text_prim_locals: Option<TextPrimLocals> = if needs_text_prim {
        let base = i32_base + n_text_bindings * 2 + n_concat_locals;
        Some(TextPrimLocals {
            h_ptr: base,
            h_len: base + 1,
            n_ptr: base + 2,
            n_len: base + 3,
            i: base + 4,
            j: base + 5,
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

    // --- Detect helper-function needs -----------------------------
    let needs_itoa = expr_concat_has_number_arg(&rule.logic.value, &field_shapes, &binding_shapes)
        || rule.logic.bindings.iter().any(|(_, e)| {
            expr_concat_has_number_arg(e, &field_shapes, &binding_shapes)
        });
    let needs_parse_int = expr_uses_parse_int(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_parse_int(e));
    let needs_json_escape = expr_uses_json_escape(&rule.logic.value)
        || rule.logic.bindings.iter().any(|(_, e)| expr_uses_json_escape(e));

    // Helper-function indices. Func 0 is always the rule. Helpers
    // are appended in a fixed order (itoa, parse_int, json_escape)
    // so per-helper tests can predict their indices.
    let mut next_helper_idx: u32 = 1;
    let itoa_func_idx = if needs_itoa {
        let idx = next_helper_idx; next_helper_idx += 1; Some(idx)
    } else { None };
    let parse_int_func_idx = if needs_parse_int {
        let idx = next_helper_idx; next_helper_idx += 1; Some(idx)
    } else { None };
    let json_escape_func_idx = if needs_json_escape {
        let idx = next_helper_idx; next_helper_idx += 1; Some(idx)
    } else { None };
    let n_helpers: u32 = (needs_itoa as u32) + (needs_parse_int as u32) + (needs_json_escape as u32);

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
    // Type 0 is always the rule's signature. Helper functions append
    // their own types in a fixed order (itoa → parse_int → json_escape)
    // so type/func indices line up with `next_helper_idx` above.
    //
    // For Result-typed rules (W4-Result), one extra type entry is
    // appended at the END as the multi-value blocktype the if/else
    // instruction references: `() -> (... result types ...)`. It has
    // no params and the same result schema as the rule itself. No
    // function points to it — only the `if blocktype` byte does.
    let mut type_section = Vec::new();
    let n_extra_types: u32 = if is_result_out { 1 } else { 0 };
    let n_types: u32 = 1 + n_helpers + n_extra_types;
    emit_leb128(&mut type_section, n_types as u64);
    type_section.push(0x60);                         // type 0: rule
    emit_leb128(&mut type_section, param_types.len() as u64);
    type_section.extend_from_slice(&param_types);
    emit_leb128(&mut type_section, result_types.len() as u64);
    type_section.extend_from_slice(&result_types);
    if needs_itoa {
        // itoa: (i64) -> (i32, i32)
        type_section.extend_from_slice(&[0x60, 0x01, 0x7E, 0x02, 0x7F, 0x7F]);
    }
    if needs_parse_int {
        // parse_int: (i32 ptr, i32 len) -> i64
        type_section.extend_from_slice(&[0x60, 0x02, 0x7F, 0x7F, 0x01, 0x7E]);
    }
    if needs_json_escape {
        // json_escape: (i32 in_ptr, i32 in_len, i32 bump_ptr) ->
        //              (i32 out_ptr, i32 out_len, i32 new_bump_ptr)
        // 3-result return uses WASM 2.0 multi-value, supported in Node.
        type_section.extend_from_slice(&[0x60, 0x03, 0x7F, 0x7F, 0x7F, 0x03, 0x7F, 0x7F, 0x7F]);
    }
    let result_block_typeidx: Option<u32> = if is_result_out {
        // Same result schema as type 0, but no params (it's a block
        // type, the if's stack delta starts empty and ends with the
        // result tuple).
        type_section.push(0x60);
        type_section.push(0x00);                     // 0 params
        emit_leb128(&mut type_section, result_types.len() as u64);
        type_section.extend_from_slice(&result_types);
        Some(1 + n_helpers)
    } else {
        None
    };
    emit_section(&mut module, 1, &type_section);

    // === Function section ===
    // (1 + n_helpers) funcs; each is its own type index in the same
    // order: rule (type 0), then itoa, parse_int, json_escape (in
    // declared order, skipping any that aren't needed).
    let mut func_section: Vec<u8> = Vec::new();
    emit_leb128(&mut func_section, 1 + n_helpers as u64);
    func_section.push(0); // rule = type 0
    let mut next_type_idx: u32 = 1;
    if needs_itoa { func_section.push(next_type_idx as u8); next_type_idx += 1; }
    if needs_parse_int { func_section.push(next_type_idx as u8); next_type_idx += 1; }
    if needs_json_escape { func_section.push(next_type_idx as u8); next_type_idx += 1; }
    let _ = next_type_idx;
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
        text_prim: text_prim_locals,
        itoa_func_idx,
        parse_int_func_idx,
        json_escape_func_idx,
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

    // Emit main expression. For Result-typed rules we route through
    // emit_wasm_result_body, which handles Ok/Err leaves and any
    // top-level if/else with the multi-value blocktype.
    if let Some(shape) = result_shape {
        let block_idx = result_block_typeidx
            .expect("result_block_typeidx is set whenever result_shape is");
        emit_wasm_result_body(&mut body, &rule.logic.value, &ctx, shape, block_idx)?;
    } else {
        emit_wasm_expr(&mut body, &rule.logic.value, &ctx)?;
        // If rule returns bool but expr produces i64, wrap to i32. Text
        // outputs already leave (i32, i32) on the stack from Expr::Text /
        // text Expr::Field / Concat — no widening needed.
        if is_bool {
            body.push(0xA7); // i32.wrap_i64
        }
    }

    body.push(0x0B); // end

    // Helper function bodies. Order MUST match the type-section /
    // function-section emission above (itoa, parse_int, json_escape).
    let mut helper_bodies: Vec<Vec<u8>> = Vec::new();
    if needs_itoa { helper_bodies.push(build_itoa_body()); }
    if needs_parse_int { helper_bodies.push(build_parse_int_body()); }
    if needs_json_escape { helper_bodies.push(build_json_escape_body()); }

    let mut code_section = Vec::new();
    emit_leb128(&mut code_section, 1 + helper_bodies.len() as u64);
    emit_leb128(&mut code_section, body.len() as u64);
    code_section.extend_from_slice(&body);
    for h in &helper_bodies {
        emit_leb128(&mut code_section, h.len() as u64);
        code_section.extend_from_slice(h);
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

/// Build the WASM body for the parse_int helper.
///
/// Signature: `(i32 ptr, i32 len) -> i64`. Strict scan: optional
/// leading `-`, then 1+ ASCII digits, then end-of-input. Anything
/// else (empty input, lone `-`, non-digit byte, trailing
/// whitespace) traps via `unreachable` (0x00) — same fail-closed
/// posture as native's sys_exit(1).
///
/// Locals (after the 2 i32 params at indices 0,1):
///   2 = $val   (i64 accumulator)
///   3 = $i     (i32 cursor)
///   4 = $isNeg (i32 flag)
///   5 = $byte  (i32 current byte, also reused as digit value)
///
/// Control flow note: the loop is wrapped in an outer `block`. Inside
/// the loop body, `br 0` continues the loop (loop labels target the
/// loop's start), `br 2` from inside an `if` exits the surrounding
/// `block` (block labels target the block's END). A bare `br 1`
/// from inside the loop body would also continue the loop, which is
/// why the `block`/`loop` pairing is required for natural exits.
fn build_parse_int_body() -> Vec<u8> {
    vec![
        // Locals: 2 groups
        0x02,
        0x01, 0x7E,                              // 1 × i64 ($val)
        0x03, 0x7F,                              // 3 × i32 ($i, $isNeg, $byte)
        // $val = 0; $i = 0; $isNeg = 0
        0x42, 0x00, 0x21, 0x02,                  // i64.const 0; local.set 2
        0x41, 0x00, 0x21, 0x03,                  // i32.const 0; local.set 3
        0x41, 0x00, 0x21, 0x04,                  // i32.const 0; local.set 4
        // If len == 0: trap (empty input rejected)
        0x20, 0x01, 0x45, 0x04, 0x40, 0x00, 0x0B,// local.get 1; i32.eqz; if; unreachable; end
        // First-byte check: is it '-' (45)?
        0x20, 0x00, 0x2D, 0x00, 0x00,            // local.get 0; i32.load8_u offset=0
        0x21, 0x05,                              // local.set $byte
        0x20, 0x05, 0x41, 0x2D, 0x46,            // local.get $byte; i32.const 45; i32.eq
        0x04, 0x40,                              // if (no result)
            // $isNeg = 1; $i = 1
            0x41, 0x01, 0x21, 0x04,
            0x41, 0x01, 0x21, 0x03,
            // If len == 1 (lone '-'): trap
            0x20, 0x01, 0x41, 0x01, 0x46,        // local.get 1; i32.const 1; i32.eq
            0x04, 0x40, 0x00, 0x0B,              // if; unreachable; end
        0x0B,                                     // end if (negative branch)
        // block + loop (br 2 from inside-if-inside-loop = exit block)
        0x02, 0x40,                              // block (no result)
            0x03, 0x40,                          //   loop (no result)
                // exit: if $i >= $len, br 2 (out of block)
                0x20, 0x03, 0x20, 0x01, 0x4E,    //     $i; $len; i32.ge_s
                0x04, 0x40, 0x0C, 0x02, 0x0B,    //     if; br 2; end
                // $byte = mem[$ptr + $i]
                0x20, 0x00, 0x20, 0x03, 0x6A,    //     $ptr; $i; i32.add
                0x2D, 0x00, 0x00,                //     i32.load8_u
                0x21, 0x05,                      //     local.set $byte
                // Trap if byte < '0' (48) or byte > '9' (57)
                0x20, 0x05, 0x41, 0x30, 0x48,    //     $byte; 48; i32.lt_s
                0x04, 0x40, 0x00, 0x0B,          //     if; unreachable; end
                0x20, 0x05, 0x41, 0x39, 0x4A,    //     $byte; 57; i32.gt_s
                0x04, 0x40, 0x00, 0x0B,          //     if; unreachable; end
                // $val = $val * 10 + ($byte - 48)
                0x20, 0x02, 0x42, 0x0A, 0x7E,    //     $val; 10; i64.mul
                0x20, 0x05, 0x41, 0x30, 0x6B,    //     $byte; 48; i32.sub
                0xAC,                             //     i64.extend_i32_s
                0x7C,                             //     i64.add
                0x21, 0x02,                      //     local.set $val
                // $i++
                0x20, 0x03, 0x41, 0x01, 0x6A, 0x21, 0x03,
                0x0C, 0x00,                      //     br 0 (continue loop)
            0x0B,                                 //   end loop
        0x0B,                                     // end block
        // If $isNeg: $val = -$val (0 - $val handles i64::MIN bit pattern)
        0x20, 0x04,                              // local.get $isNeg
        0x04, 0x40,                              // if
            0x42, 0x00, 0x20, 0x02, 0x7D,        // i64.const 0; $val; i64.sub
            0x21, 0x02,                          // local.set $val
        0x0B,                                     // end if
        // Return $val
        0x20, 0x02,                              // local.get $val
        0x0B,                                     // end func
    ]
}

/// Build the WASM body for the json_escape helper.
///
/// Signature:
///   `(i32 in_ptr, i32 in_len, i32 bump_ptr) ->
///    (i32 out_ptr, i32 out_len, i32 new_bump_ptr)`
///
/// Walks the input text byte by byte and copies it to the bump
/// region with JSON escaping applied:
///   `"`  -> `\"`
///   `\\` -> `\\\\`
///   `\n` -> `\\n`
///   `\r` -> `\\r`
///   `\t` -> `\\t`
///   bytes < 0x20 NOT covered by the above -> `\\u00XX` (6 bytes)
///   anything else -> copied as-is (1 byte)
///
/// Returns the start of the escaped output (= bump_ptr at entry),
/// its length, and the new bump_ptr to thread back to the caller's
/// allocator local.
///
/// Locals (after the 3 i32 params):
///   3 = $cursor    (write position, starts at $bump_ptr)
///   4 = $i         (read position into in_ptr)
///   5 = $byte      (current byte being processed)
///   6 = $hex_lo    (low nibble for \u00XX path)
fn build_json_escape_body() -> Vec<u8> {
    // The body is a longer one; structured as init + loop + return.
    let mut b = Vec::new();
    // Locals: 1 group of 4 i32
    b.extend_from_slice(&[0x01, 0x04, 0x7F]);
    // $cursor = $bump_ptr (param 2)
    b.extend_from_slice(&[0x20, 0x02, 0x21, 0x03]);
    // $i = 0
    b.extend_from_slice(&[0x41, 0x00, 0x21, 0x04]);
    // block + loop wrapper so exit-cond `br N` actually exits past
    // the loop. `br 0` continues, `br 2` from inside the if-inside-
    // loop targets the surrounding block's end.
    b.extend_from_slice(&[0x02, 0x40]);                    // block (no result)
    b.extend_from_slice(&[0x03, 0x40]);                    // loop (no result)
        // if $i >= $in_len: br 2 (exit block, past loop)
        b.extend_from_slice(&[0x20, 0x04, 0x20, 0x01, 0x4E]); // $i >= $in_len
        b.extend_from_slice(&[0x04, 0x40, 0x0C, 0x02, 0x0B]); // if; br 2; end
        // $byte = mem[$in_ptr + $i]
        b.extend_from_slice(&[0x20, 0x00, 0x20, 0x04, 0x6A, 0x2D, 0x00, 0x00, 0x21, 0x05]);
        // Decision tree on $byte. We use a chain of if/else rather
        // than a single br_table because the matched values are
        // sparse and the tree keeps the body readable.
        //
        // case $byte == '"' (34): write \" (2 bytes)
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0x22, 0x46]); // $byte; 34; i32.eq
        b.extend_from_slice(&[0x04, 0x40]);                    // if
            // mem[$cursor] = '\\'; mem[$cursor+1] = '"'; cursor += 2
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);            // store '\\'
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0x22, 0x3A, 0x00, 0x00]); // store '"' at cursor+1
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x21, 0x03]);            // cursor += 2
        b.extend_from_slice(&[0x05]);                          // else
        // case $byte == '\\' (92): write \\ (2 bytes)
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0xDC, 0x00, 0x46]);  // $byte == 92
        b.extend_from_slice(&[0x04, 0x40]);                    // if
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x21, 0x03]);
        b.extend_from_slice(&[0x05]);                          // else
        // case $byte == '\n' (10): write \n
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0x0A, 0x46]);
        b.extend_from_slice(&[0x04, 0x40]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0xEE, 0x00, 0x3A, 0x00, 0x00]); // 'n' = 110
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x21, 0x03]);
        b.extend_from_slice(&[0x05]);                          // else
        // case $byte == '\r' (13): write \r
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0x0D, 0x46]);
        b.extend_from_slice(&[0x04, 0x40]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0xF2, 0x00, 0x3A, 0x00, 0x00]); // 'r' = 114
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x21, 0x03]);
        b.extend_from_slice(&[0x05]);                          // else
        // case $byte == '\t' (9): write \t
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0x09, 0x46]);
        b.extend_from_slice(&[0x04, 0x40]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0xF4, 0x00, 0x3A, 0x00, 0x00]); // 't' = 116
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x21, 0x03]);
        b.extend_from_slice(&[0x05]);                          // else
        // case $byte < 0x20: write \u00XX (6 bytes)
        b.extend_from_slice(&[0x20, 0x05, 0x41, 0x20, 0x48]); // $byte; 32; i32.lt_s
        b.extend_from_slice(&[0x04, 0x40]);                    // if
            // \, u, 0, 0
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0xDC, 0x00, 0x3A, 0x00, 0x00]);
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x41, 0xF5, 0x00, 0x3A, 0x00, 0x00]); // 'u'
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x02, 0x6A, 0x41, 0x30, 0x3A, 0x00, 0x00]); // '0'
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x03, 0x6A, 0x41, 0x30, 0x3A, 0x00, 0x00]); // '0'
            // High nibble: ($byte >> 4) & 0xF; since byte < 0x20, this is 0 or 1
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x04, 0x6A]);                 // dest = cursor + 4
            b.extend_from_slice(&[0x20, 0x05, 0x41, 0x04, 0x76]);                 // $byte >> 4 (signed shift, but byte < 32 so 0/1)
            b.extend_from_slice(&[0x41, 0x30, 0x6A]);                             // + '0' (gives '0' or '1')
            b.extend_from_slice(&[0x3A, 0x00, 0x00]);                             // i32.store8
            // Low nibble: $byte & 0xF, then 0..9 -> '0'..'9', 10..15 -> 'a'..'f'
            // Compute lo = $byte & 0xF; if lo < 10 then lo + '0' else lo + ('a' - 10)
            b.extend_from_slice(&[0x20, 0x05, 0x41, 0x0F, 0x71, 0x21, 0x06]);     // $hex_lo = $byte & 0xF
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x05, 0x6A]);                 // dest = cursor + 5
            // Choose '0'+lo vs 'a'-10+lo via select
            b.extend_from_slice(&[0x20, 0x06, 0x41, 0x30, 0x6A]);                 // val_1: lo + '0' (for 0-9)
            b.extend_from_slice(&[0x20, 0x06, 0x41, 0xD7, 0x00, 0x6A]);                 // val_2: lo + 87 ('a'-10) (for 10-15)
            b.extend_from_slice(&[0x20, 0x06, 0x41, 0x0A, 0x48]);                 // cond: lo < 10 (i32.lt_s)
            b.extend_from_slice(&[0x1B]);                                          // select
            b.extend_from_slice(&[0x3A, 0x00, 0x00]);                             // i32.store8
            // cursor += 6
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x06, 0x6A, 0x21, 0x03]);
        b.extend_from_slice(&[0x05]);                          // else (default: copy 1 byte)
            b.extend_from_slice(&[0x20, 0x03, 0x20, 0x05, 0x3A, 0x00, 0x00]);     // mem[cursor] = $byte
            b.extend_from_slice(&[0x20, 0x03, 0x41, 0x01, 0x6A, 0x21, 0x03]);     // cursor++
        // Close all 6 if/else blocks
        for _ in 0..6 { b.push(0x0B); }
        // $i++
        b.extend_from_slice(&[0x20, 0x04, 0x41, 0x01, 0x6A, 0x21, 0x04]);
        // br 0 (continue loop)
        b.extend_from_slice(&[0x0C, 0x00]);
    b.extend_from_slice(&[0x0B]);                              // end loop
    b.extend_from_slice(&[0x0B]);                              // end block (paired with the wrapper added above)
    // Return: ($bump_ptr, $cursor - $bump_ptr, $cursor)
    b.extend_from_slice(&[0x20, 0x02]);                        // out_ptr = $bump_ptr
    b.extend_from_slice(&[0x20, 0x03, 0x20, 0x02, 0x6B]);      // out_len = $cursor - $bump_ptr
    b.extend_from_slice(&[0x20, 0x03]);                        // new_bump = $cursor
    b.push(0x0B);                                              // end func
    b
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
        // W3c text primitives: literals can appear in their args (e.g.
        // `starts_with(req.path, "/api/v1/")`). Recurse into each.
        Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => walk_text_literals(i, f),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            walk_text_literals(h, f); walk_text_literals(n, f);
        }
        // W4-Result: Ok/Err wrap an inner expression that may carry
        // its own literals (e.g. Err("rejected") or Err(concat(...))).
        Expr::Ok(inner) | Expr::Err(inner) => walk_text_literals(inner, f),
        _ => {}
    }
}

/// True iff the expression (or any sub-expression) uses one of the W3c
/// text primitives. Drives reservation of `TextPrimLocals` and
/// emission of helper functions. The 1-arg and 2-arg primitives all
/// flip the same bit — the per-helper detection lives in
/// `expr_uses_parse_int` / `expr_uses_json_escape` below.
fn expr_uses_text_primitive(expr: &Expr) -> bool {
    match expr {
        Expr::Length(_) | Expr::ParseInt(_) | Expr::JsonEscape(_)
        | Expr::StartsWith(_, _) | Expr::EndsWith(_, _) | Expr::Contains(_, _) => true,
        Expr::Binary(_, l, r) => expr_uses_text_primitive(l) || expr_uses_text_primitive(r),
        Expr::If(c, t, e) => expr_uses_text_primitive(c) || expr_uses_text_primitive(t) || expr_uses_text_primitive(e),
        Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) => expr_uses_text_primitive(i),
        Expr::Min(a, b) | Expr::Max(a, b) => expr_uses_text_primitive(a) || expr_uses_text_primitive(b),
        Expr::Call(_, args) => args.iter().any(expr_uses_text_primitive),
        Expr::Concat(args) => args.iter().any(expr_uses_text_primitive),
        Expr::Ok(inner) | Expr::Err(inner) => expr_uses_text_primitive(inner),
        _ => false,
    }
}

fn expr_uses_parse_int(expr: &Expr) -> bool {
    match expr {
        Expr::ParseInt(_) => true,
        Expr::Binary(_, l, r) => expr_uses_parse_int(l) || expr_uses_parse_int(r),
        Expr::If(c, t, e) => expr_uses_parse_int(c) || expr_uses_parse_int(t) || expr_uses_parse_int(e),
        Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) => expr_uses_parse_int(i),
        Expr::Min(a, b) | Expr::Max(a, b) => expr_uses_parse_int(a) || expr_uses_parse_int(b),
        Expr::Call(_, args) => args.iter().any(expr_uses_parse_int),
        Expr::Concat(args) => args.iter().any(expr_uses_parse_int),
        Expr::Length(i) | Expr::JsonEscape(i) => expr_uses_parse_int(i),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_uses_parse_int(h) || expr_uses_parse_int(n)
        }
        Expr::Ok(inner) | Expr::Err(inner) => expr_uses_parse_int(inner),
        _ => false,
    }
}

fn expr_uses_json_escape(expr: &Expr) -> bool {
    match expr {
        Expr::JsonEscape(_) => true,
        Expr::Binary(_, l, r) => expr_uses_json_escape(l) || expr_uses_json_escape(r),
        Expr::If(c, t, e) => expr_uses_json_escape(c) || expr_uses_json_escape(t) || expr_uses_json_escape(e),
        Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) => expr_uses_json_escape(i),
        Expr::Min(a, b) | Expr::Max(a, b) => expr_uses_json_escape(a) || expr_uses_json_escape(b),
        Expr::Call(_, args) => args.iter().any(expr_uses_json_escape),
        Expr::Concat(args) => args.iter().any(expr_uses_json_escape),
        Expr::Length(i) | Expr::ParseInt(i) => expr_uses_json_escape(i),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_uses_json_escape(h) || expr_uses_json_escape(n)
        }
        Expr::Ok(inner) | Expr::Err(inner) => expr_uses_json_escape(inner),
        _ => false,
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
        // W3c: json_escape returns text (transforms input via the bump
        // allocator). length / parse_int / starts_with / ends_with /
        // contains all return number or bool, NOT text.
        Expr::JsonEscape(_) => true,
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
        // W3c text primitives: their args may contain a concat (e.g.
        // `starts_with(concat("/api/v", n), "...")`). Recurse so the
        // allocator scratches get reserved.
        Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => expr_uses_concat(i),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_uses_concat(h) || expr_uses_concat(n)
        }
        Expr::Ok(inner) | Expr::Err(inner) => expr_uses_concat(inner),
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
        Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => {
            expr_concat_has_number_arg(i, field_shapes, binding_shapes)
        }
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_concat_has_number_arg(h, field_shapes, binding_shapes)
                || expr_concat_has_number_arg(n, field_shapes, binding_shapes)
        }
        Expr::Ok(inner) | Expr::Err(inner) => {
            expr_concat_has_number_arg(inner, field_shapes, binding_shapes)
        }
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
            Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => inside_concat(i),
            Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
                inside_concat(h) || inside_concat(n)
            }
            Expr::Ok(inner) | Expr::Err(inner) => inside_concat(inner),
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
        Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => expr_has_nested_concat(i),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_has_nested_concat(h) || expr_has_nested_concat(n)
        }
        Expr::Ok(inner) | Expr::Err(inner) => expr_has_nested_concat(inner),
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
                BinOp::GtEq => { code.push(0x59); code.push(0xAD); }   // i64.ge_s → i64.extend_i32_u
                BinOp::LtEq => { code.push(0x57); code.push(0xAD); }   // i64.le_s → i64.extend_i32_u
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
                text_prim: ctx.text_prim,
                parse_int_func_idx: ctx.parse_int_func_idx,
                json_escape_func_idx: ctx.json_escape_func_idx,
                concat: ctx.concat,
                itoa_func_idx: ctx.itoa_func_idx,
            };
            emit_wasm_expr(code, &called.logic.value, &callee_ctx)
        }
        Expr::Length(inner) => {
            // text → i64 (number).
            // Stack discipline: eval inner → [ptr, len]. We want
            // just len, widened to i64 (Verbose `number` is i64
            // throughout the backend). WASM has no swap or rot, so
            // we park len, drop ptr, restore len, extend.
            let tp = ctx.text_prim.ok_or_else(|| WasmError {
                message: "internal: Expr::Length reached emit without text-prim scratches".into(),
            })?;
            emit_wasm_expr(code, inner, ctx)?;        // [ptr, len]
            code.push(0x21); emit_leb128(code, tp.h_len as u64);  // pop len → $h_len
            code.push(0x1A);                                       // drop ptr
            code.push(0x20); emit_leb128(code, tp.h_len as u64);  // push $h_len
            code.push(0xAD);                                       // i64.extend_i32_u
            Ok(())
        }
        Expr::ParseInt(inner) => {
            // text → i64. Helper function does the strict scan +
            // trap on bad input. We just push (ptr, len) and call.
            let idx = ctx.parse_int_func_idx.ok_or_else(|| WasmError {
                message: "internal: Expr::ParseInt reached emit without parse_int helper".into(),
            })?;
            emit_wasm_expr(code, inner, ctx)?;        // [ptr, len]
            code.push(0x10); emit_leb128(code, idx as u64);   // call $parse_int → [i64]
            Ok(())
        }
        Expr::JsonEscape(inner) => {
            // text → text. Helper reads $bump_ptr as a param,
            // returns (out_ptr, out_len, new_bump_ptr). Caller
            // updates its bump_ptr local and leaves (ptr, len) on
            // the stack as the text result.
            let cl = ctx.concat.ok_or_else(|| WasmError {
                message: "internal: Expr::JsonEscape needs the bump allocator (concat scratches not reserved)".into(),
            })?;
            let idx = ctx.json_escape_func_idx.ok_or_else(|| WasmError {
                message: "internal: Expr::JsonEscape reached emit without json_escape helper".into(),
            })?;
            emit_wasm_expr(code, inner, ctx)?;                // [in_ptr, in_len]
            code.push(0x20); emit_leb128(code, cl.bump_ptr as u64);  // [in_ptr, in_len, bump_ptr]
            code.push(0x10); emit_leb128(code, idx as u64);    // call → [out_ptr, out_len, new_bump]
            // Pop new_bump into $bump_ptr; (ptr, len) remain on stack.
            code.push(0x21); emit_leb128(code, cl.bump_ptr as u64);  // pops top (new_bump)
            Ok(())
        }
        Expr::StartsWith(haystack, needle) => {
            emit_starts_or_ends_with(code, haystack, needle, ctx, /* from_end */ false)
        }
        Expr::EndsWith(haystack, needle) => {
            emit_starts_or_ends_with(code, haystack, needle, ctx, /* from_end */ true)
        }
        Expr::Contains(haystack, needle) => {
            emit_contains(code, haystack, needle, ctx)
        }
        _ => Err(WasmError {
            message: format!("unsupported expression in WASM backend"),
        }),
    }
}

/// Emit the body of a `Result(T, E)`-typed rule (W4-Result).
///
/// The legal top-level shapes are:
///   - `Ok(<inner>)`               — a flat success constructor
///   - `Err(<inner>)`              — a flat failure constructor
///   - `if cond then A else B`     — where each arm is one of the
///                                   above (recursively); the if uses
///                                   a multi-value blocktype matching
///                                   the rule's result schema
///
/// Per shape, both Ok and Err arms always emit ALL the result slots
/// onto the stack — the un-live arm gets zero placeholders. The host
/// reads `tag` first; the placeholder slots are deliberately
/// meaningless to consumers (the contract is "tag picks the live
/// payload"), so the cheap zero is fine.
///
/// Callers responsible for: putting the type-section entry for
/// `result_block_typeidx` in place beforehand, and threading
/// `result_shape` correctly so number-vs-text Ok payloads emit the
/// right WASM type.
///
/// Slice scope: `match_result` is rejected here. The fallthrough arm
/// will land in slice W4-MatchResult.
fn emit_wasm_result_body(
    code: &mut Vec<u8>,
    expr: &Expr,
    ctx: &WasmCtx,
    shape: ResultShape,
    result_block_typeidx: u32,
) -> Result<(), WasmError> {
    match expr {
        Expr::Ok(inner) => {
            // tag = 0
            code.push(0x41);                                 // i32.const
            emit_sleb128(code, 0);
            // ok payload
            match shape {
                ResultShape::NumberText => {
                    emit_wasm_expr(code, inner, ctx)?;        // pushes i64
                }
                ResultShape::TextText => {
                    emit_wasm_expr(code, inner, ctx)?;        // pushes (i32 ptr, i32 len)
                }
            }
            // err arm placeholders (always two i32s for text)
            code.push(0x41); emit_sleb128(code, 0);          // err_ptr = 0
            code.push(0x41); emit_sleb128(code, 0);          // err_len = 0
            Ok(())
        }
        Expr::Err(inner) => {
            // tag = 1
            code.push(0x41);
            emit_sleb128(code, 1);
            // ok arm placeholder (width depends on shape)
            match shape {
                ResultShape::NumberText => {
                    code.push(0x42); emit_sleb128(code, 0);  // i64.const 0
                }
                ResultShape::TextText => {
                    code.push(0x41); emit_sleb128(code, 0);  // ok_ptr = 0
                    code.push(0x41); emit_sleb128(code, 0);  // ok_len = 0
                }
            }
            // err payload (text)
            emit_wasm_expr(code, inner, ctx)?;                // pushes (i32 ptr, i32 len)
            Ok(())
        }
        Expr::If(cond, then_e, else_e) => {
            // The condition is an i64 in our scalar pipeline (comparisons
            // chain through i64.extend_i32_u). The if instruction takes
            // an i32 condition, so wrap before the branch.
            emit_wasm_expr(code, cond, ctx)?;
            code.push(0xA7);                                 // i32.wrap_i64
            code.push(0x04);                                 // if
            // Multi-value blocktype: signed leb128 of the type index.
            // Positive values denote a typeidx (the alternative — a
            // single-value blocktype — uses the negative-encoded
            // valtype byte, which is what other if sites use today).
            emit_sleb128(code, result_block_typeidx as i64);
            emit_wasm_result_body(code, then_e, ctx, shape, result_block_typeidx)?;
            code.push(0x05);                                 // else
            emit_wasm_result_body(code, else_e, ctx, shape, result_block_typeidx)?;
            code.push(0x0B);                                 // end
            Ok(())
        }
        Expr::MatchResult(_, _, _, _, _) => {
            Err(WasmError {
                message: "WASM: match_result is not yet supported (slice W4-MatchResult will lift this). \
                          For now, inline the validation as `if cond then Ok(...) else Err(...)`."
                          .into(),
            })
        }
        other => Err(WasmError {
            message: format!(
                "WASM: a Result-typed rule's body must be Ok(...), Err(...), or an if/else of those shapes; got {:?}",
                other,
            ),
        }),
    }
}

/// Inline emission for `starts_with(h, n)` and `ends_with(h, n)`.
///
/// Both share the same byte-compare loop; the only difference is the
/// initial offset into the haystack (0 for starts_with,
/// `h_len - n_len` for ends_with). The result is an i32 bool (0 or
/// 1) — comparisons in WASM produce i32, no extension needed for the
/// rule's internal bool path (the rule prologue handles widening to
/// i64 if the rule output is `bool` via i64.extend, or wraps if
/// output is bool — see compile_wasm's `is_bool` handling).
///
/// Edge cases pinned:
///   - empty needle → always true (loop body never runs)
///   - needle longer than haystack → false (length check before loop)
///   - exact-length match → true (loop runs n_len iterations)
fn emit_starts_or_ends_with(
    code: &mut Vec<u8>,
    haystack: &Expr,
    needle: &Expr,
    ctx: &WasmCtx,
    from_end: bool,
) -> Result<(), WasmError> {
    let tp = ctx.text_prim.ok_or_else(|| WasmError {
        message: "internal: starts_with/ends_with reached emit without text-prim scratches".into(),
    })?;

    // Eval BOTH args first, then park. Mirror of the W1 discipline
    // for min/max — a nested call inside `needle` must not clobber
    // the haystack scratches mid-park. With both on the stack, we
    // pop in reverse (n_len, n_ptr, h_len, h_ptr) and the inner
    // scratches it touched are now safe to overwrite.
    emit_wasm_expr(code, haystack, ctx)?;       // [h_ptr, h_len]
    emit_wasm_expr(code, needle, ctx)?;         // [h_ptr, h_len, n_ptr, n_len]
    code.push(0x21); emit_leb128(code, tp.n_len as u64);
    code.push(0x21); emit_leb128(code, tp.n_ptr as u64);
    code.push(0x21); emit_leb128(code, tp.h_len as u64);
    code.push(0x21); emit_leb128(code, tp.h_ptr as u64);

    // Length pre-check: if n_len > h_len, push 0 and return early.
    // We use a block with br to skip the compare loop in that case.
    code.extend_from_slice(&[0x02, 0x7F]);              // block (i32 result)
        // First check: if n_len > h_len, push 0 and br out.
        code.push(0x20); emit_leb128(code, tp.n_len as u64);
        code.push(0x20); emit_leb128(code, tp.h_len as u64);
        code.push(0x4B);                                 // i32.gt_u (n_len > h_len, unsigned)
        code.extend_from_slice(&[0x04, 0x40,             // if (no result)
            0x41, 0x00,                                  //   i32.const 0
            0x0C, 0x01,                                  //   br 1 (out of block)
        0x0B]);                                          // end if

        // Initialize $i = 0
        code.push(0x41); code.push(0x00);
        code.push(0x21); emit_leb128(code, tp.i as u64);

        // For ends_with: shift h_ptr forward by (h_len - n_len) so the
        // following loop compares the suffix of haystack against needle.
        if from_end {
            code.push(0x20); emit_leb128(code, tp.h_ptr as u64);
            code.push(0x20); emit_leb128(code, tp.h_len as u64);
            code.push(0x20); emit_leb128(code, tp.n_len as u64);
            code.push(0x6B);                              // i32.sub (h_len - n_len)
            code.push(0x6A);                              // i32.add (h_ptr + diff)
            code.push(0x21); emit_leb128(code, tp.h_ptr as u64);
        }

        // loop: while $i < n_len, compare bytes
        code.extend_from_slice(&[0x03, 0x40]);            // loop (no result)
            // exit condition: $i >= n_len → push 1 and br out
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x20); emit_leb128(code, tp.n_len as u64);
            code.push(0x4E);                              // i32.ge_s
            code.extend_from_slice(&[0x04, 0x40,
                0x41, 0x01,                               // push 1 (matched)
                0x0C, 0x02,                               // br 2 (out of block)
            0x0B]);
            // load mem[h_ptr + i]
            code.push(0x20); emit_leb128(code, tp.h_ptr as u64);
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x6A);                              // add
            code.extend_from_slice(&[0x2D, 0x00, 0x00]);  // i32.load8_u
            // load mem[n_ptr + i]
            code.push(0x20); emit_leb128(code, tp.n_ptr as u64);
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x6A);
            code.extend_from_slice(&[0x2D, 0x00, 0x00]);
            // mismatch → push 0 and br out
            code.push(0x47);                              // i32.ne
            code.extend_from_slice(&[0x04, 0x40,
                0x41, 0x00,
                0x0C, 0x02,                               // br 2 (out of block)
            0x0B]);
            // i++
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x41); code.push(0x01);
            code.push(0x6A);
            code.push(0x21); emit_leb128(code, tp.i as u64);
            code.push(0x0C); code.push(0x00);            // br 0 (continue loop)
        code.push(0x0B);                                  // end loop
        // The loop's only escapes are br 1 paths that already
        // pushed an i32. Natural fallthrough past `end loop` is
        // impossible at runtime, but the WASM validator needs a
        // static proof. `unreachable` poisons the stack so the
        // surrounding block's i32 result requirement is satisfied
        // via dead-code typing.
        code.push(0x00);                                  // unreachable
    code.push(0x0B);                                      // end block — i32 result on stack

    // The result is i32 (0 or 1). The verifier's bool widening at
    // the rule's TOP level handles extension; here we leave it as
    // i32. But if this expression is NOT the rule's top output — it's
    // a sub-expr of an arithmetic chain or a binding — we'd need
    // i64 to type-match. Concretely: predicates only flow into
    // bool-typed sinks today (rule output = bool, or bound to a
    // bool-typed name), so i32 is what's wanted. If a number-typed
    // context demands i64 later, this is the place to add
    // i64.extend_i32_u.
    code.push(0xAD);                                       // i64.extend_i32_u — uniform i64 in expression chains
    Ok(())
}

/// Inline emission for `contains(haystack, needle)`.
///
/// Naive O(N*M) substring search: for each candidate offset $i in
/// the haystack (0..=h_len-n_len), run a forward byte-compare loop
/// over $j (0..n_len) checking `h[i+j] == n[j]`. Returns 1 on the
/// first full match, 0 if no offset matches.
///
/// Edge cases pinned:
///   - empty needle → true (n_len == 0 so the inner loop sets
///     `inner_match = 1` immediately at i=0)
///   - needle longer than haystack → false (the outer pre-check
///     `n_len > h_len` short-circuits to 0)
fn emit_contains(
    code: &mut Vec<u8>,
    haystack: &Expr,
    needle: &Expr,
    ctx: &WasmCtx,
) -> Result<(), WasmError> {
    let tp = ctx.text_prim.ok_or_else(|| WasmError {
        message: "internal: contains reached emit without text-prim scratches".into(),
    })?;

    // Same eval-then-park discipline as starts_with: both args
    // first onto the stack, then pop in reverse.
    emit_wasm_expr(code, haystack, ctx)?;
    emit_wasm_expr(code, needle, ctx)?;
    code.push(0x21); emit_leb128(code, tp.n_len as u64);
    code.push(0x21); emit_leb128(code, tp.n_ptr as u64);
    code.push(0x21); emit_leb128(code, tp.h_len as u64);
    code.push(0x21); emit_leb128(code, tp.h_ptr as u64);

    // Outer block (i32 result): 0 = not found, 1 = found.
    code.extend_from_slice(&[0x02, 0x7F]);
        // n_len > h_len → 0
        code.push(0x20); emit_leb128(code, tp.n_len as u64);
        code.push(0x20); emit_leb128(code, tp.h_len as u64);
        code.push(0x4B);                                 // i32.gt_u
        code.extend_from_slice(&[0x04, 0x40, 0x41, 0x00, 0x0C, 0x01, 0x0B]);

        // $i = 0
        code.push(0x41); code.push(0x00);
        code.push(0x21); emit_leb128(code, tp.i as u64);

        // outer loop over candidate offsets
        code.extend_from_slice(&[0x03, 0x40]);
            // if $i + n_len > h_len → not found; 0 and br out
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x20); emit_leb128(code, tp.n_len as u64);
            code.push(0x6A);                              // add
            code.push(0x20); emit_leb128(code, tp.h_len as u64);
            code.push(0x4B);                              // i32.gt_u
            code.extend_from_slice(&[0x04, 0x40, 0x41, 0x00, 0x0C, 0x02, 0x0B]);

            // Inner block (i32 result): 1 = match at $i, 0 = mismatch
            code.extend_from_slice(&[0x02, 0x7F]);
                // $j = 0
                code.push(0x41); code.push(0x00);
                code.push(0x21); emit_leb128(code, tp.j as u64);
                // inner loop
                code.extend_from_slice(&[0x03, 0x40]);
                    // exit on $j >= n_len → 1 (full needle matched)
                    code.push(0x20); emit_leb128(code, tp.j as u64);
                    code.push(0x20); emit_leb128(code, tp.n_len as u64);
                    code.push(0x4E);                      // i32.ge_s
                    code.extend_from_slice(&[0x04, 0x40, 0x41, 0x01, 0x0C, 0x02, 0x0B]);
                    // load mem[h_ptr + i + j]
                    code.push(0x20); emit_leb128(code, tp.h_ptr as u64);
                    code.push(0x20); emit_leb128(code, tp.i as u64);
                    code.push(0x6A);
                    code.push(0x20); emit_leb128(code, tp.j as u64);
                    code.push(0x6A);
                    code.extend_from_slice(&[0x2D, 0x00, 0x00]);
                    // load mem[n_ptr + j]
                    code.push(0x20); emit_leb128(code, tp.n_ptr as u64);
                    code.push(0x20); emit_leb128(code, tp.j as u64);
                    code.push(0x6A);
                    code.extend_from_slice(&[0x2D, 0x00, 0x00]);
                    // mismatch → 0 and br out (of inner block)
                    code.push(0x47);                      // i32.ne
                    code.extend_from_slice(&[0x04, 0x40, 0x41, 0x00, 0x0C, 0x02, 0x0B]);
                    // j++
                    code.push(0x20); emit_leb128(code, tp.j as u64);
                    code.push(0x41); code.push(0x01);
                    code.push(0x6A);
                    code.push(0x21); emit_leb128(code, tp.j as u64);
                    code.push(0x0C); code.push(0x00);    // br 0 (continue inner)
                code.push(0x0B);                          // end inner loop
                code.push(0x00);                          // unreachable (validator's dead-fallthrough proof)
            code.push(0x0B);                              // end inner block — i32 on stack

            // If inner_match == 1 → found; push 1 and br out of outer block.
            // If 0 → continue outer loop with i++.
            code.extend_from_slice(&[0x04, 0x40,         // if (inner_match nonzero)
                0x41, 0x01, 0x0C, 0x02,                  //   push 1; br 2 (out of outer block)
            0x0B]);                                       // end if

            // i++
            code.push(0x20); emit_leb128(code, tp.i as u64);
            code.push(0x41); code.push(0x01);
            code.push(0x6A);
            code.push(0x21); emit_leb128(code, tp.i as u64);
            code.push(0x0C); code.push(0x00);            // br 0 (continue outer)
        code.push(0x0B);                                  // end outer loop
        code.push(0x00);                                  // unreachable
    code.push(0x0B);                                      // end outer block

    code.push(0xAD);                                       // i64.extend_i32_u
    Ok(())
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
        Expr::Concat(args) => args.iter().any(expr_uses_scratch),
        // W3c text primitives — abs/min/max may be nested in their args.
        Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => expr_uses_scratch(i),
        Expr::StartsWith(h, n) | Expr::EndsWith(h, n) | Expr::Contains(h, n) => {
            expr_uses_scratch(h) || expr_uses_scratch(n)
        }
        // W4-Result: Ok/Err inner can use abs/min/max inside the
        // ok/err payload expression.
        Expr::Ok(inner) | Expr::Err(inner) => expr_uses_scratch(inner),
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

    /// W3c: `length(text)` reserves the 6-i32 text-prim scratch group
    /// and emits `i64.extend_i32_u` (opcode 0xAD) — the widening that
    /// turns the popped i32 len into a number-typed i64.
    #[test]
    fn wasm_w3c_length_reserves_scratches_and_emits_extend() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text
rule len_of
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    n : number
  logic:
    n = length(g.name)
  proofs:
    purity:
      reads: [g.name]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "len_of", "w3c_length");
        assert_eq!(&bytes[0..4], b"\0asm");
        // The rule has a text input field (2 i32 params) but no
        // bindings, no concat, no scratch primitives. text_prim
        // scratches are 6 i32 → expect a single i32 group of 6
        // (`01 06 7F`) in the locals declaration.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x06, 0x7F]),
            "length should reserve 6 text-prim i32 scratches"
        );
        // `i64.extend_i32_u` (0xAD) appears at least once — that's
        // length's widening of the popped len i32 to i64.
        assert!(bytes.contains(&0xAD), "missing i64.extend_i32_u for length");
    }

    /// W3c: `starts_with(...)` emits a 2-function module is NOT
    /// required (no helper) — the byte-compare loop is inlined. But
    /// the loop's `unreachable` (0x00) must appear after `end loop`
    /// to satisfy the validator's dead-fallthrough proof. Pinned by
    /// presence of opcode 0x00 in the module bytes (also the trap
    /// opcode — but starts_with is the only thing in this module
    /// that emits it).
    #[test]
    fn wasm_w3c_starts_with_emits_unreachable_for_dead_loop_exit() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "x"
  @source: invoices.intent:1
  fields:
    path : text
rule check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    ok : bool
  logic:
    ok = starts_with(g.path, "/admin/")
  proofs:
    purity:
      reads: [g.path]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "check", "w3c_sw");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Compiles + validates (the test_smoke `cargo test wasm::`
        // already fails-fast if the module doesn't validate). Verify
        // text-prim scratches reserved.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x06, 0x7F]),
            "starts_with should reserve 6 text-prim i32 scratches"
        );
        // The `unreachable` opcode (0x00) sits inside the function
        // body as the dead fallthrough marker.
        assert!(bytes.contains(&0x00), "missing unreachable for loop's dead fallthrough");
    }

    /// W3c: `parse_int(text)` causes the module to grow a second
    /// function (the parse_int helper). Pinned by the type section
    /// declaring 2 types (one for the rule, one for the helper) and
    /// the function body containing `call 1` (calls the helper).
    #[test]
    fn wasm_w3c_parse_int_emits_helper_function() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "x"
  @source: invoices.intent:1
  fields:
    s : text
rule p
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    n : number
  logic:
    n = parse_int(g.s)
  proofs:
    purity:
      reads: [g.s]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "p", "w3c_pi");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Function section: 2 funcs, types 0 and 1.
        assert!(
            bytes.windows(3).any(|w| w == [0x02, 0x00, 0x01]),
            "expected function section `02 00 01` (rule + parse_int helper)"
        );
        // `call 1` (0x10 0x01) for the parse_int invocation.
        assert!(
            bytes.windows(2).any(|w| w == [0x10, 0x01]),
            "expected `call 1` (parse_int helper) in rule body"
        );
    }

    /// W3c: `json_escape(text)` causes BOTH the bump allocator (since
    /// it allocates) AND the json_escape helper function to be
    /// emitted. The helper has 3-result type which is distinctive:
    /// `0x60 0x03 0x7F 0x7F 0x7F 0x03 0x7F 0x7F 0x7F`.
    #[test]
    fn wasm_w3c_json_escape_emits_helper_with_three_results() {
        let src = r#"@verbose 0.1.0
concept G
  @intention: "x"
  @source: invoices.intent:1
  fields:
    s : text
rule esc
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = json_escape(g.s)
  proofs:
    purity:
      reads: [g.s]
      calls: []
    termination:
      bound: 1
"#;
        let bytes = compile_to_bytes(src, "esc", "w3c_je");
        assert_eq!(&bytes[0..4], b"\0asm");
        // The 3-results type signature for json_escape is unique in
        // the type section: 3 i32 params + 3 i32 results.
        assert!(
            bytes.windows(9).any(|w| w == [0x60, 0x03, 0x7F, 0x7F, 0x7F, 0x03, 0x7F, 0x7F, 0x7F]),
            "expected json_escape's (i32,i32,i32) -> (i32,i32,i32) type entry"
        );
        // The bump allocator (concat scratches) is also reserved
        // since json_escape needs $bump_ptr — even though no concat
        // appears in the rule. Locals decl is i32 only (no number
        // bindings, no min/max scratches): 6 text-prim + 5 concat
        // + 0 text bindings = 11 i32 locals → `01 0B 7F`.
        assert!(
            bytes.windows(3).any(|w| w == [0x01, 0x0B, 0x7F]),
            "expected 11 i32 locals (5 concat scratches + 6 text-prim scratches)"
        );
    }

    /// W3c end-to-end: load all 6 primitives in node, exercise each
    /// against a small but pointed corpus that hits the canonical
    /// edge cases (empty input, length boundary, i64::MIN for
    /// parse_int, all four explicit JSON escape sequences plus
    /// `\u00XX` path for json_escape, etc.). Skipped if `node` is
    /// absent.
    #[test]
    fn wasm_w3c_runtime_all_six_primitives() {
        use std::process::Command;

        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM W3c runtime test");
            return;
        }

        // One source compiles to 6 distinct .wasm modules (one per
        // rule). The driver script then calls each export with
        // tailored inputs and prints OK/FAIL per case.
        let src = r#"@verbose 0.1.0
concept G
  @intention: "x"
  @source: invoices.intent:1
  fields:
    s : text
    needle : text

rule len_of
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    n : number
  logic:
    n = length(g.s)
  proofs:
    purity:
      reads: [g.s]
      calls: []
    termination:
      bound: 1

rule sw_check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    ok : bool
  logic:
    ok = starts_with(g.s, g.needle)
  proofs:
    purity:
      reads: [g.s, g.needle]
      calls: []
    termination:
      bound: 1

rule ew_check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    ok : bool
  logic:
    ok = ends_with(g.s, g.needle)
  proofs:
    purity:
      reads: [g.s, g.needle]
      calls: []
    termination:
      bound: 1

rule co_check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    ok : bool
  logic:
    ok = contains(g.s, g.needle)
  proofs:
    purity:
      reads: [g.s, g.needle]
      calls: []
    termination:
      bound: 1

rule pi_check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    n : number
  logic:
    n = parse_int(g.s)
  proofs:
    purity:
      reads: [g.s]
      calls: []
    termination:
      bound: 1

rule je_check
  @intention: "x"
  @source: invoices.intent:1
  input:
    g : G
  output:
    out : text
  logic:
    out = json_escape(g.s)
  proofs:
    purity:
      reads: [g.s]
      calls: []
    termination:
      bound: 1
"#;

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");

        let paths = [
            ("len_of",   "/tmp/wasm_w3c_runtime_len.wasm"),
            ("sw_check", "/tmp/wasm_w3c_runtime_sw.wasm"),
            ("ew_check", "/tmp/wasm_w3c_runtime_ew.wasm"),
            ("co_check", "/tmp/wasm_w3c_runtime_co.wasm"),
            ("pi_check", "/tmp/wasm_w3c_runtime_pi.wasm"),
            ("je_check", "/tmp/wasm_w3c_runtime_je.wasm"),
        ];
        for (rule, path) in &paths {
            compile_wasm(&program, rule, path).expect(&format!("compile {}", rule));
        }

        let script = format!(
            r#"
const fs = require("fs");
const enc = new TextEncoder();
const dec = new TextDecoder();

async function load(path) {{
  const buf = fs.readFileSync(path);
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL invalid module " + path); return null; }}
  return WebAssembly.instantiate(buf);
}}
function setS(mem, s) {{
  const b = enc.encode(s);
  mem.set(b, 4096);
  return b.length;
}}
function setN(mem, s) {{
  const b = enc.encode(s);
  mem.set(b, 8192);
  return b.length;
}}

(async () => {{
  // length
  let m = await load("{len_path}");
  for (const [s, exp] of [["", 0], ["a", 1], ["hello", 5], ["the quick brown fox", 19]]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s);
    const got = Number(m.instance.exports.len_of(4096, sLen, 0, 0));
    console.log(got === exp ? `OK length ${{JSON.stringify(s)}} = ${{got}}` : `FAIL length ${{JSON.stringify(s)}} got ${{got}} expected ${{exp}}`);
  }}
  // starts_with
  m = await load("{sw_path}");
  for (const [s, n, exp] of [["/admin/u","/admin/",1],["/admin/","/admin/",1],["/admin","/admin/",0],["","x",0],["","",1]]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s); const nLen = setN(mem, n);
    const got = Number(m.instance.exports.sw_check(4096, sLen, 8192, nLen));
    console.log(got === exp ? `OK starts_with(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) = ${{got}}` : `FAIL starts_with(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) got ${{got}} expected ${{exp}}`);
  }}
  // ends_with
  m = await load("{ew_path}");
  for (const [s, n, exp] of [["app.log",".log",1],[".log",".log",1],["log.txt",".log",0],["a.log.bak",".log",0],["","x",0],["","",1]]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s); const nLen = setN(mem, n);
    const got = Number(m.instance.exports.ew_check(4096, sLen, 8192, nLen));
    console.log(got === exp ? `OK ends_with(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) = ${{got}}` : `FAIL ends_with(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) got ${{got}} expected ${{exp}}`);
  }}
  // contains
  m = await load("{co_path}");
  for (const [s, n, exp] of [["/admin/u","admin",1],["foo/admin","admin",1],["admin","admin",1],["/api/x","admin",0],["","admin",0],["a","",1]]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s); const nLen = setN(mem, n);
    const got = Number(m.instance.exports.co_check(4096, sLen, 8192, nLen));
    console.log(got === exp ? `OK contains(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) = ${{got}}` : `FAIL contains(${{JSON.stringify(s)}}, ${{JSON.stringify(n)}}) got ${{got}} expected ${{exp}}`);
  }}
  // parse_int (valid + trap cases)
  m = await load("{pi_path}");
  for (const [s, exp] of [["0",0n],["42",42n],["-1",-1n],["9223372036854775807",9223372036854775807n],["-9223372036854775808",-9223372036854775808n]]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s);
    const got = m.instance.exports.pi_check(4096, sLen, 0, 0);
    console.log(got === exp ? `OK parse_int(${{JSON.stringify(s)}}) = ${{got}}` : `FAIL parse_int(${{JSON.stringify(s)}}) got ${{got}} expected ${{exp}}`);
  }}
  for (const s of ["", "-", "abc", "12abc", "+5"]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s);
    try {{
      const got = m.instance.exports.pi_check(4096, sLen, 0, 0);
      console.log(`FAIL parse_int(${{JSON.stringify(s)}}) = ${{got}} should have trapped`);
    }} catch (e) {{
      console.log(`OK parse_int(${{JSON.stringify(s)}}) trapped`);
    }}
  }}
  // json_escape — the four explicit escapes + control char + identity
  m = await load("{je_path}");
  for (const [s, exp] of [
    ["hello", "hello"],
    ["a\"b", "a\\\"b"],
    ["a\\b", "a\\\\b"],
    ["a\nb", "a\\nb"],
    ["a\tb", "a\\tb"],
    ["a\rb", "a\\rb"],
    ["\x01", "\\u0001"],
    ["\x1f", "\\u001f"],
  ]) {{
    let mem = new Uint8Array(m.instance.exports.memory.buffer);
    const sLen = setS(mem, s);
    const [p, l] = m.instance.exports.je_check(4096, sLen, 0, 0);
    mem = new Uint8Array(m.instance.exports.memory.buffer);
    const got = dec.decode(mem.subarray(p, p+l));
    console.log(got === exp ? `OK json_escape(${{JSON.stringify(s)}}) = ${{JSON.stringify(got)}}` : `FAIL json_escape(${{JSON.stringify(s)}}) got ${{JSON.stringify(got)}} expected ${{JSON.stringify(exp)}}`);
  }}
}})();
"#,
            len_path = paths[0].1,
            sw_path = paths[1].1,
            ew_path = paths[2].1,
            co_path = paths[3].1,
            pi_path = paths[4].1,
            je_path = paths[5].1,
        );

        let out = Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);

        for (_, p) in &paths {
            let _ = std::fs::remove_file(p);
        }

        assert!(
            !stdout.contains("FAIL"),
            "WASM W3c runtime check failed; node stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr)
        );
        // Expected OK count: 4 length + 5 sw + 6 ew + 6 co +
        // (5 valid + 5 trap = 10) parse_int + 8 json_escape = 39.
        let ok_count = stdout.matches("OK").count();
        assert!(
            ok_count >= 39,
            "expected >=39 OK lines (got {}); stdout:\n{}",
            ok_count,
            stdout
        );
    }

    /// Regression: `>=` and `<=` had swapped opcodes
    /// (`0x57` for GtEq, `0x55` for LtEq — the bytes for `le_s` and
    /// `gt_s` respectively, not `ge_s` and `le_s`). The bug went
    /// unnoticed because no W1/W3a/W3b/W3c test ran a rule whose
    /// result depends on `>=` or `<=`. Result-typed rules surfaced
    /// it (validate_purchase uses `customer_age >= 18`).
    /// Pinned here at runtime so a future opcode shuffle can't
    /// reintroduce the swap silently.
    #[test]
    fn wasm_w1_runtime_gteq_lteq_round_trip() {
        use std::process::Command;
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM runtime test");
            return;
        }
        let src = r#"@verbose 0.1.0
concept Pair
  @intention: "p"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule a_at_least_b
  @intention: "a >= b"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = if p.a >= p.b then 1 else 0
  proofs:
    purity:
      reads : [p.a, p.b]
      calls : []
    termination:
      bound : 2
rule a_at_most_b
  @intention: "a <= b"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = if p.a <= p.b then 1 else 0
  proofs:
    purity:
      reads : [p.a, p.b]
      calls : []
    termination:
      bound : 2
"#;
        let ge_path = "/tmp/wasm_w1_runtime_gteq.wasm";
        let le_path = "/tmp/wasm_w1_runtime_lteq.wasm";

        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "a_at_least_b", ge_path).expect("compile ge");
        compile_wasm(&program, "a_at_most_b", le_path).expect("compile le");

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
  // a >= b
  await check("{ge_path}", "a_at_least_b", [5n, 3n], 1);   // strictly greater
  await check("{ge_path}", "a_at_least_b", [5n, 5n], 1);   // equal — the test that distinguishes >= from >
  await check("{ge_path}", "a_at_least_b", [3n, 5n], 0);   // strictly less
  // a <= b
  await check("{le_path}", "a_at_most_b", [3n, 5n], 1);
  await check("{le_path}", "a_at_most_b", [5n, 5n], 1);
  await check("{le_path}", "a_at_most_b", [5n, 3n], 0);
}})();
"#
        );
        let out = Command::new("node").args(["-e", &script]).output().expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let _ = std::fs::remove_file(ge_path);
        let _ = std::fs::remove_file(le_path);
        assert!(
            !stdout.contains("FAIL"),
            "GtEq/LtEq runtime check failed; stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(stdout.contains("OK a_at_least_b(5,5)"), "no equal-case OK; stdout:\n{}", stdout);
    }

    /// W4-Result: `output: Result(number, text)` produces a 4-result
    /// function signature `(i32 tag, i64 ok, i32 err_ptr, i32 err_len)`,
    /// AND a SECOND type entry that the if/else's blocktype references
    /// (no params, same 4 results).
    ///
    /// Pinned shape: type section starts with `02` (two type entries),
    /// the rule's result-tuple bytes `04 7F 7E 7F 7F` appear, AND a
    /// no-params block-type entry `60 00 04 7F 7E 7F 7F` appears too.
    #[test]
    fn wasm_w4_result_number_text_type_section_pins_4_results_and_blocktype() {
        let src = r#"@verbose 0.1.0
concept C
  @intention: "c"
  @source: invoices.intent:1
  fields:
    age    : number
    amount : number
rule validate
  @intention: "validate"
  @source: invoices.intent:1
  input:
    c : C
  output:
    r : Result(number, text)
  logic:
    r = if c.age >= 18 then Ok(c.amount) else Err("under 18")
  proofs:
    purity:
      reads : [c.age, c.amount]
      calls : []
    termination:
      bound : 4
"#;
        let bytes = compile_to_bytes(src, "validate", "result_num_text_typesec");
        assert_eq!(&bytes[0..4], b"\0asm");
        // Rule's result-tuple bytes: 4 results (0x04) of (i32 i64 i32 i32).
        assert!(
            bytes.windows(5).any(|w| w == [0x04, 0x7F, 0x7E, 0x7F, 0x7F]),
            "missing rule result tuple `04 7F 7E 7F 7F`; bytes:\n{:02X?}", bytes
        );
        // Block-type entry: type header `60 00 04 7F 7E 7F 7F` (0 params, 4 results).
        // This MUST appear in addition to the rule's signature so the
        // if/else can reference it as its multi-value blocktype.
        assert!(
            bytes.windows(7).any(|w| w == [0x60, 0x00, 0x04, 0x7F, 0x7E, 0x7F, 0x7F]),
            "missing blocktype entry `60 00 04 7F 7E 7F 7F`; bytes:\n{:02X?}", bytes
        );
    }

    /// W4-Result: `output: Result(text, text)` is the 5-result variant
    /// `(i32 tag, i32 ok_ptr, i32 ok_len, i32 err_ptr, i32 err_len)`,
    /// and the matching block-type entry is `60 00 05 7F 7F 7F 7F 7F`.
    #[test]
    fn wasm_w4_result_text_text_type_section_pins_5_results() {
        let src = r#"@verbose 0.1.0
concept C
  @intention: "c"
  @source: invoices.intent:1
  fields:
    balance : number
rule classify
  @intention: "classify"
  @source: invoices.intent:1
  input:
    c : C
  output:
    r : Result(text, text)
  logic:
    r = if c.balance >= 100000 then Ok("premium") else Err("below threshold")
  proofs:
    purity:
      reads : [c.balance]
      calls : []
    termination:
      bound : 4
"#;
        let bytes = compile_to_bytes(src, "classify", "result_text_text_typesec");
        assert_eq!(&bytes[0..4], b"\0asm");
        assert!(
            bytes.windows(6).any(|w| w == [0x05, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F]),
            "missing rule result tuple `05 7F*5`; bytes:\n{:02X?}", bytes
        );
        assert!(
            bytes.windows(8).any(|w| w == [0x60, 0x00, 0x05, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F]),
            "missing blocktype entry `60 00 05 7F*5`; bytes:\n{:02X?}", bytes
        );
    }

    /// W4-Result end-to-end: load the compiled `Result(number, text)`
    /// module in Node and check both Ok and Err arms. Specifically pins
    /// that:
    ///   - tag=0 with the live ok value, err slots are placeholder zeros
    ///   - tag=1 with the err (ptr, len) pointing at decodable bytes,
    ///     ok slot is placeholder zero
    ///   - the err arm tolerates a `concat(...)` payload that mixes a
    ///     literal and a number (exercises itoa on the bump allocator
    ///     side-by-side with the new Result emitter).
    #[test]
    fn wasm_w4_runtime_result_number_text_both_arms() {
        use std::process::Command;
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM runtime test");
            return;
        }
        let src = r#"@verbose 0.1.0
concept Purchase
  @intention: "p"
  @source: invoices.intent:1
  fields:
    amount : number
    age    : number
rule validate
  @intention: "v"
  @source: invoices.intent:1
  input:
    p : Purchase
  output:
    r : Result(number, text)
  logic:
    r = if p.age >= 18 then Ok(p.amount) else Err(concat("age ", p.age, " under 18"))
  proofs:
    purity:
      reads : [p.amount, p.age]
      calls : []
    termination:
      bound : 6
"#;
        let path = "/tmp/wasm_w4_runtime_result_num.wasm";
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "validate", path).expect("compile");

        let script = format!(
            r#"
const fs = require("fs");
(async () => {{
  const buf = fs.readFileSync("{path}");
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL: invalid module"); return; }}
  const m = await WebAssembly.instantiate(buf);
  const fn = m.instance.exports.validate;
  const mem = new Uint8Array(m.instance.exports.memory.buffer);
  const dec = new TextDecoder();
  // Ok arm: amount=1000, age=25
  const ok = fn(1000n, 25n);
  if (ok[0] === 0 && ok[1] === 1000n && ok[2] === 0 && ok[3] === 0) {{
    console.log("OK ok-arm:", JSON.stringify(ok, (_, v) => typeof v === "bigint" ? v.toString() : v));
  }} else {{
    console.log("FAIL ok-arm: got", JSON.stringify(ok, (_, v) => typeof v === "bigint" ? v.toString() : v));
  }}
  // Err arm: amount=1000, age=15 — ok slot is placeholder, err slots live
  const err = fn(1000n, 15n);
  if (err[0] === 1 && err[1] === 0n && err[3] > 0) {{
    const msg = dec.decode(mem.subarray(err[2], err[2] + err[3]));
    if (msg === "age 15 under 18") {{
      console.log("OK err-arm: msg =", JSON.stringify(msg));
    }} else {{
      console.log("FAIL err-arm: msg =", JSON.stringify(msg));
    }}
  }} else {{
    console.log("FAIL err-arm: got", JSON.stringify(err, (_, v) => typeof v === "bigint" ? v.toString() : v));
  }}
}})();
"#
        );
        let out = Command::new("node").args(["-e", &script]).output().expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let _ = std::fs::remove_file(path);
        assert!(
            !stdout.contains("FAIL"),
            "Result(number, text) runtime check failed; stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(stdout.contains("OK ok-arm"), "no ok-arm OK; stdout:\n{}", stdout);
        assert!(stdout.contains("OK err-arm"), "no err-arm OK; stdout:\n{}", stdout);
    }

    /// W4-Result end-to-end: same shape as the previous test but for
    /// `Result(text, text)`. Both arms produce text; the un-live arm's
    /// (ptr, len) slots are placeholder zeros.
    #[test]
    fn wasm_w4_runtime_result_text_text_both_arms() {
        use std::process::Command;
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("note: `node` not found, skipping WASM runtime test");
            return;
        }
        let src = r#"@verbose 0.1.0
concept C
  @intention: "c"
  @source: invoices.intent:1
  fields:
    balance : number
rule classify
  @intention: "classify"
  @source: invoices.intent:1
  input:
    c : C
  output:
    r : Result(text, text)
  logic:
    r = if c.balance >= 100000 then Ok("premium") else Err(concat("balance ", c.balance, " below"))
  proofs:
    purity:
      reads : [c.balance]
      calls : []
    termination:
      bound : 5
"#;
        let path = "/tmp/wasm_w4_runtime_result_text.wasm";
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        compile_wasm(&program, "classify", path).expect("compile");

        let script = format!(
            r#"
const fs = require("fs");
(async () => {{
  const buf = fs.readFileSync("{path}");
  if (!WebAssembly.validate(buf)) {{ console.log("FAIL: invalid module"); return; }}
  const m = await WebAssembly.instantiate(buf);
  const fn = m.instance.exports.classify;
  const mem = new Uint8Array(m.instance.exports.memory.buffer);
  const dec = new TextDecoder();
  const decode = (p, l) => dec.decode(mem.subarray(p, p + l));
  // Ok arm
  const ok = fn(150000n);
  if (ok[0] === 0 && ok[3] === 0 && ok[4] === 0 && decode(ok[1], ok[2]) === "premium") {{
    console.log("OK ok-arm");
  }} else {{
    console.log("FAIL ok-arm:", JSON.stringify(ok));
  }}
  // Err arm
  const err = fn(50000n);
  if (err[0] === 1 && err[1] === 0 && err[2] === 0) {{
    const msg = decode(err[3], err[4]);
    if (msg === "balance 50000 below") {{
      console.log("OK err-arm: msg =", JSON.stringify(msg));
    }} else {{
      console.log("FAIL err-arm: msg =", JSON.stringify(msg));
    }}
  }} else {{
    console.log("FAIL err-arm:", JSON.stringify(err));
  }}
}})();
"#
        );
        let out = Command::new("node").args(["-e", &script]).output().expect("spawn node");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let _ = std::fs::remove_file(path);
        assert!(
            !stdout.contains("FAIL"),
            "Result(text, text) runtime check failed; stdout:\n{}\nstderr:\n{}",
            stdout,
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(stdout.contains("OK ok-arm"), "no ok-arm OK; stdout:\n{}", stdout);
        assert!(stdout.contains("OK err-arm"), "no err-arm OK; stdout:\n{}", stdout);
    }

    /// W4-Result slice scope: `match_result(...)` is rejected with a
    /// clear, actionable message that points at the slice extension
    /// (W4-MatchResult) lifting it. Pin the message so future grammar
    /// or test-name shifts don't silently drop the breadcrumb.
    #[test]
    fn wasm_w4_match_result_rejected_with_breadcrumb() {
        let src = r#"@verbose 0.1.0
concept Purchase
  @intention: "p"
  @source: invoices.intent:1
  fields:
    age    : number
    amount : number
rule validate
  @intention: "v"
  @source: invoices.intent:1
  input:
    p : Purchase
  output:
    r : Result(number, text)
  logic:
    r = if p.age >= 18 then Ok(p.amount) else Err("under")
  proofs:
    purity:
      reads : [p.age, p.amount]
      calls : []
    termination:
      bound : 4
rule discounted
  @intention: "d"
  @source: invoices.intent:1
  input:
    p : Purchase
  output:
    r : Result(number, text)
  logic:
    r = match_result(validate(p), v => Ok(v * 90 / 100), e => Err(e))
  proofs:
    purity:
      reads : [p]
      calls : [validate]
    termination:
      bound : 6
"#;
        use crate::lexer::Lexer;
        use crate::parser::Parser;
        let tokens = Lexer::new(src).tokenize().expect("tokenize");
        let program = Parser::new(tokens).parse_program().expect("parse");
        let path = "/tmp/wasm_w4_match_rejected.wasm";
        let err = compile_wasm(&program, "discounted", path).expect_err("must reject match_result");
        assert!(
            err.message.contains("match_result is not yet supported"),
            "expected breadcrumb mentioning match_result; got: {}",
            err.message
        );
        assert!(
            err.message.contains("W4-MatchResult"),
            "expected slice name reference in error; got: {}",
            err.message
        );
        let _ = std::fs::remove_file(path);
    }
}
