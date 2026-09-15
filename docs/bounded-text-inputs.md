# Checked record inputs for bounded text composition

Implemented 2026-09-15. Rules participating in the
[bounded text contract](bounded-text-output.md) can now receive an explicitly
constructed flat concept, a record returned by another rule, or a lexical alias
of either. They can still receive the original input, including through an alias.

```verbose
let packed = FormatInput {
  title: concat("[", request.title, "]"),
  code: if request.code > 0 then 1 else -1
}
let saved = packed
let packed = FormatInput { title: "shadow", code: 0 }
out = render_text(saved)
```

Here `request` has a different concept from `FormatInput`. The latter declares
`title : text [..10]` and `code : number [-1, 1]`. The compiler proves that the
supplied title fits ten bytes and that both numeric branches fit the range
before applying `render_text`'s input assumptions. Concept names must match at
the call boundary: equal field layouts do not imply an implicit conversion.

## What is checked

Calls take exactly one record input. The existing flat subset admits number and
text fields; constructors must provide each declared field exactly once with
the correct type. A constructed argument also needs a known byte capacity for
each text value and a declared text capacity in the callee's input concept.
Excessive or unknown capacities cause a diagnostic naming the call and field.

Numeric literals carry their exact value; input fields carry their declared
interval, or the full i64 range when unrestricted. Conditional expressions join
the intervals of both branches. `length(text)` conservatively carries
`[0, text capacity]`, or the full nonnegative range when capacity is unknown.
These facts survive lets,
record fields and rule returns. A supplied interval must fit a constrained
numeric input field; the compiler does not infer correlations between conditions.
Arithmetic remains outside this text subset.

Rule bodies are checked against their declared inputs, independently of the
arguments at a particular call site. A shorter actual title does not justify
a tighter public output annotation. Annotated text callees still expose their
declared result capacity; unannotated dependencies expose the checked inference.

The transfer check follows field facts through aliases and returned records.
Merely constructing a value named `FormatInput` does not prove its fields fit.
This checks rule-call boundaries; it does not add general enforcement of every
record-field range in legacy record construction or output. Forwarding an entry
input preserves that concept's existing input guards, including their previous
acceptance of unused unbounded text fields.

## Evaluation and storage

Constructor fields evaluate once in source order, including fields the callee
does not use. The entire argument is evaluated before any callee let. Let aliases
keep the computed record; shadowing changes the binding used by later expressions
without changing earlier aliases. Passing the original input by an alias is valid,
and a shadowed input name denotes its newly bound value.

The native emitter passes value descriptors to its expanded callee. Already
computed text fields retain their existing immutable buffers. Creating a record
does not introduce a runtime aggregate allocation or a second copy of its text.
A field expression such as `concat` still materializes its own value, and a text
return still writes into its result destination.

Arguments retain their owners through later field evaluations, callee work,
returned records and subsequent caller uses. Buffer placement follows those
uses, including aliases. Everything remains inside the enclosing invocation's
[existing storage contract](bounded-text-storage.md), with its separate 2 MiB
native ceiling. This adds no general calling convention, heap allocator,
reference counting or ownership beyond the invocation.

## Services and backends

| Path | Support |
|---|---|
| Rust verifier | Types, byte capacities, numeric intervals, lexical aliases and acyclic composition |
| Interpreter | Existing eager record evaluation and per-rule input guards; invalid runtime inputs fail evaluation |
| Native CLI input channels | Constructed and returned records with invocation-owned text storage; existing entry guards and output policy retained |
| Native HTTP | The same composition inside pure bounded handlers, in sequential, forked and pooled services |
| Sequential HTTP `after` | A formatter can project the request into another concept internally; the outer persistent-copy call still requires the original HTTP input |
| WASM / self-hosted compiler | Explicit refusal of the bounded text contract before artifact emission |

For example, a formatter taking `HttpRequest` may call
`wrap_body(BodyInput { body: input.body })`. Its `BodyInput` contract makes the
formatter independent of HTTP. The parser supplies counted bytes, so native
copies preserve NUL and non-UTF-8 bodies. [Persistent state](bounded-text-state.md)
still receives its own copy after the outer formatter returns.

Context inputs, effects, recursion, collections, Results, nested or conditional
records, and state access inside participating rules remain outside this slice.
Existing syntax is reused; unannotated call components retain their behavior.

## Example and verification

The [complete example](../examples/bounded_text_inputs.verbose) composes a
record-producing rule, two formatter calls and a retained field alias:

```sh
cargo run -- examples/bounded_text_inputs.verbose --run compose_text --input examples/bounded_text_inputs.json
cargo run -- examples/bounded_text_inputs.verbose --run compose_text --native /tmp/text-inputs
/tmp/text-inputs café 42
```

The result is `[café]:1 | fixed:0 | [café]`. The native binary writes it with a
trailing newline; the interpreter includes it in its execution report.

The compiler's advisory x86 decoder can warn about inline literal data after a
near jump. This example exposes that existing limitation on the unused `shadow`
literal; the emitted jump skips all six data bytes. The warning was also
reproduced with the reference compiler and the previous bounded-text syntax.
The executable's stderr is empty. This slice does not change the decoder.

Differential tests cover input aliases, shadowing, nested calls, returned records,
numeric extrema, range failures, empty/NUL/multibyte fields and preservation of
existing artifacts on refusal. Entry range guards are checked on argv, tokenized
stdin, streaming stdin and raw stdin. Socket tests cover all three HTTP execution
modes and composition inside a persistent-state formatter.

The optional Linux x86-64 instruction check observes constructor evaluation,
including an unused field, before callee entry; verifies buffer copy ranges and
invocation reclamation; and permits only output/exit syscalls:

```sh
cargo build --locked
python3 tools/check_bounded_text_storage.py --check-inputs
```

Its negative control skips the unused field while retaining identical output.
The instruction counter must detect the skipped evaluation.
