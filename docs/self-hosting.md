# Self-Hosting Front End — a Verbose compiler front end written in Verbose

This document capitalizes the "self-hosting" arc: 25 incremental bricks that built a
complete compiler **front end entirely in Verbose**, living in
[`examples/vexprparse.verbose`](../examples/vexprparse.verbose) (102 concepts, 219 rules).
Every number and command output below was captured by running the code; nothing is
predicted or estimated.

## 1. What this is (and isn't)

It is a front end for a **toy subset** — the "vexpr" grammar:

- rules with typed parameters (`rule add(x : number, y : number)`),
- `let`-bindings inside a `logic:` block, with a final `out = <expr>`,
- arithmetic / boolean / comparison expressions, `if/then/else` as an expression,
- calls to other rules.

It is **not** verbosec compiling its own full source. The real compiler (`src/`, Rust)
parses the entire Verbose language; this exercise parses a deliberately small slice of
it. The point of the arc is to walk toward the north-star goal — *a Verbose compiler
written in Verbose* — by building, in Verbose, the pieces a front end needs: a lexer, a
parser, static analyses, an interpreter, a type checker, and a located diagnostic report.

The **dogfooding** is real: the whole front end is itself **compiled by verbosec to
native x86-64**. A driver like `check_program` is a ~60 KB statically-linked ELF, produced
by the real compiler from the `.verbose` file, that lexes/parses/analyzes a toy program
passed on `argv`. The front end is not interpreted by a host language at runtime — it is
machine code.

Honest limitations, stated up front (expanded in §6):

- **Toy subset only.** Records, collections, `map`/`filter`/`fold`, `match`, `Result`,
  reactions, services, modules — none of these grammar forms are parsed. The grammar is
  rules + typed params + let + scalar expressions + calls.
- **No return-type annotations yet.** Rules declare parameter types; a rule's result type
  is inferred from its body, not declared.
- **Types are `{number, bool}`.** `text` and field access exist in the token/AST surface
  but are treated as "not yet typed" by the checker.
- **Spans are located for undefined variables only.** The other four diagnostic categories
  report `(rule index, category)`, not a token span.
- **The interpreter assumes every value is a number.** `eval_main` evaluates arithmetic,
  comparison (as 0/1), `if`, and calls over integers; it has no boolean or text value
  domain.

Companion files: the grammar's intent prose is in
[`examples/vexprparse.intent`](../examples/vexprparse.intent); the AST source it parses is
the `.verbose` toy subset itself.

## 2. The pipeline

Every stage is a set of Verbose **rules** operating on an arena-allocated AST. The arena is
a single `concept_group VExpr [max_depth: 4096, max_nodes: 65535]` (line 104 of the
`.verbose` file): tokens, AST nodes, environments, and diagnostics are all variants in that
one group, linked by arena indices (cons-lists by index, not pointers).

| Stage | Rules | What it does |
|---|---|---|
| **Tokenizer** | `begin_tokenize`, `next_token`, the column-stack INDENT/DEDENT helpers | Scans the source bytes into a `TokenList` cons-list; emits `Newline` and, via a column stack, `Indent`/`Dedent` tokens (bricks 1–2, 8a–8b). |
| **Parser** | `parse_or` → … → `parse_primary` (precedence ladder), `parse_block`, `parse_rule_decl_pos`, `parse_program` | Recursive-descent over the token list into the `Ast` arena. The precedence ladder threads `or` → `and` → comparison → additive → multiplicative → unary → primary. `parse_primary` handles identifiers, calls, field access, literals, parenthesized expressions, and `if/then/else`. `parse_block` parses a `logic:` block (lets + final value); `parse_rule_decl_pos` a typed-param rule; `parse_program` a list of rule declarations (bricks 3–12, 16, 20). |
| **5 analyses** | `lint_program`, `lint_callees`, `lint_arity`, `type_check`, `argtype_check` | (1) undefined-variable + use-before-def (`lint_program`, bricks 13–14); (2) undefined-callee (`lint_callees`, brick 15); (3) call arity vs declared params (`lint_arity`, brick 17); (4) expression type errors (`type_check`, brick 19); (5) call-site argument-type mismatch (`argtype_check`, brick 21). |
| **Interpreter** | `eval_main`, `eval_call`, `bind_params`, `build_env` | A recursive tree-walking evaluator. `build_env` materializes the let-environment for a block; `bind_params` binds call arguments to parameters; `eval_call` resolves the callee and recurses (brick 10, 18). Numbers-only. |
| **Type checker** | `type_of_env`, `build_tenv` | The static pendant of the interpreter: `build_tenv` builds a *type* environment from typed params, `type_of_env` assigns `{number, bool}` to each expression (brick 19–20). |
| **Unified pipeline** | `check_program` | verbosec's `main()`, in Verbose: tokenize → parse → run all five analyses → count problems (brick 22). |
| **Diagnostics** | `find_diag`, `first_bad_rule`, `first_bad_category`, `prog_diags`, `all_diags_count`, `undef_span_start_of` / `undef_span_len_of` / `undef_span_char` | A structured report: which rule, which category, and (for undefined variables) the byte span of the offending name (bricks 23–25). `prog_diags` builds a `DiagList` (cons-list of `Diag(rule, category)` pairs). |

## 3. A live session

All outputs below are captured from the native drivers. The file verifies first:

```
$ cargo run --quiet -- examples/vexprparse.verbose 2>&1 | tail -1
verified: 102 concept(s), 219 rule(s); all proofs check out
```

Each driver is one rule compiled native, e.g.:

```
$ cargo run -- examples/vexprparse.verbose --native /tmp/check_program --run check_program
native: examples/vexprparse.verbose -> /tmp/check_program (60528 bytes, rule 'check_program', input: argv)
```

Native binary sizes (one ELF per rule, statically linked, no libc):

| driver | bytes | driver | bytes |
|---|---|---|---|
| `check_program` | 60528 | `undef_span_start_of` | 47407 |
| `all_diags_count` | 61246 | `undef_span_len_of` | 47500 |
| `nth_diag_at_rule` | 61443 | `undef_span_char` | 47464 |
| `nth_diag_at_cat` | 61443 | `eval_main` | 50098 |
| `first_bad_rule` | 60230 | `type_check` | 47715 |
| `first_bad_category` | 60230 | `lint_program` | 46363 |
| | | `count_rules` | 42781 |

Programs are passed on `argv` (the source as a single string, with `\n` newlines via bash
`$'...'`; the second arg is an index `n`, `0` unless indexing a `DiagList`).

### A — A clean program

```
SRC='rule add(x : number, y : number)
  logic:
    out = x + y
rule main
  logic:
    out = add(1, 2)'

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
SRC='rule f
  logic:
    out = foo + 1'

$ /tmp/first_bad_category   "$SRC" 0   →  1
$ /tmp/undef_span_char      "$SRC" 0   →  102
$ /tmp/undef_span_len_of    "$SRC" 0   →  3
$ /tmp/undef_span_start_of  "$SRC" 0   →  26
$ /tmp/lint_program         "$SRC" 0   →  1
```

The undefined name is `foo`: byte offset `26` in the source, length `3`; the first byte's
ASCII code is `102` (`'f'`). This is the only category that reports a token span.

### C — Undefined callee (category 2)

```
SRC='rule main
  logic:
    out = g(1)'

$ /tmp/first_bad_category "$SRC" 0   →  2
```

### D — Wrong call arity (category 3)

```
SRC='rule f(a)
  logic:
    out = 1
rule main
  logic:
    out = f(1, 2)'

$ /tmp/first_bad_category "$SRC" 0   →  3
```

`f` declares one parameter; the call passes two.

### E — Type error in an expression (category 4)

```
SRC='rule f
  logic:
    out = 1 + (2 < 3)'

$ /tmp/type_check         "$SRC" 0   →  1
$ /tmp/first_bad_category "$SRC" 0   →  4
```

`2 < 3` is `bool`; adding it to a `number` is rejected.

### F — Call-site argument-type mismatch (category 5)

```
SRC='rule g(b : bool)
  logic:
    out = f(b)
rule f(x : number)
  logic:
    out = x'

$ /tmp/first_bad_category "$SRC" 0   →  5
```

`f` expects a `number` parameter; `g` passes its `bool` parameter `b`.

### G — The full report: a multi-error program

`prog_diags` reports **every** rule×category problem, not just the first. This program
plants four:

```
SRC='rule f(a : number)
  logic:
    out = z + 1
rule main
  logic:
    out = f(1 < 2, 3) + g(9)'

$ /tmp/all_diags_count "$SRC" 0   →  4
$ /tmp/check_program   "$SRC" 0   →  4

$ /tmp/nth_diag_at_rule "$SRC" 0 / nth_diag_at_cat "$SRC" 0   →  (rule 0, cat 1)
$ /tmp/nth_diag_at_rule "$SRC" 1 / nth_diag_at_cat "$SRC" 1   →  (rule 1, cat 2)
$ /tmp/nth_diag_at_rule "$SRC" 2 / nth_diag_at_cat "$SRC" 2   →  (rule 1, cat 3)
$ /tmp/nth_diag_at_rule "$SRC" 3 / nth_diag_at_cat "$SRC" 3   →  (rule 1, cat 5)
```

Read as a report:
- rule 0 (`f`): undefined variable `z` (cat 1);
- rule 1 (`main`): undefined callee `g` (cat 2); wrong arity calling `f` with 2 args (cat 3);
  argument-type mismatch passing a `bool` (`1 < 2`) to `f`'s `number` param (cat 5).

### H — The interpreter

Recursive evaluation of real calls:

```
SRC='rule main
  logic:
    out = fact(5)
rule fact(n)
  logic:
    out = if n == 0 then 1 else n * fact(n - 1)'

$ /tmp/eval_main "$SRC" 0   →  120
```

```
SRC='rule main
  logic:
    out = fib(10)
rule fib(n)
  logic:
    out = if n < 2 then n else fib(n - 1) + fib(n - 2)'

$ /tmp/eval_main "$SRC" 0   →  55
```

`fact(5) = 120` and `fib(10) = 55` are computed by the Verbose-written evaluator, itself
compiled to native code — recursion in the *evaluated* program drives recursion in the
*evaluator's* own emitted `call`/`ret`.

### I — Diagnostic granularity contrast

`lint_program` counts problem *instances*; the `DiagList` counts rule×category *pairs*.
Two undefined variables in one rule are two instances but one diagnostic:

```
SRC='rule f
  logic:
    out = x + y'

$ /tmp/lint_program    "$SRC" 0   →  2
$ /tmp/all_diags_count "$SRC" 0   →  1
```

Both `x` and `y` are undefined (count 2), but they collapse to a single
`(rule 0, undefined-variable)` diagnostic in the report (count 1).

## 4. How it works under the hood

Terse notes an auditor would want:

- **One arena for everything.** Tokens, AST nodes, environments, and diagnostics are all
  variants of the single `concept_group VExpr [max_depth: 4096, max_nodes: 65535]`. There
  are no pointers in the language: structures link by **arena index**, and lists are
  cons-lists (`TokenList`, `RuleList`, `ArgList`, `DiagList`, …) walked recursively.
- **Recursive rules compile to real `call`/`ret`.** The mutually-recursive parser/analysis/
  interpreter rules emit as separate x86-64 callables in one ELF, resolved by a two-pass
  label pass over the strongly-connected component (the native backend's SCC emitter).
- **Group-return ABI.** Rules that return a group value — `build_env`, `find_rule`,
  `find_diag`, and the `Span`/`DiagList` builders — use the group-return ABI (the callee
  returns an arena value, not just a scalar). The single-variant "record-like" group
  concepts (`Span = MkSpan(start, len)`, `Diag = MkDiag(...)`, `RuleDecl`, `Block`) ride
  the same ABI as a multi-variant return.
- **Two diagnostic counts, deliberately different.** `check_program` / `lint_program` count
  *problem instances*; `all_diags_count` counts *rule×category pairs* in the `DiagList`.
  §3.I shows the divergence (2 vs 1). Use the right one for the question you're asking.
- **`byte_at` is fail-closed.** Reading a byte out of bounds aborts (exit 1). So
  `undef_span_char` aborts on a clean program (there is no offending byte to read). The
  cleanliness test is therefore `undef_span_start_of == 9999`, **not** running
  `undef_span_char` and checking for a value — the latter would terminate the process.
- **Termination is declared, and checked where it fits.** Every recursive rule carries a
  `termination` block. Where a numeric field strictly shrinks or grows, a Phase-C proof
  applies: the tokenizer/byte scanners use `increasing` over a bounded offset, and
  `param_nth_type` uses `decreasing : n`. The cons-list and AST walks (`eval_ast_env`,
  `type_of_env`, `find_diag`, `prog_diags`, the `Span` family, …) recurse over a value that
  is a *field* of a packed-state input rather than the input itself, so a `structural` proof
  does not fit them; they carry a `bound:` only, and the native backend emits the mandatory
  stderr breadcrumb noting that a declared `bound:` is not, by itself, a termination proof for
  recursion. The runtime backstops are real: the arena is `max_nodes`-bounded and every
  list/AST is finite, so each walk strictly shrinks toward a leaf or `Nil`.

## 5. The 25 bricks

Derived from `git log --oneline | grep -i self-hosting` on `feat/self-hosting`. One line
per brick, with the commit sha.

```
ebff7ee  1   Verbose tokenizer written in Verbose
4546f7e  2   materialized token stream (cons-list in the arena)
            (9529277  native: group-concept field in recursive-callable ABI — parser unblocker)
86a6075  3   expression parser in Verbose (precedence + arena AST)
1b1a124  4   full operator precedence (cmp / and / or / unary)
db153c8  5   parser primary — identifiers, calls, field access
bc48eb9  6   if/then/else as an expression
1e51c24  7   string and boolean literals
52fb818  8a  line traversal + Newline token
67245c3  8b  INDENT/DEDENT via a column stack
3821541  9   parse a statement block (let-bindings + final value)
e249f7b  10  evaluate a let-block with an environment (name resolution)
7ba56e7  11  parse a `rule` declaration with a logic block
da9374b  12  parse a PROGRAM (a list of rule declarations)
1532e0d  13  a LINTER in Verbose (undefined-variable check)
8945260  14  linter catches use-before-def (let-ordering analysis)
02e74ba  15  linter pass — undefined-callee (first inter-rule analysis)
cdb3c89  16  rule parameters (parsed + scoped in the linter)
c321f15  17  call-arity lint pass (args vs params)
e5e49f1  18  call/apply — the evaluator runs real function calls
39f9ac1  19  a TYPE CHECKER in Verbose (static pendant of the interpreter)
3ecf500  20  typed parameters — types come from the source
bacc149  21  call-site argument type checking — parser meets checker
45f0169  22  the FULL PIPELINE — verbosec's main(), written in Verbose
2aa38c5  23  structured diagnostic — which rule, which category (not just a count)
c6e5fc7  24  report ALL diagnostics — a DiagList, not just the first
ed84fbb  25  column-level location for undefined variables (the name span)
```

(`9529277` is a native-backend change in `src/`, not a brick — it unblocked the parser by
adding a group-concept field to the recursive-callable ABI.)

Grouped: bricks 1–2 + 8a–8b are the **lexer**; 3–12 + 16 + 20 the **parser**; 13–17 + 19 +
21 the **five analyses**; 10 + 18 the **interpreter**; 19–20 the **type checker**; 22 the
**unified pipeline**; 23–25 the **diagnostic report**.

## 6. Limitations & what's next

Honest scope, restated from §1:

- **Toy subset only.** The grammar is rules + typed params + let + scalar expressions +
  calls. No records, collections, `map`/`filter`/`fold`, `match`, `Result`, reactions,
  services, modules, or attributes. This is the slice that makes the front-end shape
  legible, not the whole language.
- **No return-type annotations.** Parameter types are parsed from the source; a rule's
  result type is inferred from its body. Declared return types are a future brick.
- **Types are `{number, bool}`.** `text` and field access exist in the surface syntax but
  are not assigned a type by the checker — they are "not yet typed".
- **Spans for undefined variables only.** Categories 2–5 report `(rule index, category)`.
  Extending the located-span machinery (brick 25) to the other categories is the obvious
  next refinement.
- **Numbers-only interpreter.** `eval_main` has one value domain (integers); comparisons
  evaluate to `0`/`1`. No boolean or text values, no short-circuit semantics distinct from
  the numeric encoding.

The north star remains a Verbose compiler written in Verbose. The distance from here is
large and worth naming plainly: this front end parses a small grammar and never emits code.
Closing that gap means parsing the full language surface, carrying a real type system
(including `text` and the named/record/collection/Result types), and — the hardest part —
a backend in Verbose. This arc proves the front-end *shape* is expressible and verifiable
in Verbose, and that verbosec can compile it to a standalone native binary. That is a
milestone on the path, not the destination.
