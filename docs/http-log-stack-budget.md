# Composing bounded service logs with the HTTP stack ceiling

Design recorded before implementation, 2026-09-25. Extend the existing service
`native_stack` contract to the already verified synchronous `append_file` log
boundary. This is lifetime composition between a producer and sequential
consumers; HTTP supplies a concrete transport frame and borrowing example.

## Contract

Keep the existing syntax, content grammar, ordering and failure policies. A pure
acyclic bounded-text handler may have literal or flat-concat service logs under
its service-level ceiling. Content capacity remains at most 1 MiB per log; it is
not the log emitter's stack reservation. The response region stays live through
all logs and sending. Each log frees its temporary storage before the next.

The service bound becomes:

```
8 + service_frame_bytes
  + max(startup_scratch,
        handler_frame_bytes + saved_handler_registers
          + max(handler_expression_scratch, largest_log_stack, response_scratch))
```

Parser-only request body use and the timestamp's eight-byte slot count in the
fixed service frame. Timestamp capture uses the existing socket-I/O timespec
slots, without extra transient stack. Sequential, forked/capped and pooled
services retain both socket deadlines. State, after mutations, handler effects,
graceful-shutdown signal frames and unknown layouts remain refused. Every source
declaration checks, including unselected services and direct backend entry APIs.

This is additional explicit native stack per process, not a pool-wide sum or an
RSS, disk-space, durability or latency bound. Blocking file I/O and the existing
single-write semantics remain: short writes are not retried, `drop` continues
after errors, and `abort` exits before sending. Earlier logs are not rolled back;
a log entry does not prove successful client delivery. Socket deadlines do not
bound logging latency.

## Layout source and compatibility

Share the concat emitter's argument classification and sizing layout with the
report. Preserve its emitted bytes, including its current 21-byte numeric
allowance and eight-byte alignment. Dynamic concat sizing currently adds the
actual lengths of method/path fields on top of their static allowances; count
both instead of silently reporting only logical output capacity. Counted request
and response bytes use their checked upper capacities. Unknown argument layouts
refuse rather than becoming zero.

A literal log has no temporary buffer. Static concat reserves its aligned
emitter size. Dynamic concat has a bounded aligned reservation, restored through
its saved stack pointer. Numeric formatting adds its existing 24-byte scratch
peak; sizing a NUL-terminated field can briefly push one word before reserving
the buffer. These are sequential peaks, not cumulative additions. File paths
and literal log bytes are embedded data, not stack storage.

The report retains its schema and existing fields. Logs add an ordered list
with source index, error policy, literal/static/dynamic strategy, checked content
capacity, buffer bound, sizing/formatting scratch and total log peak. A separate
`log_stack_bytes` is their maximum. No-log reports remain identical. The full
service ceiling gates this actual layout; no new runtime instructions or heap
management are added solely for a sufficient declaration.

WASM and self-hosted emission retain their existing explicit refusal of service
ceilings/bounded handlers. No self-hosted source extension is needed. Rule-level
argv stack proofs keep their separate meaning and remain refused in services.

## Validation

- Exact/one-byte-small ceilings, all dispatch modes, literal/empty/static/dynamic
  logs, numbers, repeated fields, method/path over-reservation, counted binary
  body/response data, parser-only body use and timestamps.
- Several sequential logs must combine by maximum while retaining the handler;
  source capacities and emitter buffer reservations must remain distinguishable.
- Independently decode stack changes, cleanup, failure joins and frame resets;
  supply independent fixture bounds for dynamic allocations. Compare bytecode
  with a sufficient declaration removed and with the reference compiler.
- Real requests, aliases and shadowing, repeated worker reuse, empty/binary
  bodies, malformed input and request timeout. Exercise open/write/send failures
  with both policies and check frame/descriptor reclamation and absence of heap
  syscalls using the existing strace fault harness.
- Preserve refusals and existing artifacts for unsupported forms. Run the
  serialized Rust suite, CLI checks, CIDX and the existing bootstrap CI. Compare
  the complete pre-existing example corpus. Performance measurement and the
  separate CI temporary-disk investigation remain deferred.
