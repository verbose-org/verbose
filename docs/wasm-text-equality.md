# WASM counted text equality

Design fixed before implementation, 2026-10-09. Parent: `271c2d5` (PR #270,
six passing CI checks).

## Contract and scope

Implement the existing text `==` / `!=` semantics in WASM: compare lengths and
every byte, including embedded NUL and multibyte UTF-8. Do not normalize Unicode
or compare addresses. Evaluate both operands once, left to right, before testing
lengths. A failing operand retains the existing WebAssembly trap convention.
Logical guards still skip an entire unselected comparison.

Cover literals, text fields, sequential lets and aliases, existing concat and
JSON-escape producers, boolean composition and supported Result branches.
Preserve the original lexical environment when classifying and emitting each
let RHS, including shadowing. Source verification still precedes optimization.

Existing inline calls must use the current input, the same input concept, no
callee lets and an acyclic call graph. Collect their literal/helper/scratch
requirements before emission, including renamed input parameters. This also
lets text-returning calls participate without adding a target calling convention.
Enforce these boundaries explicitly instead of letting an unknown layout reach
the instruction stream. Text-valued general conditionals remain unsupported:
refuse them explicitly because their two-value block signature is not implemented.
Other unsupported types, effects, collections and strict contracts retain their
existing gates. The interpreter, native emitter and self-hosted source do not
change.

## Representation and storage

Reuse the existing starts/ends-with byte loop and its shared text-primitive
locals. Equality rejects unequal lengths before reading; equal empty spans
succeed without a read; otherwise stop at the first mismatch or declared end.
Use an unsigned loop bound for equality. Inequality negates the resulting
canonical i64 zero/one without evaluating the operands again. The exported bool
ABI remains i32.

The existing scratch group has six i32 locals (five used by equality, with the
sixth reserved for contains). Reserve it once per function when needed and reuse
it across comparisons. Evaluate both operand expressions onto the WASM operand
stack before parking their spans, so nested text primitives cannot overwrite a
live comparison. No comparison buffer, allocation, import, memory growth or GC
is introduced. Existing text producers keep their separate allocator behavior.
Host engines decide actual register/stack placement; local widths are not an
RSS or machine-stack bound. Host-provided spans retain the existing ABI contract.

## Verification and delivery

- Validate and execute source and optimized modules in Node, comparing successes
  with original-source interpretation. Pin empty, equal, differing-length,
  prefix, first/last mismatch, NUL and Unicode cases, including distinct addresses.
- Exercise lexical aliases/shadowing, multiple comparisons, nested producers,
  eager operand failures, skipped comparisons, supported calls and Result binders.
- Observe operand evaluation order/count through existing producer output;
  inspect local declarations and module sections for comparison-only storage.
  Test no reads for empty/different-length spans and traps at actual invalid reads.
- Test named backend refusals, source type/proof errors and preservation of an
  existing output artifact on failure. Execute the real CLI and `layers.verbose`.
- Compare all 194 existing examples against the parent, repeat parent emission
  for determinism, validate emitted modules and check representative unchanged
  native artifacts. Run serialized Rust tests, focused release tests, Python
  tool tests and CIDX checks; require all six CI checks before marking the PR ready.
  This correctness slice makes no benchmark or general performance claim.

## Observed regression controls

The 194-file example comparison repeats parent emission as a determinism control;
the two parent runs agree completely. Current emission preserves 18 modules byte
for byte, corrects `layers`, removes an unused formatter from `enrich`, and adds
three modules whose callee text literals were previously missing from preparation.

| Example (default last rule) | Parent | Corrected |
| --- | --- | --- |
| `layers` | 98 B, invalid text/scalar comparison | 175 B, validates and executes |
| `enrich` | 430 B | 300 B, unused numeric formatter omitted after binder classification |
| `alert` | Missing callee literal refusal | 283 B, validates and executes |
| `clients` | Missing callee literal refusal | 179 B, validates and executes |
| `priv_failure` | Missing callee literal refusal | 634 B, validates and executes |

All 23 emitted modules validate in Node. Of the remaining 171 outcomes, 137
preserve the parent refusal exactly and 34 receive an earlier, explicit backend
diagnostic. This includes five parent compiler stack overflows on unsupported
recursive call shapes; these now return a compilation error before emission.
No previously emitted example becomes a refusal.

Across the five examples above, 77 module executions agree with original-source
interpretation, including both parent/current `enrich` and its selected Result
payload. These CLI comparisons use ASCII and actual UTF-8 JSON characters;
embedded NUL uses direct typed interpretation in the regression suite because the
[ordinary JSON input reader](known-gaps.md#ordinary-rule-cli-json-unicode-escapes)
does not decode Unicode escapes. Twelve representative native artifacts remain
byte-identical, including guarded booleans, strict numeric rules and SHA-256.

Tests execute source and optimized AST modules. They also inspect field-comparison-only
modules: one rule function, no imports/globals/data segment, the existing single
memory page, and a shared group of six i32 locals regardless of comparison count.
Numeric comparisons reserve no such group. Producer traces show both operands
exactly once, in source order, even when their lengths differ. Invalid host spans
are used only as probes of skipped versus required reads, not as a new validated
host-input contract.

The final serialized normal suite passes 921 unit tests and 13 CLI tests (934
in total); 28 existing ignored tests retain their dedicated gates. All 97 Python
tool tests and local CIDX validate, doctor and security checks pass. Node execution
is required by normal CI; the dedicated bootstrap job checks the existing
self-hosted corpus without changing its source or acceptance matrix.
