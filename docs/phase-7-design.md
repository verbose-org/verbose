# Phase 7 design — declarable network services

This doc sketches what Phase 7 of the native backend would look like: the first phase in which a `.verbose` file can fully describe a long-running network program, collapsing today's tier-2 and tier-3 binaries (hand-emitted plumbing in Rust) into tier 1 (described in source, verified end-to-end). See `docs/known-gaps.md` → "Three tiers of native output" for the tier classification.

It is a **design sketch, not an implementation plan**. The point is to make the north star concrete: show the grammar, the proof obligations, and the security properties so that when Phase 7 is actually built, the shape is already decided rather than improvised. Nothing in this doc commits to a timeline.

## Where Phase 7 sits

The native backend has grown phase by phase (0 → 6), each phase adding a class of computation the compiler can emit while preserving the audit story. Phase 7 is qualitatively different: it adds **persistent I/O** — long-running processes that accept connections, hold file descriptors across loop iterations, and emit bytes to the network.

Today, this territory is served by two escape hatches outside the `.verbose` language:

- `native::compile_http_server` wraps a verified `.verbose` rule in hand-emitted HTTP plumbing (tier 2)
- `native::emit_http_demo` / `compile_echo_server` emit entire binaries from Rust with no `.verbose` source (tier 3)

Both are useful as proofs that the emitter *can* produce tiny network binaries. Neither is auditable at the language level: the network behaviour lives in Rust source, not in a `.verbose` file a regulator or security engineer can read.

Phase 7's goal is to make the network behaviour itself describable, so a single `.verbose` file produces the whole binary — rule logic and network shell alike.

## What Phase 7 does NOT try to solve

Before the design, the explicit refusals — to keep the scope tractable:

- **Arbitrary syscalls.** Phase 7 adds a closed, named set of network primitives. It does not expose `syscall(n, ...)` style invocation; that would break the "everything declared" discipline by shifting what the binary does from source to runtime arguments.
- **User-written protocol parsers.** Phase 7 ships a small, fixed set of built-in protocols (raw TCP and HTTP/1.0 are the candidates). Writing an HTTP parser from scratch inside `.verbose` requires text-manipulation primitives (substring, indexing, pattern matching) that belong to Phase 8+.
- **Concurrency / threading.** Phase 7 is single-threaded, one-connection-at-a-time (accept → read → dispatch → write → close → accept next). Threading, async, or epoll-style multiplexing is out of scope.
- **TLS / cryptography.** Network listening is plaintext only in Phase 7. TLS termination is delegated to the operator (nginx, caddy, haproxy in front of the Verbose binary) — consistent with the OS-is-the-supervisor posture.
- **Outbound connections.** Phase 7 only *listens*. Outbound `connect()` is a different security profile (exfiltration surface, dependency on external services) and is deferred.

These boundaries keep Phase 7 small enough to audit as a single unit while covering the most common "Verbose binary as a network service" use case.

## Proposed direction: the `service` construct

Two design shapes were considered:

- Exposing socket / bind / accept / read / write as separate declarable reactions, with scope blocks for fd lifetimes. Most flexible; large grammar surface; many proof obligations (fd lifetime, double-close, read-bound consistency).
- Introducing a single new top-level construct `service` that declares a whole listener — port, protocol, bounded request size, handler rule — and lets the native emitter produce the plumbing around a normal rule. Smaller grammar surface; covers the common case; postpones the full fd-lifetime machinery until actually needed.

Phase 7 takes the second direction. The reasoning: today we do not know which low-level primitives users genuinely need, and exposing them before learning is the same "false explicitation" failure that killed single-value proof fields in Phase A. Start with one useful shape; let operational need pull more primitives into the language.

A `service` declaration in Phase 7 looks like this:

```verbose
service hello_server
  @intention: "Serve HTTP on port 9999, dispatching every request to hello_handler"
  @source: hello.intent:4

  listen:
    protocol    : http_1_0
    port        : 9999
    max_request : 4096

  handler : hello_handler
```

The handler is a normal rule — same shape as every rule today — whose input and output types are fixed by the protocol:

```verbose
rule hello_handler
  @intention: "Return 200 with a greeting for GET /; 404 otherwise"
  @source: hello.intent:3
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = if req.method == "GET" and req.path == "/"
           then HttpResponse { status: 200, body: "Hello from Verbose!" }
           else HttpResponse { status: 404, body: "not found" }
  proofs:
    purity:
      reads : [req.method, req.path]
      calls : []
    termination:
      bound : 5
```

The `HttpRequest` and `HttpResponse` concepts are provided by the compiler for the built-in protocols — the user does not declare them, does not parse bytes, does not format responses. Those are closed primitives, audited once in the compiler code, reused by every Phase 7 service.

## What the verifier must check

When a `.verbose` file contains a `service` declaration, the verifier adds these obligations on top of the existing proofs on the handler rule:

- `protocol` is in the closed set of built-in protocols. Unknown protocol is a hard error.
- `port` is in `[1, 65535]`. Operators who want to bind to privileged ports get the system to decide that (via `setcap` or equivalent), not the language.
- `max_request` is declared and numeric. No default — forcing the declaration makes the auditor see the bound.
- The handler rule exists in the program.
- The handler's `input` type matches the protocol's request concept (e.g., `HttpRequest` for `http_1_0`).
- The handler's `output` type matches the protocol's response concept.
- The handler carries standard proofs (purity, termination bound) — the service inherits those properties for each request cycle.
- The handler is pure: no reactions fire during request handling. Reactions that ARE declared at the service level (e.g., an Article 12 audit log append) fire around handler invocations, not inside them.

The `service` declaration adds exactly one new AST node type and one new verification pass. Everything else rides the existing rule machinery.

## What the native emitter must produce

For a Phase 7 service, the emitted binary is roughly:

```
_start:
  prologue (existing): socket, setsockopt(REUSEADDR), bind, listen
  accept_loop:
    accept(server_fd) -> client_fd
    read_bounded(client_fd, max_request) -> buffer   ; new
    parse_protocol(buffer) -> request record slots   ; new, per-protocol
    <handler rule body — existing codegen, unchanged>
    serialize_protocol(response slots) -> out_buffer ; new, per-protocol
    write(client_fd, out_buffer)
    close(client_fd)
    jmp accept_loop
```

The existing code in `compile_http_server` already emits ~80% of this. Phase 7 formalises that code as a stable `service` emitter driven by the AST, rather than a special-case function. The new additions:

- A small, bounded HTTP/1.0 request parser (method + path only — no headers for Phase 7)
- A small, bounded HTTP/1.0 response serializer (status line + Content-Length + body)
- A raw-TCP protocol variant for non-HTTP services, where request is `bytes` and response is `bytes`

Target binary size for an HTTP greeter: around 1–2 KB, comparable to the tier-3 hardcoded demo plus the rule logic. The whole thing stays in the audit-line-by-line envelope.

## Worked example, end to end

Putting the pieces together, a complete Phase 7 HTTP service in a single `.verbose` file:

```verbose
@verbose 0.1.0

-- Built-in concepts (provided by the compiler for http_1_0):
--   HttpRequest  { method: text [..8], path: text [..256] }
--   HttpResponse { status: number [100, 599], body: text [..4096] }

rule hello_handler
  @intention: "Return 200 for GET /; 404 for anything else"
  @source: hello.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = if req.method == "GET" and req.path == "/"
           then HttpResponse { status: 200, body: "Hello from Verbose!" }
           else HttpResponse { status: 404, body: "not found" }
  proofs:
    purity:
      reads : [req.method, req.path]
      calls : []
    termination:
      bound : 5

service hello_server
  @intention: "Listen on port 9999, dispatch every request to hello_handler"
  @source: hello.intent:2
  listen:
    protocol    : http_1_0
    port        : 9999
    max_request : 4096
  handler : hello_handler
```

One file. An auditor reads the intent (two sentences), reads the handler (five lines of logic plus proofs), reads the service declaration (four lines), and knows exactly what the binary does. No hidden HTTP parser, no surprise syscalls. The compiler is trusted to emit the HTTP parsing and response formatting correctly — once, in one place, auditable by whoever reviews the compiler itself.

## Open design questions

Things this sketch deliberately does not pin down, to be decided when Phase 7 is actually built:

- **Error paths.** What happens on a malformed HTTP request? On a connection that times out mid-read? Candidates: fixed 400 response, fixed 408 response, close without response. Each has security implications; the choice belongs in the built-in protocol implementation, not in user code.
- **Per-service reactions.** Audit logging (Article 12 for AI-Act-scale services) would benefit from firing *around* each handler invocation. Open question: does `service` accept a `log:` declaration that fires after each request, or does that wait for a broader "reactions-around-rules" mechanism?
- **Shutdown.** Phase 7 services loop forever. Graceful shutdown on SIGTERM is a runtime concern usually handled by the operator's supervisor (systemd, k8s). Whether Verbose emits any SIGTERM trap is an open question.
- **Multiple services in one file.** One binary listens on one port. Running two services means two binaries, two systemd units — consistent with the OS-is-supervisor posture but worth stating as a design rule.
- **Interaction with `--stream`.** The `--stream` mode already defines a "rule runs per input line" shape. Phase 7's `service` is conceptually "rule runs per HTTP request". Whether these two modes share infrastructure or stay separate paths is an implementation question.

## What this design enables, what it defers

After Phase 7, Verbose describes HTTP services and raw-TCP services end to end. The AI Act worked examples (`loan_decision`, `cv_screening`) can each become their own service — `GET /decide/30000/650/12/0` routed to the rule — making the Article 12 audit wrapper unnecessary, replaced by a declared audit reaction inside the service.

Things that still require later phases:

- User-written protocol parsers (Phase 8 — text indexing primitives)
- Outbound HTTP clients (Phase 9 — connect, SNI, cert handling)
- Persistent per-connection state across multiple reads (Phase 10 — explicit fd lifetime blocks)
- TLS (probably never — delegated to external terminators by design)

Phase 7 as described is the first phase in which Verbose describes a complete network-facing program. From that point, the statement "the `.verbose` file is the program" stops being partially true and becomes structurally correct for the class of services Phase 7 covers.
