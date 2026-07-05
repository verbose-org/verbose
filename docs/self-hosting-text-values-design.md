# Text values arc — slice 1: VText spans + scanner primitives (interpreter)

## The model (why this is cheap)
Strings were deferred as `VNum 0` (R6c). The self-source's text usage is scanner-
shaped: `source : text` fields, `byte_at(source, pos)`, `length(source)`,
`substring(...)` — ALL span-preserving. And in the self-hosted interpreter, a target
text value IS a span into the interpreted source: `AstStr(start, len)` is already a
span of `ea.src`. So:

- **`VText of (start : number, len : number)`** — a new Value variant; a span into
  `ea.src`. No heap, no copying. `concat` (fresh text) is explicitly OUT — it breaks
  the span model and needs a text heap; the scanner bricks don't use it (they use
  byte loops / spans), so defer.
- The target's primitives are implemented WITH THE HOST'S OWN primitives on
  `ea.src`: target `byte_at(t, i)` = host `byte_at(ec.src, t.start + i)`. The
  self-hosted interpreter eats its own dogfood.

## Design

### 1. VText + AstStr
Add `VText of (start, len)` to the Value group. `AstStr(start, len)` →
`VText { start, len }` — CHECK whether the lexer's string span includes the quotes;
if so, strip (`start+1, len-2`). Verify empirically (shape/e2e), don't assume.

### 2. Primitive dispatch — in the AstCall ARM, not eval_call
`eval_call`'s lets are EAGER (`let callee = find_rule(...)` runs before any `out` if
— the eager-let trap, again). Dispatch primitives in `eval_ast_env`'s `AstCall` arm
BEFORE delegating to eval_call: compare the callee span against `byte_at` /
`length` / `substring` (span_is_* style byte compares, mirroring span_is_rule):
- `byte_at(t, i)`: eval both args; t must be VText, i VNum; if `0 <= i < t.len` →
  `VNum { host byte_at(src, t.start + i) }`, else defensive `VNum 0` (do NOT let the
  host's fail-closed byte_at abort the interpreter on a target program's bug —
  guard first).
- `length(t)` → `VNum { t.len }`.
- `substring(t, a, b)`: if `0 <= a <= b <= t.len` → `VText { t.start + a, b - a }`,
  else defensive `VNum 0`.
Non-primitive names fall through to eval_call unchanged. A user rule named
`byte_at` would be shadowed by the primitive — same as verbosec (primitives are
reserved); note it.

### 3. vnum_of
`VText` → 0 in `vnum_of` (same convention as VData; drivers return numbers).

### What needs NO code
Text fields in records (`src : text`): the VData payload already holds any Value —
a VText flows through construction, field access (slice 2's vlist_nth), params,
recursion. Verify, don't build.

## Gate (CLEAN disk, eval_main, programs in files)
1. vexprparse verifies; suite green (currently 427 + 1 ignored) + a new test.
2. **MILESTONE — the interpreter runs a real scanner** (scan_word's logic):
   - `word_length(Sc { src: "hello world", pos: 0 })` where word_length recurses
     with `byte_at(s.src, s.pos)` in [97,122] and `pos + 1` → **5**.
   - `length("hello")` → 5; `byte_at("abc", 1)` → 98; `substring` composition:
     `length(substring("hello world", 6, 11))` → 5 and
     `byte_at(substring("hello world", 6, 11), 0)` → 119 ('w') — spans compose.
   - text through a record field + recursion (the Sc case covers it).
   - defensive: `byte_at("abc", 99)` → 0 (no interpreter abort).
   - UNCHANGED: records 49/15/7 evals, variant list-sum 6/15, scalar — identical.
3. Regression test (src/native.rs): word_length→5, the span compositions, defensive
   OOB, + records/variants unchanged, in one test.

## Honest scope
Slice 1 = text values in the INTERPRETER (the oracle), span model, three scanner
primitives. This is what lets the self-hosted interpreter RUN the self-source's
scanner bricks (word_length is scan_word.verbose's logic verbatim). DEFERRED:
CODEGEN slice (a compiled text value = packed (offset,len) into a data region of
literals embedded in the emitted ELF — jmp-over-data like verbosec; needs the pack/
unpack + embedded-literal design, its own slice with this oracle to lock against);
`concat`/text heap; text equality (the self-source uses name_eq-style byte loops,
which now WORK via byte_at). After slice 1, the interpreter's value model is
complete for the scanner half of self-hosting.
