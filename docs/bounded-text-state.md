# Copying bounded text into persistent state

Implemented 2026-09-15 in the native Linux x86-64 backend.

A sequential HTTP service can copy the result of a pure bounded text rule into
one of its existing text state fields:

```verbose
state:
  last : text [..4098] = "none"
after:
  set last = remember_text(req)
```

`remember_text` declares `out : text [..4098]`. Its result capacity must fit the
destination's declared capacity, even when a particular request produces fewer
bytes. The compiler also verifies the callee body and its dependencies using the
[bounded text rules](bounded-text-output.md): pure acyclic calls, known byte
capacities, lexical lets, aliases and conditionals. An annotation alone is not
evidence that its implementation fits.

The complete `set` source must be `callee(input)`. That entry must explicitly
declare a bounded text result; dependencies inside it may use inferred bounds.
The argument is the handler's original `HttpRequest` binding. Handler and callee
may name their inputs differently. Handler lets, including names such as `body`,
`path` or `req`, do not replace the parser's input to this call. Constructed
arguments and expressions wrapping the call are outside this slice.

## Ownership and lifetime

| Question | Contract |
|---|---|
| Persistent owner | The sequential service owns a fixed buffer for each state field, allocated in its service frame before serving requests. |
| Destination capacity | Existing text state bounds are 1..=65536 bytes. The callee's public result capacity must fit; zero-byte results are allowed. |
| Temporary owner | Each `set` call evaluates once in its own bounded invocation region below the service frame. Aliases inside that evaluation share immutable values. |
| Transfer | Copy exactly the counted result bytes into the state's buffer, then update its length. The state's pointer never becomes a pointer into temporary or request storage. |
| Reclamation | Restore the temporary frame after the copy, before the next mutation. The persistent bytes remain valid across subsequent requests and request-buffer reuse. |
| Excess capacity | Unknown or excessive static requirements refuse compilation before artifact emission. A defensive native length check precedes the copy; a violated invariant restores the temporary frame and follows the existing operator-abort path (exit 1). There is no truncation or heap fallback. |

The native [2 MiB invocation storage ceiling](bounded-text-storage.md) applies to
each formatter evaluation, including its fixed slots, placed live buffers and
scratch allowance. Persistent state and the surrounding handler/transport frame
are separate storage. This is not a total process memory quota. Reclamation does
not erase bytes or promise to return pages to the operating system.

HTTP bodies are counted byte spans: embedded NUL and non-UTF-8 bytes are preserved
by the native copy. This extends no general text input decoding contract. The
interpreter represents text as Rust strings and supplies a value reference for
valid UTF-8 formatter inputs; it does not execute persistent services.

## Ordering and failure

Existing service ordering remains: handler, logs, response write, then `after`
mutations in source order. A later `set copy = state.last` sees an earlier copied
value. This is not an atomic transaction: an earlier mutation is not rolled back
if a later legacy expression fails.

With paired request/response deadlines, malformed requests and failed response
writes skip `after`. Successful sending means that the kernel accepted all bytes
within the deadline, not that the remote application processed them. See
[bounded HTTP I/O](http-bounded-io.md). Services without paired deadlines retain
their existing transport and failure policies; this text contract adds none.

## Scope and backend support

| Path | Support |
|---|---|
| Rust verifier | Callee body, public capacity, destination capacity, original input and service context checked |
| Interpreter | Pure formatter evaluation; no persistent service execution |
| Native sequential HTTP | Complete bounded text call copied into existing text state, with or without paired transport deadlines |
| Native forked / pooled HTTP | State mutations remain refused; no shared state across workers |
| Native raw TCP | This bounded call transfer is explicitly refused; existing state operations retain their behavior |
| WASM | Explicit refusal of the bounded text contract, including a service whose only participating call is in `after` |
| Self-hosted compiler | Existing bounded output annotation refusal before ELF or raw machine-code emission |

State-reading handlers still use the existing service emitter. A handler that
itself participates in the bounded text call graph must still have no state,
logs or `after` mutations. Effects inside formatters, recursion, collection or
Result storage, resource ownership and thread sharing remain outside this slice.
No new syntax, callable ABI or general ownership type system is introduced.

## Example and checks

The [complete example](../examples/bounded_text_state.verbose) answers with the
previous stored result, then remembers either a wrapped POST body or a GET path:

```sh
cargo run -- examples/bounded_text_state.verbose --native /tmp/text-state --run remember_http
/tmp/text-state
# In another terminal:
curl --data-binary 'hello' http://localhost:18965/  # prev:none
curl http://localhost:18965/next                  # prev:[hello]
curl http://localhost:18965/                      # prev:</next>
```

[Socket tests](../src/http_tests/bounded_state_tests.rs) compare formatter values
with the interpreter where representable and check exact wire bytes, empty and
maximum-capacity results, repeated binary requests, alias/shadowing behavior,
source-order mutations, stable idle stack/descriptor counts and refusal before
artifact creation. The optional Linux/strace check injects response errors and
checks that state persists unchanged, the next request succeeds, and no heap
allocation syscall occurs. A separate binary with a deliberately lowered copy
guard exercises the normally unreachable capacity backstop and requires exit 1
after the complete response, without an allocation syscall:

```sh
cargo build --locked
python3 tools/check_bounded_text_state.py
```
