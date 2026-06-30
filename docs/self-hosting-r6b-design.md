# R6b — concept-aware type checker (design)

## The crux (why R6b is bigger than "fill the type_of_env stub")

`type_of_env` returns a scalar type code `{0=number, 1=bool, 2=text, 3=ERROR}`.
`AstVariant`/`AstMatch` currently stub to `0`. To type them soundly we need concept
identity in the type system — but **it's lost at parse time**: `type_code_of_span`
maps every non-scalar name (e.g. `Token`) to `0` (its final `else 0`), and
`FieldList.ty` / `out_ty` / `return_type` store that lossy *code*, not the type
NAME. So a variant payload field `head : Token` is recorded as `ty = 0 = number`.

Consequence: there is no cheap-and-sound R6b.
- Naive (type binders by the stored `ty`): a `Token` binder types as `number` →
  the checker accepts `head + 1` where head is a Token. **Unsound** — fatal for a
  verification-thesis project.
- Conservative (type unknown binders as ERROR): rejects valid `head + 1` where head
  IS a number → **false positives** on the self-source's valid matches. Unusable.

Sound + usable ⇒ the checker must know each declared/payload type's CONCEPT. That
requires storing type-name **spans** (resolved against R4's ConceptList at check
time), not the lossy code. This is the first real type-system change — hence the
design pass.

## Design

### 1. Declared types carry a span (representation fix)
Add a type-name span beside the lossy code everywhere a declared type is stored:
- `FieldList.FCons` gains `ty_start : number, ty_len : number` (keep `ty` as the
  scalar fast-path / back-compat). Set by `parse_fields` (R1/R4) — it already has
  the type span (it calls `type_code_of_span(type_span)`); just store the span too.
- `RuleDecl.MkRule`: add `out_ty_start/out_ty_len` (and optionally
  `return_type_start/len`) beside `out_ty`/`return_type`. Set in parse_rule_decl_pos.
- Ripple: every `FCons {...}` constructor + every `FCons(...)` match arm + the
  MkRule constructors/accessors/sentinel. Mechanical (mirrors the R5 MkRule widen).

### 2. The lattice: concept codes
A type code is now scalar `{0,1,2}`, `3=ERROR`, OR a **concept code** `CONCEPT_BASE
+ concept_index` (CONCEPT_BASE large, e.g. 1000, clear of scalar codes; concept_index
= position in the ConceptList). Helper `resolve_type(concepts, src, ty_start, ty_len)
-> code`: `number/bool/text → 0/1/2`; else `find_concept_index(concepts, name) →
CONCEPT_BASE + idx`; not found → `3` (ERROR). (find_concept already exists from R1;
add an index-returning variant.) This is the ONE place name→code happens; everything
downstream compares codes.

### 3. type_of_env — concept-aware (thread the ConceptList)
`TypeArg` gains `concepts : ConceptList` (thread through every recursive call + the
checker driver, which has the parsed ConceptList from R4's parse_concepts).
- **AstVariant(cstart, clen, vstart, vlen, fields)** → `resolve_type(concepts, src,
  cstart, clen)` (the value's type is its concept). (Optional, defer: check the
  variant name exists in that concept + field types match — that's an arity/field
  check, not the core type.)
- **AstMatch(scrut, arms)** → type `scrut` (should be a concept code C). For each
  arm: find the arm's variant in C's VariantList (ConceptList) → its payload
  FieldList → bind each binder to its payload field's type via `resolve_type` on the
  payload field's ty_start/ty_len (now stored, step 1) — extend `tenv`. Type the arm
  body with the extended tenv. Match type = the common arm-body type (all equal →
  that code; disagree → ERROR). Binders thus get PRECISE types (number, or Token,
  or TokenList…), so arm bodies type soundly.

### 4. Scalar classifiers treat concept codes as non-scalar
`bin_type`/`if_type` (and AstNeg/AstNot) already gate on exact `0/1`. Confirm a
concept code (≥CONCEPT_BASE) falls through to ERROR for arithmetic/logic (it's not
0/1/2), and that `if_type` allows both branches being the SAME concept code (returns
it) — add that case so `if c then Token::A else Token::B` types as Token.

### 5. Declared-output check (the payoff)
Where the checker compares a rule's body type to its declared output: resolve the
declared out_ty span → code, compare to `type_of_env(body)`. A rule `tokenize ... ->
TokenList` whose body is a match returning TokenList now type-checks; a mismatch is
flagged. (If no such check exists yet, this is where the concept typing first pays
off — wire it.)

## Gate (R6b)
1. vexprparse verifies; suite green (currently 418 + 1 ignored) + new R6b test;
   existing tests unchanged (toy bodies have no concept types → resolve_type returns
   scalars exactly as type_code_of_span did).
2. **Milestone** (a type-of driver, source via argv):
   - `Token::Eof` → its concept code (not 0/number).
   - match binder typing: a rule `match toks: Cons(h, t) => h  Nil => Token::Eof`
     (Cons payload `head : Token`) → h resolves to Token; both arms Token → match
     type = Token (a concept code), NOT ERROR, NOT number.
   - SOUNDNESS: `match toks: Cons(h, t) => h + 1  Nil => 0` → ERROR (h is Token, not
     a number — the unsound-naive case is now correctly rejected).
   - declared output: a concept-returning rule whose body matches → type-check OK;
     a wrong declared output → flagged.
3. Regression test (src/native.rs) pinning the binder-typing + soundness cases.

## Honest scope
R6b = representation fix (type spans) + concept lattice + concept-aware type_of_env.
Medium-big but mechanical + the soundness it buys is the thesis. Deferred within
R6b: full variant-construction field-type checking (the field SET/types vs the
declared variant) — that's an arity/field check, layer it after the core typing if
needed. R6c (eval value model), R6d (proofs verifier), R7 (emitter) unchanged.
