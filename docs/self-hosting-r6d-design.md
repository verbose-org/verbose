# R6d — purity verifier (reads + calls)

## Context
R5 parsed each rule's `proofs:` into `Proofs { reads : NameList, calls : NameList,
term_kind, term_bound, ... }`. R6a made the lints walk match/variant. R6d makes the
self-hosted checker VERIFY the purity proof — the core of Verbose's verification:
the body must only CALL rules it declares and only READ inputs it declares.
Termination verification (arg-pattern analysis) is genuinely harder and DEFERRED
to a follow-on; R6d = the purity half.

## Design

Two per-rule checks, wired into the checker's per-rule driver (rule_diags/prog_diags,
alongside the existing lint/type passes), plus a `count_purity_errors(rule, prog)`
driver for the gate.

### calls check (clean — no primitive table needed)
Walk the body's `AstCall` nodes (recurse through all forms incl match/variant, like
R6a). For each callee name: if `find_rule(prog, callee)` RESOLVES (it's a defined
rule) AND the callee is NOT in the declared `calls` NameList → an undeclared-call
error. Rationale: primitives (`length`, `byte_at`, `concat`, …) don't resolve to a
defined rule, so they're naturally excluded; self-recursion resolves to a rule, so
it must be declared (it is, in the self-source). Helper `name_in_namelist(names,
src, span) -> 0|1` (mirror name_bound, using name_eq over NmCons).

### reads check (thread input name + locals)
A "read" for purity = accessing the rule's INPUT — `AstField(AstVar(in), f)` or bare
`AstVar(in)` where `in` is the rule's input param name and NOT a local (let/binder).
Walk the body threading: the input name (from R5's parsed `input` FieldList — the
first input field's name) + the `binds` set of locals (R6a already tracks lets;
match binders added per-arm via binds_with_binders). Flag a read whose head-ident is
the input name (not a local) and whose declared-name is NOT in the declared `reads`
NameList. (Reads declared as `s` or `s.source` — R5 stored head-ident spans; compare
head-idents via name_in_namelist. A body reading `s.x` with `s` declared covers it;
lazier + matches how the self-source declares whole-input reads like `s`/`l`.)
ponytail: match on head-ident membership (declared `[s]` or `[s.source]` both admit
`s.*`); exact-path reads are a finer check, defer if it complicates.

### Wiring
Add a purity pass per rule: `undeclared_calls(body, declared_calls, prog, src) +
undeclared_reads(body, declared_reads, input_name, binds, src)`. Fold over the
program (like the lint/type diagnostics). The gate driver returns the total.

## Gate (R6d)
1. vexprparse verifies; suite green (currently 420 + 1 ignored) + new R6d test;
   existing tests unchanged.
2. **MILESTONE** (a count_purity_errors driver, source via argv):
   - undeclared CALL: a rule whose body calls a DEFINED rule not in its `calls:` →
     flagged (count 1). Same rule with the call declared → 0.
   - primitive call (`length(...)`) NOT in `calls:` → NOT flagged (primitives exempt).
   - self-recursion declared → 0; self-recursion NOT declared → flagged.
   - undeclared READ: body reads input field `s.x` with `reads: []` → flagged;
     with `reads: [s]` → 0. A local (let/binder) read → NOT flagged.
   - a fully-correct rule (real tokenize-shaped: reads/calls match body) → 0.
3. Regression test (src/native.rs) pinning: undeclared-call flagged / declared ok /
   primitive exempt / undeclared-read flagged / declared-read ok.

## Honest scope
R6d = purity (reads + calls) verification — the self-hosted verifier's core, over
R5's parsed proofs + R6a's walk infrastructure. Moderate. DEFERRED: termination
verification (check `decreasing : n` ⇒ every recursive call passes `n - k`, k>0;
`structural : f` ⇒ recursive arg is a match-binder sub-part) — a harder arg-pattern
analysis, its own brick (R6d.2 / fold into a later termination brick). After R6d the
self-hosted checker does lints (R6a) + sound types (R6b) + purity (R6d); the
interpreter runs match/variant (R6c). Remaining: termination + R7 codegen (giant).
