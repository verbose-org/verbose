# Native Backend — Locked Designs & Architecture

This file contains the locked-before-implementation design documents for each
native backend phase, plus the architectural rationale for continuation-passing
emission and stream semantics. These are historical reference — they're frozen
and don't change after implementation.

For the live phase table, register conventions, and rejection list, see
[CLAUDE.md](../CLAUDE.md) → "Native Backend Evolution".

---

### Phase 3 design (locked before implementation)

Four decisions locked before writing any `emit_collection_program` code, so the implementation has no surprises and philosophical drift is caught at this level, not in commit messages.

**1. Memory model: no arena.** Streaming emission means each element produced by `map` / `filter` is evaluated, serialised to stdout, and discarded before the next iteration. No element ever persists across iterations. No bump allocator, no heap, no `mmap`. Same syscall surface as Phase 2 — only `read argv`, `write fd`, `exit`. This is why `fold` / `sum` / `count` / `min` / `max` on the output side are **not** part of Phase 3: they need an accumulator, which is arena territory; when we get there, the arena decision gets its own design pass.

**2. Output format: JSON Lines, NOT a wrapping JSON array.** One JSON object (or scalar) per line, same convention as Phase 2C record output. Rationale:
- Fewer syscalls per collection — no bracket/comma state machine, so no `[`, no `,`, no `]` static writes. A 1000-element collection is ~1000 syscalls in JSON Lines vs ~2000 with array wrapping.
- Smaller binary — no bracket logic code.
- Uniform format across Phase 2C and Phase 3 native outputs — one object per line, regardless of whether the rule produces one record or a collection of records. Downstream tooling (`jq -r`, `awk`, `grep`) handles both identically.
- If a caller needs an array, `./bin args | jq -s` wraps at ~zero cost. Conversely, producing JSON Lines from a binary that emits arrays is harder. Pick the simpler side.

Cost: per-input-record grouping is lost when the binary processes multiple input records. Acceptable — single-input invocations are the common case, and multi-input can be reconstructed by running the binary separately.

**3. argv format: `<N> <element × N>` count-prefixed, trailing any scalar fields of the input concept.**
```
concept Workforce { employees: collection(Employee{name, salary}) }
./bin <N> <emp1-name> <emp1-salary> ... <empN-name> <empN-salary>

concept BigInput { tag: number, employees: collection(Employee{name, salary}) }
./bin <tag> <N> <emp1-name> <emp1-salary> ... <empN-name> <empN-salary>
```
The count comes right before its elements. No terminator, no dynamic scanning. Predictable at every iteration. If the caller passes the wrong count, the binary reads past the end of argv and exits via the natural argv-bounds-check in the outer loop.

**4. Scope v1 (restrictions rejected with clear messages):**
- Top-level logic must be `map(input.<coll_field>, var => <body>)` or `filter(input.<coll_field>, var => <pred>)`.
- Input concept: zero or more scalar fields + exactly one collection field, which must be the LAST field declared.
- Output: `collection(Record)` or `collection(number | text)`.
- Map/filter body references only the lambda variable, not the outer rule's input (simplifies `input_name` threading).
- No nested collections, no collection-returning rule calls, no `fold`.

**Register convention for collection emitter:** `r15` holds the inner loop counter (number of elements remaining). This role is distinct from its Phase 1A role as the file-descriptor returned by `open()` — the two emitters never coexist in the same binary (a reaction-producing binary and a collection-output binary are different entry points), so the register has a per-emitter role, not a global one. The register table in the "Register conventions" section names this explicitly.

**Why this design holds security pillar #1:** identical attack surface to Phase 2. No new syscalls. No heap. No dynamic parsing. The binary is still a straight machine-code translation of the source: read argv, evaluate declared logic, write declared output, exit. An auditor reading the disassembled binary sees exactly one extra loop compared to Phase 2C, nothing else.

**Filter implementation note — synthesised AST at emit time.** `filter(xs, e => pred)` does not construct a Record in the source; it keeps or drops each element verbatim. The emitter handles this by building a small Vec of synthetic `Field(Ident(<lambda>), <name>)` expressions — one per field of the input element concept — and passing them to the same `emit_record_as_json` the `map` path uses. The Vec exists only during emission, never reaches the verifier, never appears in error messages. The emitted machine code is byte-for-byte identical to what a user would get writing `map(xs, e => Employee { name: e.name, salary: e.salary })` by hand. Zero declaration added or removed at the source contract — pure emitter internals.

**Known perf tradeoff (not yet addressed).** Each element emits N+K syscalls, where N is the field count and K is the number of static JSON skeleton fragments. For a 2-field record that's ~6 syscalls per element. On Linux, each syscall is ~1 µs, so a 1000-element collection takes ~6 ms to emit. At POC scale this is invisible; for 100K+ element collections, a batch-per-element optimisation (stack buffer, single `write` per element) would be ~10-20× faster. The Phase 1B `emit_concat_to_buffer` / `emit_append_file_call` pattern already shows the technique — generalising it to record emission is ~100 lines of plumbing, deferred until someone has an actual case. Documented so it is not forgotten.

### Phase 4 design (locked before implementation)

Phase 4 introduces the first emitter with **cross-iteration state**: a reduction (`fold` / `sum` / `count` / `min` / `max`) over a collection, producing a single scalar number per input record. Four decisions are locked before writing `emit_fold_program`.

**1. Memory model: one stack slot, no arena.** The accumulator lives in a single 8-byte slot at the bottom of the rbp frame (`acc_slot`), reserved unconditionally by the prologue — same discipline as Phase 2D's `match_slot`. The inner loop reads the slot, combines with the current element, writes the slot back. No heap, no growable buffer. State is exactly one i64 wide, for the entire lifetime of one input record, and discarded at outer-loop bottom. Syscall surface stays identical to Phase 3: `read argv`, `write fd 1`, `exit`. Adding "cross-iteration state" at the source level does not add "cross-iteration state" at the syscall level.

**2. Output format: one number per input record on stdout, `\n`-terminated.** Uniform with scalar-output rules (Phase 0). `sum` returns the running total, `count` the number of elements where the predicate held, `min`/`max` the extremum (seeded with `i64::MAX` / `i64::MIN`), generic `fold` whatever the body computes. If the collection is empty, the init value is emitted verbatim — that's the well-defined answer (`sum` of empty = 0, `count` of empty = 0, `min` of empty = i64::MAX, `max` of empty = i64::MIN). The "min/max of empty" edge case is documented behaviour, not a silent bug; if a rule needs "empty is an error", it writes `Result(number, text)` with an explicit Err arm, which is Phase 2A territory, not Phase 4.

**3. Init must be a Number literal at parse time.** All desugarings of `sum` / `count` / `min` / `max` produce literal inits (`0`, `0`, `i64::MAX`, `i64::MIN`), so the restriction is free in practice. A non-literal init would require pre-loop evaluation into acc_slot, which is a (small) separate extension — not in v1. Verified at emit time: if `logic.value` is `Fold(_, init, _, _, _)` and `init` is not `Expr::Number(_)`, the emitter refuses with a clear message ("Phase 4: fold init must be a literal number").

**4. Scope v1 (everything else refused with a clear message):**
- Rule `output_ty` is `Number`.
- Top-level logic is `Fold(Field(Ident(input_name), coll_field_name), Number(init), acc_name, item_name, body)` — exactly the shape sum/count/min/max desugar to.
- Input concept: zero or more scalar fields + exactly one collection field, which must be the LAST field declared (mirrors Phase 3).
- Body is a scalar expression referencing `acc_name`, `item_name.<field>`, and any outer let/field bindings. No nested fold, no rule calls inside the body.
- No side effects in the body (no concat-to-stdout, no append_file, no rule calls that react).

**Frame layout.** Prologue extends Phase 3's layout by one slot:
```
rbp - 8*(nfields+1)             first field slot
...
rbp - 8*(nfields+nlets+1)       last let-binding slot
rbp - 8*(nfields+nlets+2)       acc_slot                       ← Phase 4 addition
```
`acc_name` is treated by the expression emitter as a read/write to `acc_slot` — no distinct register role needed. Accessing `acc_name` from the body goes through the same rbp-slot lookup as a let binding (offset table), keeping the expression emitter uniform.

**Register map: no new reservation.** The inner loop reuses `r15` (inner counter, role inherited from Phase 3) and `r13`/`r14` (argv base, current-record-start), same as Phase 3. No new entry in the register table. The "per-emitter role" policy for `r15` continues: a fold-output binary and a reaction-output binary never coexist, so the fd-vs-counter ambiguity stays resolved.

**Emission flow.**
```
_start:
  prologue (reserve field slots + let slots + acc_slot)
outer_loop_top:
  check r14 (record index) < argc - 1 else exit
  parse scalar input fields (atoi / argv-ptr for text) into field slots
  parse collection count N into r15
  store init literal into acc_slot
  set rcx = argv[ fields_base + 1 ]  (first element pointer)
inner_loop_top:
  test r15, r15; jz after_inner
  parse element fields (atoi / argv-ptr) into item slots (reused across iterations)
  evaluate body with acc=acc_slot, item=item_slots → rax
  store rax back into acc_slot
  advance argv ptr by element_field_count
  dec r15
  jmp inner_loop_top
after_inner:
  load acc_slot into rax
  emit_itoa_inline → stdout
  emit_write_newline(1)
  advance r14 past this record
  jmp outer_loop_top
```

**Why this design holds security pillar #1:** identical attack surface to Phase 3. Same three syscalls. One new stack slot — which is not an attack surface, it's a rbp offset. An auditor reading the disassembled binary sees one extra `mov [rbp-offset], rax` per iteration compared to Phase 3, and one final `itoa + write` at the bottom. Nothing else.

**What Phase 4 does NOT do** (refused with clear messages, deferred to later phases):
- `output: text` with fold producing a text (covered by Phase 5b — different accumulator shape).
- `output: Record` with fields computed by fold (would need multiple acc slots and a final record emission).
- Body containing a nested `fold` (would need acc-slot stack discipline — the outer/inner slots would collide).
- Generic `fold(coll, init_expr, ...)` with a non-literal init (pre-loop evaluation needed).
- `fold` whose target is a rule call returning a collection (Phase 3 already refuses collection-returning rule calls as map/filter targets; Phase 4 inherits the refusal).

### Phase 5b design (locked before implementation)

Phase 5b handles **text-valued folds**: `output: text` with a top-level `fold` that appends to a text accumulator over a collection. It's the first emitter where the accumulator SIZE grows across iterations — Phase 4's accumulator is one i64, Phase 5b's is a contiguous byte buffer whose final length is determined by the element data.

**Core design decision: append-only body shape.** The fold body must be `concat(acc, ...rest)` where `acc` is the FIRST argument and does NOT appear anywhere in `...rest` (nor recursively inside nested expressions). This is a structural invariant the emitter checks at compile time — not a style preference.

Why this restriction is load-bearing, not arbitrary:
- "Append to accumulator" is the semantic of text-fold every realistic use case wants (CSV rows, joined lists, report lines, command builders). A body like `concat(e.name, acc)` or `concat(X, acc, Y)` would mean "rebuild acc each iteration with prefix/interleave" — that's O(N²) memory regardless of strategy, and we'd be paying cost for a shape no one actually needs.
- With `acc` pinned to position 0, per-iteration byte growth = sum of sizes of `...rest` args, none of which reference the accumulator. That makes sizing statically decomposable into "per-element static bytes + per-element runtime-strlen bytes" — computable in one forward pass over the collection.
- The restriction is easy to explain ("append-only"), easy to relax later if needed (add more shapes as distinct cases), and survives cleanly if a future `text [..N]` declaration lands — the append-only shape is still valid, it just becomes faster to size.

**Sizing strategy: two passes over the collection.** The first pass walks argv to compute the total buffer size into rax (static per-element size + runtime strlen of each text-field arg in `...rest`, summed N times). After sizing, `mov r9, rsp; sub rsp, rax` reserves the buffer dynamically (same `r9`-saves-rsp trick as the Phase 1B text-field concat). The second pass walks argv again to actually fill the buffer. One `write(1, buffer, length)` + newline at the end, then `mov rsp, r9` to free.

Two passes is fine: the first pass is pure size arithmetic (strlen + add, no data movement), the second pass is the actual fill. No iteration of user logic is duplicated — the sizing pass doesn't evaluate the fold body, it only asks "how many bytes does each arg contribute?" which is a per-arg-kind lookup plus one strlen call per text field.

**Frame layout.** Phase 3's prologue extended by TWO slots:
```
rbp - 8*(nfields + 1)                 first scalar field
...
rbp - 8*(nfields + nlets + 1)         last let-binding slot
rbp - 8*(nfields + nlets + 2)         count_slot            (N, stored after parsing)  ← Phase 5b addition
rbp - 8*(nfields + nlets + 3)         argv_save_slot        (r14 at first element)     ← Phase 5b addition
```
Both slots are written at the top of the outer-loop body, before the sizing pass. They're read at the start of the fill pass to rewind r14 and r15 to their pre-pass-1 values. After the fill pass the slots are dead until the next outer iteration.

**Register map: no new reservation.**
- `r14` — argv index (walked forward twice, rewound from `argv_save_slot` between passes)
- `r15` — inner counter per pass (reloaded from `count_slot` for pass 2)
- `r10` — buffer base (for the final `write` syscall source)
- `r9`  — saved pre-allocation rsp (for `mov rsp, r9` cleanup)
- `rbx` — buffer write pointer during fill
- `r8`, `rax`, `rcx`, `rdx`, `rsi`, `rdi`, `r11` — ephemeral

No fresh register role, no collision with existing emitters.

**Emission flow.**
```
outer_loop_top:
  check r14 < argc - 1 else exit
  parse scalar input fields into rbp slots
  parse count N → r15 ; store r15 at count_slot ; inc r14
  store r14 at argv_save_slot                    ; remember first-element index

  ; --- pass 1: size ---
  mov rax, static_init_size                      ; init literal's length
size_loop_top:
  test r15, r15 ; jz size_done
  for each text-field arg in body's ...rest:
    mov rsi, [rbp + <field_offset>]              ; field offset relative to element start
    ; wait — we need to point rsi at argv[r14 + field_index]
    mov rsi, [r13 + r14*8 + field_index*8]
    push rax ; emit_strlen ; pop rcx ; add rcx, rdx ; mov rax, rcx
  add rax, static_per_element                    ; literals + 21 × numbers
  advance r14 by n_elem_fields
  dec r15
  jmp size_loop_top
size_done:
  add rax, 7 ; and rax, ~7                       ; round up to 8

  ; --- buffer allocation ---
  mov r9, rsp
  sub rsp, rax
  mov rbx, rsp                                   ; rbx = write ptr
  mov r10, rbx                                   ; r10 = buffer base

  ; --- copy init literal ---
  inline jmp-over-data ; lea rsi, [rip + init] ; mov rcx, init_size ; rep movsb ; mov rbx, rdi

  ; --- rewind r14 and r15 for pass 2 ---
  mov r14, [rbp + argv_save_slot]
  mov r15, [rbp + count_slot]

  ; --- pass 2: fill ---
fill_loop_top:
  test r15, r15 ; jz fill_done
  parse element fields into rbp element slots    ; same dispatch as Phase 3 (Number via atoi, Text stores ptr)
  for each arg in body's ...rest:
    evaluate/copy per kind (literal inline, number via itoa_to_buffer, text-field via strlen + rep movsb)
  advance r14 by n_elem_fields (already done by the per-field "inc r14" in the parse step)
  dec r15
  jmp fill_loop_top
fill_done:

  ; --- write(1, r10, rbx - r10) + newline ---
  mov rsi, r10 ; mov rax, 1 ; mov rdi, 1 ; mov rdx, rbx ; sub rdx, r10 ; syscall
  emit_write_newline(1)

  ; --- free ---
  mov rsp, r9

  jmp outer_loop_top
```

**Why this design holds security pillar #1:** identical syscall surface to Phase 4. Same three: read argv, write fd, exit. No heap. One additional `write` per input record (plus the newline), but that's a static property of the rule shape, not something that varies with input. Two new rbp slots (not an attack surface — just rbp offsets). Buffer size is a function of declared element data, not of arbitrary user input outside the concept's declared range.

**Scope v1 (everything else refused with a clear message):**
- `rule.output_ty == Type::Text`
- Top-level logic: `Fold(Field(Ident(input), coll_field), Text(init_literal), acc, item, Concat(args))`
- `args[0]` is `Expr::Ident(acc)`; `acc` does not appear anywhere in `args[1..]` (or nested within).
- `init` is a `Text` literal (analogous to Phase 4's Number-literal init).
- Input concept: scalars* + ONE trailing collection(Concept) field (same shape as Phase 3 / 4).
- Element concept: number/text fields only.
- Each arg in `args[1..]` classifies as either text literal, number expression, or text field (via the same `classify_concat_arg` Phase 1B uses).

**What Phase 5b does NOT do** (refused with clear messages, deferred):
- Non-append body shapes (`concat(X, acc)`, `concat(X, acc, Y)`, `if ... then concat(acc, ...)  else concat(acc, ...)`). Workaround: reorganize into append-only, use a trailing-separator pattern with an initial-empty init.
- Nested `fold` inside the body.
- `fold` target that is a rule call returning a collection.
- Bodies that are not a top-level `concat` (e.g. just `Ident(acc)` — a no-op fold producing the init unchanged; the verifier should probably reject this anyway).

### Phase 2F design (locked before implementation)

Phase 2F extends `match_result` to accept a non-pass-through Err arm. Until now, Phase 2D required the outer Err to be exactly `Err(Ident(err_var))` — the inlined callee's Err leaves wrote their text directly to stderr, no binding happened. Phase 2F lets the outer Err **transform** the callee's Err value:

```verbose
match_result(
  validate(p),
  v => Ok(v * 2),
  e => Err(concat("[", p.id, "] validation failed: ", e))
)
```

**Core design decision: two rbp slots represent the bound err_var as a (ptr, len) pair.** Any text value at runtime is two machine words — a pointer and a length. Unifying all text values under this representation avoids NUL-termination gymnastics (concat outputs aren't NUL-terminated; argv fields are; literals know their length at emit). The inlined callee's Err leaf captures whatever shape it produces into the slots; the outer arm reads from them.

**Frame layout.** `emit_record_loop_prologue` reserves FOUR slots at the bottom of the frame, unconditionally:
```
rbp - 8*(nfields + nlets + 1)         match_slot            (Ok-bound i64, Phase 2D)
rbp - 8*(nfields + nlets + 2)         err_ptr_slot          (Err-bound text ptr)          ← Phase 2F
rbp - 8*(nfields + nlets + 3)         err_len_slot          (Err-bound text length)       ← Phase 2F
rbp - 8*(nfields + nlets + 4)         err_frame_save_slot   (rsp before callee Err concat) ← Phase 2F
```
Uniform layout: 24 extra bytes per rule that uses the shared prologue, even when match_result isn't used. Acceptable noise in exchange for one emitter shape, no conditional layout.

**Binding capture per callee Err leaf shape:**
- `Err(Text(literal))`: `lea rax, [rip + literal_addr]; mov [rbp+err_ptr_slot], rax; mov qword [rbp+err_len_slot], literal_len`. No buffer.
- `Err(Field(input, text_field))`: `mov rax, [rbp+field_slot]; mov [rbp+err_ptr_slot], rax`. Length: `emit_strlen` (argv strings are NUL-terminated) → `mov [rbp+err_len_slot], rdx`. No buffer.
- `Err(Concat(args))`: call `emit_concat_to_buffer` (may be static or dynamic). On return rax=ptr, rdx=len. Store both. **The concat buffer stays on the stack until the whole match arm finishes.** Save the current rsp into `err_frame_save_slot` so the outer arm's final cleanup can free the callee's buffer (`mov rsp, [rbp+err_frame_save_slot]`).

Before emission, always `mov [rbp+err_frame_save_slot], rsp` — harmless when no concat happens (save equals current), necessary when it does. Keeps emission uniform.

**Outer Err arm.** A new `TextBindings = HashMap<&str, (ptr_slot, len_slot)>` is threaded to `emit_text_write_to_fd` / `emit_concat_to_buffer` / `classify_concat_arg` / `emit_concat_fill`. When any of these sees `Expr::Ident(name)` and `name` is in the bindings, the identifier resolves to `(ptr, len)` loaded from the two slots rather than failing. A new `ConcatArgKind::BoundText` variant covers this in concat sizing (`add rax, [rbp+len_slot]`) and filling (`mov rsi, [rbp+ptr_slot]; mov rcx, [rbp+len_slot]; mov rdi, rbx; rep movsb; mov rbx, rdi` — no strlen needed at fill time, length is already stored).

**Cleanup sequence at the end of the outer Err arm:**
1. Outer Err's own concat buffer (if any) is freed via the existing `emit_concat_buffer_free` (`mov rsp, r9` for dynamic, `add rsp, imm32` for static).
2. `mov rax, [rbp+err_frame_save_slot]; mov rsp, rax` — frees the callee's Err concat buffer if one was allocated; no-op otherwise.
3. `jmp loop_top`.

**Why this design holds security pillar #1:** same syscall surface as Phase 2D (`read argv`, `write fd 1/2`, `exit`). Three new rbp slots — not attack surface, just offsets. No new registers reserved. Buffer lifetimes are stack-only, bounded by the outer match arm. The pass-through case from Phase 2D is a pure subset and continues to work; we can refactor it to use the new slot-based path or keep the fast path — decision at implementation time.

**Scope v1 (everything else refused with a clear message):**
- Outer Err arm body: `Err(Text(lit))`, `Err(Field(input, text_field))`, `Err(Ident(err_var))`, or `Err(Concat(args))` where args classify as text literal / number expression / input text field / `Ident(err_var)`.
- Callee Err leaves: same four shapes as outer.
- `err_var` only usable in text contexts (its value is text; using it in a number context is rejected by the verifier already).
- Pass-through (`Err(Ident(err_var))`) still works — routes through the slot path OR a fast path; implementation choice.

**What Phase 2F does NOT do** (deferred):
- Using `err_var` twice in the same outer arm — the slots are single-assignment, reading twice is fine but the design doesn't support "aliasing" where err_var appears in a sub-expression already using it indirectly.
- Binding to anything other than a text var (Ok-bound numbers are already handled by `match_slot`; non-text non-number bindings aren't in the language).

### Phase 2G design (locked before implementation)

Phase 2G adds a single new arm to `emit_text_write_to_fd` that inlines a text-returning rule call. It unlocks every site that currently rejects `Expr::Call` in a text context: `output: text` with `body = Call`, Record field value = Call, `match_result` outer Err body = Call (direct or inside concat-arg-wise via BoundText-of-Call — but Call in concat args stays deferred for now), `Result(text, text)` arms = Call.

**Core mechanism: inline-substitute.** When `emit_text_write_to_fd` sees `Expr::Call(callee_name, [Ident(caller_input)])`, it looks up the callee rule, validates the restrictions, and recurses on `callee.logic.value` using `callee.input_name`. Since the restrictions ensure the callee's input concept, input name, and offsets match the caller's, the recursion emits exactly the same bytes as if the callee's body had been written inline at the call site. No new calling convention, no new slot, no new register reserved.

**Why this is honest inlining, not a hack.** The callee has no let bindings (restricted below), so there is nothing to evaluate that the caller's prologue didn't already set up. Field accesses in the callee body resolve through the caller's `offsets` map because the concepts and input names match. The callee's text-producing shapes (literal / field / concat) are exactly what `emit_text_write_to_fd` already handles for the caller's own body.

**Scope v1 (refused with clear messages otherwise):**
- Callee's `output_ty == Type::Text`.
- Callee's input concept == caller's (mirror of Phase 2D's same-concept restriction — reuses caller's argv parsing and rbp slots).
- Callee's `input_name == caller's input_name` (so `Ident(caller_input)` resolves correctly inside the callee body).
- Callee's `logic.bindings.is_empty()` — the prologue already ran and the caller's lets are what's in scope.
- Callee's `logic.value` is a text_expr `emit_text_write_to_fd` handles (literal, field, concat, or recursively another Call satisfying these same restrictions — natural recursion, no depth limit needed).
- Call arg list is exactly `[Ident(caller_input)]`.

**Why this design holds security pillar #1:** zero new syscalls, zero new registers, zero new stack slots. The emitted machine code is byte-for-byte what the user could have written by hand as `concat(...)` directly. Inlining happens only at emission; the callee rule itself is still verified independently by the verifier against its own proofs (purity, termination, determinism) — the caller inherits those properties through the `calls: [callee_name]` declaration.

**What Phase 2G does NOT do** (deferred):
- Cross-concept callees (mirror of the 2D restriction — needs real argument passing).
- Callees with let bindings (would need to evaluate them at the call site, adding frame-layout complexity).
- Rule calls inside `concat(...)` arguments (would need `ConcatArgKind::CallText` with runtime sizing — feasible but its own small phase).
- Rule calls in `append_file` content (reaction emitter currently only dispatches Text / Concat).

### Phase 2H-b design (locked before implementation)

Phase 2H-b lets a text-returning rule call appear as an argument to `concat(...)`. Today `classify_concat_arg` rejects `Expr::Call`; the call is first-class as a top-level text expression (Phase 2G) but not as a fragment being concatenated with other text.

**Core design decision: pre-evaluation into an ad-hoc slot array indexed by `r11`.** Each Call arg is evaluated ONCE, its result `(ptr, len)` stored in a 16-byte stack slot. Sizing and filling reference those slots instead of re-running the callee. The slot array is a `sub rsp, 16*N` region pointed to by `r11`; the concat's main buffer is allocated BELOW it; final cleanup via `mov rsp, r9` frees the main buffer, the slot array, and any concat buffers the callees allocated — all in one instruction.

**Why pre-eval (not interleave)**: the main buffer's allocation needs the total size computed first (`sub rsp, rax`). After the allocation, rsp moves by an amount known only at runtime, so any rsp-relative addressing to the Call results becomes fragile. Copying the results into a register-pointed slot array gives us a stable addressing base (`[r11 + 16*i + {0,8}]`) that survives the subsequent `sub rsp, rax` unchanged.

**Register choice: `r11`.** Linux syscalls clobber `rax`/`rcx`/`r11`, but no syscall happens between setting r11 (after pre-eval) and the final `write` (after fill). `r11` isn't used by `emit_eval_expr`, `emit_strlen`, `emit_itoa_to_buffer`, or any existing concat machinery. Picking a register already-clobbered-by-syscalls rather than saving `r12`–`r15` keeps the cross-phase register table unchanged.

**Composability (nested Call-in-concat).** If a callee's body is itself a `concat(...)` with its own Call args, the inner `emit_concat_to_buffer` will also want `r11`. Solution: `emit_concat_to_buffer` saves and restores `r11` across each callee evaluation when the outer concat has Call args. Two extra `push r11 / pop r11` bytes per Call arg; no new machinery.

**Emission flow (outer concat with N Call args):**
```
; classify args, compute static_total, has_text_field, has_call_text, n_calls

if !has_text_field && !has_call_text:
    (fast path, unchanged)

mov r9, rsp                             ; capture rsp BEFORE any allocations

if n_calls > 0:
    sub rsp, 16*n_calls                 ; reserve slot array
    mov r11, rsp                        ; r11 = slot base
    let slot_idx = 0
    for each arg in order:
        if arg is Call:
            push r11                     ; preserve in case callee nests
            emit_callee(arg) -> rax, rdx ; validate + recurse on callee body
            pop r11
            mov [r11 + 16*slot_idx], rax        ; ptr
            mov [r11 + 16*slot_idx + 8], rdx    ; len
            slot_idx++

mov rax, static_total
; per arg: Text field -> strlen; CallText -> add rax, [r11+16*i+8]
add rax, 7 ; and rax, -8
sub rsp, rax                            ; main buffer
mov rbx, rsp
mov r10, rbx

; fill pass: per arg -> (Text literal inline, Number itoa, text-field strlen+rep movsb,
;                        CallText: mov rsi,[r11+16*i]; mov rcx,[r11+16*i+8]; rep movsb)

mov rax, r10 ; mov rdx, rbx ; sub rdx, r10
(caller: write + `mov rsp, r9` via ConcatBufResult::Dynamic)
```

**Callee evaluation in pre-eval**: recurse on `callee.logic.value` with the same Phase 2G restrictions (same-concept, same-input-name, no-lets, output_ty == Text). The callee's body can be a Text literal (→ inline lea + const len), Field access (→ load ptr + strlen), Concat (→ nested emit_concat_to_buffer, allocates its own buffer), or another Call (→ recurse). All four land in `(rax=ptr, rdx=len)`.

**Security (pillar #1):** same three syscalls as before (`read argv`, `write fd`, `exit`). One new register convention (r11 as pre-eval slot base, per-concat, save/restore across nested Calls). No new rbp slots, no new static allocations. All buffer lifetimes are stack-bounded and freed together via `mov rsp, r9`. Slot array size = 16·N where N is compile-time known.

**Scope v1** (refused otherwise with clear messages):
- `emit_concat_to_buffer`-level: a Call arg passes the Phase 2G restrictions (output_ty == Text, same-concept, same-input-name, no lets, single arg = `Ident(caller_input)`).
- Callee body shape: text literal, input text field, `concat(...)`, or `Call(...)` (recursively 2G-compliant).

**What Phase 2H-b does NOT do** (deferred):
- Call with non-input arg (cross-concept or with transformations on the arg) — inherits from 2D/2G scope.
- Callees with let bindings — mirrored refusal.

### Continuation-passing emission for branching results

When a rule produces a discriminated output, each leaf is emitted as a tail-call: it writes its own output and `jmp`s back to the iteration top. **No intermediate tagged value materializes in registers or memory.** The pattern is now used in three emitters:

- `emit_eval_result_expr` — `Result(T, E)` outputs. Each Ok/Err leaf streams to stdout/stderr and jumps.
- `emit_eval_record_expr` — record outputs. Each Record leaf serializes to JSON and jumps.
- `emit_redirect_callee_leaves` — `match_result` inlining. The inlined callee's Ok/Err leaves are redirected: Ok values bind to `match_slot` and invoke the outer Ok arm's recursion (itself tail-terminating); Err values write directly to stderr.

Why this over the standard "materialize tag + payload, dispatch at a common exit":

1. **Lifetime simplicity.** Any leaf that allocates temporaries (e.g. a concat buffer for an Err message) keeps its allocation local to the leaf. No tracking of "does this exit path need to free a concat buffer?" across the dispatch.
2. **No calling-convention ambiguity.** `Ok(number)` and `Ok(text)` would need different payload registers; the leaf knows its declared shape and emits the right write directly.
3. **Tree scales naturally.** Nested `If` inside a Result arm: each distant leaf plants its own terminator. No phi nodes, no fall-through, no common join point to engineer.

Cost: each leaf carries ~30 bytes of trailing write + `jmp loop_top`. For rules with <10 leaves (the overwhelming majority), this is negligible vs. the machinery a tagged-dispatch approach would need.

The choice is *producer-side*: every emitter above is for a rule that PRODUCES the discriminated value. Phase 2D's `match_result` is a *consumer* in the outer rule, but it too avoids a tagged calling convention — instead it inlines the called rule and redirects ITS leaves to the outer match arms. That works because both producer and consumer are compiled together; it would stop working across separately-compiled units. When/if Verbose gains separately-compiled modules with Result-valued exports, a real tag+payload convention becomes unavoidable for consumers. Until then, inlining-plus-continuation-passing is both simpler and smaller.

### Stream semantics for Result-output native rules

A rule with `output: Result(number, text)` compiles to a binary where:

- `Ok(n)` writes `n\n` to **stdout** (newline from `itoa`).
- `Err(msg)` writes `msg\n` to **stderr** (newline appended by the emitter for symmetry).
- Exit code is **always 0** after processing all records.

The "always exit 0" choice is UNIX-idiomatic: failure on a single record is data, not a system error. The shell caller separates success from failure by stream, not by exit code:

```
./validate 200 17 | consume_valid           # pipe valids through
./validate 200 17 2>errors.log              # capture errors to a file
./validate 200 17 1>/dev/null               # keep errors only
```

If a binary wants "exit non-zero on any Err", that's a separate reaction layer (future, not shipped).
