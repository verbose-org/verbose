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
and checked callee output intervals. It folds constant arithmetic, including
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
shared AST optimizer, not this later native pass. There is no branch-local range
refinement, correlation analysis, SIMD or parallel lowering in this slice.

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
