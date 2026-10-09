# Boolean evaluation

Design fixed before implementation, 2026-10-03. Parent: `ad233e5` (PR #266,
six passing CI checks).

## Contract

Evaluate the left operand of `and` / `or` exactly once. `false and rhs` returns
false without evaluating `rhs`; `true or rhs` returns true without evaluating
`rhs`. Otherwise evaluate the right operand exactly once and require a boolean.
These operators return booleans; numbers and text are not truth values.
An error in the left operand, or in a required right operand, propagates through
the existing runtime error path. This is conditional evaluation, not recovery.

Rule lets remain eager, in source order. Binding a failing expression before
a guarded use still fails. Calls, selected conditional arms and collection
projections use the same expression evaluator. Source verification still checks
both operands, including types and declared reads/calls. A runtime guard does
not waive a proof or a backend capability restriction.

For example, `length(i.s) == 0 or byte_at(i.s, 0) > 0` succeeds on empty text
without an invalid read. `length(i.s) > 0 and byte_at(i.s, 0) > 0` returns false
on that input. Byte indexing and UTF-8 semantics are unchanged.

The runnable [guarded-byte example](../examples/boolean_guards.verbose) checks
both bounds before reading. For example:

```sh
printf '[{"s":"","n":0},{"s":"é","n":1}]' | \
  cargo run -- examples/boolean_guards.verbose --run guarded_byte --stdin --json
# [{"out":0},{"out":1}]
```

## Implementation boundary

The Rust interpreter handles logical operators before the general eager binary
path. Other binary operators keep their evaluation order. An invalid left
operand receives a boolean type diagnostic before any right operand evaluation.

Ordinary CLI rule/reaction interpretation uses the original verified AST,
matching the existing source-execution interpreter. The shared optimizer encodes
some constant comparisons as numeric 0/1 and can remove expression evaluation;
its lowered AST is not a typed interpreter input. Skip that transformation for
interpretation, and report the skip when `--stats` is requested. Native/WASM
emission, disassembly and compilation benchmarks retain their optimizer path.
There is no new AST kind or implicit numeric-to-boolean conversion.

| Backend / entry | Scope |
| --- | --- |
| Rust interpreter, rules/reactions and source executions | Short-circuit logical operators on the verified source AST |
| Rust native | Existing short-circuit lowering retained |
| Self-hosted native / evaluator compiled natively | Existing short-circuit lowering retained |
| WASM | [Short-circuit lowering](wasm-boolean-evaluation.md) for supported expressions; internal boolean values are i64 zero/one, exported bool is i32 |

No native emitter, target frame, arena, allocator or GC changes are required.
Native and interpreter failure diagnostics/process conventions remain distinct.
This does not claim complete parity for arithmetic overflow, partial UTF-8
slices or unsupported backend shapes.

The artifact-path gap observed during this slice is now corrected by
[evaluation-preserving constant folding](constant-folding-evaluation.md):
`1 == 1 or (-9223372036854775807 - 1) / -1 > 0` both interprets as true and
compiles natively without a compiler panic. Folding leaves an undefined division
as an operation, which native control flow skips. Required failing computations
also survive multiplication by zero and range-based condition elimination.
The [WASM follow-up](wasm-boolean-evaluation.md) now skips those unrequired
computations as well and makes `not` compose with other boolean expressions.
Interpreter arithmetic overflow conventions remain a separate limit.

## Verification plan

- Truth tables, empty/nonempty/NUL/Unicode text, nested guards, aliases,
  shadowing, rule calls and collection projections.
- Skipped invalid byte/slice/division/remainder operations; required RHS and
  failing LHS still propagate errors. Eager unused lets still fail.
- Boolean-only operands and complete verification of skipped source operands.
- Real CLI tests with dynamic and constant booleans, JSON output, stdin/input,
  failure status and stderr; native output parity for supported forms.
- Shared self-hosted text-map fixtures with actual guards, run by gen0/gen1.
- Serialized normal Rust tests, Python tests and CIDX checks. The normal suite
  and bootstrap CI retain existing backend gates; compare native artifacts with
  the parent compiler to confirm that interpretation routing leaves them intact.

The parent/current Rust CLI comparison covers all 194 top-level examples,
including the new guarded-byte fixture: 190 emitted binaries are byte-identical
and four refusal outcomes (status and stderr) are unchanged. This checks native
artifact preservation, not a runtime performance measurement.
The self-hosted rule-0 corpus count is 94/194: the new guarded-byte entry emits
a 1,376-byte ELF, with its empty/Unicode/extreme-index behavior pinned in the
shared gen0/gen1 driver. The remaining 193 files keep their earlier acceptance.

Local validation passes the serialized normal suite (896 unit tests and six
CLI integration tests), all 97 Python tool tests, and CIDX validate/doctor/security.
The 28 existing ignored Rust tests retain their separate gates; two-generation
bootstrap CI runs the updated shared gen0/gen1 fixtures. CIDX's configured
security phase succeeds while reporting the existing Python dependency findings;
this change does not update dependencies.
