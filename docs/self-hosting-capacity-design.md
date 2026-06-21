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

---

## C1 scoping outcome (2026-06-16) — there is NO lean version; the O(N²) fix is the full cursor rewrite

Scoped C1 (incremental memoization) before implementing, per the review's de-risking. Result, from
disk: **incremental memoization is NOT leaner than the full cursor — same blast radius.** Why: every
peek is `peek_X(Nth { lst: <state>.toks, n: <abs> })` — `lst` is always the list HEAD; positions
flow as **bare numbers** reconstructed by arithmetic (`pos + 1`, `lhs_next + 1`, `first_next`), and
`Parsed.next` carries only a number, so the cons-cell at a position is DISCARDED when a sub-parse
returns. To set `cursor_cell` for a new `pos` you must walk to it — the exact `drop_cells` cost C1
exists to kill. The only escape is to make the cell TRAVEL with the position: add a cell field to all
**12 `Parsed::Mk`** sites, add a cursor to all **16 `toks`-carrying concepts**, convert ~30
`+k`-arithmetic sites — i.e. the full cursor rewrite (measured: **16 concepts, 42 `ParseState{` +
35 `ProgramState{` + … = 118 construction sites, 76 `Nth` sites, 12 result-type sites**). Multi-day,
not a slice. **Killing the O(N²) has no shortcut.**

Two corrections this scoping established:
- **`append_toks` is O(N) total, not O(N²)** (it walks each line's lexemes once; sum = total tokens).
  It contributes DEPTH (non-tail, per-line-deep), not quadratic time.
- **The measured O(N²) TIME is the parser peeks** (`drop_cells`-from-head in the expression parser
  AND in `parse_program`/`parse_rule_decl_pos`) — C1's target. So **C2 (tail builders + `append_toks`)
  fixes the DEPTH wall but NOT the primary TIME wall.** Only the full cursor (C1) flattens the O(N²)
  the probe measured. C2-first gives no observable end-to-end speedup (large inputs stay O(N²) slow).

### Honest sizing of the whole capacity arc (the real takeaway)

There is **no cheap first slice that delivers an observable self-hosting-capacity win.** The chantier:
- **Full cursor rewrite** (O(N²)→O(N) parser time) — 16 concepts / 118 sites / 12 result-types,
  multi-day, with a dual-representation invariant to hold at every site. The load-bearing piece.
- **Tail-recursive builders + drop `append_toks`** (O(N) call depth → O(1)) — contained to ~6 builder
  rules; a real prerequisite for deep inputs, but no speedup without the cursor.
- **Bounds story** (raise position/index fields to self-scale + dead-branch re-audit) — small.
- (then a transversal backend-TCO slice, once the builders are tail-recursive).

The method's value here: it sized the capacity arc accurately as a **multi-day parser-representation
rewrite with no shortcut**, rather than letting it be mistaken for a quick win. Whether/when to invest
is a deliberate call — the arc is real and correct, but it is not small, and none of its pieces are.

## DISCOVERY (2026-06-21) — the O(N²) was the TOKENIZER, not the parser. The whole arc mis-attributed it.

The cursor rewrite (C1) was implemented and LANDED (commit 4d5c14e): the parser now navigates by
cons CELL, peeks are O(1), `drop_cells`/`Nth`-from-head are gone, suite 411 green, runs natively
(after fixing two native backend bugs it exposed — an r11/arena-base clobber in the concat-with-Call
path, and a HashMap-iteration nondeterminism; both real, both general, both in src/native.rs).

**But the perf goal was NOT met — and profiling revealed why the entire arc (and every strategic
review) had mis-attributed the wall.** Measured curve after the cursor rewrite, all bounds raised:

| K rules | tokenize-only (`count_cells_src`) | full `count_rules` |
|---|---|---|
| 100 | 0.023 s | 0.021 s |
| 200 | 0.098 s (4.3×) | 0.084 s (4.0×) |
| 400 | 0.404 s (4.1×) | 0.377 s (4.5×) |
| 800 | 1.778 s (4.4×) | 1.454 s |

**Tokenize-ONLY is equally quadratic** (~4× per doubling) — essentially the entire cost. So the
dominant O(N²) is the **tokenizer**, which the cursor never touched. The parser peeks (what C1, the
records-arc review, the arena-arc review, and the capacity design ALL fingered as the wall) were
never the dominant term.

**Root cause:** 12 per-character recursive scan rules (`skip_spaces`, `digit_run`, `ident_run`,
`num_value`, `str_content_len`, the scanners, `tokenize`, `tokenize_line`, `line_width`,
`next_line_start`) each do `let len = length(s.source)` — and `length(<text input field>)` is an
**O(N) `emit_strlen` scan** (native.rs:914 "length is recovered at read sites via emit_strlen"). Each
of these rules RECURSES per character, recomputing `length(source)` every step → **N chars × O(N)
per char = O(N²)**. This O(N²) was in the tokenizer the WHOLE time, pre- and post-cursor.

**The lesson (the article): profile, don't assume.** Five strategic-review/scoping passes correctly
killed three mis-aimed arcs — but all of them, and I, reasoned the O(N²) was the parser's
`drop_cells`. It took isolating tokenize-only with a profiler-grade measurement to see the truth.
Static reasoning about asymptotics is not a substitute for measuring each phase. The parser's
`drop_cells` IS O(N²) by construction (so the cursor is a real, necessary fix — its O(N²) would
resurface once the tokenizer is linear), but it was MASKED by the tokenizer's larger constant +
equal asymptotic, so fixing the parser alone moved nothing observable.

### The real capacity arc, corrected

- **C1 (cursor) — DONE/LANDED.** The parser HALF. Necessary but not sufficient.
- **C-tok (NEW, the actual dominant win) — make the tokenizer's `length(source)` not O(N)-per-char.**
  Two options: (a) **Verbose-level** — thread the precomputed `len` through `ScanState`/`LineState`
  (compute `length(source)` ONCE, carry it; ~68 ScanState construction sites — mechanical but
  pervasive); (b) **native-level** — make `length(<text input field>)` O(1) by computing the field's
  strlen ONCE at load and carrying `(ptr, len)` through the recursive ABI (like BoundText already
  does for read/fetch/lets) — zero Verbose change, fixes EVERY `length(field)` call program-wide, but
  a real backend ABI change. (b) is the elegant general fix and likely the right one; it also makes
  the cursor's own arena-cell reads cheaper.
- **C-depth — tail-recursive tokenizer builders + bounds** (the earlier C2/C3), still needed for the
  3200-rule SIGSEGV (call depth) and the arena/position ceilings, but secondary to C-tok for time.

### CORRECTION (2026-06-21, second profiling pass) — the dominant O(N²) was NOT length(), it was string_run

C-tok above fingered `length(<text input field>)` as the O(N)-per-char term. **A disassembly +
bisection pass on `8b9c361` showed that diagnosis was already stale.** At that HEAD the native
backend ALREADY carries `(ptr, len)` for text input fields through the recursive ABI
(`callable_text_bindings` + the multi-field struct copy in `emit_callable_into`, and the marshalling
in `emit_eval_expr`'s Call arm), so `length(s.source)` reads a precomputed len slot in O(1). The
disassembly of `count_cells_src` (bounds raised) showed exactly **ONE** `repnz scasb` in the whole
binary, not the 81 the first DISCOVERY pass reported. The `length()` strlen was therefore NOT the
dominant term anymore; option (b) was effectively already done.

The real dominant O(N²), found by bisecting source shape (single-line dense → linear; multi-line →
quadratic; quadratic scales with **token count**, independent of indentation and of `length()`):

- `next_token` and `token_end` **eagerly** evaluate `let srun = string_run(...)` for EVERY token
  (eager-let trap — all lets in a callable run before the body regardless of which branch is taken).
- `string_run` → `str_content_len(pos+1)`, which scans forward looking for the **closing quote**.
- For a NON-string token there is no closing quote, so `str_content_len` scanned to **end of source**.
- N tokens × O(remaining source) per token = **O(N²)**.

**Fix (this session): guard `string_run` to be O(1) when the start byte is not `"` (34).** One rule
changed in `examples/vexprparse.verbose`:

```
rule string_run
  logic:
    let len = length(s.source)
    out = if s.pos >= len then 0 else if byte_at(s.source, s.pos) == 34 then 1 + str_content_len(...) else 0
```

Now `string_run` only scans when it's actually at a string opener; for the common non-string token it
returns 0 immediately. Measured (`count_cells_src`, bounds raised, best of 3):

| K rules | BEFORE (O(N²)) | AFTER (O(N)) |
|---|---|---|
| 200  | 0.099 s | 0.0029 s |
| 400  | 0.472 s (4.8×) | 0.0049 s (1.7×) |
| 800  | 2.355 s (5.0×) | 0.0064 s (1.3×) |
| 1600 | 9.06 s  (3.8×) | 0.0095 s (1.5×) |

Curve flattened from ~4-5×/doubling to ~1.3-1.7×/doubling. K=1600 dropped from 9.06 s to 9.5 ms
(~950×). Behaviour is byte-identical to the pre-fix tokenizer on every tested source (no-string,
with-string, unterminated-string, string-heavy) — the guard only removes the wasted scan, it never
changes a string token's measured boundary. Suite stays 411 green; no native binary sizes shifted
(the change is in an example, not the emitter).

**Lesson reinforced:** profile each phase, AND re-profile after each landed fix. The C-tok diagnosis
was correct as of the FIRST DISCOVERY snapshot but the recursive-text-field (ptr, len) ABI had since
made `length()` O(1); the wall had moved to the next eager-let scan. The `length()` ABI work the
design called "option (b)" turned out to already be in the backend — the remaining O(N²) was purely a
source-level eager-evaluation bug. The other eager scanners (`ident_run`, `digit_run`) are
self-limiting (they stop at the first non-matching byte), so `string_run` was the sole offender.

**Still open (C-depth):** `count_rules` exits rc=1 at ~K≥800 (arena / call-depth ceiling), unchanged
by this fix. `count_cells_src` handles K=3200 fine. The depth ceiling is the next wall, secondary to
the time win delivered here.

## C-depth bisection (2026-06-21) — the remaining wall is the ARENA, not call depth. The declined arena arc is now VALIDATED.

With the O(N²) gone (cursor + string_run fix), bisected what makes `count_rules` abort at K≥800.
Method: raised ALL position/index field bounds on a throwaway copy, measured exit codes.
- K=400/600/700: exit 0, correct, FAST (0.003–0.010 s, linear). K=800+: exit 1 (clean abort), FAST
  (~0.007 s — early abort, NOT deep work, NOT SIGSEGV).
- Remaining small bounds ([0,1]/[0,8]/[0,16]/[0,64]) are enum/kind codes, not positions — ruled out.
- **Direct test:** temporarily raised the arena cap (verifier `PHASE_B1_MAX_BOUND` + the `.verbose`
  `max_nodes`) 65535→120000. **count_rules then CLEARS 800 (exit 0, "800"); 1600 aborts (needs
  >120000).** So the wall is unambiguously the **arena `max_nodes`**, scaling ~85 nodes/rule
  (count_cells_src gives 8001 token cells at K=800; the indent tokenizer + AST push total nodes past
  65535 around K≈770). NOT call depth — no SIGSEGV at these K once the tokenizer is linear (the
  earlier 3200 SIGSEGV was the old tokenizer's stack use; now a clean arena-full abort). (Verifier
  hack reverted; cap is back at 65535.)

**This VALIDATES the arena-allocation arc (docs/arena-allocation-design.md) that was DECLINED as
mis-aimed.** It was correctly declined THEN — the O(N²) time walled first, so an off-stack arena
would have moved a wall nobody was hitting. Now that O(N²) is fixed, the arena IS the operative wall,
exactly as that design's "follow-on" note predicted. The fix is that arc, revived:
- **Raise the arena cap** (PHASE_B1_MAX_BOUND) — but it can't just be raised: the arena is
  STACK-allocated (`max_nodes × entry_size`). Self-source scale (~6000 rules × ~85 = ~500k nodes ×
  48 B ≈ 24 MB) blows the 8 MB stack. So:
- **Move the arena OFF the stack (mmap)** — the option-(b) design in arena-allocation-design.md
  (mmap above a threshold, base in a slot reloaded after syscalls, r9/r11 discipline). THIS is now
  the validated next slice for self-hosting capacity, no longer premature.

The walls fell in the right order once measured: O(N²) time (fixed) → arena capacity (now). The
arena arc's decline + revival is the cleanest vindication of "measure each wall in order."

## What this arc is NOT

- Not the arena-allocation arc (declined) — that fixed capacity nobody was hitting. This fixes the
  walls that are actually hit (time, then depth, then bounds).
- Not the records/grammar arc — orthogonal; this is about *scale*, that is about *what parses*.
- Not a rewrite of the language. The cursor is a representation change in the self-hosted parser;
  TCO is a backend optimization. Both stay within the existing semantics.
