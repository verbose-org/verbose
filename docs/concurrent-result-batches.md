# Bounded result batches

Design fixed before implementation, 2026-09-24. The first
[native concurrency measurements](concurrent-benchmark.md) identify per-result
rendezvous as a substantial cost. This slice bounds and amortizes those exchanges;
it does not change the pure phase subset or promise parallel throughput.

## Language contract

Concurrent executions may declare `result_batch: B`, an integer in 1..1024,
defaulting to 1. Sequential executions refuse this field. Every declaration,
including an unselected one, is checked. This is a pending-result capacity,
not a requirement to delay each result until exactly B results exist.

Up to B completed values may be retained per worker/channel, plus one value
being published by the reference coordinator. Input storage and temporary
evaluation values are outside this count. The interpreter uses a channel with
B-1 queued slots and at most one value in a blocked send. Native workers share
one fixed serialized batch with the coordinator and wait for its consumption
before starting the next batch. They may publish fewer than B results at end of
input or before a recoverable input/evaluation failure.

Admission remains in consecutive waves. Publication retains phase order then
record order. Boolean false remains sticky until its complete phase is
published. An invalid record first flushes all preceding complete records from
its partially filled native batch, then reports failure. No later phase/wave
becomes visible. Cancellation may discard unpublished later-phase batches; it
still checks between records and joins every admitted worker. Partial admission
failure publishes none of the partially admitted wave. Fatal host failures have
the existing process-level boundary.

Output writes may now contain several results. Partial writes advance the byte
cursor, EINTR retries, and other failures cancel/join as before. A batch is not
an atomic output transaction. Already accepted output is an ordered byte prefix;
neither result-boundary atomicity nor rollback is promised on output failure.

## Native layout and control flow

Keep the existing 64-byte lane control block. For B > 1 its words are:

| Offset | Meaning |
|---:|---|
| 0 / 4 | Atomic state / kernel-cleared TID (32 bits each) |
| 8 | Serializer's record length while filling; batch length when READY |
| 16 | Terminal success/sticky-failure status |
| 24 | Serializer's next record destination |
| 32 | Worker stack top |
| 40 | Immutable batch base |
| 48 | Committed complete-record bytes |
| 56 | Complete-record count |

The existing serializers continue to write at offset 24 and finish by storing
their record length at offset 8. Only after successful serialization does the
worker add that length to offsets 24/48 and increment offset 56. Before each
record, count < B, so one complete result's proved maximum still fits. No byte
from an incomplete record contributes to the published length. Shared lengths
are read by the coordinator only after READY publication.

On a full batch: copy committed length to offset 8, publish READY with the
existing locked transition, wake, then wait for EMPTY or CANCEL. After EMPTY,
reset destination to base and committed bytes/count to zero. At end or input
failure, save terminal status, flush any committed partial batch, then publish
DONE. Cancellation wins every publication transition. The coordinator reads
the immutable base, writes the entire published length, and acknowledges once
per batch. Native scalar and bounded-text bodies and worker stack bounds stay
unchanged. The B=1 emitter path retains its existing instruction bytes.

Each lane allocates `B * max(per-record serialized capacity of assigned phases)`
bytes, using checked arithmetic. Control and output are padded/page-rounded as
before; worker stacks/guards are unchanged. `--memory-report` exposes B, lane
per-record capacities and total batch capacities. The actual reservation must
fit `native_memory` and the 256 MiB implementation ceiling. There is still one
upfront mapping, no per-record allocation, no GC and no busy waiting. Code,
initial argv/environment and kernel/output storage remain outside the ceiling.

## Validation and measurements

Implement the interpreter reference before native emission. Test closed syntax
and programmatic AST gates; default/explicit 1 equivalence; exact/minus-one
reservations; checked large-capacity refusal; full and partial batches; malformed
input after a valid prefix; false values at batch boundaries; UTF-8, NUL and flat
records; multiple waves; cancellation/backpressure; admission/output syscall
failures; joining before reuse; and original-interpreter/native parity.

Keep existing default native examples byte-identical to the reference compiler.
WASM/self-hosted execution remains refused before output. Run serialized normal
tests, CLI checks, CIDX and bootstrap. Retain the historical benchmark; compare
fresh sequential, default concurrent, and batches 8/32/128 on identical inputs
with balanced repeated order, independent output oracles and separate memory/
syscall observations. No build/test overlaps timing. Record current host load
and retain all observations. Phase-order backpressure still limits useful compute
overlap on long lots even if larger batches reduce synchronization costs.
