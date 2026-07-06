# Streaming codegen — compiled text-returning rules write bytes in order

## Context
Concat slice 1 gave the interpreter ropes (print_chain evaluable, byte-exact
oracle). Compiled, a text-returning main SIGSEGVs (concat falls through to the
real-call path). This slice is the self-hosted mirror of verbosec's OWN solution
to the same problem — docs/emitter-streaming-design.md: **never materialize; the
rule walks its result and writes bytes to fd 1 in order; _start writes only the
trailing newline.** No rope at runtime, no buffer, no heap.

## Design

### Mode decision — texty rules (syntactic, transitive depth-1)
A rule STREAMS iff its result is text-shaped: AstStr, a `concat(...)` call, an
if/match whose arms are texty, or a call to a rule whose result is DIRECTLY texty
(one level of transitivity — covers main → print_expr; deeper chains refused with
a breadcrumb, honest v1 bound). Helper `rule_is_texty(rd, prog)` + `ast_is_texty`.
Mirrors verbosec's whole-SCC WRITER-mode decision made once in compile_native_code.

### WRITER-mode proc emission (new x86 walk beside x86_node)
A texty rule's proc body is emitted by `x86_stream_node` (a SIBLING of x86_node,
writing instead of pushing):
- `AstStr(start, len)` → inline `write(1, src_base + start + 1, len - 2)` syscall
  (quotes stripped; src_base = the slice-2 embedded-source imm64 — the embedding
  now also triggers when a texty rule exists).
- `concat(args...)` (span_is_concat) → stream each arg in order. Text-typed args
  recurse; NUMBER-typed args → eval via x86_node (pushes) → pop rax → `call itoa_proc`.
- `AstVar` of a text param (packed span) → x86_node eval → pop → unpack
  (shr/mask) → write(1, src_base + start, len).
- if/match → same dispatch shapes as x86_node (cond/scrutinee evaluated via
  x86_node — they're numbers), arms streamed via x86_stream_node.
- call to a texty rule → `call` its proc (the callee streams; returns dummy).
- anything number-typed at a text position (a bare number result arm like
  `concat(v)`'s v) → x86_node eval + itoa_proc call.
Streamed procs keep the stack discipline: after streaming, `push 0` (dummy) so
the proc epilogue (`pop rax; ret`) is unchanged. Callers of texty rules in tail
text position discard the dummy.

### itoa_proc — one shared callable
Emitted ONCE when any texty rule exists (before the rule procs; jmp-over like the
callable layout): rax = value → decimal bytes (negative: '-') → write(1, buf, n).
Duplicate the trampoline's itoa logic as a proc; do NOT refactor the trampoline
itself (byte identity for number programs).

### Trampoline
If the ENTRY rule is texty: `call entry ; <write "\n">` — skip the itoa print
entirely (the walk already wrote the bytes). Number entries keep the existing
trampoline byte-for-byte.

### v1 restrictions (verbosec's own, verbatim)
- Texty-rule calls allowed ONLY in tail text positions. In conds / scrutinees /
  number positions (e.g. `length(print_expr(...))` compiled) → refuse with a
  breadcrumb naming this slice (the INTERPRETER covers rope probing — that split
  is the design: eval probes, streaming prints).
- Lets in texty rules: number lets fine (x86_node path); text lets refused v1.
- code_size must mirror x86_stream_node EXACTLY (the drift edge; syscall
  sequences are fixed-length, itoa calls are `call rel32`).

## Gate (CLEAN disk; the ELF's stdout compared against the ORACLE string)
1. vexprparse verifies; suite green (currently 434 + 1 ignored) + a new test.
2. **MILESTONE — compiled print_chain**: `main: out = print_expr(build_chain(
   Seed { n: 3 }))` → the ELF prints **"3+2+1+0"** (+ newline) — the rope oracle's
   byte probes (51/43/48) said exactly this string. Also n=0 → "0"; n=5 →
   "5+4+3+2+1+0".
3. Basics: `out = concat("ab", "cd")` → "abcd"; `out = concat("n=", 42)` → "n=42";
   `out = concat("t=", 0 - 7)` → "t=-7"; `out = "lit"` (texty main, plain literal)
   → "lit"; a texty rule with a text PARAM streamed through.
4. Refusals: `out = length(print_expr(...))` compiled → clear breadcrumb (not
   int3/crash); the same program still EVALS to 7 (the split holds).
5. Byte-identity: every number-entry program (all existing milestones) emits
   cmp-identical ELFs (itoa_proc + embedding only appear when a texty rule exists).
6. Regression test: print_chain "3+2+1+0" + basics + the refusal + byte-identity.

## Honest scope
The self-hosted emitter learns verbosec's streaming lowering: text output without
materialization. Deferred: text lets in texty rules, deep texty-call transitivity,
texty calls in non-tail positions (eval covers probing), rope-returning ABI (none —
that's the point). After this, the self-hosted compiler COMPILES the print_chain
family — the emitters tier — and the remaining tiers are Result + collections.
