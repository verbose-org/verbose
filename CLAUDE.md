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
  wasm.rs          WebAssembly module generation (browser backend)
  optimizer.rs     Platform-independent AST optimizations
  validate_x86.rs  Self-verification of emitted machine code
  main.rs          CLI entry point

examples/
  invoices.*       Minimal example (1 concept, 1 rule)
  business.*       Arithmetic + rule composition (4 rules, 3 fields)
  clients.*        Text type + string comparison
  collections.*    Nested data with all/any quantifiers
  pricing.*        Nested if/else + let bindings
  deadcode.*       Dead branch elimination demo
  showcase.*       ALL features in one scenario (6 rules)
  report.*         Business report with fold/sum/count (4 rules)
  reactions.*      Basic reaction (print on trigger)
  alerts.*         Dynamic reactions with interpolated values
  app.* + stdlib/  Module system demo (use + import)
  retirement.*     map + filter on a collection of employees
  purchase.*       Result(T, E) — declared failure path (Ok/Err)
  layers.*         @layer stratification — architectural discipline verified
  bonus.*          record construction — map produces collection(BonusReport)
  audit_log.*      append_file reaction with dynamic concat content — compiles to a 724-byte native binary
  audit_simple.*   append_file with static content — compiles to a 462-byte native binary
  purchase.*       Result(number, text) validator — validate_purchase compiles to a 705-byte native binary (Ok -> stdout, Err -> stderr)
  tier.*           Result(text, text) classifier — classify_tier compiles to a 602-byte native binary
  classify.*       Record-output rule — classify_invoice compiles to a ~970-byte native binary that emits one JSON object per record
  greeting.*       Text input field flowing into JSON output — make_report compiles to a ~590-byte native binary
                   (purchase.verbose::discounted_purchase compiles to ~750 bytes via Phase 2D match_result inlining)
  demo.html        Browser demo (WASM)

tools/
  generate.sh      Intent -> Verbose via Claude API
  benchmark.sh     Reproducible comparison vs gcc
```

## Language Features (current)

- Types: `number`, `bool`, `text`, `collection(Type)`, `Result(T, E)` (declared failure path), named types
- Field ranges: `amount : number [0, 1000000]`
- Expressions: arithmetic (+, -, *, /, %), comparisons (>, <, >=, <=, ==, !=), boolean (and, or, not)
- Control flow: `if condition then expr else expr` (nestable)
- Let bindings: `let tax = amount * rate / 100` (CSE)
- Rule calls: `important_invoice(i)` — rules can compose
- Quantifiers: `all(collection, var => predicate)`, `any(...)`
- Aggregation: `sum(coll, var => expr)`, `count(coll, var => pred)`, `min(...)`, `max(...)`
- Per-element: `map(coll, var => expr)` → collection(T), `filter(coll, var => pred)` → collection of same element type
- Result: `Ok(v)` / `Err(e)` constructors; `match_result(r, v => ok_body, e => err_body)` consumer with both arms explicit
- Record construction: `ConceptName { field: expr, field: expr, ... }` — typed constructor; verifier cross-checks field set + per-field types match the concept declaration
- Text composition: `concat(e1, e2, ...)` — variadic text builder, scalar args only (number → decimal, bool → true/false, text as-is); no operator overloading on `+`, each arg is explicit
- Verifier type check: bidirectional shape check on logic — `Ok`/`Err` rejected outside `Result(...)` context; `Ok(x)`/`Err(e)` content checked against declared arms when inferable; top-level output type checked against declared; conservative on lambda/let-bound vars to avoid false positives
- General reduction: `fold(collection, initial, acc, var => body)`
- Proofs: purity (reads/writes/calls/verdict), termination (form/bound), determinism (form)
- Hints: `vectorizable: "reason"`, `parallel: "reason"`, `cache_result: "reason"` (justification required, parser rejects bare form), `overflow: [min, max]` (bounds mechanically verified against interval arithmetic)
- Traceability: `@intention` (string), `@source` (file:line), `@layer: domain|application|interface` (optional, sealed-subgraph discipline)
- Modules: `use "stdlib/finance.verbose"`
- Reactions: declared side effects with trigger rules; effects today are `print` (to stdout) and `append_file "path" content` (to a file). Path is a string literal at parse time — dynamic paths are refused so the auditor reads every file the program can ever touch.
- String escapes: `\n`, `\t`, `\\`, `\"` — closed set, unknown escape is a lex error (no silent pass-through for typos).
- Three backends: interpreter (--run), Rust transpiler (--compile), native x86-64 (--native), WASM (--wasm)

## Writing .intent Prose

The recognized patterns that the AI maps reliably to Verbose constructs (e.g. "for each X, check Y" → `all`, "for each X, compute Y" → `map`, "total of Y over X" → `sum`, etc.) are published in `INTENT.md`. Future sessions should consult it before inventing a pattern, and extend it when a new pattern is agreed upon. `.intent` evolves freely by design, but only within what we have written down — otherwise every `.intent` file depends on improvisation.

## Separation of Concerns

The compiler (verbosec) NEVER generates code. It verifies and compiles. Code generation is the AI's job, done through a separate tool (not part of the compiler). This boundary is non-negotiable:

- **AI** (external, non-deterministic): reads .intent, generates .verbose with proofs and hints
- **verbosec** (internal, deterministic): verifies proofs against AST, compiles to binary

If they're mixed, the verification loses its value. The compiler's credibility comes from being independent of the generation process.

A dedicated intent-to-verbose generation tool is planned as a separate project/script.

## LLVM Strategy

LLVM is NOT the primary backend. Verbose emits machine code directly because:
1. LLVM IR can't express field ranges, overflow bounds, or optimization hints
2. The translation to LLVM IR loses the domain knowledge that makes Verbose unique
3. LLVM adds overhead (prologues, stack protectors, alignment) that Verbose proves unnecessary

LLVM may become an OPTIONAL fallback backend for platforms without a native emitter. But all architecture decisions must keep the direct-emission path viable and primary.

## Two Execution Modes, Two Security Profiles

Security is pillar #1. Each feature is judged by what attack surface it adds, not by whether it is "useful". Under that frame, the compiler exposes two execution modes — not one primary and one fallback, but **two modes with deliberately different surfaces**:

- **Native (small, auditable surface, actively growing)**: x86-64 machine code emitted directly. No libc, no allocator, no tagged values held across non-local control flow, no dynamic dispatch. Grows phase by phase as new constructs land, each extension reviewed against this list. Binaries stay small (500 B–2 KB) and auditable line by line. As of this writing the native path covers scalar rules, reactions with `append_file`, `Result(number|text, text)`, record outputs, text-typed input fields, and `match_result` in the pass-through shape (see the phase table below).
- **WASM (small, scalar-only)**: same principles as native but has not been grown alongside the recent native phases. WASM today handles scalar rules only (Phase 0). Bringing WASM up to parity is mechanical — the AST supports the constructs, the emitter just hasn't been written. This asymmetry is known and deliberate: the security-sensitive target is the native ELF, and WASM's growth follows native once we have a stable convention to mirror.
- **Interpreter (rich surface)**: the full language — collections, `map`/`filter`/`fold`, all `Result` / `Record` / `match_result` compositions, `@layer`. Runs in a Rust binary that parses JSON and evaluates expressions. Wider surface than native (JSON parser, allocator) but **every expression is still verified by the same compiler** against the same proofs.

Both modes verify the same AST with the same proofs. A rule accepted by the compiler is safe under both modes; only the execution profile differs. Native's trustworthiness comes from careful accumulation — adding a construct is a deliberate commit, never a drive-by "it's missing". Forcing native to grow to "completeness" (full heap, tagged unions, etc.) would add a C-sized attack surface and defeat the point. When native rejects an expression today, the answer is either "add a phase for it under the evolution rules below" or "run it in the interpreter" — never "silently upgrade native to handle it".

## Native Backend Evolution

Tracking what native emits today, what it still rejects, and the design rules that shape how it grows.

### What native emits today

| Phase | Shape | Typical binary | Milestone example |
|---|---|---|---|
| 0 | Scalar rule (`bool` / `number` output from arithmetic, comparisons, field reads) | ~500 B | `invoices.verbose` |
| 1A | Reaction with `append_file "literal_path" "literal_content"` | ~460 B | `audit_simple.verbose` |
| 1B | Reaction with `append_file "literal_path" concat(...)` — dynamic text via inline itoa + stack buffer | ~720 B | `audit_log.verbose` |
| 2A | Rule with `output: Result(number, text)` — Ok→stdout, Err→stderr, continuation-passing leaves | ~700 B | `purchase.verbose::validate_purchase` |
| 2B | Rule with `output: Result(text, text)` — Ok(text) writes to stdout (literal or concat); shared `emit_text_write_to_fd` helper | ~600 B | `tier.verbose::classify_tier` |
| 2C | Rule with `output: Named(concept)` (record) — JSON serialization to stdout, one object per record. Streaming emission (no on-stack record). Number/text fields supported; `if/else` between two record arms via continuation-passing. | ~1 KB | `classify.verbose::classify_invoice` |
| 2E | Text-typed input fields readable in record outputs — argv pointer stored at the rbp slot, length recovered via `repne scasb` (`emit_strlen`) at each read site. | ~600 B | `greeting.verbose::make_report` |
| 2D | `match_result(callee(input), v => Ok(<arith using v>), e => Err(e))` — inlined-callee form. Callee's logic is walked and its Ok/Err leaves are redirected: Ok values bind to a reserved `match_slot` then evaluate the outer Ok arm; Err values write directly to stderr (Err pass-through). Restricted to same-input-concept callees and `Err(<err_var>)` pass-through outer arm. | ~750 B | `purchase.verbose::discounted_purchase` |

### What native still rejects, and in which priority

- **Result(T, E) with non-scalar T** (e.g. `Result(Record, text)`, `Result(collection, _)`) — each shape needs its own calling convention. Decide shape by shape, never fabricate a "universal Result" that carries unnecessary machinery.
- **Text-field access in `Result(text, text)` Ok arm or in concat arguments** — Phase 2E added text-field reads only inside record-output rules (`emit_record_program`'s argv loop and `emit_text_write_to_fd`'s special case). Other emitters (`emit_full_program`, `emit_reaction_program`, `emit_result_program`) still call `atoi` on every field. Generalizing requires teaching each emitter the same field-loading dispatch — purely mechanical, not gated by design questions.
- **`output: collection(T)`** — the only case that genuinely needs heap (map/filter produce collections with runtime-determined sizes). Behind a bump-allocator design we have not committed to yet. (Phase 3.)
- **`match_result` with non-pass-through Err arm** — Phase 2D handles `Err(<err_var>)` pass-through (the value flows directly to stderr without being bound). Richer Err arms (using err_var inside concat, or transforming it) need a real text-binding mechanism — two rbp slots per text bound var (ptr + len) since Err values from concat aren't NUL-terminated.
- **`match_result` with cross-concept callees** — Phase 2D requires callee.input_concept == outer.input_concept (so the rbp slots are reused as-is). Cross-concept calls need argument-passing through additional slots or a real callee frame.
- **Nested `match_result`** — Phase 2D reserves a single `match_slot` in the prologue; nested match_results would collide. Either reserve N slots based on a static walk or switch to a stack-based binding scheme.
- **Record fields with text-typed value coming from concat or rule call** — Phase 2C/2E support text fields whose value is a literal or an input-field access. Other text expressions (e.g. `Concept { name: concat(...), ... }`) need the buffer allocated in the JSON streaming path.

### Register conventions across emitters

Emitters that span multiple syscalls or phases share a register layout. Adding a new cross-phase register use requires either claiming a currently-unused register or saving/restoring on the stack — do not casually reassign any of these without auditing every caller.

| Register | Used by | Introduced |
|---|---|---|
| `r12` | argc (read at `_start`) | Phase 0 |
| `r13` | argv base pointer | Phase 0 |
| `r14` | current record index inside the main loop | Phase 0 |
| `rbp` | field-slot frame base (fields + let bindings at `rbp - 8*(i+1)`) | Phase 0 |
| `r15` | file descriptor returned by `open()` in `append_file` effects | Phase 1A |
| `r10` | concat buffer base for later length calculation | Phase 1B |
| `rbx` | concat write pointer (advances as args are written) | Phase 1B |

Dedicated rbp-relative slots:

| Slot | Used by | Introduced |
|---|---|---|
| field slots (`rbp - 8*(i+1)`) | input concept fields — Number via atoi, Text stores argv pointer | Phase 0 / Phase 2E |
| let-binding slots (`rbp - 8*(nfields + k + 1)`) | `let` bindings evaluated in source order | Phase 0 |
| `match_slot` at the bottom of the frame | `match_result`'s inlined-callee Ok-value binding (reserved unconditionally in `emit_result_program`) | Phase 2D |

Registers *not* in this table (`r8`, `r9`, `r11`, `rcx`, `rdx`, `rsi`, `rdi`, `rax`) are ephemeral — emitters may clobber them freely within a single expression. Note that Linux syscalls clobber `rax`/`rcx`/`r11`; any state that must survive a syscall belongs in `r10` or `r12`–`r15`.

### Continuation-passing emission for branching results

When a rule produces a discriminated output, each leaf is emitted as a tail-call: it writes its own output and `jmp`s back to the iteration top. **No intermediate tagged value materializes in registers or memory.** The pattern is now used in three emitters:

- `emit_eval_result_expr` — `Result(T, E)` outputs. Each Ok/Err leaf streams to stdout/stderr and jumps.
- `emit_eval_record_expr` — record outputs. Each Record leaf serializes to JSON and jumps.
- `emit_redirect_callee_leaves` — `match_result` inlining. The inlined callee's Ok/Err leaves are redirected: Ok values bind to `match_slot` and invoke the outer Ok arm's recursion (itself tail-terminating); Err values write directly to stderr.

Why this over the standard "materialize tag + payload, dispatch at a common exit":

1. **Lifetime simplicity.** Any leaf that allocates temporaries (e.g. a concat buffer for an Err message) keeps its allocation local to the leaf. No tracking of "does this exit path need to free a concat buffer?" across the dispatch.
2. **No calling-convention ambiguity.** `Ok(number)` and `Ok(text)` would need different payload registers; the leaf knows its declared shape and emits the right write directly.
3. **Tree scales naturally.** Nested `If` inside a Result arm: each distant leaf plants its own terminator. No phi nodes, no fall-through, no common join point to engineer.

Cost: each leaf carries ~30 bytes of trailing write + `jmp loop_top`. For rules with <10 leaves (the overwhelming majority), this is negligible vs. the machinery a tagged-dispatch approach would need.

The choice is *producer-side*: every emitter above is for a rule that PRODUCES the discriminated value. Phase 2D's `match_result` is a *consumer* in the outer rule, but it too avoids a tagged calling convention — instead it inlines the called rule and redirects ITS leaves to the outer match arms. That works because both producer and consumer are compiled together; it would stop working across separately-compiled units. When/if Verbose gains separately-compiled modules with Result-valued exports, a real tag+payload convention becomes unavoidable for consumers. Until then, inlining-plus-continuation-passing is both simpler and smaller.

### Stream semantics for Result-output native rules

A rule with `output: Result(number, text)` compiles to a binary where:

- `Ok(n)` writes `n\n` to **stdout** (newline from `itoa`).
- `Err(msg)` writes `msg\n` to **stderr** (newline appended by the emitter for symmetry).
- Exit code is **always 0** after processing all records.

The "always exit 0" choice is UNIX-idiomatic: failure on a single record is data, not a system error. The shell caller separates success from failure by stream, not by exit code:

```
./validate 200 17 | consume_valid           # pipe valids through
./validate 200 17 2>errors.log              # capture errors to a file
./validate 200 17 1>/dev/null               # keep errors only
```

If a binary wants "exit non-zero on any Err", that's a separate reaction layer (future, not shipped).

## Transpilation Strategy (rejected direction)

Rust/Go/other source → Verbose transpilation is **rejected** for the same reason as LLVM: the source does not contain Verbose's declarations (reads/writes, overflow bounds, termination form, verdict, intention). Any transpiler must either emit trivial proofs (losing all verification value and all hint-driven optimizations) or infer them (violating the zero-trust rule that proofs are declared, never guessed).

The healthier answers to "don't isolate from existing ecosystems" are:
1. **Binary interop** — Verbose emits ELF; other languages link via FFI.
2. **Assisted generation** — tooling that suggests a Verbose equivalent from foreign source, with proof slots filled by a human or AI (not by the compiler).
3. **Manual module bindings** — external functions imported through an explicit Verbose declaration stating the proofs on our side.

Rule: **if the proof is not declared, it does not exist**. No pipeline fabricates proofs.

Full rationale: README.md → "Why Not Transpile Rust/Go → Verbose?".

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
- **Every feature must serve security, performance, or unique machine code.** No ergonomic sugar without optimization value.
- **All documentation in English.** The repo is international.

## Running

```bash
cargo run -- examples/collections.verbose                                           # verify
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json  # interpret
cargo run -- examples/report.verbose --run total_revenue --input examples/report.json --json  # JSON output
cargo run -- examples/business.verbose --compile /tmp/business                      # transpile to Rust
cargo run -- examples/business.verbose --native /tmp/biz --run critical_invoice     # native x86-64
cargo run -- examples/invoices.verbose --wasm /tmp/rule.wasm --run important_invoice # WASM
cargo run -- examples/invoices.verbose --benchmark --run important_invoice          # compare all backends
cargo run -- --demo-http /tmp/server                                                 # HTTP server demo
cargo test                                                                          # 84 tests
make demo                                                                           # full demo
```

## License

Apache 2.0
