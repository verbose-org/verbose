# Verbose

**A language designed for AI, verified by compiler, pushed by humans.**

> *Verbose is not a language that asks the compiler to be intelligent.*
> *It's a language that asks the author to be explicit.*
> *And the author is an AI.*

---

## The Question

AI writes code now. It writes Python, Rust, JavaScript — languages designed for **humans** to read and write concisely.

But if the AI is the author, why constrain it to a human format? Why force concision when the AI can be exhaustive? Why compress intention when it could be preserved?

And most importantly: **who verifies that the AI's code is correct?**

Right now, nobody. We trust and hope. Verbose exists because that's not good enough.

## The Idea

Let the AI express itself **fully** — not in 25 lines of elegant Python, but in 200 lines that carry:

- **Proofs** — purity, termination, determinism — declared and verified
- **Optimization hints** — vectorizable, parallelizable, memory layout — exploited by the compiler
- **Traceability** — every line traces back to a human intention

A human would never write this. An AI generates it in seconds. And the compiler doesn't guess — it **verifies** the proofs, **exploits** the hints, and **rejects** anything unproven.

```
Traditional code:    intention → compress → code → compiler guesses → binary
Verbose:             intention → AI expands → IR + proofs → compiler verifies → binary
```

## Live Example

Human writes this (`invoices.intent`):
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

If the AI lies in its proofs — says `reads: []` when the code reads a field — the compiler catches it:
```
verify error [rule 'client_blocked' / purity.reads] declared reads do not match logic; missing: [c.invoices]
```

## Numbers

| | |
|---|---|
| Lines of Rust | ~3500, zero external dependencies |
| Tests | 37, all passing |
| Native binary size | **542 bytes** for arithmetic + boolean + rule composition |
| Rust transpiler output | 441 KB for the same logic (832x larger) |
| Proof checks | 8 zero-trust verifications against the AST |
| Time to build | One session, from spec to working compiler |

The 542-byte native binary has **zero runtime dependencies** — no libc, no allocator, no runtime. It talks directly to the Linux kernel via syscalls.

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

| Feature | Example |
|---|---|
| Typed concepts | `number`, `bool`, `text`, `collection(Type)` |
| Arithmetic | `amount + amount * tax_rate / 100` |
| Comparisons & equality | `>`, `<`, `>=`, `<=`, `==`, `!=` |
| Boolean logic | `and`, `or` |
| String comparison | `status == "active"` |
| Rule composition | `important(i) and overdue(i)` |
| Collection quantifiers | `all(invoices, inv => inv.days > 30)` |
| Zero-trust proof verification | purity, termination, determinism |
| Three backends | interpreter, Rust transpiler, native x86-64 |

## Getting Started

```bash
git clone https://github.com/verbose-org/verbose.git
cd verbose
cargo test                    # 37 tests, should all pass
cargo run -- examples/collections.verbose   # verify proofs
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json
```

Other backends:
```bash
# Compile to standalone Rust binary
cargo run -- examples/business.verbose --compile /tmp/business

# Compile to native x86-64 ELF (~500 bytes, zero dependencies)
cargo run -- examples/business.verbose --native /tmp/biz --run critical_invoice
```

## The Generation Question

Who writes the `.verbose` files?

**An AI does.** Not the compiler — a separate AI (Claude, GPT, or any future model). The human writes the `.intent` file (plain language), the AI generates the `.verbose` IR with all its proofs and hints, and the compiler verifies everything. If the AI gets something wrong — a proof that doesn't hold, a bound that's too tight, a missing read — the compiler rejects it. No exceptions.

This separation is fundamental:

```
AI (non-deterministic)        generates .verbose — may hallucinate, may be wrong
verbosec (deterministic)      verifies and compiles — never trusts, never guesses
```

The compiler will never generate code. It will never "help" the AI by inferring missing proofs. It verifies, or it rejects. Like a financial auditor: if the accountant and the auditor are the same person, the audit is worthless.

As AI models grow more capable (larger context, better reasoning), they'll generate more complex and correct Verbose IR. The compiler doesn't need to change — it just verifies whatever the AI produces. The bottleneck shifts from "can the compiler handle it" to "can the AI express it correctly." And when it can't, the compiler catches it.

A dedicated generation tool (separate from the compiler) is planned for the future.

## Status

**POC / R&D.** The language works, the compiler verifies proofs, three backends produce correct results. The concept is validated.

What the compiler does today that general-purpose compilers cannot:

| Optimization | How | Why gcc/rustc can't |
|---|---|---|
| SIMD vectorization | `vectorizable: yes` hint → SSE4.2 `pcmpgtq` | Requires costly loop analysis |
| Multi-core parallelism | `parallel: yes` hint → `fork()` | Developer must do it manually |
| Overflow elimination | `overflow: [min, max]` → interval arithmetic proof | C = undefined behavior, Rust = runtime panic |
| Dead code elimination | Field ranges `[0, 50]` → branch pruning | Doesn't know value bounds |

## License

Apache 2.0

## Author

Created by Yoan Roblet ([@Arcker](https://github.com/Arcker)) — built with AI, verified by compiler.
