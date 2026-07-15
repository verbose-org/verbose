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
- **Slice 2** — `fold(coll, init, acc, item => body)` + fold-form `min`/`max`: same
  loop, source-named accumulator, literal init.
- **Slice 3** — `all`/`any`: desugar to the slice-1 fold shape (`all`: init 1, `if
  pred then acc else 0`; `any`: init 0, `if pred then 1 else acc`). Single-pass, no
  short-circuit (verbosec native parity).
- **Slice 4** — `map`/`filter` → collection OUTPUT (Phase 3): streamed per-element,
  OR the first-class arena cons-list value if the result feeds another rule (this is
  where the collection-as-Value representation, shape B, is introduced).
- **Slice 5** — text fold (Phase 5): append-only text accumulator.
- **Slice 6** — multi-field `collection(Concept)` elements (flattened-argv stride,
  `x.field` resolution).

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
