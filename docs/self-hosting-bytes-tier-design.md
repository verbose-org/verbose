# Semantic self-hosting — the missing BYTES tier

## The finding (profiled to root)
The structural fixed point is reached (vexprparse emits a valid 1.2 MB ELF for its
own source), but the self-emitted emitter does NOT reproduce `elf_program_src`
(self-emitted binary SIGSEGVs / mis-behaves). Root cause — traced, not guessed:

**The self-hosted compiler has no BYTES tier.** Its `Ast` concept variants are
AstNum/AstBin/AstNeg/AstNot/AstVar/AstField/AstStr/AstBool/AstErr/AstCall/AstVariant/
AstMatch/AstIf — no `AstBytes`, no `le32`/`le64`, no bytes-`concat`. But
`elf_program_src` and the whole codegen backend are BUILT of exactly those:
- `b"..."` literals: **176** in the source
- `le32(...)`: **60** · `le64(...)`: **8**
- rules with `output: ... bytes`: **24** (x86_node, x86_proc, x86_program, src_blob,
  elf_program_src, the streaming/arena emitters …)

How they currently mis-parse (shape-probed):
- `b"\x41"` → **AstVar(`b`)** + a dangling string token (the `b` prefix is read as an
  identifier; the string is orphaned) — mangled.
- `le32(5)` → **AstCall** to an undefined rule `le32`.
- `concat(a, b)` → **AstCall** (bytes-concat indistinguishable from text/other calls).

So when the self-hosted emitter emits a bytes-returning rule, every `b"..."` is a
wrong AstVar, every `le32`/`le64`/`concat` is a call to a non-existent rule (bad
`proc_offset` → bad `call` rel32 → jump to garbage → SIGSEGV), and the entry
trampoline for a bytes result never writes `(ptr,len)`. The emitter cannot reproduce
itself because it cannot represent its own construction material.

This is not a bug — it's a missing tier, exactly like text was before its arc. It's
the LAST tier for semantic self-hosting: the compiler's OWN output language.

## The arc (bytes tier — parse → AST → eval → codegen, like text)

**Slice B1 — lexer + AST.** A `b"..."` bytes-literal token (with `\xNN` hex escapes —
the source uses them heavily) → new `AstBytes(start, len)` (raw bytes, NOT a text
span — no quote-strip semantics; hex-decoded content). Recognize `le32`/`le64` as
PRIMITIVES (like byte_at/length), not calls. Bytes-`concat` = concat whose args are
bytes/le32/le64/bytes-rules (distinguish from text-concat by arg type).

**Slice B2 — interpreter (oracle).** A bytes VALUE model: a raw byte sequence
(cons-of-bytes, or a (ptr,len) into a bytes arena — mirror text's VText but raw). Eval
`b"..."` → its bytes; `le32(n)`/`le64(n)` → 4/8 little-endian bytes; bytes-`concat` →
concatenation; a bytes-returning rule call → its bytes. `length` of bytes = byte
count. This is the oracle for B3.

**Slice B3 — codegen.** Emit bytes-returning rules: `b"..."` → the raw bytes inline
(jmp-over-data + ptr/len, like AstStr but no quote strip); `le32`/`le64` → emit the
LE-encoded bytes (or compute + store at runtime); bytes-`concat` → the existing
concat buffer path but byte-exact; the entry trampoline for a bytes result →
`write(1, ptr, len)`. Gate against B2 + against the Rust backend byte-for-byte.

**Slice B4 — the semantic fixed point.** Reorder `elf_program_src` first, self-emit
it, run the self-emitted emitter on a small program P, and assert its output is
byte-identical to the Rust-compiled `elf_program_src(P)`. That is full semantic
self-reproduction — the north star.

## Honest scope
This is a full tier (four slices), comparable to or larger than the text arc, because
bytes are the emitter's entire material (176 literals / 68 le32/64 / 24 bytes rules).
The structural fixed point (PR #96) stands; semantic self-reproduction is gated on
this tier. It is the last major arc — after it, vexprparse compiles vexprparse in the
full sense. B1 (lexer + AST) is the opener; design each subsequent slice when reached.
