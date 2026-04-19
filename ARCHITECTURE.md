# Verbose — Complete Architecture

This document explains **everything** the compiler does, how, and why.
Written for someone discovering the project from scratch.

---

## The Big Picture

```text
                    WHAT THE HUMAN WRITES
                    ─────────────────────
                    invoices.intent
                    "An invoice is important
                     when its amount exceeds 10000"
                              │
                              ▼
                    WHAT THE AI GENERATES
                    ────────────────────
                    invoices.verbose
                    concept + rule + proofs + hints
                              │
                              ▼
              ┌───────────────────────────────┐
              │       VERBOSEC (compiler)     │
              │                               │
              │  1. LEXER     text → tokens   │
              │  2. PARSER    tokens → AST    │
              │  3. RESOLVER  imports merged  │
              │  4. VERIFIER  proofs checked  │
              │  5. OPTIMIZER AST simplified  │
              │  6. BACKEND   final output    │
              │                               │
              └───────┬───┬───┬───┬───────────┘
                      │   │   │   │
                      ▼   ▼   ▼   ▼
                   Interp Rust x86  WASM
                   JSON  441KB 570B  60B
```

---

## The 6 Compiler Stages

### Stage 1: LEXER (src/lexer.rs)

**Role:** Turn raw text into recognized tokens.

```text
Input:   "rule important\n  logic:\n    x = i.amount > 10000\n"
Output:  [Ident("rule"), Ident("important"), NEWLINE, INDENT,
          Ident("logic"), Colon, NEWLINE, INDENT,
          Ident("x"), Equal, Ident("i"), Dot, Ident("amount"),
          Gt, Number(10000), NEWLINE, DEDENT, DEDENT, EOF]
```

**Key point:** Indentation is significant (like Python). The lexer emits
`INDENT` and `DEDENT` tokens to mark blocks. No `{` `}`.

### Stage 2: PARSER (src/parser.rs)

**Role:** Turn tokens into a tree (AST = Abstract Syntax Tree).

```text
Tokens:  Ident("i") Dot Ident("amount") Gt Number(10000)
AST:     Binary(Gt,
           Field(Ident("i"), "amount"),
           Number(10000))
```

The AST is a typed tree: each node knows what it is (number, field access,
comparison, rule call, if/else, quantifier...).

**Operator precedence (weakest to strongest):**

```text
or → and → comparisons (> < >= <= == !=) → add/sub (+, -) → mul/div/mod (*, /, %) → unary (not, -) → primary (number, field, call, parens)
```

### Stage 3: RESOLVER (in src/main.rs)

**Role:** Load imported files (`use "stdlib/finance.verbose"`)
and merge all concepts/rules into a single program.

```text
app.verbose:               stdlib/finance.verbose:
  use "stdlib/finance"       concept Invoice { ... }
  rule my_rule { ... }       rule standard_tax { ... }
         │                            │
         └────────────┬───────────────┘
                      ▼
              Unified program:
                concept Invoice
                rule standard_tax
                rule my_rule
```

Circular imports are detected and skipped.

### Stage 4: VERIFIER (src/verifier.rs)

**Role:** Check that the AI's proofs are true. **ZERO TRUST.**

```text
AI declares:             Verifier checks:
  reads: [i.amount]  →  walks AST, lists all accesses → [i.amount] ✓
  calls: []          →  no function calls → [] ✓
  verdict: pure      →  calls=[] → pure ✓
  bound: 1           →  counts operations: 1 (Gt) → 1 ≤ 1 ✓
  overflow: [0, 100] →  interval arithmetic: [0, 100] ⊆ [0, 100] ✓
```

**All verification checks:**

| Check | What it verifies |
|---|---|
| reads match | Declared fields = actual field accesses in AST |
| calls match | Declared calls = actual rule invocations in AST |
| verdict coherent | pure ↔ calls=[] |
| termination bound | Declared bound ≥ actual operation count |
| determinism | total ↔ no non-deterministic calls |
| source exists | @source: file:line → file and line exist |
| field exists | Every accessed field (i.amount) exists on the concept |
| target matches | Logic target = declared output name |
| called rules exist | Every called rule exists in the program |
| hint valid | vectorizable ↔ pure with no calls |
| overflow bounds | Interval arithmetic proves bounds are respected |

**Interval arithmetic (for overflow and dead code):**

```text
Expression: i.amount * i.tax_rate / 100
Field ranges: amount ∈ [0, 10000000], tax_rate ∈ [0, 100]

Computation:
  amount * tax_rate → [0*0, 10000000*100] = [0, 1000000000]
  / 100             → [0/100, 1000000000/100] = [0, 10000000]

Result: [0, 10000000]
If AI declares overflow: [0, 10000000] → ✓ accepted
If AI declares overflow: [0, 1000]     → ✗ rejected (computed > declared)
```

### Stage 5: OPTIMIZER (src/optimizer.rs)

**Role:** Simplify the AST. **Platform-independent** — benefits
ALL backends (x86, WASM, Rust, future ARM).

```text
Before:  Binary(Add, Number(100), Number(20))     → After: Number(120)
Before:  Binary(Mul, Field(i, x), Number(0))       → After: Number(0)
Before:  Binary(Mul, Field(i, x), Number(1))       → After: Field(i, x)
Before:  Not(Not(expr))                             → After: expr
Before:  If(always_false_cond, then, else)          → After: else
```

**Universal optimizations:**

- Constant folding: 100 + 20 → 120
- Algebraic identities: x*0→0, x*1→x, x+0→x
- Double negation: not not x → x
- Dead code: if(impossible condition) then A else B → B

### Stage 6: BACKENDS (4 options)

```text
                        Optimized AST
                             │
              ┌──────┬───────┼────────┬─────────┐
              ▼      ▼       ▼        ▼         ▼
          Interpreter Rust    x86-64    WASM    (future ARM)
          src/        src/    src/      src/
          interpreter codegen native    wasm
          .rs         .rs     .rs       .rs
```

#### Interpreter (src/interpreter.rs)

- Reads JSON, evaluates expressions directly
- Simplest, most flexible
- Supports EVERYTHING (collections, quantifiers, reactions)

#### Rust Transpiler (src/codegen.rs)

- Generates Rust source code, calls `rustc`
- Binary ~441 KB (includes Rust libc)
- Supports everything except quantifiers

#### Native x86-64 (src/native.rs)

- Emits machine code bytes DIRECTLY into an ELF binary
- Binary ~400-700 bytes, zero dependencies
- Platform-specific optimizations:

| Optimization | Instruction emitted | Gain |
|---|---|---|
| SIMD (vectorizable) | pcmpgtq (SSE4.2) | 2 values/cycle |
| Fork (parallel) | syscall 57 (fork) | 2 CPU cores |
| Magic division (/ N) | mul + shr | 4 cycles vs 40 |
| Shift (* 2^n) | shl | 1 cycle vs 3 |
| Dead branch | (removed) | 0 instructions |
| Constant | mov rax, value | 0 computation |

#### WASM (src/wasm.rs)

- Emits a WebAssembly binary module
- ~60 bytes, runs in browsers (Chrome, Firefox, Safari, Node.js)
- Stack-based VM (no registers, simpler than x86)

---

## Two Types of Blocks

```text
┌─────────────────────────────────────┐
│  RULE (pure)                        │
│                                     │
│  Input → Computation → Output       │
│  No side effects                    │
│  Compiler optimizes aggressively    │
│  SIMD, fork, dead code, everything  │
│                                     │
│  Example:                           │
│    important = i.amount > 10000     │
└─────────────────────────────────────┘
            │
            │ trigger
            ▼
┌─────────────────────────────────────┐
│  REACTION (declared effects)        │
│                                     │
│  Listens to a trigger (rule)        │
│  If true → executes declared effects│
│  Effects listed explicitly          │
│  No hidden side effects             │
│                                     │
│  Example:                           │
│    trigger: important_invoice       │
│    effects:                         │
│      print "ALERT: Important!"      │
└─────────────────────────────────────┘
```

---

## Project Files

```text
src/
  main.rs          CLI entry point, import resolution, dispatch
  lexer.rs         Text → Tokens (with INDENT/DEDENT)
  parser.rs        Tokens → AST (recursive descent)
  ast.rs           All AST types (Expr, Rule, Concept, Reaction...)
  verifier.rs      Zero-trust verification + interval arithmetic
  optimizer.rs     Universal optimizations (constant fold, dead code...)
  interpreter.rs   Direct evaluation on JSON
  codegen.rs       Rust source code generation
  native.rs        x86-64 machine code emission + ELF builder
  wasm.rs          WebAssembly module emission
  validate_x86.rs  Self-verification of emitted machine code

examples/
  invoices.*       Minimal example (1 concept, 1 rule)
  business.*       Arithmetic + composition (4 rules, let bindings)
  clients.*        Text type + string comparison
  collections.*    all/any quantifiers with lambdas
  pricing.*        Nested if/else + let bindings
  deadcode.*       Dead branch elimination demo
  showcase.*       ALL features in one coherent scenario
  reactions.*      First reaction (declared side effects)
  app.* + stdlib/  Module system (use + import)
  demo.html        Browser demo (WASM)
```

---

## The Complete Pipeline

```text
HUMAN                    AI                      COMPILER
─────                    ──                      ────────

"An invoice is      →  concept Invoice         → LEXER
 important when        rule important_invoice      ↓
 its amount            proofs: pure, bound=1    PARSER
 exceeds 10000"        hints: vectorizable         ↓
                                                RESOLVER (imports)
 invoices.intent       invoices.verbose            ↓
                                                VERIFIER
                                                  reads ✓
                                                  purity ✓
                                                  bound ✓
                                                  overflow ✓
                                                   ↓
                                                OPTIMIZER
                                                  constant fold
                                                  dead code
                                                   ↓
                                                BACKEND
                                              ┌────┼────┐
                                              ▼    ▼    ▼
                                            x86  WASM  Rust
                                            570B  60B  441KB
```

---

## Glossary

| Term | Meaning |
|---|---|
| AST | Abstract Syntax Tree — the tree representing code in memory |
| Token | A word recognized by the lexer (number, identifier, operator) |
| INDENT/DEDENT | Tokens emitted when indentation increases/decreases |
| Recursive descent | Parsing technique: each grammar rule = one function |
| Zero trust | The compiler never trusts — it verifies everything |
| Interval arithmetic | Computing bounds: [min, max] for each sub-expression |
| Constant folding | Computing 100+20=120 at compile time, not at runtime |
| Dead code | Code that will never execute (impossible branch) |
| Strength reduction | Replacing a slow operation with a faster one (×4 → shift) |
| Magic division | Replacing ÷100 with ×magic_number>>shift (4 cycles vs 40) |
| SIMD | Single Instruction Multiple Data — processing 2+ values at once |
| ELF | Linux binary format (Executable and Linkable Format) |
| WASM | WebAssembly — bytecode that runs in browsers |
| Peephole | Local optimization: scanning emitted code for useless patterns |
| CSE | Common Subexpression Elimination — compute once, reuse |
| Purity | No side effects — result depends only on inputs |
| Reaction | Block with declared side effects (print, write, send) |
| Trigger | The pure rule that activates a reaction |
| LEB128 | Variable-length integer encoding (used in WASM) |

---

*This document covers Verbose v0.1.0 — 4 backends, 84+ tests, 0 dependencies.*
