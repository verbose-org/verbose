# Records arc slice 4: record params + the unified static concept resolver

## Context ("vois large")
Slice 3 emits field access for LET-BOUND records only, via an ad-hoc "let RHS is
AstVariant" check. Real self-source rules read their INPUT record (`rule f(s : Foo)`
… `s.field`) and recurse constructing records (`go(St { n: s.n - 1, … })`). Grounded
facts that make slice 4 broad-but-cheap:
- The INTERPRETER already handles all of it (typed record params → 49; recursion
  with record args → 15). The oracle is complete — slice 4 is CODEGEN-ONLY.
- The record-input ABI already exists implicitly: a record value IS its arena index
  (a Number), and indices already flow through the call ABI (args pushed, params in
  rbp slots — R7c). Nothing new to invent: a record param is passed as its index.
- `PCons` carries the param's declared-type span (ty_start/ty_len, R6b) and `FCons`
  carries each field's declared-type span — compile-time concept information exists
  for params AND for chained access.

## Design — one resolver, not three ad-hoc checks

### static_concept_of (new, recursive) — the unification
`static_concept_of(node, params, lets, concepts, src) -> concept_index | -1`:
- `AstVar(s)`: if `param_index(s) >= 0` → the param's PCons ty span →
  `find_concept_index` (a param typed `s : Foo` resolves to Foo; untyped → -1).
  Else if a let → `let_rhs(s)`; if `AstVariant(c, …)` → `find_concept_index(c)`
  (subsumes slice 3's check).
- `AstVariant(c, …)` → `find_concept_index(c)` (field access directly on a
  construction).
- `AstField(base, f)` → recurse on base → if resolved, field f's FCons ty span in
  that concept → `find_concept_index` (CHAINED access `a.b.c` — the self-source
  shape `ea.env`, `st.cell`, `li.binds`).
- anything else → -1.

### AstField emit (generalize slice 3's arm)
Replace slice 3's "base is AstVar + let_rhs is AstVariant" gate with:
`let ci = static_concept_of(base, …)`; if `ci >= 0` → the SAME 19-byte suffix
(pop; imul entry_size; add r15; mov rax,[rax + 8 + 8*field_index]; push), with
`field_index = field_index_of(cd_fields(concept_at_index(ci)), f)`; else int3.
`code_size_node` mirrors: `code_size_node(base) + 19` when resolvable, 1 otherwise —
the resolvability test MUST be the same expression in both (drift edge).

### What needs NO code (verify, don't build)
- Record args through calls: `AstVariant` emit pushes the index; `x86_args` emits
  args via `x86_node`; the callee reads the param slot. Already works.
- Recursion constructing records at the call site: same path. Already works.
- Match binders holding record indices: R7b slots hold Numbers. Already works.

## Gate (CLEAN disk — emitted ELFs run, == the eval_main oracle; programs in files)
1. vexprparse verifies; suite green (currently 426 + 1 ignored) + a new slice-4 test.
2. **MILESTONE** (elf_program_src → run ELF, each == eval_main):
   - param access: `main: out = get_a(Foo { a: 42, b: 7 })` + `get_a(s : Foo): out =
     s.a + s.b` → **49** COMPILED.
   - RECURSION with record args (the self-source shape): `go(St { n: 5, acc: 0 })`
     where `go(s : St): out = if s.n == 0 then s.acc else go(St { n: s.n - 1, acc:
     s.acc + s.n })` → **15** COMPILED. This is the arc's real target: a rule that
     recurses on a state record, like word_length/skip_spaces/count_tokens.
   - CHAINED access: a concept whose field is typed as another record concept
     (`Outer / inner : Foo`), `main: let o = Outer { inner: Foo { a: 42, b: 7 } };
     out = o.inner.b` → **7** COMPILED (proves the recursive resolver + FCons ty).
   - UNCHANGED byte-identical: slice-3 let-bound cases (42/7/49/50), variant
     list-sum 6/15, scalar 5.
3. Regression test (src/native.rs): the three milestone cases + a byte-identity
   assertion vs the slice-3 cases, cross-checked against eval_main.

## Honest scope
Slice 4 completes the records arc's structural story: the compiler handles records
in every position the self-source uses them — construct (R7a), field-read on lets
(slice 3), field-read on PARAMS + CHAINED (this slice), through calls and recursion
(free, verified). Untyped params (`rule f(s)` with a record arg) stay int3 — the
self-source always declares input types; inferring from call sites is refused
(compiler never guesses). Still deferred: text-in-arena, termination verification,
input: blocks as the param source (the toy `rule f(s : Foo)` form carries the type;
mapping the full input:-block form onto params is a parse-level equivalence to check
when compiling real self-source rules).
