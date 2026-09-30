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

## Implementation and support

The constructor walk carries a separate type environment. Every parameter is
seeded from its declaration; each let is checked before installing its new type.
Aliases capture the type visible at their definition. Variant match arms use
the declared positional payload types and restore the outer scope afterward.
The stored field name selects the expected type, independently of written order.

The strict expression classifier checks primitive operands and call arguments,
requires compatible conditional/match arms, and resolves nested field selections
by nominal concept identity. Flat collection types exist only in this checking
environment: they supply item types to reductions, map/filter and fold scopes.
They add no metadata to target values. Unknown names, unsupported inference and
incompatible types cannot satisfy a constructor obligation.

| Path or field type | Support |
| --- | --- |
| Stored number, bool, text | Exact scalar kind |
| Stored record or variant | Exact declared concept, including nested constructors |
| Stored bytes, collection or Result | Explicit capability refusal |
| Flat collection item / fold accumulator | Known element / initializer type in the local scope |
| `match_result` binder or expression as a field value | Refused: component/result inference is not represented here |
| Independent constructor inside `match_result` | Checked normally |
| Rule call used as an initializer | Arguments checked against parameters; return type from a resolvable `output:` declaration |
| Self-hosted ELF / raw output | Constructor refusal before any artifact bytes |
| `type_check` / `check_program` | Existing type-error count / diagnostic category (4) |
| Unchecked Verbose evaluator | Unchanged; not a verification entry point |
| Rust verifier, native and WASM | Unchanged acceptance and lowering |

This is not whole-program type soundness. In particular, the strict classifier
uses declared callee return types; existing checks of those declarations against
callee bodies retain their separate limitations. Header-only or unresolvable
callee outputs do not establish an initializer type in this slice. Resource
reads/fetches are also unknown to this classifier. Raw emission retains its
existing lower-level scope rather than becoming a full source verifier.

This deliberately changes acceptance of `tagged_bonuses.verbose`: its
`policy_tag: read(policy_tag)` initializer now refuses in both ELF and raw output.
The language's Rust implementation still supports that example. Three older
self-hosted construction tests now declare `Lst` or `Expr` outputs on their
recursive builder instead of asking the constructor checker to guess its
return type; their observable list sums and printed expressions remain the same.

The regression work exposed three separate Rust-verifier omissions: a constructor
could accept a future let with no established type, and constructor checks in
variant/Result arms could miss the local binder type. These probes now require
both compilers to refuse, following the separate
[Rust lexical correction](constructor-lexical-scopes.md).

All changes to production logic are in `examples/vexprparse.verbose`. The ELF
gate uses the existing rule checker; raw emission uses the existing pre-output
constructor gate. Per-rule scalar arena scopes reclaim checking temporaries.
No target instruction, layout, lifetime or allocation algorithm changes; this
does not claim that the compiler's own checking cost is unchanged.

Nested `sum(map(...), ...)` and `sum(filter(...), ...)` probes exercise type
inference only: the parent emitter already compiles these particular shapes to
binaries that trap, and this slice emits identical bytes. Direct collection
sum/fold probes also execute with explicit expected values. This does not add
general collection-composition lowering.

## Recorded validation

The serialized normal suite passes 879 tests, with 28 explicitly ignored.
The 60 KB HTTP capacity regression now opts into the existing request/response
deadlines: its complete-body premise requires bounded request assembly, not
the legacy single-read transport that sometimes received only 32,698 body bytes.
No production HTTP code changes.

Parent `457ee4c` and the corrected emitter were compared on identical inputs
for all 193 top-level example sources, selecting rule index 0. Each compiler
emitted each source twice; all 193 pairs were reproducible. Acceptance moves
from 94 to 93 solely because of the documented `tagged_bonuses` refusal.
The 93 commonly accepted binaries are byte-identical; the other 99 existing
refusals retain their status, stdout and stderr. This is first-entry coverage,
not support for every rule in those files.

For the identical updated self-source, both emitters produce 3,243,321 bytes,
SHA-256 `a703a6d8236ef894d23087175503dee08575a499e7c150e63963eecc1deae57c`.
That comparison isolates emission from checking; it does not claim that the
compiler source or compiler binary has not grown.

The Python harnesses pass all 97 tests. CIDX validation, environment checks and
the security phase pass; existing Python dependency findings remain reported.

All 27 two-generation bootstrap checks pass, including the complete example
and negative corpora. The reordered compiler reaches a 3,243,241-byte fixed
point (SHA-256 `829b724fe40dffb00dce8d8c7eecb614e2d9aefd54656bf03d17c2892a383412`).
Both gen0 and gen1 run the constructor value/refusal matrix and emit the raw,
`type_check` and `check_program` drivers used for its pre-output checks.
