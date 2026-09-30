# Self-hosted constructor value types

Design fixed before implementation, 2026-09-30. Parent: `457ee4c` (#261).

Named placement is now checked, but the self-hosted verifier still accepts a
text initializer for a numeric field. Check each initializer against its named
declaration before ELF or raw emission. The supported stored field types in this
slice are number, bool, text and exact named concepts, including nested records
and variant payloads. Different named concepts are incompatible even when they
have identical layouts. Numeric ranges and text capacities retain their existing
separate contracts; this does not introduce new emitted representations.

Use a dedicated strict typing environment for constructor obligations. Seed all
parameters, preserve sequential lets and shadowing, and bind match variables to
their declared payload types. An unknown variable or unresolved type is never
implicitly numeric. Walk every expression, including unused lets, inactive arms,
call arguments and collection bodies. Known collection element types may type
iteration variables and scalar reductions without admitting collection storage
inside a constructor. Check the arguments of a call used to establish an
initializer's type against the callee's declarations; do not expand callees.

Bytes, collections and Result stored fields are outside this slice and refuse.
Result component types are not represented by the existing parser/type lattice:
constructor values dependent on `match_result` binders therefore refuse when
their type cannot be established. Independent, well-typed constructors inside
those arms remain checkable. Unknown inference in unrelated legacy expressions
does not turn this into a global migration of the existing type checker. The
unchecked evaluator is still not a verification entry point.

Resolve synthetic HTTP fields through the closed built-in layouts, including
their exact types. Preserve the service backend's existing capability gates.
This is a compile-time check: do not change evaluation order, native layout,
instructions, runtime storage or introduce a GC. Reclaim checker temporaries
through existing scalar arena scopes.

Validation compares positive cases and corrected negative twins with the Rust
source verifier, and valid values with the original-AST interpreter. Cover all
scalar mismatches, nominal concept identity, nested constructors, aliases,
shadowing, parameters, calls, conditions, variant binders and collection scopes.
Exercise unresolved and unsupported inference explicitly. Require zero artifact
bytes on ELF/raw refusal, including after both bootstrap generations. Compare
the full example corpus with the parent emitter twice per source, accounting
for deliberate refusals. Run the serialized normal suite, Python harnesses,
full bootstrap and CIDX checks before completing the separate PR.
