# Verbose Compiler (verbosec)

## What is this project?

Verbose IR is a **new programming language** designed around a radical idea: what if the code was written by an AI, not a human?

Traditional languages (Python, Rust, C) are designed for humans to write concisely. Verbose IR is the opposite — it's intentionally verbose because an AI generates it in seconds, and all that extra information (proofs, optimization hints, traceability) lets the compiler produce better binaries without guessing.

The pipeline: **human intention** (.intent) → **AI generates IR** (.verbose) → **compiler verifies proofs** → **executes or compiles**.

## Architecture

```
src/
  lexer.rs         Turns .verbose text into tokens (INDENT/DEDENT like Python)
  parser.rs        Turns tokens into a typed AST (Abstract Syntax Tree)
  ast.rs           Pure data types that define what the AST looks like
  verifier.rs      Checks that AI-generated proofs are actually true (zero-trust)
  interpreter.rs   Evaluates rules on JSON data
  main.rs          CLI: ties everything together (parse → verify → execute)

examples/
  invoices.intent  Human intention: plain text, numbered lines
  invoices.verbose The IR: concept + rule + structured proofs
  invoices.json    Test data to run rules against
```

## Key Concepts

- **Concept**: a data entity (like a struct). Has typed fields.
- **Rule**: a pure function that computes something from a concept. Has input, output, logic, and proofs.
- **Proofs**: structured declarations (purity, termination, determinism) that the compiler verifies against the actual logic AST. The AI claims something, the compiler checks it.
- **@source**: every block traces back to a line in the .intent file, maintaining the link between human intention and generated code.

## Language Syntax (POC subset)

- Indentation-significant (spaces only, no tabs)
- Comments: `--`
- Attributes: `@verbose` (version), `@intention` (string), `@source` (file:line)
- Keywords: `concept`, `rule`, `fields`, `input`, `output`, `logic`, `proofs`
- Proof keywords: `purity`, `termination`, `determinism`
- Types: `number`, `bool`, or named types referencing concepts

## Development Rules

- **Always explain what you're doing and why.** The creators of this project are learning alongside the AI. Every change, design decision, and architectural choice must be documented clearly in conversation.
- **No silent changes.** If you modify something, explain what changed, why, and what impact it has.
- **Explain concepts when they arise.** Don't assume the reader knows compiler theory, type systems, or Rust internals.
- **Zero external dependencies** for the POC — everything is hand-written.
- **Zero-trust verification** — the compiler never trusts AI-generated proofs; it verifies them.
- **All 29 tests must pass** before any commit (`cargo test`).
- **Closed attributes** — unknown `@attributes` are rejected, not silently ignored.

## Running

```bash
# Verify a .verbose file (checks all proofs)
cargo run -- examples/invoices.verbose

# Verify AND execute a rule on JSON data
cargo run -- examples/invoices.verbose --run important_invoice --input examples/invoices.json

# Run the test suite
cargo test
```

## License

Apache 2.0
