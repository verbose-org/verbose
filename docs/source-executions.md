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
duplicate fields and unsupported mode/policy values refuse. The first slice
supports only `mode: sequential` and `on_failure: stop`. `@source` must resolve
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
| Interpreter | Execution selection explicitly refused; individual rules remain interpretable |
| WASM | Programs declaring executions explicitly refused before artifact creation |
| Self-hosted diagnostics / raw x86 / ELF | Top-level execution declarations explicitly refused before output; ordinary identifiers, literals and comments remain controls |

This slice establishes a named scope for sequential work. It adds no concurrency,
cross-phase retained values, new input protocol or whole-service memory budget.
Those need their own admission, lifetime and failure semantics. Batch processing
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
