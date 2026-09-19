# Numeric branch guards

Implemented 2026-09-19. Within the existing [strict overflow contract](numeric-overflow.md),
an explicit `if` comparison can justify arithmetic in its selected arm. The
compiler checks that justification; an LLM's assertion of safety is insufficient.
The syntax is unchanged. Native programs gain no allocator, garbage collection
or runtime proof bookkeeping.

```verbose
logic:
  let divisor = reading.value
  out = if divisor > 0 then 100 / divisor else if divisor < 0 then 100 / divisor else 0
hints:
  overflow : [-100, 100]
```

For a full-i64 `reading.value`, the first division has a divisor in `[1, MAX]`,
the second in `[MIN, -1]`, and the remaining path returns zero. The dividend is
100, so neither division can encounter `MIN / -1`. Signed division truncates
toward zero. The fallback is source behavior, not a compiler-selected recovery
policy. The complete [example](../examples/guarded_numeric.verbose) also shows an
explicit increment that keeps `MAX` unchanged.

```sh
cargo run -- examples/guarded_numeric.verbose --run ratio --native /tmp/ratio
/tmp/ratio -3 0 4
# -33
# 0
# 25
```

## Facts and their lifetime

- A direct numeric input field or lexical numeric let can be compared with a
  signed integer literal using `==`, `!=`, `<`, `<=`, `>`, or `>=`. Either operand
  order is accepted. The selected branch intersects the previous interval with
  the comparison; the opposite branch uses its negation.
- `not` reverses the fact. A true `and` establishes both operand facts; a false
  `or` establishes both negated facts. A false `and` or true `or` does not select
  which operand supplied the outcome, so it adds no facts. Nested `if`s can
  further narrow established intervals.
- A tested local is its current lexical definition, even after shadowing.
  Facts about local `x` and input field `i.x` remain distinct. Testing an alias
  narrows that alias only; it does not narrow its original value or another alias.
  Boolean lets do not retain relations to the scalars that created them.
- Facts end at the branch boundary. A resulting numeric value carries the union
  of both checked result intervals, but no facts escape into another binding,
  operand or sibling branch. Callees continue to verify independently over their
  entire declared input domain. A caller's guard cannot excuse an unsafe callee.

The complete condition is verified before either arm, using only facts already
established by enclosing branches. Eager lets and condition operands cannot use
the condition's own prospective facts. For example, `let q = 100 / divisor`
before the `if`, or `divisor > 0 and 100 / divisor > 0` as its condition, still
fails if the prior interval includes zero. This slice does not add verification
based on short-circuit boolean evaluation.

## Conservative refusals

The domain is a single interval. Removing an endpoint with `!=` can narrow it;
removing an interior point cannot. Consequently, for a divisor in `[-10, 10]`,
`if divisor != 0 then 100 / divisor else 0` is still refused. Explicit positive
and negative branches, as above, state intervals the verifier can represent.
Comparisons of two variables, arithmetic expressions, boolean aliases, calls or
named constants add no relational facts in this slice. Their expressions remain
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
Native simplification continues to use conservative whole-rule intervals and
checked callee result intervals. Original-source verification happens first.
No execution-speed, cache or memory-size improvement is claimed for this slice.

Tests compare interpreter/native values, stdout, stderr and exit status across
both arms, malformed/out-of-domain entries, lexical scopes and i64 boundaries.
Refusals cover leaked facts, eager evaluation, unsafe callees, contradictory
conditions, unrepresentable nonzero ranges and unsupported constructs. Interval
tests check containment against concrete comparisons, including MIN/MAX.
