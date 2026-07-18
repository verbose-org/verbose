# Collections tier in the self-hosted compiler — sum / count / fold / map / filter / all / any

## Goal
Let vexprparse compile programs that use verbosec's collection features. vexprparse's
OWN source uses none (it walks cons-lists by recursion+match), so this is
coverage-broadening; oracle = verbosec (`--run` and `--native`). It is the largest
tier yet: it introduces the FIRST runtime-conditional loop in the self-hosted
emitter, and a collection value.

## Design decisions (from the design investigation)

### Value (eval / `--run` parity)
A collection = `VData { tag: COLLECTION_TAG, payload: <ValueList of elements> }`,
COLLECTION_TAG = 15360002 (the reserved-tag precedent Ok/Err set at 15360000/1 —
high above any real `cidx*256+vidx`). Zero new `Value` variant; reuses
`ValueList`/`VLCons`/`vdata_payload` entirely.

### Emit (reduction slice) — no materialization
Mirror verbosec's `emit_fold_program` (Phase 4): the collection input is a
COUNT-PREFIXED argv tail (`… <N> <elem×N>`), consumed inline. No collection Value
built at runtime for a reduction. The reduction target must be the entry rule's
collection input field directly (`AstField(AstVar(input), coll_field)`) — verbosec's
own restriction.

### Lambda handling — inline loop, body in the OUTER frame
No lambdas/closures in the emitter. The `x => body` runs as an inline loop in the
same frame as the rule body (so outer params/lets stay in scope — avoids the
recursive-helper capture wall). Bind `x` via the existing match-binder slot
machinery (`lets_append_binders` so `x86_node` resolves `x`/`x.field`; widen
`mb_ast` to reserve its slot). The accumulator is a RESERVED rbp slot managed by
the emitter — NOT a source-named binder (vexprparse can't synthesize verbosec's
`"__acc"` ident: AstVar spans must point into real source).

## Slice 1 — `count` / `sum` over `collection(number)` → number
Acc-less nodes (only `x` is source-named; the accumulator is the implicit reserved
slot — this is why count/sum come before fold):
- `AstSum(coll : Ast, item_start : number, item_len : number, body : Ast)`
- `AstCount(coll : Ast, item_start : number, item_len : number, body : Ast)`

1. **Parse** — intercept `sum`/`count` as keywords in `parse_primary` (mirror
   `parse_match_result`→`mr_ok`→`mr_err`, vexprparse.verbose:2907): `<kw> (
   <coll:parse_or> , <item ident span> => <body:parse_or> )`. `=>` is op 2, `)` is
   op 22. `coll` restricted to `AstField(AstVar(input), coll_field)`. shape_ast
   bands for the two nodes; STUB the other Ast matchers (Result-slice-1 precedent).
2. **Eval** (`eval_ast_env`) — eval `coll` to a collection `VData`, walk its
   `ValueList`; per element push `VECons{item span, element}` onto the VEnv (like
   match binders), eval `body`; `sum`: `acc += vnum_of(body)`; `count`: `acc += if
   vnum_of(body) != 0 then 1 else 0`. Mirrors interpreter.rs:478-492.
3. **Emit** (`x86_node` + a NEW loop-emit helper + its `code_size` mirror) — inline
   loop over the count-prefixed argv tail, mirroring `emit_fold_program`
   (native.rs:9967): reserve acc slot (init 0) + `x`'s binder slot; read `N` from
   argv; per element atoi into `x`'s slot, `x86_node(body)` → push, pop + accumulate
   into acc (`sum`: add; `count`: `test/setne`+add); forward-exit when N==0,
   backward-jmp otherwise; leave acc in rax. **The `code_size` mirror of the loop
   body is the drift edge — it must track the emit arm-for-arm** (same discipline
   as `code_size_arms` for `x86_dispatch`/`x86_arms`).
4. **Trampoline marshal** — a collection field in the entry input: read `N`, leave
   the element words in argv for the loop (no cons-list build). Extend
   `x86_argv_marshal` + `marshal_fields_size`/`argv_marshal_size` for the collection
   field IN LOCKSTEP (they feed `blob_end_off`, the compile-time base for
   byte_at/length/text emits — a mismatch shifts every embedded-source offset).
5. **Milestone** — `count(w.xs, x => x > K)` and `sum(w.xs, x => x)` with `xs :
   collection(number)`, compiled by vexprparse, run on a count-prefixed input,
   output == verbosec's `--native` on the same program+input; plus eval==exec parity.

## Slice sequence after 1
- **Slice 2** (DONE) — `fold(coll, init, acc, item => body)`: same loop, SOURCE-NAMED
  accumulator (the real acc span goes into the binder-list slot that slice 1 reserved
  with a dummy {0,0} entry), init emitted before the loop into the acc slot (any
  expression; emitted before r8/r9 become live), replace-accumulate (`pop rax; mov
  [acc_slot], rax` — same 8 B as sum's add, so the loop rel32 constants are shared).
  Fold-form `min`/`max` are DEFERRED out of slice 2: they would have to become
  parse-time keywords, which risks the self-source's own binary `max()` calls
  (max_payload_fields — the 2^40 fix); they need verbosec-style lookahead
  disambiguation (`=>` after the second argument selects the fold form) first.
- **Slice 3** — `all`/`any`: desugar to the slice-1 fold shape (`all`: init 1, `if
  pred then acc else 0`; `any`: init 0, `if pred then 1 else acc`). Single-pass, no
  short-circuit (verbosec native parity).
- **Slice 4** — `map`/`filter` → collection OUTPUT (Phase 3): streamed per-element,
  OR the first-class arena cons-list value if the result feeds another rule (this is
  where the collection-as-Value representation, shape B, is introduced).
- **Slice 5** (DONE) — text fold (Phase 5b): `output: text` rule whose body is
  `fold(coll, "init", acc, item => concat(acc, ...rest))`. STREAMED where
  verbosec materializes (two-pass sizing + buffer + one write): the init text
  streams first (before r8/r9 go live), then per element each non-acc concat
  arg streams in source order — the accumulator never materializes, it IS the
  stream prefix already written, so the acc binder is never bound (append-only
  validation `fold_stream_ok` guarantees no body site reads it except concat
  arg 0, which the emit skips; violations → whole-fold int3, even on empty
  input). Routing: ast_is_texty/ast_texty_shallow's AstFold arms follow the
  BODY (concat body → texty stream route + entrytx '\n' + anytx itoa, all
  free; arithmetic body → 0, slice-2 number folds byte-identical,
  SHA-verified). Per-arg emit (`x86_fold_arg`): text literal → the 33-B AstStr
  write (syscall clobbers rcx/r11 only); number expr → x86_node + pop rax +
  push r9/call itoa/pop r9 (the 4b r9-save pattern), NO newline per arg; other
  texty shapes → int3. Loop constants: scalar 155 (jz 78+R / back -(87+R)),
  record 72*nf+115 via the slice-6 elem load (jz 72*nf+38+R / back
  -(72*nf+47+R)); `fold_size_cargs`/`code_size_stream_node` mirror
  byte-for-byte. Declared-type guard in x86_proc/proc_size (the 4b posture
  extended): texty fold under non-text output, or number fold under text
  output → int3 proc body. Eval stays NUMERIC (documented divergence at
  eval_fold — text-fold eval deferred; no collection input path reaches it).
  Oracle: verbosec Phase 5b on record elements — byte-identical incl. empty
  ("amts:\n") and negatives; SCALAR elements have NO oracle (Phase 5b predates
  the 4a scalar-input lift — refuses "unknown concept 'number'"), hand-pinned.
- **Slice 6** (DONE) — multi-field `collection(Concept)` elements (flattened-argv
  stride, `x.field` resolution). parse_fields stores the ELEMENT type name as a
  collection field's ty span (ty code stays 3), so `static_concept_of(coll)` IS
  the element-concept resolver (zero new resolution machinery). The five
  reduction arms in x86_node/code_size_node gain a record-element per-element
  load (`x86_elem_load`, 72*nf+26 B): nf atois off the flattened argv words ->
  arena node construct (AstVariant shape, tag = cidx*256) -> node INDEX into the
  item binder slot; the item binder is appended with a synthetic AstVariant RHS
  naming the element concept (`lets_append_elem_binders`), so the body's
  `x.field` flows through the EXISTING AstField emit unchanged. Scalar
  `collection(number)` keeps the untouched else branch — all slice 1-3 binaries
  byte-identical (SHA-256-verified). Numbers-only element fields
  (`field_list_all_number` gate); text element fields are a later slice.
  Trap re-learned: the SlotPops count-down helper's base case must be
  `slot == 0`, not `slot < 0` — the optimizer's default [0, i32::MAX] field
  range dead-eliminates `< 0` on an unbounded number field and the recursion
  never stops.

## Risks (carry into every slice)
- **New loop machinery**: first runtime-conditional loop; the forward-exit +
  backward-jmp rel32 need a `code_size_*` mirror — the drift edge.
- **Span-based identity**: no synthetic `"__acc"` — slice 1's reserved accumulator
  avoids it; future synthetic names reserve a slot, never mint a span.
- **Marshal accounting**: extend size + emit of the collection field in lockstep, or
  `blob_end_off` shifts and every string offset breaks.

## Invariant
vexprparse uses no collections, so the two-generation fixed point (gen1==gen2) must
hold through every slice — the collection paths are never exercised on the
self-compile.
