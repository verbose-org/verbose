# Self-hosted text map outputs

## Design fixed before implementation

Parent `39b9b74` (#265) checks scalar map output types but refuses
`collection(text)`. This also refuses the full `payroll.verbose` file because
its unselected `names` rule is checked. Add text output to the existing checked
scalar loop without a materialized result collection or a new text allocator.

Each supported text value is the existing packed span: one word containing a
source-relative start and a byte length. Literals refer to the prepared source
blob; record text fields refer to checked, copied argv text slots. After one
body evaluation, decode the span, write its bytes and a newline, then restore
the element arena mark. The next element can reuse the same storage only after
publication. Preserve the outer count/cursor registers across both syscalls.
Size and emit the same printing helper, including the final blob base. Text
output returns status 0; existing input/body failures stop with status 1 and
retain any completed native output prefix.

The accepted expression subset is deliberately closed: number/bool/text
literals, input/item scalar fields, scalar lets and captured aliases, scalar
operators, conditions, and `length`, `byte_at`, `substring`, `min`, `max`.
Substring bounds retain existing byte-based checks; output is raw bytes, with
no UTF-8 conversion or normalization. Text comparison operands must already
have the existing packed-span comparison lowering. A conditional text value
can be returned or sliced, but comparing it directly is outside this slice.

Check all eagerly evaluated lets in source order, including unused lets, and
all body branches. Refuse fresh concat values, user-rule calls, effects,
constructors/matches/Results, nested collections and unknown value shapes in
this new text-map scope before ELF/raw bytes. Calls outside that scope retain
their existing support. Do not infer a value representation from a text return
type alone. The existing input-layout, count, 16 text slots of 64 KiB each,
whole-rule-list and intermediate-collection gates remain in force.

There is no target lifetime tracking, new buffer, GC or increased reservation.
This extends the compiler written in Verbose; Rust production backends and
language syntax remain unchanged. Broader text value/call lowering is separate.

## Validation planned

Use the original-AST interpreter as the value oracle and Rust native where its
collection body support overlaps. Pin exact stdout/stderr/status, repeated
emission and raw output/refusals, both bootstrap generations, and every payroll
entry. Cover empty collections/text, Unicode, literal NUL/newlines, byte slices,
aliases and shadowing, both conditional arms, negative/extreme bounds, count
and copy limits, late failures, and unselected unsupported rules. Demonstrate
arena reuse under a one-page reservation with a failing negative control.

Compare all 193 existing examples against the parent, twice per compiler;
update the corpus acceptance assertion only from measured outcomes. Run the
serialized normal Rust suite, Python harnesses, CIDX controls and the complete
two-generation bootstrap. The design commit relies on the exact parent's
green validation: 890 normal Rust, 27 bootstrap, 97 Python tests and six remote
checks on `f46278a`, squash-merged as `39b9b74`.
