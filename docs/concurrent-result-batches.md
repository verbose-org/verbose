# Bounded result batches

Design fixed before implementation, 2026-09-24; now implemented. The first
[native concurrency measurements](concurrent-benchmark.md) identify per-result
rendezvous as a substantial cost. This slice bounds and amortizes those exchanges;
it does not change the pure phase subset or promise parallel throughput.

## Language contract

Concurrent executions may declare `result_batch: B`, an integer in 1..1024,
defaulting to 1. Sequential executions refuse this field. Every declaration,
including an unselected one, is checked. This is a pending-result capacity,
not a requirement to delay each result until exactly B results exist.

Up to B completed values may be retained per worker/channel, plus one value
being published by the reference coordinator. Input storage and temporary
evaluation values are outside this count. The interpreter uses a channel with
B-1 queued slots and at most one value in a blocked send. Native workers share
one fixed serialized batch with the coordinator and wait for its consumption
before starting the next batch. They may publish fewer than B results at end of
input or before a recoverable input/evaluation failure.
Native batching can delay the first visible result while the worker fills its
buffer. B=1 retains per-record publication; a larger capacity is an explicit
storage/latency tradeoff, not an automatic choice inferred from idle memory.

Admission remains in consecutive waves. Publication retains phase order then
record order. Boolean false remains sticky until its complete phase is
published. An invalid record first flushes all preceding complete records from
its partially filled native batch, then reports failure. No later phase/wave
becomes visible. Cancellation may discard unpublished later-phase batches; it
still checks between records and joins every admitted worker. Partial admission
failure publishes none of the partially admitted wave. Fatal host failures have
the existing process-level boundary.

Output writes may now contain several results. Partial writes advance the byte
cursor, EINTR retries, and other failures cancel/join as before. A batch is not
an atomic output transaction. Already accepted output is an ordered byte prefix;
neither result-boundary atomicity nor rollback is promised on output failure.

## Native layout and control flow

Keep the existing 64-byte lane control block. For B > 1 its words are:

| Offset | Meaning |
|---:|---|
| 0 / 4 | Atomic state / kernel-cleared TID (32 bits each) |
| 8 | Serializer's record length while filling; batch length when READY |
| 16 | Terminal success/sticky-failure status |
| 24 | Serializer's next record destination |
| 32 | Worker stack top |
| 40 | Immutable batch base |
| 48 | Committed complete-record bytes |
| 56 | Complete-record count |

The existing serializers continue to write at offset 24 and finish by storing
their record length at offset 8. Only after successful serialization does the
worker add that length to offsets 24/48 and increment offset 56. Before each
record, count < B, so one complete result's proved maximum still fits. No byte
from an incomplete record contributes to the published length. Shared lengths
are read by the coordinator only after READY publication.

On a full batch: copy committed length to offset 8, publish READY with the
existing locked transition, wake, then wait for EMPTY or CANCEL. After EMPTY,
reset destination to base and committed bytes/count to zero. At end or input
failure, save terminal status (1 for an input error), flush any committed partial
batch, then publish DONE. Native argv failures retain status 1 with no contextual
stderr; interpreter errors retain their phase/record diagnostic. Cancellation
wins every publication transition. The coordinator reads
the immutable base, writes the entire published length, and acknowledges once
per batch. Native scalar and bounded-text bodies and worker stack bounds stay
unchanged. The B=1 emitter path retains its existing instruction bytes.

Each lane allocates `B * max(per-record serialized capacity of assigned phases)`
bytes, using checked arithmetic. Control and output are padded/page-rounded as
before; worker stacks/guards are unchanged. `--memory-report` exposes B, lane
per-record capacities and total batch capacities. The actual reservation must
fit `native_memory` and the 256 MiB implementation ceiling. There is still one
upfront mapping, no per-record allocation, no GC and no busy waiting. Code,
initial argv/environment and kernel/output storage remain outside the ceiling.

## Validation and measurements

Implement the interpreter reference before native emission. Test closed syntax
and programmatic AST gates; default/explicit 1 equivalence; exact/minus-one
reservations; checked large-capacity refusal; full and partial batches; malformed
input after a valid prefix; false values at batch boundaries; UTF-8, NUL and flat
records; multiple waves; cancellation/backpressure; admission/output syscall
failures; joining before reuse; and original-interpreter/native parity.

Keep existing default native examples byte-identical to the reference compiler.
WASM/self-hosted execution remains refused before output. Run serialized normal
tests, CLI checks, CIDX and bootstrap. Retain the historical benchmark; compare
fresh sequential, default concurrent, and batches 8/32/128 on identical inputs
with balanced repeated order, independent output oracles and separate memory/
syscall observations. No build/test overlaps timing. Record current host load
and retain all observations. Phase-order backpressure still limits useful compute
overlap on long lots even if larger batches reduce synchronization costs.

## Reproduce the comparison

Build both compilers before timing. The reference is the implementation before
result batching (`eee1124`, also present at merge `29d8f3c`). The harness refuses
to measure if any of its three sequential or three default-concurrent controls
differs in native bytes from that reference. Explicit `result_batch: 1` is also
checked against the omitted field by the CLI tests.

```sh
python3 tools/test_concurrent_benchmark.py -v
python3 tools/benchmark_result_batches.py \
  --compiler target/release/verbosec \
  --reference-compiler /path/to/pre-batch/verbosec \
  --reference-revision eee1124605344846a2cbc0861d43bacfd9ff569a \
  --cpus 2 4 6 8 --repeats 30 \
  --host-note "Describe current host load" \
  --output /tmp/result-batches.json
```

The five modes are sequential and concurrent limit 4 with batches 1/8/32/128.
Five workloads retain the [initial benchmark's](concurrent-benchmark.md)
input/oracle and timing/memory definitions. Thirty rotated rounds balance every
mode's position; two warmups per mode precede timing. Each ratio is paired
within the same round against a freshly measured sequential or batch-1 control,
not against yesterday's readings. Raw observations and median/range/MAD remain
available. Native output is checked on every timed input before measurement;
independent arithmetic and original-interpreter checks precede all timing.

Three separate memory snapshots per workload/mode retain the same blocked-output
conditions and guards/reservation checks. CPU-accounting anomalies produce
`status: inconclusive` and exit 2; functional or execution failures exit 1.
No sample is discarded. Batching also reduces the number of stdout writes;
these measurements compare complete implementations, not isolated futex costs.
The new benchmark and its helper hashes are recorded; the historical report
and original benchmark remain available.

## Recorded result, 2026-09-24

[Raw timing/memory report](measurements/concurrent-result-batches-2026-09-24.json),
[separate syscall and fault-injection observations](measurements/result-batch-syscalls-2026-09-24.json).
Measured release compiler/source revision:
`04af394b3b6ce7db04a77fe9f5d26f3d3cf5b907`, with a clean tree.
The Ryzen 7 5800X was exposed through WSL2, kernel
`5.15.153.1-microsoft-standard-WSL2`, with affinity to guest CPUs 2, 4, 6, 8
(reported distinct cores). The user reported no significant host load; no
build, test suite or other benchmark overlapped timing. Guest affinity does
not control the Windows scheduler.

Median elapsed milliseconds, 30 samples per cell. Concurrent limit is 4;
the three-phase readings workload admits three workers.

| Workload | Records | Sequential | Batch 1 | Batch 8 | Batch 32 | Batch 128 |
|---|---:|---:|---:|---:|---:|---:|
| Light arithmetic | 1 | 0.252 | 0.625 | 0.619 | 0.616 | 0.608 |
| Light arithmetic | 4096 | 3.502 | 796.025 | 94.502 | 25.166 | 7.596 |
| Compute | 1 | 1.848 | 1.556 | 1.680 | 1.617 | 1.579 |
| Compute | 256 | 63.099 | 112.013 | 68.609 | 59.261 | 41.180 |
| Mixed readings | 4096 | 4.810 | 593.977 | 72.693 | 20.900 | 7.330 |

Batch 32 reduces elapsed time relative to batch 1 by about 31 times for the
long light workload and 29 times for readings (reciprocals of paired median
ratios 0.03187 and 0.03485). Both remain slower than sequential execution,
including at batch 128. Compute/256 at batch 128 has a paired median ratio
of 0.6463 to sequential, about 35% less elapsed time on this synthetic workload.
Ordering still constrains overlap: a worker fills one batch, then waits until
earlier phases have published their complete input lot.

For the three long workloads in table order, elapsed MADs for sequential /
batch 1 / batch 32 / batch 128 are respectively 0.165 / 7.677 / 0.865 / 0.311 ms,
1.415 / 0.857 / 0.734 / 1.048 ms, and 0.144 / 6.800 / 0.606 / 0.228 ms.
The one-record compute case is much more dispersed: sequential spans
0.894–3.111 ms with MAD 0.400 ms, and batch 128 spans 0.948–3.721 ms with
MAD 0.260 ms. It does not establish a reliable small-job speedup. Timings
include launch, parsing, serialization and output, not only arithmetic.

Median aggregate CPU time on compute/256 is 65.175 ms sequentially and
69.017 ms with batch 128: the wall-time improvement is not a reduction in
CPU consumption. For light/4096, CPU time falls from 662.651 ms at batch 1
to 20.688 ms at batch 32 (sequential: 3.278 ms); voluntary context switches
fall from 32637.5 to 1022.5 (sequential: 1). CPU capacity/probe checks flag
no inconsistency but do not calibrate WSL accounting. No energy claim is made.

### Memory and syscall observations

Three separate snapshots per workload/mode confirm the compiler's reservation:

| Workload / admitted workers | Batches | Reserved mapping | Resident within mapping | Guards, all nonresident |
|---|---|---:|---:|---:|
| Numeric / 4 | 1, 8, 32 | 36 KiB | 20 KiB | 16 KiB |
| Numeric / 4 | 128 | 44 KiB | 28 KiB | 16 KiB |
| Mixed readings / 3 | 1, 8, 32 | 28 KiB | 16 KiB | 12 KiB |
| Mixed readings / 3 | 128 | 32 KiB | 20 KiB | 12 KiB |

Batch 32 uses capacity within already reserved pages in these examples;
this is not guaranteed for other result types or lane counts. The shipped
two-worker example also retains its 20 KiB reservation at batch 32.
Light/4096 whole-process snapshot RSS is 60–68 KiB sequentially, 84–88 KiB
at batch 32 and 92–96 KiB at batch 128. These are blocked-output snapshots,
not peak RSS. The compute fixture expands to roughly 3 MB of code; its
828 KiB sequential versus roughly 3060 KiB concurrent snapshot largely
reflects which phases' instruction pages have already been touched. Code,
initial input and kernel storage remain outside `native_memory`. Neither
cache residency nor cache hits were measured.

Separate traced light/256 executions produce 1024 results:

| Mode | Writes | Futex calls |
|---|---:|---:|
| Sequential | 1024 | 0 |
| Batch 1 | 1024 | 3614 |
| Batch 8 | 128 | 487 |
| Batch 32 | 32 | 129 |
| Batch 128 | 8 | 36 |

Every concurrent trace has one `mmap`, five `mprotect`, one `munmap` and four
`clone` calls. Batching changes neither per-record allocation nor admission
count. Futex counts depend on scheduling, which tracing itself changes;
traced elapsed times are not performance samples. Reported futex errors include
normal races. Separate batch-32 write injections verify EINTR retry, cursor
advancement after a simulated short write, failure after a complete batch,
and zero-write failure with cancellation. A simulated short write reports a
byte accepted without actually writing it; its expected captured output omits
that byte. This is a control-flow check, not an actual partial-write delivery.

Validation retains 750 timed runs, 50 warmups, two clock probes and 45 memory
snapshots, with `status: ok`, no discarded samples and no flagged CPU
inconsistency. All 15 builds pass independent 201-record output oracles and
seven-record original-interpreter comparisons; all 25 full timed-input checks
pass. Six sequential/default-concurrent controls are byte-identical to the
reference compiler. The normal serialized suite passes 835 tests, bootstrap
26, CLI checks 33 and benchmark unit checks 6; 24 focused concurrent tests
also pass in release mode. The archived 187-example corpus keeps identical
acceptance/diagnostics, with all 184 accepted binaries byte-identical across
the old and new compilers and repeated builds.

These measurements support explicit bounded batching for sufficiently costly
work. They do not select a universal batch size: larger batches increase
storage and can delay the first visible result. Cheap phases still favor
sequential execution here. HTTP throughput, general thread-pool behavior and
other hardware remain separate measurements.
