# Constructor values in lexical scope

Design fixed before implementation, 2026-09-30.

The Rust verifier currently misses constructor payload errors involving future
lets and variant/Result binders. Its general type checker deliberately masks
local binders instead of tracking their types. The self-hosted constructor
checker now rejects these cases; Rust needs the same source-order discipline
without inheriting the self-hosted backend's narrower storage capabilities.

Add a dedicated constructor obligation walk to rule verification. Seed input,
context and, for service checks, state declarations; visit each let RHS before
replacing its binding. Preserve captured alias types. Each collection, fold,
variant or Result body gets its own typed environment, including unknown
bindings that mask outer names. Visit every expression, including unused lets,
inactive branches, scrutinees and call arguments.

The walk tracks types, never values or execution paths. Literal Ok/Err values
may have an absent opposite payload; absence does not invent a type for a
variable in that arm. Unknown or incompatible computations cannot establish a
constructor field obligation. Validate both conditional arms and the operands
used to establish a payload type; a nominal constructor/callee return type alone
does not establish that its inputs are valid. Diagnostics name the constructor
and field. Existing declaration/shape and general rule checks remain in place;
payload checks use this lexical walk instead of the old masked environment.

Rust's existing bytes, collections and Result field types keep their meaning.
This adds no syntax, backend support, runtime checks, allocation or GC. General
type-checker completeness outside constructor obligations is a separate task,
as are the documented native variant initializer ordering and self-hosted
nested collection lowering gaps.

Validation: negative and corrected positive scope pairs, alias capture,
parameter/binder shadowing, branch isolation, nested constructors, collection
and Result payloads, constructor dependencies on invalid operands/call inputs,
and CLI refusal before native/WASM artifacts. Check supported positives against
the original interpreter and native backend. Run the normal serialized suite,
the Python harnesses, CIDX checks and the bootstrap; compare reference/current
example acceptance and reproducible native output. Update the three existing
self-hosted regression probes that currently pin Rust's erroneous acceptance.
