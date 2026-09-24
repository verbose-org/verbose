# Retained values in checked rule composition

Values can already pass between pure bounded text rules through ordinary lets,
flat records and checked calls. This slice exposes their storage lifetimes at
each expanded call in `--stack-report`. The enclosing rule's `native_stack`
budget already checks the complete composition, including values retained by
its caller. The new explanation comes from those same buffer owners and last
uses; it introduces no source syntax or runtime storage mechanism.

## Source-level stages

The [complete example](../examples/retained_stack.verbose) prepares a record,
forwards it through a helper, renders it and reuses its original title afterward:

```verbose
let staged = prepare(input)
let preserved = forward(staged)
let staged = Prepared { title: "shadow", code: 0 }
let rendered = render(preserved)
out = concat(rendered, " | ", preserved.title)
```

The original input concept has an eight-byte title and an unrestricted number.
`prepare` produces `Prepared`, with a ten-byte title and a code in [-1, 1].
The existing [transfer checks](bounded-text-inputs.md) verify supplied fields
against every consumer's declared input. A nominal concept name or an alias
cannot replace the capacity/range proof. Each callee is checked against its
public input contract, independently of the particular call site.

`forward` returns the already computed record. Its title keeps the same immutable
buffer; the later shadowing does not change `preserved`. The title survives the
entire `render` call because `analyze` consumes it again afterward. All stages
share the enclosing invocation's storage; only the completed `analyze` result
is printed for each record.

```sh
target/release/verbosec examples/retained_stack.verbose --stack-report --json
target/release/verbosec examples/retained_stack.verbose --native /tmp/analyze
/tmp/analyze café 42 sample -42
```

Output:

```text
[café]:1 | [café]
[sample]:-1 | [sample]
```

The source retains its `native_stack: 408` declaration on `analyze`. With
[word-slot reuse](bounded-text-slots.md), its emitted layout now needs 288 bytes:

| Storage | Bytes |
|---|---:|
| Outer input/bookkeeping frame and saved base pointer | 64 |
| Inner scalar/pointer/length slots | 88 |
| Placed writable buffers | 96 |
| Saved inner registers | 16 |
| Peak numeric-formatting scratch | 24 |
| **Additional entry stack bound** | **288** |

The buffers are the final destination (44 bytes rounded to 48), prepared title
(10 rounded to 16) and rendered text (31 rounded to 32). Passing or renaming the
record adds no second title buffer. The original 408-byte declaration still
passes; an exact 288-byte declaration passes and 287 refuses
before artifact creation. Removing a sufficient declaration changes no native
bytes. Standalone helper entry budgets are not added to the caller's budget:
expanded helpers use the shared invocation layout.

## Reading call lifetimes

For bounded text entries containing expanded calls, schema-1 JSON adds
`text_frame.calls`. Numeric reports and text reports without calls retain their
previous shape. Each call object contains:

| Field | Meaning |
|---|---|
| `call` | One-based identifier in emitted call-entry order |
| `callee` | Source rule name |
| `parent_call` | Active containing call identifier, or `null` for the entry rule |
| `live_caller_buffer_capacity_bytes` | Sum of aligned capacities of possible pre-existing owners whose last use reaches this call's entry |
| `retained_caller_buffer_capacity_bytes` | The subset of those owners whose last use reaches this call's return |

Arguments evaluate before the call-entry boundary, so a call inside an argument
appears before the call receiving it. Calls expanded inside a callee carry its
identifier as their parent. Repeated call sites remain separate. Both arms of a
conditional are described, although execution selects only one.

For `analyze`:

| Call | Live at entry | Retained through return |
|---|---:|---:|
| `prepare` | 48 | 48 |
| `forward` | 64 | 64 |
| `render` | 64 | 64 |

These numbers include reserved destinations that have not yet been filled: the
48-byte final destination is protected throughout the composition. The extra
16 bytes are the already prepared title. Removing its use after `render` lowers
that call's retained capacity to 48, while its live-at-entry capacity stays 64.
The callee's own newly created buffers are not pre-existing caller owners; they
remain included in the complete frame. Descriptor and numeric slots are also
included there, with physical word slots reused after their last emitted use.

These are **possible owner capacities, not additional allocations or measured
live bytes**. Aliases of one owner count once. Owners from exclusive branches
count separately even when placement overlays their addresses. Consequently a
call's capacity sum can exceed `buffer_bytes`; it must not be added to the frame,
to another call, or used to recompute the entry bound. Use `stack_bound_bytes`
for the actual conservative placement bound. Literals and borrowed initial argv
payloads have no writable invocation buffer to count; their descriptor slots
still count in the frame.

The compiler propagates last uses through aliases and conditional joins before
calculating summaries. A reservation sweep and prefix sums avoid scanning every
buffer for every call: for B buffers and C calls, the extra analysis uses
O((B+C) log(B+2)) time and O(B+C) memory. It does not enumerate execution paths.
No instructions, payload copies, allocator, lifetime tables or GC are added to
the emitted program.

## Scope and verification

The existing native argv contract remains the boundary: pure acyclic bounded
text composition, flat number/text records, checked capacities and input ranges.
Unknown layouts, effects, recursion, nested records and unsupported input modes
retain their diagnostics. WASM and self-hosted output still refuse these source
contracts. Interpreter values provide a behavioral oracle; its allocations are
outside the native budget.

[Sequential argv selections](sequential-stack-budgets.md) still run complete,
independent batches and retain no values between selected entries. An entry may
itself use the source-level composition above; its call details appear in that
phase's report. A [pipeline execution](pipeline-executions.md) now declares an
ordered composition of these calls, passing each result to the next stage and
publishing only the final value under one shared invocation ceiling. Persistent
state across invocations and concurrent transfer need separate contracts. HTTP handlers
and compiler/data-processing stages remain examples for those future contracts.

Tests compare the call summary with an independent owner scan, exercise nested
calls, aliases, shadowing, dead values and overlaid alternatives, and compare
the total bound with emitted instruction stack accounting. CLI tests compare
interpreted and native values at UTF-8 and i64 boundaries and check refusal
without overwriting existing artifacts. This slice explains and pins existing
transfer/storage semantics; it claims no new throughput, RSS or cache result.
