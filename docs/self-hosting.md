# Self-Hosting — a Verbose compiler written in Verbose, that compiles and verifies itself

This document capitalizes the "self-hosting" arc: incremental bricks that built a complete
compiler — front end, four-surface verifier, interpreter, and x86-64 back end — **entirely
in Verbose**, living in [`examples/vexprparse.verbose`](../examples/vexprparse.verbose)
(268 concepts, 608 rules, 21,940 lines, 1,145,526 bytes). Every number and command output
below was captured by running the code in this worktree; nothing is predicted or estimated.

```
$ cargo run --quiet --release -- examples/vexprparse.verbose 2>&1 | tail -1
verified: 268 concept(s), 608 rule(s); all proofs check out

$ wc -l -c examples/vexprparse.verbose
21940 1145526 examples/vexprparse.verbose
```

The headline, stated plainly because it is now true and measured (§12, §16):

- **The compiler compiles itself.** `elf_program_src`, compiled by verbosec's Rust backend
  (gen0), reads its own full 1.1 MB source and emits gen1 — a 1,754,029-byte ELF that IS
  the self-compiled compiler. gen1 reads the same source and emits gen2. **gen1 == gen2
  byte-for-byte** — the two-generation bootstrap fixed point (PR #102), CI-gated on every
  PR since #104.
- **The compiler verifies itself before emitting.** gen1 is a *verifying* compiler:
  parse → VERIFY → emit, fail-closed. Fed its own source it scores 0 on the aggregate of
  all four self-hosted proof surfaces (lints+types, purity, termination) and emits; fed a
  program with undeclared proofs it exits 1 with **zero output bytes** (PR #119). Both
  demonstrated live in §12 and §16.

## 1. What this is (and isn't)

The arc covers **two grammars**, and the earlier revisions of this document kept them
strictly apart: "the closed vexpr grammar — compiled end-to-end" vs "the real Verbose
grammar — parsed and checked, **not yet compiled**". That second framing is now FALSE, and
the way it became false is the story of §§10–16.

**The closed grammar — what the self-hosted back end COMPILES.** Rules with typed
parameters and a declared return type, `let`-bindings, arithmetic / boolean / comparison
expressions, `if/then/else`, calls, recursion — plus, arc by arc: `concept` declarations
with fields, `concept_group` declarations with variants, `Name::Variant { ... }` and
bare-record `Name { ... }` construction, field access (including chained `o.inner.b`),
`match` with binder-scoped arms, the text primitives `length` / `byte_at` / `substring`,
**text concat ropes and streaming codegen** (the emitters tier, #87), **the bytes tier**
(`b"..."` literals, `le32`/`le64`, bytes-concat — the compiler's own output language,
#97–#101), **`Ok`/`Err`/`match_result` and the top-level `Result(number, text)` CLI ABI**
(#106–#108), and **collections** (`sum`/`count`/`fold`/`all`/`any`/`map`/`filter` over
scalar and record elements, plus the streamed text fold, #109–#115).

**The real Verbose grammar — what the self-hosted pipeline READS and VERIFIES.** Full-line
comments, `@intention`/`@source` attributes, `concept_group ... [max_depth, max_nodes]`
headers, nested concepts with `variants:` blocks, range-annotated fields, `input:` /
`output:` blocks, and the whole `proofs:` surface (purity `reads`/`calls` lists,
`termination` with `bound` / `structural` / `decreasing` / `increasing`). The checker
verifies those declarations — the same four proof surfaces verbosec checks: undeclared
**calls**, undeclared **reads**, **termination** argument patterns at self-recursive call
sites, and declared-**type** conformance.

**The pivot that closed the arc:** the two grammars converged enough that
`vexprparse.verbose` — written in the real grammar, `input:` blocks and `proofs:` blocks
and `concept_group` and all — lies **inside** what its own back end compiles. The
`input:`-block bridge (#86) made `input:`-declared rules callable; the bytes tier gave the
emitter its own construction material; the channels (#91/#93/#96) let the compiled binary
receive its full source at runtime. That intersection is the definition of self-hosting,
and the fixed point (§12) is its proof.

What it is still NOT: verbosec. The self-hosted checker is a **subset** of verbosec's
verifier (the four surfaces, with documented blind spots — §16); the compileable grammar
excludes verbosec features the self-source doesn't use (reactions, services, `read`/
`fetch`, modules — §17). The claim is precise: a compiler for a substantial, growing
subset of Verbose, written in Verbose, verified by verbosec, that verifies and reproduces
its entire self byte-for-byte.

The **dogfooding** is real: the whole pipeline is itself **compiled by verbosec to native
x86-64**. A driver like `check_program` is a statically-linked ELF, produced by the real
compiler from the `.verbose` file, that lexes/parses/analyzes a program passed on `argv`
(or, since the stdin channels, fed whole on stdin). The front end is not interpreted by a
host language at runtime — it is machine code. And gen1 is one step further: machine code
emitted by machine code that was emitted from this source.

## 2. The pipeline

Every stage is a set of Verbose **rules** operating on an arena-allocated AST. The arena is
a single `concept_group VExpr [max_depth: 4096, max_nodes: 6000000]` (line 141 of the
`.verbose` file): tokens, AST nodes, environments, values, and diagnostics are all variants
in that one group, linked by arena indices (cons-lists by index, not pointers). The
6,000,000-node bound is the **gen0** (Rust-backend) arena bound; binaries emitted by the
self-hosted back end mmap a **16 GiB MAP_NORESERVE** arena instead — only pages actually
touched cost physical RAM (the full self-compile touches ~10.4 GB at peak, §12).

| Stage | Rules | What it does |
|---|---|---|
| **Tokenizer** | `begin_tokenize`, `next_token`, the column-stack INDENT/DEDENT helpers | Scans the source bytes into a `TokenList` cons-list; emits `Newline` and, via a column stack, `Indent`/`Dedent` tokens. O(N) since the capacity arc (the eager-let `string_run` fix). |
| **Parser — expressions** | `parse_or` → … → `parse_primary` (precedence ladder), `parse_match`, `variant_build`, `parse_block` | Recursive-descent over the token list into the `Ast` arena, navigating by cons cell (O(1) cursor). `parse_primary` handles identifiers, calls, field access, literals (including `b"..."` bytes literals), parens, `if/then/else`, variant/record construction, `match` with binder lists, `Ok`/`Err`/`match_result`, and the reduction keywords (`sum`/`count`/`fold`/`all`/`any`/`map`/`filter`). |
| **Parser — real grammar** | `parse_program`, `parse_rule_decl_pos`, `parse_concepts`, `parse_concept_decl_pos`, `parse_variants`, `parse_fields`, `parse_proofs`, `parse_name_list` | Parses a whole program into `ProgramAst { concepts, rules }`: skips comments/@attrs, descends `concept_group` blocks, captures variants and range-annotated fields, and parses each rule's `input:`/`output:` types and the full `proofs:` block into `RuleDecl` (R1–R5). |
| **5 lints/analyses** | `lint_program`, `lint_callees`, `lint_arity`, `type_check`, `argtype_check` | (1) undefined-variable + use-before-def; (2) undefined-callee; (3) call arity; (4) expression type errors; (5) call-site argument-type mismatch. Since R6a all five walk `match`/variant bodies with binder scoping; since V1b (§16) the models handle `input:`-block scoping, a primitive allowlist, and real field-selection typing. |
| **Type checker** | `type_of_env`, `resolve_type`, `variant_payload`, `arms_type`, `bin_type`, `call_result_type`, `tcheck_rule`, `body_type` | Concept-aware since R6b: the lattice carries **concept codes** (`1000 + concept_index`) next to `{number, bool, text, bytes}`, `resolve_type` maps type-name spans to codes, match-arm binders get their concept types from the variant payload, and arms must agree. SOUND: `h + 1` where `h : Token` is an ERROR. |
| **Proof verifier** | `count_purity_errors`, `undeclared_calls`, `undeclared_reads`, `count_term_errors`, `term_errors`, `term_arg_ok` | The self-hosted pendant of verbosec's zero-trust verifier: flags calls to resolvable rules absent from the declared `calls:` list, input reads absent from `reads:` (locals and primitives exempt), and termination violations — `decreasing`/`increasing`/`structural` checked against the actual argument pattern at every self-recursive call site (R6d + the termination brick). |
| **Verify gate** | `verify_errors`, `abort_if` | (New, §16.) The V2 aggregate: one tokenize+parse, then lints+types + purity + termination summed into one number. `elf_program_src`'s body leads with `abort_if(verrs)` — nonzero → sys_exit(1) with zero output bytes; zero → streams nothing. parse → VERIFY → emit, fail-closed. |
| **Interpreter** | `eval_main`, `eval_ast_env`, `eval_call`, `eval_match`, `eval_match_result`, `eval_reduce`, `eval_fold`, `eval_mapfilter`, `variant_tag`, `bind_params`, `build_env`, `prim_byte_at` / `prim_length` / `prim_substring` / `prim_concat` / `prim_le32` / `prim_le64` | A recursive tree-walking evaluator over a runtime **Value model**: `VNum` (numbers), `VData { tag, payload }` (variant, record, Result, and collection values), `VText (start, len)` (text spans), plus a bytes value model (the B2 oracle). Runs match/variant, records, text scanners, Result dispatch, and the collection reductions. |
| **Diagnostics** | `find_diag`, `prog_diags`, `all_diags_count`, `nth_diag_at_*`, the `*_span_rule` finders | A structured, **located** report: which rule, which category, and — for undefined-var / undefined-callee / arity — the byte span of the offending token. |
| **Source emitter** | `print_source`, `print_expr`, `print_program_src`, … | A **streaming** pretty-printer; round-trips complete programs to the same analyses and the same evaluation. |
| **Stack-IL lowering** | `lower_expr_src`, `lower_program_src`, … | Lowers expressions/programs to a postfix stack-machine IL with `proc … ret`, `call`, `load`/`store`, structured `if/else/endif`. |
| **x86-64 generator** | `x86_expr`, `x86_node`, `x86_stream_node`, `code_size_node`, `code_size_stream_node`, `x86_program`, `elf_program_src`, `static_concept_of`, `program_uses_arena`, `program_uses_text`, `program_uses_result` | The **back end**: emits a standalone runnable ELF. Beyond the b1–b8 scalar grammar it emits an **mmap'd node arena** (base in `r15`, count in `r14`), `VariantConstruct` / `MatchVariant`, compile-time-resolved record field access, **text spans as packed i64** over the source embedded at the end of the emitted ELF, **bytes** (`b"..."`/`le32`/`le64`/bytes-concat, streamed), **Result** nodes + the top-level Result trampoline, **collection loops** (the first runtime-conditional loops it emits), `arena_scope` reclaim marks, and the `abort_if` verification gate. |

## 3. A live session — the closed vexpr grammar

All outputs below are captured from the native drivers (the toy programs live in files;
each run is `driver "$(cat prog.vx)" 0`). Each driver is one rule compiled native, e.g.:

```
$ cargo run --release -- examples/vexprparse.verbose --native /tmp/check_program --run check_program
```

Native binary sizes (one ELF per rule, statically linked, no libc) — a snapshot at the
**475-rule era**; each grows as rules are added. For scale today (608 rules):
`elf_program_src` compiles at 323,814 B (`--stdin-raw`, with the verify gate in the body)
and `verify_errors` at 164,578 B — both captured in this worktree.

| driver | bytes | driver | bytes |
|---|---|---|---|
| `check_program` | 96245 | `eval_main` | 87237 |
| `all_diags_count` | 108291 | `type_check` | 81102 |
| `nth_diag_at_rule` | 108497 | `body_type` | 79888 |
| `first_bad_rule` | 96004 | `count_purity_errors` | 72626 |
| `elf_program_src` | 105769 | `count_term_errors` | 74508 |
| `print_program_src` | 69235 | `lint_program` | 67052 |
| `lower_program_src` | 67477 | `count_rules` | 62190 |
| `undef_span_start_of` | 68355 | `count_concepts` | 66178 |
| `eval_expr` | 53638 | `shape` | 34726 |

Programs are passed on `argv` (the source as a single string; the second arg is an index
`n`, `0` unless indexing a `DiagList`).

### A — A clean program

```
SRC:  rule add(x : number, y : number)
        logic:
          out = x + y
      rule main
        logic:
          out = add(1, 2)

$ /tmp/check_program      "$SRC" 0   →  0
$ /tmp/all_diags_count    "$SRC" 0   →  0
$ /tmp/first_bad_category "$SRC" 0   →  0
$ /tmp/first_bad_rule     "$SRC" 0   →  9999
$ /tmp/count_rules        "$SRC" 0   →  2
$ /tmp/undef_span_start_of "$SRC" 0  →  9999
```

`9999` is the no-result sentinel (no bad rule, no undefined-variable span). `count_rules`
confirms the parser saw both declarations.

### B — Undefined variable, with a located span (category 1)

```
SRC:  rule f
        logic:
          out = foo + 1

$ /tmp/first_bad_category   "$SRC" 0   →  1
$ /tmp/undef_span_char      "$SRC" 0   →  102
$ /tmp/undef_span_len_of    "$SRC" 0   →  3
$ /tmp/undef_span_start_of  "$SRC" 0   →  26
$ /tmp/lint_program         "$SRC" 0   →  1
```

The undefined name is `foo`: byte offset `26` in the source, length `3`; the first byte's
ASCII code is `102` (`'f'`).

### C — Undefined callee, located (category 2)

```
SRC:  rule main
        logic:
          out = g(1)

$ /tmp/first_bad_category "$SRC" 0      →  2
$ /tmp/nth_diag_at_span_start "$SRC" 0  →  29   (the 'g' in "out = g(1)")
$ /tmp/nth_diag_at_span_len   "$SRC" 0  →  1
```

Categories 1 (undef var), 2 (callee), and 3 (arity) carry the offending token's span;
categories 4 (type) and 5 (arg-type) report `(rule, category)` only — `AstBin`/`AstNum`
carry no source span to point at.

### D — Wrong call arity (category 3)

```
SRC:  rule f(a)  … out = 1
      rule main  … out = f(1, 2)

$ /tmp/first_bad_category "$SRC" 0   →  3
```

### E — Type error in an expression (category 4)

```
SRC:  rule f  … out = 1 + (2 < 3)

$ /tmp/type_check         "$SRC" 0   →  1
$ /tmp/first_bad_category "$SRC" 0   →  4
```

### F — Call-site argument-type mismatch (category 5)

```
SRC:  rule g(b : bool)    … out = f(b)
      rule f(x : number)  … out = x

$ /tmp/first_bad_category "$SRC" 0   →  5
```

### G — The full report: a multi-error program

```
SRC:  rule f(a : number)
        logic:
          out = z + 1
      rule main
        logic:
          out = f(1 < 2, 3) + g(9)

$ /tmp/all_diags_count "$SRC" 0   →  4
$ /tmp/check_program   "$SRC" 0   →  4

$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 0   →  (rule 0, cat 1, span 38/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 1   →  (rule 1, cat 2, span 87/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 2   →  (rule 1, cat 3, span 73/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 3   →  (rule 1, cat 5, span 9999/0)
```

### H — The interpreter (scalar recursion)

```
$ /tmp/eval_main "<fact(5) program>" 0   →  120
$ /tmp/eval_main "<fib(10) program>" 0   →  55
```

`fact(5) = 120` and `fib(10) = 55` are computed by the Verbose-written evaluator, itself
compiled to native code — recursion in the *evaluated* program drives recursion in the
*evaluator's* own emitted `call`/`ret`.

### I — Diagnostic granularity contrast

Two undefined variables in one rule are two instances but one rule×category diagnostic:

```
SRC:  rule f  … out = x + y

$ /tmp/lint_program    "$SRC" 0   →  2
$ /tmp/all_diags_count "$SRC" 0   →  1
```

### J — The type system uses declared return types

```
# 1 + b() where b() : bool  →  number + bool, a type error:
$ /tmp/type_check "<rule b() : bool … out = 1 < 2 ; rule main … out = 1 + b()>" 0  →  1
# declared number, body is bool:
$ /tmp/type_check "<rule b() : number … out = 1 < 2>" 0                            →  1
# equality on bools and a text literal type-check clean:
$ /tmp/type_check "<rule f … out = (1 < 2) == (3 < 4)>" 0                          →  0
$ /tmp/type_check "<rule f … out = \"hello\">" 0                                   →  0
```

### K — The emitter: print, and round-trip

```
$ /tmp/print_source "1 + 2 * 3" 0               →  (1 + (2 * 3))
$ /tmp/print_source "if 1 < 2 then 3 else 4" 0  →  (if (1 < 2) then 3 else 4)

$ /tmp/print_program_src "<fact(5) program>" 0
rule main
  logic:
    out = fact(5)
rule fact(n : number)
  logic:
    out = (if (n == 0) then 1 else (n * fact((n - 1))))
```

Printing is a **normal form** (untyped param → `: number`, fully parenthesized, 2/4-space
indentation), and the round-trips hold, all re-verified:

```
$ OUT=$(print_program_src fact5) ; eval_main "$OUT"    →  120  == eval_main fact5
$ OQ=$(print_program_src quad)   ; check_program "$OQ" →  4    == check_program quad
$ print_program_src "$OUT"  ==  "$OUT"                 (byte-identical: idempotent)
```

### L — The stack IL

```
$ /tmp/lower_expr_src "1 + 2 * 3" 0   →  1 2 3 * +      (eval_expr agrees: 7)

$ /tmp/lower_program_src "<fact(5) program>" 0
proc main 
5 call fact ret
proc fact n 
load n 0 == if 1 else load n load n 1 - call fact * endif ret
```

### M — Machine code: the scalar back end (bricks b1–b8)

```
$ /tmp/elf_program_src "<fact(10) program>" 0 > /tmp/a.out ; chmod +x /tmp/a.out ; /tmp/a.out
3628800

$ file /tmp/a.out
ELF 64-bit LSB executable, x86-64, version 1 (SYSV), statically linked, no section header
$ od -An -tx1 /tmp/a.out | head -1
 7f 45 4c 46 02 01 01 00 00 00 00 00 00 00 00 00
```

The machine-code subset covers the FULL closed scalar grammar: `+`/`-`/`*`/`/`/`%`/unary-neg
(the `10 / 3` blob shows the unguarded `cqo; idiv` path — `48 99 48 f7 f9` — faulting on
`/0` exactly like the interpreter), the six comparisons, branchless `and`/`or`
(`5 and 0 → 0`, `0 or 7 → 1`), `if/then/else` with computed jump offsets, `let` bindings as
rbp frame slots (`sq(5) = 26`; a let inside a recursive proc: `fa(5) = 120`), parameter
loads, `call`/`ret`, and recursion. The `_start` trampoline itoa's the returned i64 (any
value, `-` for negatives) and `sys_write`s it to stdout.

## 4. The real-grammar arc — the parser reads its own source

The R1–R5 bricks pointed the self-hosted parser at the **real grammar of its own source**,
and the capacity arc made that physically possible.

**Capacity first (the walls, in the order they fell):**

- **The tokenizer was O(N²)** — not the parser. The eager-let `string_run` bound
  `str_content_len` on every `next_token` call, scanning to end-of-source for every
  non-string token. The fix guards the scan on the current byte being `"` (see
  [`docs/self-hosting-capacity-design.md`](self-hosting-capacity-design.md); the measured
  K=1600 tokenize run dropped 9.06 s → 9.5 ms).
- **The parser navigated by index** (drop-N-from-head per peek). The cursor rewrite made
  every peek O(1) by walking cons cells.
- **The arena was capped at 65,535 on-stack entries** — the real self-source needs
  millions of nodes. The off-stack **mmap arena** (a `src/` native slice) lifted the
  ceiling; `vexprparse.verbose` today declares `max_nodes: 6000000` for the gen0 arena,
  and self-emitted binaries reserve 16 GiB MAP_NORESERVE (§12).

Measured at the R2-era scale on real self-source chunks (native `count_rules`, i.e.
tokenize + full program parse + rule-list walk), the curve is linear:

```
59,769 bytes  →  count_rules = 18  in  6 ms
94,994 bytes  →  count_rules = 57  in 10 ms
128,660 bytes →  count_rules = 87  in 14 ms
```

**Structure (R2, R4):** the parser skips full-line comments and `@attribute` lines,
descends `concept_group` headers, parses nested concepts' `variants:` blocks and
range-annotated fields, and parses every rule past its `input:`/`output:`/`proofs:` blocks.
The pinned property is `count_rules == grep -c '^rule '` — 475 at the era this section was
captured, 608 today; the chunked-argv path was superseded by the stdin channels (#93/#96),
which feed the whole source in one read.

**Types and proofs (R3, R5):** `::`-variant construction, bare-record construction, and
`match` parse into real AST (fingerprinted by the `shape` driver — each construct has a
dedicated band):

```
$ /tmp/shape 'Token::Eof' 0                                        →  1000000000
$ /tmp/shape 'TokenList::Cons { head: x, tail: y }' 0              →  1000000200
$ /tmp/shape 'match x: Cons(h, t) => Token::Eof  Nil => Token::Eof' 0 → 2100000100
$ /tmp/shape 'Foo { a: x }' 0                                      →  1000000100
$ /tmp/shape 'Foo { }' 0                                           →  1000000000
$ /tmp/shape 'x' 0 → 100      'x.field' 0 → 1100      '1 + 2' 0 → 10
```

and the proofs surface parses into `RuleDecl` — on a real-shaped `tokenize` rule declaring
`reads : [s.source, s]`, `calls : [skip_spaces, token_end, next_token, tokenize]`,
`bound : 256`:

```
$ /tmp/count_reads      "$SRC" 0   →  2
$ /tmp/count_calls      "$SRC" 0   →  4
$ /tmp/rule_term_bound  "$SRC" 0   →  256
$ /tmp/rule_term_kind   "$SRC" 0   →  0      (bound-only)
# and on a rule declaring `decreasing : a`:
$ /tmp/rule_term_kind   "$SRC" 0   →  2      (decreasing)
```

This is the **verification surface**: everything the checker in §5 consumes is parsed by
the self-hosted parser from real proof syntax.

## 5. The checker — all four proof surfaces, self-hosted

The R6a/R6b/R6d + termination bricks turned the front end's analyses into a real checker
over the widened grammar. All outputs captured from the native drivers. (A later arc — V1a
and V1b, §16 — pointed this checker at the **full self-source** and found five real bugs
in the checker itself: one exponential walk and four model gaps. The examples below
survived that audit unchanged.)

**R6a — the lints walk match/variant, with binder scoping** (`lint_program`):

```
rule f … out = match x: Cons(h, t) => h + zzz  Nil => 0   →  1   (only zzz; the scrutinee
                                                                  x is a let, h/t are arm
                                                                  binders — not flagged)
rule f … out = match x: Cons(h, t) => h  Nil => 0         →  0   (binders scope)
rule f … out = Token::Num { value: www }                  →  1   (the field VALUE is a var;
                                                                  the type names are not)
```

**R6b — a concept-aware, SOUND type checker** (`body_type` / `type_check`). Concept
identity survives parsing (type-name spans, resolved to concept codes `1000 +
concept_index`), match-arm binders get their concept types from the variant payload, and
the checker REJECTS treating a variant value as a number:

```
out = Token::Eof                                       →  body_type 1000   (a concept code,
                                                           not number/bool/text)
out = match toks: Cons(h, t) => h  Nil => Token::Eof   →  body_type 1000   (h : Token flows
                                                           through the arm)
out = match toks: Cons(h, t) => h + 1  Nil => 0        →  body_type 3      (ERROR — the
                                                           soundness case: h is a Token,
                                                           h + 1 is rejected)
output: out : Token, body = match … => Token::Eof …    →  type_check 0
output: out : Token, body = 5                          →  type_check 1
```

**R6d — the purity verifier** (`count_purity_errors`). A call whose callee resolves via
`find_rule` but is absent from the declared `calls:` list is flagged; primitives don't
resolve, so they're exempt; self-recursion must be declared; an input read absent from
`reads:` is flagged; locals are exempt:

```
out = helper(1)   with calls : []          →  1        with calls : [helper]  →  0
out = r(n)        with calls : []          →  1        with calls : [r]       →  0
out = s           with reads : []          →  1        with reads : [s]       →  0
rule scan … reads : [s.pos, s.len, s.source]  calls : [scan]   →  0   (clean, real-shaped)
```

**Termination — the last proof surface** (`count_term_errors`). Each self-recursive call
site's argument pattern is checked against the declared termination kind: `decreasing : f`
requires the record argument's field `f` to be `input.f - k` (k ≥ 1); `increasing : f` the
symmetric `+ k`; `structural : p` requires the argument to be a binder from a match arm
whose scrutinee is exactly the input; bound-only rules are never flagged (verbosec parity —
the bound is a declared budget, not a proof):

```
decreasing : n    go(St { n: s.n - 1 })  →  0     { n: s.n } → 1     { n: s.n + 1 } → 1
increasing : pos  scan(St2 { pos: s.pos + 1 })  →  0         { pos: s.pos - 1 } → 1
structural : xs   … Cons(h, t) => h + lsum(t) …  →  0        … h + lsum(xs) …   → 1
bound-only        recursive fact with bound : 100 →  0       (not flagged; kind 0)
```

With that, the self-hosted checker verifies **all four proof surfaces** — undeclared calls,
undeclared reads, termination patterns, and declared types — the same declarations
verbosec's own zero-trust verifier enforces. §16 is where this checker gets pointed at the
largest program available — its own source — and becomes the gate of the bootstrap.

## 6. The interpreter — a runtime Value model

R6c replaced the numbers-only evaluator with a **Value model**: `VNum` for numbers,
`VData { tag, payload }` for variant AND record values (tag = `concept_index*256 +
variant_index`; a record is a single-variant concept whose tag has `variant_index` 0), and
`VText (start, len)` for text — a span into the evaluated source itself. Later tiers
extended `VData` with **reserved tags** far above any real `cidx*256+vidx`: `Ok`/`Err`
values at 15360000/15360001 and collections at 15360002 (§14, §15) — zero new `Value`
variants, the payload/binder machinery reused wholesale. All captured via
`/tmp/eval_main "$(cat prog.vx)" 0`:

```
# match on a constructed variant:
out = match build(): Cons(h, t) => h  Nil => 0,  build() = Lst::Cons { head: 5, tail: Lst::Nil }
                                                                    →  5
# recursive build + recursive match walk (list-sum):
out = lsum(build(3))   where build(n) = if n == 0 then Lst::Nil
                       else Lst::Cons { head: n, tail: build(n - 1) }
      and lsum(xs) = match xs: Cons(h, t) => h + lsum(t)  Nil => 0  →  6
out = lsum(build(5))                                                →  15

# records: construction + field access (+ chained access through a nested record):
let s = Foo { a: 42, b: 7 }     out = s.a → 42    s.b → 7    s.a + s.b → 49
out = get_a(Foo { a: 42, b: 7 })  where get_a(s : Foo) = s.a + s.b   →  49
let o = Outer { inner: Foo { a: 42, b: 7 } }      out = o.inner.b    →  7
recursion carrying a record: go(St { n: 5, acc: 0 })                 →  15

# text spans + the scanner primitives, implemented WITH the host's own primitives:
out = length("hello")                             →  5
out = byte_at("abc", 1)                           →  98
out = length(substring("hello world", 6, 11))     →  5
out = byte_at(substring("hello world", 6, 11), 0) →  119
out = byte_at("abc", 99)                          →  0    (defensive OOB, no abort)

# the milestone: a real recursive text scanner —
out = word_length(Sc { src: "hello world", pos: 0 })
  where word_length(s : Sc) = if s.pos >= length(s.src) then 0
        else if byte_at(s.src, s.pos) >= 97 then
          (if byte_at(s.src, s.pos) <= 122 then
             1 + word_length(Sc { src: s.src, pos: s.pos + 1 }) else 0)
        else 0                                    →  5
```

Note the primitive dispatch lives in the evaluator's `AstCall` arm, BEFORE `eval_call` —
`eval_call`'s lets run `find_rule` unconditionally (the eager-let trap,
[`docs/design-lessons.md`](design-lessons.md)), so a primitive call must never reach it.

## 7. The back end widens — variants, records, text: compiled == oracle

The R7 + records-codegen + text-codegen bricks taught the self-hosted x86-64 generator the
same constructs, under a strict **oracle discipline**: every emitted ELF must print exactly
what `eval_main` computes, and programs not using a feature must compile byte-identically.

How each construct lowers (design docs: [`r7`](self-hosting-r7-design.md),
[`records-astfield-codegen`](self-hosting-records-astfield-codegen-design.md),
[`records-input-abi`](self-hosting-records-input-abi-design.md),
[`text-codegen`](self-hosting-text-codegen-design.md)):

- **Arena:** the emitted `_start` `mmap`s a node arena; base lives in `r15`, count in
  `r14` (both callee-saved; procs never touch them). A variant/record **value is its arena
  index** — a plain Number that flows through params, returns, and recursion with NO new
  ABI. Entry = 1 tag byte + 8 bytes per payload field; tag = `concept_index*256 +
  variant_index`.
- **`VariantConstruct`** evaluates fields, bump-allocates an entry, stores tag + payload,
  pushes the index. **`MatchVariant`** loads the tag, dispatches per arm, copies payload
  fields into binder slots.
- **Records** reuse the same machinery (a record = a single-variant concept). Field access
  is resolved at **compile time**: `static_concept_of` recursively resolves the concept of
  any base expression — typed param spans, let-RHS constructions, and chained `a.b.c` —
  so the emitted access is a fixed-offset load (`[idx*entry_size + r15 + 8 + 8*field_i]`),
  no runtime tag inspection.
- **Text** compiles to ONE packed i64 — `start*2^32 + len` — spanning the **interpreted
  source, which the emitted ELF embeds at its own end** (`src_base` immediate). Compiled
  spans are numerically identical to interpreter spans; `length`/`byte_at`/`substring`
  emit as fixed-size sequences over (start, len). Text-free programs don't embed and stay
  byte-identical.

Captured — every case compiled with `/tmp/elf_program_src "$(cat prog.vx)" 0 > a.out`,
executed, and compared against the `/tmp/eval_main` oracle on the same source:

| program | compiled | oracle |
|---|---|---|
| list-sum `lsum(build(3))` (recursive variant walk) | 6 | 6 |
| list-sum `lsum(build(5))` | 15 | 15 |
| record `let s = Foo { a: 42, b: 7 } ; s.a` | 42 | 42 |
| record `s.b` | 7 | 7 |
| record `s.a + s.b` | 49 | 49 |
| record param `get_a(Foo { a: 42, b: 7 })`, `get_a(s : Foo) = s.a + s.b` | 49 | 49 |
| recursion carrying a record: `go(St { n: 5, acc: 0 })` | 15 | 15 |
| chained access `Outer { inner: Foo {…} } ; o.inner.b` | 7 | 7 |
| text scanner `word_length(Sc { src: "hello world", pos: 0 })` | 5 | 5 |
| `length("hello")` | 5 | 5 |
| `byte_at("abc", 1)` | 98 | 98 |
| `length(substring("hello world", 6, 11))` | 5 | 5 |
| `byte_at(substring("hello world", 6, 11), 0)` | 119 | 119 |
| `byte_at("abc", 99)` (defensive OOB) | 0 | 0 |

The scanner ELF is 1,230 bytes and **embeds its interpreted source** (the program text is
grep-able inside the executable — that's where the spans point); the record-param ELF is
434 bytes and embeds nothing (text-free). Recursion carrying variant indices fell out FREE
of the arena convention: `lsum(t)` passes an arena index through the existing scalar
call/ret path — no new machinery.

Two things this arc surfaced, both now pinned: the `AstIf` position-threading bug (+15
where truth was +10 for THEN-branch offsets — latent under 428 green tests because every
prior recursive program recursed in the ELSE arm; `word_length` recursed in a THEN arm and
landed a call 5 bytes short), and the `Call`-inline `arena_ctx` drop. Both are design
lessons now ([`docs/design-lessons.md`](design-lessons.md)).

## 8. How it works under the hood

Terse notes an auditor would want (the pre-R6 notes still hold; new ones marked):

- **One arena for everything.** Tokens, AST nodes, values, environments, and diagnostics
  are all variants of the single `concept_group VExpr [max_depth: 4096, max_nodes:
  6000000]`. No pointers: structures link by **arena index**, lists are cons-lists walked
  recursively. The arena is off-stack and mmap-backed since the capacity arc; the
  6M bound is gen0's (Rust-backend) arena — self-emitted binaries reserve 16 GiB
  MAP_NORESERVE (§12) and, since `arena_scope` (§13), reclaim per-proc during emit.
- **Recursive rules compile to real `call`/`ret`.** The mutually-recursive
  parser/checker/interpreter rules emit as separate x86-64 callables in one ELF, resolved
  by the native backend's two-pass SCC label pass.
- **Group-return ABI.** Rules returning group values (`build_env`, `find_rule`,
  `eval_ast_env`'s Value results, the `Span`/`Diag` builders) use the group-return ABI.
- **The Value model is arena-flat.** `VNum`/`VData`/`VText` are themselves variants
  in the same group; a `VData` payload is a `ValueList` cons-list by index. `variant_tag`
  computes `concept_index*256 + variant_index` — records are the `variant_index == 0`
  degenerate case, which is why record eval and codegen reuse the variant machinery
  wholesale. Reserved tags 15360000/1/2 carry Ok/Err/collection values with zero new
  machinery.
- **The self-hosted verifier mirrors verbosec's shape.** `find_rule` resolving is
  the "is this a rule call" test (primitives don't resolve → exempt from `calls:`);
  binder lists thread through every walk so match arms scope; the termination check is a
  per-call-site argument-pattern match, not a semantic analysis — exactly the "declared,
  then mechanically checked" posture of the host compiler.
- **`byte_at` is fail-closed in the host, defensive in the toy.** The HOST's `byte_at`
  aborts on OOB (so `undef_span_char` on a clean program aborts — test
  `undef_span_start_of == 9999` instead). The TOY grammar's `byte_at` (§6) returns
  `VNum 0` on OOB — an interpreter-domain choice, matched exactly by the compiled path.
- **Termination is declared, and checked where it fits.** Every recursive rule carries a
  `termination` block; scanners use `increasing`, list walks that recurse on a packed-state
  *field* carry `bound:` only (the native backend emits its mandatory breadcrumb for
  those). The runtime backstops are real: the arena is `max_nodes`-bounded and every
  list/AST is finite.
- **The emitter STREAMS.** Both the pretty-printer and the byte back end write to fd 1 in
  order during the walk — no materialized output, no backpatching. Forward jump offsets
  are computed ahead of time by `code_size_expr`/`code_size_node`, the pure-arithmetic
  twins of `x86_expr`/`x86_node`; every arm of the two must agree to the byte
  (the AstIf +15/+10 bug in §7 is what disagreement looks like). Streamed writes are
  wrapped `push r11`/`pop r11` (host arena base survives the syscall). The streaming
  property is also what makes `arena_scope` sound (§13) and lets the text fold avoid
  materializing its accumulator (§15).
- **The emitted arena registers are `r15`/`r14`** — chosen callee-saved so the
  emitted procs' `call`/`ret` discipline preserves them for free; the emitted programs
  make no syscalls between arena writes, so no reload dance is needed (unlike the host's
  r11).
- **The ELF wrapper is minimal and self-contained.** 64 B ELF header + one PT_LOAD R+X +
  the itoa-printing `_start` trampoline; no libc, no relocations. Text-using
  programs get the interpreted source appended at the file end and mapped with everything
  else — the packed spans index into it directly. Result-output programs get a 210-byte
  Result trampoline (Ok→stdout / Err→stderr, §14); collection-output programs get the
  bytes-entry shape (the proc streams every element itself, §15).
- **Printing is a normal form** (canonical types, full parens, fixed indentation), a fixed
  point, and semantically invisible — analyses and evaluation round-trip through it.

## 9. The bricks

Derived from `git log --oneline`. The original front-end arc (bricks 1–37 + b1–b8) is §3's
pipeline; the arcs after it are §§4–7 and the new chapters §§10–16. `src/` native-backend
changes (not Verbose bricks) are flagged inline.

```
ebff7ee  1       Verbose tokenizer written in Verbose
4546f7e  2       materialized token stream (cons-list in the arena)
            (9529277  native: group-concept field in recursive-callable ABI — parser unblocker)
86a6075  3       expression parser in Verbose (precedence + arena AST)
1b1a124  4       full operator precedence (cmp / and / or / unary)
db153c8  5       parser primary — identifiers, calls, field access
bc48eb9  6       if/then/else as an expression
1e51c24  7       string and boolean literals
52fb818  8a      line traversal + Newline token
67245c3  8b      INDENT/DEDENT via a column stack
3821541  9       parse a statement block (let-bindings + final value)
e249f7b  10      evaluate a let-block with an environment (name resolution)
7ba56e7  11      parse a `rule` declaration with a logic block
da9374b  12      parse a PROGRAM (a list of rule declarations)
1532e0d  13      a LINTER in Verbose (undefined-variable check)
8945260  14      linter catches use-before-def (let-ordering analysis)
02e74ba  15      linter pass — undefined-callee (first inter-rule analysis)
cdb3c89  16      rule parameters (parsed + scoped in the linter)
c321f15  17      call-arity lint pass (args vs params)
e5e49f1  18      call/apply — the evaluator runs real function calls
39f9ac1  19      a TYPE CHECKER in Verbose (static pendant of the interpreter)
3ecf500  20      typed parameters — types come from the source
bacc149  21      call-site argument type checking — parser meets checker
45f0169  22      the FULL PIPELINE — verbosec's main(), written in Verbose
2aa38c5  23      structured diagnostic — which rule, which category
c6e5fc7  24      report ALL diagnostics — a DiagList, not just the first
ed84fbb  25      column-level location for undefined variables (the name span)
d02a90d  26      the located full report — Diag carries the offending name span
f877814  27      locate the undefined-callee diagnostic (category 2)
2d5b2a8  28      locate the arity diagnostic (category 3)
bd859bc  29      equality on booleans in the type system
89668cf  30      text literals type as `text`
d6cd3ac  31      parse return-type annotations (rule NAME(params) : T)
ec105d4  32      use return types in the checker (calls type as their declared return)
b214fb1  33      check the declared return type against the body's inferred type
            (0ad5794  native: streaming lowering for recursive text-returning rules — FIRST EMITTER)
b13e4c7  34      the front end PRINTS its own parse — pretty-printer over the vexpr AST
49c341b  35      print a whole PROGRAM — the full-file round-trip
b3ef369  36      the first real LOWERING — expr → stack IL
527d371  37      lower a whole PROGRAM to the stack IL (call/ret, frames)
58f088c  b1      a first-class `bytes` type with `b"\xNN"` literals
56a8ea1  b2      bytes concat + le32/le64 — compose byte sequences
042f876  b3      a Verbose rule lowers an EXPRESSION to x86-64 MACHINE CODE
0291a86  b4a     comparisons + if/else in the x86-64 generator (computed jump offsets)
673c4c2  b4b-1   lower a NON-RECURSIVE program to one callable blob (frame ABI, call/ret)
c56d7fb  b4b-2   RECURSION in Verbose-emitted x86-64 machine code
f9deca6  b5      emit a STANDALONE ELF executable
d836ff9  b6      div / mod / and / or in the x86-64 generator
9f1d9f9  b7      let bindings in the x86-64 generator (rbp frame slots)
42582b0  b8      itoa in machine code — the ELF PRINTS its result to stdout

── capacity arc ──
4d5c14e  C1      cursor rewrite (O(1) peek) + 2 native fixes; O(N²) found to be the TOKENIZER
8ae44e4  C2      the real tokenizer O(N²) was the eager-let string_run — fixed (O(N))
            (8b22a9d  native: off-stack mmap arena — the 65,535-node ceiling falls)
90741b1  C3      raise self-hosted parser position bounds to real scale; capacity arc complete

── grammar arc (the parser reads its own source) ──
1ff0754  R1      parser ACCEPTS top-level concept declarations
25c4c40  R2      parser reads its own STRUCTURE — count_rules on real self-source chunks
38fb708  R3      parse :: variant construction + block-form match into AST
1ea613f  R4      parse the TYPES for real — descend concept_group + parse variants:
e915508  R5      parse input/output types + proofs (the verification surface)

── checker arc (all four proof surfaces) ──
03ccaf5  R6a     the checker recurses into match/variant (lints, with binder scoping)
a6f2f56  R6b     concept-aware, SOUND type checker over variant/match
446b459  R6d     purity verifier — the checker enforces reads/calls proofs
e8cb6ff  T       TERMINATION VERIFIER — the checker verifies all four proof surfaces

── interpreter arc (the runtime Value model) ──
77030df  R6c     the interpreter RUNS match/variant (VNum/VData Values)
18a1563  rec2    AstField eval — the interpreter reads record fields
2d9a639  txt1    VText spans — the interpreter runs a real scanner

── back-end arc (compiled == oracle) ──
fecaf77  R7a     the self-hosted compiler EMITS arena machine code (VariantConstruct)
67c7a15  R7b     self-hosted MatchVariant codegen — COMPILED recursive match/variant runs
619a14c  rec1    bare-record construction parses (no longer dropped)
3f344d4  rec3    AstField CODEGEN — the compiler emits record field access
d08e815  rec4    record params + unified static concept resolver (chained a.b.c)
81a1cbb  txt2    text CODEGEN — packed spans + embedded source; the compiler compiles a scanner

── the road to the fixed point (self-compile channels + scale, §10) ──
4e624b2  #85     the full arc consolidated — grammar, four-proof checker, interpreter, codegen
44e6ba1  #86     real source compiles — input:-block bridge + verbatim tokenizer family
1c1f929  #87     the emitters tier — concat ropes + streaming codegen
c90c007  #88     self-fragments compile + the dormant 2^40 exponential fix
1263641  #89     vexprparse's OWN TOKENIZER compiles — the first pipeline stage, self-compiled
d11f82e  #90     fail-closed rejection of raw newlines in string literals
2d114b5  #91     runtime argv input — the self-compiled front end runs on real source
c7078d0  #92     the self-compiled CHECKER verifies real source — arena ceiling raised
23fbfba  #93     stdin channel — the self-compiled front end reads its entire own source
c241e9e  #94     codegen dedup — proc sizes computed once (O(N²)→O(N), 7.6× less emit memory)
bcd0245  #95     text-let/read as a multi-field-record call arg (codegen gap fixed)
435806a  #96     raw-stdin channel + THE STRUCTURAL FIXED POINT — vexprparse emits its own source

── bytes tier (the compiler's own output language, §11) ──
5f82f33  #97     the semantic gap is the missing bytes tier + B1 (byte literals parse)
fa13814  #98     B2 — interpreter bytes value model (the oracle)
f33965d  #99     B3 — codegen (the emitter emits b"..."/le32/le64/bytes-concat)
26a2a26  #100    B4a — entry trampoline composes input-marshal with bytes output
d680a14  #101    B4b — THE SEMANTIC FIXED POINT: vexprparse compiles vexprparse (byte-identical)

── bootstrap (§12, §13) ──
2f15df4  #102    THE TWO-GENERATION BOOTSTRAP FIXED POINT — the compiler reproduces its entire self
107293f  #104    reinforce: R1 whole-source + R2 corpus + the self-hosting-bootstrap CI job
a08155a  #105    arena_scope — the self-hosted emitter reclaims per-proc (8.7 GB → 2.55 GB)

── Result tier (§14) ──
766a5e6  #106    slice 1 — parse + interpret Ok/Err/match_result
0f71254  #107    slice 2 — codegen; vexprparse compiles Ok/Err/match_result
9655dba  #108    slice 3 — top-level Result(number,text) output ABI

── collections tier (§15) ──
0f1b757  #109    slice 1 — sum/count over collection(number): the first runtime loop
de207f2  #110    slice 2 — fold with a source-named accumulator
1ec0baf  #111    slice 3 — all/any quantifiers
9c2ae63  #112    slice 6 — multi-field collection(Concept) elements
77bbda7  #113    (verbosec-side) native Phase 3 accepts scalar-element input — the missing oracle
75037f6  #114    slice 4b — map/filter → streamed collection output
5637d55  #115    slice 5 — streamed text fold (closes the tier's planned slices)

── self-verify arc (§16) ──
3be511d  #116    V1a — the lint walk survives the full self-source (O(2^n) checker bug fixed)
b389fae  #117    V1b — the four checker models fixed; the linter reaches 0 on its own source
d6b9bc5  #118    V2 — verify_errors, the aggregate gate; 0 on the self-source
80ca092  #119    V3 — THE VERIFYING BOOTSTRAP — gen1 verifies before it emits
```

## 10. The road to the fixed point — the self-source becomes compileable input

Between the b8-era milestone and the bytes tier sits an arc (#85–#96) that turned "the
back end compiles a widened toy grammar" into "the back end compiles **its own source**".
The walls, in the order they fell:

- **The `input:`-block bridge (#86).** The self-source declares its rules with `input:`
  blocks (real grammar); the executors ran paren-param rules. The bridge makes
  `input:`-declared rules callable end-to-end — `rule_params_of` resolves a rule's
  *effective* params (parens if non-empty, else the `input:` fields converted). This one
  accessor later turns out to be load-bearing for the checker too (V1b, §16).
- **The emitters tier (#87).** Text-returning rules compile via concat ropes + streaming
  codegen — the self-source's own pretty-printer family becomes compileable.
- **The first self-compiles (#88, #89).** Fragments of the self-source compile; then the
  tokenizer — the first full pipeline stage — compiles and runs. #88 also defused a
  dormant 2^40-class eager-double-mention exponential (a pattern that returns in checker
  form at V1a, §16 — same trap, twice).
- **Runtime input channels (#91, #93, #96).** The compiled front end initially embedded
  its input at compile time; #91 made it read real source from argv at runtime, #93 added
  the stdin channel (lifting the kernel's 128 KB argv cap — the full source arrives in one
  read), #96 the raw-stdin channel for the emitter drivers.
- **The checker at scale (#92).** The self-compiled checker verifies real source; the
  emitted-binary arena ceiling raised for checker-walk workloads.
- **Emit-side scale (#94, #95).** `proc_sizes` computed once instead of per call site
  (O(N²) → O(N), 7.6× less emit memory), and the last codegen gap — a text `let`/`read`
  as a multi-field-record call argument — fixed after being minimally isolated (see
  [`self-hosting-fixed-point-attempt.md`](self-hosting-fixed-point-attempt.md)).
- **THE STRUCTURAL FIXED POINT (#96).** `vexprparse` emits a valid ELF for its own full
  source. Structural, not semantic: the emitted compiler existed but did not yet
  *behave* — which is exactly what the bytes tier diagnosis explains.

## 11. The bytes tier — the compiler's own output language (B1–B4b)

**The problem, profiled to root** ([design doc](self-hosting-bytes-tier-design.md)): after
the structural fixed point, the self-emitted emitter did NOT reproduce `elf_program_src` —
the self-emitted binary misbehaved. Traced, not guessed: **the self-hosted compiler had no
bytes tier**. Its `Ast` had no `AstBytes`, no `le32`/`le64`, no bytes-concat — but the
emitter is BUILT of exactly those: at diagnosis time the source contained **176** `b"..."`
literals, **60** `le32(...)` + **8** `le64(...)` calls, and **24** bytes-returning rules.
`b"\x41"` mis-parsed as `AstVar(b)` plus an orphaned string; `le32(5)` parsed as a call to
a nonexistent rule (bad `proc_offset` → bad `call` rel32 → jump to garbage). The emitter
could not represent its own construction material. Not a bug — a missing tier, exactly
like text before its arc.

**The arc** mirrored the text tier's shape:

- **B1 (#97)** — lexer + AST: `b"..."` with `\xNN` hex escapes → `AstBytes` (raw bytes,
  no quote-strip semantics); `le32`/`le64` recognized as primitives, not calls;
  bytes-concat distinguished from text-concat by arg type.
- **B2 (#98)** — the interpreter bytes value model: the oracle for B3.
- **B3 (#99)** — codegen: `b"..."` → raw bytes inline (jmp-over-data + ptr/len),
  `le32`/`le64` → LE-encoded bytes, bytes-concat → byte-exact, gated against B2 AND
  against the Rust backend byte-for-byte.
- **B4a (#100)** — the entry trampoline composes input-marshal with bytes output: the
  FIRST semantic self-reproduction, single-rule.
- **B4b (#101)** — **THE SEMANTIC FIXED POINT**: reorder `elf_program_src` first,
  self-emit it, run the self-emitted emitter on a program P — its output is
  byte-identical to the Rust-compiled `elf_program_src(P)`, and the emitted ELF runs
  correctly (pinned across scalar / if-else / call / record / variant / recursion /
  match cases in `b4b` tests).

Honest scope note: B4b proved the self-emitted **emitter** reproduces the reference on
small programs. Reproducing the *entire self* — every proc, the full 1.1 MB source through
two generations — is the next chapter.

## 12. THE TWO-GENERATION BOOTSTRAP FIXED POINT (#102, #104)

The definition, from the test that pins it (`two_generation_bootstrap_fixed_point`,
src/native.rs):

```
gen0 = the Rust-compiled `elf_program_src` (the reference emitter).
gen1 = gen0(reordered full source)   — the SELF-compiled compiler.
gen2 = gen1(reordered full source).

The fixed point: gen1 == gen2 byte-for-byte.
```

(The reorder only moves `rule elf_program_src` to first position so the self-emitted
binary lays its entry proc at offset 0.)

Getting there surfaced a real scale wall: gen1 initially SIGSEGV'd emitting the
`elf_program_src` closure. The self-hosted emitter arena-stores EVERY record it constructs
(Verbose has no loops — every list traversal is recursion constructing an argument record
= one arena node) and never frees; the full self-compile touches ~84M nodes where gen0
(which stack-passes plain-concept records) peaks at ~1.5M. The 1 GiB arena the emitted
prologue mmap'd overflowed at ~10.3M nodes. Fix: size the self-emitted arena at **16 GiB
MAP_NORESERVE** — only touched pages cost physical RAM.

Captured live in this worktree (2026-07-20, WSL2, `ulimit -s unlimited` for the src_blob
recursion depth):

```
$ cargo run --release -- examples/vexprparse.verbose --native gen0 --run elf_program_src --stdin-raw
native: examples/vexprparse.verbose -> gen0 (323814 bytes, rule 'elf_program_src', input: stdin-raw)

# reorder: move `rule elf_program_src` to be the first rule
$ ./gen0 0 < reordered.verbose > gen1     # 3.26 s, peak RSS 194 MB   (verify + emit)
$ chmod +x gen1 && wc -c gen1
1754029 gen1
$ ./gen1 0 < reordered.verbose > gen2     # 17.59 s, peak RSS 10.45 GB (verify + emit)
$ cmp gen1 gen2 && echo "gen1 == gen2 (byte-identical)"
gen1 == gen2 (byte-identical)
$ sha256sum gen1 gen2 | cut -c1-16
0c4e111939f95058     # both
```

Note what gen1 IS: a 1,754,029-byte statically-linked ELF, emitted by machine code, that
tokenizes, parses, verifies (§16), and compiles Verbose programs — including its own
entire 1.1 MB source — with no Rust anywhere in its runtime.

**The reinforcement (#104)** hardened the claim from "a fixed point" to "a fixed point OF
THE TRUSTED REFERENCE":

- **R1 — whole source, no reorder caveat:** `gen0(ORIGINAL) == gen1(ORIGINAL)`
  byte-for-byte. Captured live:

  ```
  $ ./gen0 0 < examples/vexprparse.verbose > g0_orig
  $ ./gen1 0 < examples/vexprparse.verbose > g1_orig
  $ cmp g0_orig g1_orig && echo "R1 holds"
  R1 holds                                  # 1,754,109 bytes each
  ```

- **R2 — corpus + run-correctness:** for a spread of small programs covering the
  construct space (arith, nested-if, call-chain, multi-field record, N-arm variant/match,
  runtime recursion, max/min, chained field), `gen1(P) == gen0(P)` AND the ELF gen1
  emitted actually RUNS to the correct value.
- **CI:** the dedicated `self-hosting-bootstrap` CI job runs the full R0+R1+R2 test
  serially on every PR. The fixed point is a gate, not a demo.

Correctness frame: R0/R1/R2 prove byte-identity to gen0 — verbosec's Rust backend, the
trusted base — plus the self-fixed-point. The self-hosted emitter is a byte-identical,
run-verified fixed point of the trusted reference.

## 13. `arena_scope` — a declared reclaim boundary (#105)

**The problem:** gen1 peaked **8.7 GB** emitting the full source where gen0 does the same
job in 140 MB. The first attempt at a fix was a dedup slice (thread `blob_end` as a
precomputed constant) — and it is documented as a **negative result**
([emit-dedup design](self-hosting-emit-dedup-design.md)): zero benefit (8.7 → 8.9 GB,
marginally worse), and the sub-agent-reported "62× win" that motivated it was a
measurement artifact (it had measured gen0, which *compiles* `blob_end_off` but never
*runs* it). The real finding: **it's the arena model, not any one recompute** — with no
loops and no reclaim, every recursive traversal allocates per step, and a lookup table
doesn't help because the table walk is itself an alloc-per-step recursion.

**The insight:** the emit STREAMS (§8). Once a top-level proc has streamed its bytes,
every arena node that proc's walk allocated is dead — purity guarantees no escape except
via the return value, which was just written to fd 1. So reclaim = reset the arena bump
counter to a mark taken before the proc.

**The primitive:** `arena_scope(e)` in a streaming-bytes position — mark, stream `e`,
reset. Output bytes identical to `e` alone; the only effect is reclaim. The verifier
accepts it only where sound (one bytes-typed arg, in a bytes-returning streaming rule) and
refuses elsewhere with a breadcrumb — a *declared* boundary the verifier checks and the
backend exploits, not an inferred one (the auto-reclaim alternative was rejected: it would
have changed every existing bytes-streaming binary and hidden the boundary). One placement
in the self-source, at `x86_program`'s per-proc concat.

The subtle part ([design doc](self-hosting-arena-scope-design.md), "THE CRUX"): the two
backends manage the arena differently, so the primitive has two faithful lowerings —
native.rs saves/resets the `[r11+off]` node count (general-correct, a no-op for gen0,
whose plain records are stack-passed), while the self-hosted emitter emits `push r14` /
`pop r14` (its arena count register). Both generations emit the identical 2-byte pair, so
gen1 == gen2 survives.

**Measured (clean disk, 2026-07-12):** gen1's full-source emit fell **8.7 GB → 2.55 GB
(3.43×), 22 s → 3.7 s**; the two-generation test dropped 71 s → 10.6 s, cheap enough to
CI-gate. The honest scope note held exactly as predicted: `arena_scope` reclaims BETWEEN
procs, so the biggest proc (`x86_node`) still pays `code_size_node`'s O(n²) within its own
scope — the ~544 MB parse-tree floor plus one proc's transients is the current shape, and
a scalar-result `arena_scope` around `code_size_node(x)` is the documented follow-on.
(Today's live numbers in §12 are higher than 2.55 GB because the V3 verify pass now runs
on top of the emit — §16 quantifies that cost.)

## 14. The Result tier — Ok / Err / match_result (#106–#108)

vexprparse's own source uses zero `Result`, so this tier is **coverage-broadening** toward
a general compiler, not a self-compile unblock — the invariant through every slice is that
the fixed point holds untouched. Oracle = verbosec (`--run` and `--native`).

**The design decision** ([design doc](self-hosting-result-tier-design.md)): first-class
AST nodes (`AstOk`, `AstResErr`, `AstMatchResult` — mirroring how R3 added
AstVariant/AstMatch), but NO new Value variant: `Ok(x)` evaluates to `VData` with reserved
tag 15360000, `Err(x)` to 15360001 — high above any real `cidx*256+vidx` — reusing the
existing payload/binder machinery wholesale. A sentinel-desugar onto a fake concept was
rejected (fragile arena-sizing detection, byte-identity risk). Since every self-hosted
value is one i64 (number, packed span, arena index), one shape serves `Result(number,text)`
and `Result(text,text)` alike.

- **Slice 1 (#106)** — parse + eval. `match_result` is intercepted in `parse_primary`
  (its `binder => body` arms aren't normal call args). Milestone: self-hosted eval agrees
  with verbosec's interpreter (Ok(5) doubled → 10; Err path → -1).
- **Slice 2 (#107)** — codegen. `AstOk`/`AstResErr` emit as 2-slot arena nodes (the
  variant-construct shape); `AstMatchResult` as a 2-way tag dispatch (the match dispatch +
  binder-load shape). `program_uses_result` appends a synthetic concept so a concept-less
  Result program still gets the arena prologue and a safe entry size. Milestone: a
  COMPILED match_result program runs == `--run` == verbosec.
- **Slice 3 (#108)** — the top-level ABI. A rule whose OUTPUT is `Result(number, text)`
  gets the 210-byte Result trampoline: runtime tag test → Ok payload to stdout + exit 0,
  or the Err text's packed span materialized from the embedded source to stderr + exit 1.
  Pinned **bit-for-bit** (stdout / stderr / exit code) against verbosec's own compiled
  binary for the same validator (`result_tier_slice3_top_level_result_routes_ok_stdout_err_stderr`).

## 15. The collections tier — the first runtime loops (#109–#115)

The largest coverage tier: `sum` / `count` / `fold` / `all` / `any` / `map` / `filter`,
over scalar AND record elements — and with it, the **first runtime-conditional loop the
self-hosted emitter has ever emitted** (everything before iterated by recursion in the
compiled program, not by a loop in the emitted code). Design:
[collections-tier](self-hosting-collections-tier-design.md) +
[slice 4](self-hosting-collections-slice4-design.md). As with Result, the self-source uses
none of it (it walks cons-lists by recursion+match), so gen1 == gen2 held through every
slice.

Key design decisions:

- **No materialization for reductions.** Mirror verbosec's Phase 4: the collection input
  is a count-prefixed argv tail (`<N> <e0> … <e{N-1}>`), consumed inline; no collection
  value exists at runtime.
- **No lambdas.** The `x => body` runs as an inline loop in the rule's own frame; `x`
  binds via the existing match-binder slot machinery. The slice-1 accumulator is a
  RESERVED rbp slot, not a source-named binder — vexprparse cannot mint a synthetic
  `"__acc"` name because every `AstVar` span must point into real source bytes. (`fold`,
  slice 2, then put the real source-named accumulator into the binder slot that slice 1
  had reserved with a dummy entry.)
- **The `code_size` mirror is the drift edge.** Every loop's forward-exit and
  backward-jump rel32 must be tracked arm-for-arm by the pure-arithmetic size twin — the
  same discipline (and the same failure mode) as §7's AstIf bug.

The slices: **1 (#109)** sum/count; **2 (#110)** fold — with a non-commutative-body pin
(`acc * 2 + x` over [1,2,3] → 11, proving left-to-right iteration); **3 (#111)** all/any
desugared to the fold shape (vacuous truth on empty pinned); **6 (#112)** multi-field
`collection(Concept)` elements — flattened-argv stride, per-element arena construct, so
the body's `o.amount` flows through the existing AstField emit with zero new resolver
machinery; **4b (#114)** map/filter → **streamed** collection output (one decimal per
line; the streamed choice over a first-class collection value was deliberate — the value
form has no verbosec oracle and no consumer, YAGNI); **5 (#115)** the text fold.

Two slices deserve their own sentences:

- **Slice 4a (#113) is a verbosec-side lift.** A fresh-context adversarial review of the
  slice-4 design found that verbosec `--native` REFUSED scalar-element input collections
  entirely — meaning map/filter over `collection(number)` had **no byte-oracle in any
  form**. The fix was to extend verbosec's own Phase 3 first (one atoi per element into
  the lambda var slot) — the self-hosting arc creating its missing oracle by fixing a real
  verbosec limitation. The dependency can point both ways.
- **The text fold STREAMS where verbosec materializes.** verbosec's Phase 5b does two-pass
  sizing + buffer + one write; the self-hosted emitter streams the init text, then per
  element streams each non-`acc` concat arg in source order — **the accumulator never
  materializes, it IS the stream prefix already written**. Sound because the fold body is
  validated append-only (`fold_stream_ok`); violations compile to an int3 body, even on
  empty input. Byte-identical to verbosec's output including the empty-collection and
  negative-number cases.

A live composition capture (2026-07-20): gen1 — the self-compiled, verifying compiler of
§12/§16 — compiles a fully-declared map program, and its output agrees byte-for-byte with
verbosec's own native backend:

```
$ cat doubles.verbose
concept W
  fields:
    xs : collection(number)

rule doubles
  input:
    w : W
  output:
    r : collection(number)
  logic:
    r = map(w.xs, x => x * 2)
  proofs:
    purity:
      reads : [w.xs]
      calls : []
    termination:
      bound : 8

$ ./gen1 0 < doubles.verbose > doubles.elf && chmod +x doubles.elf
$ ./doubles.elf 3 10 20 30
20
40
60
# verbosec --native on the same program: a 529 B binary; stdout byte-identical (cmp clean).
# gen1's emitted ELF: 936 B.
```

Honest scope, this tier: fold-form `min`/`max` are deferred (they need verbosec-style
lookahead disambiguation before becoming parse keywords — the self-source's own *binary*
`max()` calls would collide); record-element `filter` (JSON output) and text element
fields are later slices; and the interpreter's `eval_fold` stays numeric — text-fold
**eval** is a documented divergence (the native path is the pinned one; no collection
input reaches the eval path in the exercised suite).

## 16. The self-verify arc — the verifying bootstrap (#116–#119)

The two-generation fixed point reproduced the EMITTER: parse → emit. But Verbose's
identity is "the compiler verifies, never guesses" — and the four proof checkers, though
present in the source and individually compileable, were absent from the bootstrap binary.
This arc closes that: **parse → VERIFY → emit, fail-closed, self-applied**
([design doc](self-hosting-self-verify-design.md)).

The opening probe (compiled checkers fed the full self-source): purity → 0 exit 0;
termination → 0 exit 0; **the lint aggregate → rc=1, no output**. The lint walk did not
survive its own source. Four slices from there:

**V1a (#116) — the O(2^n) checker bug.** Profiled, not guessed: the failure was NOT arena
exhaustion at scale but the 2^40-class **eager-double-mention trap, checker edition** —
`arms_type`'s ArmCons arm mentioned `arms_type(rest)` twice and `arm_body_type(head)` up
to 4×, i.e. O(2^arms) per match. The self-source has 44 matches with 20–24 arms; cap
titration proved the exponential (6/8/16/20M-node caps ALL exhausted — no cap can cover
2^24; adding ONE 24-arm rule to a passing 100-rule prefix blew 20M nodes). Fix:
single-evaluation combine helpers (`arms_type_combine` / `arms_type_norm` /
`match_arm_ty`). Post-fix the full walk needs **~1.5M nodes / ~0.25 s** — inside the
existing caps; the design doc's "raise the cap" branch dissolved. Same trap as #88's,
caught a second time in a different subsystem — it is a design lesson for a reason.

**V1b (#117) — the four checker models fixed, non-vacuously.** The walk now completed but
reported **1468** diagnostics on the self-source — not 1468 real lints but four
checker-MODEL false-positive families, taken down one category at a time (1468 → 871 →
415 → 319 → 0, with per-category drops matching exactly — 597/456/96/319 — and zero
cross-category movement):

- cat 1 (597, one per rule): the `input:`-block name was in scope NOWHERE — the lint
  seeded scope from `rd_params`, empty for every `input:`-block rule. Fix:
  `rule_params_of`, the executors' own effective-params accessor (#86's bridge), at both
  seed sites.
- cat 3 (456): same root on the DECLARED side — `rule_arity` read arity 0 for every
  `input:`-block rule, so every 1-argument call "mismatched".
- cat 2 (96): no primitive allowlist — fixed by `span_is_primitive` (the exact
  `span_is_*` recognizer set), mirroring how the purity pass exempts primitives.
- cat 4 (319): the R6b-era stubs — `AstField` typed as ERROR, every primitive call as
  number. Fixed by real field-selection typing, `call_result_type` resolving the callee's
  declared `output:` (concept codes included), primitive result types, arg-driven concat
  typing (text vs bytes mode — which yields a genuinely NEW lint catching the mixed
  concat verbosec's verifier also rejects), and sound non-ERROR typing for the
  Result/reduction nodes.

Non-vacuity is pinned, not assumed (`self_verify_v1b_checker_models_fixed_not_vacuous`):
a minimal verbosec-clean program covering every false-positive shape scores 0, and **8
seeded genuine violations** (one per family, including the nasty variants: an undefined
callee nested inside a primitive's args; a constructor PLUS one extra argument) still flag
with the right category.

**V2 (#118) — the aggregate gate.** `verify_errors(src)` = one tokenize+parse, then
lints+types + purity + termination summed. Captured live in this worktree:

```
$ cargo run --release -- examples/vexprparse.verbose --native verify_errors --run verify_errors --stdin-raw
native: examples/vexprparse.verbose -> verify_errors (164578 bytes, rule 'verify_errors', input: stdin-raw)
$ ulimit -s unlimited; ./verify_errors 0 < examples/vexprparse.verbose
0                                  # 0.40 s, peak RSS 172 MB
```

Zero on the self-source; a clean fully-declared program → 0; one seeded violation per
surface → nonzero (pinned per-surface in `self_verify_v2_aggregate_gate`).

**V3 (#119) — THE VERIFYING BOOTSTRAP.** One new streaming primitive: `abort_if(e)` — in
a streaming-bytes position it EXECUTES the check (nonzero → sys_exit(1), zero output
bytes) and streams nothing when clean. `elf_program_src`'s body becomes
`out = concat(abort_if(verrs), <ELF bytes…>)` where `verrs` is the V2 aggregate computed
over the parsed input (reusing the same tokenize/parse lets — no second pass). Fail-closed
by construction: no partial ELF can escape, and a clean program's output is byte-identical
to the ungated emitter's.

The refusal, demonstrated live on gen1 (2026-07-20) — a perfectly *emittable* factorial,
refused because its proofs are not declared:

```
$ cat fact_noproofs.verbose
concept N
  fields:
    v : number
rule main
  logic:
    out = fact(N { v: 5 })
rule fact(n : N)
  logic:
    out = if n.v == 0 then 1 else n.v * fact(N { v: n.v - 1 })

$ ./gen1 0 < fact_noproofs.verbose > refused.out; echo "exit: $?"
exit: 1
$ wc -c refused.out
0 refused.out
```

And the acceptance side — the same gen1, fed a fully-declared clean program:

```
$ ./gen1 0 < clean.verbose > clean.elf; echo "exit: $?"
exit: 0
$ file clean.elf
clean.elf: ELF 64-bit LSB executable, x86-64, version 1 (SYSV), statically linked, no section header
$ ./clean.elf
2            # helper(1) = 2, a 289-byte verified-then-emitted executable
```

And because the gate sits inside `elf_program_src`, **every emit in §12 was already a
verified emit**: gen0 verified the full self-source before emitting gen1; gen1 verified it
before emitting gen2. The fixed point of §12 IS the verifying bootstrap — the gate code is
itself part of the source being verified and emitted. Measured cost of the gate (dev
machine, same-machine baseline): gen0's emit ~unchanged (the Rust-hosted walk is nearly
free: 179 → 194 MB); gen1's emit grew materially (the self-hosted runtime arena-allocates
every checker-walk record — this worktree's live run: 17.59 s / 10.45 GB for verify + emit,
vs 3.7 s / 2.55 GB emit-only at the arena_scope milestone). gen1 itself grew by ~7.5 KB
(the gate + one recognizer proc).

**Honest framing, load-bearing:** the self-hosted checker is a **SUBSET of verbosec's
verifier** — the four self-hosted surfaces, not verbosec's full check set. Documented
blind spots (from the V1b review): `match_result` binders default to number (payload types
untracked); the neutral type codes 6 (Result) and 5 (collection) are payload- and
element-blind; **reduction bodies are not type-walked** (the item element type is
unknowable to the current model); `arena_scope` types bytes unconditionally; `fold` types
as its init. Two eval divergences are documented rather than hidden: text-fold eval stays
numeric (§15), and `abort_if` cannot exit inside the interpreter — eval evaluates its
argument and yields `VNum 0` (the exit-1 semantics exist only in compiled code). And
`x86_program_src` — the exec-probe harness driver — deliberately stays UNGATED (noted in
the source at the gate site). None of this is fine print to wave away: "gen1 verifies its
source" means *verifies the four self-hosted proof surfaces*, exactly as the source
comment says.

## 17. Where it stands, and what remains

The claim, assembled from the pinned tests and the live captures above:

- The self-hosted pipeline **reads and verifies the real Verbose grammar** — including
  every declaration in its own 1.1 MB source (`verify_errors` → 0, §16).
- The self-hosted back end **compiles a closed grammar wide enough to contain the
  compiler itself**: scalars, lets, calls, recursion, variants/match, records, text
  spans + concat ropes, bytes, Result, collections (§§10–15).
- The composition is a **verifying, self-reproducing compiler**: gen1 = gen0(source)
  verifies then emits; gen1 == gen2 byte-for-byte (sha `0c4e1119…`, §12); it refuses
  unverified source with exit 1 and zero bytes (§16); and it compiles fresh programs
  whose outputs are byte-identical to verbosec's own backend (§15). CI gates the fixed
  point on every PR.

Deferred, named plainly:

- **Fold-form `min`/`max`** — parse-keyword collision with the self-source's binary
  `max()` calls; needs lookahead disambiguation first (§15).
- **Record-element `filter` (JSON) and text element fields** in collections — later
  slices; record map is scalar-body only today.
- **Collection-returning rule calls / composed collections** — no oracle: verbosec
  refuses them too. Becomes its own tier if verbosec ever lands composition.
- **The checker's blind spots** (§16): Result payload typing, element-typed reduction
  bodies, span-located diagnostics for categories 4–5. Each is a checker-model slice with
  the V1b playbook (fix, then pin non-vacuity with seeded violations).
- **Text-fold eval and `abort_if`-in-eval** — documented interpreter divergences; closing
  them is bookkeeping, not architecture.
- **verbosec features outside the closed grammar** — reactions, services, `read`/`fetch`,
  modules, attributes-as-semantics. The self-source needs none of them; each would be a
  deliberate tier with its own oracle discipline, not a drive-by.

The north star was a Verbose compiler written in Verbose. What exists now is precisely
that, with the project's identity intact in the loop: a compiler, written in Verbose,
compiled by itself, that **verifies before it emits** — fail-closed on its own four proof
surfaces — and reproduces its entire self byte-for-byte, on every PR, under CI. The
remaining distance is still width, not shape — but the shape is closed.
