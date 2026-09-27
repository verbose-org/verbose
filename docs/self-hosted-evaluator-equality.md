# Self-hosted evaluator text equality

Design fixed before implementation, 2026-09-26. Implemented on the same date.

## Contract

The evaluator written in Verbose must compare the decoded contents of two text
values for `==` and `!=`. Equal bytes at different source offsets are equal;
different bytes, lengths or Unicode encodings are unequal. No normalization is
performed. Evaluate each operand once, in source order, before comparing it.
Numeric and boolean equality keep the evaluator's existing numeric
representation. Boolean equality is only an unchecked evaluator control here;
the current source verifier still refuses `bool == bool`.

The defect is in `eval_ast_env`: both text operands go through `vnum_of`, which
returns zero. Thus `"a" == "b"` is true and `"a" != "b"` is false. The compiler's
native text comparison is independent and already compares bytes.

## Representation and scope

Dispatch on the evaluated value kinds before comparison. Text spans and text
concatenation trees compare as text; numbers compare as numbers. A numeric leaf
inside a text concat contributes its decimal rendering. Bytes and aggregates do
not acquire an equality contract. Mixed or unsupported kinds in the unchecked
evaluator return a defensive unequal result; source verification remains the
authority for admissible operand types. This does not add evaluator diagnostics
or turn its existing defensive error behavior into native failure semantics.

Compare two encoded source spans with forward cursors, decoding the existing
closed escape set and stopping at the first differing byte or unequal end.
Aliases, slices and text returned by a rule retain these spans. For a concat
tree, reuse the evaluator's existing value-length and byte-access walks, checking
lengths before a bounds-guarded comparison. Neither path materializes text.
The Rust-built evaluator passes the helper records on the stack; self-hosted
emission places them in its existing arena. Enclose operand evaluation and
comparison in `arena_scope`: only the scalar comparison result escapes, and its
`VNum` wrapper is constructed afterward. Earlier environment values remain live.
This reclaims temporary operand values and cursor records after each comparison;
it does not claim constant peak space or a general evaluator memory budget.
The concat fallback retains the existing interpreter walk's potentially
quadratic cost; native target emission
and its byte loop are unchanged. There is no performance claim in this slice.

The numeric-concat boundary probe also exposes an existing evaluator defect:
negating `i64::MIN` cannot produce a positive magnitude, so the decimal readers
report length 2 and incorrect digits. Keep the ordinary magnitude path and
handle this single value with its fixed 20-byte decimal representation. Both
length and indexed reads share that exception; bounds still guard every read.

The existing NUL-terminated source transport, invalid-slice behavior and effect
stubs remain outside this correction. The equality probes use valid text slice
boundaries and source inputs this transport can carry. Native emission and WASM
receive no new accepted forms.

## Verification and delivery

- Compare original-AST Rust interpretation and explicit expected values with the
  Rust-built evaluator and an evaluator emitted by the self-hosted compiler.
  Include equal/unequal/empty texts, same-length mismatches, prefixes, all ASCII
  escapes, UTF-8, distinct normalization forms, aliases, rebinding, valid slices,
  nested concats and text-returning calls. Check both operators and numeric/
  boolean controls, including values at different source positions.
- Keep source verification negative cases for text/number, bytes and aggregate
  operands. Check unchecked evaluation separately without claiming parity for
  invalid programs or existing error/effect behavior.
- Compare exact stdout, stderr and exit status. Exercise the corrected evaluator
  after self-compilation in the two-generation bootstrap. Remove the old text
  equality exclusion from the lexical-binding evaluator probes.
- Compare the unchanged example corpus with the saved parent emitter, including
  acceptance, diagnostics and deterministic output bytes. Changes to the
  self-source itself must be identified separately.
- Run the serialized normal Rust suite, Python harnesses, CIDX checks and full
  two-generation bootstrap. Deliver through a separate PR.

## Recorded validation

The serialized normal suite passes 877 tests (28 explicitly ignored); the seven
Python harness suites pass 97 tests. CIDX configuration, environment and security
checks pass, with the existing Python dependency findings still reported.

The equality matrix passes with the Rust-built evaluator and evaluators emitted
by both gen0 and gen1. It includes the original-AST Rust interpreter as a
reference for valid programs and separate defensive/boolean controls for
unchecked inputs. The fixed point also exercises the existing lexical-binding
probes with equality enabled, nested comparison scopes and use of earlier values
after a comparison and subsequent fresh allocations.

The Rust-built self-hosted emitter itself remains byte-identical to parent
`ca87a3d` (SHA-256 `de6007d321135e317d88a9a39855581dc2941638fc2af07958a903712cf63e58`).
All 192 example sources preserve acceptance (93 accepted), diagnostics and
emitted bytes for identical source input; two emissions per compiler/source
check reproducibility. The updated self-source emits the same 3,128,560-byte
ELF with both emitters. Its new source content is intentionally distinct from
the parent's self-source; this is not a claim that the evaluator binary itself
is unchanged.
