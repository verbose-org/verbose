# Scalar `arena_scope` slice 2 — reclaim the size re-walk (the 80/20)

> **OUTCOME (2026-07-24, measured, verified from clean disk): SHIPPED —
> 12.83 → 5.54 GiB (−57%), and ~3× faster (13 s vs 42 s). 119 sites wrapped.
> gen1==gen2 byte-identical at every stage; suite 476/0; fixed point 38 s
> (was 113 s).**
>
> **THE ATTRIBUTION BELOW IS WRONG, and the measurement says where.** This doc
> blamed ~11 GB on the within-proc `code_size_node` re-walk in x86_node /
> x86_stream_node and predicted the big drops in 2b+2c. Measured per stage:
> 2b −435 MB, 2c −848 MB (≈1.3 GB combined — the emit re-walk was real but
> SMALL), **2d −6.33 GiB**. The 6.3 GB lived in `proc_size`
> (vexprparse.verbose:21792), called once per rule from `proc_sizes` (:21843):
> its body walk allocated an arg-record per visited node plus `find_rule`'s
> up-to-690 `FindRuleState` per AstCall, and NOTHING reclaimed it because
> `sizes` is a top-level `let` with no enclosing scope — unlike the emit path,
> which the shipped streaming `arena_scope(x86_proc(...))` already reclaims per
> proc. So slice 1's "~0.8 GB proc_sizes baseline" estimate was off by ~8×, and
> the "size PASS" (not the size RE-WALK) was the dominant term all along.
> Lesson (third time in this arc): the top-level unscoped `let`s are where the
> memory hides; measure per stage rather than trusting a decomposition.
>
> **THE BUMP-MARK LEVER IS NOW EXHAUSTED — slice 3 is dead as designed.** Three
> probes, each applied, measured, and reverted: (a) the doc's slice 3
> (`arena_scope` on `proc_sizes`' `size:`) → 5.814 GiB, inside noise (2d already
> reclaims from INSIDE proc_size); (b) 255 residual number-returning helper walks
> (`blob_end_off`, `proc_offset`, `static_concept_of`, …) → 6.02 GiB, WORSE;
> (c) 96 emit-recursion sites in streaming position → 5.89 GiB, WORSE. All held
> gen1==gen2. The residual ~5.5 GiB is LIVE arena-resident data (parse tree +
> derived lists) interleaved with the parser's own transients; a mark reset
> cannot separate them. The next levers are REPRESENTATION (narrower arena
> entries — 104 B/entry is set by the widest variant's payload) and threading the
> constant `blob_end_off`/`proc_offset` through `ByteGenState` so those walks
> stop happening at all.


REVISED after adversarial review (2026-07-24), which found the first draft's
memory model FATALLY wrong and its site counts off by 2.4×. The placement is
switched from B (wrap the size rules' own recursion) to **A (wrap the emit call
sites)** on the reviewer's evidence. All line refs verified at main `4e124a3`.

Slice 1 (PR #128) built the scalar `arena_scope` primitive and applied it to one
zero-drift site, taking gen1 16.09 → 12.83 GiB. This slice attacks the DOMINANT
remaining term: the within-proc size re-walk.

## The problem
`x86_node` emits a node, then calls `code_size_node(subtree)` to back-patch a
rel32 — re-walking the SAME subtree, allocating arg-records per visited node,
none reclaimed until the enclosing proc boundary (which
`arena_scope(x86_proc(...))` only reaches at the END of the whole proc). The
per-visit cost is worse than "one record per node": `code_size_node`'s AstCall
arm (:21623) evaluates up to 9 `SpanCheck` constructions plus
`callee_is_texty(FindRuleState{...})` → `find_rule` (:5987), which allocates ONE
`FindRuleState` PER RULE SCANNED — up to 690 arena nodes for a single call node.

## PLACEMENT — A (wrap the emit call sites). Why B was rejected.
**B (rejected)** = wrap the recursive mentions inside `code_size_node` /
`code_size_stream_node`. The first draft claimed this makes the size walk
O(depth). **It does not.** The structural recursion for the self-source's
dominant shapes lives in OTHER rules — `code_size_args` (:21692),
`code_size_arms` (:21453), `code_size_vfields` (:20835), plus
`stream_size_cargs` / `stream_size_arms` / `fold_size_cargs` /
`code_size_map_record` / `code_size_filter_record` (reached from
code_size_stream_node :22207/:22225/:22245-22249). The size-walk SCC is **12
rules, not 2**. Leaving those unwrapped leaves an "unwrapped spine" (the
match→arms→concat→args→record-fields backbone) on which every node leaks its
per-visit helper allocations — including the 690-node `find_rule` cost. B's peak
stays O(Σ unwrapped-spine), i.e. still O(n²)-shaped with a smaller constant.
Even B + a stage wrapping the six carriers leaves residual O(sites ×
top-frame-cost), which at O(n) emit-visits × ~690 nodes is still GBs.

**A (chosen)** = wrap at the emit call sites: `bg.off + code_size_node(lhs)` →
`bg.off + arena_scope(code_size_node(lhs))`. The arg-record is constructed
INSIDE the bracket (verified: x86_node's AstCall path emits args → call → add rsp
→ push rax, :21970), so **residual per site is provably ZERO** — the entire
walk, spine and helpers included, is reclaimed when the number returns. Peak =
one full body walk (~10^5-10^6 nodes ≈ tens of MB), which lands the total
essentially at the **parse floor (~0.9 GB)**.

Cost of A: ~117 sites across 15 rules (vs B's 68 across 2) and it needs prereq
#1, which B did not. Accepted — the goal is the number, and A is the only
placement with a principled floor.

### Sites (measured, comment lines excluded)
`x86_node` 60 · `x86_stream_node` 38 · 19 more across `x86_arms`, `x86_args`,
`x86_vfields`, `x86_dispatch`, `x86_stream_arms`, `x86_stream_cargs`,
`x86_stream_dispatch`, `x86_map_record`, `x86_filter_record`, `x86_let_stores`,
`x86_fold_arg`, `x86_proc`, `proc_size`. **A missed hot site re-introduces
O(n²)** — completeness over x86_node + x86_stream_node is the minimum bar.

## Prerequisites (land these FIRST, in one stage)
1. **verifier.rs — accept `(ArenaScope, Number)`, NARROWLY.** Today only
   `(ArenaScope, Bytes)` is accepted (:2224); other expected types hit the error
   arm (:2227-2235). NEEDED FOR A (unlike B): most emit sites are
   `le32(code_size_*(...))`, and `le32`'s arm (:2206) recurses with
   `expected = Number`. Write it **Number-only, never a catch-all** — the arm
   then doubles as the anti-dangling guard (prereq 3): `(ArenaScope, Named(..))`
   keeps erroring. (Empirically verified by the reviewer: arithmetic operands are
   NOT reached — `check_expr_against` has no Binary arm, :2565 — but `le32(...)`
   and whole-branch positions ARE.)
2. **self-hosted type transparency + the `calls:` edit.** `arena_scope` types as
   bytes(4) via `call_result_type` (:13939/13945) → `bin_type` (:13883) requires
   both operands 0 for Add, else ERROR(3) → `tcheck_rule` (:14180) → verrs > 0 →
   `abort_if` REFUSES emission. Fix at **`type_of_env`'s AstCall arm** (:13996 —
   it HAS `args`): `... else if span_is_arena_scope(...) then
   type_of_env(arg_first(args), ...) else call_result_type(...)`. NOT in
   `call_result_type` (its CallRetState has no args field, :13948).
   **CRITICAL — same commit**: add `span_is_arena_scope` AND `arg_first` to
   `type_of_env`'s `calls:` proof list (:14037). Both are real rules; omitting
   them fails verbosec's purity check (gen0 never builds) AND the self-hosted
   purity pass (verrs > 0). Termination has headroom (measured 65 ops vs
   `bound : 4000`).
   Regression check: `arena_scope(x86_proc(...))` (:23434) must still type bytes
   (transparency resolves to x86_proc's declared `bytes`). Documented side
   effect to PIN: slice 1's `let verrs = arena_scope(...)` (:24262) retypes
   4 → 0 — harmless (`tcheck_binds` flags only ==3; abort_if's arg type is never
   checked) but it IS a behaviour change.
3. **Scalar-only soundness** — covered by writing #1 narrowly. The self-hosted
   diag is a documented follow-up, not a blocker (every site in this slice wraps
   a `number`-returning call). **NEVER-WRAP list** (would reclaim under a live
   arena index = silent corruption, not a crash):
   - `proc_sizes` (:21835) `rest: proc_sizes(...)` — a list index (slice 3 must
     wrap only `size:`).
   - `call_result_type` (:13942) `let callee = find_rule(...)` — a RuleDecl index.

NO proof edits are needed for the 117 wrapped sites themselves: `arena_scope` is
not a rule, so `count_undecl_call_ast` (:15535, gated on `rule_named == 1`) can
never flag it — proven by the shipped state (elf_program_src's `calls:` at :24301
omits it and the gate is green). Only `type_of_env` (prereq 2) needs a `calls:`
edit. verbosec matches (verifier.rs:3225 adds no call fact). Termination bounds
have ample headroom (measured: code_size_node 444, code_size_stream_node 223,
x86_node 1524, x86_stream_node 571, all vs `bound : 8000`; `count_operations`
counts ArenaScope as +1, verifier.rs:4046).

## Staging (fixed point + RSS measured after EACH)
- **2a — prerequisites only**, no wraps. Suite + fixed point green ⇒ plumbing
  sound in isolation. (Expect no RSS change.)
- **2b — wrap `x86_node`'s 60 sites.** Measure.
- **2c — wrap `x86_stream_node`'s 38 sites.** Measure.
- **2d — the remaining 19 sites.** Measure.
Expected: the big drops land in 2b+2c (they cover the hot procs). Target after
2d: **~0.9–1.2 GB** (the parse floor is ~0.84 GB and is not beatable by any
reclaim strategy). If 2b shows ~zero drop, the wrap isn't reclaiming — STOP and
diagnose before continuing.

## Invariants / risks
- **Value-preservation**: `arena_scope(e)` returns e's number UNCHANGED ⇒ every
  size, every rel32/jz offset is identical ⇒ user-program output bytes are
  byte-for-byte unchanged (user programs contain no arena_scope).
- **THE single dependency**: `x86_node`'s emit arm (:21970) ↔ `code_size_node`'s
  mirror (:21623) must stay in lockstep on the 6-byte overhead. Validated in
  slice 1. `proc_sizes`/`blob_end_off` consume the same mirror, so the ~117×6 B
  of growth shifts every later offset CONSISTENTLY — provided the mirror is
  right. That is the whole risk of the slice.
- **NO gen0/gen1 byte agreement is required** (the first draft's "bootstrap
  subtlety" was framed wrongly). gen0 is native.rs-compiled machine code
  EXECUTING the self-hosted algorithm; the sizes it computes come from
  `code_size_node` (:21623), not from native.rs. native.rs's own value-position
  ArenaScope arm emits ZERO overhead bytes (src/native.rs:13160-13181, it just
  forwards to emit_eval_expr(inner)) vs the self-hosted 6 — that divergence is
  correct and irrelevant. There is no iteration to converge: gen1 and gen2 both
  apply the self-hosted arms to the same source AST; a mirror/emit divergence
  yields a BROKEN gen1 (wrong rel32 → SIGILL), which the fixed-point test
  catches. No native.rs change is needed for slice 2 (the reviewer verified the
  value-position arm already threads `self_call`/`arena_ctx` and works inside a
  recursive callable, :13176-13179).
- **Reachable in match arms** ✓ (x86_node AstMatch → x86_arms :21577 → x86_node →
  AstCall :21970; mirrored code_size_arms :21453 → code_size_node :21623 — same
  dispatch order in emit and mirror).
- **Nesting** ✓ marks are stack-saved LIFO; a scalar scope inside the shipped
  streaming `arena_scope(x86_proc(...))` restores correctly.
- **Runtime cost**: ~117×6 B ≈ 700 B of ELF, 4 extra instructions per wrapped
  invocation; syscalls clobber rcx/r11, never r14/r15. Reclaim reuses hot arena
  pages — slice 0's precedent was 20 s → 3.7 s, i.e. likely FASTER.
- **2^N trap**: not triggered (no mention duplicated; eval treats arena_scope as
  identity, :5704).
- **Bonus**: the self-hosted `AstVariant` emit has NO arena bounds check (25 B:
  mov/imul/add/mov/push/inc), and the header records a real SIGSEGV at node
  #165M (:130-137). Lowering the high-water is also what keeps gen1 inside its
  mmap.

## Gate (clean disk, per stage)
1. Proofs check out; `cargo test --release` all green.
2. Fixed point `two_generation` (gen1==gen2) + composite demo green.
3. Existing example binaries byte-identical (R2 corpus covers it).
4. **MEASURED** peak RSS, one isolated gen1 emit (`--stdin-raw`,
   `/usr/bin/time -v`, stray heavy procs killed first, note `free -g`):
   12.83 GiB → target ~0.9–1.2 GB after 2d.
5. Self-verify gate intact (self-source emits; unverified source refused).
6. Pin the prereq-2 side effect (verrs retypes 4 → 0).

## Slice 3 (not this slice)
`arena_scope(proc_size(...))` inside `proc_sizes` (:21835) — wrap ONLY the
`size:` field, never `rest:` (see the NEVER-WRAP list).
