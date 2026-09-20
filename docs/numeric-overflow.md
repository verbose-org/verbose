# Strict numeric overflow contracts

Implemented 2026-09-17. An existing `hints.overflow: [min, max]` is now a
checked obligation, not a hint accepted when its analysis is unknown. The
compiler proves the output interval and the safety of every supported numeric
operation evaluated by the participating rules. It does not infer a business
requirement from prose.

```verbose
logic:
  let factor = 1 + 2
  let product = sample.reading * factor
  value = product / 2
hints:
  overflow : [-150, 150]
```

For `reading : number [-100, 100]`, the intermediate product fits `[-300, 300]`
and the signed result fits `[-150, 150]`. See the complete
[example](../examples/strict_overflow.verbose), including aliases and calls
whose input variables have different names.

## What is established

- Eager lets, including unused lets, conditions, operands and both conditional
  branches are analyzed. A small output does not hide an overflowing intermediate.
- Addition, subtraction, multiplication, negation and `abs` must fit signed i64.
  Division/remainder must exclude zero and `MIN / -1` or `MIN % -1`.
- Undeclared numeric ranges mean the full i64 domain. No nonnegative 32-bit
  range is invented. Explicit input bounds are checked at execution before the
  participating entry runs; invalid inputs fail evaluation/exit 1.
- Calls use the callee's checked public output interval, or its inferred interval
  if it has no `overflow` declaration. Callers and dependencies participate too,
  including calls in untaken branches. Every participating rule is analyzed over
  its declared input domain, independently of an individual invocation.
- Unknown analysis, incompatible types, excessive analysis or unsafe arithmetic
  is a compilation error. Diagnostics identify the rule, binding/output, and the
  failed operation or unsupported obligation.

This is conservative analysis with at most two intervals per numeric fact.
Two direct reads of the same numeric field or current local definition now
share one value for arithmetic: `x - x` is zero, a safe `x * x` is nonnegative,
and a proved nonzero `x / x` is one. Equal intervals on different values do not
establish this identity. See [repeated scalars](numeric-identities.md).
Direct scalar comparisons in `if`
conditions can now refine the selected arm's interval; see
[numeric branch guards](numeric-guards.md). Every branch still has an obligation,
including statically untaken branches. The analysis does not establish general
correlations between expressions. A safe program can be refused: rewrite it or
state a narrower input domain that the entry will enforce.
An explicit `!= 0` can exclude zero even when a value may have either sign.
Direct comparisons between numeric fields/lets can transfer their checked bounds
in both directions within a branch. Equality preserves represented holes within
the fixed precision budget; no graph of relations or fixed-point solver is added.
Computed domains use at most four interval pairs per operation; excess result
pieces widen to their enclosing interval. Public `overflow` declarations remain
single intervals. See the [precision budget](numeric-guards.md#nonzero-values-and-bounded-analysis).

## Scope and representation

Participating rules are pure and acyclic, with number/bool outputs, numeric
literals/fields, lexical scalar lets, arithmetic, comparisons, boolean operations,
`min`, `max`, `abs` and conditionals. Inputs are nonempty flat concepts of numbers
and text; text fields can be carried but not read by checked expressions. Calls
must be `callee(input)` with the same input concept and an unshadowed input name.
Numeric local aliases and rebinding are supported. Record construction, collections,
Results, effects, context inputs, services and reactions are refused in this slice.
Rules disconnected from the contract retain their existing acceptance rules.

Analysis is limited to 100000 expression visits, 256 expression levels and 128
nested calls. Native call expansion is separately limited to 100000 nodes on the
original source, before simplification, and a conservative 2 MiB frame ceiling
based on live storage. These are compiler/storage limits, not CPU-time or
whole-process memory bounds.

Native lowering shares the checked scalar/frame machinery with `try_byte_at`:
acyclic calls expand into fresh lexical scopes and each stored numeric value
occupies one 64-bit slot. Input fields and local variables remain distinct
even when their names match. Arithmetic uses the signed operations whose safety
was proved. This path avoids legacy constant-division rewrites that do not preserve
negative signed division. It reserves fixed stack storage, with no allocator or
runtime interval analysis. Numeric expression temporaries and expanded callee
locals are released after their result reaches registers. Sequential calls and
exclusive branches reuse those slots; live caller values keep their storage.
The frame is sized from the emitter's maximum live slot count. Nonconstant lets
still evaluate once in source order. Their slots can now be reused after the last
complete expression that reads them, including through lexical aliases and
shadowing. Unused nonconstant lets execute without a persistent result slot.
There is no reuse partway through an expression or branch-specific shortening
of local lifetimes; see [numeric local storage](numeric-local-lifetimes.md).

## Native simplification after verification

The original source is checked first, including eager unused lets and both
branches. A private native emission view then precomputes constants, substitutes
constant aliases, and removes branches decided by enforced input ranges and
checked callee output domains. Constant lets can disappear only after that
proof: the supported arithmetic is pure and cannot fail within its input domain.
An unsafe unused let or an unsupported expression in an impossible branch still
refuses compilation. The general source optimizer and interpreter retain the
original rules and obligations.

Every original input guard stays in place, even when simplification removes the
last call connecting an entry to its numeric contract or makes its result constant.
Missing or malformed arguments and out-of-domain values therefore keep their
original failure behavior. Signed constant division/remainder preserve truncation
toward zero, including negative values and i64 extremes.

No speculative code motion, retry, SIMD or parallel lowering is enabled by this
pass. Its unknown facts mean “keep the expression”, after strict verification has
already established safety. The native simplifier now uses selected-arm bounds
to remove decided nested tests and substitute singleton values. It retains up
to two intervals through operations, branch joins and checked calls, allowing
nonzero facts across both signs to eliminate zero tests. Decisions must agree
across every interval pair; excess precision widens conservatively. It reuses
the guard analysis only after verifying the complete original source. See the
reproducible [numeric benchmark](numeric-optimization.md) for scope and measured costs.

## Entry behavior and support

| Path | Support |
|---|---|
| Rust verifier | Strict analysis above, including callers/dependencies |
| Interpreter | Checked input types/bounds before each participating rule |
| Native Linux x86-64 | Single-rule argv entry, including expanded calls |
| Native stdin/raw/stream/multi-rule/legacy HTTP | Explicit refusal before artifact creation |
| WASM | Explicit refusal: contract input guards are not implemented |
| Self-hosted compiler | Every `overflow` entry refused before ELF/raw emission |

Native numeric arguments must be `[-]digits`, within i64 and any declared field
range. Incomplete records, empty strings, a sign alone, nondigits and overflowing decimal strings exit
1 before that record's body. Leading zeroes and negative zero are accepted.
Number outputs print a decimal plus newline; bool outputs preserve existing
true/false output and sticky false exit status. Invalid input does not undo output
from earlier records. Malformed/out-of-range fields and partial trailing records have no stderr
payload. Too few arguments for the first record retain the existing
`error: not enough arguments` diagnostic. Interpreter errors retain their
diagnostic message.

## Migration and remaining limits

Existing `overflow` programs whose claimed proof was unknown can now fail
compilation. `pricing.verbose` declares amount/tax bounds explicitly so its
participating arithmetic is provable. Self-hosted compilation of previously
accepted `overflow` examples is deliberately refused until it can establish this
contract. This is a capability refusal, not self-hosted interval verification.

The optimizer/native interval helpers also stop inventing nonnegative ranges for
unbounded fields outside this contract. That correction does not establish a
general proof for legacy programs. `termination.bound` remains structural;
business intent, arbitrary effects and source-to-binary equivalence remain outside
the verifier's guarantees. The CLI therefore reports “supported source checks
passed”, rather than “all proofs check out”. Its benchmark summary lists declared
hints without claiming that every backend applied them.

Regression tests cover arithmetic intervals against exhaustive small signed
domains, i64 extremes, hidden failures, alias/shadowing/call composition, native
versus interpreter results, malformed inputs and artifact-preserving refusals.
