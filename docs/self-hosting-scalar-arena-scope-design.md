# Scalar-result `arena_scope` — reclaiming within-proc walk intermediates

Grounded in a code-level scoping pass (evidence as vexprparse.verbose:line /
native.rs:line / verifier.rs:line). The arena-reclamation chantier; this doc is
SLICE 1 (the primitive + one zero-drift application). Slices 2-3 sketched.

## The problem (measured)
gen1 (the self-emitted compiler) peaks ~15.4 GiB emitting the full self-source
(690 rules / 1.32 MB), vs the ~2.55 GiB when `arena_scope` shipped (~509 rules).
The self-hosted emitter's runtime arena NEVER reclaims (Verbose has no loops —
every list traversal is recursion building an arg-record = one arena node).
`arena_scope` (PR #105) reclaims BETWEEN top-level procs (resets r14 = arena node
count at each `x86_program` proc boundary), which cannot touch:
- **~11 GB** — the within-proc O(n²) re-walk: `x86_node`/`x86_stream_node` emit
  a node, then call `code_size_node(subtree)` to back-patch a rel32 — re-walking
  the SAME subtree, allocating O(subtree) garbage arg-records per call, ×
  O(program) call sites. The doc's proc-boundary scope reclaims only after the
  whole proc.
- **~2.5 GB** — `let verrs = <diag/purity/term/resource/reaction sum>`
  (elf_program_src:24262): a top-level verify walk over the whole program,
  OUTSIDE every per-proc scope.
- **~0.8 GB** — `proc_sizes`/`proc_size` (elf_program_src:24263 / :21826): the
  size-pass baseline, also a top-level `let`, also unscoped.
- ~0.84 GB parse-tree floor (grew from 544 MB with the bigger source — the hard
  floor no reclaim beats).

## The primitive: scalar-result `arena_scope`
Today `arena_scope` works ONLY in streaming-bytes position (x86_stream_node arm,
:22751): `concat(b"\x41\x56", x86_stream_node(inner), b"\x41\x5e")` = push r14 /
stream inner's bytes / pop r14. In value position (x86_node) it's REFUSED (int3,
:21970). Extend it to wrap a NUMBER-returning walk. The inner leaves its result
PUSHED (stack machine); we must restore r14 WITHOUT losing that value:
```
41 56                push r14        ; save mark            stack: [mark]
<x86_node(inner)>    ; inner pushes number                 stack: [mark, number]
58                   pop rax         ; number -> rax        stack: [mark]
41 5e                pop r14         ; reclaim (restore)    stack: []
50                   push rax        ; re-push number       stack: [number]
```
So the self-hosted `x86_node` scalar arm = `concat(b"\x41\x56",
x86_node(<inner at off: bg.off + 2>), b"\x58\x41\x5e\x50")`. Overhead 6 bytes.
**CRITICAL (review Amendment 1): the inner MUST be emitted at `off: bg.off + 2`,
not `bg.off`** — the `push r14` prefix is 2 bytes, and inner's own body contains
real-rule `call`s (the verrs sum calls diag_count/prog_diags/purity_list/
term_list/resource_errors/reaction_errors, each emitting `\xe8 le32(proc_offset −
(off + code_size_args + 5))`). Emit inner at `bg.off` and every such rel32 is 2
bytes short → gen1's verrs code jumps to a wrong offset → SIGILL at gen1 runtime →
no gen2. The shipped streaming arm already does exactly this (`off: bg.off + 2`,
:22751). This is the REAL drift surface: verrs's VALUE feeds no downstream offset,
but the arm's own inner has an internal rel32 surface that depends on the +2.
`code_size_node`'s mirror arm (:21623, today `1`) becomes `6 +
code_size_node(inner)` — sizes are off-independent (:21781), so no +2 there.
Semantics: returns inner's number UNCHANGED; only effect is inner's arena
allocations are reclaimed (r14 reset). Sound: a Number is a scalar i64 that
references no arena node, so nothing dangles after the reset (`code_size_node`
is pure).

## The changes needed for slice 1 (review cut this from four to TWO)
The review verified that verrs's TYPE is invisible to both verifiers for this
site (a `let`-RHS is never `check_expr_against`'d in verifier.rs, and the
self-hosted checker types `abort_if` as bytes regardless of its arg, and
`tcheck_binds` flags a let RHS only when its type == 3/ERROR — `arena_scope`
types 4/bytes ≠ 3). So the doc's original "riskiest" changes (#1 verifier.rs
accept-arm, #4 call_result_type transparency) are NOT exercised by slice 1 and
are DROPPED here (moved to the slice-2 prerequisites below). Slice 1 needs only:

### A. native.rs (gen0, the runner — Rust backend) — value-position arm, PRESERVE rax
Today `emit_eval_expr`'s `Expr::ArenaScope` REFUSES in value position
(:13160-13168) — so gen0 won't even COMPILE elf_program_src without this arm (it's
necessary, not optional). For gen0's STACK-passed records, `code_size_node`
allocates ZERO concept_group arena nodes, so the `[r11+size]` save/reset is a
genuine no-op with nothing to reclaim — but the arm MUST **preserve rax** (the
returned number). The streaming reset (:6183) clobbers rax via `pop rax`, so do
NOT reuse it: in the value arm, evaluate inner → rax and SKIP the reset entirely
(correct for gen0 — nothing to reclaim), or bracket the reset with `push rax` /
`pop rax`. gen0's bytes are the runner, NEVER byte-compared to gen1/gen2 (only
gen1==gen2 is pinned — review point 1 CONFIRMED), so this needs functional
correctness (compute verrs == the sum's value), not byte-match to the self-hosted
arm. Shape-check (:1938) already transparent.

### B. self-hosted `x86_node` scalar arm + `code_size_node` mirror (vexprparse.verbose)
The `x86_node` AstCall arm's `span_is_arena_scope` branch (:21970, today
`b"\xcc"`) becomes `concat(b"\x41\x56", x86_node(<inner at off: bg.off + 2>),
b"\x58\x41\x5e\x50")` (the +2 is load-bearing — see the primitive section).
`code_size_node`'s matching branch (:21623, today `1`) becomes `6 +
code_size_node(inner)`. THE TWO-PASS DRIFT EDGE: these two MUST stay in exact
lockstep.

Interpreter: NO change (eval_ast_env already treats arena_scope as identity for
any arg, :5704). verifier.rs infer (:2647) + purity (:3225) already transparent.

## Slice-2 prerequisites (NOT slice 1 — documented so they aren't forgotten)
These become necessary only when arena_scope wraps a value in a BODY or
ARITHMETIC position (`bg.off + arena_scope(code_size_node(x))`), which slice 2
does. They are NOT needed for the `let verrs` site.
- **verifier.rs accept-arm**: add `(Expr::ArenaScope(inner), Type::Number) =>
  check_expr_against(inner, Number)` beside the Bytes arm (:2224). (Sound: a
  Number references no arena node.)
- **self-hosted type-transparency**: make `arena_scope` type-transparent. The
  doc's original `call_result_type(arg_first(args))` is MALFORMED — `CallRetState`
  has NO args field (:13948), it can't compile. The CORRECT site is `type_of_env`'s
  AstCall arm (:13996, which HAS args), mirroring concat's dispatch: `... else if
  span_is_arena_scope(...) then type_of_env(arg_first(args), ...) else
  call_result_type(...)`.
- **soundness guard (self-hosted)**: the scalar arm is sound ONLY when inner
  returns a genuine scalar (Number/Bool). An arena-index-carrying inner
  (variant/record/Result) would leave `pop rax` holding a stale index into the
  reclaimed region. verifier.rs's Number accept-arm guards verbosec; the
  self-hosted side needs its own guard before any slice-2 site could wrap a
  non-scalar. (Moot for slice 1 — verrs is a Number.)

## Slice 1 application: `let verrs = arena_scope(...)` ONLY
Wrap exactly ONE site: elf_program_src:24262, `let verrs = arena_scope(<the
existing diag+purity+term+resource+reaction sum>)`. Rationale (the de-risking
opener):
- `verrs` is a NUMBER used only by `abort_if(verrs)` — NEVER in an offset
  computation, so even a subtle sizing issue in the arena_scope arm cannot
  desync a rel32 elsewhere (lowest drift risk of any site).
- It exercises the WHOLE primitive: both verifiers (verbosec's + the self-hosted
  self-verify gate), both backends (gen0 emits it, gen1 emits it), and the fixed
  point (gen1==gen2 must still hold).
- Reclaims the ~2.5 GB verify baseline: expected 15.4 → ~13 GB.

## Verification / invariants
- **Value-preservation**: `arena_scope(e)` returns e's number unchanged, so verrs
  is identical → `abort_if` behaves identically (clean source still emits;
  unverified source still refused). Pin: the self-verify gate still refuses a
  known-bad program and accepts the self-source.
- **Fixed point HOLDS**: wrapping verrs adds the 6 push/pop-r14 bytes to
  elf_program_src's OWN emitted proc, so gen1's bytes CHANGE vs pre-slice — but
  gen1==gen2 because both are emitted by the same self-hosted arm, and the
  `code_size_node` mirror keeps elf_program_src's proc sizing self-consistent
  (blob_end_off etc.). Pin: `two_generation_bootstrap_fixed_point` green.
- **User-program output UNCHANGED**: any user program P contains no arena_scope,
  so gen1_new(P) == gen1_old(P) byte-for-byte (arena_scope changed only gen1's
  RUNTIME RSS, never P's emitted bytes). Pin: existing example binaries
  byte-identical (SHA/cmp; the two_generation R2 corpus).
- **RSS drop MEASURED from the first commit** (the lesson from slice 2a): measure
  gen1's peak RSS emitting the self-source before AND after, on a clean footing
  (kill stray heavy procs; account for the ~13 GiB IDE baseline — measure a
  single gen emit in isolation via `--stdin-raw`, `/usr/bin/time -v`, NOT the
  full test under IDE load). Target: 15.4 → ~13 GB. If it does NOT drop, the
  arena_scope arm isn't reclaiming (r14 not actually reset at runtime) — STOP and
  diagnose, don't proceed to slice 2.
- **Register discipline**: mark saved on the STACK (`41 56`), nests through
  recursion like the streaming form; r15/r11 untouched.
- **2^N trap**: no new recursive helper; each `code_size_node(x)` still evaluated
  once per existing mention — not triggered.

## Gate (clean disk)
1. Proofs check out; suite green; existing example binaries byte-identical.
2. two_generation gen1==gen2 (fixed point) + composite demo green.
3. MILESTONE: gen1's peak RSS emitting the full self-source drops 15.4 → ~13 GB
   (measured in isolation). The self-verify gate still accepts the self-source
   and refuses a known-bad program.
4. self-verify gate pin: the self-source still emits (verrs==0 → abort_if
   passes) AND a known-bad program is still refused (verrs>0 → exit 1). (No new
   verifier arm is needed for slice 1 — verrs's type is invisible to both
   checkers; the pin confirms we didn't accidentally perturb the gate.)
5. Update the STALE in-tree docstrings in the same slice: native.rs:41236 /
   :41259-41263 (say "~2.55 GB", the 855 KB-source figure) and :41271 (the
   ignore-attr "~16 GiB") → the fresh measured numbers. The ci.yml note was
   already corrected in PR #127.

## Slices 2-3 (NOT this slice — sketched)
- **Slice 2 (the 80/20, ~11 GB → target ~4 GB)**: wrap the `code_size_node` +
  `code_size_stream_node` calls in `x86_node` (58 sites) and `x86_stream_node`
  (28+10 sites) in scalar arena_scope. THE drift-sensitive slice — each of ~100
  sites adds 6 bytes the mirror must count exactly; a single miscount desyncs a
  rel32 → gen1 emits a truncated/mislinked ELF that faults at run. Stage
  carefully (e.g. one emit rule at a time, fixed point after each). Do NOT wrap
  code_size_node's own internal recursive calls — the OUTER wrap at the emit call
  site reclaims the whole recursion in one reset.
- **Slice 3 (~0.8 GB)**: `arena_scope(proc_size(...))` inside `proc_sizes`
  (:21835). Brings peak toward the ~0.84 GB parse floor (~1-1.5 GB total).
Honest ceiling: the parse tree (~840 MB at 1.32 MB source) is the floor no
reclaim beats; the doc's "544 MB" was the 855 KB-source floor and is stale.
