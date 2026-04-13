# Verbose

[![CI](https://github.com/verbose-org/verbose/actions/workflows/ci.yml/badge.svg)](https://github.com/verbose-org/verbose/actions/workflows/ci.yml)

Verbose is an experimental AI-native intermediate representation designed to sit between human intention and machine execution.

It is based on a simple idea: modern AI systems can generate far more detail than traditional human-oriented programming languages can naturally carry — but that extra expressive power must remain **explicit**, **verifiable**, **auditable**, and **compilable**.

Verbose explores that space.

> *Making exhaustiveness a compilable material.*

Instead of asking the compiler to guess, Verbose asks the authoring system to declare:
- what the program means
- what it reads and writes
- what properties are expected to hold
- what optimization intentions are desired
- where the intent came from

Then the compiler verifies those claims under a zero-trust model and lowers the result to executable targets: native x86-64, WebAssembly, or Rust.

---

## Why Verbose Exists

Traditional languages were designed primarily for human authors.

AI-generated software changes the situation: an AI system can expand an intent into a much richer representation than most conventional languages are built to hold.

Verbose is an attempt to answer this question: **can we give AI a more explicit and more machine-relevant form of expression, while keeping strict control through verification, traceability, and compilation?**

## Roles

- **Human:** states the intention, constraints, and acceptance criteria
- **AI:** expands that intention into a highly explicit Verbose program
- **Verbose IR:** carries logic, proofs, optimization hints, and provenance
- **Compiler:** verifies everything, rejects false claims, and lowers to executable code

```text
human intention (.intent)     "An invoice is overdue when it has more than 30 days"
        │
AI generates IR (.verbose)    rule + fields + proofs + hints
        │
compiler verifies proofs      purity? termination? determinism? field access?
        │
compiler produces binary      interpreter, Rust transpiler, native x86-64, or WASM
```

## What Verbose Is Not

Verbose is not trying to replace mainstream languages.

It is not a general-purpose language optimized for human ergonomics.

It is an **explicit, specialized representation** meant for auditable, optimizable, and verifiable program generation.

## Live Example

Human writes this (`collections.intent`):
```text
1. A client has a name and a list of invoices.
2. An invoice is overdue when it has more than 30 days overdue.
3. A client is blocked when all their invoices are overdue.
```

AI generates this (`collections.verbose`):
```verbose
rule client_blocked
  @intention: "A client is blocked when all their invoices are overdue"
  @source: collections.intent:3

  input:
    c : Client
  output:
    blocked : bool
  logic:
    blocked = all(c.invoices, inv => invoice_overdue(inv))

  proofs:
    purity:
      reads   : [c.invoices]
      writes  : []
      calls   : [invoice_overdue]
      verdict : pure
    termination:
      form  : constant_bound
      bound : 2
    determinism:
      form : total
```

Compiler verifies and runs:
```text
$ verbosec collections.verbose --run client_blocked --input data.json

verified: 2 concept(s), 3 rule(s); all proofs check out

executing rule 'client_blocked' on 4 record(s):
  [0] blocked = true     ← Dupont: all invoices overdue
  [1] blocked = false    ← Martin: no invoices overdue
  [2] blocked = false    ← Durand: only 1 of 2 overdue
  [3] blocked = true     ← Lefevre: empty collection (⚠ edge case flagged)
```

If the AI lies in its proofs — the compiler catches it:
```text
verify error [rule 'client_blocked' / purity.reads] declared reads do not match logic; missing: [c.invoices]
```

## Numbers

| | |
|---|---|
| Lines of Rust | ~7200, zero external dependencies |
| Tests | 84, all passing |
| Native binary size | **407–676 bytes** for business logic, **498 bytes** for HTTP server |
| WASM module size | **58–73 bytes** for browser execution |
| Proof checks | 10+ zero-trust verifications against the AST |
| Examples | 10 across business, finance, collections, pricing, and more |

## Verbose vs gcc -O3

Same logic (`amount > 10000`), same input, same output:

| | gcc -O3 -s (production, stripped) | Verbose native |
|---|---|---|
| Binary size | 14,472 bytes | **589 bytes** (24x smaller) |
| Dependencies | 3 shared libraries (libc) | **Zero** |
| Proofs | None | Purity, termination, determinism |
| Overflow safety | Undefined behavior | Proven via interval arithmetic |
| SIMD | Must analyze (may miss) | Declared + verified (`pcmpgtq`) |
| Traceability | None | Every instruction → source intention |

gcc has 20 years of register allocation and instruction scheduling. Verbose has domain knowledge that gcc will never have.

## Three Axioms

1. **Nothing is implicit.** Every block carries all information needed for verification and optimization.
2. **Intention survives.** Every element traces back to its human origin. The reverse path (binary → IR → intention) is always navigable.
3. **The compiler never guesses.** Every decision is backed by a verifiable proof or explicit declaration.

## Optimization Philosophy

Verbose does not treat optimization as a hidden compiler trick.

Optimization intent should be declared explicitly whenever possible. The compiler's role is to verify, reorganize, and materialize those decisions safely across targets.

The long-term direction is to let the representation carry not only semantic intent, but also execution intent: vectorization, parallelism, resource sensitivity, and potentially architecture-aware preferences.

## Design Priorities

```text
1. Verifiability     every declaration is mechanically verifiable
2. Exploitability    every declaration is USED by the compiler
3. Safety            unproven code is rejected
4. Traceability      intention → IR → binary always navigable
5. Readability       auditable without blind spots
```

If a declaration serves neither verification nor optimization, it doesn't belong in the IR.

## What Works Today

### Language Features

| Feature | Example |
|---|---|
| Typed concepts | `number`, `bool`, `text`, `collection(Type)` |
| Field value ranges | `temperature : number [0, 50]` |
| Arithmetic | `amount + amount * tax_rate / 100` |
| Comparisons & equality | `>`, `<`, `>=`, `<=`, `==`, `!=` |
| Boolean logic | `and`, `or`, `not` |
| Parentheses & negation | `(a + b) * c`, `-amount` |
| If/then/else | `if days > 90 then 20 else if days > 30 then 10 else 0` |
| Let bindings (CSE) | `let tax = amount * rate / 100` |
| String comparison | `status == "active"` |
| Rule composition | `important(i) and overdue(i)` |
| Collection quantifiers | `all(invoices, inv => inv.days > 30)` |
| Module system | `use "stdlib/finance.verbose"` |
| Reactions | Declared side effects triggered from verified rules |

### Proof Verification (Zero-Trust)

| Check | What it verifies |
|---|---|
| Purity reads | Declared reads == actual field accesses in AST |
| Purity writes | Declared writes == actual mutations (must be empty for pure) |
| Purity calls | Declared calls == actual rule invocations in AST |
| Purity verdict | `pure` consistent with empty writes/calls |
| Termination bound | Declared bound ≥ actual operation count |
| Determinism | `total` consistent with call purity |
| Source traceability | `@source: file:line` points to existing line |
| Field existence | Accessed fields exist on the input concept |
| Logic/output coherence | Logic target matches declared output name |
| Called rules exist | All called rules are defined in the program |
| Overflow bounds | Interval arithmetic proves declared range |
| Stack depth | Expression nesting within safety limits |

### Optimization Hints (Exploited by Compiler)

| Hint | What the compiler does | Why gcc can't |
|---|---|---|
| `vectorizable: yes` | Emits SSE4.2 `pcmpgtq` — 2 values per CPU cycle | Requires costly loop analysis |
| `parallel: yes` | Uses `fork()` — real multi-core parallelism | Developer must do it manually |
| `overflow: [min, max]` | Proves safe via interval arithmetic — no runtime check | C = undefined behavior, Rust = runtime panic |
| `field [min, max]` | Eliminates impossible branches from binary | Doesn't know value bounds |

### Compile-Time Optimizations

| Optimization | Example | Impact |
|---|---|---|
| Constant folding | `100 / 2` → `50` at compile time | Zero runtime cost |
| Strength reduction | `x * 4` → `shl rax, 2` | 1 cycle instead of 3 |
| Magic division | `x / 100` → `mul + shr` | 4 cycles instead of 40 |
| Dead branch elimination | `if temp > 100` with range [0,50] → removed | Fewer instructions |
| SIMD vectorization | Comparison → `pcmpgtq` | 2 results per instruction |
| Let binding CSE | `let tax = expr` → compute once, load N times | No redundant work |
| Peephole optimization | Redundant push/pop eliminated | Smaller binary |

### Four Backends

| Backend | Command | Output |
|---|---|---|
| Interpreter | `--run rule --input data.json` | Executes directly on JSON data |
| Rust transpiler | `--compile output` | Standalone binary via `rustc` |
| Native x86-64 | `--native output --run rule` | ELF binary, zero dependencies (~400-700 bytes) |
| WebAssembly | `--wasm output.wasm --run rule` | WASM module for browsers (~60 bytes) |

## Getting Started

```bash
git clone https://github.com/verbose-org/verbose.git
cd verbose
cargo test                    # 84 tests
cargo run -- examples/showcase.verbose   # verify all proofs
cargo run -- examples/showcase.verbose --run bonus_rate --input examples/showcase.json
```

All backends:
```bash
cargo run -- examples/business.verbose --compile /tmp/business          # Rust
cargo run -- examples/business.verbose --native /tmp/biz --run total_with_tax  # x86-64
cargo run -- examples/business.verbose --wasm /tmp/rule.wasm --run total_with_tax  # WASM
cargo run -- examples/invoices.verbose --benchmark --run important_invoice  # compare all
```

Browser demo:
```bash
cargo run -- examples/business.verbose --wasm examples/demo.wasm --run total_with_tax
cd examples && python3 -m http.server 8000
# Open http://localhost:8000/demo.html
```

## The Generation Question

Who writes the `.verbose` files?

**An AI does.** Not the compiler — a separate AI (Claude, GPT, or any future model). The human writes the `.intent` file (plain language), the AI generates the `.verbose` IR with all its proofs and hints, and the compiler verifies everything.

```text
AI (non-deterministic)        generates .verbose — may hallucinate, may be wrong
verbosec (deterministic)      verifies and compiles — never trusts, never guesses
```

The compiler will never generate code. It will never "help" the AI by inferring missing proofs. It verifies, or it rejects. Like a financial auditor: if the accountant and the auditor are the same person, the audit is worthless.

A generation tool is included (`tools/generate.sh`) — it calls the Claude API to produce `.verbose` files from `.intent` files. It is deliberately separate from the compiler:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
./tools/generate.sh examples/invoices.intent > generated.verbose
cargo run -- generated.verbose   # compiler verifies independently
```

## Why Not LLVM?

LLVM loses the information that makes Verbose unique. When translating to LLVM IR, domain knowledge is stripped: field ranges, optimization hints, purity proofs, overflow bounds — all gone. LLVM then spends dozens of analysis passes trying to re-discover what Verbose already knew.

Verbose native binaries are 400-700 bytes. LLVM would produce 10-50 KB minimum (function prologues, stack protectors, alignment padding, exception handling).

LLVM may become an optional backend for platforms we don't support natively. But the primary path stays direct — that's where the advantage lives.

## Long-Term Direction

Verbose explores a future where AI-generated programs carry enough explicit information to inform not only correctness and optimization, but also execution strategy and target preference.

In that future, a program description may express:
- semantic logic
- proofs and invariants
- optimization intent
- side effects
- preferred execution properties
- target-aware compilation hints

The compiler remains the final arbiter.

## Status

**POC / R&D.** 52 commits, ~7200 lines, 84 tests, 0 dependencies, 4 backends.

```bash
cargo run -- examples/invoices.verbose --benchmark --run important_invoice
```

## Origin

This project started as an open question: *"If AI writes code now, do we still need languages designed for humans?"*

A few hours later, the question had become a working compiler with verified proofs, four backends, SIMD optimization, and a 498-byte HTTP server.

No spec committee. No funding. No team. One human with a vision, one AI that codes, and a question that turned out to have a very concrete answer.

## License

Apache 2.0

## Author

Created by Yoan Roblet ([@Arcker](https://github.com/Arcker)).

The vision, the architecture decisions, and every "no" that kept the project on track came from a human. The Rust code came from an AI. The compiler trusts neither.
