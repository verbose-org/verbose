# Native optimization of verified numeric contracts

Implemented 2026-09-17, following the strict `overflow` contract in PR #228.
The source contract and supported backends are specified in
[numeric overflow](numeric-overflow.md). No new annotation is needed.

## Verification precedes simplification

The original rule component must pass strict analysis before native lowering
can simplify it. Every eager let and both source branches are checked. Neither
`let unused = MAX + 1` nor `if 0 == 0 then 1 else 1 / 0` becomes acceptable
because its result could be discarded. The original 100000-node call-expansion
limit also applies before a dead call tree can be removed.

The native pass uses enforced numeric field domains, lexical constant aliases,
and checked callee output domains. It folds constant arithmetic, including
signed division/remainder; precomputes known comparisons and boolean operations;
and selects a branch when its condition is proved. Constant lets can disappear
because their computation is proved pure and nonfailing. Unknown optimization
facts leave an expression intact; they never supply the original safety proof.

Every original argument is still parsed and checked, including unused fields.
This remains true if lowering removes the last checked call from an unannotated
caller or turns its entire output into a constant. Invalid input retains the
same exit status, stdout prefix and stderr behavior. Self-hosted/WASM and
unsupported native entry modes retain their explicit refusals.

This view is private to native emission. Source proofs, rule participation and
interpreter input checks use the original program. `--stats` still reports the
shared AST optimizer, not this later native pass. The native simplifier performs
no general correlation analysis, SIMD or parallel lowering.

## Simplification inside checked branches

Implemented 2026-09-20. After original-source verification, the native pass now
reuses the [numeric guard analysis](numeric-guards.md) to refine each selected
arm. A nested comparison already decided by those bounds disappears, and a
scalar constrained to a single value can be substituted in arithmetic. For
example, inside `if x > 0`, the test `x <= 0` is false; inside `if x == 3`,
`x * price` can become `3 * price`. The outer guard still executes when its
outcome depends on input.

The guard is the original condition, before its own constants were substituted.
Each arm gets a separate sparse scope. Facts do not leak into siblings, later
operands or earlier eager lets; they do not follow alias identity or specialize
a callee under its caller's premises. Shadowing clears the previous definition's
numeric and boolean facts, including when the new result is unknown to this
pass. Pure computations proved constant may disappear; other lets retain their
evaluation order.

The initial branch-folding slice used conservative interval hulls for arithmetic
and comparison decisions. The follow-up below preserves two-piece domains too.
Unknown optimization facts mean retaining code, not weakening or rerunning the
source contract. The existing interval, expression, depth, call-expansion and
frame limits remain; no relation solver or path enumeration is introduced.

Every source branch is checked **before** this pass. An invalid operation in
an impossible arm still refuses compilation. All original input checks remain,
even if a branch-local fact removes the final call connecting an unannotated
entry to a strict numeric rule. No backend gains support for the contract:
WASM, self-hosted emission and alternate native entry modes retain their refusals.

See [guarded_total.verbose](../examples/guarded_total.verbose). Its zero-quantity
arm can return zero without multiplication, and the positive-quantity test in
the other arm is redundant under the declared nonnegative quantity domain.
Numeric storage is still one word, with fixed frame reservation and no runtime
proof metadata or allocator. This slice makes no execution-speed, CPU-cache or
RSS claim.

### Branch-folding layout observations

[Raw observations and complete fixtures](measurements/numeric-branch-folding-2026-09-20.json)
compare the same sources with merged PR #234 (`fe50ca3`) and this change. The
report records source/compiler/binary hashes and the candidate lowering-file hash.
These are deterministic emitted sizes, not timings or process-memory measurements.

| Fixture | ELF bytes, before → after | Reserved frame bytes, before → after |
|---|---:|---:|
| Guarded line-item total | 922 → 818 | 72 → 72 |
| Synthetic deep arithmetic in an impossible nested arm | 1974 → 783 | 304 → 72 |
| Existing guarded increment | 638 → 641 | 64 → 64 |

The increment grows by three bytes: its MAX fallback can now load a known
64-bit immediate instead of reading the input slot. Substitution is not a promise
that every binary shrinks. The deep-arm fixture demonstrates eliminated temporary
storage; it is not a workload-wide memory or speed claim. Both compilers match
integer oracles and the interpreter on 36 total cases and 105 synthetic cases,
plus invalid/partial input checks. The changed increment and the adjacent ratio
entry each match on 205 inputs including MIN/MAX and eight entry-compatibility
cases. Allocation syscall traces for the total and synthetic cases are empty.

To reproduce the layout sizes, build the two compiler revisions and run this
from the repository root, replacing the two compiler paths:

```python
import json, subprocess, sys, tempfile
from pathlib import Path
sys.path.insert(0, "tools")
from benchmark_numeric_contract import frame_bytes

report = json.loads(Path("docs/measurements/numeric-branch-folding-2026-09-20.json").read_text())
for row in report["cases"]:
    with tempfile.TemporaryDirectory() as directory:
        directory = Path(directory)
        source = directory / row["source_file"]
        source.write_text(row["source"])
        (directory / row["intent_file"]).write_text(row["intent"])
        for compiler in ["/path/to/reference-verbosec", "/path/to/candidate-verbosec"]:
            binary = directory / "program"
            subprocess.run([compiler, str(source), "--run", row["entry"],
                            "--native", str(binary)], check=True)
            print(row["name"], compiler, binary.stat().st_size, frame_bytes(binary))
```

## Preserving disjoint numeric domains

Implemented 2026-09-20, following PR #235. A native optimization fact now retains
the verifier's union of at most two signed intervals instead of immediately
replacing it with its hull. Inside `if x != 0`, a domain such as `[-10, -1]` plus
`[1, 10]` proves `x == 0` false and `abs(x) > 0` true. Such tests can disappear;
the outer input-dependent nonzero guard still executes.

Lets, aliases, branch joins, arithmetic, negation, absolute value, `min` and
`max` preserve the same fixed representation when possible. Every arithmetic
pair is checked before joining its results. A comparison folds only if all
interval pairs agree: knowing `x` is either negative or positive does **not**
decide `x > 0`. Unannotated acyclic callees can provide checked two-piece output
domains; an explicit public `overflow` declaration still provides only its
declared single interval. Caller guards never specialize a callee.

This reuses the [existing precision budget](numeric-guards.md#nonzero-values-and-bounded-analysis):
at most four interval pairs, at most two retained pieces, and conservative
widening when a join or calculation needs more. A guard requiring a third piece
retains its prior facts. Exclusions are recomputed through arithmetic: dividing
a nonzero value by two can produce zero, and replacing a local clears its old
facts. General remainder ranges remain conservative; exact singleton remainders
still fold. Unknown facts keep code intact.

See [guarded_magnitude.verbose](../examples/guarded_magnitude.verbose): signed
quantity can be negative or positive, and its nonzero guard proves that its
absolute magnitude is positive. The compiler checks the complete source before
removing that redundant inner test. Tests cover all one-/two-interval subsets
of `[-3, 3]`, signed i64 edges, shadowing, precision loss, call contracts and
unchanged boolean output/exit behavior. All original entry guards and source
refusals remain, including unsafe operations in impossible arms.

The facts exist only in the compiler. Native numbers remain one word; there is
no new runtime metadata, allocator or GC. WASM, self-hosted emission and alternate
native entry modes retain their existing refusals for strict numeric contracts.

### Disjoint-domain layout observations

[Raw observations and complete fixtures](measurements/numeric-disjoint-folding-2026-09-20.json)
compare merged PR #235 (`a6b2122`) with this follow-up. Compiler, source, lowering
file and binary hashes are recorded alongside the tested inputs and expected
integer outputs. Reuse the reproduction snippet above with this report path.

| Fixture | ELF bytes, before → after | Reserved frame bytes, before → after |
|---|---:|---:|
| Guarded signed movement magnitude | 925 → 837 | 72 → 72 |
| Synthetic deep arithmetic behind an impossible zero test | 1974 → 783 | 304 → 72 |

Both native versions and both interpreters agree with integer oracles on 54
magnitude cases and 105 synthetic cases. Each native case also checks ten
missing/malformed/out-of-range argument sequences and a partial record after
valid output, including stderr and exit status. The four allocation-syscall
traces are empty; four unsupported-backend refusals preserve existing artifacts.
The 178 existing top-level examples retain the same compiler diagnostics and
176 byte-identical native artifacts, with the same two refusals. A repeated
reference compilation also reproduces every artifact and diagnostic.

These are deterministic code/frame observations, not CPU timing, RSS or cache
measurements. They do not imply that every source becomes smaller or faster.

## Stack storage

Numeric values use one word. An expression's temporary slots can be reused once
its result is in registers. The caller's live operands and local bindings remain
allocated while a callee runs; callee locals become reusable on return. Exclusive
branches use the same available scratch. Nonconstant lets retain their source
order. A subsequent [local-lifetime pass](numeric-local-lifetimes.md) now reuses
their slots after the last complete expression that reads them; the measurements
below describe the initial PR #229 implementation, before that follow-up.

The emitter measures its maximum live slot count and reserves that storage once
per process, plus input and existing entry/output bookkeeping. The conservative
2 MiB frame ceiling uses this live count. There is no heap allocator, dynamic
allocation metadata or runtime interval analysis. This does not make a promise
about OS stack limits, process RSS or which CPU cache contains the stack.

## Reproducible comparison

Harness: [benchmark_numeric_contract.py](../tools/benchmark_numeric_contract.py).
The harness now uses a [version-2 measurement protocol](numeric-benchmark.md);
the observations below retain the original version-1 timing scope.
[Raw observations](measurements/numeric-native-2026-09-17.json) include compiler
and fixture hashes, candidate working-tree state, all samples and host metadata.
The reference was rebuilt from merged PR #228, commit `0cfda1d`; the candidate
was this change's working tree based on that commit, identified by its compiler
hash. Both compile the same generated source.

Measured on an AMD Ryzen 7 5800X under WSL2, pinned to logical CPU 2. Each sample
runs 16000 argv records; 11 repetitions alternate before/after order, after two
warmups per binary. Timings include the Python harness/argument setup, process
startup, input parsing, computation and writes to `/dev/null`. External Windows
activity was not independently measured. These are small synthetic workloads,
not an HTTP throughput or general language performance claim.

| Workload | Reserved frame, before → after | ELF bytes, before → after | Median milliseconds, before → after | Median absolute deviation, before → after |
|---|---:|---:|---:|---:|
| Constant lets and a proved impossible branch | 3248 → 64 | 4017 → 604 | 11.737 → 8.295 | 0.836 → 0.389 |
| Dynamic arithmetic | 1200 → 64 | 1448 → 1352 | 8.692 → 8.321 | 0.202 → 0.087 |
| Successive calls in exclusive branches | 113792 → 72 | 92700 → 78921 | 33.182 → 32.875 | 0.545 → 0.682 |

The frame reductions are deterministic layout measurements, separate from timing
noise. The constant case's median is about 29% lower in this run. The small timing
difference on the calls case is within the observed dispersion. Smaller offsets
can also shorten instruction encodings without changing the arithmetic work.
Hardware cache hits/misses were not measured; reserved frame bytes are not an RSS
measurement, and the previous frame was not necessarily touched in full.

The harness checks all 201 inputs in `[-100, 100]` against both native binaries
and the interpreter before timing, plus missing, malformed, out-of-range and
partially processed input sequences. The separate compiler tests cover signed
i64 extremes, source refusals before optimization, lexical shadowing, live values
across nested calls/branches, and exhaustive small-domain comparison decisions.

```sh
python3 tools/benchmark_numeric_contract.py \
  --compiler target/release/verbosec \
  --reference-compiler /path/to/verbosec-from-0cfda1d \
  --reference-revision 0cfda1d3335a448be186b2a0ea012b46b3f4dc55 \
  --cases constants arithmetic calls \
  --records 16000 --repeats 11 --cpu 2 \
  --output /tmp/numeric-comparison.json
```

Choose a CPU allowed by the host's affinity. Do not overlap this measurement with
the compiler test suites or another benchmark.
