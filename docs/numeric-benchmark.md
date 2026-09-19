# Measuring native numeric execution

The [numeric benchmark](../tools/benchmark_numeric_contract.py) now records
elapsed time and child CPU accounting separately. This follows the storage
changes in PRs #229 and #230; the compiler and emitted programs are unchanged
by this measurement-only slice.

## What the clocks measure

All argument strings and command vectors are prepared before timing. The elapsed
interval includes process launch, execution and waiting for completion. It still
includes launch overhead in Python and the OS, input parsing, arithmetic, output
formatting and writes to `/dev/null`. It is not an isolated arithmetic kernel.

CPU samples use the difference between two `RUSAGE_CHILDREN` readings surrounding
one serial child that has terminated and been waited for. This excludes earlier
compiler/interpreter runs and CPU work in the Python parent. The report separates
user-mode and system-mode seconds, converted to milliseconds, and retains their
sum. See the [Python resource API](https://docs.python.org/3/library/resource.html#resource.getrusage).

Voluntary/involuntary context switches and minor/major page faults are also
recorded as deltas. They are OS observations, not hardware cache counters.
CPU accounting does not remove frequency, cache, scheduling or virtualization
effects. No RSS, cache-residency or HTTP-throughput conclusion is made.

## Refusing incoherent CPU comparisons

These fixtures execute in one thread and do not create child processes. CPU time
therefore should not substantially exceed its enclosing elapsed interval. Each
sample checks this with a deliberately generous diagnostic tolerance:
`max(20 milliseconds, 5% of enclosing elapsed time)`. The enclosing interval also
contains the accounting reads. This tolerance detects large inconsistencies; it
is not the clock's resolution, a confidence interval or a guarantee of accuracy.

Before and after the workloads, an isolated Python process (`-I -S`) runs a short
single-thread loop independently of Verbose. Both its internal process/elapsed
clocks and its parent's child accounting are checked. The warmups and measured
samples are checked too. No inconsistent sample is deleted or corrected.

The command returns:

- **0, `status: ok`**: value/failure checks pass and no clock inconsistency was
  detected. Dispersion and controls still need examination before interpreting
  timings; this status is not evidence of a speedup or absence of regression.
- **2, `status: inconclusive`**: the functional checks pass but CPU accounting
  violates the diagnostic tolerance. Raw CPU values remain available for diagnosis
  and must not be used to establish a performance gain or loss. Argument errors
  also exit 2, without producing a completed report.
- **1, `status: failed`**: compilation, correctness, timeout, unexpected stderr or
  other execution failure. The report records the failure when initialized.

## Protocol and report format

Two warmups per binary precede alternating before/after and after/before pairs.
The default is now **32 paired samples**, giving equal order counts. Odd counts
remain accepted and are marked `balanced_order: false`. All samples are retained.
Summaries include median, minimum, maximum and median absolute deviation for each
clock. Paired percentage changes compare each before/after pair; they are not a
ratio of unpaired medians. A zero reference CPU reading makes the percentage
summary `null`, rather than dropping the reading or inventing a percentage.

Each workload first checks all 201 inputs in `[-100, 100]` against both native
versions and the interpreter, then checks entry failures on stdout, stderr and
exit status. SHA-256 equality identifies controls with identical native binaries.
The report keeps compiler/harness/fixture hashes, host metadata, affinity and
the original argument configuration.

Reports use **schema version 2**: per-case `runs` hold the order and full per-side
samples, while `summary` is indexed by `wall_ms`, `cpu_ms`, `user_ms` and
`system_ms`. Version 1's `runs_ms` and summary layout are no longer emitted. The
historical version-1 reports remain untouched. Their elapsed intervals also
included argument-string preparation, so their wall-time numbers should not be
compared directly with the new protocol's numbers.

Run after building both compilers, with no overlapping test/build/benchmark:

```sh
python3 tools/benchmark_numeric_contract.py \
  --compiler target/release/verbosec \
  --reference-compiler /path/to/reference/verbosec \
  --reference-revision <reference-commit> \
  --records 32000 --repeats 32 --cpu 2 \
  --output /tmp/numeric-measurement.json
```

Choose a CPU allowed by the host's affinity. Passing the same compiler as both
versions provides an additional control. The harness itself is checked in CI
with `python3 tools/test_numeric_benchmark.py -v` after `cargo build`; those tests
check measurement/oracle behavior without imposing performance thresholds.

## Recorded comparison, 2026-09-19

[Raw report](measurements/numeric-cpu-2026-09-19.json): Ryzen 7 5800X, WSL2 kernel
5.15.153.1, pinned to logical CPU 2, 32000 records and 32 balanced pairs per
workload. The reference compiler was rebuilt from PR #229's merge `a33bcb9`;
the candidate is PR #230's implementation, present at merge `8fe0504`. Compiler
and harness hashes identify the binaries and the uncommitted measurement-tool
revision used. Every fixture and emitted binary hash matches the earlier
September 17 comparison, including four byte-identical control workloads.

All 1206 valid values and the entry-failure comparisons pass. The harness exits
**2**, with `status: inconclusive`: **9 of 384 measured executions and 3 of 24
warmups** exceed the CPU consistency tolerance. The two independent probes do
not exceed that generous threshold in this run. These are observations on this
environment, not a diagnosis of the underlying clock/accounting cause or a claim
that every WSL installation behaves this way.

| Workload | Elapsed median before → after, ms | Elapsed MAD before → after, ms |
|---|---:|---:|
| 128 successive locals | 30.543 → 30.295 | 1.454 → 1.444 |
| 48 calls with 128 locals each | 730.035 → 723.316 | 14.818 → 13.609 |
| 128 simultaneously live locals, identical binaries | 30.444 → 30.144 | 1.490 → 1.763 |

These elapsed differences do not establish a speedup, and the inconsistent CPU
accounting cannot establish absence of a small regression. No samples were
excluded and no times were corrected. The deterministic frame/ELF reductions
documented in [local lifetimes](numeric-local-lifetimes.md) remain confirmed.
A consistent-clock environment and an adequately stable comparison are still
needed for a runtime-cost conclusion; this slice makes that limitation explicit
and machine-readable.
