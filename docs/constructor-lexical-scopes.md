# Constructor values in lexical scope

Design fixed before implementation, 2026-09-30.

The Rust verifier currently misses constructor payload errors involving future
lets (unknown values silently accepted) and variant/Result binders. Its general
type checker deliberately masks local binders instead of tracking their types. The self-hosted constructor
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

## Implementation and boundaries

`src/constructor_types.rs` performs one lexical walk per rule/service scope.
A handler also keeps the legacy baseline of established operand diagnostics
without assuming a service's state declaration. The complete obligations are
then checked in each actual service scope.
It distinguishes established types, unknown facts, and partial Result types.
The absent arm of a literal Result can fit a declared Result field, but a binder
from that absent arm has unknown type; it cannot borrow an outer variable's
fact. Both conditional branches must agree, with partial Result components
combined independently. Field selection follows nominal declarations, including
nested fields. Calls check argument types and use declared result types; this
pass does not expand callees or repair general return/body checking.

All expressions are traversed even when their type is unknown or a constructor
is unused. Invalid operand or call-input facts propagate through aliases to a
field diagnostic. Unknown facts elsewhere do not introduce a general new
acceptance policy for scalar rules. Existing constructor shape checks stay in
the legacy checker; payload checks move to the lexical pass to avoid reading
binders through an outer environment. Service state checks retain the
service, constructor field and declared state type in their diagnostics.

A fold body containing constructors requires equal initial and body accumulator
facts. Changed facts refuse; unknown facts cannot certify a field that uses
the accumulator. This is a
conservative refusal, including partial Results whose component facts change;
there is no iterative type solver. Independent constructor-free folds retain
the legacy general checker, but an unresolved fold cannot establish a later
constructor field. The work does not change declaration bounds or runtime
representations and adds no code to accepted native binaries.

The old note attributing the future-let case to a final type environment was
imprecise: source-order checking had already landed; an unknown field value
still passed silently. The regression now requires an explicit field refusal.

For a `Pair.first : number`, this alias captures the earlier numeric value:

```verbose
let value = 7
let saved = value
let value = "later"
let p = Pair { first: saved, second: 2 }
```

Replacing `saved` with `value` is a field-type error. Moving the first
`let value = 7` after the constructor is an unknown-binding error, even if
`p` is never used. A local binder with the same name follows its own declared
payload type, and its type never leaks into a sibling arm.

## Backend support

| Path | This correction |
| --- | --- |
| Rust verification / CLI | Lexical constructor obligations before execution or output |
| Original interpreter | Existing value semantics; used as the reference |
| Rust native / WASM | Existing lowering capabilities, behind the corrected verifier |
| Compiler written in Verbose | Unchanged previous constructor contract; shared refusal probes retained |

In particular, Rust native still refuses a numeric `match_result(Ok(...), ...)`
entry because that path requires a rule-call target. This correction verifies
such source accurately; it does not add the missing emission path. Valid
record arguments and variant binders in supported native shapes have direct
interpreter/native differential regressions.

## Validation evidence (2026-09-30)

Nine new regression tests cover lexical aliases, input and local binder
shadowing, both Result arms (including absent payloads), collection/fold scopes,
invalid dependencies, Rust storage types, service diagnostics and original
interpreter/native values, stdout, stderr and exit status. Existing state and
raw-TCP diagnostic tests retain their expectations. Six manual CLI probes
(three invalid programs, both native and WASM) refuse before creating artifacts.

Against parent `4da5fb2`, all 193 top-level examples retain verification
acceptance. Selecting the first declared rule where present yields 150 identical
accepted native binaries and 38 identical emission refusals; five files have no
such rule. Two independent emissions per compiler are reproducible. The final
verifier also repeats the 193-source acceptance comparison. Additional final
comparisons cover aggregate calls, variants and the complete self-hosted ELF
compiler (`elf_program_src`, raw stdin): its native image remains 704,829 bytes,
SHA-256 `65f42a68b48263f3368925c6e2654d14625d0a7c0e08b0a5bbe50ed0802b2f6d`.
These are compatibility checks, not execution-speed measurements.

The final normal suite passes 888 tests (28 opt-in tests ignored), serialized
with `cargo test -- --test-threads=1`; the Python harnesses pass 97 tests.
CIDX configuration validation, environment checks and its security phase pass.
The two-generation bootstrap remains a separate required PR check; production
self-hosted source and its target lowering are unchanged in this correction.
