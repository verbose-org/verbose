# Agent guide — Verbose Compiler

This guide distills the working principles from [CLAUDE.md](CLAUDE.md).
Keep the detailed design journal there; use this file for day-to-day work.

## Read first

- [docs/current-status.md](docs/current-status.md): present implementation scope,
  backend boundaries, and known limits of the guarantees.
- [ARCHITECTURE.md](ARCHITECTURE.md): compiler pipeline and implementation map.
- [CLAUDE.md](CLAUDE.md): architectural decisions and historical rationale.
- [docs/design-lessons.md](docs/design-lessons.md): lessons to read before proposing
  substantial design changes.
- [docs/spec-proofs.md](docs/spec-proofs.md): what individual declarations actually
  establish.

Dated milestones describe their own revision. Check the current implementation
before treating a journal entry, test count, binary size, or gap as current fact.

## Project direction

Verbose is a language for explicit, verifiable declarations that support
optimization and human auditing. Its priorities are verifiability,
exploitability, safety, traceability, and readability, in that order.

This repository is the canonical language and compiler implementation. Separate
POCs consume the language; their needs do not automatically justify new features.
The current direction includes developing the compiler written in Verbose under
human direction. Full parity with the Rust compiler is not established.

## Repository map

- `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`: syntax and AST.
- `src/verifier.rs`: declaration and proof checks.
- `src/optimizer.rs`: platform-independent optimizations.
- `src/native.rs`, `src/wasm.rs`, `src/interpreter.rs`: native x86-64 emission,
  WebAssembly emission, and reference execution.
- `src/validate_x86.rs`: emitted instruction validation.
- `src/main.rs`: CLI and import resolution.
- `examples/`: feature examples and fixtures; `examples/vexprparse.verbose` is
  the self-hosted compiler source.
- `examples/holdout/`: holdout intents, kept separate from generation examples.
- `tools/`: generation, evaluation, and benchmarking tools.
- `docs/`: reference documentation, designs, and implementation status.
- `INTENT.md`: prose patterns used by the generator.

## Implementation principles

- Keep the compiler free of external dependencies.
- Verify declared proofs against the program. Never invent missing obligations
  or silently trust author-supplied claims.
- Reject unknown attributes. Every declaration must contribute to mechanical
  verification, optimization, or analysis; avoid decorative rigor.
- Keep direct native emission primary. WASM is the hosted portability path;
  the removed Rust transpiler is not a fallback. LLVM and source-to-Verbose
  transpilation are rejected directions documented in `CLAUDE.md`.
- Require features to serve security, performance, or distinctive machine code.
- Optimization hints must preserve observable behavior, including output, exit
  status, and accepted inputs. Compare hinted and unhinted builds on the same
  inputs when changing hint handling.
- Emission must be reproducible. Never let `HashMap` or `HashSet` iteration order
  determine emitted bytes or layout. Use declaration order or a deterministic
  sort whose ties cannot retain randomized order.
- Read the relevant reference implementation before relying on a reported gap.
  Describe the actual missing mechanism when documenting a limitation.
- Preserve explicit backend refusals for unsupported constructs. Verification
  success does not imply every backend supports a program or proves equivalence.

## Workflow and validation

Keep changes focused and explain what changed, why, and its practical impact.
Read the code being modified and verify claims against it. Explain compiler and
Rust concepts when they matter to the discussion.

Use the repository's `cidx.toml` and CI workflows as the command reference.
The standard local CI commands are:

```sh
cidx validate
cidx run security
cidx run ci
```

Use feature branches and the cidx PR workflow for changes being submitted.
Check actual branch protection before assuming direct updates are allowed;
protected branches receive changes through PRs. Run the complete normal test
suite before committing:

```sh
cargo test -- --test-threads=1
```

Serial execution is required: parallel native tests can fail with `ETXTBSY`
because forked processes inherit executable write descriptors. Do not diagnose
these failures as path collisions or fix them by renaming temporary files.
Network tests require permission to bind local sockets.

The separate bootstrap checks include ignored tests and need additional time,
memory, and stack. Run them when relevant to self-hosting changes:

```sh
ulimit -s unlimited
cargo test --release -- --ignored --test-threads=1 two_generation
```

For differential checks, materialize declared resource fixtures and exercise
both matching and non-matching inputs. Compare relevant effects and files as
well as stdout and exit status; matching failures alone do not establish correct
execution. State exactly which checks were run and any remaining limitations.

## Useful commands

```sh
# Verify a program.
cargo run -- examples/invoices.verbose

# Interpret a rule on JSON input.
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json

# Emit a native executable or a WASM module.
cargo run -- examples/invoices.verbose --native /tmp/inv --run important_invoice
cargo run -- examples/invoices.verbose --wasm /tmp/rule.wasm --run important_invoice
```

## Documentation

Write repository documentation in English. The scoped exception is `docs/learn/`:
French pedagogical articles tied to a stated language version, with their
canonical home on arcker.org. They do not track current compiler behavior.

Update reference documentation when behavior changes. Keep historical design
notes clearly dated, avoid copying stale measurements into current guidance,
and distinguish implemented checks from ambitions or unproved guarantees.
