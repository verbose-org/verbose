# Native storage for bounded text

Implemented 2026-09-13. This implements the native storage of the
[bounded text result contract](bounded-text-output.md); it adds no syntax and
changes no unannotated call component.

A participating native entry owns one fixed stack region for its evaluation.
An independently materialized text result receives a destination sized from its
declared capacity (or its inferred capacity for an unannotated dependency).
Calls and concats in output position write directly into that destination.
Calls are still expanded at compilation; this is an invocation storage
convention, not a new general callable ABI.

```verbose
let message = piece(request)
let saved = message
let message = "shadow"
out = concat(saved, " | ", saved)
```

`piece` runs once. `saved` keeps the already computed value, including after
shadowing; the two uses do not expand its expression or repeat its computation.
Lets remain eager, including unused lets. A conditional executes only its selected
branch, and `and`/`or` retain short-circuit evaluation. Nested concats and text
conditionals can be materialized without the former emitter nesting restriction.

| Memory question | Contract in this slice |
|---|---|
| Owner | The enclosing entry evaluation or HTTP request owns all writable text buffers. Aliases share immutable values. Literals and input fields can be borrowed within that lifetime. |
| Capacity | Each text expression has a proved byte bound of at most 1 MiB. The native storage region plus saved registers and fixed formatting scratch must fit in 2 MiB. Slots and buffers use checked size arithmetic and eight-byte alignment. |
| Lifetime | Buffers stay valid until the entry output is written, or until the HTTP response finishes or its client is closed. Repeated argv records, stream lines and pooled requests release the region before the next evaluation. |
| Exhaustion | Excessive or unknown requirements cause a compile diagnostic before the output artifact is opened. There is no truncation, runtime heap fallback or recoverable memory error in this contract. |

The 2 MiB ceiling is a backend limit, not a new source annotation. It sums all
reserved temporaries, including unused lets and temporaries on both sides of a
conditional. Since 2026-09-15, output-position calls and conditional branches
forward the same result destination to their final producer. A concat in that
position fills the destination directly, without an intermediate concat buffer
or return copy. This applies through annotated and inferred-capacity callees,
with different input names; each rule still exposes its checked public capacity.
The selected output branch alone writes the shared destination.

This does not reuse slots based on last use. Lets and concat operands evaluate
in source order into independent values. A returned alias, input field or literal
still copies into the result destination; nested concat arguments still have
their own storage. The destination is fresh and is never exposed through a let
while it is being filled, so no live alias can observe partial writes or be
overwritten by a later call. A small output with large temporaries can still
exceed the storage limit.

In the [example](../examples/bounded_text_storage.verbose), `forward_text` passes
its destination through the selected `reuse_text` or `piece` call. `reuse_text`
keeps `saved` in its own buffer and reads it twice while filling the destination.
Two independent calls still reserve distinct results. A single 1 MiB concat
returned through a call chain now fits the 2 MiB region ceiling; two independent
1 MiB results plus their slots still exceed it and are refused before artifact
emission.

This region excludes the existing input/transport frames, argv data, literal
bytes embedded in the binary and the surrounding process. Numeric formatting
uses up to 24 additional stack bytes, covered by the fixed scratch allowance.
The bound is not a process RSS quota or a guarantee that an operator-supplied
stack limit has enough room. Releasing the region makes its bytes reusable; it
does not erase them or return every page to the OS.

The compiler also bounds work: at most 100,000 expression visits and 256 nested
levels during call expansion, and 16 MiB of emitted text literals per selected
entry. Alias descriptors do not copy literal data. The earlier whole-expression
substitution pass has been removed. Independent calls still expand separately,
so a branching call graph can still receive an expansion or storage diagnostic.
The existing verifier limits and subset restrictions remain in force.

The same emitter evaluates pure HTTP handlers. Only its result slots are passed
back to the transport; its region stays live through response writes. Request
bodies use the parser's counted pointer/length pair, so embedded NUL bytes do not
shorten a copy or comparison. The CLI retains its existing input-channel rules,
including NUL-terminated text inputs. Scalar and flat-record wrappers can consume
a bounded text call; persistent state, effects, recursive rules and cross-concept
call inputs remain outside this subset.

| Path | Support |
|---|---|
| Interpreter | Existing eager lexical value semantics and capacity checks; its Rust allocations are not covered by the native storage ceiling |
| Native argv / stdin / raw stdin / stream | Fixed invocation storage; output policy and input guards retained |
| Native HTTP, sequential / forked / pooled | Same storage emitter, pure handler without state, logs or after mutations |
| WASM / self-hosted compiler | Explicit refusal of the output contract before artifact emission |

Run the [storage example](../examples/bounded_text_storage.verbose):

```sh
cargo run -- examples/bounded_text_storage.verbose --run choose_text --input examples/bounded_text_storage.json
cargo run -- examples/bounded_text_storage.verbose --run choose_text --native /tmp/text-storage
/tmp/text-storage café 42 café -42

cargo run -- examples/bounded_text_storage.verbose --run forward_text --native /tmp/text-forward
/tmp/text-forward café 42 café -42
```

`choose_text` outputs `<[café]42 | [café]42>` followed by
`{[café]-42 | [café]-42}`. `forward_text` outputs `[café]42 | [café]42`
followed by `[café]-42`. Differential tests cover nested branches, numeric
extremes, empty/NUL/multibyte text, lexical records and aliases. Reuse tests process
600 argv records and 600 stream lines with a 256 KiB stack, and inspect idle worker
stack pointers and descriptor counts after repeated binary HTTP responses and
malformed requests. These establish storage reuse and value correctness for the
covered cases; they are not a throughput benchmark.

For a direct execution check, run:

```sh
cargo build --locked
python3 tools/check_bounded_text_storage.py
```

This Linux x86-64 check needs ptrace permission. It counts actual evaluations of
unused lets, aliased calls and conditional operands, including their order;
checks copy destinations against the reserved region and non-overlapping
sources; and checks that the traced argv binaries use only `write` and `exit`
syscalls. Its negative control changes short-circuit execution
while preserving stdout, and must be caught by the evaluation counts.

To compare destination forwarding with a compiler from before this optimization:

```sh
python3 tools/check_bounded_text_storage.py --reference-compiler /path/to/reference-verbosec
```

The comparison uses the same source and records in both compilers. It measures
the reserved frame, executed copy operations and actual copied bytes (including
REP instructions stepped across multiple traps), and requires a reduction in
each metric for all six cases. It also checks output, exit status, evaluation
counts and frame reclamation on both binaries. These are instruction/storage
measurements, not throughput or latency claims.

Measured on 2026-09-15 against `f1656fc` (before destination forwarding), all
six cases reserve 280 frame bytes instead of 696, excluding the unchanged
48-byte saved-register/scratch allowance. For the three-record cases, actual
copied bytes fall from 516 to 151 and executed copy operations from 26 to 14.
Both compilers pass the value, evaluation and reclamation checks; the negative
control is caught. These figures describe this fixture only.
