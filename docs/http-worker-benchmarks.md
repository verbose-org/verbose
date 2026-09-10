# HTTP worker baseline

This measures the same native echo handler with binary bodies in `concurrency: forked`
and `concurrency: pooled`. It establishes a baseline for the
[reusable worker contract](pooled-http-workers.md), including memory reuse.
It does not measure HTTPS, Apache, or production capacity.

## Reproduce

On Linux x86-64, with Python 3.10+, rustc 1.73+, and access to loopback sockets and
the child processes' `/proc` files:

```sh
cargo build --locked
python3 tools/test_http_load.py -v
python3 tools/benchmark_http.py --output /tmp/http-baseline.json
```

No Python packages or Rust crates are needed by the harness. The compiler build
uses the repository's usual dependencies. The client is compiled with `rustc -O`.
`--compiler` selects an already-built verbosec. Native service machine code comes
from Verbose's emitter; the Rust compiler's optimization flag applies to the load
client, not to the generated service.

Optional `--server-cpus` and `--client-cpus` accept comma-separated logical CPU
numbers and require `taskset`. Choose sets allowed by the current affinity, and
inspect `/sys/devices/system/cpu/cpuN/topology/thread_siblings_list` to avoid
placing the client on the server's SMT siblings. These affinities do not reserve
cores against unrelated workloads. The measured run used:

```sh
python3 tools/benchmark_http.py \
  --server-cpus 0,2,4,6 --client-cpus 8,10,12,14 \
  --output /tmp/verbose-http-baseline-2026-09-10.json
```

The JSON records the revision, working-tree status, compiler/client/harness
hashes, generated service sources and binary hashes, hardware, affinity, relevant
network settings, every run's results, and per-process memory snapshots. Generated
binaries, sources, and server stdout/stderr remain in a printed temporary
directory. Partial reports survive failures, which also cause a nonzero exit.
The client has five-second socket operation timeouts; `--load-timeout` separately
bounds a whole client invocation (120 seconds by default).

## Method

- One fresh TCP connection per HTTP/1.0 request, using the handler from
  `examples/http_pooled.verbose`. Both modes have a 4096-byte request buffer and
  two-second receive/send deadlines. No logging, disk access, TLS, or keep-alive.
- One or four blocking client threads; the pool has the matching worker count.
  Each client waits for the response and connection close before its next request.
  This is a closed-loop test. It cannot characterize overload or the latency of
  arrivals that would continue independently while the server is stalled.
- The forked service is **uncapped**. `max_connections` is deliberately absent:
  its immediate overload-close policy differs from the pool's kernel queue.
  Client count limits outstanding requests in the test, not forked process count
  in the language contract.
- Empty, 1024-byte, and 3900-byte bodies. Binary payloads vary by request and client,
  with an identifying eight-byte prefix for the larger bodies. The oracle checks
  the exact status line, headers, body, absence of trailing bytes, and EOF for
  every response. Incorrect responses are failures, not successful throughput.
- Each mode gets a fresh service, 500 warmup requests, then 30,000 measured
  requests. Three repeats per scenario alternate mode order. Server compilation,
  startup, readiness probes, and warmup are outside measured throughput.
- Goodput counts successful responses over wall time, including client work.
  Per-request latency starts before connect and ends after the byte check and
  close; body generation is outside that latency but inside throughput time.
  JSON p50/p95/p99/max use nearest rank over successful requests only; errors are
  counted separately by connect/write/read/response category. Client user/system
  CPU time is recorded separately; server CPU consumption is not measured.

## Memory accounting

A separate pool run warms up with 500 large bodies, then processes four batches
of 10,000 requests, alternating empty and large bodies. After each batch, all
workers must return to `accept`. Worker PIDs, open descriptor counts, and idle
stack pointers must match the first snapshot. Samples include the supervisor.

`smaps_rollup` RSS counts resident mappings; shared pages can be counted in more
than one process. PSS apportions shared pages among their processes, so summed
PSS is used here for approximate group residency. Measurements read processes
sequentially and exclude kernel socket buffers and other kernel bookkeeping.
See the [Linux accounting documentation](https://docs.kernel.org/filesystems/proc.html).

The memory phase observes the pool after warmup, not its peak during processing
or its startup cost. It does not sample transient forked children or compare the
two modes' total memory. Stable residency is evidence about this handler and
these request shapes, not a whole-language RAM bound or a proof of no leaks.
Reclaiming the request stack permits reuse; it does not wipe old bytes or require
returning resident pages to the OS. The separate pool acceptance tests also cover
a larger temporary response and recovery after a send timeout.

## Recorded measurement

Measured on 2026-09-10 against compiler revision `b26ddc2`, on an AMD Ryzen 7 5800X
(8 cores, 16 logical CPUs), WSL2 kernel `5.15.153.1-microsoft-standard-WSL2`.
Client and server use the separate CPU sets above. This is a development host,
not dedicated benchmark hardware; CPU affinity does not isolate it from Windows
or other activity. The [complete JSON](measurements/http-worker-baseline-2026-09-10.json)
retains all 36 runs, including slow ones. Timed runs lasted 3.70–12.36 seconds.

After the run, the operator reported playing a video game on the host during
the measurements. Concurrent host activity is a possible source of contention;
its timing and resource use were not captured by the harness. Keep this context
with the results: neither the throughput differences nor the latency tails can
be attributed solely to the worker mode or solely to the game. A quiet-host
repeat is still needed. The original measurement JSON is preserved unchanged.

Rates are medians across three runs, with minimum–maximum in parentheses.
The last column is the median of the three run p99 values, not a pooled percentile.

| Clients / pool workers | Body bytes | Forked responses/s | Pool responses/s | Median gain | p99 milliseconds, forked → pool |
|---:|---:|---:|---:|---:|---:|
| 1 | 0 | 4,149 (3,227–4,354) | 4,915 (4,627–5,715) | +18.5% | 0.678 → 1.513 |
| 1 | 1024 | 2,555 (2,428–2,628) | 3,025 (2,893–3,222) | +18.4% | 2.659 → 3.765 |
| 1 | 3900 | 2,634 (2,556–2,657) | 3,184 (3,087–3,488) | +20.9% | 2.217 → 3.005 |
| 4 | 0 | 6,078 (5,205–6,807) | 7,590 (6,137–8,116) | +24.9% | 6.806 → 6.261 |
| 4 | 1024 | 5,303 (5,279–6,006) | 7,080 (6,895–8,116) | +33.5% | 9.027 → 7.597 |
| 4 | 3900 | 6,008 (5,811–6,345) | 7,006 (6,078–7,272) | +16.6% | 6.398 → 6.708 |

The pool has higher median goodput in these six scenarios, but higher median p99
in four. One individual four-client, large-body repeat is slower with the pool.
These observations do not establish which runtime or scheduling mechanism caused
the latency tails. The initial shorter calibration run also produced substantially
different absolute rates; its sub-second windows motivated the longer published
run. Neither the rates nor the relative gains should be treated as portable
capacity guarantees. Investigating tails on a controlled host, then testing
independent arrivals and overload, remains necessary before a production claim.

All **1,179,000** requests passed the byte oracle: 1,080,000 timed requests, 18,000
timing warmups, and 81,000 in the memory phase. Every client exited 0; servers
produced no stdout/stderr and remained alive until harness shutdown. The harness
then kills its service process groups; this does not test graceful shutdown.

| Pool workers | Snapshots after completed requests | Summed RSS | Summed PSS | Summed private pages |
|---:|---|---:|---:|---:|
| 1 | 500; 10,500; 20,500; 30,500; 40,500 | 116 KiB each | 68 KiB each | 20 KiB each |
| 4 | 500; 10,500; 20,500; 30,500; 40,500 | 316 KiB each | 106 KiB each | 56 KiB each |

All five snapshots per pool have identical PIDs, descriptor counts, and idle
stack positions. This supports storage reuse for the measured echo handler;
the memory-accounting limits above still apply.

## Validation

The load client's controlled-peer tests cover binary echo, request distribution
across threads, same-length corruption, truncated/trailing responses, missing EOF,
socket timeouts, an overall client deadline, and invalid arguments. A short native
run exercises both service modes and memory snapshots before the full measurement.
Benchmark failures are not ignored or averaged away.

Recorded checks: all five controlled-peer tests passed, as did the native smoke
run and full measurement. JSON integrity checks confirm the recorded harness and
client source hashes, request totals, and zero client failures. `cidx validate`
passed. `cidx run ci` fails in security because the cargo-audit container lacks
`curl`; it does not reach tests or build. Gitleaks reported no leaks; Trivy exited
successfully with existing Python dependency findings. This is not an overall CI
pass. GitHub workflows target `main`; the draft PR stacked on the pool branch
must be retargeted after its base lands to receive those checks.

This slice changes only measurement tools and documentation. The compiler,
language contract, and service runtime are unchanged from `b26ddc2`; their serial
suite, bootstrap, reference comparison, and fault-injection results are recorded
in the [pool validation](pooled-http-workers.md#validation-recorded-on-2026-09-10).
