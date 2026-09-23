# Sequential native stack budgets

Checked numeric and bounded text rules can now run as successive argv phases
inside one Linux x86-64 executable. The existing `--run a,b,c` selection defines
their order. Each phase finishes consuming its results and releases its frame
before the next phase starts, so their additional stack bound is their **maximum**.

This is a first composition case for the language's reusable resource contracts.
A batch tool can perform several analyses of the same input with explicit bounds.
HTTP remains another integration example for future scopes involving retained
state, input/output buffers and concurrent work.

## Example and execution order

The [complete example](../examples/sequential_stack.verbose) gives each rule a
checked `proofs.native_stack` declaration:

| Phase | Result | Additional stack bound |
|---|---|---:|
| `clamp` | Each reading clamped to [-100, 100] | 104 bytes |
| `nonnegative` | Whether each clamped reading is nonnegative | 88 bytes |
| `label` | Original label and reading, at most 29 output bytes | 192 bytes |

```sh
target/release/verbosec examples/sequential_stack.verbose \
  --stack-report --run clamp,nonnegative,label --json
target/release/verbosec examples/sequential_stack.verbose \
  --native /tmp/readings --run clamp,nonnegative,label
/tmp/readings a 2 b 1000
```

This prints `2`, `100`, `true`, `true`, `a:2`, `b:1000`, one per line, and exits 0.
Each phase processes **the entire argv batch**, in record order. Its successor
rereads the original inputs; preceding output is already consumed by stdout.
Changing phase order changes output order. Repeating a name runs that phase again.

The maximum is `max(104, 88, 192) = 192` bytes, including the entry and invocation
frames, input guards, saved registers and formatting scratch covered by the
[single-entry contract](native-stack-budget.md). No extra stack word is needed
for dispatch or for values shared between phases. Initial argv/environment storage
and kernel/output storage retain their existing exclusions.

## Failure behavior

A phase must complete with status 0 before its successor starts. For a boolean
phase, the existing sticky exit policy applies: it prints every valid record's
result, then returns 1 if any result was false. With `a 2 b -1 c 3`, the example
prints the three clamped values and `true`, `false`, `true`, then exits 1 before
`label` starts. Earlier output is not rolled back.

Input-bound violations, malformed numeric input and partial records retain the
selected emitter's existing behavior. A fatal input failure exits immediately;
no later phase executes. Composition does not change which errors each standalone
emitter detects, or make their input policies identical. Runtime output, stderr
and status match running the standalone phase executables as a shell `&&` chain.

The emitter provides a structured phase-completion path. It checks the existing
exit flag, keeps all abort paths terminal, and restores the original stack on
success before the next prologue reads argc/argv. It does not search for and
remove a byte pattern from finished code. Single-entry emission stays byte-identical.

## Source contracts and reports

Every source `native_stack` declaration still describes its rule as an independent
argv entry. Helpers and unused rules are checked too; expanded callee storage is
already included in its caller's phase. An exceeded declaration refuses the
whole compilation before opening the output artifact, including when only a
later phase or an unselected helper exceeds its budget.

The selected sequence has a calculated aggregate bound. A named
[source execution](source-executions.md) can now declare its order, input,
failure policy and aggregate ceiling. Declared ceilings need not equal actual use,
and their sum is not the sequence's memory requirement. Removing sufficient
optional declarations changes neither standalone nor composed native bytes.

A single-name `--stack-report` keeps its existing schema-1 JSON. Multiple names
produce a schema-1 object with:

| Field | Meaning |
|---|---|
| `composition` | `"sequential"` |
| `on_phase_failure` | `"stop"` |
| `retained_stack_bytes` | `0`: no additional stack storage survives a phase |
| `stack_bound_bytes` | Maximum of the phase bounds |
| `phases` | Ordered single-entry report objects, including repeated names |

The target, argv mode and `additional_entry_stack` scope are retained. The human
report gives the aggregate calculation followed by each phase's details.
Branches that cannot reach a later phase can make actual use smaller: this is a
conservative upper bound, not a minimum or runtime measurement.

## Scope and validation

All selected phases must belong to the supported strict numeric or bounded text
subsets and use the **same input concept**. Different input variable names are
allowed. Numeric, boolean, bounded text and supported flat record outputs can
mix; their results are not passed to the next phase. An unknown phase layout or
an unchecked phase mixed into the selection causes a named diagnostic.

The checked composition supports 2 through 64 phases, with a compiler limit of
64 MiB of combined code. Stdin/raw/stream, services, reactions, cross-phase state,
concurrent execution and passing results between phases need separate composition
rules. WASM and self-hosted emission keep their current capability refusals.
Existing unchecked multi-rule emission retains its own behavior and restrictions.

To pass a prepared value to its consumer within one entry, use ordinary lets,
records and calls. The enclosing rule's budget covers their overlapping storage;
the [retained-call report](retained-call-storage.md) explains which caller buffer
capacities survive each expanded call. These details also appear inside each
selected phase's report. Independent batch execution keeps its existing meaning.

Rust tests compare reports with independent instruction-level stack tracking
through nested frame restoration, branches and record loops. They cover repeated
phases, exact/minus-one budgets, the 64-phase limit, record/text/numeric consumers,
unknown layouts and preservation of existing artifacts on refusal. Execution is
compared with the standalone `&&` oracle, including failed boolean phases, input
errors, UTF-8 and i64 extremes. CLI tests also compare successful phase values
with the interpreter. This slice introduces no allocator or GC and makes no
claim about RSS, cache residency or CPU speed.
