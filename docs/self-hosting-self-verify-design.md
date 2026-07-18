# The self-verifying bootstrap — the self-compiled compiler verifies before it emits

## Why (vois large)
The two-generation fixed point reproduces the EMITTER: `elf_program_src` = parse →
emit. The four proof checkers (R6a lints, R6b sound types, R6d purity, termination)
exist in the self-source and compile individually — but the bootstrap binary never
runs them. Verbose's identity is "the compiler verifies, never guesses", and the
verifier is the thesis's durable artifact — yet verification is absent from the
self-compiled binary. This arc closes that: **parse → VERIFY → emit, fail-closed,
self-applied** — gen1 verifies the full source before emitting gen2.

## Probe results (2026-07-18, compiled checkers fed the full 855 KB self-source)
- `count_purity_errors` → **0**, exit 0 ✓ (the self-hosted purity checker accepts
  its own source)
- `count_term_errors` → **0**, exit 0 ✓
- `all_diags_count` (the R6a lint aggregate: prog_diags → diag_count) → **rc=1, no
  output** — the abort path. The lint walk does NOT survive the full source.
  Suspected: arena exhaustion (prog_diags allocates DiagList + walk states per
  rule × 594 rules on top of the ~5.3M-node parse; the checker closure already hit
  a 2.45M cap at 174-rule scale in PR #92). Diagnose before fixing.

## Slices
- **V1 — the lint pass survives the self-source.** Diagnose the rc=1 (strace /
  bounds audit / node-count measurement), then fix by MEASURED means: raise
  VExpr max_nodes + PHASE_B1_MAX_NODES if it's the arena (measure the actual need),
  or raise position bounds if it's a [0,4000000]-class abort. Milestone: compiled
  `all_diags_count`(self-source) → prints a count, exit 0 — and the count is **0**
  (or surfaces REAL lints in the self-source, which would be a finding to fix in
  the source itself — either way the checker gets validated against the largest
  real codebase: itself).
- **V2 — one aggregate gate.** `verify_errors(src) = all_diags_count + purity +
  termination (+ the R6b type surface if a source-level driver exists — check
  check_program:14184)`. One compiled binary reports the total. Milestone: 0 on
  the self-source; nonzero on seeded-violation probes (one per surface).
- **V3 — the gate in the bootstrap.** `elf_program_src` refuses to emit when
  verify_errors > 0 (fail-closed exit; the bytes-entry exit-code mechanism needs a
  design decision — int3 vs a trampoline extension; the Result-slice-3 Err ABI is
  the precedent). Then THE MILESTONE: the two-generation fixed point re-established
  with verification in the loop — gen1 VERIFIES its own source (4 surfaces) then
  emits gen2 byte-identical; a seeded violation makes gen1 REFUSE. The complete
  bootstrap: parse → verify → emit, self-applied.

## Risks
- Perf/memory: the verify pass runs at gen1 runtime ON TOP of the 2.55 GB emit —
  measure; `arena_scope` is bytes-streaming-only today, so checker-walk reclaim
  (if needed) is either a cap raise or a scalar arena_scope extension (its own
  design if required).
- The self-hosted checkers are a SUBSET of verbosec's (R6-era scope). The gate
  checks what they check — honest framing: "verifies the four self-hosted proof
  surfaces", not "reproduces verbosec's full verifier".
- gen1==gen2 must survive every slice (the gate code itself is part of the source
  being verified and emitted).
