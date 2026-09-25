# Self-hosted text literal escapes

Design fixed before implementation, 2026-09-25. Implemented on the same date;
validation results below refer to this correction.

## Contract

The self-hosted compiler must give ordinary text literals the same byte content
as the Rust front end: decode `\n`, `\r`, `\t`, `\\` and `\"` exactly once, retain
UTF-8 bytes, and reject unknown/truncated escapes or unclosed text literals before
emitting any ELF bytes. Byte literals retain their separate grammar. Escapes in
comments are not tokens. Attribute lines, discarded by the main tokenizer, are
tokenized separately for this validation. Existing backend subset refusals remain
in force.

The correction covers direct/concatenated output and literals carried through
lets, calls, records, variants and Results wherever those forms already compile.
Lengths, indexing, slicing and equality must see decoded bytes, including their
failure boundaries. A printer-only patch would leave the shared value wrong.

## Representation

Keep the emitted packed `(source_offset, byte_length)` word and all its consumers.
The embedded source storage keeps its existing file size and offsets. At emission,
use the already parsed token stream to locate text literals with escapes. Within
each such literal's original content span, write decoded bytes followed by zero
padding to the original span size. Quotes, comments, byte literals, attribute
lines and other source spans remain unchanged. Attribute text is not runtime
constant data. Packed literal values and direct text writes use
the decoded length. Padding is outside the value's accessible span.

This storage is a constant-data image, not a source snapshot that can be reparsed.
Parser/verifier walks continue to use the original source. Compile-time data
preparation adds no target allocation, GC, decoding loop or general calling
convention. Source-free programs and unchanged constant data retain their existing
emission. The raw `src_blob` helper remains available for unchanged spans.

The emitter walks tokens and source positions forward, with a four-byte raw copy
path outside transformed spans. It reuses the existing escape decoder after
validation. Its final four-byte alignment and the ELF file-size calculation stay
identical. Literal and scalar instruction sizes stay fixed; only literal length
immediates and embedded bytes change.

The self-hosted evaluator has a separate representation: its `VText` spans still
refer to the original encoded source. Shared length/read helpers decode these
spans on demand; substring maps decoded boundaries back to encoded source spans.
This preserves its existing defensive invalid-index behavior and does not extend
its supported operations. Native emission remains the semantic comparison against
the Rust interpreter and native backend.

## Validation and delivery

- Every escape, consecutive escapes and literal backslash-letter pairs; UTF-8
  before/after escapes, empty strings, quote boundaries and all four source
  alignments. Comments, metadata, adjacent strings and byte literals must not
  confuse the data transformation.
- Direct output, concat, aliases, branches, calls, record/variant/Result
  payloads in already supported forms, equality, length, byte reads and slices.
  Compare exact stdout, stderr and exit status against explicit values and Rust.
- Invalid text escapes and unclosed literals refuse before an artifact, including
  unused declarations. Keep byte-literal escapes and prior subset refusals.
- Check data offsets, padding, ELF size and fixed instruction lengths; compare
  unaffected emitted programs with the reference self-hosted compiler.
- Run the normal Rust suite serially, Python/CIDX checks, the complete bootstrap
  and its fixed point, and the existing corpus. Add escaped-text probes to gen1
  execution as well as gen0 so host-compiler agreement cannot mask a bootstrap gap.

No performance benchmark is part of this correction. The NUL-terminated source
input transport and general text ownership remain separate work.

The parent-emitter comparison covered all 192 examples at rule index 0: the same
93 are accepted, and repeated current emissions are deterministic. For identical
source input, every ELF size is unchanged; only `health_check` and `vexprparse`
change bytes. An independent decoder accounts for every changed literal-length
immediate and constant-data byte. This is an emission comparison, not a claim that
all accepted programs run correctly at their intended application entry.

Historical service hashes also required a narrow update: across 19 baseline
fixtures, eight change exactly two embedded source-data bytes (`\n` becomes LF
plus padding). Their headers, instructions and file sizes remain identical.
The service tests retain their network, log-file and concurrency assertions.

## Scope found during verification

Three pre-existing limitations prevented extending the positive matrix to every
form originally proposed: streaming a text alias prints its packed descriptor,
streaming a general substring emits a trap, and repeated let names do not obey
lexical shadowing consistently across the existing backends. They are recorded
with their mechanisms in [known gaps](known-gaps.md#text-alias-output-shadowing-and-streamed-substrings).
The correction tests alias and slice contents through numeric consumers, and
tests direct literal/concat/call output and both conditional branches. It does
not claim to repair those separate lookup/classification/emission paths.
