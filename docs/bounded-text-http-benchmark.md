# Bounded text branch placement under HTTP load

This compares the same pooled HTTP program before and after exclusive branch
storage reuse. Both compilers must emit equal-size binaries differing only in
frame sizes and buffer address immediates. The fixture's reserved handler frame
falls from **8352 to 4256 bytes**, excluding the unchanged 48-byte allowance and
the surrounding input/transport frame. It saves one 4096-byte text buffer.

## Reproduce

Build a reference compiler from `555e402` (before overlay), retaining its binary
separately, and build the current compiler. On Linux x86-64:

```sh
cargo build --release --locked
python3 tools/test_http_load.py -v
python3 tools/benchmark_text_branches.py \
  --reference-compiler /path/to/reference-verbosec \
  --reference-revision 555e402 \
  --server-cpus 0,2,4,6 --client-cpus 8,10,12,14 \
  --requests 100000 \
  --output /tmp/bounded-text-http.json
```

Choose CPU sets permitted by the host and without shared SMT siblings. Affinity
does not reserve cores against other applications or Windows activity under WSL.
Omit those options to use the current affinity. Only Python's standard library
and `rustc` are needed by the harness. The load client is compiled with `rustc -O`;
both servers are emitted directly by their supplied Verbose compilers.

The reference revision is recorded as supplied, with both compiler hashes.
The harness checks the actual emitted binary difference, not that label: passing
the same compiler twice must fail before a load starts. Sources, binaries,
compiler logs and server logs remain in the printed temporary directory.
The report preserves every run, source/hash, CPU topology, client/server CPU
time and memory sample. Errors leave a failed partial report and a nonzero exit.

## What is measured

The generated `.verbose` source declares a pure `piece` rule with a 4096-byte
text result capacity. It copies the counted request body. A handler selects
`HttpResponse` records whose bodies call `piece` in opposite arms of an `if`.
Requests alternate `/a` and `/b` per client; both return the exact binary body.
This exercises both writable destinations while preserving identical behavior.
The source uses the language's bounded text contract; this is not a hand-written
Rust HTTP handler.

Both variants use the same HTTP/1.0 transport, port, request limit and two-second
receive/send deadlines, with one or four worker processes and matching client
threads. Empty, 1024-byte and 3900-byte bodies vary by client/request, including
NUL and `0xff`. Every response must have the exact header/body and end in EOF;
corruption, truncation, trailing bytes or timeouts are failures.

The recorded longer run starts each variant fresh, warms up for 500 requests,
then serves 100,000 timed requests. The tool defaults to 30,000 for a shorter
run; the command above explicitly increases this count. Three paired repeats
alternate variant order per scenario. Compile,
startup, readiness, warmup and `/proc` sampling are outside timed throughput.
Latency includes connect through response verification and EOF. Goodput includes
client work; clients issue their next request only after completion, so this is
closed-loop load, not a test of independent arrivals or overload. Successful
request p50/p95/p99/max use nearest rank; failures are reported separately.

Server CPU time sums supervisor/worker `/proc` user/system ticks around each
timed load. Its resolution is the recorded host clock tick; it is not an
instruction count. Client CPU time is recorded separately. Neither measures
CPU cache misses. No cache-counter values are synthesized from frame size.

Before/after snapshots observe idle workers, and a separate memory phase runs
four 2000-request batches after warmup, alternating empty and large bodies.
PIDs, descriptor counts and idle stack positions must remain stable within a
server instance. Summed PSS apportions shared pages; summed RSS may count them
multiple times. These samples exclude kernel socket buffers, are not peak
memory, and do not bound whole-process or whole-service memory.

This fixture and the instruction traces answer different questions: the traces
pin evaluation/copy semantics; this benchmark observes end-to-end cost on one
host. Smaller frames do not establish cache residency or a portable speedup.
It does not measure HTTPS or compare with Apache.

## Measurement recorded on 2026-09-16

The [complete longer report](measurements/bounded-text-http-2026-09-16.json)
compares the pre-overlay compiler from `555e402` with the integrated compiler
containing `a7b6335`. The checkout at start was `78c6e84`; its compiler/runtime
sources are identical to `a7b6335`. Both executable hashes and the measured
harness/client hashes are recorded. The host was a Ryzen 7 5800X under WSL2
`5.15.153.1-microsoft-standard-WSL2`, with the disjoint CPU sets in the command
above. After the run, the operator confirmed that no substantial concurrent
workload was running during the measurement. This is reported operating context,
not measured host isolation. The raw JSON reports retain their original
"Host activity outside Linux not reported" note, recorded before this clarification.

The [initial calibration](measurements/bounded-text-http-calibration-2026-09-16.json)
used 30,000 requests per passage. Its 1.20–4.52-second windows motivated increasing
every scenario to 100,000 requests, producing 4.08–14.25-second windows. Both full
reports are retained. The shorter run's median goodput differences ranged from
−3.8% to +4.5%; it is not discarded because some results were unfavorable.

Rates below are medians of three runs, with minimum–maximum in parentheses.
The p99 column is the median of each run's p99, not a pooled percentile. Server
CPU seconds sum supervisor and worker ticks per 100,000-request passage.

| Workers / clients | Body bytes | Before responses/s | After responses/s | Median change | p99 µs, before → after | Server CPU seconds, before → after |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 7,832 (7,817–7,849) | 7,876 (7,757–7,887) | +0.6% | 269.6 → 260.6 | 7.62 → 7.57 |
| 1 | 1024 | 7,380 (7,358–7,472) | 7,372 (7,296–7,525) | −0.1% | 287.8 → 274.2 | 8.00 → 8.10 |
| 1 | 3900 | 7,083 (7,048–7,101) | 7,082 (7,017–7,099) | ≈0% | 284.1 → 281.7 | 8.20 → 8.22 |
| 4 | 0 | 23,248 (22,671–24,223) | 23,968 (23,452–24,506) | +3.1% | 464.8 → 366.1 | 9.80 → 9.73 |
| 4 | 1024 | 21,683 (21,642–22,350) | 21,513 (21,455–22,934) | −0.8% | 442.7 → 476.4 | 10.71 → 10.65 |
| 4 | 3900 | 21,802 (21,593–21,986) | 21,354 (20,953–22,002) | −2.1% | 383.9 → 470.9 | 10.61 → 10.67 |

The smaller layout does not demonstrate uniformly better timing. Median goodput
changes span −2.1% to +3.1%, with overlapping run ranges in every scenario. Median
p99 improves in four scenarios and worsens in two, including a 22.7% increase
for four workers and 3900-byte bodies. The shorter run gives a different direction
for some cases. Three repeats on a shared development host establish neither
performance equivalence nor that placement caused those timing differences.
The follow-up below increases repeats for the four-worker tails on the same
host after the operator reported no substantial concurrent workload. Host
isolation and hardware-counter measurements remain outside these experiments.

Memory samples below include the supervisor and are identical across all five
snapshots after 500, 2500, 4500, 6500 and 8500 requests in the longer run:

| Workers | Summed RSS KiB, before → after | Summed PSS KiB, before → after | Private KiB, before → after |
|---:|---:|---:|---:|
| 1 | 120 → 116 | 72 → 68 | 24 → 20 |
| 4 | 332 → 316 | 122 → 106 | 72 → 56 |

This run observes one fewer private 4 KiB page per worker, alongside the proved
4096-byte reduction in its reserved handler frame. The short calibration also
saved these private pages, but its one-worker PSS remained 72 KiB and summed RSS
rose from 120 to 124 KiB. A frame reduction therefore must not be presented as
an unconditional equal RSS/PSS reduction. Neither run measured cache misses.

All **3,652,000 requests** in the longer run passed the exact-byte oracle:
3,600,000 timed requests, 18,000 warmups and 34,000 memory-phase requests. The
calibration's 1,132,000 requests also passed. PIDs, descriptor counts and idle
stack positions remained stable within every instance; servers stayed alive
and produced no stdout/stderr until harness shutdown. Shutdown uses process-group
termination and does not test graceful draining.

The six controlled-client tests, the native smoke run, and the identical-compiler
negative control passed. The latter fails before issuing requests when no frame
reduction exists. Report validation checks source hashes, scenario/repeat counts,
all request totals and zero failures. This measurement slice changes tools and
documentation; compiler/runtime sources remain unchanged from the merged series.

## Follow-up with ten pairs on 2026-09-16

The [complete follow-up report](measurements/bounded-text-http-followup-2026-09-16.json)
records the follow-up from 09:54:48 to 10:03:58 UTC, using the same
compiler and harness hashes as the longer run above. The checkout was clean at
`459e8a2`. Before this run, the operator confirmed no substantial concurrent
workload. No local test or build suite ran during timing. The CPU sets remained
unchanged; this reported context and affinity still do not establish host
isolation under WSL2. Hardware cache counters were not measured.

The protocol was chosen before the run: four workers/clients only, all three
body sizes, ten pairs per size, and 200,000 timed requests per passage. Five
pairs run before/after and five after/before, alternating in the recorded order.
Each variant still starts fresh and receives 500 warmup requests. All 60
passages are retained; durations range from 7.90 to 8.98 seconds. Reproduce with
the same compilers and permitted CPU sets:

```sh
python3 tools/benchmark_text_branches.py \
  --reference-compiler /path/to/reference-verbosec \
  --reference-revision 555e402 \
  --workers 4 --payloads 0,1024,3900 \
  --requests 200000 --repeats 10 \
  --server-cpus 0,2,4,6 --client-cpus 8,10,12,14 \
  --host-note 'Describe concurrent host activity for this run' \
  --output /tmp/bounded-text-http-followup.json
```

Each entry below is a median across ten passages. The p99 is the median of
per-passage p99 values, not a pooled percentile. Percentage changes compare
these medians; positive latency changes mean slower responses.

| Body bytes | Before responses/s | After responses/s | Median change | p99 µs, before → after | p99 change |
|---:|---:|---:|---:|---:|---:|
| 0 | 24,733 | 24,957 | +0.9% | 369.2 → 349.6 | −5.3% |
| 1024 | 23,411 | 23,196 | −0.9% | 371.0 → 370.7 | −0.1% |
| 3900 | 22,820 | 22,650 | −0.7% | 369.6 → 374.2 | +1.3% |

Pairwise changes provide a separate view of variability. They are calculated
as `100 × (after / before − 1)` for each adjacent pair, then summarized; they
need not equal the change between the two medians above.

| Body bytes | Paired goodput change, median (min–max) | Paired p99 change, median (min–max) | Pairs with higher after p99 |
|---:|---:|---:|---:|
| 0 | +0.6% (−2.1% to +8.0%) | −4.0% (−19.5% to +3.1%) | 2 / 10 |
| 1024 | −0.7% (−4.3% to +3.9%) | +1.0% (−10.6% to +11.0%) | 6 / 10 |
| 3900 | −0.6% (−2.7% to +2.5%) | +3.4% (−12.3% to +9.3%) | 6 / 10 |

Order remains a material limitation. For 3900-byte bodies, the median paired
p99 change is +5.1% when the after variant runs second, versus −7.8% when it
runs first. For 1024-byte bodies those values are +3.3% and −6.2%. Each group
has only five pairs, so this does not establish a cause; it does show why a
single aggregate should not be interpreted as a pure placement effect.

The previous +22.7% median p99 difference for large bodies is not reproduced
at that magnitude here. Goodput medians differ by less than 1%, while a small
placement cost or benefit remains unresolved. These results support retaining
the memory optimization for this fixture, without claiming zero runtime cost,
performance equivalence, or improved cache residency. A further causal timing
study should address order/carryover effects and host isolation, using a
predefined schedule and, if available, hardware counters.

The reserved handler frame again falls from 8352 to 4256 bytes. Equal-size
41,103-byte binaries differ only in frame/address immediates. All five idle
memory snapshots match the previous four-worker result: summed RSS 332 →
316 KiB, PSS 122 → 106 KiB, and private memory 72 → 56 KiB, including the
supervisor. This again observes 4 KiB less private memory per worker in this
fixture; it is not a general process-memory or cache bound.

All **12,047,000 requests** pass: 12,000,000 timed, 30,000 warmups and 17,000 in
the separate memory phase. Every pool retains its PIDs, descriptors and idle
stack positions within an instance, and all 124 server stdout/stderr logs are
empty. Report validation checks all request totals, pair order, source and
binary hashes, the layout-only difference, and pool identity. The raw report
is preserved alongside both earlier measurements; no compiler, runtime or
benchmark code changed for this follow-up.
