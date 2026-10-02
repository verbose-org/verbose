# Self-hosted scalar map outputs

The subsequent [text map slice](self-hosted-text-map.md) extends this loop to
supported `collection(text)` outputs and supersedes the payroll refusal
recorded below. The rest of this document records the number/bool slice.

## Design fixed before implementation

Parent `ba7d382` (#264) refuses intermediate collections but still emits an
`int3` for a record-element map whose output is a scalar. In particular,
`retirement.verbose::retirement_status` traps instead of printing booleans.
The Rust native reference prints every result and exits 1 if any result is
false. Empty collections print nothing and exit 0.

Implement number/bool projections from flat records with number/text fields,
and boolean maps over number elements. Preserve existing number-to-number and
record-to-record emission. Keep #264's restriction to final streamed producers;
there is still no intermediate collection value ABI.

Use the existing strict lexical type facts for map bodies, including sequential
lets, aliases and the item binder. The declared output element must match the
established body kind; unknown or unsupported scalar output facts refuse before
ELF/raw bytes. Unsupported boolean filters also refuse rather than using an
uninitialized boolean-map status. This is a backend capability check, not a
general repair of legacy return/body typing.

The new path requires a direct collection field of the unshadowed rule input.
The collection is the last input field; preceding input fields are numeric.
Elements are numbers, or nonempty flat records of numbers/text. Limit copied
text fields to the existing argv 1 MiB region (16 slots of 64 KiB), with an explicit
length check before each copy. Reject other input layouts for the new path.
Validate the count against the remaining argv words before the first element
load, using division by the element stride to avoid multiplication overflow.
Negative counts and truncated argument lists fail with exit 1 before output.

Text element slots are overwritten by another collection traversal. On this
new path, a body with text-bearing elements therefore refuses nested collection
operations and calls taking collection-bearing/nested input records. Calls
with scalar or flat scalar-record inputs remain available; they cannot carry
an argv collection pointer into another traversal. Number-only element maps
can use nested reductions, with the outer loop registers saved explicitly.

Per element, save the arena mark, load the item, save the loop cursor/count,
evaluate the body once, restore the loop registers, print the scalar and restore
the arena mark. Only the current element and its body temporaries remain live.
The existing second binder slot holds the boolean failure flag; numbers need
no result collection or accumulated output buffer. This adds no GC and makes
no claim about a body's independent recursion/resource budgets.

Boolean formatting writes `true\n` or `false\n` from a temporary stack word.
False sets the frame flag but does not stop later elements. The procedure
returns the flag and the collection entry uses it as exit status. Number output
reuses the existing decimal printer. Emission and sizing must share the new
loop helpers and agree exactly. Boolean literals and `not` also need ordinary
value lowering, which currently uses placeholders in `x86_node`.

Keep the established native streaming publication policy: a later body failure
preserves the already printed prefix and stops subsequent elements. The
interpreter remains the value/evaluation oracle; its eagerly materialized list
and CLI error output are not an oracle for native partial-output publication.

## Validation planned

Compare original-AST values, Rust native behavior where supported, self-hosted
ELF output and raw refusal, including both bootstrap generations. Cover empty,
all-true, all-false and mixed collections; number extremes; UTF-8/text fields;
aliases/shadowing; nested reductions and calls that clobber loop registers;
late failures; malformed counts/argv and text capacity edges. Test unknown or
mismatched output types and unsupported layouts before artifact bytes.
Demonstrate bounded arena reuse independently of output values, and compare
unchanged examples against the parent twice per source. Run serialized normal
tests, Python harnesses, CIDX checks and the full two-generation bootstrap.

The design commit uses the exact parent's green validation: 889 normal Rust,
27 bootstrap and 97 Python tests, with all six remote checks passing on
`f6a39e5` before the merge.

Implementation inspection corrected the draft's text-region assumption: argv
entry reserves 1 MiB; the 4 MiB reservation belongs to stdin source input.
The scalar map limit is therefore 16 text fields, not the draft's 64, without
increasing the existing reservation.

## Implemented support

The implementation is in `examples/vexprparse.verbose`; no Rust production
emitter, collection representation or language syntax changes. The existing
`collection_lowering_check` driver includes scalar-map capability failures.
Both output drivers check the complete rule list before entry selection/output.
Refusal follows their current convention: exit 1, no stdout or stderr.

| Shape | Self-hosted ELF / raw | Reference |
| --- | --- | --- |
| Number elements → number | Existing emitter retained | Existing semantics |
| Number elements → bool | New scalar loop, sticky false status | Interpreter and Rust native |
| Flat number/text records → number/bool | New scalar loop, checked argv/text loads, per-element arena restoration | Interpreter; Rust native for its supported body forms |
| Record elements → record, direct filters | Existing emitter retained | Existing limits |
| Aliases of captured scalar lets, shadowed item name | Typed in lexical scope | Original-AST interpreter |
| Collection through an alias or unsupported input/element layout on the new path | Refused before bytes | Backend capability limit |
| Nested traversal over text-bearing items, including collection-bearing call inputs | Refused before bytes | Text-slot lifetime boundary |
| Intermediate map/filter or collection-returning call | Refused before bytes | Earlier collection boundary retained |

The Rust reference emitter still refuses some captures and rich record-call
forms that the self-hosted backend supports; those cases use the original-AST
interpreter as their value oracle. The self-hosted parser also already recognizes
`true`/`false` expression literals, unlike the current Rust expression grammar.
Literal tests compare against equivalent constant comparisons in the reference;
`not`, comparisons and conditional evaluation use the original expressions.

For example, compile the existing retirement program with the compiler written
in Verbose:

```sh
verbosec examples/vexprparse.verbose --native /tmp/compiler --run elf_program_src --stdin-raw
/tmp/compiler 0 < examples/retirement.verbose > /tmp/retirement
chmod +x /tmp/retirement
/tmp/retirement 3 alice 64 bob 65 carol 70
# false, true, true on separate lines; exit status 1
```

This replaces the parent's `int3` for `retirement_status`. An empty batch exits
0 with no output. A malformed count or incomplete record tail exits 1 before
any element output. A later body/copy failure preserves completed output.
The existing decimal input parser is retained; this slice does not introduce a
new decimal grammar or general overflow validation for argv conversion.

## Storage validation

`src/selfhost_scalar_map_tests.rs` runs the scalar matrix on gen0 and gen1 in
the existing bootstrap driver. In addition to value/status comparisons, it
reduces the test target's arena reservation to one 4096-byte page and processes
4000 records, allocating an additional record for a body call on each element.
All outputs must agree. A negative-control target discards each saved mark
instead of restoring it and must exhaust that page. This demonstrates reuse
across elements, not a general bound on recursive body allocation or process RSS.

Text tests exercise the last of all 16 distinct slots, lengths 65535/65536/65537,
UTF-8 and empty strings. Invalid nominal/bool/nested element fields, the 17th
text field, aliases of collection inputs, incompatible/unknown result types and
uncalled incompatible rules refuse before ELF/raw bytes. Existing native
reservations remain unchanged; there is no runtime tracing or garbage collector.

The full `payroll.verbose` file now refuses because its `names` rule returns
`collection(text)`, including when another entry is selected. The parent
accepted that file but its `names` entry trapped. This is a real file-level
acceptance change (93 → 92 of 193 examples at entry 0), caused by checking all
rules. Isolated `compute_bonuses` (2377 bytes) and `high_earners` (2295 bytes)
retain byte-identical code and JSON output. Isolated `salaries` changes from a
1535-byte trapping ELF to 1972 bytes printing the salary values. Text collection
output remains a later slice; Rust's acceptance of the full file is unchanged.

Ordinary `not` value lowering also repairs `priv_failure_from_external`: its
true and false cases now agree with Rust native instead of trapping when the
right-hand predicate is evaluated. Its unrelated entry-0 output is unchanged;
the complete embedded procedure body grows by 30 bytes.

## Recorded validation

- Serialized normal Rust suite: 890 passed, 28 explicitly ignored; the final
  strengthened scalar matrix passes separately too.
- Python harnesses: 97 passed.
- Parent and current emitters produce the same 3,336,909-byte ELF from the final
  self-source, twice each (SHA-256
  `e22bf5a54044046145d03991b0a981776b715a7bf6e2333cbd4a494f4e4be281`).
- Local CIDX configuration, environment and security phase pass. Trivy still
  reports the existing Python-environment findings; passing the configured
  phase does not mean there are no dependency vulnerabilities.
