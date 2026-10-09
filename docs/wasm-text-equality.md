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
