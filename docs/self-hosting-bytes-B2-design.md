# Bytes tier B2 — interpreter bytes value model (the oracle)

## Model (reuses the concat rope; on-demand, no materialization)
A bytes value is a rope, exactly like the concat arc's VConcat, with two new LEAVES
(the rest of the Value group — VNum/VData/VText/VConcat — is unchanged, and VConcat
is REUSED for bytes-concat, its leaves just being bytes leaves):

- `VBLit of (start, len)` — a span of a `b"..."` literal's CONTENT (the escape-
  encoded source chars). Decoded ON DEMAND: `\xNN` (2 hex after `\x`) → 1 byte;
  `\n`→10 `\t`→9 `\\`→92 `\"`→34; any other char → its own byte. length = count of
  DECODED bytes; byte_at(i) = the i-th decoded byte (escape-aware scan). Zero-copy,
  like VText but with escape-aware indexing.
- `VBLe of (value, width)` — `le32`/`le64`: length = width (4/8); byte_at(i) =
  (value >> (8*i)) & 255 (little-endian).

## Changes
1. **Value variants**: add `VBLit`, `VBLe`. RIPPLE: every `match` over Value gets the
   two arms (vnum_of→0, vtext_is→0, vdata_tag→-1, vdata_payload→VLNil, vlength/
   vbyte_at→real, others→leaf/defensive). ~10-12 sites (fewer than the Ast ripple).
2. **eval AstBytes** (currently VNum 0 stub) → `VBLit { start, len }`.
3. **le32/le64 dispatch**: new `span_is_le32` / `span_is_le64` (mirror span_is_concat,
   4/4 bytes "le32"/"le64") in eval_ast_env's AstCall arm, before eval_call → new
   `prim_le32` / `prim_le64`: eval the single arg → VBLe { value: n, width: 4|8 }.
4. **vlength / vbyte_at** learn the leaves:
   - vlength: VBLit → `bytelit_decoded_len(src, start, len)` (escape-aware count);
     VBLe → width; VConcat → left+right (unchanged).
   - vbyte_at: VBLit → `bytelit_byte_at(src, start, len, i)` (escape-aware: scan to
     the i-th decoded byte, decode it); VBLe → (value >> 8*i) & 255; VConcat →
     descend (unchanged). Guard i in range → else 0.
   Helpers `bytelit_decoded_len` + `bytelit_byte_at` (+ a hex-nibble decoder) —
   recursion over the content span, bound + breadcrumb like the other scanners.
5. bytes-concat needs NO new code: prim_concat already builds VConcat; with bytes-leaf
   args the rope carries them, and vlength/vbyte_at descend. Verify.

## Gate (CLEAN disk, eval_main, programs in files)
1. vexprparse verifies; suite green (currently 444 + 1 ignored) + a new B2 test.
2. **MILESTONE — the bytes oracle** (via length/byte_at, which return numbers, so no
   driver change):
   - `length(b"\x41\x42")` → 2; `byte_at(b"\x41\x42", 0)` → 65; `byte_at(..,1)` → 66.
   - `length(b"")` → 0; `length(b"\x0a")` → 1; `byte_at(b"\x0a",0)` → 10.
   - `length(le32(5))` → 4; byte_at 0..3 → 5,0,0,0. `length(le64(1))` → 8; byte 0 → 1.
   - `le32(258)` → bytes 2,1,0,0 (0x0102 LE); a multi-byte value proves LE order.
   - bytes-concat: `length(concat(b"\x41", le32(5)))` → 5; byte_at 0 → 65, 1 → 5;
     nested concat depth ≥ 3 → correct.
   - non-\x escapes: `length(b"\n\t")` → 2; byte_at 0 → 10, 1 → 9.
3. UNCHANGED: text/records/variant/scalar milestones (VBLit/VBLe are additive; VText
   and the concat rope for TEXT unchanged).
4. Regression test (src/native.rs, mirror concat_slice1): the oracle probes above.

## Honest scope
B2 = bytes VALUES in the interpreter (the oracle for B3 codegen). Escape decoding is
the fiddly part (hex nibbles, the closed escape set) — test byte-by-byte. Reuses the
concat rope; VBLit is span-based (on-demand decode) so no bytes arena needed. After
B2: the interpreter evaluates the emitter's construction material; B3 emits it,
locked to this oracle; B4 is the semantic fixed point.
