# Backend IR + register allocation (approach B) — DESIGN, pre-review

Status: **DESIGN, not implemented.** This document is the input to a strategic-review
pass (fresh-context critique) before any code lands. The register-allocation arc so far
(A1, A2, A3 — peephole-at-emission) took verbosec-native `fib(40)` from 5.3× to 3.5×
slower than `gcc -O3 -static`, almost all of it from A2 (single param kept in `rbx`).
A3 proved the peephole well is dry for the **compute axis**: the remaining gap is anchored
by call/ret + per-call frame setup + the outer non-simple `fib(n-1)+fib(n-2)` add, none of
which a peephole-at-emission can reach because it never sees liveness across the call tree.
Approach B introduces that liveness.

## 1. The problem A1–A3 cannot solve

The native backend (`src/native.rs`) is an **emit-as-you-go tree walker**: `emit_eval_expr`
recurses an `Expr`, leaving each subexpression's result in `rax`, and combines via the stack.
It has no representation of "this value lives in register R from here to there." So:

- A value live **across a call** must be saved on the stack (`push rax` / `pop rcx`), because
  every caller-saved register is clobbered by the callee. In `fib`, the result of `fib(n-1)`
  is pushed, `fib(n-2)` runs, then it is popped and added. That push/pop pair per recursive
  level is the dominant avoidable cost.
- The **per-call frame** (`push rbp ; mov rbp,rsp ; push rbx ; sub rsp,N`) runs every call
  even when the body needs no spill slots.
- There is no **instruction scheduling** and no **cross-statement register reuse**.

All three need liveness across the whole expression/call tree. That is a register
allocator's job, and a register allocator needs an IR to allocate over.

## 2. Scope — deliberately narrow (security pillar #1)

**B is a new, opt-in codegen path for ONE class of rule. Everything else is untouched and
stays byte-for-byte identical.** This is the same discipline every native slice follows: the
attack surface grows only by deliberate, reviewed commits, never by a sweeping rewrite.

B applies **iff** a callable qualifies under a predicate that extends A2's `qualifies_rbx`:

- input is a record of **Number/Bool** fields only (single OR multi-field — lifts A2's
  single-field restriction; multi-field is the `gcd` shape),
- output is **Number/Bool** (scalar),
- no `concept_group` (the `r11` arena path),
- no text/bytes anywhere (no `rbx`/`r11`/streaming claims),
- body is built only from: Number/Bool literals, field reads, arithmetic/comparison/logic
  binops, `if/then/else`, number-typed `let` bindings, and **calls to rules in the same SCC**
  (self- and mutual recursion) whose arguments are themselves qualifying scalar expressions.

If a callable fails the predicate, **it falls through to the existing tree-walker, unchanged.**
That is the safety contract: B can only ever make qualifying scalar-arithmetic callables
faster; it cannot perturb a single byte of the text/bytes/service/HTTP/fetch/reaction/
collection/arena emitters. The benchmark targets (`fib`, `factorial`, `gcd`, `even_odd`) all
qualify; that is exactly the compute axis we are optimizing.

**No new syscalls, no new effects, no libc, no new instruction families.** B re-uses the same
instruction repertoire the tree-walker already emits — it just decides *where values live*.
The IR is a compile-time artifact, invisible in the binary. Binary attack surface: unchanged.

## 3. The IR

A small linear 3-address IR over basic blocks with infinite virtual registers (`Vreg`).

```rust
struct VReg(u32);

enum IrInst {
    Const   { dst: VReg, val: i64 },
    LoadField { dst: VReg, slot: i32 },        // incoming param, from its arrival reg/slot
    Bin     { dst: VReg, op: BinOp, a: VReg, b: VReg },   // add/sub/imul/idiv/mod/and/or
    Cmp     { dst: VReg, op: CmpOp, a: VReg, b: VReg },   // -> 0/1 in dst
    Call    { dst: VReg, target: RuleId, args: Vec<VReg> },
    Ret     { val: VReg },
}

struct Block { insts: Vec<IrInst>, term: Term }
enum Term {
    Br  { cond: VReg, then_blk: BlockId, else_blk: BlockId },
    Jmp { blk: BlockId },
    Return { val: VReg },
}
```

`if/then/else` lowers to three blocks (cond+Br, then→merge, else→merge) with a merge block;
the two arm results feed a value the allocator places (a φ in spirit; implemented by writing
both arms to the SAME vreg, which linear-scan handles by giving them one interval).

Lowering is a straightforward post-order walk of `Expr` mirroring `body_is_pure_scalar_arith`'s
accepted grammar (so the lowering and the predicate stay in lockstep — a grammar the predicate
accepts but the lowering can't handle is a bug, caught by a debug assert).

## 4. Register allocation: linear-scan

Linear-scan, not graph coloring — simpler, zero deps, near-optimal at this size (these
bodies are tiny). Steps:

1. **Linearize** blocks in a fixed order (cond, then, else, merge) and number instructions.
2. **Live intervals**: for each vreg, `[first def, last use]` over the linear numbering.
   Conservative for branches: a vreg used in either arm is live from its def to the later arm.
3. **Register file**:
   - **Caller-saved scratch** (clobbered by a call): `rax, rcx, rdx, rsi, rdi, r8, r9, r10`
     — fine for intervals that do **not** span a call.
   - **Callee-saved** (survive a call): `rbx, r12, r13, r14, r15` — for intervals that span a
     call. Each one actually used is `push`ed in the prologue and `pop`ped in the epilogue
     (this is why `fib(n-1)`'s result needs no `push rax`/`pop rcx` — it lives in, say, `rbx`,
     and `rbx` is saved once at entry, not once per recursive level).

     Note: `_start` holds `argc/argv/index` in `r12/r13/r14` across the call into the entry
     callable; because the callable saves/restores any callee-saved it uses, `r12–r14` are safe
     to allocate inside the callable. (Same kernel/ABI guarantee A2 already relies on for `rbx`.)
4. **Allocation**: sort intervals by start; for each, expire intervals whose end passed, then
   assign a free register — preferring caller-saved when the interval spans no call,
   callee-saved when it does. If none free, **spill** the interval with the furthest end to an
   `rbp` slot (the frame we already allocate). Spill ⇒ `mov [rbp-k], reg` at def reuse points
   and `mov reg, [rbp-k]` at uses. For these tiny bodies, ≤5 callee-saved is almost always
   enough and spilling is the rare path (but it MUST exist for correctness — a deep nested
   expression could need 6+ cross-call live values).
5. **Call lowering**: evaluate args into `rdi, rsi, rdx, rcx, r8, r9` in order; values already
   in callee-saved regs need no save; the result comes back in `rax` and is `mov`ed to `dst`'s
   register (elided if `dst` is `rax`).

## 5. Integration with the two-pass label resolution

`emit_self_recursive_program` already emits each callable twice: pass 1 with placeholder
(zero) labels to measure sizes, pass 2 with real cross-callable offsets. B preserves this:

- `call rel32` is **5 bytes regardless of value** (the existing invariant), so pass-1 and
  pass-2 sizes match for cross-callable calls.
- **Intra-callable branches** (`if/else`) emit as `jcc rel32` / `jmp rel32` (always 6 / 5
  bytes), with offsets computed from block layout **after** allocation — size-stable across
  both passes. This is strictly simpler than today's `code_size_*` arithmetic because the IR
  knows block boundaries explicitly.

So B slots into `emit_callable_into` as an alternative body emitter selected by the predicate;
the prologue/epilogue (push rbp, save used callee-saved, the abort/bounds-check tails) stay
shared. The bounds-check at field load (the slice-5.1b prerequisite) is preserved verbatim.

## 6. Proposed slice arc (the payoff lands at B2)

- **B0** — IR types + lowering + emit for **straight-line scalar** bodies (no if, no call):
  pure arithmetic single-expression rules. Output must match A3 for those shapes (or be
  provably better). Smallest possible first step; proves the IR→machine-code pipeline.
- **B1** — `if/then/else` basic blocks + branch layout.
- **B2** — **calls**: callee-saved allocation across calls. THE payoff slice — this is where
  `fib`'s push/pop-per-level disappears. Measured against the 3.5× baseline; this is the
  number that justifies the whole arc.
- **B3** — multi-field input (`gcd`) + number `let` bindings.
- **B4** — linear-scan **spilling** when cross-call live values exceed the callee-saved pool.

Each slice: gated on full suite green (correctness), the qualifying path cross-checked
`== interpreter` over input ranges, and (from B2) the benchmark moved vs 3.5×. Pinned sizes
for `fib`/`factorial`/`gcd`/`even_odd` get UPDATED (never disabled). Non-qualifying rules stay
byte-identical — the existing 10-binary SHA gate must remain green throughout.

## 7. Risks and honest unknowns

- **Stack/register discipline bugs.** The A2 segfault (an epilogue that skipped a pushed
  `rbx`) is the cautionary tale: frame surgery is where this breaks. Mitigation: the IR path
  is gated and falls through on anything unhandled; every qualifying example is cross-checked
  `== interpreter` and run (rc=0) on each commit; the prologue/epilogue push/pop set is derived
  mechanically from "callee-saved regs actually assigned," not hand-maintained.
- **Is the win real?** The hypothesis is that removing push/pop-per-recursive-level + tightening
  the body closes a meaningful chunk of the 3.5× gap. It is a hypothesis until B2 is measured.
  If B2 does not move the ratio materially, that is itself a finding (the gap is then dominated
  by call/ret overhead intrinsic to the recursion shape, not by allocation) and we stop the arc
  rather than over-engineer — "evidence > assumptions."
- **Complexity cost.** B adds a real IR + allocator to a 33k-line file. Justification: it is
  the *only* path to the compute axis, it is contained to one rule class, and it is the
  foundation any future scalar optimization (scheduling, strength reduction) would build on.
  If the reviewer judges the win/complexity ratio unfavorable, the honest alternative is to
  accept ~3.5× on naive recursion and bank the structural wins (size, startup, RSS, syscalls)
  that are already category-leading — the compute axis is the one place Verbose is *not* ahead,
  and "naive direct emitter vs LLVM -O3" is an honest framing, not a defeat to hide.

## 8. What B is explicitly NOT

- Not a rewrite of the tree-walker. Text/bytes/streaming/services/HTTP/fetch/reactions/
  collections/arena keep their proven emitters.
- Not graph coloring (linear-scan suffices at this size; revisit only if measured to matter).
- Not a general SSA optimizer. No GVN, no licm, no inlining beyond what already exists.
- Not a new instruction repertoire. Same opcodes, better placement.
