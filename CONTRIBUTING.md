# Contributing to Verbose

Verbose is an experimental project. Contributions are welcome — whether it's code, ideas, bug reports, or documentation.

## Before You Start

Read these to understand the project's philosophy:

- **README.md** — what Verbose is and why it exists
- **docs/current-status.md** — implemented paths, limits, and validation commands
- **ARCHITECTURE.md** — how the compiler works (pipeline, AST, backends)
- **CLAUDE.md** — development rules and design priorities

The short version: every feature must serve **security**, **performance**, or **unique machine code**. If it's just convenience, it can wait.

## How to Contribute

### Bug Reports

Run `cargo test -- --test-threads=1` and the relevant example. If something fails or produces wrong results, open an issue with:

- The `.verbose` file that triggers the bug
- The command you ran
- Expected vs actual output

### Code Contributions

1. Fork the repo
2. Create a feature branch
3. Make your changes
4. Run `cargo test -- --test-threads=1` — the normal suite must pass
5. Run the relevant examples; `make demo` exercises a small demonstration subset
6. Open a pull request with a clear description

### Useful contribution areas

- Strengthen verification with negative cases and regression tests for incorrect
  acceptance, including interval analysis and recursion obligations.
- Compare supported interpreter, native, WASM, and self-hosted behavior on the
  same inputs; keep backend restrictions explicit.
- Improve native lowering and instruction validation with measured correctness
  and performance evidence. Design documents alone are not an implementation map.
- Evaluate generation on unseen intentions and different models, measuring both
  source acceptance and intended behavior.
- Keep the current reference, examples, diagnostics, and editor support usable by
  human and LLM authors. Consult the existing implementation before proposing a
  feature that an older journal still lists as future work.

### Development Rules

- **Zero dependencies.** Everything is hand-written. Don't add crates.
- **Zero trust.** The compiler verifies, never trusts. Don't add inference.
- **All docs in English.** The repo is international.
- **Tests required.** New features need tests. `cargo test -- --test-threads=1` must pass.
- **Explain your changes.** The project creators are learning. PRs should explain not just what, but why.

### Architecture Quick Reference

```text
source.verbose → lexer → parser → resolver → verifier → optimizer → backend
                                                                      ↓
                                                         interpreter | x86-64 | WASM
```

- **Lexer** (`lexer.rs`): text → tokens with INDENT/DEDENT
- **Parser** (`parser.rs`): tokens → AST (recursive descent)
- **Verifier** (`verifier.rs`): checks proofs against AST (zero trust)
- **Optimizer** (`optimizer.rs`): platform-independent AST transforms
- **Backends**: each in its own file, consumes optimized AST

### Running Tests

```bash
cargo test -- --test-threads=1  # normal suite; local sockets required
make demo               # full pipeline demo
make benchmark          # comparison vs gcc
./tools/benchmark.sh    # reproducible benchmark
```

The bootstrap has a separate ignored-test suite; see [current status](docs/current-status.md#verification-commands) for its command and resource requirements.

## Code of Conduct

Be respectful. This project was built by a human with a vision and an AI that codes. Both are welcome here.

## License

By contributing, you agree that your contributions will be licensed under Apache 2.0.
