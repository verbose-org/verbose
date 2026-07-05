# Records arc slice 3: AstField CODEGEN (field access → machine code)

## Context
Slice 2 made the interpreter (oracle) read record fields. `x86_node`'s AstField arm
is still `b"\xcc"` (int3, line ~14522; code_size 1 at ~14328). Slice 3 emits real
machine code so the COMPILER handles field access. The crux vs eval: no runtime tag
to read — the base's concept must be resolved at COMPILE time. Lazy solution: the
`lets` Binding list already carries each let's RHS Ast (`BCons of (…, value : Ast,
…)`), so for `let s = Foo {…}` the binding's value is `AstVariant(Foo,…)` — derive
`s`'s concept from it. No new type-tracking map.

## Design (reuse R7b slot-load + slice-2 field_index_of + entry_size)

### concept derivation
New helper `let_rhs(binds, src, q_start, q_len) -> Ast` (mirror `let_index`, but
return the `value` Ast from the matching `BCons`; sentinel `AstErr` if absent). For
`AstField(base, f)` with `base = AstVar(s)`: `let rhs = let_rhs(lets, s)`; match rhs:
`AstVariant(cstart, clen, _, _, _) => <cstart,clen is s's concept>`. Anything else
(not a let, RHS not a construction) → can't resolve → keep int3 (documented; the
let-bound-record case is the slice, matching slice 2's milestone).

### emit
`AstField(base, fstart, flen)` when resolvable:
1. `x86_node(base)` — emits the base, leaving its arena INDEX pushed (for
   `base = AstVar(s)` this is the let-slot load R7 already emits).
2. suffix (fixed bytes, mirrors R7b's node-addr + payload read):
   `pop rax` (58); `imul rax, rax, entry_size` (48 69 c0 + le32); `add rax, r15`
   (4c 01 f8); `mov rax, [rax + 8 + 8*field_index]` (48 8b 80 + le32) — field_index =
   `field_index_of(cd_fields(concept_at_index(find_concept_index(cstart,clen))), f)`;
   `push rax` (50).
   entry_size = the existing helper (same value R7a/R7b use — one source of truth).
3. `code_size_node` AstField = `code_size_node(base) + <fixed suffix length>` — must
   match the emit EXACTLY (the two-pass offset sharp edge). The suffix is constant
   length regardless of field_index / entry_size (imm32 forms), so it's a fixed number.

## Gate (verify from CLEAN disk — emitted ELF runs, matches slice-2 eval oracle)
1. `cargo run -q -- examples/vexprparse.verbose` → "all proofs check out"; suite green
   (currently 425 + 1 ignored) + a new slice-3 test.
2. **MILESTONE** (emit via elf_program_src, run the ELF):
   - `concept Foo / fields: a : number, b : number` + `main: let s = Foo{a:42,b:7};
     out = s.a` → ELF prints **42**; `out = s.b` → **7** (proves field_index, not
     first-slot); `out = s.a + s.b` → **49**. Each == the slice-2 `eval_main` oracle.
   - UNCHANGED: the R7 variant list-sum ELF → 6/15; scalar `2+3` → 5, byte-identical
     (AstField-free programs must not change — the arm only fires on AstField).
3. Regression test (src/native.rs, mirror r7b_match_variant_dispatch_and_bind): emit +
   run the ELF for `s.a`→42, `s.b`→7, and cross-check `--run eval_main` gives the same.

## Honest scope
Slice 3 = AstField codegen for the LET-BOUND record case (base = AstVar(s), s a let
whose RHS is a record construction) — the arena-node case, matching slice 2's oracle.
Reuses R7b's slot load + slice 2's field_index_of + entry_size; the only new piece is
`let_rhs` (concept from the binding). DEFERRED: input-field access
(`AstField(AstVar(input), f)` where input is the rule's declared record input) — needs
the record-input ABI decision (how a record param is passed to a proc: arena index vs
flattened fields), a separate slice. After slice 3 the compiler handles let-bound
record field access end-to-end; input-record rules (the self-source's real shape) are
the next slice.
