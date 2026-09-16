# Bounded HTTP socket I/O

Implemented in the Rust compiler on 2026-09-09; design committed before emission changes. This is the first transport slice
of the HTTP → bounded concurrency → TLS roadmap, not a production-server claim.

## Source contract

An `http_1_0` service opts in by declaring both `request_timeout` and
`response_timeout`, integer seconds in 1..3600, beside `handler`. The compiler
requires the pair, rejects duplicates and other protocols, and uses `max_request`
as the total request capacity (request line, headers, and body), capped at
1 MiB in this slice to keep the fixed receive frame bounded. Existing services
without the pair retain their current behavior and emitted bytes.

Receive a complete request before invoking the handler, reading per-request
resources, upstream connections, entropy, or logging. Assemble arbitrary TCP
fragments into a compiler-owned per-connection frame buffer; no growth or heap
allocation. Parse strict CRLF framing and a bounded decimal Content-Length.
Reject malformed/duplicate Content-Length, all Transfer-Encoding and Expect
headers, folded headers, invalid header names, and requests exceeding capacity.
Names are case insensitive. Missing Content-Length means an empty body. Bodies
are byte spans, including NUL; existing HttpRequest.body text operations retain
their individual contracts. This does not introduce general mutable byte buffers.

The request line admits a token method and an origin-form target with HTTP/1.0
or HTTP/1.1; existing method/path length limits apply. This is a deliberately
limited, close-after-one-request transport: no chunking, keep-alive, pipelining,
100-continue, Host validation, or full HTTP/1.1 conformance. Bytes beyond the first
complete request are ignored; they never invoke another handler.

The request deadline starts after accept/fork dispatch and before receiving; the
response deadline starts before serialization. Use CLOCK_MONOTONIC and poll with
remaining time, and nonblocking recvfrom/sendto. Repeated bytes, short I/O, EINTR,
and EAGAIN do not restart a deadline. Send each response segment until complete;
MSG_NOSIGNAL makes peer disconnect an I/O outcome rather than process SIGPIPE.
Socket errors, premature EOF, malformed input, and expired deadlines close the
client and return to accept (or exit the forked child). They do not become a
language BoundsError. No partial response is retried on another connection.

These deadlines cover the receive and response-write phases, not handler CPU,
resource I/O, logging, fork scheduling, or total process lifetime. Existing logs
run after the handler and before sending: a logged request need not have a fully
sent response. A failed response skips `after:`. A phase succeeds only when its final
completion check is still before the deadline, even if the last bytes have
already entered the socket queue. Successful sending means the kernel accepted
the bytes; it does not establish that the peer application processed them.
No rollback is implied.

## Example and backend support

See [the complete body-echo example](../examples/http_bounded.verbose):

```verbose
service bounded_http
  @intention: "Receive and answer one bounded request before its phase deadlines"
  @source: http_bounded.intent:3
  listen:
    protocol: http_1_0
    port: 18960
    max_request: 4096
  handler: echo_body
  request_timeout: 2
  response_timeout: 2
```

Build and run it:

```sh
cargo run -- examples/http_bounded.verbose --native /tmp/http-bounded --run bounded_http
/tmp/http-bounded
```

| Path | Support |
|---|---|
| Rust parser and verifier | Paired deadlines, ranges, duplicate rejection, HTTP-only context |
| Native Linux x86-64 | Bounded request assembly and complete sends; sequential or existing fork-per-connection mode |
| Interpreter | Handler rules only; no service transport execution |
| WASM | Explicit refusal for the bounded HTTP service entry |
| Self-hosted compiler | Service-scoped refusal before ELF or raw machine-code output |

The receiver reserves `max_request` bytes plus 128 scratch bytes beyond existing
service slots. A deterministic static header transition table grows the executable
by 34 KiB. Neither receive nor send loops allocate dynamically. Existing
handler expressions may still allocate, and existing response text-range checks
are not a general proof of response size. Deadlines use millisecond resolution.
The separate [admission slice](bounded-service-concurrency.md), implemented on
2026-09-10, adds an optional cap on admitted forked children. Deadlines alone do
not bound their number.

## Native implementation and register lifetimes

1. Construct a deterministic byte DFA for request-line/header syntax. Numeric
   Content-Length accumulation and duplicate detection have explicit actions.
   Every byte transition is bounds-checked by the receive/scan cursor; bytes in
   the body are not interpreted as headers. Static tables require no runtime
   allocator and keep scanning linear across arbitrarily small fragments.
2. Reserve a fixed scratch region after existing service slots, before the
   max_request buffer. All offsets are rbp-relative. The listener r12 and service
   rbp remain untouched. Parser cursors and phase deadlines survive syscalls and
   waits in dedicated slots; no borrowed pointer escapes the request frame.
3. Emit one receive loop, then the existing method/path/body binding logic. On
   success return the exact first request length in rax. A constant handler also
   takes this path when opted in, so it cannot bypass validation.
4. Before serialization capture a second absolute deadline. Each of the six wire
   segments uses a complete-send loop with the same deadline. Digit buffers stay
   live until their segment finishes. On failure branch directly to existing
   close/reset; the reset releases any temporary response buffer.
5. Emit local rel32 branches with checked resolution. No source-to-source or
   external runtime dependency. Services without the pair do not emit these
   helpers or their tables.

## Reference and acceptance

The [acceptance tests](../src/http_tests.rs) use a separate Rust framing parser
and native sockets with independently specified expected wire responses. They cover every split position, binary bodies,
empty bodies, exact capacity, length overflow, ambiguous framing, malformed
headers, unsupported framing, premature EOF, slow-drip absolute timeout, peer
reset, response backpressure/short writes, and a healthy client after failures.
They also cover declaration refusals at verification and direct emission,
deterministic emission, instruction decoding, 256 seeded request mutations,
resource/log ordering, and skip-after on partial send failure. The backpressure
fixture uses an existing large dynamic concat to exceed socket buffer capacity;
it is a transport stress case, not a new response-size guarantee.

WASM and the self-hosted compiler explicitly refuse this transport contract
before artifact output. The interpreter executes handler rules, not services;
the transport reference is a test oracle, not an interpreted server runtime.
Run the normal suite serially, CIDX checks, and self-hosted bootstrap after adding
its refusal. General buffer/ownership syntax, typed socket results, worker
pools/threads, TLS, and load qualification remain later work.

Framing references: [RFC 9112 sections 2–6](https://www.rfc-editor.org/rfc/rfc9112.html),
with the narrower rejection policy above; Linux [poll](https://man7.org/linux/man-pages/man2/poll.2.html)
and [send](https://man7.org/linux/man-pages/man2/send.2.html) define the syscall behavior.


## Validation recorded on 2026-09-09

- Normal suite: 636 passed, 26 ignored, run serially.
- Separate two-generation bootstrap: 25 passed.
- Ten bounded HTTP acceptance tests include native/reference comparisons,
  fragmented reads, actual send backpressure, state/effect ordering, backend
  refusals, and instruction decoding outside the static table.
- Against compiler revision `3996594`, the 161 pre-existing top-level example
  entries selected for native compilation produced 158 byte-identical binaries
  and three refusals on both compilers. The evolving self-host source and the new
  HTTP example were excluded from this binary-identity comparison.
- `strace` on the example covered valid empty/binary bodies, length overflow, and
  receive timeout: no allocation syscalls, no process stdout/stderr, and expected
  wire responses/connection closure. This is observed coverage, not a universal
  proof of memory safety or delivery.
- `cidx validate` passed. Local `cidx run security` and `cidx run ci` passed
  Trivy/Gitleaks but stopped at cargo-audit because its container lacks `curl`.
  These local pipelines did not reach a successful overall result.
