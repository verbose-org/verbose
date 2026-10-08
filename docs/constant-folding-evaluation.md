# Constant folding and required evaluation

Design fixed before implementation, 2026-10-08. Parent: `0e4e9d3` (PR #267,
six passing CI checks).

## Contract and scope

Compile-time calculation must not turn a runtime arithmetic failure into a
compiler panic or a successful constant. Signed division and remainder by zero,
and `i64::MIN / -1` or `i64::MIN % -1`, remain operations when no valid constant
result exists. Native control flow can skip them; an evaluated native operation
retains its existing arithmetic trap. No new recoverable error type is introduced.

Literal negation uses the existing native wrapping convention, matching literal
addition, subtraction, multiplication and absolute value. Strict overflow proofs
are still checked on the original source before optimization and may reject such
an operation. This does not generalize wrapping semantics in the interpreter.

Value equivalence alone does not justify removing evaluation. `calculation * 0`
must still evaluate a calculation that can fail or have an effect, in its source
position. Eager lets retain their evaluation. A condition whose returned value is
predictable must likewise retain any required computation that can fail.

This slice covers the shared Rust optimizer and legacy native scalar lowering.
The checked numeric/text/Result paths retain their separate verified contracts.
WASM receives safe shared constant folding, but its eager `and`/`or` remains a
separate follow-up. Self-hosted source and its arithmetic conventions are unchanged.

## Implementation

- Use checked division/remainder for literal folds. Decline an undefined fold
  instead of executing it in Rust. The native emitter's own literal division
  fold must follow the same rule; it must not wrap an overflowing division.
- Use wrapping literal negation, consistent across debug and release compiler
  builds and with emitted native negation.
- Share a conservative scalar-evaluation predicate between optimization and
  native lowering. Literals and already available scalar binding/field reads
  can be discarded after source verification. Recognized arithmetic needs safe
  intervals and discardable operands; boolean/conditional forms must preserve
  every potentially evaluated child. Unknown forms, calls, reads, gates and
  arena scopes are not evidence of discardability.
- Guard multiplication-by-zero and interval-based condition elimination with
  that predicate. Native multiplication by zero evaluates a retained operand
  once, then clears the result, avoiding an unnecessary multiply or temporary.
- Keep the existing verifier and its range results separate: a range of possible
  successful values does not by itself establish that evaluation can be dropped.

These are compiler decisions, with no runtime analysis, allocator or GC. Corrected
programs may contain operations previously erased incorrectly. Existing safe
literal folds and simple scalar identities should keep their compact output.

## Validation

Exercise debug and release compilation, original and optimized AST native entry,
and the real CLI. Cover valid signed division/remainder, zero divisors, both i64
extremes, wrapping negation, guarded/unselected failures, required failures,
nested zero multiplication, eager lets, calls, and nested conditions whose value
range hides a failing condition. Compare interpreter values/errors where its
existing arithmetic semantics permit, and pin native arithmetic traps separately.

Verify original-source typing and strict overflow proofs still reject invalid
programs even under guards. Retain safe-fold/size checks and compare the existing
example corpus against the parent compiler, explaining any artifact changes.
Run the normal Rust suite serialized, Python tools, CIDX checks and the existing
two-generation bootstrap CI. Reuse the existing guarded-byte example and avoid
changing the bootstrap corpus denominator for test-only arithmetic fixtures.

## Measured regression controls

Real CLI comparison with parent `0e4e9d3`, using the guarded-byte example's input
concept and temporary arithmetic bodies (argv `"" 0`):

| Body | Parent | Corrected native |
| --- | --- | --- |
| `if 1 == 1 or (-9223372036854775807 - 1) / -1 > 0 then 7 else 9` | Compiler panic, status 2 | Prints `7`, status 0, 545 B |
| `(1 / i.n) * 0` | Prints `0`, status 0, 470 B | SIGFPE, empty stdout/stderr, 482 B |
| `if (if 1 / i.n > 0 then 1 else 2) > 0 then 7 else 9` | Prints `7`, status 0, 470 B | SIGFPE, empty stdout/stderr, 567 B |
| `i.n * 0` | Prints `0`, status 0, 470 B | Same output/status and 470 B |

The larger corrected failure cases retain operations that the old compiler
incorrectly removed. They add neither allocation nor runtime proof bookkeeping.
The audit also recorded an existing [legacy signed power-of-two division gap](known-gaps.md#legacy-native-signed-division-by-a-power-of-two),
which is unchanged by this slice.

The 194-file top-level example comparison preserves all 190 emitted native
artifacts byte for byte and all four refusal outcomes (status and stderr).
The self-hosted source and corpus membership are unchanged.

Local validation passes 902 unit tests and eight CLI integration tests, serialized
(28 existing ignored tests retain separate gates). The six new unit regressions
and two new CLI regressions also pass in release mode. All 97 Python tool tests
pass, as do CIDX validate/doctor and the three security tools. `cargo-audit`
needed one retry after a transient container DNS failure; Trivy continues to
report existing Python dependency findings. Two-generation bootstrap validation
remains a required CI gate.

The first CI run exposed `ETXTBSY` in the existing self-hosted constructor test
installer while replacing a previously executed failure probe. The shared test
helper now writes and chmods a staging file, then renames it into place, retaining
all output/status assertions and avoiding mutation of an occupied executable
inode. The affected self-hosted tests are rerun locally and in CI; production
emission is unchanged by this test-harness correction.
