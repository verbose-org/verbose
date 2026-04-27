# Effect Model — declared capabilities, bounded surface, audit visibility

Every interaction a Verbose binary has with the world outside its own stack frame is **declared, bounded, proved, and visible in the source**. This doc enumerates the effects that exist today, the rules they share, and the closed list of refusals that bound the surface.

It serves three audiences:
- The author of a `.verbose` file who needs to know what they can declare and what shape the declaration must take.
- The auditor (regulator, security reviewer, compliance officer) who needs a single place to enumerate every external interaction the binary can perform.
- Future contributors who want to add a new effect — this doc is the contract every new effect signs against.

## The effect model in one sentence

> A Verbose binary has no capability it has not explicitly declared in its source; every declared capability is bounded at compile time; every bound is verified before emission; the auditor reads the source (or `strings` the binary) and sees the complete list.

This is downstream of two project axioms (see [Design Priorities](../README.md#design-priorities) in the README): the compiler controls and applies, never guesses; and there are no layers between intent and machine code. The effect model is what makes the verifier's "no undeclared interaction" claim concrete.

## Catalogue of effects

| Effect | Declaration site | Required proof | Syscalls emitted | Error policy | Memory bound | Audit visibility | Allowed contexts |
|---|---|---|---|---|---|---|---|
| **`print` to stdout** | `reaction { effects: [print expr...] }` | None (write to fd 1 is intrinsic to the runtime) | `write(1, ptr, len)` per arg + spaces + newline | Silent ignore on partial write | Args must be scalar (text/number/bool); concat buffer bounded by sum of declared field ranges + literal lengths | `print "user=" e.user " amount=" e.amount` is in source verbatim | Reaction `effects:` block |
| **`append_file` to a literal path** | `reaction { effects: [append_file "/path" expr] }` OR `service { log: { append_file "/path" expr } }` | Reaction: trigger rule's `reads:` covers fields used in expr. Service log: closed grammar (req.method/path, resp.status/body, req.timestamp, literals, concat). | `open("/path", O_WRONLY\|O_APPEND\|O_CREAT, 0644)` → `write(fd, content_ptr, content_len)` → `close(fd)` | Reaction: silent ignore. Service log: `on_error: drop` (default, silent) or `abort` (sys_exit 1) | Path is a literal; content concat sized by declared field bounds + literals | Path appears inline in the binary; `strings binary \| grep /` enumerates every file the binary can write | Reaction effect; service `log:` block |
| **`read(<resource>)`** | Top-level `resource <name> { path: "/literal", max: N, on_read_error: abort, cache: bool }` | Rule's `reads:` lists the resource name | `open("/literal", O_RDONLY, 0)` → `read(fd, buf, max)` → `close(fd)` | `abort` only (slice 9 strict). Slice 9.5+ may add `drop`. Failure → `sys_exit(1)` via shared abort label | `max` is a u32 literal, verifier-bounded `1..=64 MiB`. Stack-allocated buffer of `max` bytes per resource | Path appears inline in the binary; resource declaration enumerates every file the binary can read | Rule logic (`output: text`, `Result(text, _)`, `Result(_, text)` arms); HTTP service handler body (`body: read(name)` directly or via `concat`) |
| **`fetch(<connection>, request_bytes)`** | Top-level `connection <name> { host: "X.X.X.X", port: N, max_response: M, on_connect_error: abort }` | Rule's `reads:` lists the connection name | `socket(AF_INET, SOCK_STREAM, 0)` → `connect(fd, &sockaddr_in, 16)` → `write(fd, req_ptr, req_len)` → `read(fd, buf, max_response)` → `close(fd)` | `abort` only. Failure at any step → `sys_exit(1)` via shared abort label | `max_response` u32, verifier-bounded. Request bytes sized by declared field bounds + literals | `host:port` declared inline; `strings binary \| grep -E '\.[0-9]+\.[0-9]+'` enumerates every IP the binary can reach. No DNS — no surprise destinations | Rule logic (text-typed); HTTP service handler body |
| **`service listen`** | Top-level `service <name> { listen: { protocol, port, max_request }, handler: <rule>, ... }` | Handler input/output types match the protocol's built-in concepts (`HttpRequest`/`HttpResponse` for `http_1_0`, byte concepts for `raw_tcp`) | `socket` → `setsockopt(SO_REUSEADDR)` → `bind(:port)` → `listen(128)` → loop: `accept` → handler-emitted body → `close` | Per-iteration; if HTTP parse fails, close client_fd and loop. Server itself does not exit. | `max_request` u32 caps per-connection read buffer | Port and protocol in source; protocol is from a closed set (`raw_tcp`, `http_1_0`); no user-written wire parsers | Top-level `service` declaration |
| **`fork()` per accept** | `service { ..., concurrency: forked }` (default `sequential`) | None (orthogonal to handler logic) | At startup: `rt_sigaction(SIGCHLD, SIG_IGN, NULL, 8)`. Per accept: `fork()`. Parent: `close(client_fd) ; jmp accept_top`. Child: runs the iteration body, then `sys_exit(0)`. Fork failure: `write(2, "fork failed\n", 12) ; close(client_fd) ; jmp accept_top` (drop the connection, keep serving). | Fork failure → drop-and-continue (transient resource exhaustion shouldn't kill the server). Child failures → child exit only. | None (no per-fork buffer growth) | `concurrency: forked` line is in source; one keyword tells the auditor the binary spawns children | Service-level only; restricted to `Protocol::Http10` |
| **`clock_gettime`** (timestamp slot) | Implicit when `req.timestamp` appears in a service `log:` block | Verifier validates that `req.timestamp` is referenced only inside a `log:` content expression (not in the handler logic — see `req.timestamp` doctrine below) | `clock_gettime(CLOCK_REALTIME, &timespec)` once per accept (only when `req.timestamp` is referenced) | Failure ignored (would only happen on kernel-level corruption) | One i64 slot per service binary that uses it | Implicit; `strings binary` won't show it but the source's `req.timestamp` reference is the marker | Service `log:` block content (not handler logic) |

## Cross-cutting doctrine

### Naming convention

Every effect that touches the world has a **handle**: a top-level declaration with a name. `resource <name>`, `connection <name>`, `service <name>`. The rule that uses it must reference the name in its purity proof's `reads:` list. The verifier checks the cross-reference both ways:

- Every name listed in `reads:` resolves to a declared field, resource, or connection.
- Every `read(name)` / `fetch(name, _)` resolves to a declared resource/connection.
- A rule that references a resource via `read(...)` but does NOT list it in `reads:` is rejected with "declared reads do not match logic; missing: [...]".

This is the same `purity reads` machinery that's enforced for input field reads. The `reads:` list is the **single audit-visible enumeration** of every external dependency a rule has.

### "purity" terminology

`purity reads:` is sometimes confusing — a rule that calls `fetch(upstream)` is not "pure" in the mathematical-function sense. In Verbose, **"purity" means "every interaction is named"**, not "absence of interaction". The list is exhaustive: nothing the rule does to the world is missing from it.

This terminology is deliberate. Renaming `reads:` to `effects:` or `capabilities:` would be sugar. The discipline that matters is "the source enumerates every interaction" — the word that labels the list is secondary.

### Error policy: `abort` is the default for fail-closed effects

Two effects expose an `on_*_error: abort` knob:
- `service { log: { ..., on_error: abort } }` (slice 8d)
- `resource { ..., on_read_error: abort }` (slice 9, only option today)
- `connection { ..., on_connect_error: abort }` (slice 11, only option today)

`abort` means: any failure of the underlying syscall (open, write, read, connect) terminates the process with `sys_exit(1)` via a shared abort label at the end of the binary's `.text` section. The label is emitted exactly once per binary, regardless of how many `js abort_label` patches reference it — every failure path resolves to the same exit.

The semantic: **fail-closed**. If the binary cannot complete its declared interaction (audit log can't be written, cached resource is missing at startup, upstream is unreachable), the binary terminates rather than serve a request whose contract it cannot honour. This matches the AI Act / Article 12 pattern: no log persisted = no claim of having served the request.

`drop` (silent-ignore) is opt-in for service log effects only (slice 8d default). Resources and connections do not expose `drop` today — the strict-only default forces failure to be obvious.

### Memory bounds

Every effect that reads from outside-the-binary state into a stack buffer has a declared `max` (or `max_request` / `max_response`). The verifier enforces:
- `max ≥ 1` (no zero-byte allocations)
- `max ≤ 64 MiB` (slice 9 ceiling — anything larger needs streaming, deferred)

The buffer is allocated by the rule's prologue or the service's per-accept iteration via `sub rsp, ...`. There is no heap. There is no `malloc`. There is no growable buffer. Every byte the binary touches is either a stack slot whose offset is computable at compile time, or a region inside a syscall buffer whose size is a declared u32 literal.

### Audit visibility

The auditor's first action is `strings binary | sort -u`. Every effect in this catalogue puts its key parameters in the binary's data section as inline literals:

- `append_file "/path"` → `/path` is in `.text` via jmp-over-data + lea-rip-relative.
- `read(resource)` → resource's `path:` literal is in `.text` the same way.
- `fetch(connection, ...)` → connection's `host` (4 bytes inet_aton'd) and `port` (2 bytes htons'd) appear in the inline 16-byte sockaddr_in struct.
- Service `log: append_file "..."` → log path is inline.
- Service `listen: port: N` → port (2 bytes htons'd) appears in the bind sockaddr_in.

A 5-second `strings` audit lists every file the binary can touch and every host:port it can dial. Every interaction is in the source AND in the binary.

### Allowed contexts

Some effects are universally available (any rule can use `read(resource)`); others are scoped:
- **Reactions** can `print` and `append_file`.
- **Rules with `output: text`** can use `read(resource)` and `fetch(connection, _)` in their logic.
- **Rules with `output: Result(_, _)`** can use both via the same paths (slice 9.1, 11.1).
- **HTTP service handlers** can use `read(resource)` and `fetch(connection, _)` directly in `body:` or via `concat` (slices 9.2, 11.2).
- **Service `log:` blocks** have a closed grammar: literals, concat, `req.method`, `req.path`, `req.timestamp`, `resp.status`, `resp.body`, `append_file` (path literal). Other expressions are rejected at parse/verify time.
- **Collection / fold / parallel rule contexts** do NOT yet support `read` or `fetch` (slice 9.5 deferred).

Each new effect declares its allowed contexts as part of its slice. Adding an effect to a new context is a separate slice with its own review.

### `req.timestamp` is intentionally restricted

`req.timestamp` (Unix seconds, captured once per accept loop iteration) is visible inside service `log:` blocks but **not** inside the handler's response logic. This is a deliberate restriction: if the handler could see the timestamp, the response would depend on time, and replaying a request would not produce the same response — making test reproduction impossible and audit reasoning harder.

The trade: the audit log can timestamp events (operationally necessary for Article 12) without contaminating the response (which stays a function of `(method, path, body)` alone).

## What is NOT in the effect model

The closed list of effects above is the complete set. Verbose deliberately refuses, today and as a discipline:

- **Arbitrary syscalls** — no `syscall(n, ...)` primitive. Adding a new syscall is adding a new effect, with its own declaration shape and audit story.
- **DNS resolution** — `connection { host: "example.com" }` is rejected at parse time. Domain names introduce dynamic resolution, resolver config dependency, spoofing surface, and a hidden network interaction. IPv4 dotted-quad literal is austere but enumerable. DNS, if it ever lands, will be its own slice with its own audit story (declared resolver, declared timeout, declared cache behaviour, etc.).
- **TLS / HTTPS** — delegated to operator-side terminators (nginx, stunnel, haproxy in front). Rationale: TLS introduces a large parser surface and a credential-management story that does not fit the "no hidden state" discipline. The OS-as-supervisor memory commits to this externalisation.
- **POST/PUT request bodies** — the HTTP/1.0 parser handles method + path only; bodies are discarded. Lifting this requires a body-bounds declaration on the service and a means to expose the body to the handler. Future slice; no urgency.
- **Headers (request or response)** — same reasoning. Request headers are discarded; response headers are limited to `Content-Length` (computed from body length). Adding `header: name: value` would be a new effect with its own slice.
- **Pthreads / shared-memory concurrency** — concurrency is via `fork()` (separate process, separate memory, kernel-supervised). Refused on principle: pthreads need locks, locks need a memory model, a memory model is a research problem in its own right.
- **Heap allocation / dynamic data structures** — no `malloc`, no growable arrays, no recursive structures without declared depth bounds. Stack-only emission stays a hard constraint.
- **User-written protocol parsers** — protocols are from a closed set (`raw_tcp`, `http_1_0` today). A user can't write a "Phase X HTTP/2 parser in Verbose"; if Verbose ever supports HTTP/2 it'll be a built-in protocol with its own emitter, audited once.
- **Inline assembly** — would punch a hole in the verifier's guarantees. Refused. If a syscall is needed and not yet declared, add a Verbose-level primitive with its own audit story; do not let users emit raw bytes.

This list is part of the model. Refusing an effect is as much a design decision as accepting one. New requests for refused effects are evaluated against the discipline above, not against "is it useful?".

## Adding a new effect — the checklist

Anyone (the AI, a future contributor, the user himself) proposing a new effect must answer:

1. **Declaration shape**: what does the source look like? What's the top-level keyword? What are the required fields?
2. **Required proof**: what does the rule that uses it have to declare in `reads:` / `calls:` / a new proof field?
3. **Canonical syscalls**: what exact syscall sequence does the native emitter produce? No options, no heuristics.
4. **Error policy**: what happens on syscall failure? Default? Opt-in alternatives?
5. **Memory bound**: is there a declared bound? What's the upper limit the verifier enforces?
6. **Audit visibility**: what appears in the binary's data section? What does `strings binary` show?
7. **Allowed contexts**: which rule outputs / handler positions / log scopes can use it?
8. **Refusals**: what shapes of the effect does this slice deliberately NOT support? Why?

A new effect that cannot answer all eight is not ready to land. Each shipped slice in this catalogue went through this exact discipline.

## See also

- `docs/spec-proofs.md` — the proof grammar this catalogue's "required proof" column references.
- `docs/known-gaps.md` — features that exist as "NOT in the model" today but are documented as deferred future slices.
- `docs/ai-act-usage.md` — how the effect model translates into the Article 12 / Article 86 audit story.
- `docs/native-designs.md` — locked emitter designs per phase (the source of truth for the "syscalls emitted" column).
