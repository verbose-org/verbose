# Contributing to Verbose

Verbose is an experimental project. Contributions are welcome — whether it's code, ideas, bug reports, or documentation.

## Before You Start

Read these to understand the project's philosophy:

- **README.md** — what Verbose is and why it exists
- **ARCHITECTURE.md** — how the compiler works (pipeline, AST, backends)
- **CLAUDE.md** — development rules and design priorities

The short version: every feature must serve **security**, **performance**, or **unique machine code**. If it's just convenience, it can wait.

## How to Contribute

### Bug Reports

Run `cargo test` and `make demo`. If something fails or produces wrong results, open an issue with:
- The `.verbose` file that triggers the bug
- The command you ran
- Expected vs actual output

### Code Contributions

1. Fork the repo
2. Create a feature branch
3. Make your changes
4. Run `cargo test` — all 84+ tests must pass
5. Run `make demo` — all examples must verify
6. Open a pull request with a clear description

### What We Need Most

**Backends:**
- ARM64 native backend (the x86-64 backend in `src/native.rs` is the template)
- WASM backend improvements (collections, fold support)
- Complete the x86-64 instruction validator (`src/validate_x86.rs`)

**Language:**
- Mutable arrays and loops (path to sorting, real algorithms)
- More effect types for reactions (write_file, http_respond)
- String operations (concatenation, length, substring)

**Optimization:**
- Register allocation for the native backend (eliminate push/pop overhead)
- SIMD reduction for sum/count/min/max
- Division-by-constant via multiply-shift for more divisors

**Ecosystem:**
- Generation tool improvements (multi-AI support, better prompts)
- VS Code extension (syntax highlighting for `.verbose` files)
- Online playground (compile + run in browser)

### Development Rules

- **Zero dependencies.** Everything is hand-written. Don't add crates.
- **Zero trust.** The compiler verifies, never trusts. Don't add inference.
- **All docs in English.** The repo is international.
- **Tests required.** New features need tests. `cargo test` must pass.
- **Explain your changes.** The project creators are learning. PRs should explain not just what, but why.

### Architecture Quick Reference

```text
source.verbose → lexer → parser → resolver → verifier → optimizer → backend
                                                                      ↓
                                                         interpreter | Rust | x86-64 | WASM
```

- **Lexer** (`lexer.rs`): text → tokens with INDENT/DEDENT
- **Parser** (`parser.rs`): tokens → AST (recursive descent)
- **Verifier** (`verifier.rs`): checks proofs against AST (zero trust)
- **Optimizer** (`optimizer.rs`): platform-independent AST transforms
- **Backends**: each in its own file, consumes optimized AST

### Running Tests

```bash
cargo test              # all unit tests
make demo               # full pipeline demo
make benchmark          # comparison vs gcc
./tools/benchmark.sh    # reproducible benchmark
```

## Code of Conduct

Be respectful. This project was built by a human with a vision and an AI that codes. Both are welcome here.

## License

By contributing, you agree that your contributions will be licensed under Apache 2.0.
