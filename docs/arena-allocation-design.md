# concept_group arena allocation: lifting the self-hosting node ceiling (arc design, pre-review)

Status: **DESIGN, not implemented.** Input to a fresh-context strategic review before any code
lands — the same gate that declined the IR arc and reshaped the records arc.

## The goal and the real constraint

The self-hosted compiler `examples/vexprparse.verbose` is ONE `concept_group VExpr [max_depth:
4096, max_nodes: 65535]`. To parse a self-hosting-scale program — ultimately its own source — it
needs far more than 65535 arena nodes: its own non-comment source is ~57.6k tokens → ~115k
token-stream nodes (`~2N+3`) plus the parsed AST (~30–60k more) ≈ **150–180k nodes**. The
`max_nodes: 65535` ceiling is the wall ([[project_self_hosting_arena_wall]]).

Reconnaissance of `src/native.rs` + `src/verifier.rs` established the wall's true shape:

- **The 65535 cap is VERIFIER-ONLY** (`PHASE_B1_MAX_BOUND` in `src/verifier.rs:1418`, checked for
  both `max_nodes` and `max_depth`). It is a deliberate forward-planning cap, not a storage limit.
- **Storage and arithmetic are ALREADY full 64-bit.** Self-reference fields are 8 bytes
  (`arena_field_byte_width`, native.rs:3130), `node_count` is an i64 slot with 64-bit `inc`, and
  index→address arithmetic (`imul index, entry_size ; add base`) is 64-bit throughout
  (native.rs:13673-13736 produce, 13859-14040 consume). Raising the cap requires no codegen change
  to the index path.
- **BUT the arena is STACK-ALLOCATED.** `arena_size = max_nodes * entry_size` is folded into
  `frame_size` and reserved with a single `sub rsp, frame_size` at the entry prologue
  (native.rs:3266, 3286-3296, 3316-3318). This is the real ceiling.

### The arena-size math (measured)

`vexprparse.verbose`'s widest variant is `MkRule` (5 fields) → padded **entry_size = 48 bytes**.
Every node (even a 1-field `Token`) occupies the uniform entry size. So:

| nodes | entry 48 B | vs 8 MB stack |
|---|---|---|
| 150,000 | 6.9 MB | fits, tightly (plus the rest of the frame) |
| 180,000 | 8.2 MB | **over** |
| 200,000 | 9.2 MB | **over** |

Self-hosting-scale lands **at or over the standard 8 MB stack** (`ulimit -s` default 8192 KB).
Two independent hazards make stack allocation the wrong home for an arena this size:

1. **It overflows the stack** at full-self-compile scale, and any headroom is fragile (a slightly
   larger input or a wider variant pushes it over).
2. **Large-`sub rsp` guard-page hazard.** Linux grows the stack on-demand via a guard page; a
   single multi-MB `sub rsp` that then writes deep into the new region *without touching
   intermediate pages* can skip the guard and SIGSEGV. The current emitter does a bare `sub rsp,
   frame_size` with no stack probing (fine for the small frames seen so far; unsafe at MBs).

**Conclusion: raising the cap alone is wrong** — it would let a program declare a 9 MB arena that
the binary then can't safely allocate on the stack. The load-bearing decision is the **allocation
strategy**, and for self-hosting-scale the answer is to move the arena **off the stack**.

## Proposed design: mmap-backed arena above a threshold

Replace the stack reservation of the arena (only — not the rest of the frame) with an anonymous
`mmap` for groups whose arena exceeds a threshold; keep the in-frame allocation for small arenas.

### Allocation
At the entry rule's group-prologue, when `arena_size > THRESHOLD`:
```
mmap(NULL, arena_size, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
  ; rax = base, or negative (MAP_FAILED range) on failure → abort (shared sys_exit(1) tail)
mov [rbp + arena_base_slot], rax     ; stash the mmap base
```
`arena_size` is still the compile-time constant `max_nodes * entry_size`, page-rounded up.
`frame_size` no longer includes `arena_size` (it shrinks back to the small fixed frame), so the
`sub rsp` returns to its small, guard-page-safe size. `node_count_slot` stays in-frame (it's an
8-byte counter, not the arena).

### Base-register management
Today `r11` (arena base) is `lea r11, [rbp + arena_rbp_offset]`, recomputable after a syscall
clobbers it. With mmap, the base is the mmap'd pointer, so r11 is reloaded as `mov r11, [rbp +
arena_base_slot]` wherever it must be re-established (the same points the current code re-derives
r11 — syscalls clobber r11 by ABI, an already-documented constraint). The index→address
arithmetic (`imul index, entry_size ; add r11`) is **unchanged** — only the provenance of the base
changes (mmap slot vs rbp-relative lea). This keeps the change surgical and the audit story intact.

### Cleanup
The arena lives for the whole process; rely on process-exit reclamation (no `munmap`) — simplest,
no leak (the kernel reclaims on exit), consistent with the no-runtime ethos. For a long-running
*service* with a group this would matter, but `concept_group` rules are batch shapes today
(verified by the example set). Documented as a deliberate scope line; a `munmap`-at-exit slice can
follow if a group ever lands in a service loop.

### The threshold (keeps small binaries lean + byte-identical)
Small `concept_group` binaries (sum_chain, label_tree, factorial-with-group, etc.) should stay
**byte-for-byte identical** and syscall-free: a sum_chain arena is a few hundred bytes — putting it
on the heap would add an mmap syscall and an audit surface for zero benefit, violating
[[feedback_optimize_not_compress]] (leanness as a free consequence; security pillar #1: no syscall
the program doesn't need). So: arenas `≤ THRESHOLD` keep the current in-frame allocation
(byte-identical); only arenas `> THRESHOLD` use mmap. THRESHOLD candidates: a fixed conservative
size (e.g. 256 KB or 1 MB) well below the stack hazard zone. The exact value is a review question.

### The cap
With mmap making large arenas safe, `PHASE_B1_MAX_BOUND` is raised — NOT to unbounded, but to a
generous, documented cap (a few million nodes) that covers self-hosting with headroom and keeps the
arena within sane address space / RAM. `max_depth` (same verifier-only cap, bounds the structural-
recursion proof + the runtime walk's call depth) is raised in step. Note: raising `max_depth` does
NOT add runtime stack-depth enforcement (there is none today); a genuinely 4096+-deep AST walk
recurses that many call frames — a separate concern flagged for the review (does self-hosting-scale
*depth*, as opposed to *node count*, threaten the call stack independently of the arena?).

## Proposed slice arc

- **A1 — mmap-backed arena above THRESHOLD.** The codegen change: group-prologue emits mmap when
  `arena_size > THRESHOLD`, stashes the base, reloads r11 from the slot, frame excludes arena_size.
  Small groups unchanged (byte-identical — the existing group examples are the gate). The index
  path is untouched. THIS is the load-bearing slice.
- **A2 — raise the verifier cap** (`PHASE_B1_MAX_BOUND`) for `max_nodes` and `max_depth`, with
  reworded messages (the "16-bit index budget" wording is now wrong — the storage was always
  64-bit). Coupled to A1: the cap is only safe to raise because A1 made large arenas heap-backed.
- **A3 — prove it on vexprparse.** Bump `vexprparse.verbose`'s group to the new `max_nodes`/
  `max_depth`, feed it a large program (ideally a big chunk of real Verbose, building toward its
  own source), and verify it parses + runs without stack overflow or arena-full abort. This is the
  measurement that justifies the arc — the self-hosting-scale node count actually working.

## Risks / honest unknowns (for the review to pressure-test)

1. **Is the threshold the right design, or should ALL group arenas move to mmap** (uniform, simpler
   one code path, at the cost of byte-identity + a syscall on every group binary)? Trade-off:
   surgical+lean (threshold) vs uniform+simple (always-mmap). The threshold preserves the audit/
   leanness story; the review should confirm that's worth the branch.
2. **Stack-depth at self-hosting scale (independent of node count).** A 150k-node arena holds a
   tree; walking/building it recurses. The build/eval walks are structural recursion — how deep for
   a 57k-token source? If the *call* depth (not the arena) is what overflows the 8 MB stack first,
   A1 doesn't help and the real blocker is elsewhere (an explicit work-stack instead of native
   recursion — a much bigger change). The review must judge whether node-count or call-depth is the
   binding constraint, ideally with a back-of-envelope on the walk depth. **If call-depth is the
   real wall, this whole arc is mis-aimed** — exactly the kind of error the IR review caught.
3. **mmap failure / partial pages**: arena_size page-rounding, the MAP_FAILED check (rax in
   [-4095, -1]), and the shared abort tail. Mechanical but must be exact (a wrong failure check is
   a silent miscompile).
4. **The "16-bit index" deferred optimization** (docs/recursive-types-design.md §6/Q2): raising the
   cap past 65535 means that future width-selection optimization, if ever built, picks 4-byte (not
   2-byte) indices for groups in (65535, 4G]. Does raising the cap now foreclose or complicate it?
   (Probably not — it's deferred and width is currently always 8 B — but name it.)
5. **Cap value**: what's the right new `PHASE_B1_MAX_BOUND`? Big enough for self-hosting headroom,
   small enough to reject obvious nonsense. Justify the number.

---

## Review outcome (2026-06-16) — DECLINED, the arc is mis-aimed

Fresh-context strategic review (the gate that declined the IR arc and reshaped the records arc)
found this arc aims at the **second** wall while the **first** goes unaddressed. Verified from disk;
the design above is retained as the record. **Do NOT build A1–A3.**

**The binding constraint is CALL DEPTH + quadratic time, not arena capacity.** The self-hosted
token stream is a cons-list with O(N)-deep construction and O(N) random access:
- `tokenize` (vexprparse.verbose:862) builds the stream as `Cons{next_token(s), tail: tokenize(...)}`
  — **recurses once per token**, so a 57k-token source is a ~57k-deep native call stack (~3 MB),
  independent of where the arena lives.
- `drop_cells` (896) removes the first `n` cells by **O(n) recursion**, and every `peek_*` is
  `tok_kind(head_tok(drop_cells(arg)))` (1272) addressing the token by **absolute** `ParseState.pos`.
  `parse_primary` issues ~13 peeks per item ⇒ **O(N²) total** (~3 billion cons traversals for the
  self-source) AND O(N)-deep. It would not overflow first only because it would not *finish*.
- **The real current ceiling is the position field bounds**, not the arena: `ScanState.pos [0,256]`,
  `Nth.n` / `ParseState.pos` / `FoldState.pos` `[0,512]` (verified at lines 502/893/1474/1524/…).
  The runtime bounds-check sys_exit(1)s any input past ~512 positions — so today the compiler
  *physically cannot be fed* an input long enough to reach the arena cap. The arena ceiling is not
  even the first wall.

Two further problems the design carried:
- **Raising `max_depth` would be a false explicitation.** There is no runtime stack-depth check;
  `max_depth: 4096` is honored today only because the [0,512] position bounds keep real depth far
  below it. Raising it writes a declared bound nothing enforces — against the "every declaration
  mechanically verified or exploited" axiom ([[project_verbose_compiler_no_guessing]]).
- **A3 isn't demonstrable.** The self-hosted grammar parses a small subset of real Verbose (R1 just
  added *accepting* concept decls), so there is no real large self-input to justify 150k nodes yet.
  The arena arc and the grammar arc are NOT orthogonal in the load-bearing direction.

### What the real chantier is (the actual self-hosting capacity unblock)

Not the allocator location — the **data-structure + recursion model**:
1. **O(1) indexed token access**: the token stream must be a flat array/arena addressed by index,
   not a cons-list walked by `drop_cells`. This kills both the O(N²) time and the O(N)-deep peek
   recursion in one move.
2. **Iteration instead of deep native recursion** for the linear walks (`tokenize` and list
   consumption): an explicit index loop / work-stack rather than one call frame per token.

This is a real arc of its own, design+review-gated like the rest. A cheap probe to characterize it
empirically before designing (per the review): raise ONLY the position field bounds to a few
thousand, keep the arena on-stack, feed a growing synthetic input, and measure whether SIGSEGV
(call depth) or arena-full hits first and how wall-time scales (expect ~quadratic). mmap-backed
arena (A1) remains legitimate as a *follow-on* once the recursion model is fixed and a real large
self-input exists — sequencing it first was the error.

## What this arc is NOT

- Not a general allocator / GC. One mmap'd region per process, freed at exit.
- Not a change to the index path, the entry layout, or the calling convention.
- Not the records-grammar arc ([[docs/self-hosting-records-arc.md]]) — orthogonal. This arc is
  about *capacity* (holding self-hosting-scale node counts), not *grammar* (what can be parsed).
- Not a runtime-growable arena. Size is fixed at compile time from `max_nodes`; over-run aborts
  (the existing arena-full abort, unchanged).
