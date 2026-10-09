# WASM boolean evaluation

Design fixed before implementation, 2026-10-08. Parent: `bed8c98` (PR #269,
six passing CI checks).

## Contract and scope

Apply the existing [boolean contract](boolean-evaluation.md) to supported WASM
expressions: evaluate the left operand once; `false and rhs` skips `rhs`, and
`true or rhs` skips `rhs`. Otherwise evaluate the right operand once. Required
failures keep the existing WebAssembly trap convention. A guard does not recover
from an error in the left operand or an eager let binding.

Original-source verification continues to check both operands, types, reads and
calls before optimization. Numbers and text are not source boolean operands.
Unsupported WASM expressions and strict contracts retain their existing gates;
this change does not add byte indexing, general collections, effects or new call
shapes. Native emission, the interpreter and self-hosted source are unchanged.

## Representation and lowering

WASM scalar boolean expressions use i64 zero/one internally, like comparisons.
The exported bool function still returns i32 through the existing final wrap.
For `and` and `or`, replace eager `i64.and` / `i64.or` with an i64-result `if`:

```text
emit left
i64.eqz
if (result i64)
  and: i64.const 0      or: emit right
else
  and: emit right      or: i64.const 1
end
```

Both operand subtrees are emitted once. The instruction sequence consumes the
left operand as its condition, leaves one i64 result, and adds no temporary
local, memory access, allocation, import or helper function. It costs six more
instruction bytes per surviving logical operator than eager bitwise lowering;
module length-prefix encodings may also grow at LEB128 boundaries. Host engine
machine code and execution cost are separate measurements.

`not` currently leaves the i32 result of `i64.eqz`, violating that internal
representation and producing invalid modules when composed or exported. Append
the same unsigned i32-to-i64 extension already used for comparisons. This adds
one instruction byte and preserves a canonical boolean result. It is necessary
to make nested logical expressions well typed.

No new local-lifetime mechanism is needed: branch results live on the WASM
operand stack, and existing text/helper scratch reservations remain unchanged.
Text-valued `if` blocks and other documented WASM representation gaps remain
separate.

## Verification and delivery

- Validate and execute actual modules in Node, from both source and optimized
  ASTs. Compare supported successful cases with source interpretation.
- Cover truth tables, nested `and`/`or`/`not`, bool and number outputs, eager
  lets and aliases, supported renamed-input calls, Result conditions, and text
  primitives with empty, NUL and multibyte UTF-8 inputs.
- Pin skipped and required division/remainder-by-zero and parse-int failures,
  failing left operands, and guarded literal arithmetic traps. Keep the existing
  WASM `MIN % -1 == 0` behavior distinct from native/interpreter trap behavior.
- Observe evaluation counts with the existing text scratch allocator, without
  adding instrumentation to emitted programs. Check scalar local declarations
  and instruction budgets to prevent hidden scratch storage.
- Exercise the actual CLI, original-source type/proof failures and artifact
  preservation on refusals. Node runtime checks must execute in the normal CI
  job; minimal local/container environments without Node may skip those checks
  with an explicit message, like the existing WASM runtime tests.
- Compare all existing example WASM outcomes and artifacts with the parent,
  repeat the parent emission to check determinism, and investigate changes.
  Confirm native artifact preservation for representative existing examples.
- Run serialized normal Rust tests, focused release tests, Python tools and
  CIDX checks. Require all six CI checks, including bootstrap, before marking
  the separate PR ready. Do not claim a speedup without a controlled benchmark.

## Observed regression controls

Using the guarded-byte example's `(s: text, n: number)` input schema, with
empty text and `n = 0`, the actual optimized CLI artifacts behave as follows:

| Boolean body | Parent module | Corrected module |
| --- | --- | --- |
| `i.n != 0 and 10 / i.n > 0` | 77 B, division-by-zero trap | 83 B, returns i32 `0` |
| `i.n == 0 or 10 / i.n > 0` | 77 B, division-by-zero trap | 83 B, returns i32 `1` |
| `not (i.n > 0)` | 68 B, invalid i32/i64 stack types | 69 B, valid, returns i32 `1` |

The tests validate each module before invoking it, distinguishing a malformed
module from a required runtime trap. Scalar-only probes have no locals,
imports, linear memory, globals or data section. Tests with text operands also
observe the existing concat allocator's output bytes and result pointer: traces
show the left operand exactly once, followed by the right operand exactly once
only when required, for source and optimized ASTs. No instrumentation or host
imports are added to those modules.

The 194-file WASM corpus comparison repeats the parent compilation as a
determinism control. It preserves 174 refusal outcomes (status and stderr) and
16 modules byte for byte. Four modules change: `app` (73 to 79 B), `business`
(76 to 82 B), `config` (199 to 205 B) and `layers` (92 to 98 B). Each differs by
exactly one logical `and` lowering and its code-section length fields; all
other sections are identical.

Node validates 19 of the 20 emitted modules on both compilers. The remaining
`layers` module has the same pre-existing [text equality type mismatch](known-gaps.md#wasm-text-equality)
on parent and current: this is not evidence of complete WASM acceptance parity.
For the other three changed modules, 24 parent/current executions over their
bundled JSON inputs agree with source interpretation, including propagated
Result errors. Twelve representative native examples remain byte-identical,
including `layers`, guarded booleans, strict numeric rules and SHA-256.

Local validation passes 914 unit tests and 11 CLI tests, serialized; the 28
existing ignored tests retain their dedicated gates. All eight new focused tests
and the existing guarded-constant CLI test pass in release mode as well. The 97
Python tool tests and CIDX validate, doctor and security pass. The security phase
retains the existing Python environment findings; no dependency is added.
