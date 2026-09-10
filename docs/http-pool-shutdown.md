# Bounded graceful shutdown for HTTP worker pools

Contract set on 2026-09-10, before implementation. This extends the
[isolated pool](pooled-http-workers.md) with an explicit operational lifetime.

## Declaration and boundary

An HTTP pool may add `shutdown_timeout: S`, an integer in 1..3600 seconds.
It requires `concurrency: pooled`, `workers`, and both existing HTTP phase
deadlines. Other modes/protocols, duplicates, and out-of-range values are refused.
Services without this declaration retain their current signal behavior and bytes.

Send SIGTERM to the supervisor PID or its process group. Once the supervisor
observes the first SIGTERM, it records a monotonic deadline and disables the
shared listening socket. That kernel operation is the acceptance cutoff:
connections already accepted may finish their request, response, and declared
effects; connections still queued are not guaranteed service. A request may have
been accepted between signal delivery and the cutoff. No worker accepts a new
connection after the listener has been disabled.

Idle workers exit; busy workers finish their existing iteration and exit when
they return to accept. Ordinary malformed-client/socket-timeout recovery retains
its meaning and can finish a worker's last iteration. The supervisor reaps all
workers, then exits 0. This is process-level completion, not confirmation that a
remote client received or persisted every response byte.

If the grace deadline expires, the supervisor sends SIGKILL to remaining owned
workers, reaps them, and exits 1. Repeated SIGTERM does not extend the deadline.
Unexpected worker death, operational failure, and setup failure retain the pool's
failure policy: terminate/reap survivors and exit 1. A stopped worker is still
owned and is killed at expiry. Signals cannot impose a hard wall-clock bound on
kernel scheduling or an uninterruptible kernel wait; the bound controls when
forced termination is requested, not guaranteed process disappearance.

SIGTERM is blocked in workers; direct SIGTERM to one worker does not request a
pool shutdown. SIGKILL and supervisor death remain abrupt. SIGINT, reload,
automatic respawn, retry, and replacement listener handoff are separate policies.
The grace interval begins when the supervisor consumes SIGTERM after startup;
cached-resource loading and worker creation are outside it. Effects completed
before forced termination are not rolled back or replayed.

## Storage and implementation

The supervisor retains one copy of the listening descriptor so it can stop the
socket shared with the workers. Linux `shutdown(SHUT_RD)` on that listener wakes
blocked accepts; already accepted sockets are distinct. A worker confirms that
the socket is no longer listening before treating accept's EINVAL as a clean
exit. Other accept failures remain operational failures.

Before fork, SIGTERM and SIGCHLD are blocked, with default dispositions restored.
The supervisor consumes them through `rt_sigtimedwait`, without an asynchronous
userspace handler or a shared mutable userspace flag. Nonblocking wait4 reaps
children; every reaped PID is removed before another process could reuse it.
The original parent-death protection remains in each worker.

Control storage is fixed per service frame, alongside the existing PID table.
No request data survives through this control state, and shutdown introduces no
userspace heap allocation. Worker request-frame reuse is unchanged. Frame bytes
and syscall coverage will be recorded with validation.

Linux references: [signal waiting](https://man7.org/linux/man-pages/man2/sigwaitinfo.2.html),
[socket shutdown](https://man7.org/linux/man-pages/man2/shutdown.2.html), and
the [TCP listener shutdown implementation](https://github.com/torvalds/linux/blob/v5.15/net/ipv4/af_inet.c).

## Backends and acceptance

| Path | Scope |
|---|---|
| Rust parser/verifier | Declaration and context checks |
| Native Linux x86-64 | SIGTERM, accepted-request completion, deadline, cleanup |
| Interpreter | Handler execution only; no service runtime |
| WASM and self-hosted compiler | Explicit refusal before artifact output |

Acceptance must cover idle and busy pools, process-group SIGTERM, fragmented
requests and blocked responses, queued clients, repeated SIGTERM, stopped/dead
workers, partial startup failure, syscall interruptions/failures, inherited
ignored signals, stable request storage, and unsupported-backend refusals.
Normal tests run serially; self-hosted refusal changes require bootstrap checks.
Compare existing native examples against the pool reference before delivery.
