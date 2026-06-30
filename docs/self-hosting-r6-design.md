# R6 — semantics over the real grammar (design)

## Context

R1–R5 made the self-hosted parser read its own source's full surface (structure,
bodies, types, proofs) — but all PARSER-ONLY. R3 left **stub arms** for the new
`Ast` variants `AstVariant`/`AstMatch` in 17 rules (each returning a placeholder:
0 / 3-ERROR / "?" / `b"\xcc"`). R6 makes the **semantics** real. This is a step up:
syntax → meaning. The interpreter is number-only (`eval_ast : number`), the type
lattice is scalar `{0=num,1=bool,2=text,3=ERROR}`, and the lints thread bound
names via a `Binding` list.

The 17 stubs split by layer:
- **lints (6)**: count_undef_ast, count_badcall_ast, count_badarity_ast + the 3
  `*_span_ast` variants → **R6a**
- **type checker (1)**: type_of_env → **R6b**
- **interpreter (2)**: eval_ast, eval_ast_env → **R6c**
- **emitter (≈8)**: print_expr, lower_expr/_src, x86_expr/_node, code_size_* → **R7** (codegen, separate)
- plus **proofs checking** (R5 parsed `Proofs`; nothing verifies it yet) → **R6d**

## Slices (cheapest-correct → biggest). Do R6a first.

### R6a — the 6 LINTS walk match/variant (FIRST; detailed below)
No value model, no lattice change — pure AST-walk + scoping. Cheapest, and it's the
durable artifact (the checker). Replace the 6 lint stubs with real recursion.

### R6b — type_of_env over variant/match (MEDIUM)
`AstVariant` → the concept's type; `AstMatch` → the common arm-body type, with each
arm's binders typed from the matched variant's payload fields (lookup in R4's
ConceptList). Requires extending the scalar lattice with **concept-type codes**
(e.g. a band per concept, or a "concept ref" code carrying the concept id) so a
variant value has a type to check fields/match against. Self-contained but real.

### R6c — eval_ast/eval_ast_env over match/variant (BIG)
Needs a runtime **variant Value** (tag + payload values); today eval returns a bare
number. `AstVariant` constructs a tagged value; `AstMatch` dispatches on the tag,
binds the payload to the arm binders, evals the arm body. This is the interpreter's
value-model extension — the biggest R6 piece. Lets `--run` execute match/variant
rules (the self-hosted compiler runs its own bodies).

### R6d — proofs verifier (BIG, semi-independent)
Verify R5's parsed `Proofs`: purity (the body only reads declared `reads` / calls
declared `calls`) + termination (structural/decreasing field actually shrinks).
The self-hosted equivalent of verbosec's verifier. Its own brick.

## R6a — detailed (the next brick)

Replace the `AstVariant`/`AstMatch` stub arms (currently `=> 0`) in the 6 lint
rules with real recursion. **File: examples/vexprparse.verbose** (+ test in src/native.rs).

The lints thread a `Binding` list (`a.binds`) of in-scope names (params + lets);
`AstVar` flags a name not in it (count_undef), `AstCall` checks callee
defined/arity. The two new forms:

- **`AstVariant(cstart, clen, vstart, vlen, fields)`**: the concept/variant NAMES
  are type references — NOT undefined vars, NOT calls, no arity. Only the field
  VALUE expressions are sub-expressions to walk. So each lint: recurse over the
  VFieldList's value exprs with `binds` UNCHANGED, sum the counts. (badcall/
  badarity: same — walk field values; the variant itself isn't a call.)
- **`AstMatch(scrut, arms)`**: walk `scrut` with `binds`; then for EACH arm, the
  arm's binders (e.g. `Cons(head, tail)` → head, tail) are NEW in-scope names
  ONLY within that arm body. So: `count(scrut, binds) + Σ_arms count(arm_body,
  binds ⊕ arm_binders)`. Need a small helper `binds_with_binders(binds,
  BinderList) -> Binding` that prepends each binder (span) as a bound name
  (mirror how `let` adds a Binding). For badcall/badarity, the binders don't
  affect call-checking, but the arm body still walks with the (extended) binds;
  the helper is only load-bearing for the undef lints — badcall/badarity can pass
  `binds` straight through (or the same helper, harmless).

New helpers: `binds_with_binders` (+ maybe `count_undef_vfields` / `count_undef_arms`
recursors mirroring `count_undef_args`). Same shape for the 2 other lints + the 3
`*_span` variants (which return a Span instead of a count — recurse the same,
return the first non-empty span).

### Gate (R6a)
1. vexprparse verifies; suite green (currently 417 + 1 ignored) + new R6a test.
   Existing tests unchanged (toy-grammar bodies have no match/variant).
2. **Milestone** (native driver, e.g. `count_undef` on a body): a match body
   referencing an UNDEFINED var in an arm → counted (was 0/stub); a match body
   using a BINDER (`Cons(head, tail) => head`) → head is bound, NOT undef (0);
   a variant field value referencing an undefined var → counted. Proves the
   checker now sees inside match/variant.
3. Regression test (src/native.rs): a rule body `match x: Cons(h, t) => h + zzz
   Nil => 0` with `x` bound, `zzz` not → count_undef = 1 (zzz), and `h`/`t` NOT
   flagged (binders in scope). Plus a variant field undef case.

## Honest scope

R6a is cheap (pure walk + binder scoping, no model change) — the safe opener,
mirrors the existing lint structure. R6b (lattice + concept types) and R6c
(variant value model) are the real semantic lifts; R6d (proofs verifier) is its
own arc. R7 (emitter for match/variant — the green-field codegen) remains the
giant. Each slice is parser-... no longer parser-only: R6 is where the
self-hosted compiler starts to CHECK and (R6c) RUN its own constructs. Do R6a;
re-plan R6b+ when reached.
