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

This is a real arc of its own, design+review-gated like the rest. mmap-backed arena (A1) remains
legitimate as a *follow-on* once the recursion model is fixed and a real large self-input exists —
sequencing it first was the error.

### Probe results (MEASURED 2026-06-16, throwaway copy, arena left at 65535 on-stack)

Ran the cheap probe: raised the position/index field bounds on a copy of vexprparse.verbose, kept
the arena unchanged, fed a growing synthetic program (`rule rN \ logic: \ out = 1`, ~12 tokens
each) to the `count_rules` driver, measured exit code + wall-time.

| rules | ~tokens | exit | wall | result |
|---|---|---|---|---|
| 200 | 2,400 | 0 | 0.37 s | 200 ✓ |
| 400 | 4,800 | 0 | 1.12 s | 400 ✓ |
| 500 | 6,000 | 0 | 2.98 s | 500 ✓ |
| 700 | 8,400 | 0 | ~3–5 s | 700 ✓ |
| 800 | 9,600 | **1 (abort)** | 13 s | — |
| 1,600 | 19,200 | 1 | 26 s | — |
| 2,400 | 28,800 | 1 | 69 s | — |

Findings, decisive:
1. **Time is quadratic.** 200→500 rules: 0.37 s → 2.98 s (~8× for 2.5× input). The per-peek
   O(position) `drop_cells` over the cons-list is the cause — confirmed empirically.
2. **It walls at ~700–800 rules / ~9k tokens**, EVEN with the position field bounds raised — a
   thicket of small `[0,256]/[0,512]/[0,4096]` bounds bite in sequence, and lifting them is not a
   single knob. The abort at 800 is NOT the arena (≈22k nodes, well under 65535) — confirming the
   arena ceiling is not the operative limit.
3. **vexprparse's own source is ~6,000 rules / ~57k tokens — ~8× beyond the wall**, and at O(N²)
   the time would be ~60×+ the already-multi-second runs (minutes-to-hours) even if every bound
   were lifted. The arena cap is irrelevant at this scale.

The probe confirms the verdict: the binding constraints are the **quadratic cons-list access model**
and the **pervasive small position/index field bounds**, not arena capacity. The real fix is
architectural (O(1) indexed token access to kill the O(N²) + the deep peeks; a coherent story for
the index/position field bounds; iteration over deep native recursion), and it is the actual
self-hosting-capacity arc.

---

## Implementation spec — off-stack mmap arena (REVIVED + validated, 2026-06-21)

The C-depth bisection (docs/self-hosting-capacity-design.md) confirmed the operative wall is the
arena `max_nodes` (raising the cap 65535→120000 cleared count_rules K=800; 1600 then needs more;
~85 nodes/rule; no SIGSEGV — not call depth). So this arc is no longer premature. Below is the
implementation-ready spec, grounded in the current code.

### Current mechanism (what changes)
- `arena_size = max_nodes × entry_size` is added to `arena_extra_bytes` → `frame_size` → reserved by
  `sub rsp, frame_size` (native.rs ~3266 / ~3296 / ~3316). **Stack-allocated.**
- `arena_rbp_offset = node_count_slot − arena_size`; `r11 = lea [rbp + arena_rbp_offset]` once
  (native.rs ~3468). Arena reads/writes address `[r11 + index*entry_size + field_off]`.
- Callables use the sentinel `arena_rbp_offset == i32::MAX` (native.rs 4431/5051) = "DON'T recompute
  r11 from rbp — trust the r11 the entry/_start set." (Callables have their own rbp, can't reach the
  entry frame's arena.)
- `node_count_slot` is a small frame slot (the running counter).

### The change (threshold-gated; small groups stay byte-identical)
Decide per program at compile time: `use_mmap = arena_size > THRESHOLD` (THRESHOLD ≈ 64 KiB — covers
all existing small-group examples on the stack; vexprparse's 3 MB+ arena → mmap). When `use_mmap`:

1. **Exclude `arena_size` from `frame_size`** (the `sub rsp` shrinks back to the small fixed frame —
   also removes the multi-MB-`sub rsp` guard-page hazard). Keep `node_count_slot` + tmp slots in the
   frame. Add one new frame slot: `arena_base_slot` (8 B, holds the mmap pointer).
2. **mmap in the _start/entry prologue**, right where `lea r11` is today:
   ```
   mmap(NULL, arena_bytes, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
     ; rax=9 ; rdi=0 ; rsi=arena_bytes(page-rounded) ; rdx=3 ; r10=0x22 ; r8=-1 ; r9=0 ; syscall
     ; (kernel ABI: arg4 in r10, NOT rcx; syscall clobbers rcx, r11)
   cmp rax, -4095 (unsigned-above test) → jae .arena_mmap_fail   ; MAP_FAILED / -errno → abort(1)
   mov [rbp + arena_base_slot], rax    ; stash base for reload
   mov r11, rax                        ; r11 = arena base (replaces the lea)
   ```
   `arena_bytes` rounds up to a page (4096). Reuse the shared sys_exit(1) abort tail for the failure.
3. **r11 discipline (sharper than the stack case — state it as an invariant):** r11 is the arena base
   and is **NOT rbp-recomputable** under mmap (the old failsafe `lea [rbp+arena_rbp_offset]` is gone).
   So: (a) the callable sentinel stays `i32::MAX` = "trust r11" (UNCHANGED — callables don't recompute
   it either way); (b) **every syscall in a group program must preserve r11** — push r11 / syscall /
   pop r11, exactly as `emit_streamed_write_rsi_rdx` (native.rs:5055) already does, and as the recent
   concat-Call fix (base its slot array on r9 not r11) established. Entry-level code MAY instead reload
   `mov r11, [rbp + arena_base_slot]` after a syscall (the slot is reachable at entry rbp). Callables
   can't reach that slot, so callable syscalls MUST push/pop r11. The parser's recursive callables make
   no syscalls (pure parse), so in practice r11 survives all call/ret; the guard matters only for the
   effect/streaming paths, which already guard. **Net new rule: never emit a syscall in a group
   program without a surrounding r11 guard or an entry-level reload.**
4. **Cleanup:** none — rely on process-exit reclamation (no munmap), consistent with no-runtime ethos.
   Documented scope line; a munmap-at-exit slice can follow if a group ever lands in a service loop.
5. **Stack (non-mmap) path UNCHANGED** for `arena_size ≤ THRESHOLD` — byte-for-byte identical to
   today (sum_chain/label_tree/factorial-with-group/token_* keep their exact binaries).

### Cap raise (coupled — only safe because the arena is now off-stack)
Raise the verifier cap `PHASE_B1_MAX_BOUND` (verifier.rs:1418) for `max_nodes` (and `max_depth`) from
65535 to a generous documented ceiling — proposed **4_000_000** (covers self-source ~500k–1M nodes
with headroom; worst-case arena 4M×48 B ≈ 192 MB mmap, which fails gracefully via the MAP_FAILED
abort if the host can't back it). Reword the "16-bit index budget" message (storage was always
64-bit; the index path needs no change — confirmed in the earlier recon). `max_depth`: raising it is
still a FALSE EXPLICITATION unless runtime depth is enforced — so RAISE `max_nodes` now, leave
`max_depth` at a defensible value (or tie it to a real check) per the no-guessing axiom.

### Gate
- Suite green (currently 412). **Byte-identical for every non-mmap (small-group) program** — the
  SHA/size-pinned group examples (sum_chain etc.) must not shift (threshold guarantees this).
- The native==interpreter cross-check (just restored) holds on a large multi-line source.
- **Scale test:** count_rules clears K=800/1600/3200 (was arena-walled at 800) and scales linearly in
  time; push toward a meaningful self-input. Measure peak RSS (mmap arena is now heap, not stack).
- mmap failure path tested (e.g. an absurd max_nodes → MAP_FAILED → clean exit 1, not a crash).

### Risks
1. **r11-after-syscall** is the sharp edge (the recent r11-clobber bug is the cautionary tale). The
   invariant in §3 must hold at EVERY syscall site in a group program — audit them (streaming write,
   append_file, read, fetch, mmap itself). A missed guard = arena corruption = the exit(2) MatchVariant
   trap. Mitigation: a single `emit_syscall_in_group` helper that wraps push/pop r11, used everywhere.
2. **mmap arg4 in r10** (kernel ABI), not rcx — easy to get wrong; the syscall clobbers rcx+r11.
3. **Threshold value** — 64 KiB is a guess; confirm all existing group examples' arenas are below it
   (they are tiny) so none flips to mmap unexpectedly (which would break their byte-identity pins).
4. **Depth is the NEXT wall after this**, not solved here: at true self-source scale the parser/
   tokenizer recursion depth (parse_program ~per-rule, tokenize_indent ~per-line) may eventually
   exceed the 8 MB stack. The bisection showed no SIGSEGV at tested K (arena walls first), so depth is
   deferred — but note it so the next bisection expects it.

This is the validated, implementation-ready next slice. Suggested execution: worktree subagent (like
the C-tok fix), strong gate (byte-identity for small groups + cross-check + scale test), commit-don't-
revert, monitored via worktree state.

## What this arc is NOT

- Not a general allocator / GC. One mmap'd region per process, freed at exit.
- Not a change to the index path, the entry layout, or the calling convention.
- Not the records-grammar arc ([[docs/self-hosting-records-arc.md]]) — orthogonal. This arc is
  about *capacity* (holding self-hosting-scale node counts), not *grammar* (what can be parsed).
- Not a runtime-growable arena. Size is fixed at compile time from `max_nodes`; over-run aborts
  (the existing arena-full abort, unchanged).
