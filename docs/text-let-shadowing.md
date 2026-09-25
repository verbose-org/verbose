# Text literal propagation and sequential let scope

Design fixed before implementation, 2026-09-25.

## Contract and defect

Each rule evaluates its `let` bindings in source order. A binding's RHS sees the
previous environment; the new value becomes visible only to subsequent bindings
and the rule result. An earlier alias keeps the value it captured. Nested lambda,
fold and match binders retain their own lexical scope.

```verbose
let text = "old"
let first = text
let text = "new"
out = concat(first, text)
```

The result must be `oldnew`. The original-AST interpreter already implements this
with evaluation before environment insertion. The shared optimizer instead keeps
all text substitutions in a source-order list: the first replacement consumes
the identifier before a newer one can apply. A nonliteral rebinding also leaves
the obsolete constant active. Consequently the optimized interpreter, native
compiler and WASM compiler can observe an older value.

## Correction

Keep only the currently visible text substitution for each name. First rewrite a
binding's RHS using the previous environment. Then invalidate any substitution
for the bound name, whether the new value is text, numeric or another supported
type. Add the new substitution only when the rewritten RHS is a text literal;
otherwise retain the binding and its eager evaluation. Previously captured
aliases are already rewritten to their values and remain independent.

Keep the existing scope-aware expression substitution walker. Preserve the order
of retained bindings, the existing decisions to skip optimization for bounded
contracts, and all backend capability refusals. This is a compile-time correctness
fix with no new syntax, allocation, calling convention or GC.

This slice repairs the shared optimizer's text-literal propagation. It does not
claim general scope parity across every backend. The self-hosted compiler has a
separate first-match binding lookup and alias-output classification defect,
documented in [known gaps](known-gaps.md#text-alias-output-shadowing-and-streamed-substrings).
Those require their own handling of visible binding environments.

## Verification

- Compare original and optimized interpretation against explicit values for
  repeated text literals, aliases before/after rebinding, dynamic text and numeric
  rebinding, a RHS referring to the previous value, and multiple redefinitions.
- Exercise nested binders and both branches without substituting through a local
  shadow. Preserve failures in eagerly evaluated bindings whose values are later
  replaced or unused.
- Compare exact native stdout, stderr and status, and execute supported WASM
  cases in Node. Unsupported backend forms remain outside positive assertions.
- Compare the existing example corpus with the parent Rust compiler, including
  acceptance, diagnostics and deterministic emitted bytes. No performance claim
  or benchmark is part of this correction.
- Run the normal Rust suite serially, Python harnesses and CIDX checks. CI also
  runs the self-hosted bootstrap; no self-hosted source changes are planned here.
