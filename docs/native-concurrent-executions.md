# Native bounded concurrent executions

Design fixed before implementation, 2026-09-23; now implemented. This extends the shipped
[interpreter reference](concurrent-executions.md) to Linux x86-64 argv entries.
The reference's consecutive waves, ordered publication, sticky boolean failure,
partial-admission failure and complete joining remain the observable contract.

## Source and budget

Concurrent executions may additionally declare `native_memory: N`, a byte ceiling
in 1..268435456. Existing declarations without it still run in the interpreter;
native emission requires it. Sequential executions refuse this field.
`native_stack` remains reserved for sequential entries and independent rules.

`--memory-report [--json] --run <execution>` computes the concurrent layout even
without a ceiling, without creating an artifact. A declared insufficient ceiling
refuses every entry path, including unselected executions. The existing
`--stack-report` does not describe this off-stack allocation.

The ceiling covers one fixed, page-rounded userspace mapping: scheduler words,
per-worker synchronization/termination words, one serialized result buffer per
worker, worker stacks with their transient scratch, padding and inaccessible
guard pages. It is a bound on reserved virtual address bytes for this execution,
not RSS, touched bytes or whole-process memory. Code/ELF, kernel-provided initial
argv/environment, kernel thread/descriptor/synchronization storage and external
output storage are excluded explicitly. The coordinator uses registers and the
mapping, with zero additional ordinary entry stack. No per-record allocator,
collector or unbounded result queue is introduced.

There are min(max_in_flight, phase count) reusable lanes. A lane's stack and result
capacities are the maxima of phases assigned to that lane across consecutive
waves. Sum those lane reservations, not independent source budget declarations.
Every phase's actual lowering supplies its stack and serialized result capacities;
unknown layouts refuse. Keep existing independent rule budgets checked against
their original standalone argv layouts. The report exposes both logical storage
and page reservation, including per-phase and per-lane calculations.

## Running the example

[`native_concurrent_execution.verbose`](../examples/native_concurrent_execution.verbose)
adds `native_memory: 20480` to the reading analyses:

```sh
target/release/verbosec examples/native_concurrent_execution.verbose --memory-report --json
target/release/verbosec examples/native_concurrent_execution.verbose --native /tmp/readings
/tmp/readings a 2 b 1000
```

It prints `2`, `100`, `true`, `true`, `a:2`, `b:1000` on separate lines,
with status 0. The reservation is 4096 bytes for control/results plus two lanes
of 4096 stack bytes and 4096 inaccessible guard bytes: 20480 bytes. The report
also exposes the smaller actual stack bounds and output capacities; reserved
pages and logical live storage are distinct. A 20479-byte declaration refuses;
a larger sufficient ceiling changes no native bytes.

The CLI report and native concurrent emission use the same original-source
lowering. Numeric simplification remains inside the checked numeric emitter.
Standalone/sequential optimization paths are unchanged. New code is checked by
following both branch arms and skipping embedded literals through their jumps;
this is an instruction-boundary check, not a formal proof of the native runtime.

## Threads, ownership and registers

The emitted binary uses raw Linux `clone` with shared VM, file descriptors and
signal handlers, `CLONE_THREAD`, `CLONE_PARENT_SETTID` and
`CLONE_CHILD_CLEARTID`. No libc, TLS or language-visible pointer/lock API is used.
The kernel writes the TID before returning to the parent; on thread exit it
clears the aligned TID word and wakes the joining coordinator. Waiting for that
clear precedes reuse or unmapping, even after the final result/status is received.

One `mmap(PROT_NONE)` reserves the complete checked layout before admitting work.
Checked `mprotect` calls enable control/results and each worker stack, retaining
one inaccessible lower guard page per lane. Mapping/protection failure starts no
workers. All waves reuse this mapping; it is released after every worker joins.
Initial argv storage remains borrowed, immutable and live until process exit.

Coordinator: rbp is the mapping base; original entry rsp is saved in the header;
r15 addresses the selected lane. Output cursors/counts are registers. Worker:
r12=argc, r13=argv, r14=record cursor, r15=lane control address. rbp addresses its
private input/body frame. Existing checked numeric lowering and bounded-text
fragments preserve r12-r15; text invocation frames also save/restore rbp/rbx.
No worker writes stdout or changes another worker's result or input frame.

Workers parse one complete argv record, validate its numeric/text bounds, execute
the existing pure checked body, and serialize one result into their fixed lane
buffer. Numbers need at most 20 bytes plus newline, booleans six, text its proved
capacity plus newline; flat record capacities include names, punctuation and each
number/text field. NUL in result text is data. Native argv cannot carry NUL input.
The new worker parser checks complete decimal i64s and incomplete records for
all phase types; older standalone/sequential text argv parsing is unchanged.
Empty input and input/kernel failures return status 1 without native contextual
stderr diagnostics; the interpreter retains its explanatory error messages.

## Rendezvous and cleanup

Each lane has an aligned 32-bit state word: EMPTY, READY, DONE, ERROR or CANCEL.
Only the worker fills its buffer. Publishing READY uses an atomic compare-and-exchange from EMPTY after
the bytes and length are stored; a prior CANCEL wins. The coordinator reads that buffer only after
observing READY, writes it completely (retrying EINTR and advancing short writes),
then exchanges EMPTY and wakes the worker. The worker waits for EMPTY before
evaluating its next record. This deliberately tighter backpressure still satisfies
the reference's pending-value maximum and avoids a coordinator copy.

Waits use futex compare-and-block loops, rechecking after wake, EINTR or EAGAIN;
there is no busy-spin loop. State transitions wake waiters. Native x86-64 ordering
and the locked exchange publish the payload before its state. The coordinator
admits the complete wave before receiving any of its results. It consumes lanes
in declared order, then joins the whole wave before the next admission.

A boolean false remains sticky until all valid records of that phase have been
published. An input/evaluation failure publishes only its prior complete records.
A clone failure publishes none of its partially admitted wave. On any ordinary
failure, exchange CANCEL into every admitted lane and wake it before joining
any worker. Cancellation is checked between records and before publication;
an evaluation already in progress may finish. A cancelled worker must not
overwrite CANCEL with READY/DONE. Joining uses the kernel-cleared TID word and
non-private FUTEX_WAIT, matching the kernel's clear-TID wake.

Block SIGPIPE before admission so failed writes take the same cancellation/join
path rather than terminating the coordinator alone. Signals and unrecoverable
kernel synchronization failures are process-level failures; no recovery or
completed-prefix guarantee is claimed for a fatal signal. The final coordinator
uses exit_group, while workers use raw exit. Checked host failures are status 1.
No admission slot, buffer or stack may be reused before the relevant TID clears.

## Validation and boundaries

Compare native concurrency with the interpreter and native sequential phases on
valid records, boolean failures, numeric/UTF-8 boundaries, text/flat records,
aliases, repeated phases, limits 1/2/64 and multiple waves. Check malformed input
and partial prefixes independently where older argv parsing is permissive.
Exercise slow output/backpressure, early output failure and partial worker-start
failure. Observe actual clone, futex, mapping and join behavior; inject failures
at recorded syscall emission sites, never by searching arbitrary finished bytes.
Check exact/minus-one budgets, capacity arithmetic, phase/lane maxima, guard-page
boundaries, deterministic emission and preservation of artifacts on refusal.

The source gate continues to exclude effects, recursion, nested executions,
services and unknown pure phase layouts. Only argv native execution is added;
stdin/stream, HTTP execution scopes, WASM and self-hosted execution stay refused.
Existing standalone/sequential bytes must remain identical to the reference.
Run serialized Rust tests, CLI checks, CIDX and bootstrap before delivery.

Kernel ABI references: [clone](https://man7.org/linux/man-pages/man2/clone.2.html),
[futex](https://man7.org/linux/man-pages/man2/futex.2.html). The generated runtime
targets their Linux x86-64 raw syscall ABI, not the C wrapper calling convention.
