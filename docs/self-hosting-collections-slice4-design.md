# Collections slice 4 — map/filter → collection output (streamed)

> **REVISED after fresh-context adversarial review (2026-07-13).** The review found
> 3 blockers + 4 must-fixes in the v1 design; the mechanism below incorporates all:
> - **BLOCKER 1 — no direct oracle**: verbosec `--native` REFUSES scalar-element
>   INPUT collections entirely ("Phase 3.2 allows Record inputs only"; scalar input
>   stays interpreter-only). → **Slice 4a (verbosec, FIRST)**: extend Phase 3's
>   input handling to `collection(number)` (one atoi per element into the lambda
>   var slot). This creates the direct oracle AND fixes a real verbosec limitation.
> - **BLOCKER 2 — filter had NO byte-oracle in any form** (scalar refused; the
>   record-filter twin emits identity JSON, not decimals). Slice 4a's scalar-input
>   filter (decimal-per-line pass-through) creates it.
> - **BLOCKER 3 — itoa_proc clobbers r9**, the loop's element cursor (`mov r9,rax`
>   at its head). The per-element call MUST be wrapped `push r9`/`pop r9` (+4 B,
>   counted in the code_size mirror). r8 is untouched by the proc; the newline
>   write clobbers only rcx/r11.
> - **Routing pinned = Package B (stream route)**: `ast_is_texty` returns 1 for
>   AstMap/AstFilter, so the rule routes through `x86_stream_node` and the anytx /
>   blob_end_off / fsz consistency comes FREE (one shared texty definition — the
>   review showed forcing anytx at a local let corrupts p_filesz by 86). The
>   AstMap/AstFilter arms replace the b"\xcc" stubs in x86_stream_node. TRAP: the
>   entry trampoline's collection branch MUST precede entrytx in BOTH
>   elf_program_src's chain AND blob_end_off's chain (else the entrytx branch fires
>   → stray trailing "\n"; empty input would print "\n" instead of ""). Knock-on:
>   callee_is_texty classifies collection rules texty at call sites → value-position
>   calls hit the int3 guard — acceptable, collection-returning rule calls are out
>   of scope (verbosec refuses them too).
> - **Entry detection**: out_ty CODE == 3 confirmed working (output fields go
>   through parse_fields; the code computes before slice 6's span swap). The v1
>   span fallback is DEAD (slice 6 swapped the stored span to the ELEMENT name) —
>   struck.
> - **Filter/record guard**: for record elements x's slot holds an ARENA INDEX —
>   a naive filter would print indices. Filter requires `coll_elem_nfields == 0`;
>   record-element map is fine only with a scalar body. Record-element filter
>   (JSON) is a later slice.
> - COLLECTION_TAG named: reserve 15360002 (Result-style) — v1 referenced an
>   undefined symbol.
> - `filter` needs a NEW kw6 rule (none exists); next keyword codes 20/21 → kinds
>   220/221. Collision sweep CLEAN for both map and filter.
> - Declared-type/body-shape mismatch (out_ty==3 without a top-level map/filter or
>   vice versa) → int3 fail-graceful posture, per the ABI-consistency discipline.


## Goal
vexprparse compiles rules whose OUTPUT is a collection: `map(w.xs, x => x * 2)` and
`filter(w.xs, x => x > 10)` over `xs : collection(number)`, `output: r :
collection(number)`. Oracle = verbosec Phase 3.2 (scalar elements): the binary
reads the count-prefixed argv and emits ONE DECIMAL PER LINE, one line per output
element; empty result → empty stdout, exit 0.

## The design decision — streamed, NOT collection-as-value
Two candidate representations were named in the tier design:
(A) **Streamed per-element output** (verbosec Phase-3 parity): the loop writes each
    element as it's produced; no collection value ever exists at runtime.
(B) **First-class collection value** (arena cons-list): composable (a map result
    could feed another rule / a reduction), but needs a whole value model + itoa
    at the END (an output ABI that walks the list), and verbosec has NO oracle for
    composed collection-returning rules (it rejects them — "collection-returning
    rule calls" are still refused in the Rust backend too).
**Slice 4 = (A).** (B) has no oracle, no consumer (verbosec itself doesn't compose
collections through rules), and would be pure speculation — YAGNI. If composition
ever lands in verbosec, (B) becomes its own designed tier.

## Scope
- `collection(number)` elements only (both input and output; record elements in
  map output = later, needs JSON serialization).
- `map` body → number; `filter` pred → element pass-through (verbosec's identity
  rule: the output element is the INPUT element, the pred only gates).
- The map/filter must be the TOP-LEVEL node of the entry rule's body (verbosec's
  own Phase-3 restriction).
- Input marshal: unchanged (slice 1's collection-field marshal).

## Mechanism
1. **AST**: `AstMap(coll, item_start, item_len, body)` / `AstFilter(...)` — same
   4-field shape as AstSum. Keywords `map` (kw3) + `filter` (kw6) — COLLISION CHECK
   first (the count→cnt / all→full lesson; `filter` and `map` as identifiers in the
   self-source must be checked). parse_reduce generalizes again (kinds 4=map,
   5=filter).
2. **Entry detection**: `entry_rule_collection(pg)` — the entry rule's out_ty CODE
   == 3 (type_code_of_span learned "collection"→3 in slice 1). Verify the OUTPUT
   field parse actually runs type_code_of_span on "collection" (it may go through
   a different path than parse_fields — check parse_rule_decl's output handling;
   if the code isn't set, key on the out_ty span == "collection" like
   entry_rule_result keys on "Result").
3. **Emit — the streaming variant of the slice-1 loop**. A collection-output rule
   routes through a streaming-style proc (like texty rules): the trampoline branch
   is the bytes-entry shape (call proc; proc writes everything; exit 0 — no final
   itoa, no trailing newline beyond the per-element ones). The AstMap/AstFilter
   node emits the slice-1 loop where the per-element step is:
   - map: `x86_node(body)` → pop rax → `call itoa_proc` (the shared 86-B proc the
     texty path already emits — `anytx` must be forced on for collection-output
     programs so the proc exists) → write one `\n` to fd 1.
   - filter: `x86_node(pred)` → pop → test → jz skip; load the ELEMENT value from
     x's slot → rax → `call itoa_proc` → write `\n`; skip:.
   No accumulator slot. The loop rel32s + code_size_node mirror follow the
   slice-1/3 discipline (the drift edge).
   NOTE the itoa_proc call is a rel32 to a FIXED proc location (the texty path
   computes it as -(off+92)-style back-references — mirror exactly how
   x86_stream_node's AstNum arm calls itoa, including how `off` threads).
4. **Interpreter eval**: `AstMap` → VData(COLLECTION_TAG, mapped ValueList);
   `AstFilter` → VData(COLLECTION_TAG, filtered list) — the collection-as-value
   exists in EVAL (cheap, already the tier's eval model) even though native
   streams; same not-runtime-exercised posture as slices 1-6.
5. **Multi-field elements (slice-6 compose)**: `map(w.orders, o => o.amount)` —
   the per-element load reuses slice 6's arena-construct path when the element is
   a concept. Include if free (the loop prefix is shared); note if deferred.

## Milestone
- `map(w.xs, x => x * 2)` on [10,20,30] → "20\n40\n60\n"; on [] → "" (exit 0).
- `filter(w.xs, x => x > 10)` on [5,15,3,25] → "15\n25\n"; all-filtered → "".
- Byte-identical stdout to verbosec `--native` on the same program+input.
- Slice-6 compose case if included: `map(w.orders, o => o.amount)` → amounts.
- Existing binaries byte-identical; two-generation fixed point holds.

## Risks
- The itoa_proc dependency: collection-output programs must force the anytx proc
  emission; blob_end_off must account for the entry-branch + proc presence exactly
  (the entry_rule_result precedent added +210; this branch needs its own constant).
- Newline/format parity with verbosec (trailing newline per element, nothing else).
- The filter pass-through must re-emit the ELEMENT (x's slot), not the pred value.
