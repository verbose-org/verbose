# Measuring native bounded concurrency

This compares sequential and concurrent execution of the same pure phases in
the Linux x86-64 native backend. It measures the implementation added in PR #248,
not HTTP throughput, a general thread pool, or a comparison with C/Apache.
The compiler and emitted runtime are unchanged by this measurement work.
The subsequent [bounded result-batch comparison](concurrent-result-batches.md#recorded-result-2026-09-24)
measures the follow-up implementation against fresh sequential/default controls;
the historical observations below remain unchanged.

## Recorded result, 2026-09-23

[Raw timing/memory report](measurements/concurrent-native-2026-09-23.json),
[separate syscall observations](measurements/concurrent-syscalls-2026-09-23.json).
Compiler revision `eee1124605344846a2cbc0861d43bacfd9ff569a`, release build;
Ryzen 7 5800X exposed through WSL2, kernel `5.15.153.1-microsoft-standard-WSL2`.
Affinity was guest CPUs 2, 4, 6, 8, reported as distinct cores. This does not
control the Windows host scheduler. The user reported no significant host load;
no build, test suite or other benchmark overlapped the recorded runs.

**No tested workload became faster than sequential execution.** The fixed
reservation behaved as specified, but per-result synchronization made long lots
substantially slower. These numbers are a baseline for this runtime on this
machine, not a universal cost of threads or bounded memory.

Median elapsed milliseconds, 32 samples per cell (four phases unless noted):

| Workload | Records | Sequential | 1 worker | 2 workers | 4 workers |
|---|---:|---:|---:|---:|---:|
| Light arithmetic | 1 | 0.250 | 0.670 | 0.593 | 0.622 |
| Light arithmetic | 4096 | 3.576 | 784.247 | 788.423 | 788.736 |
| Compute | 1 | 0.743 | 1.282 | 0.900 | 0.815 |
| Compute | 256 | 62.632 | 112.352 | 112.128 | 112.652 |
| Mixed readings, three phases | 4096 | 4.732 | 587.120 | 583.454 | 585.070* |

\* Limit 4 admits three workers for three phases.

The paired median elapsed ratios for limit 4 are 219.90 for light/4096, 1.806
for compute/256 and 123.38 for readings/4096. Their sequential → concurrent
elapsed MADs are respectively 0.077 → 17.288 ms, 1.002 → 1.402 ms and
0.134 → 7.258 ms. The smaller single-record compute difference is sensitive to
launch overhead and dispersion (MAD 0.041 → 0.047 ms); it does not establish a
useful parallel speedup.

On light/4096, limit 4 records a median 637.066 ms of aggregate CPU time and
32667.5 voluntary context switches, versus 3.203 ms and 1 switch sequentially.
Most reported concurrent CPU time is system time (median 620.523 ms). CPU
accounting is an observation from this kernel: no capacity/probe inconsistency
was flagged, but this check is deliberately loose and not a clock calibration.

The untimed `strace -f -c` runs show 3748 futex calls for 1024 results
(four phases × 256 records) with limit 4; sequential emission uses no futex.
Both write 1024 results. Futex counts depend on scheduling, and tracing changes
that scheduling; use the separate untraced samples for timings/context switches.
The concurrent trace reports one `mmap`, five `mprotect`, one `munmap` and four
`clone` calls for both one record and 256 records. Allocation/thread admission
does not scale per record in these fixtures. Syscall error counts in the raw
summary include normal futex races; the executions returned success.

This agrees with the [implemented rendezvous protocol](native-concurrent-executions.md#rendezvous-and-cleanup):
each result wakes the coordinator, and the worker must wait for its buffer to be
consumed before computing another. Later phases can compute one result ahead,
then wait while earlier phases publish their complete lot. Increasing the lane
count therefore provides little useful compute overlap on long lots with equal
phase costs. Bounded result batches could reduce handoff frequency; they would
still need explicit capacities, unchanged publication/failure semantics and new
measurements. They would not automatically remove the phase-order constraint.

The memory snapshots confirm the numeric fixtures' compiler reservation:

| Admitted workers | Reserved mapping | Resident bytes in that mapping | Inaccessible guards | Observed total threads |
|---|---:|---:|---:|---:|
| 1 | 12 KiB | 8 KiB | 4 KiB | 2 |
| 2 | 20 KiB | 12 KiB | 8 KiB | 3 |
| 4 | 36 KiB | 20 KiB | 16 KiB | 5 |

Those mapping sizes and resident bytes match in all three snapshots per fixture.
All guards have zero RSS. For the mixed reading example at limit 2, the whole
process RSS snapshot is 120–124 KiB, compared with 104–108 KiB sequentially;
the execution mapping itself remains 20 KiB reserved / 12 KiB resident.

The compute binaries contain about 3.05 MB of expanded code (3,044,007 bytes
sequential, 3,048,180 at limit 4). Their snapshot RSS is 828 KiB sequentially
and 3060 KiB at limit 4, largely reflecting which phases' code has already been
touched. This is why a small `native_memory` reservation is not evidence that
the complete program fits in a processor cache, or that whole-process peak RSS
has the same bound. Cache hits were not measured.

Validation: 12 generated binaries pass the independent 201-record oracle and
seven-record interpreter comparisons; all 20 complete timed-input comparisons
pass. The report retains 640 timed runs, 40 warmups, two independent clock probes
and 36 memory snapshots, with `status: ok` and no flagged CPU inconsistency.
The five harness unit tests pass. The legacy sequential mixed-text emitter
prints an instruction-decoder warning at compile time; its exact diagnostic is
retained in the report. Native runtime output/status checks still pass. This
measurement work neither changes nor claims to repair that existing validator.

## Protocol

[`benchmark_concurrent.py`](../tools/benchmark_concurrent.py) uses the same
release compiler for sequential execution and `max_in_flight` 1, 2 and 4. Each
phase processes the original argv batch, and publication follows phase order.
All builds finish before timing. Generated sources and executables remain in
the artifact directory; the report records their hashes, compiler and harness
hashes, imported example hashes, source revision, diagnostics and configuration.

Before timing, each binary checks 201 records against an independent Python
oracle, including signed division truncation, phase order and UTF-8 output.
Seven boundary/representative records per build also match the original-AST
interpreter. The complete input used for each timed case is checked separately.
Successful checks require exact stdout, empty runtime stderr and exit status 0.
These benchmarks exercise successful executions; failure behavior is covered by
the compiler's concurrent execution tests, not timed here.

Two warmups per mode precede 32 measured rounds. Mode order rotates each round,
giving each mode an equal number of appearances in each position. No sample is
dropped. Summaries retain median, range and median absolute deviation (MAD).
Ratios compare each concurrent sample to the sequential sample in its round.

Argument vectors are prepared before timing. Elapsed time includes process
launch, argument parsing, calculation, serialization, writes to `/dev/null` and
waiting for termination. It includes Python/OS launch overhead; sub-millisecond
results are especially sensitive to that overhead. This compares the complete
execution modes, including their existing lowering paths, rather than isolating
thread creation or a single arithmetic kernel.

Child CPU time sums coordinator and worker user/system time. It may exceed
elapsed time. The harness flags CPU accounting exceeding elapsed time multiplied
by the smaller of allowed CPUs and admitted threads, with a diagnostic tolerance
of `max(20 ms, 5% of that bound)`. Independent single-thread clock probes run
before and after timing. Status `inconclusive` (exit 2) retains questionable CPU
readings; `failed` (exit 1) means a correctness or execution check failed. Status
`ok` does not prove a speedup, stable clocks, or absence of a small regression.

## Workloads and memory observations

- **Light arithmetic:** four distinct phases return `input + phase_number`.
  One record shows launch/admission costs; 4096 records expose per-result costs.
- **Compute:** four phases each sum 96 calls to a leaf containing 127 successive
  signed divisions, then add the phase number. The leaf starts at 17 and applies
  `(previous + input) / 2`, with input bounded to `[-100, 100]`. One record and
  256 records distinguish a single independent result from a phase-ordered lot.
  This is deliberately synthetic. Acyclic native expansion produces a large
  instruction footprint; these are not 96 cached calls or a realistic application.
- **Mixed readings:** the existing `clamp`, `nonnegative`, `label` rules over
  4096 named readings, including UTF-8 titles, produce numbers, booleans and text.
  With three phases, limit 4 admits three workers.

All phases use successful inputs. The generated ceilings are deliberately
sufficient (1 MiB); reported actual reservations, rather than these ceilings,
describe allocated storage. A concurrent limit of 1 still creates worker threads
and uses rendezvous; it is useful for exposing that overhead against sequential
execution.

Memory is measured in separate executions with 4096 records and a 4096-byte
output pipe. Once the coordinator is blocked writing to the undrained pipe,
the harness reads `/proc/PID/smaps` and counts threads. It then drains and checks
the entire output and status. Three independent snapshots per mode/workload
check the anonymous reservation against the compiler report and verify that
guard pages have no resident bytes.

These are **snapshots under output backpressure, not peak RSS**. In particular,
later sequential phases have not necessarily touched their instruction pages,
while admitted concurrent phases have. Do not present their snapshot RSS ratio
as a ratio of whole-execution peaks. Code, initial argv/environment and kernel
storage are outside `native_memory`; smaps RSS itself excludes kernel thread
storage and external output storage. No hardware cache counter, cache residency,
energy consumption or whole-machine memory claim is made.

## Reproduce

Build the compiler first; run no other builds/tests/benchmarks during timing.
Choose allowed CPUs on distinct cores according to the exposed host topology:

```sh
cargo build --release
python3 tools/test_concurrent_benchmark.py -v
python3 tools/benchmark_concurrent.py \
  --compiler target/release/verbosec --cpus 2 4 6 8 --repeats 32 \
  --host-note "Describe current host activity" \
  --output /tmp/concurrent-measurement.json
```

The measurement tests check oracle ordering/signed arithmetic, UTF-8 output,
CPU-accounting limits, smaps parsing and paired summaries without performance
thresholds. The full harness checks generated fixtures and memory observations
before reporting success. Python's standard library is sufficient; Linux procfs
access and the ability to observe `pipe_write` are required for memory snapshots.
