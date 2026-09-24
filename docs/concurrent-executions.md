# Bounded concurrent execution phases

Design fixed on 2026-09-23, before implementation; the interpreter reference is
now implemented. This slice gives pure source
executions an admission/lifetime contract and an executable interpreter reference.
The [native follow-up](native-concurrent-executions.md) implements Linux x86-64
threads and a fixed checked reservation. This page defines the shared semantics
and interpreter storage; native memory has its own scope and report.

## Source contract

```verbose
execution inspect_together
  @intention: "Evaluate independent analyses with bounded admission and ordered output"
  @source: concurrent_execution.intent:1
  input: Reading
  mode: concurrent
  phases: [clamp, nonnegative, label]
  on_failure: stop
  max_in_flight: 2
```

The existing six common execution fields/attributes remain mandatory and unique.
Concurrent mode requires `max_in_flight` in 1..64 and refuses `native_stack`.
Its optional `native_memory` ceiling enables native emission; without it the
interpreter remains usable and `--memory-report` can calculate the reservation.
The optional [`result_batch: B`](concurrent-result-batches.md) bounds pending
results per worker in 1..1024, defaulting to 1. Native emission groups serialized
results into a fixed buffer whose complete capacity counts towards `native_memory`.
Sequential mode keeps its required `native_stack` and refuses `max_in_flight`
and `native_memory`, as well as `result_batch`.
Names, source references, input concepts, 2..64 phases, repeated names and pure
acyclic numeric/bounded-text restrictions retain their checks. Effects, services,
recursion, nested executions and unknown phase analysis refuse. Every declaration
is checked, including unselected ones. Existing independent rule stack contracts
keep their native argv meaning.

Run the [complete example](../examples/concurrent_execution.verbose) with the
existing [reading batch](../examples/execution_stack.json):

```sh
target/release/verbosec examples/concurrent_execution.verbose \
  --run inspect_together --input examples/execution_stack.json --json
```

Plain and JSON output use the existing [execution output formats](source-executions.md#interpreter-reference).
JSON phase indices retain declaration order, including repeated rule names.

## Admission and observable order

Split the declared phases into consecutive waves of at most `max_in_flight`.
Admit every phase of a wave before publishing its results. Each phase processes
the whole original input batch in record order. Admission slots remain occupied
until the phase has terminated and its worker has been joined, including workers
waiting to publish a result. A new wave starts only after every preceding phase
has succeeded and every worker from that wave has been joined.

Within a wave, evaluation may overlap. Publish results in declaration order,
then record order, never in completion order. This yields the same successful
values/output and ordinary failure prefix as sequential execution on the same
decoded input. The source author does not expose shared mutable state or locks.
This contract guarantees a maximum; it promises neither simultaneous CPU time,
fairness, speedup nor a particular number of CPU cores.

`on_failure: stop` observes failures in publication order. Boolean false remains
sticky until that complete phase has been published, then prevents later output
and waves. An input/evaluation/output error stops at that record. A later phase
may already have computed speculatively; its results are discarded on failure.
The pure phase restriction makes this cancellation compatible with the language
effect model. Already published output remains visible; there is no rollback.

Cancellation is cooperative between evaluations. Finish any evaluation already
in progress, release its results, and join all admitted workers before returning.
A worker-start failure cancels and joins the already started part of that wave
before publishing any results from it. A worker panic/disconnection is an error,
not success; remaining workers are cancelled and joined. Previously completed
waves remain published. These host failures need not have a sequential analogue.

## Reference storage and control flow

The interpreter uses scoped host threads with immutable borrowed program/input
data. Each worker has a channel with B-1 queued slots for `result_batch: B`:
including a value in a blocked send, it can retain at most B values awaiting
publication. Default B=1 retains the original rendezvous channel. While the
coordinator publishes a received value, that worker may compute the next.
Thus at most N×B worker/channel-owned pending results plus one coordinator-owned
result can be retained, for N admitted phases. This is a count of result values,
not a bound on their bytes, temporary
evaluation values, input storage or the Rust process's memory.

The host uses its existing 64 MiB interpreter stack size per worker, subject to
host allocation failure. Those host stacks are unrelated to native phase reports.
This reference prioritizes tested semantics; no native memory/performance claim
follows from its host-thread implementation and it does not select a future
native thread/process ABI.

Control flow per wave:

1. Create a cancellation flag, at most N bounded channels and scoped workers.
2. Each worker evaluates one record, checks cancellation, sends its value, and
   repeats. It sends an ordinary error immediately or a final sticky-bool status.
3. The coordinator drains each phase in declaration order, publishing each value
   immediately. It never collects an entire phase's results.
4. On failure, set cancellation and drop **all** receivers before joining; this
   releases workers blocked in send. On success, join every worker as well.
5. Start the next wave only after successful completion of step 4.

The input batch and program outlive every scoped worker. Cancellation cannot
release their owners early. No detached task or user-visible pointer is exposed.

## Backend boundary and validation

| Path | Current support |
|---|---|
| Rust parser/verifier | Closed modes, admission count, phase types/purity/bounds and all declarations checked |
| Interpreter | Concurrent waves, ordered native-style/JSON events, bounded pending results, cancellation and join |
| Native execution entry | Linux x86-64 argv threads; requires checked `native_memory` |
| `--stack-report` on concurrent execution | Explicit refusal; use `--memory-report` for the fixed reservation |
| Explicit standalone rule or sequential execution | Existing behavior and budgets, even with an unselected valid concurrent declaration |
| WASM and self-hosted diagnostics/raw/ELF | Existing source-execution refusal |

Tests force out-of-order completion and check ordered publication, and prove
admission/wave barriers with synchronization rather than timing benchmarks.
They exercise false results, input/evaluation/output errors, worker failure,
partial admission failure, blocked sender cancellation and complete joining.
Differentials compare the concurrent reference with sequential interpretation
and the corresponding native sequential selection. They cover counts 1/2/64,
repeated phases, i64/UTF-8/text and flat records, parser/type
refusals, unselected invalid declarations, and zero-artifact backend refusals.
Existing native corpus bytes must remain unchanged. Run serialized tests, CLI
checks, bootstrap and CIDX checks before delivery.

This is a reusable execution contract for batch analyses or compiler checks.
HTTP remains an integration case for a later transport-aware scope. The native
reservation includes scheduler storage, worker stacks, pending output and guards;
it excludes initial input/code/kernel storage and is not a whole-process bound.
