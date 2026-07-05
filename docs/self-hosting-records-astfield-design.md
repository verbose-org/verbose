# Records arc slice 2: AstField eval (interpreter) + record-aware variant_tag

## Context
Slice 1 made bare `Foo { a: e }` parse to `AstVariant(Foo, Foo, fields)`. But
`eval_ast_env`'s `AstField` arm is stubbed to `VNum 0` (line ~4386), so `s.a` → 0
(verified: `let s = Foo{a:42,b:7}; out = s.a` → 0). This slice makes field access
WORK in the interpreter — the oracle. Codegen (compile-time concept resolution) is
the NEXT slice; eval must come first so there's an oracle to compare against.

## The design (interpreter-side, clean — runtime tag carries the concept)

### record-aware variant_tag
Record construction `Foo { a, b }` → `AstVariant(Foo, Foo, fields)` (slice 1: variant
span = concept name). `variant_tag(concepts, src, Foo, Foo)` currently looks up "Foo"
as a variant of concept Foo → not found (a record has fields, no variants). Fix: when
the variant-name span equals the concept-name span (or: the named concept has an
empty VariantList / a non-empty FieldList — a record), return `concept_index * 256 +
0`. So a record's VData tag encodes its concept with variant-index 0.

### AstField eval
`AstField(base, fstart, flen)`:
1. eval base → a Value; expect `VData { tag, payload }` (defensive: non-VData → VNum 0).
2. `concept_index = tag / 256`; `concept = concept_at_index(concepts, concept_index)`.
3. `field_index` = position of the name `(fstart,flen)` in `concept`'s FieldList
   (a `field_index_of(fields, src, fstart, flen) -> number` helper; -1 if absent).
4. Read the `field_index`-th element of `payload` (ValueList) — a `vlist_nth(payload,
   field_index) -> Value` helper. Return it. (field not found / index out of range →
   defensive VNum 0.)

Payload order = FieldList declaration order. Record construction evaluates fields in
SOURCE order; this slice assumes source order == declaration order (the common case
+ how the self-source writes them). ponytail: if they can differ, a later slice sorts
by field_index at construction — note it, don't build it now.

### Prerequisite
`parse_concepts` must capture TOP-LEVEL record concepts (`concept Foo / fields:`)
with their FieldList — not just concept_group variants. Verify; if top-level records
aren't in the ConceptList, add them (they carry `fields`, empty `variants`). Without
this, `concept_at_index`/`field_index_of` can't resolve Foo.

## Gate
1. `cargo run -q -- examples/vexprparse.verbose` → "all proofs check out"; suite green
   (currently 424 + 1 ignored) + a new AstField-eval test.
2. **MILESTONE** (eval_main, source via argv):
   - `concept Foo / fields: a : number, b : number` + `main: let s = Foo{a:42,b:7};
     out = s.a` → **42**; `out = s.b` → **7**; `out = s.a + s.b` → **49**.
   - a two-field record where the accessed field is the second → correct (proves
     field_index, not just "first slot").
   - UNCHANGED: the R6c variant list-sum → 6/15 (variant tags still resolve — the
     record-aware branch must not perturb real variants); scalar programs unchanged.
3. Regression test (src/native.rs, mirror records_r6c): `s.a`→42, `s.b`→7, and a
   variant program (list-sum→6) unchanged in the same test.

## Honest scope
Slice 2 = field access in the INTERPRETER (the oracle) + record-aware variant_tag.
Clean because the runtime VData tag carries the concept. DEFERRED to slice 3:
AstField CODEGEN (x86_node) — needs the base's concept at COMPILE time (no runtime
tag to read), i.e. track let→concept / use the rule's declared input type; reuse
R7b's payload-slot load `[node_addr + 8 + 8*field_index]`. Also still deferred: text
values, record construction codegen already works (slice 1 parse → R7a arena emit),
termination verification. After slice 2 the interpreter fully runs record programs;
slice 3 compiles them.
