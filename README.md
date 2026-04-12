# Verbose

**A new language for AI, pushed by humans.**

Verbose is not a language that asks the compiler to be intelligent. It's a language that asks the author to be explicit. And the author is an AI.

## The Problem

Traditional languages compress intention. The programmer thinks "filter overdue invoices", writes `invoices.filter(|i| i.days > 30)`, and the compiler reverse-engineers what it can — guessing at purity, parallelism, memory layout, and termination. Most of the time it guesses right. Sometimes it doesn't.

AI-generated code makes this worse. The AI generates plausible-looking code, but nobody verifies whether it's actually correct, safe, or optimal. We trust and hope.

## The Idea

What if the AI could be *exhaustive* instead of concise?

A human would never write 200 lines for a 25-line function. But an AI generates them in seconds. And those extra 175 lines can carry **proofs** (purity, termination, determinism), **optimization hints** (vectorizable, parallelizable, memory layout), and **traceability** (every line traces back to a human intention).

The compiler doesn't guess. It **verifies** the proofs, **exploits** the hints, and **rejects** anything unproven.

## The Pipeline

```
human intention (.intent)     "An invoice is overdue when it has more than 30 days"
        |
AI generates IR (.verbose)    rule + fields + proofs + hints
        |
compiler verifies proofs      purity? termination? determinism? field access?
        |
compiler produces binary      interpreter, Rust transpiler, or native x86-64
```

## Three Axioms

1. **Nothing is implicit.** Every block carries all information needed for its verification and optimization. No inter-block analysis required.
2. **Intention survives.** Every element traces back to its human origin. The reverse path (binary -> IR -> intention) is always navigable.
3. **The compiler never guesses.** Every optimization decision is backed by a verifiable proof or explicit declaration in the IR.

## Design Priorities

```
1. Verifiability     — every declaration is mechanically verifiable
2. Exploitability    — every declaration is USED by the compiler (optimization, codegen, analysis)
3. Safety            — unproven code is rejected
4. Traceability      — the path intention -> IR -> binary is always navigable
5. Readability       — auditable without blind spots
```

If a declaration serves neither verification nor optimization, it doesn't belong in the IR. This is the filter against *false explicitation* — verbose for the sake of verbose.

## What It Looks Like

```verbose
@verbose 0.1.0

concept Invoice
  @intention: "An invoice has an amount and a number of days overdue"
  @source: invoices.intent:1
  fields:
    amount       : number
    days_overdue : number

rule invoice_overdue
  @intention: "An invoice is overdue when it has more than 30 days overdue"
  @source: invoices.intent:2
  input:
    inv : Invoice
  output:
    overdue : bool
  logic:
    overdue = inv.days_overdue > 30
  proofs:
    purity:
      reads   : [inv.days_overdue]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
```

The proofs are not comments. They are structured declarations that the compiler verifies against the actual logic AST. If the AI claims `reads: []` but the code reads `inv.days_overdue`, the compiler rejects it.

## What It Can Do (POC)

| Feature | Example |
|---|---|
| Concepts with typed fields | `number`, `bool`, `text`, `collection(Type)` |
| Arithmetic expressions | `i.amount + i.amount * i.tax_rate / 100` |
| Comparisons | `>`, `<`, `>=`, `<=`, `==`, `!=` |
| Boolean logic | `and`, `or` |
| String comparison | `c.status == "active"` |
| Rule composition | `critical = important_invoice(i) and overdue_invoice(i)` |
| Collection quantifiers | `all(c.invoices, inv => invoice_overdue(inv))` |
| Lambda syntax | `any(c.invoices, inv => inv.days_overdue > 30)` |
| Zero-trust proof verification | 8 checks against the AST |
| Three backends | interpreter (JSON), Rust transpiler, native x86-64 ELF |

The native backend produces a 542-byte standalone binary with zero dependencies — 832x smaller than the Rust transpiler output for the same logic.

## Running

```bash
# Verify a .verbose file (checks all proofs)
cargo run -- examples/collections.verbose

# Verify and execute a rule on JSON data
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json

# Compile to standalone binary via Rust transpiler
cargo run -- examples/business.verbose --compile output_binary

# Compile to native x86-64 ELF (zero dependencies, ~500 bytes)
cargo run -- examples/business.verbose --native output_binary --run total_with_tax

# Run tests
cargo test
```

## Project Status

This is a **POC / R&D project**. The language works, the compiler verifies proofs, and three backends produce correct results. The vision is validated. What remains is expanding the language (more types, more proof forms, optimization hints exploitation) and hardening the compiler.

## License

Apache 2.0
