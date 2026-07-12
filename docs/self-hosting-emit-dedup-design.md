# Emit-memory dedup — the self-hosted emitter stops re-walking the program per node

> **OUTCOME (2026-07-12, measured): slice 1 gave ZERO benefit — abandoned.**
> Threading `blob_end` as a precomputed constant was byte-behavior-preserving
> (output identical, valid new fixed point) but the self-hosted gen1 STILL peaked
> at ~8.9 GB (baseline 8.7 GB — marginally worse, wider state records). The
> subagent's reported "62× win" was a **measurement artifact**: it measured gen0
> (the Rust backend, ~140 MB regardless — it *compiles* `blob_end_off`, never
> *runs* it), not gen1 (the self-hosted binary that executes it at runtime).
> Root cause of the mis-rank: `blob_end_off` fires only at `byte_at`/`AstStr`/
> `AstVar`-text emit sites, which are RARE in the self-source — removing its
> recompute saved almost nothing.
>
> **The real finding — it's the arena model, not any one recompute.** Verbose has
> no loops: every list traversal is recursion, and each recursive step constructs
> its argument record = one arena node. The arena never reclaims. So `proc_offset`
> (scan the rule list) and `code_size_node` (walk a subtree) each allocate O(len)
> nodes *per call site*, and a name→offset TABLE does NOT help — the table lookup
> is itself a recursive alloc-per-step scan. Neutering either to profile is
> invalid: they compute the call offsets IN the self-emitted binary, so any change
> corrupts gen1's own control flow (neutering `proc_offset` sent gen1 to 26.7 GB /
> 2 min — a broken binary looping, not a clean measurement).
>
> **The principled fix is arena RECLAIM between top-level procs, not per-recompute
> dedup.** The emit streams its output bytes (`emit_bytes_program`), so once a
> proc's bytes are written its walk-transients are dead. Reset the arena bump
> pointer to a post-parse checkpoint after each top-level proc → working set
> collapses to one proc's transients (≈ the 140 MB the Rust backend achieves).
> This needs a new arena-scope primitive (native.rs runtime support + a Verbose
> surface), i.e. a real feature, not a slice — its own design + session.
>
> Everything below is the (now-abandoned) slice-1 plan, kept for the record.

---


## The waste (grounded, measured)
gen0 (verbosec's Rust backend emitting `elf_program_src`) peaks **140 MB / 1.5 s**
emitting the full 855 KB self-source. gen1 (the SELF-hosted emitter, same
byte-identical output) peaks **8.7 GB / 22 s**. The gap is redundant
recompute-with-allocation: the Verbose runtime arena-stores every record it
constructs and never reclaims, and the codegen walk re-computes program-wide
constants at every emit site — each recompute constructs O(concepts)/O(rules)
state records that are pure garbage but never freed. ~90% of the ~90M peak nodes
(× ~104 B entry) are redundant. #94 already fixed the `proc_size` body re-walk
inside `proc_offset`; this closes the remaining recompute.

Investigation verdict (byte-identical fixes, ranked by leverage-per-effort):
1. `blob_end_off` — a PROGRAM CONSTANT, recomputed at every string/text/byte_at
   emit site. **This slice.**
2. `proc_offset` — recomputed per call site, O(rules) each. (slice 2)
3. `code_size_node` offset-threading in `x86_node` — O(n²)-per-proc. (slice 3, invasive)

## Slice 1 — thread `blob_end` as a precomputed constant

`blob_end_off(pg)` returns the file offset where the embedded source blob ends —
it depends only on the program, not on the current node or offset, so within one
emit it is a single value. It fires at three HOT runtime sites in `x86_node`
(once per node of that kind emitted):
- `:17479` `AstCall(byte_at)` — `le64(4194304 + blob_end_off(...))`
- `:17767` `AstVar` text param — `le64(4194304 + blob_end_off(...))`
- `:17769` `AstStr` literal — `le64(4194304 + blob_end_off(...) + start + 1)`
and three COLD sites (O(1) per emit): `:18211`, `:18378`, `:18411`.

Each hot call rebuilds a `ProgGenState` and, inside `blob_end_off`, walks all
~239 concepts (`find_concept_index` / `fields_to_params` / `params_to_fields`),
allocating per concept. Emitting the self-source hits thousands of these →
O(emit_sites × concepts) garbage.

**The fix — mirror #94's `sizes` field exactly.** Add `blob_end : number` to the
codegen state records that already carry `sizes`, thread it from a single
compute at the top, and replace the hot `blob_end_off(ProgGenState { .. })` calls
with the threaded field read.

1. **Compute once.** `x86_program` already builds `let sizes = proc_sizes(..)`
   at its entry (`:17900`). Add `let blob_end = blob_end_off(ProgGenState { .. ,
   sizes: sizes })` beside it (sizes is a dependency of blob_end_off, so order
   after). Thread `blob_end` into the top `x86_program(ProgGenState { .. })`
   call. (`elf_program_src` at `:18411` already computes `fsz0 = blob_end_off(..)`
   — reuse: pass that value in rather than recompute, so the whole emit calls
   `blob_end_off` exactly ONCE.)
2. **Add the field** to every state that carries `sizes` and reaches a hot site:
   `ByteGenState` (30 ctor sites), `ByteGenArgs` (8), `ProcGenState` (4),
   `ProgGenState` (16). Each construction propagates `blob_end: <parent>.blob_end`
   — identical shape to how `sizes:` was propagated. The verifier rejects any
   missed field (field-set mismatch is a compile error), so an omission fails
   loudly, never silently.
3. **Read at the hot sites.** `:17479` / `:17767` / `:17769` become
   `le64(4194304 + bg.blob_end [+ start + 1])`. Update each rule's `proofs:
   purity: reads`/`calls` (drop `blob_end_off` from `calls`, add `bg.blob_end`
   to `reads`; keep `blob_end_off` in `calls` only where it's still invoked —
   the compute-once site + the 3 cold sites, or thread those too if trivial).
4. **Cold sites** (`:18211`, `:18378`, `:18411`): low volume (O(1)/emit). Leave
   as direct `blob_end_off(..)` OR read the local `blob_end` if in scope — either
   is byte-identical; don't add ctor churn for them.

## Gate (CLEAN disk — byte-identity is the correctness contract)
1. `cargo run -- examples/vexprparse.verbose` → "all proofs check out"; full
   suite green.
2. **BYTE-IDENTICAL output.** Build gen0 from the modified source
   (`--native --run elf_program_src --stdin-raw`). For each of: the R2 corpus
   (8 small programs) AND the full self-source (original + reordered), assert
   `gen0_new(P)` == the pre-change `gen0_old(P)`. The dedup must change NOTHING
   in the emitted bytes.
3. **The fixed point still holds.** gen1_new = gen0_new(reordered);
   gen1_new(reordered) == gen1_new → still a fixed point; and gen1_new ==
   gen1_old (the committed golden) — byte-identical to the pre-dedup emitter.
4. **The win, measured.** `/usr/bin/time -v` gen1_new emitting the full source
   directly (not through sh): peak RSS drops materially vs the 8.7 GB baseline.
   Report before/after — that number decides whether slice 2/3 are needed for a
   CI-affordable target.
5. Regression: the extended `two_generation_bootstrap_fixed_point` (R0+R1+R2)
   still passes.

## Honest scope
Slice 1 only — `blob_end_off`. Byte-identical output (gate #2/#3). Does NOT touch
parse, runtime semantics, or any feature surface — the self-hosted compiler
optimizing its own emit. `proc_offset` (slice 2, needs a name→offset table) and
`code_size_node` fusion (slice 3, invasive within-proc restructure) follow if the
measured drop from slice 1 doesn't clear the CI-memory target on its own.
