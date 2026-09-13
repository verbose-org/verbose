# Bounded text results

Implemented contract, 2026-09-12.

```verbose
output:
  out : text [..64]
```

The rule promises a result of at most 64 bytes, measured in UTF-8 bytes rather
than characters. Zero is a valid bound for an empty result; the initial ceiling
is 1 MiB. This is a static obligation: compilation refuses an excessive or
unknown bound. It never truncates a result. Without the annotation, the legacy
acceptance rules remain in force outside the participating call graph.

The analysis checks pure acyclic rules, their callers and their dependencies.
Inputs are flat concepts of numbers and text. Text literals have their exact
byte lengths; input fields use their declared bounds; numeric formatting needs
at most 20 bytes. Concatenation adds capacities and a conditional takes the
maximum of both branches. Lets are processed in lexical order, including aliases
and shadowing. A checked callee exposes its declared capacity to its callers;
an unannotated dependency exposes its inferred capacity. Calls pass the original
input, `callee(input)`, with the same concept. Constructed call inputs, recursion,
collections, Results, effects and context inputs receive explicit diagnostics.
Unknown bounds are never accepted as evidence of the annotation.

Supported scalar expressions are number literals/fields, `length(text)`, scalar
comparisons, boolean operations and conditionals. Arithmetic, `substring`,
`json_escape` and other checked primitives are outside this slice. Flat record
construction supports a wrapper such as `HttpResponse`; conditional records and
nested records are refused. An annotation applies to a rule's text output only,
not to record-field ranges or service state. Optimization hints on participating
rules are also refused. Rules elsewhere in the program keep their existing
acceptance and optimization behavior.

The analysis does not infer correlations between conditions: both branches must
fit. It stops with a diagnostic after 100,000 visited expression nodes, 256
expression levels or 128 nested calls. Capacity addition uses checked arithmetic.

The annotation bounds the returned value. Native compilation now also assigns
invocation-owned storage to the checked subset: lets evaluate once, aliases
share values, and text returns have caller-owned destinations. The separate
2 MiB native storage ceiling counts slots, buffers and fixed scratch, including
both branches and unused lets. See [native text storage](bounded-text-storage.md)
for ownership, reclamation, limits and exclusions. This is not a process memory
quota or a general ownership type system.

Existing native input guards enforce the capacities assumed by the analysis.
The interpreter checks those inputs before evaluating a participating rule,
including an unannotated caller whose bounded callee sits in an untaken branch.
It checks annotated results as a runtime backstop. Input violations fail
evaluation; they do not produce a recoverable `Result`. Native emission requires
each text expression to have a known capacity at most 1 MiB and refuses declared
input text bounds above 1 MiB.

| Path | Support |
|---|---|
| Rust verifier | Static capacities, lexical binding types and call graph checks |
| Interpreter | Value semantics, annotated input checks and result backstop |
| Native Linux x86-64 | Checked subset with fixed invocation storage |
| HTTP service | Pure handler without state, logs or after mutations; request bounds derive from service declarations |
| WASM | Explicit refusal of participating rules and services before artifact emission |
| Self-hosted compiler | Output-section token refusal in both ELF and raw machine-code entry points |

The result capacity excludes the CLI's trailing newline and HTTP headers. HTTP
input `body` uses the maximum `max_request` across the program's HTTP services;
`path` uses the enforced 256-byte limit. This may be conservative for a smaller
service, but never assumes a bound only one of its callers enforces.

See [the standalone composition](../examples/bounded_text.verbose) and
[HTTP formatter](../examples/http_bounded_text.verbose). The standalone example
prints `<[café]42>`; changing its outer capacity from 32 to 31 is refused because
the callee's declared 30 bytes plus two delimiters require 32.

```sh
cargo run -- examples/bounded_text.verbose --run decorated_label --input examples/bounded_text.json
cargo run -- examples/bounded_text.verbose --run decorated_label --native /tmp/label
/tmp/label café 42
cargo run -- examples/http_bounded_text.verbose --native /tmp/label-http
```
