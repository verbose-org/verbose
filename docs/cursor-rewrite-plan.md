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

## Review outcome (2026-06-18) — core sound, surface undercounted. Corrections below.

Fresh-context strategic review (6th method pass of the session). Core thesis VALIDATED against the
code: **no dual representation needed** (0 backward token refs, 0 position-vs-position comparisons,
0 token positions stored into the AST — only source-byte spans are stored), advance is always
small-fixed-forward (`tail^k`, k≤3) except the one count site, the hard case is the ONLY one
(`parse_rule_decl_pos` correctly threads `pb_next`, doesn't count), and the suite is a real
end-to-end gate. **No fatal flaw.** But the plan undercounts its own surface — corrections (verdict
(b), implement-with-changes):

1. **15 threading concepts, not 6.** The plan listed only the 6 with an explicit `pos : number`
   field and silently omitted **9** that carry `toks : TokenList` and derive their position from a
   child via `parsed_next`: `JoinState`, `WrapState`, `LetJoinState`, `ParenState`, `ArgConsState`,
   `CallState`, `IfThenState`, `IfElseState`, `RuleParseState`. Once navigation is by cell, their
   `toks` field goes DEAD — and a dead field violates CLAUDE.md's anti-decoration rule, so it must be
   DELETED (recommended) — adding their ~21 construction sites + re-proofs to the surface. Not zero-edit.
2. **Two position-PRODUCER rules unaccounted for:** `skip_seps` (3351) and `skip_seps_dedent` (4717)
   return `out : number` token positions (used pervasively, e.g. `skip_seps(...) + 2` at 5131/5136).
   They are a third category the buckets don't name; each must be rewritten to RETURN a cell and walk
   forward over cells. Straightforward but real.
3. **~75 token-`Nth` peek sites, not ~60** (`grep "lst: …toks"` = 75). The ~124 position-usage total
   is roughly right; the per-bucket lookahead count was ~25% low.
4. **The `parse_fields` end-cell fix is a small rule rewrite, not trivial:** thread the end cell UP
   through every `FCons` (today the cell exists only in the `FNil` arm) + a 7th result concept +
   accessor. Low risk (mirrors `parse_stmts`), but real. Reassurance: the current `4*nfields` is
   deliberately approximate (trailing Dedents absorbed by the caller's `skip_seps_dedent`); the exact
   end cell lands identically after that absorption — AST stays identical.
5. **Suite gate has one thin spot:** the `4*nfields`-sensitive path can't currently diverge (the
   self-hosted `type_code_of_span` only knows single-token types `number`/`bool`/`text`, so no
   multi-token field type exists to expose a `parse_fields`-cell bug). Moot today, but **ADD one test
   BEFORE starting**: drive `concept_field_count` on a 3-field AND an empty-`fields:` concept to pin
   the FCons-recursive and FNil-base end-cell threading explicitly.
6. **Re-estimate: 2–3 days, not 1–2** — the 9 vestigial-`toks` concepts each force a
   proof/`@source`/construction-site sweep, and a big-bang red-mid-flight over ~118 sites usually
   needs 2+ debugging passes.

Corrected scope: 6 result types + 6 accessors + **15** concepts + **2** skip-producers + **75** peek
sites + 27 advance + 11 pass-through + 15 bounds + 1 hard case ≈ 118 construction sites (matches the
scoping doc). Pre-test (item 5) lands FIRST. Otherwise the plan stands.

## Implementation attempt outcome (2026-06-18) — language-level DONE, blocked by a native backend bug

First implementation pass executed the FULL transform per the corrected plan. Result:
- **The language-level rewrite is COMPLETE and interpreter-VERIFIED.** Verifier accepts it (all
  proofs check out), and the interpreter produces correct results — `count_concepts` on a 1-field
  concept → 1, `shape "1+2*3"` → 20, `eval_expr "2+3*4"` → 14 via `--run` — **proving the ASTs are
  byte-identical** (the transform preserved behavior). All buckets/concepts/sites/hard-case done.
  One refinement to the review's scope: `RuleParseState.toks` was NOT dead (parse_rule_decl
  navigates absolute indices 0/1 from it) — renamed `toks`→`cell` rather than deleted. So 14 concepts
  edited (6 pos→cell, 8 toks-deleted, RuleParseState renamed), not "9 deleted."
- **BLOCKED by a NATIVE backend codegen bug (out of the rewrite's scope).** 10 vexprparse-driven
  NATIVE tests abort (exit 2, in `_start` prologue, before any syscall); verifier + interpreter
  confirm logic/ASTs are correct, so it is purely native codegen. Isolated: a throwaway probe
  replacing the recursive `advance` with inline `tl(tl(...))` FLIPPED which programs abort — pinning
  the defect to **a recursive helper callable that takes/returns a `TokenList` group-ref and is
  pulled into the parser's ENTRY SCC** (the `(group-ref, number)` ptr-in-rdi shape the old
  `drop_cells` also had — but `drop_cells` was only called from non-recursive leaf peeks, so it never
  entered the SCC; the cursor model puts the forward-walk helper deep inside the recursion cycle).
  The SCC detection / `emit_self_recursive_program` two-pass labels / pointer-in-rdi marshalling
  mis-compile it.
- Tree reverted clean (back to the pre-test checkpoint, suite green) — the verified transform is
  re-derivable (mechanical) once the native bug is fixed.

**Corrected sequencing (the native bug is the true blocker):**
1. **Native backend fix FIRST, as its own slice/PR, on the current tree.** Minimal repro: add a
   recursive `rule advance(Advance{lst:TokenList, k:number})` and CALL it from inside an existing
   recursive parser rule (e.g. parse_primary), then `--native --run shape` → exit(2). Debug
   `collect_scc_containing` (is `advance` wrongly pulled into / wrongly excluded from the entry SCC?),
   the two-pass label sizing in `emit_self_recursive_program`, and the group-ref ptr-in-rdi
   marshalling for a callable that is BOTH recursive AND called from other SCC members. Land it + a
   native regression test.
2. **Re-apply the cursor rewrite** (mechanical, proven AST-identical by the interpreter this pass).
3. **Perf measurement** (count_rules K=200/400/800/1600 on the raised-bounds harness) — only
   meaningful once native binaries run.

The rewrite is no longer the unknown; the **native backend bug** is the real remaining blocker.

### Diagnosis refinement (2026-06-18 isolation pass) — root cause NOT yet definitively pinned; needs the failing binary

Tried to reproduce the native abort in ISOLATION (minimal concept_group programs) to fix it. **Five
faithful-looking repros ALL compiled and ran correctly** natively: (1) recursive group-ref-returning
helper (`advance`-shape); (2) mutual-recursion SCC (`ping↔pong`) passing a group-ref, calling the
helper from inside the SCC; (3) read-only recursive walk (`sum_lst`); (4) build-only recursive
(`build`); (5) read+build+recurse in one body (`maplist` — MatchVariant + VariantConstruct +
recursion, the parser shape). None abort. So the trigger is specific to the FULL cursor parser's
structure, not any simple shape.

Confirmed facts (from disk): `exit(2)` is the **MatchVariant tag-corruption trap** (native.rs:5373/
5858) — it fires NATIVELY but the interpreter runs the same ASTs correctly, so it's a native-only
**wrong-tag read** (reads an arena entry whose tag matches no arm). The earlier **r11-clobber theory
is WEAKENED**: grep shows r11 is **never used as scratch** (only `add rax, r11` arena-base reads at
:5733/:13866), and the comments confirm r11 is clobbered **only by syscalls** — but the cursor parser
makes **no syscall mid-parse** (it returns a number, printed in `_start` after the rule returns). So
there is no mid-parse r11 clobber to corrupt the base. The wrong-tag read is therefore more likely a
**cell-index marshalling bug** (a group-ref index passed wrong through a specific recursive-call
shape) or an **arena-write ordering bug** unique to the cursor parser — NOT a generic r11 loss.

**The fix needs the actual failing binary.** Next-slice plan: re-derive the cursor rewrite on a
throwaway branch and KEEP the red state (do NOT revert this time) → disassemble one failing entry
(`count_rules`) → find the exact MatchVariant site reading a wrong tag and trace back whether the
arena INDEX or r11 or the entry's written tag is wrong → fix in src/native.rs + a native regression
test (built from whatever minimal shape the disassembly reveals actually triggers it) → re-apply the
cursor rewrite (mechanical, proven AST-identical) → perf measurement. The diagnosis is precise enough
to know it's a native arena/marshalling codegen bug exposed by the cursor parser; pinning the exact
defect requires the binary, which the isolation repros could not produce.

## Bottom line

The full cursor is a **bounded, mechanical, self-contained 2–3 day rewrite** of the parser's
position layer — ~118 construction sites across 15 threading concepts + 6 result types + 2
position-producers, one structurally-removed hard case, a sharp identical-AST suite gate (plus one
pre-test to add), and a measured perf win on the probe harness. No language change, no backend
change, no downstream ripple (verified: 0 position reads past the parser, spans are source-byte).
It is the load-bearing piece of self-hosting capacity; this plan + review make it a **known,
correctly-sized quantity to schedule** rather than an open-ended risk.
