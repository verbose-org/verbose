# Numeric branch guards

Implemented 2026-09-19, with bounded two-interval and scalar-comparison extensions.
Within the existing [strict overflow contract](numeric-overflow.md),
an explicit `if` comparison can justify arithmetic in its selected arm. The
compiler checks that justification; an LLM's assertion of safety is insufficient.
The syntax is unchanged. Native programs gain no allocator, garbage collection
or runtime proof bookkeeping.

```verbose
logic:
  let divisor = reading.value
  out = if divisor != 0 then 100 / divisor else 0
hints:
  overflow : [-100, 100]
```

For a full-i64 `reading.value`, the division has a divisor in the union
`[MIN, -1] ∪ [1, MAX]`, and the other path returns zero. The dividend is
100, so the division cannot encounter `MIN / -1`. Signed division truncates
toward zero. The fallback is source behavior, not a compiler-selected recovery
policy. The complete [nonzero example](../examples/nonzero_numeric.verbose)
also accepts a numerator bounded to `[-100, 100]`. The original
[sign-guard example](../examples/guarded_numeric.verbose) remains supported and
also shows an explicit increment that keeps `MAX` unchanged.

```sh
cargo run -- examples/nonzero_numeric.verbose --run quotient --native /tmp/quotient
/tmp/quotient 100 -3 100 0 100 4
# -33
# 0
# 25
```

## Facts and their lifetime

- A direct numeric input field or lexical numeric let can be compared with a
  signed integer literal or another direct numeric field/let using `==`, `!=`,
  `<`, `<=`, `>`, or `>=`. Either operand order is accepted. The selected branch
  restricts each operand's domain using the comparison; the opposite branch
  uses its negation. Comparisons of a scalar with itself add no new facts.
- `not` reverses the fact. A true `and` establishes both operand facts; a false
  `or` establishes both negated facts. A false `and` or true `or` does not select
  which operand supplied the outcome, so it adds no facts. Nested `if`s can
  further narrow established intervals.
- A tested local is its current lexical definition, even after shadowing.
  Facts about local `x` and input field `i.x` remain distinct. Testing an alias
  narrows that alias only; it does not narrow its original value or another alias.
  Boolean lets do not retain relations to the scalars that created them.
- Facts end at the branch boundary. A resulting numeric value carries the union
  of both checked result domains, within the two-interval limit below. Constraints
  on existing values do not escape into another binding, operand or sibling
  branch. Callees continue to verify independently over their
  entire declared input domain. A caller's guard cannot excuse an unsafe callee.

The complete condition is verified before either arm, using only facts already
established by enclosing branches. Eager lets and condition operands cannot use
the condition's own prospective facts. For example, `let q = 100 / divisor`
before the `if`, or `divisor > 0 and 100 / divisor > 0` as its condition, still
fails if the prior interval includes zero. This slice does not add verification
based on short-circuit boolean evaluation.

## Comparing scalar bounds

An explicit comparison can now use both operands' checked domains. For example,
`if counter < ceiling then counter + 1 else counter` is safe even when either
value can reach MAX: the true branch establishes `counter <= MAX - 1`. See the
complete [capped counter](../examples/capped_counter.verbose), with nonnegative
input bounds checked at entry. It preserves a value already above its ceiling;
it does not silently clamp it.

```sh
cargo run -- examples/capped_counter.verbose --run advance --native /tmp/counter
/tmp/counter 4 5 5 5 7 5 9223372036854775807 9223372036854775807
# 5
# 5
# 7
# 9223372036854775807
```

For `x < y`, the verifier intersects x's domain with values below y's largest
possible value, and y's domain with values above x's smallest possible value.
Non-strict comparisons include the endpoint; `>` and `>=` reverse the operands.
Both projections use the domains from **before** that comparison. Equality
intersects the two domains, including represented holes when the result fits
the two-interval budget. Inequality excludes a value only when the other operand
has exactly one possible value. Thus `x != zero`, with `let zero = 2 - 2`, can
justify `100 / x`; `x != y` with a varying y does not generally exclude zero.

Computed bounds can be bound to a numeric let and then compared. The computation
must first pass its own checks. A checked callee result can likewise supply a
bound; an explicit public overflow interval remains the caller's premise.
Arithmetic expressions and calls directly inside a comparison do not supply
refinement in this slice. Boolean aliases do not preserve comparisons either.

This remains one source-order pass over each selected guard, not a stored graph
of relations or a solver iterated until nothing changes. With both x and y in
`[-10, 10]`, `y > 0 and x >= y` can prove `100 / x`; `x >= y and y > 0` cannot
in this slice, because the first comparison saw y's original domain. A nested
guard can make that order explicit. Nor does `x > y` necessarily prove
`100 / (x - y)`: subsequent arithmetic uses the independent projected domains,
which may still overlap. No relation or alias identity escapes a branch or
becomes a premise for a separately verified callee.

## Nonzero values and bounded analysis

Each numeric fact is now a union of at most **two** nonempty signed intervals.
An interior `!=` can split a range: excluding zero from `[-10, 10]` produces
`[-10, -1] ∪ [1, 10]`. Arithmetic checks each interval pair before joining
the results, so a nonzero divisor still requires protection against `MIN / -1`
and `MIN % -1`. For full-i64 numerator/divisor fields, a guard such as
`divisor != 0 and divisor != -1` establishes both requirements. Zero remains
possible after some computations: a nonzero `x` does not make `x + 1` or
`x * 0` nonzero. Unary arithmetic, `min` and `max` also check the represented
pieces rather than copying an exclusion flag onto their results.

Result domains can pass through eager lets, aliases, conditional results and
independently checked unannotated callees. For example, a callee returning either
`-2` or `2` has an inferred domain excluding zero. An explicit `overflow: [min, max]`
still exposes that complete public interval to callers, even if its implementation
produces fewer values. If that declaration includes zero, a caller needs its own
guard before dividing by the result. There is no new public union type or syntax.

The limit is a precision budget, not a new obligation on runtime values:

- A branch join or arithmetic result needing more than two disjoint intervals
  uses the enclosing interval from the lowest to highest endpoint. It may thereby
  include values the program never produces. Every potentially unsafe operation
  is checked **before** joining results; widening cannot hide overflow or a
  zero divisor.
- A further `!=` that would create a third interval keeps the prior domain.
  That new exclusion cannot justify an operation, while previously proved facts
  remain available. For `x` in `[-10, 10]`, `x != 0 and x != 2` can still justify
  `100 / x`, but does not prove `100 / (x - 2)` in this slice. Guard order can
  affect which facts fit this fixed precision budget.
- An equality intersection needing a third interval also keeps each operand's
  prior domain. It cannot fill an existing hole or pretend the exact intersection
  fits. Each projection checks at most four interval intersections in fixed space.
- Every arithmetic operation handles at most four interval pairs with a fixed temporary
  workspace in the compiler. There is no enumeration of combinations of source
  branches, no growing interval list and no runtime representation of these sets.
  Unknown safety after a loss of precision is refused with the existing rule,
  binding/branch and operation diagnostic.

## Conservative refusals

Comparisons involving arithmetic expressions, boolean aliases, calls or global
named constants add no refinement in this slice. Their expressions remain
subject to all ordinary type and arithmetic checks.

The existing lexer cannot spell `-9223372036854775808` as a signed source literal;
this slice does not change number parsing. Full-i64 runtime input remains valid.
For example, `value < -9223372036854775807` selects exactly MIN.

Both arms are always checked, including types, supported forms and arithmetic.
If collected constraints produce an empty interval, the arm is checked using its
enclosing facts, with all newly collected facts discarded. There is no empty-range
proof that silently accepts an invalid operation or an effect in a dead branch.
An unsafe constant such as `1 / 0` is refused even in an impossible arm. Failure
diagnostics retain the rule, binding/output, arm and unsafe operation.

A newly understood scalar comparison can reveal a contradiction that earlier
versions did not recognize. For example, with two separate zero-valued lets,
`x != 0 and zero != same` now produces an empty constraint set: its arm is
checked with the enclosing facts, without retaining the new `x != 0` fact.
Such a rule can therefore be refused where the previous verifier accepted it.
This preserves the existing treatment of recognized impossible arms.

The existing expression, nesting, call-expansion and native frame limits remain.
The compiler stores sparse branch facts and does no path enumeration. There is
no general relation solver, propagation of caller premises, or proof of total
execution time, whole-program memory safety or whole-process memory usage.

## Execution and backend support

| Path | Behavior |
|---|---|
| Rust verifier | Checks branch intervals and every original obligation |
| Interpreter | Existing conditional evaluation and numeric entry guards |
| Native Linux x86-64 argv | Existing conditional/signed arithmetic emission and input guards |
| Other native entry modes and services | Existing strict-overflow refusal before artifact creation |
| WASM | Existing strict-overflow refusal before artifact creation |
| Self-hosted ELF/raw emission | Existing `overflow` refusal; no branch prover added |

This widens the set of provable programs. The branch itself still executes when
its outcome depends on input; the extra analysis runs only in the Rust compiler.
Native simplification continues to use conservative enclosing intervals and
checked callee result intervals; it does not use holes to precompute comparisons.
Original-source verification happens first.
No execution-speed, cache or memory-size improvement is claimed for this slice.

Tests compare interpreter/native values, stdout, stderr and exit status across
both arms, malformed/out-of-domain entries, lexical scopes and i64 boundaries.
Refusals cover leaked facts, eager evaluation, unsafe callees, contradictory
conditions, precision-limit refusals and unsupported constructs. Interval
tests check containment against concrete comparisons, including MIN/MAX.
