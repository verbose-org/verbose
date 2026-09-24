# Source-declared sequential executions

An `execution` names a complete native argv entry: its input concept, ordered
phases, failure policy and aggregate stack ceiling are part of the source.
An author or LLM can therefore specify and audit the execution order without
having to recover it from `--run a,b,c`. The compiler checks that declaration
against the same lowering used by [sequential argv phases](sequential-stack-budgets.md).

## Declaration

The [example](../examples/execution_stack.verbose) imports the existing reading
rules and declares:

```verbose
execution inspect_readings
  @intention: "Run the declared analyses in order within one checked entry budget"
  @source: execution_stack.intent:1
  input: Reading
  mode: sequential
  phases: [clamp, nonnegative, label]
  on_failure: stop
  native_stack: 192
```

All seven fields/attributes are required and may occur once. Unknown fields,
duplicate fields and unsupported mode/policy values refuse. This page describes
`mode: sequential` with `on_failure: stop`. The separate
[concurrent reference contract](concurrent-executions.md) requires `max_in_flight`
instead of `native_stack`, plus an optional `native_memory` ceiling required for
[native concurrency](native-concurrent-executions.md). Optional
[`result_batch`](concurrent-result-batches.md) bounds pending results per worker.
Sequential mode refuses all three concurrent resource fields. `@source` must resolve
to an existing intention line; imported executions have their source references
rewritten like imported rules. An intention must be nonempty.

An execution name cannot collide with another declaration or primitive. Its
phases are 2 through 64 rule names, all with the declared input concept.
Repeated names are allowed and preserve their positions. Executions are entry
selections, not callable rules; nested executions, services and reactions cannot
be phases. `execution` remains an ordinary identifier outside declaration position.

## Runtime meaning

```sh
target/release/verbosec examples/execution_stack.verbose --stack-report --json
target/release/verbosec examples/execution_stack.verbose --native /tmp/readings
/tmp/readings a 2 b 1000
```

The executable prints:

```text
2
100
true
true
a:2
b:1000
```

Each phase processes the **whole original argv batch** in record order.
`nonnegative` runs only after `clamp` completes successfully; `label` then reads
the original values, including 1000. Results are consumed by stdout and do not
become the next phase's input. To transfer a prepared record within one phase,
use ordinary checked lets and calls, as in [retained call storage](retained-call-storage.md).

A nonzero phase status stops the execution before the next phase. The existing
boolean policy prints all valid records of its phase, then returns 1 if any
result was false. Fatal input errors stop immediately. Earlier output remains
visible; no rollback or recovery is implied. Runtime behavior matches the
corresponding standalone executables joined by shell `&&`.

`--run inspect_readings` selects the declared entry explicitly. Without `--run`,
native compilation selects the last declared service or execution, otherwise
the last rule. Files without executions keep their old default. `--stack-report`
defaults to the last execution, otherwise the last rule. Existing explicit rule
and comma-separated selections remain available as other entry selections;
an execution name cannot be mixed into a comma-separated list.

## Interpreter reference

The same declaration can run through the interpreter on JSON records:

```sh
target/release/verbosec examples/execution_stack.verbose \
  --run inspect_readings --input examples/execution_stack.json
cat examples/execution_stack.json | target/release/verbosec \
  examples/execution_stack.verbose --run inspect_readings --stdin --json
```

This path evaluates the **original verified AST, before optimization**. Every
phase sees the same original records, in order. A false result is printed and
remembered until the boolean phase finishes; later phases are skipped and the
exit status is 1. Evaluation/input failures stop at the failing record, with
the completed output prefix preserved. An empty batch fails like native argv
entry. The interpreter supplies contextual diagnostics on stderr for runtime
errors; native input failures currently return status 1 without those messages.

Default output matches native number, boolean, text and supported flat-record
rendering, without a compiler banner. Native record rendering writes raw text
between quotes and does not escape it: this form is not valid JSON for arbitrary
text. The interpreter's `--json` mode instead emits an array of escaped events:

```json
[{"phase":1,"rule":"clamp","record":0,"value":2}]
```

`phase` is one-based and `record` is zero-based. Repeated phases retain separate
indices. Values preserve their number/bool/text/record types. Events stream in
phase/record order; completed values are not retained by the runner. On a runtime
failure the array closes around the completed prefix (provided stdout remains
writable); exit status and stderr describe the failure. Boolean false alone adds
no stderr diagnostic. An error before the first result produces `[]` in JSON
mode. Failed source verification or malformed input produces no stdout.

Execution input is a complete JSON array of flat objects. Strings support JSON
escapes and Unicode surrogate pairs; numbers must be decimal i64 integers.
Duplicate fields, malformed syntax, floats, null and nested values refuse during
decoding, before any phase runs. Boolean input values decode but fail the numeric
or text field contract when that record is reached. Missing fields, wrong types,
numeric ranges and text UTF-8 byte capacities are checked immediately before each
record evaluation; additional keys are unused. JSON syntax is decoded for the
whole document first, unlike the native executable's argv parser. Differential
parity concerns equivalent decoded records; embedded NUL input text cannot be
passed through argv, and malformed JSON has no argv counterpart.

`--stdin` reads directly without a shared temporary file. Choose either `--input`
or `--stdin`; raw/stream input, benchmark/stats/disassembly modes do not apply to
this interpreter entry. Existing individual-rule interpretation, input acceptance
and labelled/JSON output retain their behavior, including their boolean status
policy. No native instructions or self-hosted source change for this support.

The same source/native contract is verified even for interpreted executions,
including unselected budgets and unsupported phases. `native_stack` continues
to describe emitted native storage, **not interpreter memory**. The interpreter
retains the input batch and uses host allocations for values; it is a behavioral
reference, not a bounded-memory execution backend.

## Budget and verification

`native_stack` is required on the execution and accepts 1 through 2,097,152 bytes.
Its scope is **additional Linux x86-64 native argv entry stack**, with the same
inclusions and exclusions as [rule stack budgets](native-stack-budget.md).
It covers input/bookkeeping frames, placed buffers and reusable words, saved
registers and peak transient scratch. Initial argv/environment storage, OS stack
mappings, kernel memory, compiler/interpreter allocations and process RSS are
outside this contract.

The phases release their frames before their successors start. Consequently
`inspect_readings` needs `max(104, 88, 192) = 192` bytes. An exact declaration
passes and 191 refuses before an artifact is created or overwritten. The
compiler checks **every** execution declaration, including ones not selected,
and retains the independent checks on every rule's optional `proofs.native_stack`.
Declared limits are not added together. A rule's limit keeps its standalone
entry meaning; an execution's limit covers its selected sequence.

Original source verification precedes optimization and layout. Each phase must
already belong to the supported strict numeric or bounded text subset. An
execution does not make legacy arithmetic strict, infer missing contracts, or
accept an unknown layout. Unsupported effects, recursion, record shapes or
input modes retain the underlying lowering's refusals. Combined code retains
the existing 64 MiB compiler limit. This bound is a conservative layout result,
not a runtime measurement or a proof of arbitrary machine code.

The declaration changes no emitted instructions, copies, allocation or GC:
selecting it emits the same bytes as selecting its phases explicitly.
The metadata and checks run in the compiler. Compiler layout changes can change
the required bound, so a source budget should be rechecked on rebuild.

## Report and backend support

The schema-1 sequential JSON report retains its existing fields and ordered
`phases`. Selecting an execution adds `execution`, `input_concept` and
`declared_bytes` at the root. `stack_bound_bytes` is the calculated maximum;
`retained_stack_bytes` remains zero and `on_phase_failure` is `"stop"`.
Reports for ordinary rules and explicit comma-separated selections are unchanged.

| Path | Support |
|---|---|
| Rust parser/verifier and `--stack-report` | Source, phase input types, supported lowering and all declared budgets checked |
| Native argv | Source execution selection and existing structured sequential exits |
| Native stdin/raw/stream, legacy HTTP shell | Execution entry refused before artifact creation |
| Services/reactions or calls selecting an execution as a rule | Refused; executions are not handlers or callees |
| Interpreter | Original-AST execution on JSON records; native-style output or typed phase/record events; host memory outside the native budget |
| WASM | Programs declaring executions explicitly refused before artifact creation |
| Self-hosted diagnostics / raw x86 / ELF | Top-level execution declarations explicitly refused before output; ordinary identifiers, literals and comments remain controls |

This page establishes a named scope for sequential work. The separate
[concurrent contract](concurrent-executions.md) now runs in the interpreter and
[native threads with a checked reservation](native-concurrent-executions.md).
Cross-phase retained values and whole-service memory need further
lifetime/transport contracts. Batch processing
and compiler passes are applications of this source contract; HTTP remains an
integration case for later transport-aware execution scopes.

## Validation

Rust tests cover required/duplicate/unknown fields, names, input concepts,
2/64/65 phase boundaries, order/repetition, unselected invalid budgets, exact
limits, backend refusals and artifact preservation. Declared executions must
produce byte-identical output to existing explicit phase selections; execution
tests compare stdout, stderr and status at UTF-8/i64 boundaries, false results
and malformed input. Existing sequence tests independently track machine-code
stack depth. CLI tests also cover default selection and imported source references.

The self-hosted refusal tests omit the older overflow/text/stack annotations,
so those gates cannot hide a missing execution gate. They exercise diagnostics,
raw and ELF output, including declaration placement and accepted identifier
controls. Run `cargo test -- --test-threads=1`, `python3 tools/test_stack_budget.py`
and the release `two_generation` bootstrap serially before delivery.

Interpreter differentials compare the original AST with optimized native output
for i64 extremes, UTF-8, control/delimiter text, repeated phases, flat records,
aliases, shadowing and sticky boolean failure. CLI tests cover strict JSON,
stdin, partial output on type/capacity failures, JSON event order, source refusal
before input access, and unchanged standalone rule behavior. Writer failures
stop further evaluation. No throughput, cache or interpreter-memory bound is
claimed by these checks.
