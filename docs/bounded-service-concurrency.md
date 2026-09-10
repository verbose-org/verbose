# Bounded forked service concurrency

Design fixed on 2026-09-09; implemented on 2026-09-10. This slice adds an admission
limit to the existing fork-per-connection HTTP execution model. It is one step
toward bounded concurrent services; it does not implement a thread or worker pool.

## Source contract

An HTTP service with `concurrency: forked` and both bounded HTTP deadlines can
declare `max_connections: N`, where N is an integer in 1..65535. For example:

```verbose
  concurrency: forked
  max_connections: 64
  request_timeout: 2
  response_timeout: 2
```

The [complete echo example](../examples/http_capped.verbose) admits eight handler
children and closes excess connections. Run it with:

```sh
cargo run -- examples/http_capped.verbose --native /tmp/http-capped --run bounded_http
/tmp/http-capped
```

The compiler verifies the range, uniqueness, protocol, concurrency mode, and
presence of the deadline pair. Sequential services, raw TCP, missing deadlines,
and `after:` mutations are refused for this new contract. Read-only service state
and existing resources/logs retain their forked semantics. Programs without
`max_connections` retain their previous admission behavior and emitted bytes.

The bound covers **at most N admitted handler children**, each owning one client
connection. A child occupies its slot until the parent observes its termination.
The parent can temporarily hold one more accepted socket while deciding admission;
that socket is either transferred to a new child or closed without a handler,
resource read, entropy draw, or log. Kernel listen-queue entries and inherited
non-service descriptors are outside this count. The fixed kernel backlog remains
128 (subject to kernel configuration); this is not an application request queue.

At capacity, close the new client immediately, without emitting an HTTP response
or trying to execute its request. A rejected connection cannot mutate service
state or fire request effects. Fork failure also closes that accepted socket and
does not consume a slot. Normal exit, client-error exit, timeout, or a terminating
signal releases a slot when reaped. A stopped child remains counted.

The admitted child uses the already-declared request/response deadlines. Those
bound socket phases, not handler CPU, resource I/O, or logging. A child stuck in
one of those other phases can retain a slot indefinitely; the quota prevents
unbounded growth but does not establish fairness or availability. Cached resources
and read-only state are inherited snapshots, not shared mutable state. Concurrent
file effects retain their existing ordering/atomicity limitations.

## Native flow and register/ownership audit

The parent is the sole acceptor and the sole writer of the admission counter.
No PID table, shared memory, user signal handler, or runtime allocation is needed.
Reserve 24 frame bytes (counter, pollfd, wait status) after existing service slots
and HTTP scratch, before the receive buffer. Child writes are private fork copies.
Keep listener r12 and service rbp intact in parent helpers; all syscall arguments
and temporaries use otherwise-dead registers and rbp slots.

1. Check socket/bind/listen errors on the opted-in path. Set the listener itself
   to O_NONBLOCK (accepted-socket flags alone do not do this). Reset SIGCHLD to
   SIG_DFL and check the result: inherited SIG_IGN would lose waitable exits and
   make counting unsound. The legacy path keeps its auto-reaping SIG_IGN helper.
2. Before waiting, drain `wait4(-1, &status, WNOHANG, NULL)`. Decrement only for
   terminated children, including signal deaths. Do not decrement for a traced
   stop. Counter underflow and ECHILD with a nonzero counter are invariant errors,
   not permission to admit more clients. Unexpected wait errors stop the parent.
3. Poll the nonblocking listener with a 100 ms timeout, then return to reaping on
   timeout/EINTR. This ensures idle parents continue collecting exits. Polling
   cadence is an implementation detail, not a wall-clock cleanup guarantee under
   arbitrary scheduling. Invalid listener/error events stop the parent.
4. Accept once on readiness. EINTR/EAGAIN and documented pending TCP network
   errors retry; permanent admission errors stop the parent rather than spinning.
   A failed accept never reaches fork. Store the accepted fd at existing rbp-48.
5. Reap again before comparing the counter with N. At capacity, close the accepted
   fd and return to the admission loop. Otherwise fork. Increment only in the
   successful parent branch, then close the parent's accepted-fd copy and loop.
   On fork failure, close and loop without incrementing.
6. The child closes its inherited listener before entering existing bounded HTTP
   receive → handler/effects → send → close/exit. It never accepts another client
   or updates the parent's counter. Child startup close failure exits the child.

Startup/invariant failures exit the parent with status 1. Already-admitted
children retain their existing lifetimes; supervisor shutdown, child termination
on parent death, graceful draining, and restart are separate contracts. Because
children close the listener, they do not intentionally keep admission open when
the parent exits. No claim of rollback, exact request completion, or effect
cancellation follows from a process count.

## Backend refusals and acceptance

| Path | Support |
|---|---|
| Rust parser and verifier | Range, uniqueness, HTTP/forked/deadline context, no `after:` mutations |
| Native Linux x86-64 | Admission cap, immediate overload close, child reaping and slot recovery |
| Interpreter | Handler rules only; no service runtime |
| WASM | Explicit refusal for the selected capped service |
| Self-hosted compiler | Service-scoped refusal before ELF or raw machine-code output |

`max_connections` remains usable as an ordinary identifier outside service
attributes. Existing backend gates remain in force.

The [socket acceptance tests](../src/http_tests/admission_tests.rs), shared
[backend refusal tests](../src/http_tests.rs), and native admission checks cover:

- N simultaneous incomplete requests and immediate N+1 rejection without effects;
  N=1, N>1, bursts, and later admission after capacity is freed.
- Slots recovered after valid response, malformed request, timeout, and signal
  death; no decrement for a stopped child. Reap without requiring new clients.
- Parent/client descriptor closure, child listener closure, inherited SIGCHLD
  ignore, and normal startup refusal when the declared port is already bound.
- Fork failure without counter increment and failed accept without fork, through
  [targeted syscall fault injection](../tools/check_http_admission.py), plus
  emitted control-flow tests and allocation-syscall checks on admission/overload.
- Range/duplicate/context refusals at parser/verifier/direct emitter; self-hosted
  and WASM zero-artifact refusals, with accepted identifier controls.
- Deterministic emission and binary identity for existing examples without the
  new attribute. Run the normal suite serially, the affected bootstrap checks,
  and CIDX validation/security/CI; record any environment failures honestly.

The emitted syscall behavior follows Linux [wait](https://man7.org/linux/man-pages/man2/waitpid.2.html),
[accept](https://man7.org/linux/man-pages/man2/accept.2.html), and
[poll](https://man7.org/linux/man-pages/man2/poll.2.html).

The fault-injection checks require Linux, `strace`, loopback sockets, and ptrace
permission. Run `cargo build`, then `python3 tools/check_http_admission.py`.
The script retains its traces in the temporary directory printed with the report.

## Validation recorded on 2026-09-10

- Normal suite: 644 passed, 26 ignored, run serially. The focused `bounded_` run
  passed 21 tests, including inherited signal disposition and backend refusals.
- Separate two-generation bootstrap: 25 passed, including the binary fixed point
  and corpus acceptance (97/164; the added capped service is explicitly refused).
- Self-source verification: 376 concepts and 963 rules, with all proofs accepted.
- Against compiler revision `e35a880`, 162 pre-existing top-level example entries
  produced 159 byte-identical binaries and three refusals on both compilers.
  The evolving self-source and new capped example were excluded.
- The optional strace check passed twelve syscall-failure cases and one overload
  burst/recovery case. It observed no allocation syscalls or stdout/stderr output;
  a full quota never reached fork. This is observed coverage, not a general
  guarantee about allocations or side effects inside arbitrary handlers.
- `cidx validate` passed. `cidx run security` and `cidx run ci` stopped in
  cargo-audit because the configured container lacks `curl`; CI did not reach
  test/build. Gitleaks found no secrets. Trivy exited successfully but reported
  23 vulnerabilities in the unchanged `tools/requirements.txt` (also found in
  an existing local worktree); the Cargo lockfile had no findings. Neither
  pipeline is recorded as an overall pass.
