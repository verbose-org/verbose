# Signed native division by powers of two

Design fixed before implementation, 2026-10-08. Parent: `54f7a0f` (PR #268,
six passing CI checks).

## Contract and scope

For a signed number `x` and a positive literal divisor `2^k`, native division
must truncate toward zero, including negative odd values and both i64 extremes.
For example, `-7 / 2` is `-3`; a logical shift incorrectly returns a large
positive number, and an arithmetic shift alone incorrectly returns `-4`.

This correction targets the legacy Rust native scalar emitter's strength
reduction. Shared literal folding, strict numeric lowering, interpreter, WASM
and self-hosted source keep their existing division implementations. No new
syntax, acceptance rule, input-domain enforcement or error convention is added.
The divisor-one identity preserves dividend evaluation. Zero, negative and
non-power-of-two divisors retain their existing lowering and trap behavior.

## Emission and register lifetimes

Evaluate the dividend exactly once into `rax`, including eager calls and failing
subexpressions. Then, for a positive literal power of two:

- `k = 0`: leave the evaluated value in `rax`.
- If the existing checked interval calculation establishes a nonnegative
  dividend, keep the existing four-byte `shr rax, k` fast path. This uses the
  existing declared input domains; it does not introduce new entry guards.
- Otherwise, for `1 <= k <= 62`, emit:

```text
cqo                  ; rdx = 0 for x >= 0, or all ones for x < 0
shr rdx, 64 - k      ; bias = 0 or 2^k - 1
add rax, rdx
sar rax, k
```

For a negative dividend, adding `2^k - 1` converts the final floor rounding
into truncation toward zero. The addition cannot overflow: a negative `x` plus
at most `2^62 - 1` stays within i64; nonnegative `x` receives zero bias.
The `k = 0` case is separate because x86 masks shift counts modulo 64.

The signed sequence is 13 bytes (nine more than the invalid logical shift).
It uses only `rax`, `rdx` and flags, already clobbered by ordinary signed
division in this emitter. There is no branch, allocation, new frame slot or
stack operation. Enclosing expressions must retain their existing register
preservation, including when the dividend contains calls or nested divisions.

A successful-value range is sufficient for choosing the shift because the
complete dividend still executes. It is not used to discard evaluation:
failing nested conditions and zero products retain the protections of PR #268.

## Validation and delivery

- Compare interpreter, native source AST and optimized AST over every positive
  i64 power-of-two divisor, with negative/positive neighbors of multiples,
  zero, both extremes and deterministic broad input samples.
- Exercise lets, aliases, shadowing, nested arithmetic, both operand positions,
  conditionals, calls, recursive calls, collection reductions and numeric
  results inside text output.
- Pin required versus skipped arithmetic failures and retain original-source
  typing and strict numeric proofs. Include negative, zero and ordinary
  non-power-of-two divisor controls without expanding their optimizations.
- Check emitted signed and proven-nonnegative sequences, unchanged frame size
  and the absence of divide instructions in the corrected power-of-two path.
- Verify the actual CLI in debug and release builds. Compare all 194 existing
  top-level examples with the parent compiler, investigate every artifact
  change, and retain the current bootstrap corpus membership.
- Run the normal Rust suite serialized, the Python tools and CIDX checks;
  require all six CI checks, including the two-generation bootstrap, before
  marking the separate PR ready. No runtime performance claim is made without
  a separate controlled benchmark.

## Observed results

The minimal unbounded `i.n / 2` artifact grows from 459 to 468 bytes. On inputs
`-7, -1, 0, 7`, it prints `-3, 0, 0, 3`; the parent printed
`9223372036854775804, 9223372036854775807, 0, 3`. Instruction checks cover every
positive power of two: the divisor-one identity adds zero bytes, the proven
nonnegative path remains four bytes, and the general signed path is 13 bytes.
These sequences contain no branch, allocation or stack access after the input
load. This measures code size, not execution speed.

All 194 existing top-level examples were compiled with both the parent and the
new compiler. A second parent pass confirmed deterministic output. Results:

- 189 emitted binaries are byte-identical, including their diagnostics.
- Four compilation refusals retain their status and diagnostic bytes.
- `sha256_big.verbose` grows from 11,669 to 11,678 bytes. Its
  `(len + 72) / 64` uses a let-bound length whose sign the existing interval
  helper does not establish. The only instruction change replaces that shift
  with the signed sequence; the other byte changes are two ELF size fields and
  two relative jump/call displacements. The compression loop is unchanged.

Both SHA-256 artifacts agree with Python's `hashlib` on all eight digest words
for ten messages, including empty text and lengths 55, 56, 64, 500, 1000, 2000
and 4087: 160 executions with matching values, empty stderr and exit status 0.
The existing bootstrap corpus membership is unchanged.

The serial normal suite passes 908 unit tests and nine CLI tests (28 tests
remain reserved for dedicated runs). The 97 Python tool tests, CIDX validation,
doctor and security phase pass. Security retains the pre-existing Python
environment findings; this change adds no dependencies.
