# Self-Hosting Front End — a Verbose compiler front end written in Verbose

This document capitalizes the "self-hosting" arc: 35 incremental bricks that built a
complete compiler **front end — and its inverse — entirely in Verbose**, living in
[`examples/vexprparse.verbose`](../examples/vexprparse.verbose) (119 concepts, 250 rules).
Every number and command output below was captured by running the code; nothing is
predicted or estimated.

## 1. What this is (and isn't)

It is a front end **plus an emitter** for a **toy subset** — the "vexpr" grammar:

- rules with typed parameters and a declared return type (`rule add(x : number, y : number) : number`),
- `let`-bindings inside a `logic:` block, with a final `out = <expr>`,
- arithmetic / boolean / comparison expressions, `if/then/else` as an expression,
- calls to other rules.

It **reads and writes**: it lexes, parses, runs five static analyses, type-checks,
interprets, locates diagnostics at the offending token, **and prints a parsed program back
to source** — and the printed program reparses to the same analyses and the same
evaluation (the round-trip, §3.J–K). It is **not** verbosec compiling its own full source:
the real compiler (`src/`, Rust) parses the entire Verbose language; this exercise parses a
deliberately small slice of it. The point of the arc is to walk toward the north-star goal
— *a Verbose compiler written in Verbose* — by building, in Verbose, the pieces a compiler
needs: a lexer, a parser, static analyses, an interpreter, a type checker, a located
diagnostic report, and a streaming emitter.

The **dogfooding** is real: the whole front end is itself **compiled by verbosec to
native x86-64**. A driver like `check_program` is a ~62 KB statically-linked ELF, produced
by the real compiler from the `.verbose` file, that lexes/parses/analyzes a toy program
passed on `argv`. The front end is not interpreted by a host language at runtime — it is
machine code.

What it does NOW that the 25-brick version did not: declared **return types** (parsed +
checked, bricks 31–33); a `{number, bool, text}` type system where **calls type as their
declared return type** (brick 32); **located diagnostics** for undefined-variable,
undefined-callee, and arity errors (the token's source span, bricks 26–28); and a
**streaming emitter** — the first time the front end produces output rather than a verdict
(bricks 34–35, on the native streaming lowering of commit `0ad5794`).

Honest limitations, stated up front (expanded in §6):

- **Toy subset only.** Records, collections, `map`/`filter`/`fold`, `Result`, reactions,
  services, modules — none of these grammar forms are parsed. The grammar is rules + typed
  params + return type + let + scalar expressions + calls + `if`/`then`/`else`. (`match` is
  used heavily *inside* the front end's own rules over its concept-group AST, but the toy
  grammar it *parses* does not include `match`.)
- **Types are `{number, bool, text}`.** Equality works on number-or-bool; ordering on
  numbers; text is a literal/param/arg type with no operations; field access is still "not
  yet typed" (`AstField` → ERROR in the checker).
- **Spans are located for categories 1–3** (undefined var, undefined callee, arity). Type
  and arg-type errors (4–5) report `(rule index, category)` only — `AstBin`/`AstNum` carry
  no source span to point at.
- **The interpreter's value domain is integers.** `eval_main` evaluates arithmetic,
  comparison (as 0/1), `if`, and recursive calls over integers; the *type checker* is
  richer than the *evaluator*.
- **The emitter prints SOURCE.** A pretty-printer is the first emitter, not yet a code
  generator targeting something executable — that lowering is the next frontier (§6).

Companion files: the grammar's intent prose is in
[`examples/vexprparse.intent`](../examples/vexprparse.intent); the emitter's design arc
(an arena proposal, the adversarial review that killed it, and the streaming alternative
that shipped) is in
[`docs/emitter-streaming-design.md`](emitter-streaming-design.md).

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
| **Type checker** | `type_of_env`, `build_tenv`, `bin_type`, `if_type`, `call_result_type`, `tcheck_rule` | The static pendant of the interpreter over `{number, bool, text}`: `build_tenv` builds a *type* environment from typed params, `type_of_env` assigns a type to each expression; equality types number-or-bool (brick 29), text literals type as text (brick 30), a call types as its callee's **declared return type** (brick 32, the program threaded through the checker), and `tcheck_rule` flags a declared return that disagrees with the body (brick 33, via the `9`=unannotated sentinel). |
| **Return types** | `parse_rule_decl_pos` (extended), `rd_return_type`, `type_code_of_span` | The header grammar gained `rule NAME(params) : T`; the return type is stored in `RuleDecl` and consumed by the checker (bricks 31–33). |
| **Unified pipeline** | `check_program` | verbosec's `main()`, in Verbose: tokenize → parse → run all five analyses → count problems (brick 22). |
| **Diagnostics** | `find_diag`, `first_bad_rule`, `first_bad_category`, `prog_diags`, `all_diags_count`, `nth_diag_at_cat`, `nth_diag_at_span_start` / `_span_len`, the `*_span_rule` finders | A structured, **located** report: which rule, which category, and — for categories 1 (undef var), 2 (undefined callee), 3 (arity) — the byte span of the offending token (bricks 23–28). `prog_diags` builds a `DiagList` of `Diag(rule, category, span_start, span_len)`. |
| **Emitter** | `print_source`, `print_expr`, `print_args` / `_rest`, `op_text`, `print_program_src`, `print_rule`, `print_params` / `_rest`, `print_binds`, `ty_text`, `ret_text` | A **streaming** pretty-printer: `print_expr` walks the `Ast` and writes its bytes to stdout in order (fully parenthesized); `print_program_src` prints an entire multi-rule program — headers, typed params, return types, `logic:` blocks, lets, out-lines, 2/4-space indentation. Round-trips to the same analyses and the same evaluation (bricks 34–35, on the native streaming lowering). |

## 3. A live session

All outputs below are captured from the native drivers. The file verifies first:

```
$ cargo run --quiet -- examples/vexprparse.verbose 2>&1 | tail -1
verified: 119 concept(s), 250 rule(s); all proofs check out
```

Each driver is one rule compiled native, e.g.:

```
$ cargo run -- examples/vexprparse.verbose --native /tmp/check_program --run check_program
native: examples/vexprparse.verbose -> /tmp/check_program (60528 bytes, rule 'check_program', input: argv)
```

Native binary sizes (one ELF per rule, statically linked, no libc) — a snapshot; each grows
by a few hundred bytes as rules are added, and the emitter drivers are larger (`print_source`
~32 KB, `print_program_src` ~50 KB):

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

### C — Undefined callee, located (category 2)

```
SRC='rule main
  logic:
    out = g(1)'

$ /tmp/first_bad_category "$SRC" 0      →  2
$ /tmp/nth_diag_at_span_start "$SRC" 0  →  29   (the 'g' in "out = g(1)")
$ /tmp/nth_diag_at_span_len   "$SRC" 0  →  1
```

Categories 1 (undef var), 2 (callee), and 3 (arity) all carry the offending token's span
(bricks 26–28); a span finder walks the AST and returns the first offending node's
`(start, len)`. Categories 4 (type) and 5 (arg-type) report `(rule, category)` only — the
`AstBin`/`AstNum` nodes involved carry no source span to point at.

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

$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 0   →  (rule 0, cat 1, span 38/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 1   →  (rule 1, cat 2, span 87/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 2   →  (rule 1, cat 3, span 73/1)
$ nth_diag_at_rule/_cat/_span_start/_span_len, n = 3   →  (rule 1, cat 5, span 9999/0)
```

Read as a report:
- rule 0 (`f`): undefined variable `z` (cat 1) — located at byte 38;
- rule 1 (`main`): undefined callee `g` (cat 2, byte 87); wrong arity calling `f` with 2 args
  (cat 3, the `f` at byte 73); argument-type mismatch passing a `bool` (`1 < 2`) to `f`'s
  `number` param (cat 5, `span 9999/0` — not located, the sentinel).

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

### J — The type system uses declared return types

A call's result type is the callee's **declared** return type, so type errors that involve
call results are caught — and false positives from assuming "everything is a number"
disappear:

```
# 1 + b() where b() : bool  →  number + bool, a type error now caught:
SRC='rule b() : bool
  logic:
    out = 1 < 2
rule main
  logic:
    out = 1 + b()'
$ /tmp/type_check "$SRC" 0   →  1

# a rule that declares bool but computes... a different type than its body:
SRC='rule b() : number
  logic:
    out = 1 < 2'
$ /tmp/type_check "$SRC" 0   →  1   (declared number, body is bool)

# equality on bools (brick 29) and a text literal (brick 30) type-check clean:
$ /tmp/type_check 'rule f
  logic:
    out = (1 < 2) == (3 < 4)' 0   →  0
$ /tmp/type_check 'rule f
  logic:
    out = "hello"' 0              →  0
```

### K — The emitter: print, and round-trip

The front end now **emits**. `print_source` prints an expression (fully parenthesized, so
precedence and associativity are visible):

```
$ /tmp/print_source "1 + 2 * 3" 0            →  (1 + (2 * 3))
$ /tmp/print_source "if 1 < 2 then 3 else 4" 0  →  (if (1 < 2) then 3 else 4)
```

`print_program_src` prints an entire program — headers, typed params, return types, blocks,
lets, indentation re-synthesized:

```
SRC='rule main
  logic:
    out = fact(5)
rule fact(n)
  logic:
    out = if n == 0 then 1 else n * fact(n - 1)'

$ /tmp/print_program_src "$SRC" 0
rule main
  logic:
    out = fact(5)
rule fact(n : number)
  logic:
    out = (if (n == 0) then 1 else (n * fact((n - 1))))
```

Printing is a **normal form**: an untyped param `n` prints as `n : number` (the parser
stores untyped as `number`, brick 20 — semantically identical), expressions are fully
parenthesized, indentation is exactly 2/4 spaces. Hence the round-trips hold — the printed
program reparses to the *same* analyses and the *same* evaluation:

```
# THE KILLER — the front end re-executes its own output:
$ OUT=$(/tmp/print_program_src "$SRC_factorial" 0)
$ /tmp/eval_main "$OUT" 0   →  120   ==  /tmp/eval_main "$SRC_factorial" 0  →  120

# the printed QUAD-error program reproduces all four diagnostics:
$ OQ=$(/tmp/print_program_src "$SRC_quad" 0)
$ /tmp/check_program "$OQ" 0   →  4   ==  /tmp/check_program "$SRC_quad" 0  →  4

# idempotence — the printed form is a fixed point:
$ /tmp/print_program_src "$OUT" 0   ==  "$OUT"   (byte-identical)
```

The emitter is the streaming acceptance example `examples/print_chain.verbose`'s big
sibling: `show(s) = print_expr(build_chain(s))` builds the `Int`/`Add` AST then prints it —

```
$ cargo run -- examples/print_chain.verbose --native /tmp/pc --run show
native: examples/print_chain.verbose -> /tmp/pc (1088 bytes, rule 'show', input: argv)
$ /tmp/pc 3   →  3+2+1+0
```

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
  `type_of_env`, `find_diag`, `prog_diags`, the `Span` family, `print_expr`, …) recurse over
  a value that is a *field* of a packed-state input rather than the input itself, so a
  `structural` proof does not fit them; they carry a `bound:` only, and the native backend
  emits the mandatory stderr breadcrumb noting that a declared `bound:` is not, by itself, a
  termination proof for recursion. The runtime backstops are real: the arena is
  `max_nodes`-bounded and every list/AST is finite, so each walk strictly shrinks toward a
  leaf or `Nil`.
- **The emitter STREAMS — it never materializes the printed string.** A recursive
  text-returning rule whose body builds fresh text (`concat(print(lhs), op, print(rhs))`)
  cannot return a stack-built buffer (it dies at `ret`). Instead the native backend lowers
  the whole text SCC to *writer mode*: each callable writes its bytes to fd 1 **in order**
  during the tree walk and returns nothing. No buffer, no arena, no dangling pointer. The
  one hazard — `write` clobbers `r11`, which in-callable code TRUSTS as the arena base — is
  closed by wrapping every streamed write in `push r11`/`pop r11` (4 bytes, size-stable for
  the two-pass label resolution). Streaming mode is a whole-SCC ABI property decided once at
  compile time; rules that fit the old literal-leaf grammar keep the materializing path, so
  every pre-emitter example compiles **byte-identically** (a 10-binary SHA-256 gate proves
  it). The full design arc — a bounded text-arena proposal, the adversarial review that
  found its flagship consumer breaks the arena's own `r11` invariant, and the streaming
  alternative that won on attack surface — is in
  [`docs/emitter-streaming-design.md`](emitter-streaming-design.md).
- **Printing is a normal form.** The emitter does not reproduce the input byte-for-byte; it
  reproduces a *canonical* form (untyped param → `: number`, `rule f()` → `rule f`,
  unannotated return → nothing, fully parenthesized expressions, 2/4-space indentation).
  That form is semantically invisible — every analysis and the evaluator round-trip through
  it — and it is a **fixed point**: `print(parse(print(parse(s)))) == print(parse(s))`. One
  documented divergence from the interpreter: on a mid-tree abort the streaming binary has
  already written a stdout prefix, whereas the interpreter errors with no output; exit codes
  agree, bytes-on-failure do not.

## 5. The 35 bricks

Derived from `git log --oneline` on `feat/self-hosting`. One line per brick, with the
commit sha. Two entries are `src/` native-backend changes, not bricks — flagged inline.

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
d02a90d  26  the located full report — Diag carries the offending name span
f877814  27  locate the undefined-callee diagnostic (category 2)
2d5b2a8  28  locate the arity diagnostic (category 3)
bd859bc  29  equality on booleans in the type system
89668cf  30  text literals type as `text`
d6cd3ac  31  parse return-type annotations (rule NAME(params) : T)
ec105d4  32  use return types in the checker (calls type as their declared return)
b214fb1  33  check the declared return type against the body's inferred type
            (0ad5794  native: streaming lowering for recursive text-returning rules — FIRST EMITTER)
b13e4c7  34  the front end PRINTS its own parse — pretty-printer over the vexpr AST
49c341b  35  print a whole PROGRAM — the full-file round-trip
```

(`9529277` and `0ad5794` are native-backend changes in `src/`, not bricks: the first
unblocked the parser by adding a group-concept field to the recursive-callable ABI; the
second is the streaming lowering that the emitter bricks 34–35 consume.)

Grouped: bricks 1–2 + 8a–8b are the **lexer**; 3–12 + 16 + 20 + 31 the **parser**; 13–17 +
19 + 21 + 26–30 + 32–33 the **analyses + type checker + located report**; 10 + 18 the
**interpreter**; 22 the **unified pipeline**; 23–28 the **diagnostic report**; 34–35 the
**emitter**.

## 6. Limitations & what's next

Honest scope, restated from §1:

- **Toy subset only.** The grammar is rules + typed params + return type + let + scalar
  expressions + calls + `if`/`then`/`else`. No records, collections, `map`/`filter`/`fold`,
  `Result`, reactions, services, modules, or attributes. (`match` runs inside the front
  end's own rules but is not in the grammar it parses.) This is the slice that makes the
  shape legible, not the whole language.
- **Types are `{number, bool, text}`.** Equality on number-or-bool, ordering on numbers,
  text as literal/param/arg with no operations; `AstField` is still ERROR in the checker.
- **Spans for categories 1–3.** Type (4) and arg-type (5) errors report `(rule, category)`
  only — `AstBin`/`AstNum` carry no source span. Locating them needs spans added to those
  AST nodes (a parser + every-construct-site change).
- **Integer-domain interpreter.** `eval_main` evaluates over integers (comparisons as
  `0`/`1`); the type checker is richer than the evaluator.
- **The emitter prints SOURCE, not a lowered target.** A pretty-printer reconstructs the
  program in a normal form; it does not yet generate code for a different machine.

The north star remains a Verbose compiler written in Verbose. What this arc has now shown:
the front-end *shape* — lexer, parser, five analyses, type checker, interpreter, located
diagnostics — **and its inverse**, a streaming emitter that round-trips complete programs,
are all expressible and verifiable in Verbose, and verbosec compiles the whole thing to
standalone native binaries. The front end reads *and* writes.

The distance still to go, named plainly:

1. **A real lowering.** The pretty-printer prints *source*. The first true code-generation
   step is to print a *different* target — e.g. `vexpr` → a small stack-machine IL, or a
   simpler intermediate form — which is the moment "pretty-printer" becomes "code
   generator". The streaming machinery (bricks 34–35) is the foundation; the work is
   choosing a target and writing the per-node lowering rules.
2. **The full language surface.** Records, `match`, `Result`, collections, effects — parsed,
   not just consumed internally.
3. **A backend in Verbose** that emits something executable — the hardest part, and the
   actual meaning of "a compiler in Verbose".

Each is a milestone on the path, not the destination. But the half that did not exist when
this document was first written — emitting — now does.
