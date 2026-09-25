# UTF-8 source text

Verbose source files are UTF-8. A quoted text literal preserves the bytes between
its quotes, except for the closed escape set `\n`, `\r`, `\t`, `\\`, and `\"`.
Unknown escapes and unclosed literals are lexer errors. Text does not gain
`\xNN` or `\uNNNN` escapes; `b"..."` keeps its separate raw-byte contract,
including `\xNN`.

```verbose
out = length("é€🦀")
```

This returns 9: the characters occupy two, three and four UTF-8 bytes. `byte_at`
indexes those bytes, and `text [..N]` bounds bytes rather than characters. For
example, `"é"` fits `text [..2]` but not `text [..1]`. There is no normalization:
`"é"` and `"é"` have different bytes, lengths and equality results. Embedded NUL
bytes are retained in literals; this does not change argv or other transport
restrictions. Diagnostic columns continue to count source bytes.

## Correction and compatibility

Before the 2026-09-25 correction, the Rust lexer expanded each non-ASCII byte into
a separate Unicode character. `"é"` consequently became `"Ã©"`. It now collects
bytes directly and converts the buffer into a string once. The input is already
valid UTF-8, and replacing ASCII escape pairs with ASCII bytes preserves validity.
The conversion reuses the buffer. Generated programs gain no conversion loop,
runtime allocation, normalization or garbage collection from this correction.

ASCII literals and raw byte literals retain their previous meaning. Non-ASCII
text literals intentionally change to the written bytes. This also affects text
used as paths, comparison operands, metadata or bounded outputs. A program that
depended on the corrupted spelling needs to write that spelling explicitly.
Storage reports continue to account for the bytes the compiler actually emits.

The Rust front end is shared by interpretation, native emission and WASM. The
self-hosted compiler already preserved unescaped UTF-8. A subsequent
[escape correction](self-hosted-text-escapes.md) decodes ordinary text constants
before emission, retains their source offsets with inaccessible padding, and
stores their decoded byte lengths. Its evaluator measures and reads encoded
source spans through the same escape decoder. Neither correction extends a
backend's supported language subset.

## Regression coverage

- Lexer tests cover UTF-8 width boundaries, combining accents, NUL, ASCII,
  escapes next to multibyte characters, byte columns and malformed literals.
- Original and optimized ASTs agree with explicit expected values and native
  stdout/stderr/exit status for text, concat, length, indexed bytes and equality.
- Bounded native outputs accept exact byte capacities, reject one byte less and
  preserve an existing artifact on refusal. HTTP log budgets count source UTF-8.
- WASM data segments preserve exact bytes and lengths. The `two_generation`
  bootstrap suite runs UTF-8 and escaped-text fixtures through the self-hosted
  compiler, including the self-generated gen1. It compares explicit output,
  errors, status, literal storage and ELF sizes, and refuses invalid escapes in
  used/unused declarations and metadata before emission. Embedded source NUL
  remains covered through the Rust front end only: the self-hosted compiler's
  legacy `stdin-raw` input is NUL-terminated.

Run `cargo test source_utf8 -- --test-threads=1` for the focused checks. Run
`cargo test --release source_utf8 -- --ignored --test-threads=1` with an unlimited
shell stack for the self-hosted differential.
