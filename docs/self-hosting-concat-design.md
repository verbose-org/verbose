# Concat arc — slice 1: rope values in the interpreter (no heap)

## The refusal, revisited
`concat` builds FRESH text; a span (start, len) into the source can't represent it —
that's why every text slice so far refused it. The emitters tier (print_chain.verbose,
and ultimately vexprparse's own x86 rules) is gated on concat, so it's next.

## The insight: a rope IS an arena value — no heap needed
Fresh text doesn't need fresh STORAGE if it's never materialized: represent
`concat(a, b, c)` as a **rope** — a tree value whose leaves are existing text spans
and numbers:

- `VConcat of (left : Value, right : Value)` — one new Value variant (n-ary concat
  folds right into nested VConcats). Leaves: `VText` (span — zero copy), `VNum`
  (renders as its decimal text — print_chain's `Int(v) => concat(v)` needs
  number→text).
- This is exactly verbosec's own streaming-emitter insight (docs/
  emitter-streaming-design.md: never materialize, walk in order) applied to the
  interpreter's VALUE model. The rope is built of arena nodes the Value group
  already provides.

## Semantics (the primitives learn ropes)
- `length(v)`: VText → len; VNum → decimal digit count (negative: +1 for '-');
  VConcat → length(left) + length(right). Recursive.
- `byte_at(v, i)`: VText → source byte; VNum → the i-th byte of its decimal
  rendering (digit-at-position via pow10 math; '-' at 0 for negatives); VConcat →
  if i < length(left) then byte_at(left, i) else byte_at(right, i - length(left)).
  (length(left) recomputed per descent — O(depth·size) worst case; fine for the
  interpreter tier; note it.)
- `substring` on a rope: defensive VNum 0 for slice 1 (refused semantics — the
  emitters tier never re-slices built text; honest note).
- concat DISPATCH: in the AstCall arm beside byte_at/length/substring
  (span_is_concat), n-ary: eval each arg → fold into right-nested VConcats;
  0 args → empty VText{0,0}; 1 arg → the value itself (a number arg stays wrapped
  so length/byte_at see decimal semantics — wrap single args in VConcat(arg,
  empty)? simpler: represent 1-arg concat as VConcat(arg, VText{0,0}) so the
  rope-aware paths uniformly apply).

## Gate — byte-exact through the NUMBER drivers (no driver changes)
The result is probed via length + byte_at, so eval_main's number convention stands:
1. vexprparse verifies; suite green (currently 433 + 1 ignored) + a new test.
2. **MILESTONE — print_chain's shape evaluates** (the sum_chain AST + printer,
   driver main composing them):
   - `length(print_expr(build_chain(Seed { n: 3 })))` → **7** ("3+2+1+0").
   - byte probes: byte_at(result, 0) → **51** ('3'), 1 → **43** ('+'), 6 → **48**
     ('0') — byte-exact rendering without materializing.
   - basics: `length(concat("ab", "cd"))` → 4; `byte_at(concat("ab", "cd"), 2)` →
     99; `length(concat(42))` → 2; `byte_at(concat(42), 0)` → 52 ('4');
     `length(concat(0 - 7))` → 2, byte 0 → 45 ('-').
   - nesting: concat of concats (rope depth ≥ 3) probes correctly.
3. UNCHANGED: scanner/records/variant milestones + verbatim family (suite pins).

## Honest scope
Slice 1 = concat VALUES in the interpreter via ropes — print_chain's logic becomes
evaluable, byte-exact, zero heap. DEFERRED: rendering a rope to stdout (driver
output is numbers; the compiled-side equivalent is the STREAMING lowering — mirror
verbosec's own emitter-streaming design for text-returning SCCs, its own slice);
substring-on-rope; concat CODEGEN (the streaming slice covers it: compiled
text-returning rules write bytes in order, no rope at runtime). After slice 1 the
interpreter oracle covers the emitters tier's core shape.
