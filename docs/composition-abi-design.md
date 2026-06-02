# Composition ABI — design note (reuse / structured returns)

**Status:** design note, NO implementation. Written 2026-06-02 after the TLS 1.3
crypto arc (X25519, Ed25519, ECDSA-P256) shipped. Companion to
[native-call-convention-design.md](native-call-convention-design.md), which
covers the *call* convention from the recursion angle (Phase A/B/C). This note
covers the same machinery from the **reuse / composition** angle, and argues
about *when* (if ever) it's worth building. It is a decision aid for the author,
not a commitment.

**Scope:** this note is about ONE missing capability — a Verbose rule returning a
**structured value** (a field element = N limbs, a Record, a Result) to another
rule, so the callee's body exists ONCE and is *called* rather than *re-emitted*.
Everything else (recursion, arg passing) is already designed/shipped.

---

## 1. The observation that triggered this

The crypto arc shipped a working browser-reachable HTTPS server, all crypto on
Verbose machine code. But the binaries duplicate enormously:

- `p256_field.py` and `p256_scalar.py` are the SAME Montgomery CIOS code, twice
  (different modulus constant). Every `fmul` in the point ops re-emits the full
  ~780-let multiply inline.
- Every KDF binary (`derive_*`, `expand_*`, `tls_schedule`) re-emits SHA-256 in
  full — that's why they're ~327 KB each, ~5 MB total.
- `p256_scalarmult` inlines `point_add`/`point_double`, which inline `fmul`/
  `fadd`/`fsub`, which... There is no shared `fmul` callable; it's textually
  expanded at every site.

This is not a *size* problem (5 MB on a machine is nothing — the author's call,
and it's correct). It is a **composition** problem: there is no way to write
`fmul` once and have `point_add` *call* it. Today "reuse" means "the generator
re-emits the same lets."

## 2. Why this is the real future-facing question (not size)

The size win from looping (recursive `ninv`: 11.2 MB → 84 KB; recursive
`x25519_finish`: 1.3 MB → 42 KB) was free and proved a point, but it does NOT
give reuse — a recursive `fmul` would still be one binary's private code, not a
shared function other rules call. The two are different axes:

| | mechanism | gives reuse? | shipped? |
|---|---|---|---|
| loop a primitive | recursion (`decreasing : j`) | no | yes (5.0–5.4) |
| share a primitive | structured-value return between rules | **yes** | **no** |

The thing that's missing is the second row. It matters for three reasons, in
increasing order of importance:

1. **Deduplication.** One `fmul` callable, called by point ops, scalar field,
   ECDSA. One `sha256` callable, called by every KDF. The ~5 MB collapses as a
   side effect — but that's the least interesting benefit.
2. **A real standard library.** Today `field_emit.py` / `p256_field.py` are
   *generator-side* Python that emits lets. A composition ABI lets the *language*
   have reusable `.verbose` functions — `fmul`, `sha256`, `hmac` as callable
   rules other `.verbose` files import and call. That's the difference between
   "the generator copies code" and "Verbose has libraries."
3. **Self-hosting — but NOT via sret (this was the note's original error, now
   corrected).** A Verbose compiler in Verbose must compose functions that pass
   structured values — AST nodes, tokens, symbol-table entries. The first draft
   of this note claimed that needs the sret machinery below. **It does not.**
   Native ALREADY returns arena-backed recursive types (sum types declared in a
   `concept_group`) from one rule to another, as a single i64 **arena index** in
   rax — shipped, exercised by `examples/label_tree.verbose` (`build_tree(seed)
   -> LNode` returns a tree; `total_label_length(node)` recurses passing the
   matched `left`/`right` indices across real `call`s). The carve-out lives at
   `src/native.rs:446-455`. So the AST/self-hosting composition case is served by
   the EXISTING Number-return ABI (an index is just a number) plus Phase B's
   arena — no new ABI needed. The sret machinery below is therefore NOT a
   self-hosting prerequisite; it's needed only for the two cases in §4.

## 3. What's already shipped (so this note doesn't reinvent it)

Per `native-call-convention-design.md` and the CLAUDE.md native table, slices
5.0–5.4 shipped:
- `call`/`ret` ABI for self-recursive and mutually-recursive rules.
- Argument passing: single field in a register; **multi-field input via the
  pointer-in-rdi fields-struct (Option A)** — so a callee CAN already *receive* a
  structured input.
- Returns: **Number / Bool** (rax), **Text** (rax=ptr, rdx=len).

### 3.1 This is a native + WASM codegen gap, NOT a language gap

The interpreter (`--run`) already composes everything freely: its `Expr::Call`
arm recursively evaluates the callee on any value, structured or not. `fmul(a,
b)` returning a field element already **parses, verifies, and runs** under
`--run`. Composition is purely a *native* (and *WASM*) codegen limitation — the
language and verifier are fine. WASM has the identical gap (`src/wasm.rs` only
inlines `Expr::Call`: refuses callees with bindings, no recursion, no structured
return). So any sret/composition decision must cover BOTH native and WASM, and is
correctly understood as "make two backends do what the interpreter already does,"
not "add a language feature." That makes the problem materially smaller than a
language change.

### 3.2 The native wall, precisely

The wall is narrower than "cannot return structured values" — native CAN return
an arena-backed recursive type by index (see §2, point 3). What native **cannot** return
from a callable is:
- a **flat field element** (10 limbs not in an arena) — so `fmul(a, b) -> <field
  element>` is not expressible as a shared callable;
- a **Record** or **Result** by value (sret).

`src/native.rs:446-461` encodes exactly this: group-concept output is allowed
(i64 index in rax); otherwise output must be Number/Bool/Text, and Record/Result
are refused ("later slices"). So the composition gap is specifically: **flat
field-element reuse** (the crypto case) and **Record/Result return** — NOT the
arena/AST case, which already composes.

This matters because it reshapes the whole motivation: the consumer that looked
like it would force this work (self-hosting) doesn't need it. What's left is
crypto reuse (a nice-to-have, §5 says skip) and Record/Result composition (build
when a consumer appears).

## 4. The core design choice: how a rule returns a structured value

The companion doc already sketched these as future slices 5.5/5.6. This note
fleshes out the field-element case (the crypto-relevant one) and the trade-offs.

### 4.1 Option A — sret (caller-allocated destination, pointer in rax)

System V's struct-return rule. The caller allocates a destination buffer (on its
stack frame), passes its address as a hidden first argument (or reuses the rdi
fields-struct convention), the callee writes the N limbs there and returns the
pointer in rax.

- **Pros:** scales to any size (10 limbs, a 32-byte hash, a multi-field record);
  one uniform convention for field elements, Records, and Results; mirrors the
  fields-struct *input* convention already shipped (symmetric, easy to audit —
  "structured values cross the call boundary by pointer, in and out").
- **Cons:** the caller must manage the destination buffer's lifetime; a chain
  `point_add` → `fmul` → ... nests these buffers (each call frame holds its
  callee's result buffer). Needs a clear frame-layout discipline so the buffers
  don't alias. This is the standard C compiler problem; tractable but it's where
  the bugs would live.

### 4.2 Option B — fixed return registers for small structured values

For a field element that fits in a few registers... it doesn't (10 limbs ≫ 6
registers). Text already uses the 2-register (rax, rdx) pair. A field element
can't ride registers. So Option B only ever covers Text and tiny tuples; field
elements/records force Option A. **Conclusion: Option A is the only one that
covers flat field elements and Records/Results** (the arena/AST case is already
handled by index-in-rax, §2 point 3). Keep the (rax, rdx) text pair as the
special small case it already is; everything bigger is sret.

### 4.3 The hard part — it's not the ABI, it's the lifetime/frame discipline

Returning 10 limbs by pointer is easy. The hard part is composition *depth*:
`scalarmult` calls `point_add` calls `fmul` calls `fadd`. Each callee needs a
destination buffer that outlives the call but not the caller. In an inlined world
this is free (everything is one flat let-chain the optimizer SSA's). In a
called-function world, the caller must reserve a result slot per outstanding
callee, and the recursion (`scalarmult` is recursive) multiplies the frames.
This is the genuine new machinery — not the `call`/`ret`, which is shipped, but
the **multi-buffer frame allocator** for nested structured returns.

This is also where the **CPU-overhead** question (the author's standing rule)
bites: an inlined `fmul` is ~780 lets the optimizer can fold across; a *called*
`fmul` is a `call` + a memory write of 10 limbs + a `ret` + the caller reading
10 limbs back. For a hot path doing millions of `fmul`s (a scalarmult), that
per-call memory traffic is NOT obviously negligible — unlike the recursion
call/ret we measured for `ninv`/`x25519` (which added selects, not memory round
trips). **This needs measurement before committing**, because it could trade size
for real CPU cost — exactly the trade the author said to flag, not assume.

## 5. The honest recommendation

**Don't build this now.** Reasons:

1. The size problem it would solve isn't a problem (author's correct call).
2. The reuse it enables has no *current* consumer demanding it — the generators
   copy code and that works for the crypto arc.
3. It carries a real, unmeasured CPU risk on hot paths (§4.3) that violates the
   "no CPU overhead" rule unless proven otherwise.
4. **The consumer that looked like it would force the issue — self-hosting —
   does NOT need it.** Arena-backed AST nodes already compose by index over the
   existing Number-return ABI (§2.3, §3.2). This removes the strongest reason to
   build speculatively.

So what's actually left that sret would serve is narrow: (a) flat field-element
**crypto reuse** (one shared `fmul`), and (b) **Record/Result** return by value.
Neither has a consumer demanding it today.

### Three options, named honestly

- **Option A — sret (build it).** The only convention that covers flat field
  elements and Records/Results; symmetric with the shipped fields-struct input;
  reserved as companion slices 5.5/5.6. Cost: the nested-result frame allocator
  (§4.3) is new attack surface, and the hot-path memory traffic is unmeasured.
- **Option C — keep reuse generator-side (maybe the right permanent answer).**
  The generators (`field_emit.py`, `p256_field.py`) already act as macro
  expanders: write the primitive once in Python, emit it inline everywhere. The
  native/WASM backends stay minimal and fully inlined — which *aligns with the
  small-auditable-surface pillar*. "Duplication in the binary" is the cost; "no
  call ABI, no frame allocator, no new attack surface, every callee visible at
  its use site" is the benefit. This is not just a stopgap; for a security-first
  backend it may be the correct end state, with reuse living in the (non-trusted)
  generator rather than the (trusted) emitter.
- (Option B — return registers — is ruled out for anything bigger than the
  existing Text pair; see §4.2.)

**Recommendation:** don't build A now. If/when a real consumer for Record/Result
composition appears (not self-hosting — that's covered), measure the §4.3
hot-path cost first, then choose A vs C *for that consumer*. For crypto reuse
specifically, C (generator-side) is probably the right answer permanently —
inlined+SSA-folded field ops are the fast path, and the duplication is harmless.
Treat A as "reserved, build against a pinned consumer," not "inevitable."

## 6. Filter check (the five pillars + the axiom)

- **Verifiability:** unchanged — a called `fmul` is verified once, at its
  definition, instead of at every inlined site. Arguably better.
- **Exploitability:** the whole point — declarations (a rule's signature) become
  a reuse boundary the compiler exploits.
- **Safety:** the sret frame discipline (§4.3) is the new attack surface; nested
  result buffers must not alias. This is the thing to get right.
- **Traceability:** SLIGHT degradation, same as the recursion slices already
  accepted — a called `fmul` is one `call <addr>` the auditor follows once,
  instead of the body inline. The companion doc already argued this is acceptable
  (one fixed location beats N copies for review).
- **Readability:** no new `.verbose` syntax — a rule calling `fmul(a,b)` already
  parses; today it's inlined, tomorrow it's called. Source is unchanged.
- **Compiler axiom (controls + applies, never guesses):** intact — the decision
  "inline vs call" is mechanical (cyclic ⇒ call; or an explicit `@no_inline`),
  never a heuristic. No guessing introduced.

## 7. Open questions for whoever picks this up

1. **Measure first:** benchmark a called `fmul` vs inlined `fmul` inside a
   scalarmult. If the per-call memory traffic is >a few % of total, the hot
   crypto path should stay inlined and composition is for cold/structural code
   only. This decides whether composition is universal or selective.
2. **Inline-vs-call predicate:** cyclic-only (shipped), or add `@no_inline` for
   reuse, or auto-call when a rule is referenced N+ times? The companion doc's
   §3.1 leans cyclic-only; reuse would want one of the others. Needs the real
   consumer to decide.
3. **Which structured types cross the boundary first?** Field-element (10 limbs,
   crypto reuse) and Record (by-value, a non-arena record output) both need sret;
   they share the mechanism but differ in field typing. Note the AST case is NOT
   in this list — it's arena indices, already done (point 4). So whichever sret
   consumer appears first (crypto reuse vs a Record-returning rule) drives the
   field-typing details; there's no self-hosting pressure forcing the order.
4. **The arena already closes the self-hosting case — verified, not
   speculative.** Phase B recursive types (`concept_group`) store self-references
   as zero-extended 16-bit arena indices (`src/native.rs:2733-2734, 2748`); a
   group-concept input resolves to one i64 index slot (`:3112-3128`); a
   group-concept output returns one i64 index in rax (`:446-455`). This is
   shipped and exercised: `label_tree.verbose`'s `build_tree` returns a tree and
   `total_label_length` recurses passing `left`/`right` indices across real
   `call`s. So a self-hosting compiler whose AST nodes live in the arena composes
   functions over the EXISTING Number-return ABI (an index is a number) — no sret
   needed. The remaining open question is only whether the crypto hot path should
   pay sret's memory traffic (§4.3) or stay generator-inlined (Option C); for AST
   composition there is no open question.
