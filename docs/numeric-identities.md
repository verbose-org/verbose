# Repeated numeric scalar reads

Implemented 2026-09-20 within the [strict overflow contract](numeric-overflow.md).
Two direct reads of the same numeric input field or current numeric local
definition designate one value. The verifier now uses this limited identity
instead of combining two independent copies of its domain. No new syntax or
author-supplied equality assertion is required.

```verbose
logic:
  out = if sample.value != 0 then sample.value / sample.value else 0
hints:
  overflow : [0, 1]
```

This accepts any signed i64 input, including MIN: MIN divided by itself is one.
The zero guard remains necessary. Two distinct full-i64 values still need to
exclude both a zero divisor and the MIN / -1 combination. See
[scalar_identities.verbose](../examples/scalar_identities.verbose), whose other
rules square a reading only inside its safe domain and prove exact cancellation.

```sh
cargo run -- examples/scalar_identities.verbose --run square --native /tmp/square
/tmp/square -3 0 3 3037000499 3037000500
# 9
# 0
# 9
# 9223372030926249001
# 0

cargo run -- examples/scalar_identities.verbose --run unit --native /tmp/unit
/tmp/unit -9223372036854775808 -1 0 1 9223372036854775807
# 1
# 1
# 0
# 1
# 1
```

The square example's zero result outside the safe interval is explicit source
behavior. Overflow is not caught or converted into a fallback by the compiler.

## Bounded arithmetic and identity

| Operation on the same scalar | Checked result |
|---|---|
| `x - x` | Exactly zero, even at MIN/MAX |
| `x / x` | Exactly one, only after excluding zero |
| `x % x` | Exactly zero, only after excluding zero |
| `x + x` | Double each interval piece, checking i64 overflow |
| `x * x` | Square each piece, checking i64 overflow and including zero when a piece crosses it |

Each calculation uses at most two interval pieces, with the existing fixed
workspace and conservative result joins. A square is safe over
`[-3037000499, 3037000499]`; extending either endpoint by one can overflow and
is refused. No growing relation graph, alias analysis, solver or path enumeration
is introduced. Ordinary operations on different values retain their independent
interval-pair checks.

Identity is lexical and local to the expression. Both `x` reads use the current
definition after shadowing. A numeric local named `x` is distinct from field
`input.x`; fields `input.x` and `input.y` remain distinct even with equal bounds.
Testing or subtracting `alias` and its origin does not establish identity, while
`alias - alias` qualifies. Calls and structurally repeated computations are not
direct scalar reads: `callee(input) - callee(input)` gains no such shortcut.

## Verification before simplification

Both operands are checked before identity arithmetic. Unsafe eager lets,
unsupported expressions and unsafe original branches still refuse compilation.
For example, `(x + 1) - (x + 1)` cannot hide an overflowing increment; neither
can `if x == x then 0 else x + 1` over full-i64 x. Input domains, callee contracts,
source proofs and the original expression/call-expansion limits remain enforced.

Greater precision can expose a previously unrecognized impossible condition.
For x in `[-10, 10]`, `let zero = x - x` now gives exactly zero. A following
`if zero != 0 and x != 0 then 100 / x else 0` is refused: the contradiction
discards all newly collected guard facts, and the arm is checked in its enclosing
domain where x can be zero. Earlier independent intervals could accept it.
This preserves the existing [impossible-arm policy](numeric-guards.md#conservative-refusals).

After complete verification, native simplification reuses these facts. It can
remove self-subtraction or proved-safe self-division/remainder and decide
`x == x`, `x != x`, `<`, `<=`, `>` and `>=`. Both source branches must already
be valid. A varying double or square still needs runtime arithmetic. Every
original input is still parsed and checked, even when the output becomes a
constant or the last call connecting an entry to its contract disappears.
Boolean outputs retain their true/false text and sticky false exit status.

These facts exist only in the compiler. Native numbers still occupy one word;
there is no new runtime metadata, allocation, garbage collection or equality
check. This does not establish general algebraic equivalence or whole-program
memory/CPU bounds.

## Backend support

| Path | Support |
|---|---|
| Rust verifier | Strict repeated-scalar arithmetic above |
| Interpreter | Existing signed operations and checked entry domains |
| Native Linux x86-64 argv | Checked scalar lowering and constant simplification |
| Other native entry modes/services | Existing strict-overflow refusal before artifact creation |
| WASM / self-hosted ELF and raw output | Existing strict-overflow capability refusal |

Tests exhaust small domains, cover signed i64 and square/doubling boundaries,
compare interpretation and native output/status, and check aliases, shadowing,
equal-domain independent operands, public calls, original refusals and entry
guards after simplification.

## Reproducible layout observations

[Raw observations and complete fixtures](measurements/numeric-scalar-identities-2026-09-20.json)
compare merged PR #236 (`90e5757`) with this implementation, identified by its
compiler and source hashes. Both compilers accept the two layout fixtures below.
Reuse the [layout reproduction snippet](numeric-optimization.md#branch-folding-layout-observations)
with this report path to rebuild them.

| Fixture | ELF bytes, before → after | Reserved frame bytes, before → after |
|---|---:|---:|
| Guarded self-division | 825 → 783 | 72 → 72 |
| Synthetic deep arithmetic behind a negative-square test | 1975 → 749 | 304 → 72 |

Both native versions and both interpreters match integer oracles on 105 input
records per fixture. Each native version also preserves stdout, stderr and exit
status for nine missing/malformed/out-of-range sequences and a partial record
after valid output. All four allocation-syscall traces are empty.

The new example's square, unit and cancellation entries each match interpretation
and an integer oracle on 211 inputs, including signed i64 and square boundaries,
plus eight invalid sequences and malformed input after valid output. Their
candidate ELF/frame sizes are respectively 699/64, 601/64 and 532/48 bytes.
The reference compiler refuses this program, so those are not before/after gains.
The report also records the newly recognized contradiction described above,
four unsupported entry-mode refusals that preserve existing artifacts, and
explicit self-hosted ELF/raw refusals with no output artifact bytes.

All 179 existing top-level examples retain the same diagnostics and 177
byte-identical native artifacts, with the same two refusals. A repeated reference
compilation reproduces every diagnostic and artifact. These are deterministic
code/frame observations, not CPU timing, RSS or cache measurements; the synthetic
branch does not establish workload-wide memory or speed improvements.
