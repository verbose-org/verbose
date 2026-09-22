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

This slice requires the existing [strict numeric contract](numeric-overflow.md)
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

## Inspecting the result

```sh
cargo build --release
target/release/verbosec examples/native_stack.verbose --stack-report --run magnitude
target/release/verbosec examples/native_stack.verbose --stack-report --run magnitude --json
```

`--stack-report` writes no artifact, defaults to the last rule and refuses
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
| `stack_bound_bytes` | Saved base pointer + fixed frame + maximum transient use |
| `frame_bytes` | Actual fixed reservation emitted by the prologue |
| `input_slot_bytes` | Input words in the fixed frame |
| `shared_slot_bytes` | Peak shared locals/callee/scratch words in that frame |
| `bookkeeping_bytes` | Remaining fixed entry storage |
| `saved_base_pointer_bytes` | Space for the entry's saved base pointer |
| `input_stack_bytes` | Saved pointer while checking a carried text field's length |
| `expression_stack_bytes` | Maximum temporary scalar spill below the frame |
| `output_stack_bytes` | Numeric formatting storage; zero for boolean output |

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
| Native stdin/raw/stream/multiple entries | Refusal through the strict numeric entry gate |
| HTTP/service/reaction contexts | Unsupported by the participating strict numeric contract |
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
aliases, shadowing, repeated calls and preservation of existing artifacts on
refusal. Annotated/unannotated native code must be byte-identical. Interpreter
and native results are compared with an integer oracle.

Self-hosted fixtures omit `hints.overflow`, ensuring its existing refusal cannot
hide a missing stack-budget gate. Fields, lets, strings and comments spelling
`native_stack` remain accepted controls. Run normal tests serially; the bootstrap
suite also checks the updated self-hosted source.
