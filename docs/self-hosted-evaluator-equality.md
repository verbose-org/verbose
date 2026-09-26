# Self-hosted evaluator text equality

Design fixed before implementation, 2026-09-26.

## Contract

The evaluator written in Verbose must compare the decoded contents of two text
values for `==` and `!=`. Equal bytes at different source offsets are equal;
different bytes, lengths or Unicode encodings are unequal. No normalization is
performed. Evaluate each operand once, in source order, before comparing it.
Numeric and boolean equality keep the existing numeric representation.

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
lengths before a bounds-guarded comparison. Neither path materializes text or
creates new arena nodes during comparison. The concat fallback retains the
existing interpreter walk's potentially quadratic cost; native target emission
and its byte loop are unchanged. There is no performance claim in this slice.

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
