# Conditional records in bounded text rules

Implemented 2026-09-15. Rules participating in the
[bounded text contract](bounded-text-output.md) can choose between flat records
of the same concept, then retain, return or pass the selected value to another
rule. This reuses `if`, concepts and lets; it adds no syntax.

```verbose
let selected = if request.code > 0 then FormatInput { title: concat("[", request.title, "]"), code: 1 } else FormatInput { code: -1, title: concat("<", request.title, ">") }
let saved = selected
let selected = FormatInput { title: "shadow", code: 0 }
out = render_text(saved)
```

The complete [example](../examples/bounded_text_branches.verbose) declares
`title : text [..10]` and `code : number [-1, 1]` in `FormatInput`. Both
alternatives meet that input contract. The second constructor deliberately
writes its fields in a different order.

## Checking the choice

Both branches must have the same nominal concept. Each constructor provides
each declared field exactly once with the correct type. Fields correspond by
name, independently of their written order. The analysis joins each field:

- A text capacity is the maximum of both capacities; either unknown capacity
  makes the joined capacity unknown.
- A numeric interval covers both intervals, preserving negative values and
  the i64 extrema.
- Boolean fields retain their type and can be used in later conditions.

These facts follow aliases, nested conditions and record-returning calls. The
[existing call transfer check](bounded-text-inputs.md) still proves that every
supplied field fits the callee's declared input. Rule inputs remain flat
concepts of numbers and text. The caller cannot use a narrower incidental value
to weaken the callee's public contract.

Both branches are checked even when the condition is constant. The analysis
does not infer correlations between fields or conditions: checking `code` again
does not narrow `title` back to one branch's capacity. Record construction and
output retain their existing range policy; this is not global enforcement of
all record-field ranges.

An original input can be one or both alternatives. A conditional record exposes
all its fields to the join, so native compilation requires a known capacity for
every text field, including unused fields. Direct forwarding of the original
input retains its previous acceptance of unused unbounded fields. Synthesized
field joins count toward the existing 100,000-visit analysis budget, preventing
large input aliases from hiding unbounded analysis work behind small expressions.

## Evaluation and storage

The condition evaluates once. Only the selected branch runs; its constructor
fields evaluate once in source order, including unused fields. Lets surrounding
the conditional remain eager. A saved alias retains the selected record after
shadowing or subsequent calls.

Native joins reserve fixed scalar/pointer/length slots for the selected fields.
Text fields move their pointer and length into those slots; the choice itself
does not allocate an aggregate buffer or copy text payloads. Producing a field
with `concat`, or returning text into a result destination, still performs the
copies required by those operations.

Each joined text pointer records both possible owners. Both stay live through
the joined field's last use, including returned aliases and later caller uses.
Nested joins reuse the existing provenance graph. Since 2026-09-16,
[native placement](bounded-text-storage.md) can overlay buffers created in
mutually exclusive arms, including nested choices, while retaining the region
until all joined aliases have finished using it. The compiler chooses this
placement only when it reduces the frame compared with ordinary last-use reuse.
Two exclusive 1 MiB writable alternatives with a tiny output now fit; two 1 MiB
values live together still exceed the 2 MiB ceiling once slots are included.
Placement remains conservative, and no runtime allocator fallback is introduced.

## Backends and services

| Path | Support |
|---|---|
| Rust verifier | Same-concept field joins, capacities, intervals, aliases and analysis budget |
| Interpreter | Existing selected-branch evaluation and per-rule input guards |
| Native CLI | Joined number/text record outputs retain JSON; scalar consumers retain their output policy |
| Native HTTP | Conditional `HttpResponse` values inside bounded pure handlers, with the existing sequential, forked and pooled transports |
| Sequential HTTP `after` | The formatter can select a record internally; its complete annotated text result is still copied into owned state |
| WASM / self-hosted | Explicit refusal of the bounded text contract before artifact emission |

Known literal HTTP status values retain their `[100, 599]` diagnostic through
joins, including an invalid value in just one alternative. This does not add a
general proof for arbitrary dynamic status expressions. The existing outer
`after` call restriction and persistent-copy ownership contract remain unchanged.
Nested record fields, effects, recursion, collections, Results and context/state
access inside participating rules remain outside this subset.

## Example and verification

```sh
cargo run -- examples/bounded_text_branches.verbose --run compose_text --input examples/bounded_text_branches.json
cargo run -- examples/bounded_text_branches.verbose --run compose_text --native /tmp/text-branches
/tmp/text-branches café 1 café -1
```

The two results are `[café]:1 | fixed:0 | [café]` and
`<café>:-1 | fixed:0 | <café>`. Native output appends a newline to each;
the interpreter includes the results in its execution report. The example can
expose the existing advisory inline-literal decoder warning described in the
[input example](bounded-text-inputs.md).

Differential tests cover field order, original/constructed/returned inputs,
aliases, shadowing, nested joins, empty/NUL/UTF-8 fields, booleans, record output,
invalid alternatives and budget refusals. Socket tests alternate binary response
bodies, statuses, empty bodies and malformed clients across all three transports,
and exercise a selected record field copied into persistent state.

The optional instruction trace checks condition and branch evaluation, copy
ranges, region reclamation and the absence of allocator syscalls on the argv
path. It compares copied bytes and operations with an equivalent constructor
using a scalar conditional; a record join must add no payload copy.

```sh
cargo build --locked
python3 tools/check_bounded_text_storage.py --check-branches
```

Its negative control selects the wrong record with an identical title. The
instruction counter must catch the changed branch even though stdout is unchanged.
