# Self-hosted scalar map outputs

## Design fixed before implementation

Parent `ba7d382` (#264) refuses intermediate collections but still emits an
`int3` for a record-element map whose output is a scalar. In particular,
`retirement.verbose::retirement_status` traps instead of printing booleans.
The Rust native reference prints every result and exits 1 if any result is
false. Empty collections print nothing and exit 0.

Implement number/bool projections from flat records with number/text fields,
and boolean maps over number elements. Preserve existing number-to-number and
record-to-record emission. Keep #264's restriction to final streamed producers;
there is still no intermediate collection value ABI.

Use the existing strict lexical type facts for map bodies, including sequential
lets, aliases and the item binder. The declared output element must match the
established body kind; unknown or unsupported scalar output facts refuse before
ELF/raw bytes. Unsupported boolean filters also refuse rather than using an
uninitialized boolean-map status. This is a backend capability check, not a
general repair of legacy return/body typing.

The new path requires a direct collection field of the unshadowed rule input.
The collection is the last input field; preceding input fields are numeric.
Elements are numbers, or nonempty flat records of numbers/text. Limit copied
text fields to the existing 4 MiB region (64 slots of 64 KiB), with an explicit
length check before each copy. Reject other input layouts for the new path.
Validate the count against the remaining argv words before the first element
load, using division by the element stride to avoid multiplication overflow.
Negative counts and truncated argument lists fail with exit 1 before output.

Per element, save the arena mark, load the item, save the loop cursor/count,
evaluate the body once, restore the loop registers, print the scalar and restore
the arena mark. Only the current element and its body temporaries remain live.
The existing second binder slot holds the boolean failure flag; numbers need
no result collection or accumulated output buffer. This adds no GC and makes
no claim about a body's independent recursion/resource budgets.

Boolean formatting writes `true\n` or `false\n` from a temporary stack word.
False sets the frame flag but does not stop later elements. The procedure
returns the flag and the collection entry uses it as exit status. Number output
reuses the existing decimal printer. Emission and sizing must share the new
loop helpers and agree exactly. Boolean literals and `not` also need ordinary
value lowering, which currently uses placeholders in `x86_node`.

Keep the established native streaming publication policy: a later body failure
preserves the already printed prefix and stops subsequent elements. The
interpreter remains the value/evaluation oracle; its eagerly materialized list
and CLI error output are not an oracle for native partial-output publication.

## Validation planned

Compare original-AST values, Rust native behavior where supported, self-hosted
ELF output and raw refusal, including both bootstrap generations. Cover empty,
all-true, all-false and mixed collections; number extremes; UTF-8/text fields;
aliases/shadowing; nested reductions and calls that clobber loop registers;
late failures; malformed counts/argv and text capacity edges. Test unknown or
mismatched output types and unsupported layouts before artifact bytes.
Demonstrate bounded arena reuse independently of output values, and compare
unchanged examples against the parent twice per source. Run serialized normal
tests, Python harnesses, CIDX checks and the full two-generation bootstrap.

The design commit uses the exact parent's green validation: 889 normal Rust,
27 bootstrap and 97 Python tests, with all six remote checks passing on
`f6a39e5` before the merge.
