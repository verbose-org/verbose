# Direct numeric operand reads

Implemented 2026-09-20, following PR #238. The strict native numeric emitter
can read an operand from its existing input/local slot instead of first copying
that value into expression scratch. This is a placement optimization within the
[existing numeric contract](numeric-overflow.md), with no new syntax or proof
declaration.

```verbose
let saved = input.x + input.y
out = saved - calculate(input)
```

`saved` stays in its local slot while `calculate` runs. The subtraction then
reads that slot directly. The callee's computed result still receives scratch
so the operation can use both values. A pair such as `input.x + input.y` needs
no operand-copy slots; the inputs already have stable entry storage.

## Why the direct reads remain valid

Input fields are immutable in this pure subset. The compiler's
[complete-expression lifetimes](numeric-local-lifetimes.md) keep a local's slot
allocated throughout every expression that reads it, including expanded calls
and both arms of a conditional. A callee releases only its own slots. Loading
such an immutable value later, when its operation runs, therefore reads the same
value without preserving a second copy.

The optimization covers numeric input fields and current lexical number/boolean
locals used by the existing scalar operations. It does not merge different
definitions or change alias/shadowing semantics. Each operand is mapped into an
isolated synthetic namespace, so local, field and generated-looking source names
cannot redirect a load. Returning a local still loads its result before that
rule releases its slots.

Computed operands retain their evaluation order, single evaluation and scratch
storage. Calls, arithmetic expressions and conditionals are not turned into lazy
bindings. Eager lets, including unused initializers, still run. Both original
branches and all intermediates pass verification before this placement happens;
source/call-expansion budgets and every entry guard still apply.

Constants retain their existing slot normalization. In particular, exposing a
literal divisor to the legacy scalar emitter would activate unsigned constant
reductions that are unsuitable for negative signed inputs. This slice preserves
the checked signed division/remainder path, including truncation toward zero and
the separate refusal of zero or MIN / -1 when not excluded.

## Cost and scope

The compiler adds a direct-read lookup per operand. It does not build another
lifetime graph or run a new solver. The runtime loses the redundant load/store
pair used to make an operand copy; remaining reads use the existing value's
slot. Slot offsets and frame size can also shrink. There is no reference count,
collector, runtime lifetime check or new value representation.

This affects only the native Linux x86-64 argv path participating in a strict
numeric contract. Input fields keep their entry slots, and materialized aliases
keep their own numeric values. The two-word BoundsError path, bounded-text
placement and unannotated native components retain their existing behavior.
WASM, self-hosted emission and unsupported entry modes keep their explicit
strict-contract refusals. This is not a general borrowing feature in the language
or a whole-process memory guarantee.

Tests cover arithmetic, comparisons, unary operations, minimum/maximum, immutable
locals across calls and branches, synthetic-looking names, aliases, shadowing,
i64 extremes, signed constant division/remainder and malformed entry data.

## Reproducible layout observations

[Raw observations and complete fixtures](measurements/numeric-direct-operands-2026-09-20.json)
compare merged PR #238 (`5f5f47e`) with this implementation, identified by its
compiler and source hashes. Both compilers accept every fixture. Reuse the
[layout reproduction snippet](numeric-optimization.md#branch-folding-layout-observations)
with this report path to rebuild them.

| Fixture | ELF bytes, before → after | Reserved frame bytes, before → after |
|---|---:|---:|
| Add two input fields | 748 → 720 | 72 → 56 |
| Deep sum with direct field operands | 3156 → 1960 | 568 → 64 |
| Successive expanded calls with live caller values | 8663 → 5225 | 592 → 328 |
| Deep sum with computed left operands | 3035 → 2045 | 312 → 304 |
| Control returning an input field | 714 → 714 | 56 → 56 |

Both native versions and both interpreters match integer oracles on 105 records
per fixture. Ten invalid argument sequences and malformed/partial records after
valid output preserve stdout, stderr and status. All ten allocation-syscall
traces are empty. Four unsupported entry-mode refusals preserve existing
artifacts. The control is byte-identical; computed operands still need live
scratch, as the fourth row demonstrates.

All 180 existing top-level examples retain compatible compiler diagnostics and
the same two refusals. Of 178 native artifacts, 169 remain byte-identical and
nine shrink. Their changed entries are separately compared against integer
oracles and both interpreters, including branch thresholds, i64 boundaries,
carried text input, boolean exit status and malformed/partial records. The report
records their full input-domain products, expected output hashes and sizes.
A repeated reference compilation reproduces every original artifact and diagnostic.

These are deterministic code/frame observations, not CPU timing, RSS or cache
measurements. There is no general speedup or cache-residency claim.
