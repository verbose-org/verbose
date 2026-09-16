# Borrowing bounded HTTP responses in service logs

Implemented 2026-09-16. A pure HTTP handler participating in the
[bounded text contract](bounded-text-output.md) can now use the service's
declared `log` effects. The completed response stays alive while each log reads
it, and through the subsequent response send. This composes existing language
constructs; it adds no annotation or general ownership type system.

```verbose
log:
  append_file "/tmp/response.log" concat(req.method, " ", req.path, " ", resp.status, " ", resp.body, "\n")
  on_error: abort
```

The [complete example](../examples/http_bounded_log.verbose) formats a body or
path with a public `text [..4098]` result. Its handler retains an alias across
shadowing and a later call, then publishes it as the response. The log reads
that completed value; it does not call the formatter again.

## Checked boundary

The log content must be a text literal or a **flat** `concat` of text/number
literals and these fields:

- `req.method`, `req.path`, `req.body`, `req.timestamp`;
- `resp.status`, `resp.body`.

`req` and `resp` are the log's canonical bindings, independent of the handler's
parameter name or lexical lets. The timestamp is captured by the transport and
does not enter the pure handler's inputs. The request body is parsed even if
only the log reads it.

The compiler proves each log's content capacity is at most **1 MiB**. Literal
sizes count bytes; request fields use the service's enforced bounds; numbers
need at most 20 bytes; response fields use the verified handler result, including
callee public capacities and the maximum of conditional alternatives. Concat
adds these capacities with checked arithmetic. An unknown or excessive capacity
is refused before an output artifact is opened. Incidental short runtime values
do not weaken a callee's declared capacity.

Nested concats, `length`, `parse_int`, `json_escape`, arbitrary expressions,
handler lets and rule calls in these logs are explicitly refused in this slice.
The existing log acceptance rules for unannotated handlers remain unchanged.
An explicit log formatter or broader effect transfer needs a separate contract.

## Ownership and memory

| Storage | Owner and lifetime |
|---|---|
| Parsed request | The transport frame; valid through logs and sending |
| Completed response | The bounded handler's invocation region; logs borrow its counted pointer/length |
| Formatted log | A separate temporary stack buffer below the response region; released after its write, before the next log |
| Next request | The accept-loop reset releases all invocation storage; pooled workers reuse their frames |

There is no additional response copy at the log boundary. Formatting a concat
still copies its selected bytes into the log buffer. A literal log is written
from embedded data. Flat concat requires at most its checked content capacity
plus the existing fixed numeric-formatting scratch; there are no nested log
buffers. No heap allocator, reference counter or garbage collector is added.

The handler's separate 2 MiB invocation limit remains in force. Log scratch,
transport storage, persistent kernel/file buffers and process overhead are
outside that limit. The 1 MiB per-log content limit is neither a disk quota nor
a total service-memory bound. Logs run sequentially, so their temporary buffers
do not accumulate across log blocks or requests.

## Ordering and failure

The order is handler, declared logs in source order, then response send. With
paired socket deadlines, malformed/incomplete requests never reach the handler
or logs. A later send failure can occur after the logs were written; a log entry
does not establish successful delivery to the client.

Existing `on_error` policies apply to negative open/write syscall results:
`drop` continues to later logs and sending; `abort` exits the executing process
with status 1 before later logs and sending. In forked mode that is the request
child; in pooled mode worker failure causes the supervisor to terminate the
pool. Earlier log effects are not rolled back. The existing single-write policy
still does not retry short writes or guarantee durability. Logging uses blocking
file I/O; socket deadlines do not bound log latency. Concurrent workers may
interleave their separate log blocks; this adds no transaction or ordering
guarantee between requests.

Bodies remain counted bytes, including NUL and non-UTF-8 data. This is raw
logging, without JSON encoding or line escaping; the example is not a structured
audit-log format for arbitrary input.

## Support and verification

| Path | Support |
|---|---|
| Rust verifier | Closed log scope, types, static content capacities and analysis budget |
| Native sequential/forked HTTP | Bounded handlers with logs, with or without socket deadlines |
| Native pooled HTTP | Same log borrowing; existing mandatory socket deadlines |
| Interpreter | Pure formatter value reference; does not execute services |
| WASM / self-hosted compiler | Existing bounded-text contract refusal before artifact emission |

State, `after` mutations and effects inside participating handlers remain outside
this slice. The separate [persistent-copy contract](bounded-text-state.md)
retains its existing scope.

```sh
cargo run -- examples/http_bounded_log.verbose --native /tmp/bounded-log --run bounded_log_http
cargo test bounded_logs -- --test-threads=1
cargo build --locked
python3 tools/check_bounded_text_logs.py
```

Socket tests cover exact log and response bytes, NUL/UTF-8/non-UTF-8 bodies,
empty and large bodies, multiple logs, aliases/shadowing, timestamps, parser-only
body use, all worker modes, capacity boundaries and refusal without modifying
an existing artifact. Idle worker PIDs, descriptors and stack positions remain
stable across repeated successful and malformed requests.

The optional Linux/strace check injects log open/write and socket receive/send
failures. It verifies effect order, subsequent requests, stack/descriptor
reclamation and the absence of `mmap`, `brk` and `mremap` syscalls. Its negative
control deliberately releases the response region before logging; byte checks
must detect the resulting corruption. This validates storage and failure paths,
not logging throughput or latency.
