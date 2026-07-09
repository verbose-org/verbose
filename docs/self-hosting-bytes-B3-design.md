# Bytes tier B3 — codegen (stream bytes; the emitter emits its own material)

## Architecture: bytes-returning rules STREAM (reuse x86_stream_node)
The self-hosted emitter already has a streaming path (rule_is_texty / x86_stream_node
/ the `entrytx` trampoline): text-returning rules write their bytes to fd 1 in order.
`elf_program_src` builds its ELF via a big `concat`, so streaming each piece in order
is the natural model. B3 = extend that path to bytes nodes. The ONE structural
difference from text: a bytes entry must NOT append the trailing newline the text
entry does (it would corrupt the ELF).

This is why simple bytes programs currently output EMPTY: a bytes-returning rule isn't
recognized as streamable (rule_is_texty is false for it), so it falls to the number
trampoline (itoa of a stub) → nothing. B3 fixes that.

## Changes
1. **Streamable decision**: extend `rule_is_texty` (and `ast_texty_shallow`/
   `ast_is_texty`) so a rule whose result is bytes-shaped (AstBytes, le32/le64,
   bytes-`concat`, or a call to a bytes-returning rule) is streamable. Keep a way to
   know text-vs-bytes at the ENTRY (for the newline): a `rule_is_bytes` / entry flag.
2. **x86_stream_node — new arms**:
   - `AstBytes(start, len)`: DECODE the content at emit time (\xNN→byte, \n/\t/\\/\"
     — reuse B2's decode logic to produce the raw bytes), embed them inline
     (jmp-over-data), then `write(1, ptr, decoded_len)`. (Constant bytes → embed; the
     write is the streaming shape already used for text literals.)
   - `le32(n)` / `le64(n)`: eval the arg → rax; store 4/8 little-endian bytes to a
     stack scratch (mov [scratch], eax for le32 — x86 is LE so a raw 4-byte store IS
     the encoding; le64 = 8-byte store); `write(1, scratch, 4|8)`.
   - bytes-`concat`: stream each arg in order (the existing concat-streaming shape —
     verify it already recurses per-arg; bytes args stream via the above).
   - `code_size_stream_node` mirrors each new arm EXACTLY (the drift edge).
3. **Bytes entry trampoline**: a variant of the `entrytx` branch that does
   `call entry ; exit(0)` with NO `push 10 / write newline`. Gate the newline on
   text-vs-bytes entry. Number/text entries byte-identical.

## Gate (CLEAN disk; self-emit bytes programs, compare output to the B2 oracle)
1. vexprparse verifies; suite green (currently 445 + 1 ignored) + a new B3 test.
2. **MILESTONE — the emitter emits bytes** (feed a bytes program to the self-hosted
   emitter `--native --run elf_program_src` [argv, <128KB], run the emitted ELF, check
   its raw stdout bytes via `od`/`xxd`):
   - `out = b"\x41\x42"` → ELF outputs exactly bytes 0x41 0x42 (no trailing newline).
   - `out = b"\x00\xff\x0a"` → 0x00 0xff 0x0a (NUL and high byte preserved).
   - `out = le32(5)` → 05 00 00 00 ; `out = le32(258)` → 02 01 00 00 ; `out = le64(1)`
     → 01 00 00 00 00 00 00 00.
   - `out = concat(b"\x41", le32(5))` → 41 05 00 00 00 (5 bytes).
   - Each matches what the B2 interpreter oracle says (length + byte_at per index).
3. UNCHANGED byte-identical: number-returning and text-returning programs' emitted
   ELFs (the streaming/number trampolines for non-bytes entries must not change —
   cmp vs an origin build).
4. Regression test (src/native.rs): the milestone bytes programs self-emitted, raw
   stdout asserted byte-for-byte.

## Slices (if too big for one pass)
- B3a: AstBytes literal + bytes entry (no newline) → `b"\x41\x42"` outputs AB. Founds it.
- B3b: le32/le64 streaming.
- B3c: bytes-concat streaming (+ nested — the elf_program_src shape).
Land B3a first (observable, fixes the empty-output finding); B3b/B3c follow.

## Honest scope
B3 = the emitter emits bytes, reusing the streaming path (+ no-newline entry + the
new le32/le64 encode). After B3, the self-hosted emitter can emit `b"..."`/`le32`/
`le64`/bytes-concat correctly — its own construction material. B4 then reorders
elf_program_src first, self-emits it, and asserts the self-emitted emitter's output
is byte-identical to the reference on the same input: the semantic fixed point.
