# Verbose

[![CI](https://github.com/verbose-org/verbose/actions/workflows/ci.yml/badge.svg)](https://github.com/verbose-org/verbose/actions/workflows/ci.yml)

**A language designed for AI, verified by compiler, pushed by humans.**

> *Verbose is not a language that asks the compiler to be intelligent.*
> *It's a language that asks the author to be explicit.*
> *And the author is an AI.*
>
> *Making exhaustiveness a compilable material.*

---

## The Question

AI writes code now. It writes Python, Rust, JavaScript — languages designed for **humans** to read and write concisely.

But if the AI is the author, why constrain it to a human format? Why force concision when the AI can be exhaustive? Why compress intention when it could be preserved?

And most importantly: **who verifies that the AI's code is correct?**

Right now, nobody. We trust and hope. Verbose exists because that's not good enough.

## The Idea

Let the AI express itself **fully** — not in 25 lines of elegant Python, but in 200 lines that carry:

- **Proofs** — purity, termination, determinism — declared and verified
- **Optimization hints** — vectorizable, parallelizable, cache-friendly — exploited by the compiler
- **Traceability** — every line traces back to a human intention

A human would never write this. An AI generates it in seconds. And the compiler doesn't guess — it **verifies** the proofs, **exploits** the hints, and **rejects** anything unproven.

```
Traditional code:    intention → compress → code → compiler guesses → binary
Verbose:             intention → AI expands → IR + proofs → compiler verifies → binary
```

## Live Example

Human writes this (`collections.intent`):
```
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
```
$ verbosec collections.verbose --run client_blocked --input data.json

verified: 2 concept(s), 3 rule(s); all proofs check out

executing rule 'client_blocked' on 4 record(s):
  [0] blocked = true     ← Dupont: all invoices overdue
  [1] blocked = false    ← Martin: no invoices overdue
  [2] blocked = false    ← Durand: only 1 of 2 overdue
  [3] blocked = true     ← Lefevre: empty collection (⚠ edge case flagged by spec)
```

If the AI lies in its proofs — the compiler catches it:
```
verify error [rule 'client_blocked' / purity.reads] declared reads do not match logic; missing: [c.invoices]
```

## Numbers

| | |
|---|---|
| Lines of Rust | ~7200, zero external dependencies |
| Tests | 84, all passing |
| Native binary size | **407–676 bytes** for business logic, **498 bytes** for HTTP server |
| Rust transpiler output | 441 KB for the same logic (832x larger) |
| Proof checks | 8 zero-trust verifications against the AST |
| Examples | 10 (invoices, business, clients, collections, pricing, deadcode, showcase, reactions, app+stdlib, HTML demo) |

## Verbose vs gcc -O3

Same logic (`amount > 10000`), same input, same output:

| | gcc -O3 -s (production, stripped) | Verbose native |
|---|---|---|
| Binary size | 14,472 bytes | **589 bytes** (24x smaller) |
| gcc -Os -s (size-optimized) | 14,472 bytes (same) | **589 bytes** |
| Dependencies | 3 shared libraries (libc) | **Zero** |
| Proofs | None | Purity, termination, determinism |
| Overflow safety | Undefined behavior | Proven via interval arithmetic |
| SIMD | Must analyze (may miss) | Declared + verified (`pcmpgtq`) |
| Traceability | None | Every instruction → source intention |

gcc has 20 years of register allocation and instruction scheduling that we don't have yet. But Verbose has domain knowledge that gcc will never have.

## Three Axioms

1. **Nothing is implicit.** Every block carries all information needed for verification and optimization.
2. **Intention survives.** Every element traces back to its human origin. The reverse path (binary → IR → intention) is always navigable.
3. **The compiler never guesses.** Every decision is backed by a verifiable proof or explicit declaration.

## Design Priorities

```
1. Verifiability     every declaration is mechanically verifiable
2. Exploitability    every declaration is USED by the compiler
3. Safety            unproven code is rejected
4. Traceability      intention → IR → binary always navigable
5. Readability       auditable without blind spots
```

If a declaration serves neither verification nor optimization, it doesn't belong in the IR. Verbose without purpose is just noise.

## What Works Today

### Language Features

| Feature | Example |
|---|---|
| Typed concepts | `number`, `bool`, `text`, `collection(Type)` |
| Field value ranges | `temperature : number [0, 50]` |
| Arithmetic | `amount + amount * tax_rate / 100` |
| Comparisons & equality | `>`, `<`, `>=`, `<=`, `==`, `!=` |
| Boolean logic | `and`, `or`, `not` |
| Parentheses | `(a + b) * c` |
| Unary negation | `-amount` |
| If/then/else | `if days > 90 then 20 else if days > 30 then 10 else 0` |
| Let bindings (CSE) | `let tax = amount * rate / 100` |
| String comparison | `status == "active"` |
| Rule composition | `important(i) and overdue(i)` |
| Collection quantifiers | `all(invoices, inv => inv.days > 30)` |
| Lambda syntax | `any(list, item => predicate)` |

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

### Optimization Hints (Exploited by Compiler)

| Hint | What the compiler does | Why gcc can't |
|---|---|---|
| `vectorizable: yes` | Emits SSE4.2 `pcmpgtq` — 2 values per CPU cycle | Requires costly loop analysis |
| `parallel: yes` | Uses `fork()` — real multi-core parallelism | Developer must do it manually |
| `overflow: [min, max]` | Proves safe via interval arithmetic — no runtime check | C = undefined behavior, Rust = runtime panic |
| `field [min, max]` | Eliminates impossible branches from binary | Doesn't know value bounds |

### Compile-Time Optimizations (Native Backend)

| Optimization | Example | Machine code impact |
|---|---|---|
| Constant folding | `100 / 2` → `50` at compile time | Zero runtime instructions |
| Strength reduction | `x * 4` → `shl rax, 2` | 1 cycle instead of 3 (imul) |
| Multiply by 0/1 | `x * 0` → `xor rax, rax` | No computation |
| Add/sub 0 | `x + 0` → identity | No instruction emitted |
| Dead branch elimination | `if temp > 100` with range [0,50] → branch removed | Fewer jumps, smaller binary |
| SIMD vectorization | Comparison → `pcmpgtq` | 2 results per instruction |
| Let binding CSE | `let tax = expr` → compute once, load N times | No redundant evaluation |

### Three Backends

| Backend | Command | Output |
|---|---|---|
| Interpreter | `--run rule --input data.json` | Executes directly on JSON data |
| Rust transpiler | `--compile output` | Standalone binary via `rustc` (~441 KB) |
| Native x86-64 | `--native output --run rule` | ELF binary, zero dependencies (~400-700 bytes) |
| WebAssembly | `--wasm output.wasm --run rule` | WASM module for browsers (~60 bytes) |

## Getting Started

```bash
git clone https://github.com/verbose-org/verbose.git
cd verbose
cargo test                    # 51 tests, should all pass
cargo run -- examples/collections.verbose   # verify proofs
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json
```

Other backends:
```bash
# Compile to standalone Rust binary
cargo run -- examples/business.verbose --compile /tmp/business

# Compile to native x86-64 ELF (zero dependencies)
cargo run -- examples/business.verbose --native /tmp/biz --run total_with_tax

# Compile to WebAssembly (runs in browsers)
cargo run -- examples/business.verbose --wasm /tmp/rule.wasm --run total_with_tax

# Show generated Rust source
cargo run -- examples/pricing.verbose --emit-rust
```

Try it in a browser:
```bash
cargo run -- examples/business.verbose --wasm examples/demo.wasm --run total_with_tax
cd examples && python3 -m http.server 8000
# Open http://localhost:8000/demo.html
```

## The Generation Question

Who writes the `.verbose` files?

**An AI does.** Not the compiler — a separate AI (Claude, GPT, or any future model). The human writes the `.intent` file (plain language), the AI generates the `.verbose` IR with all its proofs and hints, and the compiler verifies everything. If the AI gets something wrong — a proof that doesn't hold, a bound that's too tight, a missing read — the compiler rejects it. No exceptions.

```
AI (non-deterministic)        generates .verbose — may hallucinate, may be wrong
verbosec (deterministic)      verifies and compiles — never trusts, never guesses
```

The compiler will never generate code. It will never "help" the AI by inferring missing proofs. It verifies, or it rejects. Like a financial auditor: if the accountant and the auditor are the same person, the audit is worthless.

As AI models grow more capable (larger context, better reasoning), they'll generate more complex and correct Verbose IR. The compiler doesn't need to change — it just verifies whatever the AI produces. A dedicated generation tool (separate from the compiler) is planned for the future.

## Why Not LLVM?

LLVM is a powerful general-purpose compiler backend used by Rust, Clang, and Swift. Verbose doesn't use it for the primary path. Here's why:

**LLVM loses the information that makes Verbose unique.**

When translating Verbose IR to LLVM IR, domain knowledge is stripped:

```
Verbose IR:                          LLVM IR:
  field: amount [0, 1000000]    →    %amount = i64       (no bounds known)
  hints: vectorizable: yes      →    (lost — LLVM must re-discover)
  proofs: pure                  →    (lost — LLVM must re-analyze)
  overflow: [0, 100]            →    (lost — LLVM doesn't know)
```

LLVM then spends dozens of analysis passes trying to re-discover what Verbose already knew. Sometimes it succeeds. Often it can't — because the information was never expressible in LLVM IR to begin with.

**What LLVM adds that we don't need:**

| Overhead | Why LLVM adds it | Why Verbose skips it |
|---|---|---|
| Function prologues/epilogues | Standard calling convention | We emit only the instructions needed |
| Stack protectors | Buffer overflow defense | Proven safe via interval arithmetic |
| Alignment padding | Cache line optimization | We control layout directly |
| Exception handling tables | Unwinding support | Pure functions can't throw |

Result: Verbose native binaries are 400-700 bytes. LLVM would produce 10-50 KB minimum.

**Where LLVM wins:** complex programs with hundreds of functions. LLVM has 20 years of register allocation, instruction scheduling, and loop optimization. For large codebases, LLVM would produce faster code than our handwritten emitter.

**Where Verbose wins:** domain-aware optimization. Division-by-constant with field-range safety. Dead branch elimination with declared bounds. SIMD guided by hints. These are impossible in LLVM because the information doesn't exist in LLVM IR.

## What Else Can It Build?

The native backend isn't limited to rule evaluation. As a proof of concept:

```bash
$ verbosec --demo-http /tmp/server
HTTP demo server: /tmp/server (498 bytes)

$ ./server &
Verbose HTTP server on port 9999

$ curl http://localhost:9999
Hello from Verbose!
```

A fully functional HTTP server in **498 bytes**. Zero dependencies. 7 Linux syscalls in a loop. For context: Nginx is 1.5 MB. Caddy is 40 MB. Node.js runtime alone is 50 MB.

This is not a language feature yet (Verbose doesn't have I/O primitives). It proves the infrastructure can produce real networked applications. The path to a Verbose HTTP server is: add reaction blocks (declared side effects) and I/O primitives to the language.

**Our strategy:**

```
Primary path:     direct machine code (maximum control, minimum size)
Fallback path:    Rust transpiler (for complex cases and portability)
Future option:    LLVM backend (for platforms we don't support natively)
```

LLVM may become an optional backend for platforms where we don't have a native emitter. But the primary path stays direct — that's where Verbose's advantage lives.

## Status

**POC / R&D.** 50 commits, ~7200 lines, 84 tests, 0 dependencies, 4 backends.

One command to see it all:
```bash
cargo run -- examples/invoices.verbose --benchmark --run important_invoice
``` The language works, the compiler verifies proofs, three backends produce correct results. The concept is validated.

## License

Apache 2.0

## Author

Created by Yoan Roblet ([@Arcker](https://github.com/Arcker)) — built with AI, verified by compiler.
