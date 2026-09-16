# Verbose — Architecture

Verbose explores what a language can require authors to make explicit when LLMs
can produce the source: dependencies, effects, bounds, and optimization intent.
Humans can also write it directly. The compiler checks the supported declarations
and uses the program's structure to execute it or generate a specialized artifact.

This is a map of the current implementation. Read [current status](docs/current-status.md)
for backend boundaries and [proof classification](docs/spec-proofs.md) for the
meaning of individual declarations. Dated design documents preserve earlier stages.

## Source and generation

```text
.intent (optional prose) ── human or LLM authorship ── .verbose
                                                       │
                                    lexer → parser → import resolution
                                                       │
                                                    verifier
                                                       │
                                                    optimizer
                                                       │
                                    ┌──────────────────┼──────────────────┐
                                    ▼                  ▼                  ▼
                                interpreter       native x86-64          WASM
```

The bundled generators in `tools/generate.py` and `tools/generate_sdk.py` use
Claude and a bounded correction loop driven by compiler diagnostics. The `.verbose`
format and compiler have no model-provider dependency. Other authors can submit
source through the same interface. Successful generation by every LLM is an
ambition to evaluate, not an established property of the language.

The compiler checks referenced source lines exist; it does not decide whether the
program faithfully expresses their prose. That comparison belongs to the author
and auditor.

## Compiler stages

| Stage | Implementation | Responsibility |
|---|---|---|
| Lexer | [src/lexer.rs](src/lexer.rs) | Recognizes tokens and indentation boundaries |
| Parser and AST | [src/parser.rs](src/parser.rs), [src/ast.rs](src/ast.rs) | Parses declarations and expressions into structured nodes |
| Import resolution | [src/main.rs](src/main.rs) | Loads `use` declarations and merges the program |
| Verifier | [src/verifier.rs](src/verifier.rs) | Checks types, reads/calls, source references, layers, termination declarations, and supported effect/resource restrictions |
| Optimizer | [src/optimizer.rs](src/optimizer.rs) | Transforms the AST using constant folding, range information, and supported simplifications |
| Execution/emission | [src/interpreter.rs](src/interpreter.rs), [src/native.rs](src/native.rs), [src/wasm.rs](src/wasm.rs) | Executes or lowers the accepted program on a supported path |

Parsing creates structurally typed AST nodes; semantic type checking happens in
the verifier. Indentation delimits declaration blocks, while braces also appear
in expressions such as record construction.

Verification precedes optimization in the main pipeline. A successful verifier
result does not independently certify the optimizer's or backend's implementation.
Unsupported backend shapes can still be rejected after source verification.

## Declarations and effects

Rules express computations with declared dependencies. Pure arithmetic rules are
one case; rules can also reference declared resources, connections, or entropy.
The historical `purity` block name must not be read as a claim that every rule is
independent of external state.

| Construct | Role |
|---|---|
| `concept`, `concept_group` | Data layout, field types/ranges, and bounded recursive structures |
| `rule` | Inputs, output, logic, dependencies, and termination declarations |
| `reaction` | Effects attached to a trigger rule |
| `resource` | Declared file input with a literal path and size bound |
| `connection` | Declared outbound connection and response bound |
| `entropy` | Declared random-byte source |
| `service` | Protocol, listener, handler, and optional logging/concurrency/state |

The native backend implements the machinery behind these constructs: parsing,
buffers, syscalls, and calling conventions. Those implementation details are part
of the trusted compiler. Declaring a protocol does not constitute a formal proof
of its machine-code implementation.

The executable fixes its logic and declared capability structure. Resource
contents, requests, and declared service state can still change its decisions.
For service resources, `cache: true` loads at startup; uncached reads can observe
later file updates. See the [effect model](docs/effect-model.md) for its design
history and [examples](examples/README.md) for concrete declarations.

## What the checks establish

### Memory as a language design criterion

Recorded on 2026-09-10: evaluate new service capabilities by whether their storage
has an identifiable owner, a capacity, a lifetime, and a defined outcome when
that capacity is exhausted. Declarations should let the compiler organize and
check that storage; unknown bounds must remain explicitly unknown. Apache is a
useful reference for operational capabilities, not a feature-cloning objective
or a prerequisite for Verbose to make useful design choices.

Memory efficiency is also an objective: minimize live storage, unnecessary
copies and the working set to support processor-cache locality. Capacity bounds
are ceilings, not targets to fill. The compiler should exploit verified
lifetimes and exclusive execution to reuse storage without runtime management.
This does not guarantee cache residency: reserved bytes, touched bytes, RSS and
hardware cache misses must be distinguished in measurements. No particular
cache size becomes a language contract.

This is already concrete for bounded HTTP reception: `max_request` determines a
fixed frame buffer, receiving does not grow it, and oversize requests close the
client. It is not yet a whole-service memory bound: response temporaries, callees,
resources, kernel buffers, and process overhead have separate lifetimes and costs.
Reclaiming a request region means making its storage reusable, not erasing its
bytes or releasing every page to the OS. Native emission alone establishes no
performance or safety advantage over another native server.

At rule boundaries, a text output annotated `[..N]` now requires a static proof
of its byte capacity, including pure acyclic composition. See
[bounded text results](docs/bounded-text-output.md). This makes value capacity
explicit independently of services. Its checked native subset now reserves
[invocation-owned text storage](docs/bounded-text-storage.md), with evaluated
lets, shared aliases and a separate ceiling on slots and temporary buffers.
Output-position calls and branches forward a fresh result destination to the
producer; other live values retain distinct storage. Text buffers can share
space after their proved last use, including through aliases and branch joins.
Buffers created in opposite `if` arms can overlap while their region remains
protected through all subsequent alias uses. The compiler keeps that placement
only when it reduces the reserved frame compared with ordinary last-use reuse.
Placement happens at compilation; scalar/pointer/length slots are not reused.
Rules can [construct checked record inputs](docs/bounded-text-inputs.md) for
callees with different concepts. The compiler proves field capacities and numeric
intervals at that boundary, evaluates fields once, and retains their storage
through callee and caller uses.
[Conditional records](docs/bounded-text-branches.md) join capacities and numeric
intervals field by field. Native selection moves text descriptors while keeping
their possible owners alive through subsequent uses.
A sequential HTTP service can also [copy a bounded rule result into its own text
state](docs/bounded-text-state.md). The compiler checks the destination capacity
and releases temporary storage after copying. General ownership across resources
or threads remains a separate contract.

[Service logs](docs/bounded-text-logs.md) can borrow a pure bounded handler's
completed response. Their separate checked formatting buffers live below the
response region and are reclaimed after each write; the response remains live
through sending. This adds a specific synchronous effect boundary.

Worker reuse must preserve request-local lifetimes and demonstrate stable storage
across requests and failure paths. Shared mutable memory between threads needs an
explicit ownership/synchronization contract. See the
[pooled HTTP design](docs/pooled-http-workers.md) for the next implementation slice.

### Existing checks

`reads` and `calls` are compared against dependencies collected from the AST.
`@layer` constrains the call graph. `@source` checks that a referenced file and
line exist, without checking the prose's meaning.

`termination.bound` is checked against a structural operation count. For example,
a call contributes a node plus its arguments; a fold contributes its expression
structure without multiplying by the number of iterations. Recursion has separate
structural/decreasing/increasing checks. The structural count is not a total
runtime instruction, latency, or service-lifetime budget.

Range analysis computes intervals for supported expressions. The current hint
checker accepts an unknown interval without establishing the overflow hint.
Acceptance therefore does not mean every annotation has been proved. The precise
boundary and a recorded range-analysis counterexample are in
[proof classification](docs/spec-proofs.md).

## Execution and output paths

**Interpreter.** Evaluates rules on JSON input and provides an execution path for
many language constructs. It is also useful as an oracle for differential tests;
it is not a claim of universal feature parity or an independently proved oracle.

**Native x86-64.** Emits Linux ELF executables directly, using syscalls without
linking libc. Rule entry points support several input/output conventions; services
have their own entry points. Scalar arithmetic, aggregates, collections, recursive
arenas, text/bytes, and services use specialized lowering paths. Support for a
construct in one path does not imply support in every combination.

**WebAssembly.** Emits modules with a host-facing ABI. Numeric values use scalar
parameters; text uses pointer/length pairs in linear memory. The implementation
supports a subset of rule expressions and outputs. Native services and syscalls
are not automatically portable to this backend.

The former Rust source transpiler was removed. The CLI rejects `--compile` and
`--emit-rust`; older transcripts mentioning four backends describe that earlier
implementation. The compiler itself is still written in Rust.

## Instruction validation and the trust boundary

[src/validate_x86.rs](src/validate_x86.rs) decodes supported instruction structures
to help catch emission mistakes. It is incomplete and does not prove equivalence
between the source and the binary, constrain all possible effects, or certify
arbitrary machine code supplied by an LLM.

The lexer, parser, resolver, verifier, optimizer, backend, and execution environment
remain part of the trust chain. Tests exercise their behavior, including negative
programs and comparisons between execution paths.

## The compiler written in Verbose

This is the project's concrete continuation: LLMs can author, maintain, and use
the compiler in the same language as its applications, under human direction.
Direct LLM generation of binary instructions is the originating thought
experiment, not a dependency of this work.

[examples/vexprparse.verbose](examples/vexprparse.verbose) contains a tokenizer,
parser, analyses/verifier, interpreter, and native emitter written in Verbose.
Its supported language subset includes its entire own source.

```text
Rust-written verbosec + self-source → gen0 executable
                       gen0 + self-source → gen1 executable
                       gen1 + self-source → gen2 executable
                                             gen1 == gen2
```

The dedicated [CI job](.github/workflows/ci.yml) checks this fixed point along with
verification refusals and executable examples. The self-hosted compiler checks
input before emitting on its verifying path. Its support and verification coverage
still differ from the Rust compiler. A reproducible fixed point establishes
bootstrap stability on that source, not universal compiler correctness.

Read [self-hosting](docs/self-hosting.md) as a milestone journal: its sizes,
counts, restrictions, and memory measurements belong to the stages where they
were recorded.
