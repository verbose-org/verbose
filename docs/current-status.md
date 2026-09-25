# Current implementation status

Documentation baseline reviewed against the checkout on **2026-09-05**. This page
is the entry point for present implementation scope. Dated journals and slice
designs record their own milestones; a statement that something is pending there
is not automatically a current limitation.

## The language-design goal

Verbose asks how much useful information a language can require authors to make
explicit when LLMs can write it: dependencies, bounds, effects, and optimization
intent. The goal is a specification any LLM can work from, with human authorship,
inspection, and experimentation also supported. The bundled generators use Claude;
the language and compiler are independent of that choice. Broad model authorability
remains an evaluation question.

Verbose's general-purpose direction covers command-line tools, data processing,
compilers and long-running services within explicitly supported, verifiable
subsets. Memory, resource, effect and concurrency contracts should be reusable
across these applications. HTTP remains a concrete example for exercising their
composition; these goals do not extend the implementation boundaries listed below.

Across these applications, the [memory design criterion](../ARCHITECTURE.md#memory-as-a-language-design-criterion)
asks who owns storage, its capacity and lifetime, and what happens at exhaustion.
The aim is useful, predictable specialization; matching Apache's feature list is
not the project's identity or a claim of general superiority.

[Memory management](memory-management.md) distinguishes compile-time slot/buffer
reuse and scoped arenas from runtime garbage collection, with the current limits.

Strict numeric and supported pure bounded text rules can declare
`proofs.native_stack: N`. The compiler checks an upper bound on additional native
argv stack storage against this byte ceiling, using actual entry/invocation
frames, placed text buffers and transient storage. `--stack-report` exposes the
calculation as text or JSON. This does not bound interpreter or total process
memory; see [native stack budgets](native-stack-budget.md).

Pure bounded HTTP services can now declare a separate
[`native_stack` ceiling](http-stack-budget.md) per process. It includes reception,
dispatch, the expanded handler and response storage retained through sending.
Sequential, forked/capped and pooled modes are supported with both socket
deadlines. [Sequential bounded logs](http-log-stack-budget.md) now contribute
their maximum temporary requirement while the handler result remains retained.
Persistent state and graceful-shutdown signal frames remain outside this
service contract. A sufficient declaration adds no native
instructions; `--stack-report` explains the same layout used for emission.

The first [composition across execution phases](sequential-stack-budgets.md)
runs checked rules sequentially over the same argv batch. It releases each frame
before the next phase, combines bounds by maximum and stops after a failed phase.
A [source `execution`](source-executions.md) now names this scope and declares
its input, ordered phases, failure policy and aggregate stack ceiling. Every
declaration is checked, including unselected entries; native code stays identical
to the corresponding explicit phase selection.
The interpreter also executes these declarations against the original verified
AST on JSON records, with matching phase order and boolean failure policy.
Results stream as native-style output or typed JSON events; its host memory
remains outside the native budget. This supplies a reference for optimized
native execution without extending the declared phase subset.
An interpreter reference now supports [bounded concurrent waves](concurrent-executions.md):
`mode: concurrent` requires `max_in_flight`, orders publication and joins all
admitted workers before a new wave or return. The optional
[`result_batch`](concurrent-result-batches.md), default 1, bounds pending results
per worker. This bounds admission/pending result counts, not host memory.
The [native implementation](native-concurrent-executions.md) now uses Linux
threads with a fixed `native_memory` reservation and `--memory-report`. The
calculation covers reusable worker stacks, control/result batches, padding and guard
pages, with ordered publication and full joins. Initial argv/code/kernel storage
are excluded; this is not RSS or total process memory. Native stdin/stream,
service scopes and persistent state between records remain separate work.
For sequential value transfer, a [bounded pipeline](pipeline-executions.md) now
passes each record through declared phases and publishes only its final result.
The compiler checks concept links, transferred capacities/ranges and the whole
shared invocation frame, including retained values. Native argv uses strict i64
and complete-record guards; the original-AST interpreter provides the reference.
This pure, acyclic flat-record subset adds no allocator or GC. Concurrent
pipelines, persistent cross-record state and service integration remain separate.
Those compositions now admit [proved numeric arithmetic](bounded-record-arithmetic.md):
every intermediate must fit i64, division/remainder exclude exceptional inputs,
and computed field intervals must fit each consumer's public domain. The same
operations work in ordinary bounded text rules and existing pure HTTP fragments.
They reuse the placed word slots and stack accounting without runtime interval
checks or allocation. This subset uses single intervals without branch narrowing;
the richer scalar `hints.overflow` contract remains separate.
The [initial measurements](concurrent-benchmark.md) show substantial per-result
synchronization costs. The [batch comparison](concurrent-result-batches.md#recorded-result-2026-09-24)
measures reduced exchanges within the same ordered publication contract: on the
recorded machine, batches of 32 retain the same reserved pages in the fixtures;
batches of 128 beat sequential elapsed time on the long synthetic compute case.
Cheap phases remain faster sequentially. Bounded memory is not a speedup guarantee.
An optional [predicted workload](workload-profiles.md) now describes weighted
batch sizes, an elapsed/CPU objective and desired per-case elapsed times within
an execution. `--workload-report` distinguishes invocation frequency from record
volume and embeds the checked native layout. Predictions neither narrow input
acceptance nor change emitted code. A separate [offline experiment tool](workload-experiments.md)
now checks explicit execution variants, measures the weighted objective on
selection/validation data and proposes a reviewable source diff. It preserves
source ceilings and excludes functional differences; a mandatory empty-argv
probe currently excludes mode switches because their stderr differs. Concurrent
lane/batch comparisons are supported, with no automatic source application.
Further clock calibration and performance comparisons are deferred as of
2026-09-24; the recorded workload experiment remains inconclusive.
A batch-processing pipeline, a compiler pass and an HTTP request provide concrete
cases. The contract should describe the execution scope and overlapping storage;
protocol-specific input/output paths supply their own costs and checks.

Within one entry, ordinary lets and checked flat-record calls already pass
values between stages. The [retained-call report](retained-call-storage.md) now
details possible caller buffer capacities live at each call and retained through
return. The enclosing `native_stack` ceiling covers the complete shared frame;
alias names do not duplicate buffers. This exposes existing lifetimes without
changing emitted code or the separate argv selections' independent batch meaning.

## Concrete continuation

The original thought experiment was an LLM producing a binary directly. The
current development direction is a compiler written in Verbose, maintained and
used by LLMs under human direction. Self-compilation and a verifying emission
path already exist; coverage and guarantees continue to evolve. Direct generation
of machine code by an LLM has no assumed timetable and is not a prerequisite.

Specialized native output retains the safeguards chosen for its behavior.
Compile-time reasoning can remove unnecessary work; input-dependent checks remain
at execution. Fast compilation and fast execution are measured separately, with
the relevant program, constraints, and build flags stated.

## Implementation map

| Component | Present scope | Boundary |
|---|---|---|
| Rust-written `verbosec` | Parser, import resolution, verifier, optimizer, three execution/output paths | Experimental implementation; acceptance is not a proof of toolchain correctness |
| Interpreter | Rule evaluation on JSON and supported effects | Useful reference execution, not universal backend parity |
| Native x86-64 | Linux ELF rules and declared services; text/bytes, collections, records, recursive structures, supported effects | Support depends on entry ABI and construct combinations; no general library FFI is established by emitting ELF |
| WebAssembly | Scalar/text rules and supported Result paths | Subset of the language; bytes and aggregate returns are among explicit refusals; native services do not carry over |
| Self-hosted compiler | `examples/vexprparse.verbose` compiles its full self-source and has a verifying emission path | A growing subset with its own checks and restrictions, not a replacement with full `verbosec` parity |
| Generation tools | Prose-to-source with compiler-feedback correction through Claude API/SDK | No multi-model success guarantee; acceptance rate does not measure business correctness |
| Former Rust transpiler | Removed; `--compile` and `--emit-rust` are rejected | Old four-backend descriptions and some CLI descriptive text are stale |

The native backend's `service` path supports declared networking. `resource` and
`connection` declare file reads and outbound fetches. Older notes saying these are
future work describe the pre-service implementation. `--http-server` remains a
legacy rule-plus-shell path; `--demo-http` is a hand-emitted probe without Verbose
source. Use source-declared services to demonstrate the language's effect model.

## Bounded values

Rules can declare `out : text [..N]` to require a statically proved result
capacity in bytes. The analysis follows aliases, branches and acyclic rule
composition; unknown capacities are refused. Native evaluation uses a fixed
invocation region with a separate 2 MiB ceiling: lets evaluate once, aliases
share values, and storage is reclaimed after output or HTTP response consumption.
Output-position calls and branches pass their destination to the final producer,
removing intermediate result buffers and copies. Writable text buffers can also
share storage after their proved last use, including through aliases and branch
joins. Buffers created in mutually exclusive `if` arms can also overlap; their
region remains protected through subsequent alias uses. This placement is used
only when it shrinks the frame, without adding runtime instructions or copies.
Values that can be live together remain distinct;
[scalar/pointer/length slots](bounded-text-slots.md) also reuse space after their
last emitted use, with no extra instructions. Reserved space, actually touched
memory and measured cache behavior are separate quantities.
Pure rules can also pass explicitly constructed or returned flat records between
different input concepts. [Input transfer checks](bounded-text-inputs.md) prove
field capacities and numeric intervals; fields evaluate once and their owners
remain live through callee and caller uses.
[Conditional records](bounded-text-branches.md) can select complete values of
the same concept; field bounds cover both alternatives and only the selected
branch executes. Their text fields retain their owners without a join-time copy.
Sequential HTTP services can now copy a complete annotated text call into an
existing bounded state field. The service keeps its own buffer; the invocation
region is released after copying. See [persistent text copies](bounded-text-state.md).
Pure bounded HTTP handlers can also expose their completed response to
[checked service logs](bounded-text-logs.md). Log scope and content capacities
are verified, and response storage stays live through logging and sending.
This does not bound process memory or establish general ownership across effects
or threads.
See [bounded text results](bounded-text-output.md) and
[native storage](bounded-text-storage.md) for the limits and backend matrix.

`try_byte_at` returns `Result(number, BoundsError)` with explicit handling or
propagation checked across the supported acyclic numeric-input rules. The
interpreter and native argv path support it; WASM and the self-hosted compiler
refuse it. See the [contract and support matrix](try-byte-at.md). This is separate
from service failure recovery and does not generalize the old Result contract.

HTTP services can opt in to bounded request assembly and complete response writes
with paired `request_timeout` / `response_timeout` declarations. Native Linux
x86-64 enforces whole-phase socket deadlines and closes failed clients; WASM and
the self-hosted compiler refuse this service contract. See the
[contract and support matrix](http-bounded-io.md). It retains one request per
connection and existing sequential/forked modes. Forked HTTP services can also
declare `max_connections` to cap admitted handler children, close overload before
request effects, and recover slots after child exits. See the
[admission contract and support matrix](bounded-service-concurrency.md).
Alternatively, `concurrency: pooled` with `workers: N` reuses N isolated worker
processes and their request frames. Busy workers leave clients in the kernel
queue; a worker death terminates the pool. See the
[pool contract and support matrix](pooled-http-workers.md). A pool may also declare
`shutdown_timeout` to finish accepted requests after supervisor SIGTERM, then
force termination at its deadline; see [shutdown and failure boundaries](http-pool-shutdown.md).
Threads, TLS, automatic worker replacement, and listener handoff remain separate.

## Guarantees and measurements

- Reads/calls consistency, types, layers, source references, and supported resource
  restrictions have mechanical checks. Known rule-call argument types are compared
  with declared inputs, including calls nested in reductions and matches; unknown
  local binder types remain outside this check. See [proof classification](spec-proofs.md).
- HTTP service record callees reject potentially failing `byte_at`, `substring`,
  and `parse_int` checks until callee-to-handler error propagation exists. A
  literal byte access with a constant valid index remains supported. Move a
  runtime check to the handler's constructor argument to use client-only abort.
- `termination.bound` counts expression structure, not total runtime work.
  Recursion checks are separate.
- [Numeric overflow contracts](numeric-overflow.md) now require known intervals
  and safe intermediate arithmetic across their supported pure acyclic call graph.
  Unknown analysis is refused; unbounded numeric inputs use the full i64 domain.
  [Numeric branch guards](numeric-guards.md) use explicit scalar/literal comparisons
  to justify arithmetic within selected `if` arms. Facts stay local; eager
  calculations and callees retain their independent obligations.
  Numeric facts can retain two separate intervals, allowing explicit nonzero
  guards and nonzero computed values. A fixed precision budget keeps the analysis
  bounded; no interval-set representation is added to native program values.
  Direct scalar comparisons now transfer checked operand bounds within a branch,
  including configured ceilings and equality with a nonzero result. This is one
  source-order pass with fixed precision, without a general relation solver.
  Interpreter/native argv entries enforce the premises. WASM/self-hosted emission
  refuses the contract until it can provide those guarantees.
  After verification, native emission precomputes constants, removes proved
  impossible branches and reuses dead scalar scratch. It also uses branch-local
  bounds to remove redundant nested tests and substitute singleton scalars.
  It preserves up to two intervals through numeric operations and calls, so
  proved nonzero values can eliminate zero tests even across both signs.
  [Repeated numeric reads](numeric-identities.md) now establish cancellation,
  nonnegative squares and nonzero self-division within the same fixed domain
  budget. This recognizes the same field/local, not arbitrary equal expressions.
  Original entry guards and source obligations remain; see
  [numeric optimization](numeric-optimization.md).
  [Numeric local lifetimes](numeric-local-lifetimes.md) also allow slots to be
  reused after their last enclosing expression, with no runtime lifetime tracking.
  Expression scratch and expanded callees can reuse those dead slots even below
  still-live locals; caller operands remain protected throughout evaluation.
  [Direct operand reads](numeric-operands.md) avoid copying immutable numeric
  fields/locals into scratch when their existing slot remains valid.
  The [numeric benchmark](numeric-benchmark.md) separates elapsed and child CPU
  time and marks inconsistent clock accounting inconclusive. Frame savings are
  established; the recorded timings do not settle small runtime-cost differences.
- Source-to-binary semantic equivalence is not independently proved by the x86
  instruction decoder. Compiler and optimizer correctness remain trusted.
- The bootstrap checks `gen1 == gen2` for the self-source, plus refusal and
  execution cases. It does not prove correctness for every accepted program.
- Binary sizes and performance results are measurements for specific programs,
  flags, and revisions. Use the dated [benchmark report](benchmarks.md) and rerun
  its commands for a new checkout. The [HTTP worker baseline](http-worker-benchmarks.md)
  separately measures forked/pool goodput and pool memory reuse.

## Immutable artifact, changing inputs

Compiled logic and declared capability structure are fixed in the executable.
A threshold or allowlist loaded from a declared file is runtime data and can affect
policy decisions without changing the binary. Service resources with `cache: true`
load at startup; uncached resources can observe later changes. Explicit service
state is another source of changing behavior. A source-level change to logic or
capability declarations requires a new compilation.

## Verification commands

Run the normal suite serially: native tests share temporary executable paths.
Network tests need permission to bind local sockets.

```sh
cargo test -- --test-threads=1
cargo run -- examples/invoices.verbose
cargo run -- examples/audit_gateway.verbose
```

The separate bootstrap suite includes ignored tests and needs more time, memory,
and stack than routine checks. CI runs it in a dedicated job:

```sh
ulimit -s unlimited
cargo test --release -- --ignored --test-threads=1 two_generation
```

These are reproduction commands, not a claim that every suite was run for this
documentation revision. Test totals change with the checkout; use the actual run
summary rather than historical counts.

## Reading order and maintenance

The [bounded-error and failure-boundary proposal](error-boundaries-design.md)
is a design for discussion, not an implemented general error contract. It builds
on `Result` and the scoped service recovery while keeping returned errors,
boundary termination, and partial effects distinct.

1. [README](../README.md): purpose, examples, and design direction.
2. [Architecture](../ARCHITECTURE.md): implementation map and trust boundaries.
3. [Proof classification](spec-proofs.md): individual declaration semantics.
4. [Examples](../examples/README.md): concrete syntax and entry points.
5. [Self-hosting](self-hosting.md), [known-gap history](known-gaps.md), and
   [vision journal](vision-journal.md): milestones and reasoning over time.

When implementation scope changes, update this page and the relevant reference.
Keep dated evidence in the journals; label superseded claims instead of silently
rewriting a past experiment. Code and its executable tests decide behavior if a
reference has drifted. A `DESIGN` document alone does not establish implementation.
