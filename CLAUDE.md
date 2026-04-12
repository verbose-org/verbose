# Verbose Compiler (verbosec)

## Vision

Verbose is a language where:
- **the AI expresses its reasoning explicitly** — proofs, hints, dependencies, all declared
- **the human can audit it** — every block traces to a numbered intention line
- **the compiler verifies, never guesses** — proofs are checked against the AST, not trusted
- **the compiler exploits declarations for optimization** — not just safety, also performance

The identity is: **explicit + verified + optimized**. Without optimization, it's just Coq with better syntax. Without verification, it's just a transpiler. Both halves matter.

## Design Priorities

```
1. Verifiability     — every declaration is mechanically verifiable
2. Exploitability    — every declaration is USED (optimization, codegen, analysis)
3. Safety            — unproven code is rejected
4. Traceability      — intention -> IR -> binary always navigable
5. Readability       — auditable without blind spots
```

Key filter: if a declaration serves neither verification nor optimization, it doesn't belong in the IR. This prevents *false explicitation* (verbose noise that looks rigorous but isn't mechanically checked).

## Architecture

```
src/
  lexer.rs         Tokens with Python-style INDENT/DEDENT
  parser.rs        Recursive-descent parser -> typed AST
  ast.rs           Pure data types for the AST
  verifier.rs      Zero-trust proof verification (8 checks)
  interpreter.rs   Expression evaluator on JSON data
  codegen.rs       Rust source code generation (transpiler backend)
  native.rs        Direct x86-64 machine code generation (native backend)
  main.rs          CLI entry point

examples/
  invoices.*       Minimal example (1 concept, 1 rule)
  business.*       Arithmetic + rule composition (4 rules, 3 fields)
  clients.*        Text type + string comparison
  collections.*    Nested data with all/any quantifiers
```

## Language Features (current)

- Types: `number`, `bool`, `text`, `collection(Type)`, named types
- Expressions: arithmetic (+, -, *, /), comparisons (>, <, >=, <=, ==, !=), boolean (and, or)
- Rule calls: `important_invoice(i)` — rules can compose
- Quantifiers: `all(collection, var => predicate)`, `any(...)`
- Proofs: purity (reads/writes/calls/verdict), termination (form/bound), determinism (form)
- Traceability: `@intention` (string), `@source` (file:line)
- Three backends: interpreter (--run), Rust transpiler (--compile), native x86-64 (--native)

## Development Rules

- **Always explain what you're doing and why.** The creators are learning alongside the AI. Every change must be explained clearly.
- **No silent changes.** Explain what changed, why, and what impact it has.
- **Explain concepts when they arise.** Don't assume knowledge of compiler theory or Rust internals.
- **Zero external dependencies** — everything is hand-written.
- **Zero-trust verification** — the compiler verifies AI proofs, never trusts them.
- **All tests must pass** before any commit (`cargo test`).
- **Closed attributes** — unknown `@attributes` are rejected, not silently ignored.
- **No false explicitation** — every declaration must be mechanically verified or exploited. If it's just decoration, remove it.
- **The native backend is the destination** — the Rust transpiler is a fallback. Architectural decisions should keep the native path open.

## Running

```bash
cargo run -- examples/collections.verbose                                           # verify
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json  # interpret
cargo run -- examples/business.verbose --compile /tmp/business                      # transpile to Rust
cargo run -- examples/business.verbose --native /tmp/biz --run critical_invoice     # native x86-64
cargo test                                                                          # 37 tests
```

## License

Apache 2.0
