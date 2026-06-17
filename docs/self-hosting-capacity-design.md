# Self-hosting capacity: killing the O(N²) and the depth wall (arc design, pre-review)

Status: **DESIGN, not implemented.** Input to a fresh-context strategic review before any code
lands. This is the arc the arena-allocation review (docs/arena-allocation-design.md, declined)
pointed to as the *actual* self-hosting-capacity wall, now backed by a measured probe.

## The measured wall (probe, 2026-06-16)

Raised the position field bounds on a throwaway copy of `vexprparse.verbose`, kept the arena
on-stack at 65535, fed a growing synthetic program to the `count_rules` driver:

| rules | ~tokens | exit | wall |
|---|---|---|---|
| 200 | 2,400 | 0 | 0.37 s |
| 500 | 6,000 | 0 | 2.98 s |
| 700 | 8,400 | 0 | ~3–5 s |
| 800 | 9,600 | **abort** | 13 s |
| 2,400 | 28,800 | abort | 69 s |

- **Time is quadratic** (200→500 rules = 0.37 s → 2.98 s ≈ 8× for 2.5×). vexprparse's own source
  is ~6,000 rules / 57k tokens — at O(N²), minutes-to-hours even if every other limit were lifted.
- It walls at ~700–800 rules / ~9k tokens, NOT at the arena cap (≈22k nodes ≪ 65535).

Two independent constraints, in priority order:
1. **O(N²) time** from `drop_cells` (vexprparse.verbose:896): every `peek_*` is
   `tok_kind(head_tok(drop_cells(arg)))` (1272) addressing the token by ABSOLUTE `ParseState.pos`,
   so peeking position P re-walks P cons cells. ~13 peeks/item × O(P) = O(N²). **This is the first
   wall — it makes self-scale infeasible by time before depth or nodes matter.**
2. **O(N) call depth** from per-token recursion: `tokenize` (862) builds the token cons-list
   `Cons{next_token, tail: tokenize(...)}` recursing once per token (~57k deep for the self-source,
   ~3 MB stack), and the parser's fold rules consume it recursively. Survivable at 57k, fatal well
   beyond, and fragile.
3. **A thicket of small position/index field bounds** (`[0,256]/[0,512]/[0,4096]`) that sys_exit(1)
   far below self-scale — and lifting them is not one knob (the probe raised several and still
   walled at 800). Plus the optimizer-default-range trap: an unbounded `number` field defaults to
   `[0, i32::MAX]`, which interval-arithmetic can mis-fold ([[feedback_optimizer_default_field_range]]).

## Slice arc (highest leverage first — the probe dictates the order)

### Slice 1 — the CURSOR refactor: O(1) token access (kills the O(N²)). Self-hosted-Verbose only, no backend change.
The parser threads an absolute `pos : number` and re-derives the current token by `drop_cells` from
the head on every peek. Replace it with a **cursor**: `ParseState` and `Parsed.next` carry the
current `TokenList` cons CELL instead of an index. Then:
- `peek` = `head` of the cursor cell (O(1)); lookahead-by-k = `head(tail^k(cell))` (O(k), k small/fixed).
- `advance` = `tail` (O(1)); a sub-parse returns the cell at its end.
- `drop_cells`/`Nth`-by-position disappear from the hot path; total parse work drops O(N²) → O(N).

**Viability verified from disk:** the parser's access is **monotonic-forward** — `parse_add` →
`parse_mul(st)` → `first_next` → `add_rest(pos: first_next)`; every position advances from a
sub-parse result, never jumps backward (the only `pos-1` uses are in the number-value scanner over
source bytes, not the token list). So a forward-only cursor suffices; no random access is needed.

Cost: pervasive but mechanical — every parse rule threads `next` as a number today and would thread
a cell. The `Parsed` concept's `next` field changes type (number → TokenList). Risk concentrated in
(a) the lazy block-`match` accessor split (`parsed_node`/`parsed_next`) and (b) any rule that does
arithmetic on `next` (e.g. `lhs_next + 1` becomes `tail(cell)`). Slice 1 does NOT touch tokenize or
the field bounds — it only changes how the parser navigates an already-built token list.

### Slice 2 — depth: tail-call optimization in the native backend (transversal). Backend change.
Even with O(1) peeks, `tokenize` and the fold-consumers recurse O(N) deep → SIGSEGV beyond ~100k
tokens and fragile at 57k. The fix is **TCO**: when a rule's body is a tail self-call (the recursive
call is the whole `else`/arm result, no work after it), the native backend reuses the current frame
(`jmp` to the body top after re-binding params) instead of `call` (a new frame). This makes
`tokenize`-shape per-element recursion run in O(1) stack. **This benefits EVERY recursive Verbose
program, not just self-hosting** — it's the transversal half. It's a real `src/native.rs` change
(detect tail position in the emit, re-bind params in place, `jmp` not `call`), gated on byte-identity
for non-tail-recursive rules and == interpreter for the tail-recursive ones. Sequenced AFTER slice 1
because the O(N²) time walls first; TCO on a still-quadratic parser buys nothing observable.

### Slice 3 — the bounds story: position/index fields at self-hosting scale.
With O(1) access (slice 1) and O(1) stack (slice 2), the position/index field bounds become the
operative limit. They must be raised to cover self-scale (tens of thousands) WITHOUT becoming false
explicitations: each bound stays mechanically enforced (the runtime bounds-check already
sys_exit(1)s on violation) and the optimizer-default-range trap is avoided by keeping explicit
ranges. Open question for the review: is there a single coherent bound (e.g. tie all position fields
to `max_nodes`) or must each be sized independently? And does raising them re-expose any optimizer
dead-branch fold that the current small ranges mask?

## Risks / unknowns (for the review to pressure-test)

1. **Is the cursor refactor's blast radius truly contained to the parser?** Count the rules that read
   `ParseState.pos` / `Parsed.next` arithmetically. If `next`-as-number is assumed in the analyses,
   the checker, the emitter, or the drivers (not just the parser), slice 1 ripples wider than
   claimed — quantify it.
2. **Lazy block-`match` constraint.** vexprparse notes a real language gap: block-form `match` is
   legal only in the final `out =` position, forcing the `parsed_node`/`parsed_next` accessor split.
   Does carrying a cell (vs a number) interact badly with that constraint anywhere?
3. **TCO correctness corners.** Mutual recursion (the SCC two-pass labels), multi-field
   pointer-in-rdi ABI, the runtime bounds-check at field load, the arena r11 base — does in-place
   param re-bind + `jmp` preserve all of them? TCO that corrupts a frame is the A2-segfault class of
   bug at larger scale. Is byte-identity for non-tail rules actually achievable, or does detecting
   tail position perturb the common path?
4. **Sequencing честность:** is slice 1 alone enough to make a *meaningful* self-input (not the full
   self-source) compile in reasonable time, giving an early measurable win before the harder slice
   2/3? Or is there no satisfying input between "toy" (works today) and "needs all three slices"?
5. **Is TCO the right depth fix, or an explicit work-stack / iteration primitive?** TCO is
   transversal and idiomatic but only helps *tail* recursion; `tokenize` is tail-recursive (good),
   but are the parser's fold-consumers tail-recursive too, or do they do work after the recursive
   call (making TCO inapplicable and forcing a different restructuring)? Check the actual recursion
   shapes before betting slice 2 on TCO.
6. **Cheap probe for slice 1** before the full refactor: prototype the cursor on the ONE hottest
   path (the expression precedence chain's peeks) and measure whether the O(N²) flattens, before
   converting every parse rule.

---

## Review outcome (2026-06-16) — RESHAPED. Targets right, first tools wrong.

Fresh-context strategic review (4th of the session; the discipline that declined the IR and arena
arcs). The two walls (time, then depth) are the right targets, but the first tool for each was
wrong. Verified from disk. The slices below are reshaped accordingly.

**Slice 2 (TCO) was MIS-AIMED — falsified.** The depth-generating recursions are ALL non-tail
(recursive call is a `Cons{...}` constructor argument, with the cons/arena-write after the call):
`tokenize` (873), `tokenize_line` (2947), `append_toks` (2978, also O(N)-per-line splicing),
`tokenize_indent` (3053, recursion buried two constructors deep), `cons_dedents` (2916),
`parse_program` (4874), `parse_concepts` (5204). TCO applies ONLY to the fold cycle (`add_rest ↔
add_join`, `mul_rest`, `and_rest`, `or_rest` — accumulator-passing, genuinely tail) — but those
recurse to depth O(operators-in-one-expr), which is SHALLOW and not the wall. Backend TCO would
deliver no observable improvement on the self-source. **The real depth fix is Verbose-side: rewrite
the list-builders into accumulator-passing tail-recursive form (build reversed + reverse once, or
append-via-cursor) AND eliminate `append_toks` (gratuitous O(N²) on top of depth) — THEN backend
TCO has a real target.** Sequence: builders-tail-recursive (Verbose) → backend TCO, not the reverse.

**Slice 1 (full cursor) was OVER-SCOPED — reshaped to incremental memoization.** The full cursor is
correctly parser-contained (zero `.next`/`.pos`/`parsed_next` reads past line ~5400 — the 5 analyses,
checker, interpreter, emitter consume the AST and source-byte spans, NOT parse positions), but it is
a representation rewrite (6 `*Parsed` result types' `next` field, ~15 threading concepts, 76 `Nth`
sites, ~30 arithmetic conversions incl. non-forward `+k` peeks in `parse_rule_decl_pos`/decl
parsers) — multi-day, not "mechanical." **The lean equivalent: keep `pos : number`; add
`(cursor_cell, cursor_pos)` to the threading state; compute each peek's `drop_cells(Nth{toks, n})` as
`tail^(n − cursor_pos)(cursor_cell)` instead of `tail^n(head)`.** Same O(N²)→O(N) win (access is
monotonic-forward, so `n ≥ cursor_pos` always), localized to the `peek_*` helpers + two fields on the
threading concepts, no result-type surgery. CAVEAT to verify first: a few decl-parser sites peek
base+1 then base+2 then descend — confirm no peek goes BACKWARD of the memoized cursor; fall back to
head for any that does.

**Slice 1's "prototype the cursor on just the hot path" de-risking step was ILLUSORY** — `Parsed`/
`ParseState` are shared types threaded through the whole parser; there's no seam to convert one path.
Replaced by the real probe below.

**Reshaped arc:**
- **C1 — incremental cursor memoization** (NOT full cursor). Kills the O(N²) peek cost. Lean,
  parser-only. Gate: existing suite green + measure the time curve flattens (the earlier probe's
  800/1600/2400-rule runs go from O(N²)/abort to O(N)). The implementation IS the probe.
- **C2 — Verbose-side tail-recursive builders + drop `append_toks`** (the real depth fix), THEN a
  separate backend-TCO slice (transversal) once there's a tail target. NOT backend-TCO-first.
- **C3 — bounds story** (smallest risk). Keep; ADD a post-raise dead-branch re-audit (widening a
  `[0,512]`→`[0,65535]` interval can flip which arm the optimizer folds at `if pos < k` sites).
- **C0 (optional) — instrumentation probe**: count peek-walk steps + max recursion depth on 3k/9k
  inputs to confirm C1 flattens peeks and the residual depth is `tokenize_indent`/`parse_program`
  (proving C2's target). Or skip and let C1's measured time curve be the proof.

Targets right (time → depth → bounds); first tools corrected (memoization not cursor; Verbose-side
tail-recursion not backend-TCO-first). Design above retained as the record.

## What this arc is NOT

- Not the arena-allocation arc (declined) — that fixed capacity nobody was hitting. This fixes the
  walls that are actually hit (time, then depth, then bounds).
- Not the records/grammar arc — orthogonal; this is about *scale*, that is about *what parses*.
- Not a rewrite of the language. The cursor is a representation change in the self-hosted parser;
  TCO is a backend optimization. Both stay within the existing semantics.
