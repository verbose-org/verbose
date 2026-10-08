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
  conditionals, calls, recursive calls and numeric results inside text output.
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
