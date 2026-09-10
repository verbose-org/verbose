# Reusable HTTP worker processes

Designed and implemented on 2026-09-10, with the contract committed before
emission changes. This slice gives a fixed set
of isolated processes repeated request lifetimes, following the
[memory design criterion](../ARCHITECTURE.md#memory-as-a-language-design-criterion).

## Source contract

An `http_1_0` service may declare `concurrency: pooled` with `workers: N`, an integer
in 1..64, and both `request_timeout` and `response_timeout`. The compiler refuses
missing/duplicate worker counts, counts on other modes, other protocols, `after:`
mutations, and `max_connections` on this mode. Existing services keep their bytes.

Each worker handles one connection at a time, then reuses its request frame for
another connection. Workers are created before they serve; there is no per-request
fork. Their private memory is not shared mutable state. Read-only service state
and cached resources remain inherited snapshots. Uncached resources, entropy,
and logs retain their existing per-request timing and effect semantics.

The N-worker bound covers concurrent processing. Busy workers leave new clients
in the kernel listen queue (backlog 128, subject to kernel limits). There is no
application admission queue and no immediate overload-close guarantee. The
existing `forked` + `max_connections` contract still provides that policy; it is
refused here rather than silently acquiring queueing semantics. Request deadlines
begin after a worker accepts, and do not bound queue residence, handler CPU, or
resource/log latency. Keep-alive, threads, TLS, and request fairness are separate.

## Storage and lifetime

- Each worker inherits the fixed service frame and receives into its own
  `max_request` buffer. Request metadata and phase cursors are initialized on
  every iteration. Lengths delimit live input; old bytes are not cleared.
- Success and client-error paths close the accepted socket and restore rsp to
  the frame base before accepting again. Dynamic stack response/handler buffers
  are reclaimed together. No pointer into a request may be stored in persistent
  service state; `after:` mutations remain refused in this slice.
- The supervisor owns a fixed PID table: 24 control bytes plus eight bytes per
  declared worker, reserved at compilation. The 64-worker ceiling bounds this
  bookkeeping to 536 bytes per inherited frame. It is not a RAM or RSS quota.
- The service backend currently refuses recursive group arenas in handler
  callees. This slice does not turn process-lifetime arenas into request arenas
  or claim a bound for every reachable temporary allocation.

## Process and failure contract

The supervisor creates exactly N workers, closes its own listener copy, and waits.
Workers retain the listener and use checked blocking accept, retrying EINTR and
documented transient network errors. Each uses the existing bounded HTTP receive,
handler/effects, send, and close/reset paths.

A malformed request, socket failure, or phase timeout ends that connection and
keeps the worker available. An unexpected worker exit (including an operator
failure or terminating signal) terminates the pool: the supervisor kills/reaps
remaining workers and exits 1. A stopped worker remains in the pool but supplies
no capacity until resumed. Automatic respawn/retry and graceful draining require
separate policies; repeating effects is not implied by worker reuse.

Workers can begin accepting while the supervisor creates the remaining workers;
startup is not an atomic transaction. Startup fork failure kills/reaps
already-created children. SIGCHLD is reset to
SIG_DFL before spawning. Workers request Linux parent-death SIGKILL and compare
getppid with the recorded supervisor PID after installing it, covering the setup
race. Supervisor death thus cannot intentionally leave orphan listeners. This is
an abrupt shutdown boundary and does not roll back completed/partial effects.

The supervisor removes a reaped PID from its table before signalling survivors,
so a recycled PID cannot be signalled. ECHILD means there are no owned children
left; unexpected wait failures terminate, rather than restarting an unknown pool.

## Backends and acceptance

| Path | Support |
|---|---|
| Rust parser and verifier | Worker count, HTTP/deadline context, mutation and admission-policy refusals |
| Native Linux x86-64 | Fixed process pool with private reusable request frames |
| Interpreter | Handler rules only; no service runtime |
| WASM | Selected pooled service explicitly refused before artifact output |
| Self-hosted compiler | `workers` or `concurrency: pooled` refused in service scope before ELF/raw output |

`workers` and `pooled` remain usable as ordinary identifiers outside these service
attributes. Native direct-emission APIs enforce the same context constraints.

Run the [two-worker echo example](../examples/http_pooled.verbose):

```sh
cargo run -- examples/http_pooled.verbose --native /tmp/http-pooled --run bounded_http
/tmp/http-pooled
```

Acceptance covers stable worker PIDs and stack position over repeated requests;
overlapping clients; long then short/binary requests without stale data; request
and response timeout recovery; descriptor stability; read-only state and resource
lifetimes; worker/supervisor death; partial startup failure; parser/verifier/direct
emitter refusals; and unsupported backend zero-artifact refusal. Compare existing
native example bytes and run the serial suite plus affected bootstrap checks.

The [socket tests](../src/http_tests/pool_tests.rs) exercise those lifetimes,
including reclaiming a 4 MiB response temporary after a write timeout. This is
an existing dynamic-concat stress shape, not a new general response-size proof.
The [strace script](../tools/check_http_pool.py) checks reuse, partial fork failure,
wait/accept/setup failures, and the parent-death setup race. Run `cargo build`,
then `python3 tools/check_http_pool.py`; Linux loopback/ptrace access is required.

The [HTTP worker benchmark](http-worker-benchmarks.md) compares the same echo
handler in forked and pooled modes and samples memory across repeated batches.

Process semantics follow Linux [parent-death signals](https://man7.org/linux/man-pages/man2/PR_SET_PDEATHSIG.2const.html),
[wait](https://man7.org/linux/man-pages/man2/waitpid.2.html), and
[accept](https://man7.org/linux/man-pages/man2/accept.2.html).

## Validation recorded on 2026-09-10

- Normal suite: 653 passed, 26 ignored, run serially. The nine focused pool tests
  passed, including stable PIDs/stack/descriptors across 200 requests, binary and
  changing-length bodies, receive/write timeout recovery, and resource lifetimes.
- Separate two-generation bootstrap: 25 passed, including the binary fixed point
  and corpus acceptance (97/165; the new pooled service is explicitly refused).
  Self-source verification accepted all 376 concepts and 963 rules.
- Native comparison against `065b284`: 160 byte-identical binaries and three
  refusals on both compilers across 163 existing example entries; excludes the
  changing self-source and new pooled example. No mismatches.
- The strace script passed thirteen reuse/failure/race scenarios. Those traces
  contain no allocation syscalls or process stdout/stderr, and no listener
  survives supervisor termination. This is observed coverage of the example,
  not a whole-program memory-safety proof.
- `cidx validate` passed. Local security and CI pipelines stop at cargo-audit
  because its container lacks `curl`; CI does not reach test/build. Gitleaks
  reported no leaks. Trivy exited successfully with 23 findings in the unchanged
  Python tools requirements (also present in a local worktree), and none in
  Cargo.lock. Neither pipeline is an overall pass.
