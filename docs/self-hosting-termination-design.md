# Termination verification — the self-hosted checker's last proof check

## Context
R5 parsed the termination block (term_kind 0=bound / 1=structural / 2=decreasing /
3=increasing + term_bound + term_field span). R6d verified purity. Termination was
deferred — the arg-pattern analysis needed records parsing (a recursive call arg IS
a bare-record construction, `go(St { n: s.n - 1 })`) — unblocked since records
slice 1. This closes the checker: all four proof kinds verified, mirroring
verbosec's Phase C (structural/decreasing/increasing) semantics.

## Design — per-rule check over SELF-recursive call sites

For each rule R with a declared kind 1/2/3 proof: find every AstCall in R's body
whose callee name == R's name (SELF-recursion; mutual recursion is out — verbosec's
per-rule checks are also self-shaped; note it). Walk all forms (reuse R6d's
count_undecl_call walk family: if/bin/neg/not/field/call-args/variant-fields/
match-arms). For each such call site, the FIRST argument (toy grammar: single-arg
rules; multi-arg → check the arg list head) must match the declared pattern:

- **decreasing : f** (kind 2): arg is `AstVariant(C, C, fields)` (a record
  construction — slice 1 parse) whose field `f` has value
  `AstBin(14, AstField(AstVar(<input param>), f), AstNum k)` with `k >= 1` —
  i.e. literally `<input>.f - k`. Anything else at that field → violation.
- **increasing : f** (kind 3): symmetric — `AstBin(13, AstField(AstVar(input), f),
  AstNum k)`, `k >= 1` (`<input>.f + k`).
- **structural : p** (kind 1): the call arg is `AstVar(b)` where `b` is a binder
  introduced by an ENCLOSING match arm whose scrutinee is the recursion parameter
  (`match <p or AstVar(input)>: V(...b...) => ... R(b) ...`). Slice scope: scrutinee
  is exactly `AstVar(input)` (covers `lsum(xs) = match xs: Cons(h,t) => h + lsum(t)`,
  and eval-style walkers). Thread the "binders bound by a match on the input" set
  down the walk (extend it at each qualifying arm, like R6a's binds_with_binders).
- **bound-only** (kind 0): NOT flagged even for recursive rules — parity with
  verbosec, which warns (breadcrumb) but doesn't refuse. Non-recursive rules: any
  kind passes vacuously.

Driver `count_term_errors(s : ScanState) -> number` (tokenize + parse_program +
fold the per-rule check), mirroring count_purity_errors. The input param name comes
from the parsed params (params head) — same source the purity reads-check used.

## Gate (CLEAN disk, programs in files, count_term_errors native)
1. vexprparse verifies; suite green (currently 429 + 1 ignored) + a new test.
2. **MILESTONE**:
   - decreasing OK: `go(s : St)` recursing `go(St { n: s.n - 1 })`, `decreasing : n`
     → **0**. BROKEN: same rule but `St { n: s.n }` (no decrease) → **1**; and
     `St { n: s.n + 1 }` (wrong direction) → **1**.
   - increasing OK: `scan(St { pos: s.pos + 1 })`, `increasing : pos` → 0; BROKEN
     `s.pos - 1` → 1.
   - structural OK: `lsum(xs : Lst) = match xs: Cons(h, t) => h + lsum(t) Nil => 0`,
     `structural : xs` → **0**. BROKEN: `... => h + lsum(xs)` (recursing on the
     WHOLE input, no descent) → **1**.
   - bound-only recursive → 0 (parity); non-recursive with any kind → 0.
3. Regression test (src/native.rs, mirror records_r6d): the six cases above.

## Honest scope
Closes the R6 checker: lints + sound types + purity + TERMINATION — all four proof
surfaces verified by the self-hosted checker. Scope bounds (each noted, matching
verbosec's own Phase-C shape): self-recursion only; single checked arg; structural
scrutinee = the input param directly; decreasing/increasing recognize the literal
`input.f ∓ k` form (k a literal ≥ 1). Deferred: mutual-recursion termination,
nested/derived scrutinees, non-literal k.
