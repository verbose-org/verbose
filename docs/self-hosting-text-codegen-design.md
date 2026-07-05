# Text values arc — slice 2: text CODEGEN (packed spans + embedded source)

## Context
Slice 1 (interpreter oracle): a text value = a span (start, len) into the interpreted
source; byte_at/length/substring span-preserving; word_length → 5. Slice 2 makes the
EMITTED ELF match that oracle — the compiler compiles scanners.

## Design — two moves, both compile-time-constant

### 1. A compiled text value = ONE packed i64: `start * 2^32 + len`
Both fit 32 bits (argv source ≤ 128 KB). A packed span is a Number → it flows through
the stack machine, arena slots, call ABI, match binders — ALL FREE (the same argument
as "a record value is its arena index"). Unpack: `start = v >> 32`, `len = v & 0xFFFF_FFFF`
(`mov eax, eax` zeroes the upper half — len extraction is 2 bytes).

### 2. The emitted ELF EMBEDS the interpreted source
The ELF has a fixed base 0x400000 and ONE PT_LOAD mapping the whole file. Append the
source bytes at the FILE END (after all procs; nothing falls through — same
jmp-over-data spirit, they're just never executed), extending `fsz` so they're mapped.
Then `src_base = 0x400000 + <pre-append fsz>` — a COMPILE-TIME imm64. No register
reservation, no prologue. Payoff: compiled spans are NUMERICALLY IDENTICAL to
interpreter spans (same start/len into the same bytes) — the oracle equivalence is
exact by construction. Gate embedding on `program_uses_text` (AstStr or a primitive
call appears) so text-free programs stay BYTE-IDENTICAL.

### Emits (x86_node; dispatch in the AstCall arm reusing slice 1's span_is_* helpers)
- `AstStr(start, len)` → push packed `((start+1) << 32) | (len-2)` (quotes stripped,
  SAME numbers as eval's VText) — mov rax, imm64; push rax.
- `length(t)`: emit t; pop rax; `mov eax, eax` (len); push rax.
- `byte_at(t, i)`: emit t, emit i; pop rcx (i), pop rax (t); split: rdx = len
  (`mov edx, eax` — careful: mov edx,eax copies low32 = len), `shr rax, 32` (start);
  bounds `cmp rcx, rdx ; jae OOB` (unsigned covers negative i); hit: `mov rdx,
  imm64(src_base) ; movzx eax, byte [rdx + rax + rcx]` (SIB base+index needs start
  in one reg: `add rax, rcx ; movzx eax, byte [rdx + rax]`); push; OOB: push 0 —
  ORACLE PARITY (eval returns VNum 0, never aborts). Small fwd jumps, fixed length.
- `substring(t, a, b)`: emit three; pop b (rcx), a (rdx), t (rax); split start/len;
  bounds `a <= b && b <= len` else push 0; hit: `new = ((start + a) << 32) | (b - a)`;
  push. Fixed length.
Each primitive's byte length is CONSTANT → code_size arms are `sum(arg sizes) + K`
(same discipline as AstVariant/AstField; the resolvability/dispatch condition must be
the VERBATIM same expression in emit and code_size — the drift edge).
Non-primitive AstCall falls through to the existing call emit unchanged.

### What needs NO code (verify, don't build)
Text through record fields / params / recursion / match binders: a packed span is a
Number. The slice-1 milestone program compiles as-is once AstStr + primitives emit.

## Gate (CLEAN disk — emitted ELFs run, EACH == the slice-1 eval_main oracle)
1. vexprparse verifies; suite green (currently 428 + 1 ignored) + a new test.
2. **MILESTONE — the COMPILER compiles a scanner**:
   - `word_length(Sc { src: "hello world", pos: 0 })` → ELF prints **5** (== oracle).
   - `length("hello")` → 5; `byte_at("abc", 1)` → 98;
     `length(substring("hello world", 6, 11))` → 5;
     `byte_at(substring("hello world", 6, 11), 0)` → 119.
   - OOB `byte_at("abc", 99)` → **0**, exit 0 (parity with the oracle's defensive 0).
   - BYTE-IDENTICAL for text-free programs: records 49/15/7, variant list-sum 6/15,
     scalar 5 (embedding + emits gated on program_uses_text).
3. Regression test (src/native.rs): the scanner→5 + compositions + OOB→0 compiled,
   each cross-checked vs eval_main, + text-free byte-identity.

## Honest scope
Slice 2 completes text for the scanner half: the compiler emits byte_at/length/
substring over packed spans of the embedded source. Deferred: concat/text heap
(refused for spans), runtime text INPUT (argv text into the entry — the closed-main
convention has none; a driver-level slice if ever needed), text equality (byte loops
work). After slice 2, compiled ELFs and the interpreter agree on the full scanner
value model: numbers, variants, records, text.
