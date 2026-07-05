# Self-Hosting — a Verbose compiler front end *and back end* written in Verbose

This document capitalizes the "self-hosting" arc: incremental bricks that built a complete
compiler **front end, its inverse, and a real back end — entirely in Verbose**, living in
[`examples/vexprparse.verbose`](../examples/vexprparse.verbose) (223 concepts, 475 rules,
16,697 lines, 781,484 bytes). Every number and command output below was captured by running
the code in this worktree; nothing is predicted or estimated.

```
$ cargo run --quiet --release -- examples/vexprparse.verbose 2>&1 | tail -1
verified: 223 concept(s), 475 rule(s); all proofs check out
```

## 1. What this is (and isn't)

The arc now covers **two grammars**, and the distinction matters:

**The closed "vexpr" grammar — compiled end-to-end.** Rules with typed parameters and a
declared return type, `let`-bindings, arithmetic / boolean / comparison expressions,
`if/then/else`, calls, recursion — **plus, since the R6/R7 arcs: `concept` declarations with
fields, `concept_group` declarations with variants, `Name::Variant { ... }` construction,
bare-record `Name { ... }` construction, field access (including chained `o.inner.b`),
`match` with binder-scoped arms, and the text primitives `length` / `byte_at` /
`substring`.** For this grammar the self-hosted pipeline does everything a compiler does:
lexes, parses, runs the lints, type-checks with concept awareness, **verifies purity and
termination proofs**, interprets (numbers, variant values, records, text spans), locates
diagnostics at the offending token, prints programs back to source, lowers to a stack IL,
and **emits a standalone runnable x86-64 ELF** whose output is checked byte-for-byte against
the Verbose-written interpreter (the oracle).

**The real Verbose grammar — parsed and checked, not yet compiled.** Since the R2–R5
grammar arc, the self-hosted parser reads the *structure of its own full source*: full-line
comments, `@intention`/`@source` attributes, `concept_group ... [max_depth, max_nodes]`
headers, nested concepts with `variants:` blocks, range-annotated fields, `input:` /
`output:` blocks, and the whole `proofs:` surface (purity `reads`/`calls` lists,
`termination` with `bound` / `structural` / `decreasing` / `increasing`). The R6 checker
then *verifies* those declarations — the same four proof surfaces verbosec checks:
undeclared **calls**, undeclared **reads**, **termination** argument patterns at
self-recursive call sites, and declared-**type** conformance.

It is **not** verbosec compiling its own full source end-to-end: the CODEGEN target grammar
is the closed vexpr subset (§7 lists what compiles), and the full 781 KB self-source is
parsed by the self-hosted parser (chunked under the argv limit natively, whole-file via the
interpreter) but not yet fed through its own back end. §10 names the remaining distance
plainly.

The **dogfooding** is real: the whole front end and back end are themselves **compiled by
verbosec to native x86-64**. A driver like `check_program` is a ~96 KB statically-linked
ELF, produced by the real compiler from the `.verbose` file, that lexes/parses/analyzes a
program passed on `argv`. The front end is not interpreted by a host language at runtime —
it is machine code. And `elf_program_src` is itself a ~106 KB ELF that *reads a program as
text and writes out a complete, runnable x86-64 ELF* — a compiler, written in Verbose,
compiled to native, whose output is itself a standalone executable file, including
executables that **mmap a node arena, construct and match variants, walk records, and scan
text**.

## 2. The pipeline

Every stage is a set of Verbose **rules** operating on an arena-allocated AST. The arena is
a single `concept_group VExpr [max_depth: 4096, max_nodes: 2000000]` (line 104 of the
`.verbose` file): tokens, AST nodes, environments, values, and diagnostics are all variants
in that one group, linked by arena indices (cons-lists by index, not pointers). The
2,000,000-node bound is real-scale — the capacity arc (§4) replaced the old 65,535-entry
on-stack arena with an **off-stack mmap arena**, and made the tokenizer O(N).

| Stage | Rules | What it does |
|---|---|---|
| **Tokenizer** | `begin_tokenize`, `next_token`, the column-stack INDENT/DEDENT helpers | Scans the source bytes into a `TokenList` cons-list; emits `Newline` and, via a column stack, `Indent`/`Dedent` tokens. O(N) since the capacity arc (the eager-let `string_run` fix). |
| **Parser — expressions** | `parse_or` → … → `parse_primary` (precedence ladder), `parse_match`, `variant_build`, `parse_block` | Recursive-descent over the token list into the `Ast` arena, navigating by cons cell (O(1) cursor). `parse_primary` handles identifiers, calls, field access, literals, parens, `if/then/else`, `Name::Variant { ... }` construction, bare-record `Name { ... }` construction, and `match` with binder lists. |
| **Parser — real grammar** | `parse_program`, `parse_rule_decl_pos`, `parse_concepts`, `parse_concept_decl_pos`, `parse_variants`, `parse_fields`, `parse_proofs`, `parse_name_list` | Parses a whole program into `ProgramAst { concepts, rules }`: skips comments/@attrs, descends `concept_group` blocks, captures variants and range-annotated fields, and parses each rule's `input:`/`output:` types and the full `proofs:` block into `RuleDecl` (R1–R5). |
| **5 lints/analyses** | `lint_program`, `lint_callees`, `lint_arity`, `type_check`, `argtype_check` | (1) undefined-variable + use-before-def; (2) undefined-callee; (3) call arity; (4) expression type errors; (5) call-site argument-type mismatch. Since R6a all five walk `match`/variant bodies with binder scoping. |
| **Type checker** | `type_of_env`, `resolve_type`, `variant_payload`, `arms_type`, `bin_type`, `call_result_type`, `tcheck_rule`, `body_type` | Concept-aware since R6b: the lattice carries **concept codes** (`1000 + concept_index`) next to `{number, bool, text}`, `resolve_type` maps type-name spans to codes, match-arm binders get their concept types from the variant payload, and arms must agree. SOUND: `h + 1` where `h : Token` is an ERROR. |
| **Proof verifier** | `count_purity_errors`, `undeclared_calls`, `undeclared_reads`, `count_term_errors`, `term_errors`, `term_arg_ok` | The self-hosted pendant of verbosec's zero-trust verifier: flags calls to resolvable rules absent from the declared `calls:` list, input reads absent from `reads:` (locals and primitives exempt), and termination violations — `decreasing`/`increasing`/`structural` checked against the actual argument pattern at every self-recursive call site (R6d + the termination brick). |
| **Interpreter** | `eval_main`, `eval_ast_env`, `eval_call`, `eval_match`, `variant_tag`, `bind_params`, `build_env`, `prim_byte_at` / `prim_length` / `prim_substring` | A recursive tree-walking evaluator over a runtime **Value model**: `VNum` (numbers), `VData { tag, payload }` (variant and record values; tag = `concept_index*256 + variant_index`), `VText (start, len)` (text spans into the source). Runs match/variant, record construction + field access, and text scanners built from the host's own primitives (R6c + records + text slices). |
| **Diagnostics** | `find_diag`, `prog_diags`, `all_diags_count`, `nth_diag_at_*`, the `*_span_rule` finders | A structured, **located** report: which rule, which category, and — for undefined-var / undefined-callee / arity — the byte span of the offending token. |
| **Source emitter** | `print_source`, `print_expr`, `print_program_src`, … | A **streaming** pretty-printer; round-trips complete programs to the same analyses and the same evaluation. |
| **Stack-IL lowering** | `lower_expr_src`, `lower_program_src`, … | Lowers expressions/programs to a postfix stack-machine IL with `proc … ret`, `call`, `load`/`store`, structured `if/else/endif`. |
| **x86-64 generator** | `x86_expr`, `x86_node`, `code_size_node`, `x86_program`, `elf_program_src`, `static_concept_of`, `program_uses_arena`, `program_uses_text` | The **back end**: emits a standalone runnable ELF. Beyond the b1–b8 scalar grammar it now emits an **mmap'd node arena** (base in `r15`, count in `r14`), `VariantConstruct` (tag + payload stores), `MatchVariant` (tag dispatch + payload→binder slots), record field access resolved at compile time via `static_concept_of`, and **text spans as packed i64** (`start*2^32 + len`) over the source embedded at the end of the emitted ELF (R7 + records + text codegen). |

## 3. A live session — the closed vexpr grammar

All outputs below are captured from the native drivers, re-verified in this worktree (the
toy programs live in files; each run is `driver "$(cat prog.vx)" 0`). Each driver is one
rule compiled native, e.g.:

```
$ cargo run --release -- examples/vexprparse.verbose --native /tmp/check_program --run check_program
native: examples/vexprparse.verbose -> /tmp/check_program (96245 bytes, rule 'check_program', input: argv)
```

Native binary sizes (one ELF per rule, statically linked, no libc) — a snapshot at 475
rules; each grows as rules are added:

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
value, `-` for negatives) and `sys_write`s it to stdout. All of §M re-verified in this
worktree.

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
- **The arena was capped at 65,535 on-stack entries** — the real self-source needs ~115k
  nodes. The off-stack **mmap arena** (a `src/` native slice) lifted the ceiling;
  `vexprparse.verbose` now declares `max_nodes: 2000000`.

Measured today on real self-source chunks (native `count_rules`, i.e. tokenize + full
program parse + rule-list walk), the curve is linear:

```
59,769 bytes  →  count_rules = 18  in  6 ms
94,994 bytes  →  count_rules = 57  in 10 ms
128,660 bytes →  count_rules = 87  in 14 ms
```

**Structure (R2, R4):** the parser skips full-line comments and `@attribute` lines,
descends `concept_group` headers, parses nested concepts' `variants:` blocks and
range-annotated fields, and parses every rule past its `input:`/`output:`/`proofs:` blocks.
Captured against the real source (the native drivers take one argv string, so the source is
fed in chunks cut at declaration boundaries under the kernel's 128 KB single-arg cap):

```
# chunk = everything before the first top-level rule (42,206 bytes):
$ /tmp/count_concepts "$(cat chunk)" 0  →  39     (38 group-nested + 1 top-level == grep)
$ /tmp/count_variants "$(cat chunk)" 0  →  10     (the group's first concept, Token:
                                                   Ident/Keyword/Num/Op/Str/Newline/
                                                   Indent/Dedent/IndentErr/Eof)

# chunk = the largest prefix under the argv cap, cut at a rule boundary (128,660 bytes):
$ /tmp/count_rules "$(cat chunk)" 0     →  87     ==  grep -c '^rule ' on the chunk
```

The whole-file run (781 KB > the argv cap) goes through the interpreter path with the
source as a constructed text Value — the `#[ignore]`d test
`records_r2_full_file_count_rules` (slow: a whole-file tree-walk in the host interpreter;
run with `cargo test --release records_r2_full_file_count_rules -- --ignored`) asserts
`count_rules(self-source)` equals the live `grep -c '^rule '` count — 475 today.
(Historical note: at R2-commit time the chunked milestone read 302 rules — the source has
since grown to 475 and the chunk boundaries moved; the pinned property is
`count_rules == grep`, not any frozen number.)

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

This is the **verification surface**: everything the checker in §5 consumes is now parsed
by the self-hosted parser from real proof syntax.

## 5. The checker — all four proof surfaces, self-hosted

The R6a/R6b/R6d + termination bricks turned the front end's analyses into a real checker
over the widened grammar. All outputs captured from the native drivers.

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
verbosec's own zero-trust verifier enforces.

## 6. The interpreter — a runtime Value model

R6c replaced the numbers-only evaluator with a **Value model**: `VNum` for numbers,
`VData { tag, payload }` for variant AND record values (tag = `concept_index*256 +
variant_index`; a record is a single-variant concept whose tag has `variant_index` 0), and
`VText (start, len)` for text — a span into the evaluated source itself. All captured via
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
  2000000]`. No pointers: structures link by **arena index**, lists are cons-lists walked
  recursively. *(New)* the arena is off-stack and mmap-backed since the capacity arc; the
  old 65,535-entry ceiling is gone.
- **Recursive rules compile to real `call`/`ret`.** The mutually-recursive
  parser/checker/interpreter rules emit as separate x86-64 callables in one ELF, resolved
  by the native backend's two-pass SCC label pass.
- **Group-return ABI.** Rules returning group values (`build_env`, `find_rule`,
  `eval_ast_env`'s Value results, the `Span`/`Diag` builders) use the group-return ABI.
- *(New)* **The Value model is arena-flat.** `VNum`/`VData`/`VText` are themselves variants
  in the same group; a `VData` payload is a `ValueList` cons-list by index. `variant_tag`
  computes `concept_index*256 + variant_index` — records are the `variant_index == 0`
  degenerate case, which is why record eval and codegen reuse the variant machinery
  wholesale.
- *(New)* **The self-hosted verifier mirrors verbosec's shape.** `find_rule` resolving is
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
  wrapped `push r11`/`pop r11` (host arena base survives the syscall).
- *(New)* **The emitted arena registers are `r15`/`r14`** — chosen callee-saved so the
  emitted procs' `call`/`ret` discipline preserves them for free; the emitted programs
  make no syscalls between arena writes, so no reload dance is needed (unlike the host's
  r11).
- **The ELF wrapper is minimal and self-contained.** 64 B ELF header + one PT_LOAD R+X +
  the itoa-printing `_start` trampoline; no libc, no relocations. *(New)* text-using
  programs get the interpreted source appended at the file end and mapped with everything
  else — the packed spans index into it directly.
- **Printing is a normal form** (canonical types, full parens, fixed indentation), a fixed
  point, and semantically invisible — analyses and evaluation round-trip through it.

## 9. The bricks

Derived from `git log --oneline` on `feat/self-hosting`. The original front-end arc
(bricks 1–37 + b1–b8) is §3's pipeline; the entries after it are the arcs of §§4–7.
`src/` native-backend changes (not Verbose bricks) are flagged inline.

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
```

## 10. Limitations & what's next

Honest scope, restated:

- **The CODEGEN target grammar is the vexpr subset, not verbosec's full grammar.** What
  compiles: scalar expressions, lets, calls, recursion, variants/match, records/field
  access, and the three text primitives over spans. What does NOT compile (some of it
  parses and checks, none of it lowers): `concat` or any text that isn't a span of an
  existing buffer (there is no text heap in the emitted programs), collections,
  `map`/`filter`/`fold`, `Result`, reactions, services, modules, attributes-as-semantics.
- **No runtime text input to the emitted programs.** Compiled text values are spans into
  the source **embedded in the emitted ELF at compile time**. The compiled scanner scans a
  literal carried in its own file — a real milestone (compiled == oracle on the same
  bytes), but an emitted program cannot yet take a text argument at runtime.
- **The termination verifier covers self-recursion only.** Mutual recursion (`f` calls `g`
  calls `f`) is out of scope for the call-site pattern check — same slice-shaped gap
  verbosec closed with its SCC machinery.
- **`input:`-block rules and paren-param rules are parsed, not unified.** The real-grammar
  `input:` block parses into `RuleDecl` (R5) and feeds the checker; the evaluator and back
  end run rules declared with paren params. Making `input:`-declared rules callable
  end-to-end in the toy pipeline is its own brick.
- **Span-located diagnostics remain categories 1–3.** Type and arg-type errors report
  `(rule, category)` only.
- **It does not yet compile its own full source end-to-end.** The parser reads the real
  grammar (chunked natively, whole-file via the interpreter path); the checker verifies the
  real proof surfaces; the back end compiles the widened toy grammar. The composition —
  `elf_program_src(vexprparse.verbose)` — needs the codegen-side grammar (proofs blocks,
  `input:` rules, the full analysis pipeline as *compilable* code) and a compiled-side
  arena/value story for the front end's own 2M-node workloads.

The north star remains a Verbose compiler written in Verbose. What this arc has now shown,
beyond the b8-era milestone (a complete front end + scalar back end for a closed toy
grammar): the parser **reads the real grammar of its own source**; the checker **verifies
all four proof surfaces** (calls, reads, termination, types) — self-hosted, over real proof
syntax; the interpreter **runs variants, records, and text scanners** on a proper Value
model; and the back end **compiles variants, records, and text** with every milestone
pinned compiled-vs-oracle. The front end reads, the checker verifies, the emitter writes,
and the back end emits runnable executables whose behavior is the interpreter's — the
whole loop, in Verbose, for a grammar that now includes the shapes a compiler is made of
(sum types, pattern matching, records, text scanning). The remaining distance is width,
not shape.
