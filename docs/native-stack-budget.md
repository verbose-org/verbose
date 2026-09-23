# Native entry stack budgets

`proofs.native_stack: N` declares an upper bound in **bytes** on additional stack
storage used by a rule compiled as a Linux x86-64 native argv entry. It is an
optional checked constraint, not an optimization hint. Existing programs need
not add it; a sufficient budget does not change their emitted code.

```verbose
  proofs:
    native_stack: 112
    purity:
      reads: [input]
      calls: [clamp]
    termination:
      bound: 20
  hints:
    overflow: [0, 100]
```

The [complete example](../examples/native_stack.verbose) clamps a signed reading,
then computes its magnitude through a helper, a local and an alias. Its entry
needs a 64-byte fixed frame, saves 8 bytes for the base pointer and temporarily
uses up to 24 bytes for output formatting: **96 bytes**, within its 112-byte
declaration. The 8-byte expression spill is restored before formatting, so these
two transient requirements combine by maximum, not addition.

## Verification

The numeric path requires the existing [strict numeric contract](numeric-overflow.md)
on the rule or its connected call graph: pure numeric/boolean scalars, eager
lets, aliases and shadowing, conditions, and acyclic `callee(input)` calls over
the same input concept. Different input variable names are allowed. Effects,
recursion, collections, aggregate results, text calculations, context inputs and
service/reaction uses remain unsupported. Unused text input fields may be
carried: their pointer slots count, while pre-existing argv strings do not.

The source verifier checks the original program first, including obligations in
branches that native simplification might remove. It then asks the native
emitter to lower each budgeted rule into memory, without writing an artifact.
That same lowering places locals, shared scratch and expanded callee bindings.
Its actual frame reservation supplies the report, rather than an independent
AST size estimate. The bound includes:

- Input slots and peak shared slots for locals, computed operands and expanded
  calls, including simultaneous caller values.
- All reserved entry bookkeeping, including currently unused reserved slots.
- The saved base pointer and transient input guards, expression spills and
  output formatting. These phases finish separately; their peak is the maximum.

Transient accounting is closed over supported scalar lowering forms. Unknown
forms, unsafe arithmetic, failed call expansion or a frame above the existing
internal ceiling cause refusal. The declaration accepts integers from 1 through
2,097,152. An equal bound passes; one byte over fails. Repeated `native_stack`
entries and duplicate `proofs` blocks fail parsing, so a later block cannot
discard an earlier budget.

Every declared budget is checked, including helpers and unused rules. A helper's
declaration describes that helper **as a standalone argv entry**. Expanded into
a caller, it shares the caller's frame; the caller's bound accounts for its whole
expanded body and live values. Standalone helper budgets are not added together,
and a helper budget does not establish a budget for its caller.

The frame is reused across argv records. Record count changes work performed,
not the stack reservation. Input guards and malformed/partial-record failures
retain their existing behavior. No GC or runtime lifetime table is added.

## Bounded text entries

The same proof and report now cover the existing [bounded text storage subset](bounded-text-storage.md)
in a pure, single native argv entry. Text concatenation, numeric formatting,
length, comparisons, lets/aliases, shadowing, conditions and acyclic calls are
supported. Flat constructed inputs and returned records keep their existing
[checked call rules](bounded-text-inputs.md); record output supports number/text
fields. This does not add arithmetic, substring, nested records, effects or
recursion to the bounded text subset. Unknown lowering still refuses.

Text entries keep two frames live during construction: the ordinary input frame
and an invocation frame containing scalar/pointer/length slots and placed text
buffers. The report takes both reservations from the emitter. Destination
capacities use explicit `text[..N]` limits (or inferred limits for unannotated
participating callees), rounded to eight bytes. Increasing a declared output
capacity can therefore increase reserved stack, even for a short result.
Read-only literal bytes embedded in code and pre-existing argv strings are not
stack buffers. Descriptor slots and every reserved byte still count.

Placement already reuses dead buffers and overlays exclusive branch regions;
alias uses keep their owners alive through the final consumer. The budget uses
that actual placement, including holes, rather than adding all capacities or
assuming an ideal packing. Expanded calls share the invocation frame, with
simultaneous caller values retained. The existing internal 2 MiB invocation
ceiling (including its conservative fixed allowance) also remains enforced.
Scalar/pointer/length words also [reuse dead slots](bounded-text-slots.md),
independently of buffer ownership. The report includes their actual placed peak.

For the [text example](../examples/text_stack.verbose), `repeat_reading` returns
at most **63 bytes**, but its whole native entry needs **336 bytes**, within its
unchanged 384-byte declaration:

| Component | Bytes |
|---|---:|
| Outer input/bookkeeping frame | 56 |
| Saved outer base pointer | 8 |
| Inner scalar and pointer/length slots | 104 |
| Placed writable buffers | 128 |
| Saved inner rbp/rbx | 16 |
| Numeric-to-text conversion scratch | 24 |

The input length guard temporarily saves 8 bytes before the inner frame opens.
It does not overlap with construction. Numeric formatting restores its 24-byte
scratch after each operand; nested concats and successive calls do not accumulate
that scratch. Text/newline output uses no additional stack. Numeric output and
numeric fields of a returned record use 24 bytes while the inner frame is live;
boolean output closes the inner frame first and needs no scratch.

The total is therefore:

```text
outer frame + saved outer rbp
  + max(input scratch,
        inner frame + saved inner registers + max(expression scratch, output scratch))
```

This analysis adds no instructions, runtime allocation or GC to supported native
entries. No transport budget is implied: a rule reaching a declaration, even
through an unannotated caller, refuses stdin/raw/stream,
the legacy HTTP shell, and service/reaction uses. Unrelated unannotated entries
retain their existing support.

Future composition should cover execution scopes across the language: calls,
successive phases, storage retained between them and bounded concurrent work.
Command-line processing, batch pipelines and compiler passes are examples along
with services. HTTP request/handler/response/log storage remains a concrete case
for validating these reusable contracts. The present `native_stack` declaration
still describes only the supported standalone argv entry; a whole-program or
whole-service memory budget requires further analysis and an explicit scope.

Checked rules can now also run as [sequential argv phases](sequential-stack-budgets.md)
using `--run a,b,c`. With no stack values retained between phases, their aggregate
bound is the maximum of the individual bounds; a failed phase stops the sequence.
Each source declaration keeps its standalone-entry meaning.

## Inspecting the result

```sh
cargo build --release
target/release/verbosec examples/native_stack.verbose --stack-report --run magnitude
target/release/verbosec examples/native_stack.verbose --stack-report --run magnitude --json
```

`--stack-report` writes no artifact, defaults to the last rule and accepts a
comma-separated phase selection. It refuses
compilation, execution or other input-mode flags in the same command. Unknown or
unsupported entries fail with a diagnostic and nonzero status. An exceeded
declaration fails source verification before a success report or artifact is
produced. Omit the optional declaration to inspect a supported rule before
choosing its budget.

JSON stdout is one object, without verification banners. Schema version 1 reports
`target: "x86_64-linux"`, `entry_mode: "argv"` and
`scope: "additional_entry_stack"`, plus the rule and these byte counts:

| Field | Meaning |
|---|---|
| `declared_bytes` | Source limit, or `null` when absent |
| `stack_bound_bytes` | Additional entry stack, using the numeric or nested-frame formula above |
| `frame_bytes` | Actual outer frame reservation emitted by the prologue |
| `input_slot_bytes` | Input words in the fixed frame |
| `shared_slot_bytes` | Peak shared locals/callee/scratch words in that frame |
| `bookkeeping_bytes` | Remaining fixed entry storage |
| `saved_base_pointer_bytes` | Space for the entry's saved base pointer |
| `input_stack_bytes` | Saved pointer while checking a carried text field's length |
| `expression_stack_bytes` | Maximum expression scratch below its frame (inner frame for text) |
| `output_stack_bytes` | Numeric formatting storage; zero for text/boolean output |
| `text_frame` | Present only for bounded text lowering: `frame_bytes`, `slot_bytes`, `buffer_bytes`, `saved_register_bytes` |

`text_frame` is an additive schema-1 field; numeric JSON reports are unchanged.
Its `frame_bytes` equals `slot_bytes + buffer_bytes`. Consumers computing a total
must include this nested frame as shown above, or use `stack_bound_bytes` directly.

When a bounded text entry expands calls, `text_frame.calls` also explains the
possible caller buffer capacities live at entry and retained through return.
Aliases count one owner; exclusive owners can share placed storage. These
capacities are already covered by the frame and must not be added to it. See
[retained call storage](retained-call-storage.md) for the schema and a record
passed between stages with a computed 288-byte bound under its original
408-byte enclosing budget.

This is a calculated upper bound, not a runtime measurement. Branches are
conservatively included unless verified lowering removes them. Compiler changes
may alter placement and make a formerly sufficient declaration fail; rebuild and
inspect the report rather than treating a budget as ABI stability.

## Support and limits

| Path | Behavior |
|---|---|
| Rust source verification | Checks every declared native entry budget |
| Rust interpreter CLI | Verifies the native property, then interprets values; interpreter storage is not bounded by it |
| Rust native, single argv entry | Supported; the declaration adds no runtime instructions |
| Native sequential argv phases | Supported for checked phases over the same input concept; bounds combine by maximum |
| Native stdin/raw/stream | Refused for entries reaching a budget declaration; strict numeric restrictions also remain |
| HTTP/service/reaction contexts | Refused when they reach a budget declaration, including calls in after mutations or logs |
| WASM | Refuses programs declaring `native_stack` before writing an artifact |
| Self-hosted diagnostics, ELF and raw x86 emission | Detect and refuse the proof key; no stack planner yet |

This does **not** bound initial argv/environment storage, the OS-reserved stack
mapping, resident pages, compiler/interpreter memory, kernel buffers, signals or
total process memory. Nor does it prove cache residency, runtime work or elapsed
time. `termination.bound` retains its structural meaning. Placement relies on
trusted compiler correctness; it is not a semantic proof of arbitrary machine
code.

## Validation

Rust regressions traverse the emitted instruction control-flow graph and check
stack depth independently of layout metadata, including both arms and record
loops. They cover exact/one-byte-too-small limits, numeric and boolean output,
aliases, shadowing, repeated calls, nested frame restoration, text buffer reuse,
flat record output and preservation of existing artifacts on refusal. Annotated/unannotated native code must be byte-identical. Interpreter
and native results are compared with numeric/text oracles, including UTF-8 argv
inputs, embedded NUL output and repeated records.

Self-hosted fixtures omit `hints.overflow`, ensuring its existing refusal cannot
hide a missing stack-budget gate. Fields, lets, strings and comments spelling
`native_stack` remain accepted controls. Run normal tests serially; the bootstrap
suite also checks the updated self-hosted source.
