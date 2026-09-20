# Native numeric local lifetimes

Implemented 2026-09-17, following PR #229's verified numeric optimization.
This changes storage placement in the existing native argv numeric-contract path.
The [language contract and backend matrix](numeric-overflow.md) stay unchanged.

## What ends a local's lifetime

The native emitter resolves each identifier to its lexical definition and finds
the last complete binding or output expression that reads it. A shadowing let's
initializer still reads the previous definition. Both arms of an `if` count as
possible uses; the analysis does not shorten lifetimes separately inside arms.

The placement description and measurements below record the initial 2026-09-17
implementation. The [shared-scratch follow-up](#sharing-dead-slots-with-expression-scratch)
removes its restriction that scratch must sit above all live locals.

After an expression puts its result in registers, the slots of locals whose last
use was in that expression are available for the next binding. The allocator
chooses the lowest available slot. It never moves a live value to close a hole.
Expression scratch starts above the highest still-live local, and an expanded
callee starts above all its caller's live locals and operands. The resulting
maximum slot offset determines the reserved frame.

For example, in a chain of `let next = previous + input.x`, each result can occupy
its predecessor's slot. Keeping an alias for later use extends the required live
storage; values used together cannot overwrite one another. A final expression
that reads every local keeps every one of them live, as it did before.

The pass operates only after verification and constant lowering. Every nonconstant
let still evaluates once in source order, including an unused let; an unused
result simply receives no persistent slot. An unsafe unused initializer is still
rejected from the original source. Input guards, call-expansion limits, outputs,
failure behavior and the separate BoundsError path remain unchanged.

## Cost and limits

All lifetime and placement bookkeeping runs in the compiler. The executable has
no free list, lifetime checks, allocator, new copies or garbage collection. Some
dead-result stores disappear, and shorter frame offsets may reduce instruction
size. This is not register allocation or last-use tracking inside an expression.
Holes below a live local are reused by later bindings, not expression scratch.
The existing conservative 2 MiB frame limit still applies.

## Sharing dead slots with expression scratch

Implemented 2026-09-20, following PR #237. A single compile-time slot pool now
serves numeric locals and expression temporaries throughout an expanded entry.
Any free slot can hold subsequent scratch or a callee local, including a hole
below a still-live caller value. The compiler picks the lowest free slot; it
never moves a live value to make room.

```verbose
let early = input.x + 1
let keep = input.y + 2
let unused = early + input.x
out = keep + computation(input)
```

After the third initializer, `early` is dead. Its slot can now hold an operand
or a temporary inside `computation`, while `keep` remains protected. The unused
initializer still executes. The result of an expression reaches registers before
its scratch is released; already evaluated operands remain allocated until their
enclosing expression finishes. A callee releases only its own locals. Opposite
arms of a branch reuse scratch after each arm's result, while locals read in
either arm remain protected through the complete conditional.

Last-use analysis still follows lexical definitions: aliases keep their copied
numeric values, and a shadowing initializer reads the previous definition.
There is no finer last-use analysis within an expression, call specialization,
register allocation or change to numeric proofs. Input fields keep their separate
entry slots. The two-word BoundsError path and bounded text placement are separate.

The compiler maintains one free set and a stack of temporary slot identities.
Free-set operations are logarithmic in the peak slot count; lifetime schedules
are linear in binding count, with one visit per local read. Existing source and
call-expansion limits still bound the work. Branches are traversed structurally,
without enumerating execution paths or building pairwise conflict graphs.

Only frame size and slot offsets change in the generated program. There is no
runtime free list, reference counting, tracing collector, extra lifetime check
or relocation copy. Scalar words retain their representation and eager order.
This is [compile-time placement, not runtime garbage collection](memory-management.md).

A [subsequent direct-operand pass](numeric-operands.md) also avoids making a
scratch copy of a numeric field/local that already has a stable slot. Computed
operands retain scratch; the complete-expression lifetimes described here keep
the directly read values alive.

### Shared-scratch layout observations

[Raw observations and complete fixtures](measurements/numeric-scratch-reuse-2026-09-20.json)
compare merged PR #237 (`c06240f`) with this implementation, identified by compiler
and source hashes. Both compilers accept all three sources. Reuse the
[layout reproduction snippet](numeric-optimization.md#branch-folding-layout-observations)
with this report path to rebuild them.

| Fixture | ELF bytes, before → after | Reserved frame bytes, before → after |
|---|---:|---:|
| 32 dead locals below a live value, then deep arithmetic | 6225 → 6144 | 840 → 584 |
| The same holes reused by successive expanded calls | 8825 → 8663 | 848 → 592 |
| Control retaining all 32 locals through the final expression | 7505 → 7505 | 840 → 840 |

Each fixture matches both interpreters and native versions against an integer
oracle on 105 input records. Ten invalid argument sequences and malformed/partial
records after valid output preserve stdout, stderr and status. All six allocation
syscall traces are empty; four unsupported entry-mode refusals preserve existing
artifacts. The all-live control is byte-identical: values that remain necessary
cannot share their slots. These synthetic observations establish code/frame
sizes, not CPU speed, process RSS or hardware cache behavior.

The 180 existing top-level examples retain identical compiler diagnostics and
178 byte-identical native artifacts, with the same two refusals. A repeated
reference compilation also reproduces every artifact and diagnostic.

Smaller reserved frames do not by themselves establish lower RSS or better cache
hit rates. Timing must be compared independently; the benchmark includes a case
with 128 simultaneously live locals as a control where reuse is unavailable.

## Measurements

Reference: merged PR #229, commit `a33bcb943852b433fbc1d0369d543a6839509156`.
The candidate is this change's working tree, identified by the compiler hash in
the [first raw report](measurements/numeric-local-lifetimes-2026-09-17-run1.json).
Both compilers consume the same fixture sources. Measurements use the shared
[harness](../tools/benchmark_numeric_contract.py), Ryzen 7 5800X/WSL2, CPU 2,
16000 argv records, two warmups and 11 alternating repetitions per binary.

| Workload | Reserved frame, before → after | ELF bytes, before → after |
|---|---:|---:|
| 128 successive locals | 1080 → 72 | 7364 → 6680 |
| 48 calls with 128 successive locals each | 1088 → 80 | 328421 → 295448 |
| 128 locals all read by the final expression | 1088 → 1088 | 12478 → 12478 |

The three original constant/arithmetic/call workloads and the all-live control
produce byte-identical binaries before and after. All six workloads agree on
201 valid inputs each (1206 values total), plus missing, malformed, out-of-range
and partially processed input sequences, checked on stdout, stderr and exit
status. The interpreter supplies a third result comparison for valid inputs.

The first timing series is inconclusive: successive locals have medians
19.415 → 21.958 ms (MAD 1.190 → 2.231 ms), and local calls 424.660 → 440.831 ms
(MAD 31.051 → 32.551 ms). But the byte-identical constant control also changes
14.176 → 16.829 ms, while the byte-identical all-live control changes
21.589 → 19.703 ms. Those control differences prevent attributing the observed
timing changes to storage reuse. They do not establish absence of a regression.
The raw observations are retained rather than presenting the smaller frame as
a measured execution-speed gain.

A [second series](measurements/numeric-local-lifetimes-2026-09-17-run2.json) uses
32000 records and 31 alternating repetitions on the two changed workloads and
the constant/all-live controls. It reproduces the exact frame and ELF sizes.
Successive locals measure 47.362 → 49.511 ms (MAD 8.890 → 5.986 ms); local calls
883.787 → 900.138 ms (MAD 52.436 → 68.620 ms). The observed differences remain
small relative to dispersion. Current external host load was unconfirmed during
this run. Neither series establishes a speedup or rules out a small regression;
the established improvement is storage size, with further quiet-host timing
needed for a runtime-cost conclusion. Both series, including every sample, are
retained.

The user subsequently reported no heavy workload during those measurements.
The [updated measurement protocol](numeric-benchmark.md) separates child CPU
accounting from elapsed time and refuses incoherent CPU comparisons. This
measurement work does not change the storage placement or original binaries.

Timings include Python argument setup, process startup, parsing and output
syscalls. External Windows activity was not independently measured. Reserved
storage and ELF sizes are deterministic; RSS and hardware cache counters were
not measured. No HTTP throughput claim follows from these scalar workloads.

```sh
python3 tools/benchmark_numeric_contract.py \
  --compiler target/release/verbosec \
  --reference-compiler /path/to/verbosec-from-a33bcb9 \
  --reference-revision a33bcb943852b433fbc1d0369d543a6839509156 \
  --records 16000 --repeats 11 --cpu 2 \
  --output /tmp/numeric-local-lifetimes.json
```

Choose a CPU in the host's allowed affinity, and keep benchmarks separate from
tests, builds and other heavy workloads. `--cases` can restrict the comparison.
