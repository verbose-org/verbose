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

For service evolution, the [memory design criterion](../ARCHITECTURE.md#memory-as-a-language-design-criterion)
asks who owns storage, its capacity and lifetime, and what happens at exhaustion.
The aim is useful, predictable specialization; matching Apache's feature list is
not the project's identity or a claim of general superiority.

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
joins. Simultaneously live values remain distinct; scalar/pointer/length slots
are not reused.
This does not bound process memory or establish ownership across effects/state.
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
- Overflow hints are checked when an interval can be computed. An unknown interval
  is currently accepted without establishing the hint. The signed-modulo interval
  defect is fixed in the Rust verifier; see the regression and remaining limits
  in the proof document.
- Source-to-binary semantic equivalence is not independently proved by the x86
  instruction decoder. Compiler and optimizer correctness remain trusted.
- The bootstrap checks `gen1 == gen2` for the self-source, plus refusal and
  execution cases. It does not prove correctness for every accepted program.
- Binary sizes and performance results are measurements for specific programs,
  flags, and revisions. Use the dated [benchmark report](benchmarks.md) and rerun
  its commands for a new checkout.

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
