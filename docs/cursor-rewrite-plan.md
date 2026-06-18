# Full cursor rewrite — implementation plan (pre-implementation framing)

Status: **PLAN, not implemented.** Frames the full cursor rewrite (capacity arc slice C1) at the
site level so the implementation is mechanical and the investment is a known quantity. Built from
the disk inventory of `examples/vexprparse.verbose`. Companion to docs/self-hosting-capacity-design.md.

## Goal & one-line model

Kill the parser's O(N²): every peek today is `peek_X(Nth { lst: <state>.toks, n: <pos> })` →
`drop_cells` walks `n` cons cells **from the head** (O(n)), ~13 peeks per item ⇒ O(N²). The fix:
**a parse state carries the cons CELL at the current position, not an absolute `pos : number`.**
- peek = `head_tok(cell)` (O(1))
- advance-by-k = `tail^k(cell)` (O(k), k small/fixed)
- a sub-parse returns the cell at its end (so every `*Parsed` result's `next` field becomes a cell)

**No dual representation, no invariant.** `pos : number` is REPLACED by `cell : TokenList`, not
carried alongside it. (Carrying both is the failed "memoization" path — its invariant is the trap;
clean replacement has none.)

## Scope — exactly what changes (and what does NOT)

IN (the parser token-position layer):
- **6 parser-state concepts** (`pos : number [0,512]` → `cell : TokenList`): `ParseState` (1469),
  `FoldState` (1519), `FieldState` (1640), `ProgramState` (4690), `ParamParseState` (4133),
  `FieldParseState` (4935).
- **6 result types** (`next : number` → `next : TokenList`, the end cell): `Parsed` (223),
  `ParsedArgs` (230), `ParsedBlock` (291), `ParsedRule` (352), `ParsedParams` (377),
  `ParsedConcept` (488) + their 6 `*_next` accessors.
- **The 11 `peek_*` helpers + `Nth`/`drop_cells`**: replace the `Nth{lst,n}` indexing layer with a
  cell layer. `peek_X` takes a `TokenList` cell directly; `head_tok(cell)` stays; `drop_cells`/`Nth`
  are retired from the parser hot path (a small `advance(cell, k) = tail^k(cell)` replaces them).
- **~124 position-usage sites**, by bucket (counts from inventory): 27 advance-by-k, 11 pass-through,
  15 bounds/end checks, ~60 absolute-lookahead peeks, 1 hard computed-position, ~10 conditional/value.

OUT (unaffected — do NOT touch):
- **Tokenizer source-byte positions** `ScanState.pos [0,256]`, `LineState.pos [0,256]` — these index
  SOURCE BYTES via `byte_at`, not tokens. Their O(N) is the tokenizer's (C2/depth), a separate arc.
- **Spans / diagnostics / pretty-printer**: `span_start/len`, `name_start/len`, `fstart/flen` are
  SOURCE byte offsets (`substring(src, …)`), confirmed independent of token positions. Zero changes.
- **`tok_*` token accessors**: operate on a Token value (`head_tok(cell)` still provides it). Zero changes.
- **The AST, the 5 analyses, the type checker, the interpreter, the emitter**: consume the AST, never
  `Parsed.next`/`ParseState.pos` (verified: zero reads past ~line 5400). Zero changes.

## Mechanical transform per bucket

| Bucket | Today | After | Sites |
|---|---|---|---|
| **4a advance-by-k** | `pos: st.pos + k` | `cell: advance(st.cell, k)` | 27 (parser ones; the source-byte `s.pos + 1` in scanners stay) |
| **4b pass-through** | `pos: rhs_next` (= `parsed_next(child)`) | `cell: parsed_next(child)` (now a cell) | 11 |
| **4c bounds/end** | `if pos >= len then …` / `n == 0` | `match cell: Nil => end ; Cons(h,t) => …` | 15 (parser ones; scanner byte-checks stay) |
| **4d absolute lookahead** | `Nth { lst: st.toks, n: st.pos + k }` | `advance(st.cell, k)` (k=0 ⇒ `st.cell`) | ~60 |
| **4e hard computed-pos** | `endp = body_idx + 4*nfields` (5139) | see below | 1 |
| **4f conditional advance** | `close_next = if c then p+1 else p` | `if c then advance(cell,1) else cell` | ~10 |

`peek_X` rules change signature from `(arg : Nth)` to `(cell : TokenList)`, body `tok_X(head_tok(cell))`.
Each of the ~60 call sites passes a cell (`st.cell`, or `advance(st.cell, k)`) instead of building `Nth`.

## The one hard case (line 5139) — fix by making `parse_fields` return its end cell

`parse_concept_decl_pos` computes `endp = body_idx + 4 * field_list_len(fl)` — the end position by
COUNTING fields × 4 tokens/field, not by walking. With cells there is no number to compute. **Fix:
make `parse_fields` return a `ParsedFields` result `(fields : FieldList, next : TokenList)`** — the
cell after the fields block — exactly like every other sub-parser returns its end. Then
`parse_concept_decl_pos` reads `next` from that result instead of arithmetic. This removes the only
non-mechanical site and is strictly cleaner (it also removes the fragile "4 tokens/field" assumption
the R1 work introduced). Small added surface: one result concept + accessor, mirroring `ParsedParams`.

## Migration strategy — big-bang within the parser, suite as the gate

The shared types (`Parsed`, `ParseState`) change together, so the parser layer migrates as ONE
coordinated rewrite (the suite is red mid-migration, green at the end). This is acceptable because:
- The blast radius is **self-contained to the parser** (nothing downstream reads positions), so "red"
  is confined and the 410-test suite is a sharp, fast correctness gate (identical ASTs ⇒ green).
- The transform is mechanical and bucketed; a coordinated pass over ~124 tagged sites is tractable.

Recommended order within the rewrite (minimizes thrash):
1. Add `advance(cell, k)` helper + change the 11 `peek_*` to take a cell. (Compiles standalone.)
2. Change the 6 result types `next : number → TokenList` + their 6 accessors.
3. Change the 6 parser-state concepts `pos → cell`.
4. Convert the ~124 sites bucket-by-bucket (4a → 4b → 4c → 4d → 4f), then the hard case (parse_fields
   returns end cell), then `parse_concept_decl_pos`/`parse_rule_decl_pos` end handling.
5. Retire the now-unused `Nth`/`drop_cells` from the parser (keep if any non-parser user remains —
   the inventory shows `drop_rules`/`param_nth_type` use an `n==0` index pattern on OTHER lists; those
   are list-index helpers unrelated to token positions and stay).
6. Update each rule's `@source`/proofs/termination as fields change types (mechanical).

Alternative (only if incremental-green is mandated): introduce parallel cell-based types
(`ParseStateC`/`ParsedC`), migrate bottom-up, delete the old — doubles type surface temporarily, more
total edits. NOT recommended; the big-bang with the suite gate is cleaner for a self-contained layer.

## Gate (when implemented)

- **Correctness**: `cargo test --release -- --test-threads=1` green (currently 410). The parser
  produces BYTE-IDENTICAL ASTs — every existing parse/eval/check/emit test passes unchanged. This is
  the proof the rewrite preserved behavior.
- **Verify**: `cargo run -- examples/vexprparse.verbose` still "all proofs check out".
- **Performance (the point)**: on a throwaway copy with position bounds raised (the probe harness:
  `sed 's/\[0, 512\]/[0, 300000]/g'`), `count_rules` on K=200/400/800/1600/3200 rules must show the
  time curve FLATTEN to ~linear (vs today's quadratic 0.37→1.12→abort). This is the measured win.
- Note: the committed file keeps `[0,512]` bounds (C3 raises them separately); the cursor change is
  what MAKES raising them useful — but C1 lands first, proven on the harness.

## Effort & risk (honest)

- **Effort**: ~124 mechanical sites + 6 concepts + 6 result types + 11 helpers + 1 hard case. A
  focused **1–2 day** coordinated rewrite, dominated by careful site conversion + suite iteration +
  the perf measurement. Not parallelizable across people cleanly (one coherent type change).
- **Risk**: a missed site or a wrong `advance(k)` silently mis-parses. Mitigations: (1) the 410-test
  suite catches behavioral drift sharply (identical-AST gate); (2) bucket-by-bucket conversion with a
  build after each bucket localizes errors; (3) the hard case is removed structurally (parse_fields
  returns its cell), not hacked. The `tok_*`/span/AST independence (verified) means the blast radius
  cannot leak past the parser.
- **What C1 does NOT do**: it kills the O(N²) *time*; the O(N) call *depth* (tokenize/parse_program
  non-tail recursion, ~57k frames) remains — that's C2. C1 alone makes mid-size inputs fast; full
  self-source still needs C2 (depth) + C3 (bounds, partly dissolved since token-position number
  fields disappear). Sequencing unchanged: C1 (this plan) → C2 → C3.

## Bottom line

The full cursor is a **bounded, mechanical, self-contained 1–2 day rewrite** of the parser's
position layer — ~124 tagged sites, one structurally-removed hard case, a sharp identical-AST suite
gate, and a measured perf win on the probe harness. No language change, no backend change, no
downstream ripple. It is the load-bearing piece of self-hosting capacity; this plan makes it a
known quantity to schedule rather than an open-ended risk.
