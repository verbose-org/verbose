# Emitter in Verbose: the text-that-survives-`ret` problem → the STREAMING lowering

Status: **DECIDED + IMPLEMENTED (2026-06-10).** The discussion arc preserved below is the
record: §1–§5 proposed a bounded text arena; §6b's adversarial review found the arena's
flagship consumer breaks its own r11 invariant and surfaced the streaming alternative; §6d's
feasibility probe confirmed streaming fits the existing emitter; the owner approved streaming.
The implemented slice is the streaming lowering (no new language surface, no verifier change);
the arena remains a REJECTED-for-now option, revisitable only if a future slice genuinely needs
string-return-up-the-stack, with §6b's concerns A–E pre-resolved first. Security is pillar #1;
this memo was judged first by attack surface, and the smallest surface won.

## 1. The goal

The self-hosting arc (33 bricks, `examples/vexprparse.verbose`) built, entirely in Verbose, a
front end: lexer → parser → 5 analyses → recursive interpreter → type checker → located
diagnostics. **It never emits code.** The north-star — a Verbose compiler written in Verbose —
needs the other half: an **emitter** that turns the arena AST into output.

The smallest *real* emitter is a **pretty-printer**: `emit_expr(Ast) -> text` that reconstructs
source from the parsed AST. It is emit-shaped (walk the tree, produce a byte stream) without the
machine-code complexity, and it is a genuine milestone: **round-trip** (parse, then print). It is
the natural first emitter slice and the first time the front end produces output rather than a
verdict.

## 2. The precise blocker

Native text values are `(ptr, len)` pairs. Three ways text exists today:

- **Literal** → `ptr` into the binary's data section. Static; survives everything (this is how
  `recursive_label.verbose` returns text from recursion — every frame returns the same pointer to
  the same 4 static bytes).
- **`concat(...)`** → bump-allocated into a buffer below `rsp` on the *current* frame, freed via
  `mov rsp, r9` after the write. **Lifetime = the current frame.**
- **Input field / BoundText** → `(ptr, len)` into argv or a read/fetch buffer.

A pretty-printer's core rule is, in spirit:

```
emit_bin(node) = concat(emit(lhs(node)), op_text(node), emit(rhs(node)))
```

Each recursion level builds **new** text with `concat`. That `concat` buffer lives on *that
level's* stack frame. When the level `ret`s, its frame is gone, so the `(ptr, len)` it returns
points at **freed stack** — a dangling pointer the parent then reads. This is exactly the family
the native backend refuses today (CLAUDE.md: *"Concat-producing text let bindings in recursive
callables — refused… the stack buffer's lifetime doesn't survive across recursive frames"*).

So: **recursive text return works only when the returned bytes are static.** A pretty-printer
needs recursive text return of *freshly built* bytes. That is the gap.

## 3. Option space

### Option A — a bounded **text arena** (recommended)

Reserve one fixed-size byte region, allocated **once** at `_start` (the outermost frame, which
never returns — it `sys_exit`s). `concat` (in an emitting context) bump-allocates into this region
instead of the current stack frame. Returned `(ptr, len)` point into the arena, which outlives
every callee frame. Monotonic bump; **nothing is ever freed** during a run.

- The size is a **declared bound** (e.g. `max_text_bytes: N`), carried and verified, enforced at
  runtime — fail-closed `sys_exit(1)` on overflow (same posture as `byte_at` OOB). An auditor
  reads the cap and knows the memory ceiling.
- This is the **symmetric twin of the existing `concept_group` arena**: the project already has a
  bounded, declared, no-libc arena for AST *nodes* (`max_nodes`). A bounded arena for *bytes* is
  the same blessed pattern, one already reviewed and shipped.

### Option B — caller-provides-buffer (out-param)

The caller passes a destination pointer + remaining capacity; the callee appends and returns the
new length. The buffer lives in a caller frame (or the `_start` frame), so it survives the
callee's `ret`. Essentially manual arena threading through the ABI (C's `snprintf` model).

- More complex ABI (every emit call threads dst + cap + cursor), more per-call bounds arithmetic,
  more places to get a bound wrong. The memory safety argument is *per-call* rather than *one
  region*.

### Option C — a real heap (mmap/brk + allocator) — **REJECTED**

Adds `malloc`/`free`, free-lists, size classes, allocator metadata — the C-sized attack surface
the project explicitly rejects (CLAUDE.md: *"Forcing native to grow to completeness (full heap,
tagged unions) would add a C-sized attack surface and defeat the point"*). Use-after-free,
double-free, metadata corruption. Out of scope, consistent with the LLVM and transpile rejections.

## 4. Recommended: the bounded text arena (Option A) in detail

### Lifetime & placement
Allocated once at `_start` via a `sub rsp, max_text_bytes` region (or a single startup `mmap` —
to decide; the `sub rsp` route keeps it no-syscall and mirrors the node arena). Because `_start`
never returns and every callee frame sits *below* this region, callee `sub rsp`/`ret` never touch
it. The arena base + bump pointer live in a fixed `_start`-frame slot (reloaded as needed) or a
reserved callee-saved register.

### Allocation
A bump pointer. `concat`-into-arena: copy each arg's bytes to `[bump]`, advance `bump += total`,
bounds-check `bump <= base + cap` (else abort). Returned value = `(start_of_this_concat, total)`.
**No free.** A one-shot compile builds its output, writes it, exits — monotonic bump is the right
and simplest model.

### Declaration surface
A new declared bound, e.g. on the emitting rule or the program: `max_text_bytes: N` (range-checked
like `max_nodes`). Carried by the verifier, exploited by native for the fixed reservation + the
bounds check. No inference — the bound is declared (compiler-axiom clean).

### When does `concat` use the arena vs the stack?
Two sub-options to decide:
- **(A1) Context-driven:** `concat` whose result is *returned from* (or flows out of) a recursive
  callable uses the arena; a `concat` whose result is consumed-and-freed within the frame keeps the
  cheap stack path. Requires the emitter to know "this concat escapes." More analysis.
- **(A2) Explicit:** the escaping case is marked (a distinct construct, or the arena is the model
  for *all* text return from recursive callables). Simpler, no escape analysis, slightly more
  arena traffic. **Leaning A2** — it keeps the compiler from *guessing* which concats escape
  (axiom-clean), at the cost of using the arena whenever text is returned from recursion.

### Security analysis (pillar #1)
- **Surface:** one fixed region, a bump pointer, one bounds check. No allocator metadata to
  corrupt; no free-list; **no `free` → no use-after-free, no double-free.** Overflow is
  fail-closed (`sys_exit 1`). The max memory is *statically declared and verified*, so the
  ceiling is auditable — the same property as `max_nodes`, field ranges, and overflow bounds.
- **vs Option C:** strictly smaller surface — none of the heap exploitation primitives exist.
- **No new syscalls** on the `sub rsp` route (one-time startup reservation; no per-emit syscall).
- **Register discipline:** r12–r15 are largely claimed (argc/argv/record-idx/fd-or-loop). The
  arena base/bump likely need a reserved `_start`-frame stack slot reloaded at each arena access,
  or a carefully audited register reassignment. **This is the highest-risk mechanical detail and
  the thing the review must scrutinize** — a clobbered arena base is a memory-safety bug.

### Compiler-axiom check (controls + applies, never guesses)
- The arena size is **declared**, not inferred. ✓
- `concat`-into-arena is a **deterministic lowering**, no heuristic. ✓ (A2 avoids escape-guessing.)
- Lifetime is "the arena region" — statically known, not inferred per value. ✓
- It is an **exploitable declaration**: the bound enables a fixed allocation + a bounds check that
  the optimizer/native can rely on. ✓ (Not false explicitation.)

## 5. Slice plan

1. **Text-arena infra (backend, `src/`).** Declare + reserve + bump + bounds-check. The acceptance
   test is the rule refused today: a recursive callable that builds text with `concat` and returns
   it. Additive + opt-in (gated on the declaration), so **every existing example must compile
   byte-identically** (the SHA-256 baseline discipline). This is the language evolution; it is the
   one that needs the owner's go-ahead.
2. **Pretty-printer in Verbose.** `emit_expr(Ast) -> text` over the vexpr AST, reconstructing
   source. First real emitter; round-trip milestone (`parse(s)` then `emit` ≈ `s`, modulo
   whitespace).
3. **Beyond:** emit a real target (an IL, or Verbose-subset source for another stage). Much larger
   — the actual self-hosting backend. Out of scope for this memo.

## 6. Open decisions for the owner

1. **Go / no-go on the text arena** (the `src/` backend evolution). This is the gate.
2. **A1 (escape-analysis) vs A2 (arena for all recursive text return).** Recommend A2 (axiom-clean,
   no guessing).
3. **`sub rsp` region vs a single startup `mmap`** for the arena. Recommend `sub rsp` (no syscall,
   mirrors the node arena) unless the size makes a stack reservation unreasonable.
4. **Where the bound is declared** — per emitting rule, or per program. Recommend per program (one
   ceiling, like `max_nodes`).
5. **Reclamation:** none (monotonic) for v1. A reset-between-inputs hook (for a long-running/stream
   emitter) is a later question; the one-shot compile model does not need it.

## 6b. Adversarial review outcome (fresh-context, grounded in `src/native.rs`)

A fresh-context review stress-tested §4 against the real backend. Verdict: **reconsider the
approach for the first slice.** Findings:

- **The arena's base lives in `r11`, which survives `call`/`ret` only because today's recursive
  bodies do NO syscalls** (native.rs:11974 trusts r11 on exactly that constraint; syscalls clobber
  rcx/r11 by ABI, native.rs:3057). A pretty-printer *exists to write output* (a `write` syscall) —
  its flagship consumer **breaks the invariant the existing node arena relies on**. So the arena is
  *not* the drop-in "symmetric twin" §4 claimed; it inherits the node arena's mechanism AND its
  unspoken "no I/O in the recursion" precondition, which this use case violates.
- **The "`_start`-frame slot, reloaded as needed" placement (§4) is unreachable** from inside a
  recursive callee — a callee's `rbp` differs, and the code already *skips* such a reload for r11
  (native.rs:11972–11986). The bump pointer must be a dedicated register or a fixed r11-relative
  offset, never an outer-frame rbp-relative slot.
- The bounds check must be **unsigned + overflow-safe + pre-copy**, reusing the shipped
  `jae`/`arena_abort_patches` tail (native.rs:11960), not the signed `<=` §4 wrote.
- A1 (escape analysis) is **disqualified on the compiler axiom** (it guesses which concats escape),
  not merely "more analysis." A2 is axiom-clean.

**The decisive finding — a 4th option the memo missed: STREAMING emit.** For a pretty-printer whose
output goes to stdout, you do not need to *return* text up the recursion at all. Write each leaf's
bytes to fd 1 *in order* during the walk; the recursion just sequences the writes:
`emit_bin(node): emit(lhs); write(op); emit(rhs)`. No materialized string → **no arena, no new
declaration, no bounds check, no r11-lifetime hazard, no go/no-go backend gate.** It is strictly the
smaller attack surface (the memo's own selection criterion). The arena is only truly needed when a
caller must *further transform* returned bytes (build-an-IL-then-reread) — which the pretty-printer
is not.

**The catch streaming raises on the Verbose SURFACE (not resolved by the review):** Verbose rules
are pure expressions returning one value; side effects are top-level reactions, NOT composable
mid-recursion. So "write lhs, then op, then rhs" as an effectful sequence is not directly
expressible today. Two ways to get streaming without a heap:
  (i) **Backend lowering of the existing `output: text` recursive shape.** Keep the surface
      `emit_bin = concat(emit(lhs), op, emit(rhs))` returning text; when such a rule is the
      top-level stdout driver and the concat tree is pure in-order (args already in output order,
      no value reuse), native lowers it to ordered `write`s instead of materializing a buffer. No
      new syntax; but recognizing "pure in-order concat tree" is a restricted-shape check (must be
      syntactic, not a guess, to stay axiom-clean).
  (ii) **A small effectful-sequence construct** (a `then`/`emit` for ordered writes in a rule). New
      surface, but explicit and bounded.

Both are smaller evolutions than the arena, and (i) may need **no new surface at all**. Option B
(caller-provides-buffer) also deserves a real comparison if streaming is rejected: its blast radius
(one caller frame) is smaller than a process-lifetime mutable arena.

## 6c. Revised recommendation

**Do NOT open the arena as the first slice.** Investigate streaming first:
1. Determine whether the existing `output: text` recursive driver (Phase 5a/5b lowering) can be
   native-lowered to ordered fd writes for a syntactically-restricted "in-order concat tree" — the
   no-new-surface path (i). This is a focused backend probe, not a language change.
2. If (i) is not cleanly expressible, weigh a minimal effectful-sequence construct (ii) vs. the
   arena vs. Option B — explicitly, with the security blast-radius comparison.
The arena moves to "future slice, only if a backend genuinely needs string-return-up-the-stack,"
with Concerns A–E pre-resolved before it lands.

## 6d. Streaming feasibility probe (read-only, code-grounded) — VERDICT: feasible-with-modifications

A second fresh-context probe mapped the streaming lowering against the real emitter. Key findings
(all file:line-cited in the probe report):

- **Single refusal point today:** `emit_recursive_text_body`'s catch-all (native.rs:4481) refuses
  `Concat` in a text-returning recursive callable; the let-gate (native.rs:392–403) refuses
  concat/call text lets. Everything upstream (recursion detection → callable path → text ABI)
  already routes correctly.
- **rdi is dead after the callable prologue** — every input field/struct slot is spilled to rbp
  slots up front (native.rs:3843–3877), so in-body `write` syscalls (which need rdi=fd) do NOT
  conflict with input access. The streaming body can write then keep reading fields from rbp.
- **One real register hazard: r11 (arena base).** Inside a callable the arena ctx TRUSTS r11
  (sentinel at native.rs:3936–3954) because today no syscall ever runs inside a callable. A
  streamed write clobbers r11 with no recovery path. Fix is local and cheap: push/pop r11 (4 bytes)
  around every streamed write, or save/reload via an rbp slot.
- **MatchVariant is mandatory and missing from the earlier sketch.** A pretty-printer over an arena
  AST dispatches on variants; `emit_recursive_text_body` has no MatchVariant arm (it would die at
  the same 4481 catch-all before reaching concat). The streaming slice must port the arena
  MatchVariant dispatch (native.rs:12102–12230) into the streaming text context — the largest
  single piece (~150–200 LOC).
- **Mode is a whole-SCC ABI property** — decided once in `emit_self_recursive_program`, threaded as
  a bool into `emit_callable_into` (branch at the is_text dispatch, native.rs:4033) and `_start`'s
  consumption switch (native.rs:3719–3731). Bodies passing today's literal-leaf grammar keep the
  old path → existing examples byte-identical.
- **Shape check is purely syntactic** (no guessing — axiom-clean), and the optimizer (runs before
  native, main.rs:190 vs 342) can only collapse all-literal concats, never reorder — so it cannot
  push a tree out of shape.
- **Verifier: zero changes.** Nothing in verifier.rs models materialize-vs-stream.
- **Documented divergence:** on a mid-tree abort the streaming binary has already written a stdout
  prefix; the interpreter errors with no output. Exit codes agree; bytes-on-failure differ. Stated,
  accepted.
- **Effort:** ~450–550 LOC total, all additive behind the streaming predicate; touched functions
  compile_native_code / emit_self_recursive_program / emit_callable_into. Byte-identical risk low;
  pin recursive_label 739 B, factorial 802 B, gcd 954 B, even_odd 879 B + stdout diff vs --run.
- **v1 grammar:** literal | text input field/BoundText | substring | number expr (itoa) | concat |
  if/else | match with streaming arm bodies | Call(SCC text member). Text-SCC results legal ONLY in
  tail text positions; lets keep today's rules + no text-SCC references; explicit field ranges
  (optimizer interval-arithmetic trap).
- **Acceptance example:** `print_chain.verbose` — Expr = Int|Add (sum_chain's shape),
  `print_expr(e) : text = match e: Int(v) => concat(v) ; Add(l, r) => concat(print_expr(l), "+",
  print_expr(r))` with `structural : e`; seed 3 → `3+2+1+0`, byte-identical to --run.

## 7. Honest framing

This memo proposes the *smallest* step that unblocks an emitter, chosen for the *smallest* attack
surface. It is still a real backend change and a real new declaration in the language. It does not
get us to "a Verbose compiler in Verbose" — it gets us to a **pretty-printer in Verbose**, which is
the first emitter and the proof that recursive text production is expressible and safe. The
distance to a real backend remains large and is named as such.
