# Self-hosting: widening the parsed grammar to records + match (arc design, pre-review)

Status: **DESIGN, not implemented.** Input to a strategic-review pass before any brick lands.

## Where the self-hosted compiler is now

`examples/vexprparse.verbose` is a complete, verified front-to-back compiler for a **closed
scalar grammar**: a program is a flat list of `rule` declarations with typed params, an optional
return type, `let` bindings, and scalar expressions (arithmetic `+ - * / % `, neg, the six
comparisons, `and`/`or`, `not`, calls, `if/then/else`, vars, bool/text literals). The pipeline
is real end-to-end: source text → tokenizer (INDENT/DEDENT) → parser → five analyses → type
checker → located diagnostics → interpreter, AND its inverse → source emitter → stack-IL
lowering → x86-64 machine-code generator → **standalone runnable ELF that prints its result**
(b8). Verified: the emitted `./a.out`'s stdout == the Verbose-written interpreter, for arbitrary
i64 results.

The named next frontier (docs/self-hosting.md §6) is the language surface the front end **parses**:
records, `match`, `Result`, collections, effects. This is the one that actually closes the
self-hosting loop, because the front end's OWN rules are written with records and `match` — to
compile Verbose in Verbose, the parsed grammar must eventually handle what the compiler itself
is written in.

## The one structural decision: the program type reshapes

Today a parsed program is a flat `RuleList` (`RCons(head: RuleDecl, tail) | RNil`). `parse_program`
(vexprparse.verbose:4753) recognizes a `rule` item via `span_is_rule`, conses a `RuleDecl`, and
**stops at anything else** (`else RNil`). Every downstream consumer takes a `RuleList`: `find_rule`,
`rule_list_len`, the analyses, the type checker, `x86_program` / `program_total_size` /
`lower_program` / `print_program` in the back end.

Adding `concept` declarations to the parsed grammar means the program is no longer just rules.
Two ways to absorb that, and the choice shapes the whole arc:

- **(A) Side-list.** `parse_program` returns rules **unchanged** (`RuleList`) and a **separate**
  `ConceptList` parsed in the same top-level loop. A new `ProgramAst { concepts: ConceptList,
  rules: RuleList }` (a single-variant "record-like" group concept, exactly like `Parsed::Mk`,
  `RuleDecl::MkRule`, `Block::MkBlock` already are). Downstream: the back end keeps consuming
  `RuleList` **byte-for-byte unchanged**; only the type checker gains the `ConceptList` to resolve
  `AstField` against. **This is the recommended shape** — it isolates the blast radius. The
  emitter doesn't change until a brick actually emits record/match code; the existing closed-grammar
  ELF path stays verified throughout.

- **(B) Unified item list.** `ItemList(Rule | Concept)` replacing `RuleList`. More faithful to
  real Verbose's interleaved top level, but it touches *every* `RuleList` consumer at once —
  larger, riskier, and it buys nothing the side-list doesn't until the emitter handles concepts.
  Rejected for the early bricks; revisit only if the side-list proves awkward.

## Proposed brick arc (side-list shape)

- **R1 — parse `concept Name` with scalar fields.** New `Concept` AST node (`MkConcept(name_start,
  name_len, fields: FieldList)`), `FieldList` (`FCons(name_start, name_len, ty, rest) | FNil` —
  same span+type-code shape as `ParamList`'s `PCons`), a `parse_concept_decl` mirroring
  `parse_rule_decl`'s header recognition (byte-recognize the `concept` ident + the `fields:`
  block), and `parse_program` extended to a top-level loop that dispatches `rule` vs `concept`
  into the two side-lists. Verified: a parse/print round-trip over a program mixing concepts and
  rules, and the existing rule-only programs still produce the identical `RuleList`. **Parser +
  AST only — no checker, no emitter change.** Smallest brick that captures concept structure.
- **R2 — `AstField` typechecking.** Lift the checker's `AstField => ERROR` (docs/self-hosting.md
  §6) — resolve `base.field` against the `ConceptList`: infer `base`'s concept, look up `field`,
  return its type; locate the error when the field is absent or `base` isn't a record. First
  consumer of R1's concept info.
- **R3 — record construction** `Name { field: expr, ... }` in the expression grammar (`AstRecord`),
  with the checker cross-checking the field set + per-field types against the concept (mirrors the
  verbosec verifier's record check).
- **R4 — sum-type concepts (`variants:`) + `match`** in the parsed grammar (`AstMatch`). This is
  where the parsed grammar reaches what the front end is itself written in.
- **R5+ — emitter.** The genuine hard part: records/match → machine code. **But the target already
  exists**: the verbosec native backend (`src/native.rs`) emits `concept_group` arenas + tagged-union
  `match` dispatch, and `vexprparse.verbose`'s own AST *is* a `concept_group`. So R5 is "the
  self-hosted emitter generates the same arena+tag machine code verbosec already generates," not a
  green-field design. Until R5, records/match are parse+check+interpret only (the interpreter path
  already handles them internally — `eval_ast` walks the front end's own match/records).

## Why this is review-gated (the IR lesson)

The IR arc (docs/backend-ir-design.md) was just designed, reviewed, and **declined on evidence**
after a cheap probe — the review caught an error of commitment before an expensive build. The
records arc is a larger structural commitment (it reshapes the core program type and is the
gateway to several bricks), so it gets the same gate: this design → fresh-context strategic
review → revise → implement R1. Questions the review should pressure-test:

1. Is the side-list (A) actually the right call, or does deferring the unified item list (B) just
   move pain to R5? Does any analysis/checker truly need concepts and rules in one ordered list?
2. Is R1's "rule-only programs produce the byte-identical `RuleList`" invariant real and testable?
   (It's the safety contract that keeps the verified ELF path intact across the arc.)
3. Is the brick ordering right — does R1+R2 deliver standalone value (better diagnostics on field
   access) before the heavy R3/R4/R5, so the arc has early payoff and a natural stop point?
4. The arena-index ceiling: `concept_group [max_nodes: 65535]` and the front end's own
   `max_depth: 4096` — does parsing real concept/match source risk exceeding the AST arena the
   self-hosted compiler itself runs in? (The front end parses into its OWN arena; deeper source
   grammars mean deeper ASTs.)
5. Is there a smaller first step than R1 that de-risks the program-type reshape even further?

## Honest scope

This arc does NOT aim to parse all of real Verbose (proofs, attributes, modules, services,
effects, collections, `Result`). It targets the **records + match** core because that is what the
self-hosted compiler is itself written in — the minimum the parsed grammar needs to grow toward
before "a Verbose compiler in Verbose" is more than a shape. Each brick is a milestone on that
path, not the destination, and R1+R2 stand on their own (real field-access diagnostics) even if
the arc pauses there.
