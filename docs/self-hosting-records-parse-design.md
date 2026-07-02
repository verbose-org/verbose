# Records arc — slice 1: bare-record construction parsing

## Why records, why now
The self-source uses BARE record construction pervasively — `Advance {...}` (105×),
`ScanState {...}` (83×), `ParseState {...}` (68×), `ProgramState {...}` (61×), … —
for the top-level `concept X / fields:` state types (168 top-level concepts). Today
`parse_primary` (line ~2650) routes an ident NOT followed by `::` or `(` to
`field_rest(base = AstVar(Name))`, so `Name { field: e }` parses as **AstVar(Name)**
and the `{...}` is silently dropped (verified: `Foo { a: 1 }` → shape 100 = AstVar).

Consequence: the self-hosted parser mis-parses its own dominant data form. This
gates everything downstream — self-compile codegen, termination's view of recursive
call args (which ARE bare-record constructions like `ScanState { pos: pos + 1 }`),
and purity/type seeing real constructions. Bare records are the #1 self-compile
blocker. This slice fixes the PARSE (representation + codegen/eval of records are
follow-on slices).

## Design (bounded, parser-only — mirrors R3's variant parse)
In `parse_primary`'s `isident` branch, add a lookahead: ident followed by `{`
(has_braces) WITHOUT `::` → BARE RECORD construction (today only the `Name::V {`
variant path checks has_braces). Route it to the EXISTING `variant_build` /
`parse_vfields` machinery with the "variant" = the concept itself:
`AstVariant(cstart = Name, clen, vstart = Name, vlen = clen, fields)`.

Reusing `AstVariant` (variant span = concept name span) means NO new AST node → no
exhaustive-match ripple (unlike adding `AstRecord`). It also lets follow-on slices
detect the record case structurally: "variant name == concept name" (or: the named
concept has an empty VariantList / a non-empty FieldList) ⇒ a record, tag =
concept_index*256 + 0, payload = fields in FieldList order.

The `{ }` empty-record case reuses the empty-fields branch already in the `::` path
(`VFieldList::VFNil`). Advance count over the consumed tokens mirrors the `::` path
minus the `::Variant` tokens (bare `Name {` consumes fewer — get the advance-k right,
the sharp edge; the `::` path advances 4 with braces / 3 without, bare records
advance 2 with braces / ... — count against the token stream, test empty + 1-field +
2-field).

## Gate
1. `cargo run -q -- examples/vexprparse.verbose` → "all proofs check out"; suite green
   (currently 423 + 1 ignored) + a new records-parse test. Existing tests unchanged
   (the `::` variant path + all scalar/AstVar cases must be byte-identical — this only
   adds the `ident {` route that previously fell through to AstVar).
2. **MILESTONE** (the self-hosted `shape`/parser, source via argv):
   - `Foo { a: 1 }` → shape in the **AstVariant band** (not 100/AstVar). The fields
     are captured (not dropped).
   - `Foo { a: 1, b: 2 }` (multi-field) → AstVariant band; `Foo { }` (empty) → band.
   - regression: `Foo::Bar { a: 1 }` (real variant) unchanged; bare `x` (no braces) →
     still AstVar (100); `x.field` → still AstField; `f(1)` → still call.
3. Regression test (src/native.rs, mirror records_r3): bare `Name { a: 1 }` parses to
   an AstVariant-shaped node with the field captured; `::` variant + plain ident +
   field-access + call all unchanged.

## Honest scope
Slice 1 = PARSE bare records (stop dropping them). Follow-on slices: field access
(`AstField` codegen/eval — read payload slot by field index, reusing R7b's slot
load + a field-index lookup, needs the base's concept), record-aware `variant_tag`/
`entry_size` (the "variant==concept ⇒ record, tag+0" case), and eval/codegen of
record construction (reuse R7a's arena with the record payload layout). Text values
+ termination verification remain separate. This slice unblocks the parser seeing
the self-source's real constructions — the foundation the rest of the records arc
(and self-compile) build on.
