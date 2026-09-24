# Native stack budgets for bounded HTTP services

Design recorded before implementation. Extend source-selected storage ceilings
from argv compositions to a closed HTTP service path. The contract measures the
emitted service frame and the complete lifetime of its bounded handler result,
including response serialization. HTTP is the first transport consumer of the
existing invocation layout; this does not replace its reusable language contract.

## Source and scope

A service can declare `native_stack: N`, an integer byte ceiling in
1..2,097,152. It applies to additional explicit native stack storage per process,
starting before the service prologue, including startup and all request paths.
It is not a cumulative request counter or a total-process/whole-pool memory quota.
Omitting the declaration preserves existing acceptance and emitted bytes.

This first service scope requires:

- `protocol: http_1_0` and both existing request/response deadlines;
- a handler in the existing pure, acyclic bounded-text call graph;
- no service state, after mutations or service logs;
- no `shutdown_timeout`: the draining pool's asynchronous signal frames need
  separate accounting before this contract can cover that mode.

Existing sequential, forked (including capped admission) and pooled process
dispatch are supported within those limits. Worker/PID/admission bookkeeping
already present in the emitted frame counts too. The reported per-process ceiling
is a conservative bound for both workers and their supervisor, not their sum.
Unreferenced resource declarations do not reserve storage. Handler effects,
resources, entropy, recursive calls and unknown shapes remain refused by the
bounded-text checker. Legacy unbounded handlers do not gain a budget by assertion.

The service declaration does not reinterpret a rule's `proofs.native_stack`,
which still describes a standalone argv entry and remains refused when reached
from a service. Every service ceiling, selected or not, must check before input
execution or artifact emission. Direct native APIs use the same gate. Invalid or
unknown analysis produces an attributable diagnostic and preserves existing files.

## Layout and lifetime

Use the existing HTTP dynamic emitter and bounded handler fragment. Prepare
code and layout together; verification/reporting never start a listener or write
an artifact. No runtime instructions, allocation, guard or calling convention
are added solely for a sufficient declaration.

The fixed service frame contains request/output/fd words, optional counted-body
words, bounded socket-I/O bookkeeping, admission/pool bookkeeping, and the
`max_request` receive buffer. Its base pointer save adds eight bytes. Startup
temporaries have a 16-byte peak. Receive/parse/dispatch use fixed frame slots.
The handler reserves its placed scalar words and text buffers plus saved rbp/rbx
(16 bytes), and retains that complete region through response sending. Expression
formatting and response header formatting are sequential peaks; the latter uses
one 24-byte numeric buffer at a time. The enclosing calculation is:

```
8 + service_frame_bytes
  + max(startup_scratch_bytes,
        handler_frame_bytes + 16
          + max(handler_expression_scratch_bytes, response_scratch_bytes))
```

Sequential/pool close paths reset rsp to the fixed frame even after malformed
input, timeout or failed send; forked children exit. Handler buffers are never
freed before sending consumes their pointers. No per-request storage accumulates.
Initial argv/environment, kernel socket/process storage, code/static data,
interpreter allocations and total RSS are excluded. Cache residency and speed
are not inferred from the byte ceiling.

`--stack-report --run <service> [--json]` exposes a separate schema-1 service
report: target, HTTP entry mode, per-process scope, service/handler/concurrency,
declared bytes, total, fixed frame breakdown, nested handler placement and peak
scratch. Default report selection may choose a service when no execution is
declared; existing explicit rule/sequence reports keep their meaning. Reporting
an eligible service without a ceiling is useful, but unsupported services refuse
instead of falling back to a handler-only argv estimate.

## Backend and verification boundary

The Rust verifier and Linux x86-64 native service path support the contract.
The interpreter can still evaluate pure handlers but does not execute services
or claim their native storage. WASM and the self-hosted compiler explicitly
refuse service ceilings before any artifact. Extend the self-hosted token gate
at immediate service depth, retaining ordinary `native_stack` identifiers in
fields, locals, strings and comments.

Validation must cover parser ranges/duplicates, exact and one-byte-small ceilings,
all declarations including unselected services, direct backend gates, deterministic
emission, and byte identity with the declaration removed. Independently decode
emitted stack changes/branches across startup, receive, computation, sending and
close/reset. Exercise numeric formatting, body/path aliases, retained calls,
branches and empty/binary text; run repeated real requests and failure recovery
through the accepted dispatch modes. Refuse every excluded context without
overwriting artifacts, including isolated self-hosted token-gate probes.

Run serialized Rust/CLI suites, CIDX and the two-generation bootstrap. Compare
pre-existing example outputs against the reference compiler; the self-hosted
compiler's own source changes deliberately for the refusal gate. Performance
measurements and the separate CI temporary-disk investigation remain deferred.
