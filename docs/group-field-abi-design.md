# Group-concept fields in the recursive-callable ABI — design note

**Status:** design note, NO implementation. Written 2026-06-02. Companion to
[native-call-convention-design.md](native-call-convention-design.md) (the
recursive `call`/`ret` ABI) and [composition-abi-design.md](composition-abi-design.md)
(the "an arena index is just a number" insight — this note is its concrete
application to a record *field*). Decision aid for the author, not a commitment.

**Scope:** ONE missing native capability — allowing a **group-concept-typed
field** inside the input concept of a recursive rule, e.g.

```
concept Nth
  fields:
    lst : TokenList   -- TokenList is a concept_group cons-list
    n   : number [0, 512]

rule drop_cells
  input:  arg : Nth
  output: out : TokenList
  logic:
    out = match arg.lst:
      Cons(head, tail) => if arg.n == 0 then arg.lst else drop_cells(Nth { lst: tail, n: arg.n - 1 })
      Nil => TokenList::Nil
```

This is the shape the self-hosting parser (brick 3) needs: a recursive rule
carrying *parse state* — the remaining token list + a position (+ AST-so-far) —
threaded as one multi-field record across recursive calls.

---

## 0. The need, reproduced

`examples/vtokenstream.verbose::drop_cells` is exactly this shape (`arg : Nth`,
`Nth { lst: TokenList, n: number }`). Native refuses it:

```
$ cargo run --release -- examples/vtokenstream.verbose --native /tmp/x --run drop_cells
verified: 4 concept(s), 22 rule(s); all proofs check out
native codegen error: recursive rule 'drop_cells' input field 'lst' must be Number or Text (got Named("TokenList"))
```

The refusal is the per-rule field-type check at `src/native.rs:432-444`. The
interpreter has no such limit — `nth_kind` (the driver that builds an `Nth` from
`tokenize(...)` and calls `drop_cells`) runs correctly under `--run`:

```
$ echo '[{"source": "rule foo if x", "pos": 0}]' | \
    cargo run --release -- examples/vtokenstream.verbose --run nth_kind --input /dev/stdin
  [0] out = 209          # Keyword("rule") kind code — the first token
```

So this is a **native codegen gap, not a language gap** (same framing as
composition-abi-design.md §3.1).

---

## 1. Verification of the observation (a/b/c/d) — is it "just a type-check widening"?

The crux claim is: a group-concept value is, at the ABI level, just a number (an
arena index), so a group-typed field can be marshalled exactly like a Number
field. Verified against code, point by point.

### (a) Group input → i64 index in rdi — TRUE

`src/native.rs:414-422`: when a recursive rule's input is a group concept
(`!c.variants.is_empty()`), the per-field shape check is skipped — "ABI is i64
index in rdi, no field layout." The `_start` prologue (`:3112-3147`) reads the
single argv token via `atoi` into one synthetic slot keyed by the rule's
`input_name`, and `emit_self_recursive_program` (`:3627-3636`) loads that slot
into `rdi` before the `call`. The callable prologue (`:3768`, `:3834-3835`,
`:3875-3876`) spills `rdi` to `[rbp-8]` and maps `input_name → -8`. Exercised
natively by `expr_stmt.verbose` (`eval_expr : Expr`, `count_stmts : Stmt`) and
`label_tree.verbose` (`total_label_length : LNode`):

```
$ /tmp/es 3   →  7      (expr_stmt::compose, builds Stmt tree then walks it)
$ /tmp/lt 5   →  11     (label_tree::measure, builds LNode tree then sums labels)
```

### (b) Group output → i64 index in rax — TRUE

`src/native.rs:446-465`: group-concept output is allowed (`is_group_output`),
"single i64 index in rax, populated by VariantConstruct emit." A
`VariantConstruct` leaf bumps `node_count` and "leav[es] the PRE-inc index in
rax" (`ArenaCtx` doc, `:10699-10700`; emit at `:11591-11986`). So a rule like
`build_tree(seed) -> LNode` returns an index in rax, and the caller consumes it
directly. Verified: `label_tree::measure` calls `build_tree` (returns a tree
index) then `total_label_length` (recurses over it) — both across real
`call`/`ret`, both reading/writing one shared arena.

### (c) Multi-field recursive ABI marshals Number fields as i64 struct slots — TRUE, and a group field slots in identically

The slice-5.3 pointer-in-rdi fields-struct convention:

- **Layout build** (`:3563-3575`): each field is classified `is_text` (16 B,
  2 slots) or **else** (8 B, 1 slot). A group-typed field falls into *else* →
  one 8-byte struct slot. No special case needed.
- **Caller marshalling** (`emit_eval_expr` Call arm, `:11294-11393`):
  - `Ident(input)` pass-through (`:11305-11323`): copies each rbp field slot to
    the struct via `emit_copy_rbp_to_rsp`.
  - `Record(...)` constructor (`:11324-11382`): for each non-text field it does
    `emit_eval_expr(fexpr) → rax → emit_store_rax_to_rsp(struct_off)`
    (`:11372-11380`). For `Nth { lst: tail, n: arg.n - 1 }`, `tail` evaluates to
    the i64 index (it's a MatchVariant binder bound to a self-reference — see
    (d)); storing it at the struct offset is byte-identical to storing a Number.
  - `mov rdi, rsp` (`:11392`) passes the struct pointer.
- **Callee prologue** (`emit_callable_into`, `:3836-3895`): copies struct slots
  to rbp slots, again classified text vs *else*; a group field gets one 8-byte
  rbp slot, registered in `callable_offsets[field_name]` (`:3886-3888`). The
  body's `Field(Ident(arg), lst)` then resolves through `offsets["lst"]` and does
  a plain `load_rax_from_rbp` (`Expr::Field` arm, `:10869-10872`).

So the entire fields-struct path — layout, caller copy, callee unpack, field
load — is **already index-agnostic**: it only distinguishes text (16 B) from
everything-else (8 B), and a group index is an 8-B everything-else value.

### (d) Is the ONLY blocker the type check at 432-444? — Almost. Two refusals, both narrow; the rest of the pipeline is index-agnostic.

There are **two** type refusals on the path, not one:

1. **The recursive per-field check** (`:432-444`) — the one in the error message.
   Refuses `Named("TokenList")` as a field type.
2. **The `_start` per-field argv-parse** (`:3192-3199`) — `_ => Err("only
   number/text today")`. This fires only if a **group-field record is the ENTRY
   rule** (the one invoked via `--run`), because `_start` parses entry fields
   from argv. For the parser, the entry is `nth_kind`/`parse` taking a
   `ScanState {source: text, pos: number}` — both argv-parseable — and
   `drop_cells` is only ever a *callee*. So (2) does **not** block the parser
   use case, and should stay refused: you cannot parse a meaningful arena index
   from argv (the arena is built inside the binary, not handed in).

Everything *downstream* of the type check is already index-agnostic, verified
above in (c) plus:

- **MatchVariant arena dispatch** (`:12045-12232`): step 1 `emit_eval_expr` on
  the scrutinee `arg.lst` loads the i64 index; step 2 computes
  `r11 + idx * entry_size`; step 3 loads the tag; per-arm it binds payload slots.
  The self-reference binder (`tail`) is a non-text field → stored as one i64 and
  registered in `extended_offsets` (`:12175-12192`), so the recursive call reads
  it as a plain index. **MatchVariant on a group field already works** once the
  field reaches `offsets` as an i64 slot.
- **Arena layout** (`arena_field_byte_width`, `:2744-2750`): a group-typed
  payload field (e.g. `head: Token` inside `Cons`) is 8 bytes — an index. So
  `Cons(head: Token, tail: TokenList)` is already laid out as
  tag(1) + head_idx(8) + tail_idx(8), padded. No new layout.

**Conclusion on "just a type-check widening":** Yes, *mechanically* it is. The
fields-struct ABI, the callee unpack, the `Field` load, the MatchVariant arena
dispatch, and the arena entry layout all already treat a group field as a plain
i64. The change is: in the recursive per-field check (`:432-444`), accept
`Type::Named(n)` when `n` is a group concept, *in addition to* Number/Text.
**BUT** that is true *only because* the arena is reachable from the callee — which
is the real question, addressed next.

---

## 2. The arena-liveness question — THE crux

An arena index is meaningless without the arena it indexes into. The arena base
lives in `r11` (`src/native.rs:3086-3097`, `lea r11, [rbp + arena_rbp_offset]`),
and the arena bytes + `node_count` live in the **entry rule's `_start` frame**
(allocated by `emit_record_loop_prologue`, `:3068-3101`). A recursive callable
runs in its **own** frame (its own `push rbp; mov rbp, rsp`). The question: can a
callee that receives a `TokenList` index reach the arena `tokenize` built?

**Answer: YES — and it is already solved, shipped, and exercised by slice 4a.3.**

Evidence:

1. **r11 survives `call`/`ret`.** `r11` is set once in `_start`'s prologue,
   *before* the per-record loop and the `call` into the entry callable
   (`:3623` prologue, `:3679` the call). Verbose-emitted callables never write
   `r11` (it is not in the callable prologue/epilogue, and the recursive bodies
   contain no syscalls — the only thing that clobbers `r11` by the Linux ABI).
   The `ArenaCtx` doc states this explicitly (`:10714-10717`): inside a recursive
   callable the `arena_rbp_offset` is the sentinel `i32::MAX`, meaning "do NOT
   reload — TRUST the current r11 (set up once by the outer prologue, preserved
   across `call`/`ret` because callables don't touch r11 and recursive bodies
   have no syscalls)." The callable's body `ArenaCtx` is built with
   `arena_rbp_offset: i32::MAX, in_callable: true` at `:4009-4022`.

2. **The arena lives in `_start`'s frame and is addressed via r11, not rbp.**
   Both MatchVariant reads (`r11 + idx*entry_size`, `:12052-12060`) and
   VariantConstruct writes (`:11591-11986`, which reload/trust r11) reach the
   arena through `r11`, never through the callee's rbp. So a callee with a
   *different* rbp still reaches the *same* arena. (The `node_count` counter is
   the one piece that conceptually belongs to `_start`'s frame; slice 4a.3's
   chosen scheme keeps construct-from-callee working — see `ArenaCtx` doc
   `:10752-10791`. `drop_cells` only *reads* the arena, so even that subtlety
   doesn't bite the parser-consumer case.)

3. **The builder and the consumer end up in the SAME emit pass, sharing one r11.**
   `compile_native_code` (`:320-349`) extends `scc_rules_owned` with **every
   recursive rule transitively reachable from the entry** when a group is
   declared. So an entry `nth_kind` pulls `tokenize` (arena builder),
   `count_cells`, `drop_cells` (arena consumer), etc. into one
   `emit_self_recursive_program` call (`:554`). They are emitted as sibling
   callables behind one leading `jmp`, and `_start` sets up r11 once for all of
   them. The arena `tokenize` writes is the arena `drop_cells` reads — same
   `r11`, same frame.

**Probe — the existence proof.** `label_tree.verbose` does precisely the
build-then-consume-across-`call` pattern (one rule builds an arena tree, another
recurses over it), and `expr_stmt.verbose` does it with a *multi-concept* group
(`Expr` + `Stmt`, the same group shape `vtokenstream` uses for `Token` +
`TokenList`):

```
$ cargo run --release -- examples/label_tree.verbose --native /tmp/lt --run measure
$ /tmp/lt 5   →  11      # build_tree writes arena; total_label_length recurses over it; one r11
$ cargo run --release -- examples/expr_stmt.verbose --native /tmp/es --run compose
$ /tmp/es 3   →  7       # multi-concept group, group input/output, arena recursion across call/ret
```

Both produce correct results, proving the arena is reachable from recursive
callees across `call`/`ret` today.

**The one genuine difference vs. what's shipped.** In `expr_stmt`/`label_tree`,
the group value crosses the call boundary as the *whole input* (i64 index in
rdi). In `drop_cells` it crosses as *one field of a multi-field record* (i64
index in a fields-struct slot). But the fields-struct slot for a non-text field
is *also* an i64, marshalled by the *same* `emit_store_rax_to_rsp` /
`emit_copy_rbp_to_rsp` used for Numbers (verified in §1(c)). The index value, the
register conventions, the r11 it's interpreted against — all identical. **There
is no new arena-liveness risk** introduced by moving the index from "the whole
rdi" to "a struct slot": r11 is untouched by the marshalling, and the index it
qualifies is the same value.

**Honest residual risks** (small, not arena-liveness):

- **Frame sizing for binder slots.** `count_max_match_arm_binder_slots`
  (`:4115-4158`) computes binder-slot reservation but searches only
  `g.concepts.first()` for the matched variant (`:4122`). For `vtokenstream`,
  `TokenList::Cons` lives in the *second* concept, so the lookup misses and falls
  back to `a.binders.len()` (count, not type-aware slots). For `Cons(head, tail)`
  both binders are non-text (1 slot each), so count == correct slots = 2 — the
  fallback is *accidentally correct here*. This is a **pre-existing slice-4c bug**
  (multi-concept groups + a text binder in a non-first concept would mis-size),
  surfaced by this analysis, not introduced by the extension. Fix it as part of
  the slice (search all concepts), or it ships latent.
- **Cross-concept `head: Token` binder.** `Cons`'s `head` is a *sibling*
  group-concept index. It's read as a plain i64 (1 slot, 8 B per
  `arena_field_byte_width`) and only ever passed onward as an index, so it is
  index-agnostic too. No issue.

---

## 3. Precise scope if built — honest size: **SMALL**

Each step and what it touches:

1. **Widen the recursive per-field check** (`src/native.rs:432-444`). Accept
   `Type::Named(n)` when `n` resolves to a group concept (reuse the
   `is_group_input` lookup pattern already at `:411-417`). One arm added; ~6
   lines. **This is the load-bearing change.**

2. **Keep the `_start` entry refusal** (`:3192-3199`) as-is. A group-field record
   must not be the `--run` entry (no way to parse an arena index from argv).
   Optionally improve the breadcrumb to say "group-typed field is supported as a
   callee, not as the entry rule's argv input." ~0–3 lines.

3. **Fix the binder-slot sizing bug** (`count_max_match_arm_binder_slots`,
   `:4122`) to search all concepts in the group, not just the first. ~4 lines.
   Strictly: not required for `vtokenstream` (accidentally correct), but it is a
   correctness landmine the extension makes reachable. Fix it now.

4. **Tests.** Pin `drop_cells` compiling + running natively (it's a callee, so
   drive it through `nth_kind`); assert byte-for-byte parity with `--run` for the
   `vtokenstream` canonical inputs. Add a regression for the binder-slot fix
   (a multi-concept group with a text binder in a non-first concept).

What it does **not** touch: no new ABI, no new register convention, no new arena
layout, no frame-allocator, no syscall, no marshalling helper. The fields-struct
ABI, callee unpack, `Field` load, MatchVariant arena dispatch, and arena entry
layout are all already index-agnostic (§1). **No CPU overhead** (the emitted code
for a group field is byte-identical to a Number field plus the existing arena
dispatch). **No new attack surface** (no new memory shape; the index is
bounds-checked at the MatchVariant `r11 + idx*entry_size` site exactly as today,
and VariantConstruct keeps its arena-overflow `jae` guard).

Honest size: **small** — a type-check widening plus one latent-bug fix, riding
entirely on slice-4a.3 machinery. The "is the arena reachable" risk that would
have made it large is *already retired* by the shipped design.

---

## 4. Alternatives weighed

### (a) Parser runs interpreter-only — no ABI change

The interpreter already handles group-typed fields freely (`nth_kind` works
under `--run`). Self-hosting's *parser stage* could stay interpreter-only.

- **What it loses:** the self-hosting north-star is a Verbose compiler emitted as
  a native binary. A parser that can *only* run in the interpreter means the
  self-hosted toolchain is not a standalone native artifact for that stage — it
  carries the Rust interpreter as a dependency. That contradicts the
  "native is the destination" rule and weakens the self-hosting claim (the lexer
  bricks already compile native; an interpreter-only parser would be the one
  non-native link).
- **Pillars:** verifiability/safety unchanged (same proofs, same verifier).
  Exploitability/traceability: weaker — the parser doesn't become a small
  auditable ELF. Readability: unchanged.
- **Verdict:** fine as a *stopgap* to unblock parser development, but not the end
  state for a native self-hosted compiler.

### (b) Recurse directly ON the group concept (like `count_cells`), thread extra state elsewhere

`count_cells(lst: TokenList)` already recurses natively on a bare group concept
(no extra fields). Could the parser avoid multi-field state?

- A parser fundamentally needs **at least** the token list *and* an output/AST
  accumulator, and usually a position/lookahead. `count_cells` works because its
  only state IS the list (structural recursion consumes it). A parser produces a
  *different* type than it consumes (TokenList → AST node index) and must thread
  both. You cannot encode "(remaining list, AST-so-far)" as a single bare group
  index without inventing a pairing concept — which is exactly a multi-field
  record with a group field (the thing this note enables), or a new group variant
  `ParseFrame(rest: TokenList, ast: AstNode)` (a group *holding* sibling group
  indices — also already supported, but it forces every state tuple to be
  arena-allocated and matched, which is heavier and less readable than a plain
  record).
- **Verdict:** not viable as a *general* answer — a real parser needs multi-field
  state. The group-variant-as-tuple trick works but is strictly worse on
  readability and arena pressure than allowing a group field in a record.

### (c) The full extension (§3)

Build the type-check widening. Covered above.

- **Pillars:** verifiability (unchanged — same proofs), exploitability (the
  recursive-callable ABI now exploits the "index is a number" identity for a
  field, as it already does for whole-input and arena payloads), safety
  (no new surface — verified in §3), traceability (unchanged — same `call`/`ret`,
  same arena), readability (the *source* shape `ParseState { tokens, pos }` is
  the natural, readable way to write a stateful parser — strictly better than the
  group-variant-tuple workaround of (b)).
- **Compiler axiom (controls + applies, never guesses):** intact. The decision
  "this field is an i64 index, marshal it as 8 bytes" is mechanical from the
  declared type (group concept ⇒ index), not a heuristic. No inference.

---

## 5. Recommendation

**Build the extension now (Option c), small as scoped in §3.** Rationale:

1. **It is genuinely small and low-risk.** The arena-liveness question — the one
   thing that could have made this large or unsafe — is *already retired* by
   slice 4a.3's shipped, exercised design: r11 survives `call`/`ret`, the arena
   is addressed via r11 not rbp, and the SCC-extension puts builder and consumer
   in one emit pass with one shared r11 (§2, proven by `expr_stmt` and
   `label_tree` running correctly). Moving the index from "whole rdi" to "a
   struct slot" introduces no new arena risk: the marshalling is the existing
   Number path, and r11 is untouched by it.

2. **It is the self-hosting critical path.** Brick 3 (parser) needs multi-field
   recursive state with a group-typed field, and alternative (b) shows that need
   is fundamental, not incidental. Alternative (a) (interpreter-only parser)
   would leave the self-hosted toolchain with one non-native link, contradicting
   "native is the destination."

3. **It respects the standing rules.** No CPU overhead (byte-identical codegen to
   a Number field + existing arena dispatch). No security regression (no new
   memory shape; same bounds-check at the MatchVariant index site; VariantConstruct
   keeps its overflow guard). No language change (the source already parses and
   runs under `--run`; this is making native do what the interpreter already
   does, exactly as composition-abi-design.md §3.1 frames it).

**Honest caveats to carry into the slice:**

- Fix the `count_max_match_arm_binder_slots` first-concept-only bug (§2 residual
  risk, §3 step 3) as part of the change — the extension makes it reachable, and
  it would mis-size frames for a text binder in a non-first group concept. It is
  accidentally harmless for `vtokenstream` only because `Cons`/`Nil` have no text
  binders; do not rely on that for the next group.
- Keep the entry-rule argv refusal (§3 step 2). A group-field record cannot be the
  `--run` entry — its index would dangle (no arena built from argv). The parser's
  entry is a `ScanState`/`Source` taking text, which is correct.
- Termination: `drop_cells` already carries `decreasing : n` (Phase C), so the
  recursion proof is mechanical — the breadcrumb is suppressed. The list-walking
  rules (`count_cells`) carry `structural : lst`. So this extension lands on
  rules that already have audit-defensible termination, not bound-only recursion.

If, despite the above, the author prefers to de-risk: ship (a) interpreter-only
parser *first* to validate the parser's logic end-to-end, then build (c) to make
it native. The two are not exclusive — (a) is the stepping stone, (c) is the
destination. But there is no arena-liveness reason to defer (c); that risk is
already paid down.
