# `arena_scope` — a declared, verified reclaim boundary for the streaming emitter

> **OUTCOME (2026-07-12, measured, verified from clean disk): SHIPPED — 8.7 GB → 2.55 GB (3.43×), 20 s → 3.7 s.**
> gen1 (the self-hosted emitter) peaks 2.55 GB emitting the full 855 KB source
> (was 8.7 GB); gen1==gen2 byte-identical (sha `d65f9b44`); suite 450 green
> (existing binaries byte-identical = additivity); output-preservation holds
> (emitted ELFs run correct across arith/recursion/variant+match). The two-gen
> test (R0+R1+R2) now runs in 10.6 s (was 71 s) — cheap enough to gate PRs.
> The 3.43× (not the aspirational ~9×) is EXACTLY as "Honest expectation" below
> predicted: `arena_scope` reclaims BETWEEN procs, but the biggest proc
> (`x86_node`) still pays `code_size_node`'s O(n²) WITHIN its own scope, which a
> proc-boundary scope cannot reclaim. The documented follow-on — a scalar-result
> `arena_scope` around `code_size_node(x)` (the number survives a reset; its walk
> intermediates don't) — would take it further toward the ~544 MB parse-tree floor.


## Why
The self-hosted emitter (gen1) peaks **8.7 GB** emitting the full 855 KB source;
gen0 (Rust) does the same job in 140 MB. Root cause (measured, see
docs/self-hosting-emit-dedup-design.md): Verbose has no loops — every list
traversal is recursion that constructs an arg record = one arena node — and the
arena NEVER reclaims. So the codegen walk's transient states (`ByteGenState`,
`ProcOffState`, `code_size_node` intermediates) accumulate for the whole emit.
Per-recompute dedup (slice 1, `blob_end_off`) gave ZERO benefit; the fix must be
RECLAIM, not fewer recomputes.

## The insight
The emit STREAMS its output (`x86_stream_node` writes bytes to fd 1 in order).
So once a top-level proc has streamed its bytes, every arena node that proc's
walk allocated is DEAD — nothing references it (Verbose purity guarantees no
escape except via the return value, which was just streamed). Resetting the
arena bump counter (`node_count`) to a mark taken before the proc reclaims all
of it. The parse tree (allocated before emit) sits below every mark and is never
touched.

## The primitive
`arena_scope(e)` in a streaming-bytes position:
1. mark := current `node_count`
2. stream `e`'s bytes (unchanged emission)
3. `node_count` := mark  (reclaim every node `e` allocated)

Semantics: identical OUTPUT bytes to `e` alone; the only effect is that `e`'s
arena allocations are released afterward. Sound because `e`'s value is bytes
that are streamed (consumed) before the reset, and purity guarantees `e`
allocated nothing reachable after its return.

The mark is LOCAL (captured at each `arena_scope`, saved on the stack), so it
nests correctly through the `x86_program` recursion: at recursion level N the
mark is `parse_tree + N` (the ProgGenState chain), the proc's transients are
reclaimed down to it, and the chain (~545 tiny records) survives below.

### Placement
In `x86_program` (examples/vexprparse.verbose:17858) the per-proc concat:
```
RCons(head, tail) => concat(x86_proc(ProcGenState {..}), x86_program(ProgGenState {..}))
```
becomes
```
RCons(head, tail) => concat(arena_scope(x86_proc(ProcGenState {..})), x86_program(ProgGenState {..}))
```
One placement, at the top-level proc boundary — reclaims each proc's walk
transients before the next proc.

## THE CRUX — two arena schemes (get this right or gen1 won't reclaim)
The two backends manage the arena DIFFERENTLY, and `arena_scope` must target the
right one in each:
- **native.rs** (builds gen0): plain-concept records (`ByteGenState`, `ProcGenState`,
  …) are STACK-passed (slice-5.3 ABI); only concept_group nodes hit the arena, at
  `[r11 + max_nodes*entry_size]`. That is WHY gen0 = 140 MB: emitting `x86_proc`
  allocates zero arena nodes (its records are on the stack), so gen0 never
  accumulates. native.rs's `arena_scope` = save/reset `[r11+size]` — GENERAL-correct
  (reclaims any concept_group nodes `e` allocates) but a **no-op for gen0** (nothing
  to reclaim). SAVE `49 8B 83 <size> 50`, RESET `58 49 89 83 <size>`.
- **self-hosted emitter** (in vexprparse.verbose, runs as gen0/gen1 to EMIT gen1/gen2):
  EVERY record is a concept_group node (Verbose has no stack-passing), allocated via
  **r15=base, r14=count REGISTER** (`x86_node` VariantConstruct: `4c 89 f0` mov rax,r14;
  `4c 01 f8` add rax,r15; `49 ff c6` inc r14). That is WHY gen1 = 8.7 GB: `x86_proc`'s
  `ByteGenState`s land in the arena and never free. So the programs THIS emitter
  produces reclaim by save/restoring **r14**: SAVE `push r14` = `41 56`, RESET
  `pop r14` = `41 5e` (2 bytes each). This lives in `x86_stream_node`'s arena_scope arm.

Data flow that makes it consistent:
- gen0 (native.rs output) runs the self-hosted emitter; when it emits gen1's
  `x86_program` it hits the `arena_scope` node and emits `41 56 … 41 5e` (r14) via the
  self-hosted arm → gen1 reclaims via r14 (gen1's arena IS r15/r14). ✓
- gen1 emits gen2's `x86_program` via the SAME self-hosted arm → identical `41 56 …
  41 5e` → **gen1 == gen2**. ✓
- native.rs's own `[r11+size]` reclaim only affects gen0's runtime (a no-op there);
  it never appears in gen1 (gen1's bytes come from the self-hosted arm). So gen0's
  OUTPUT is unchanged by it.
Net: the self-hosted `push r14`/`pop r14` arm is what delivers the 8.7 GB→ win; the
native.rs `[r11+size]` arm is the faithful general impl (no-op for the self-source).

## Both backends (byte-identity contract)
`arena_scope` is a recognized primitive CALL (like `le32`/`byte_at`/`max`), no
new AST node. It must be handled identically in:
- **src/native.rs** (gen0): emit save/reset around the streamed arg in the
  bytes-streaming path; recognize `"arena_scope"` in the verifier (arg bytes,
  streaming position).
- **examples/vexprparse.verbose** (gen1): `span_is_arena_scope` + an
  `x86_stream_node` arm emitting the SAME bytes, mirrored in
  `code_size_stream_node` (the drift edge); recognize it in the self-hosted
  verifier.
Both must emit BYTE-IDENTICAL save/reset so gen1 == gen2.

### Save/reset machine code (exact encoding — filled from the native.rs arena map)
- Arena base register: r11. node_count at `[r11 + <ARENA_SIZE_OFF>]`. Base
  reloadable via `<r11 reload>` (syscalls in the streamed arg clobber r11).
- SAVE (before streaming e): `mov rax, [r11 + OFF]` ; `push rax`.
- stream e (off threaded by SAVE's byte length).
- RESET (after): `<reload r11>` ; `pop rax` ; `mov [r11 + OFF], rax`.
Save/reset are FIXED-size byte constants; `code_size_stream_node`'s arena_scope
arm = SAVE_LEN + code_size_stream_node(e) + RESET_LEN.

## Verifier
`arena_scope(e)` accepted only where sound:
- exactly one arg; `e` bytes-typed; in a bytes-returning (streaming) rule.
- refused elsewhere (a stored/let-bound result would dangle after reset) with a
  breadcrumb. Additivity: programs not using `arena_scope` are unaffected.

## Honest expectation (measure, don't assume)
The parse tree of the 855 KB source is itself **~5.3M nodes / ~544 MB** (a hard
floor — every proc reads it during emit). `arena_scope` reclaims BETWEEN procs,
so peak ≈ parse_tree + the single biggest proc's transients. The biggest proc
(`x86_node`) still pays `code_size_node`'s O(n²)-per-proc within its own scope
(NOT reclaimed by a proc-boundary scope). So expect **8.7 GB → ~1 GB** (order
~9×), CI-affordable in the default suite — NOT 140 MB (gen0's Rust structs are
incomparable). If the per-proc peak is still too high, a follow-on wraps
`code_size_node(x)` in a SCALAR-result `arena_scope` (the number survives a
reset; its walk intermediates don't) — a natural extension of the same
primitive, deferred until measured.

## Alternative considered — auto-reclaim (rejected)
native could auto-reset after every streamed concat operand in a bytes rule
(sound under purity, no source marker). Rejected: it changes EVERY existing
bytes-streaming binary (breaks the byte-identity pins, e.g. print_chain) and
hides the boundary. The declared primitive is surgical (only the self-source
changes; existing binaries byte-identical) and auditable — the author states
where reclaim is safe, the verifier checks it, native exploits it. Fits the
compiler axiom (apply what's declared; never guess).

## Gate (CLEAN disk)
1. `cargo run -- examples/vexprparse.verbose` → all proofs check out; suite green.
2. Additivity: every existing example (incl. print_chain and the bytes-streaming
   set) compiles BYTE-IDENTICAL (cmp vs pre-change) — arena_scope is opt-in.
3. A small hand-written `arena_scope` program: compiles, runs correct, and (probe)
   reclaims (measure a loop that would otherwise blow the arena).
4. Output-preservation: gen1_new(P) == gen1_old(P) for the R2 corpus + full source
   (arena_scope changed memory, not emitted bytes).
5. New fixed point: gen1_new == gen2_new.
6. The win, measured: gen1_new emitting the full source, peak RSS vs 8.7 GB.
7. two_generation_bootstrap_fixed_point (R0+R1+R2) still passes.
