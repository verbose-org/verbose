# Verbose

[![CI](https://github.com/verbose-org/verbose/actions/workflows/ci.yml/badge.svg)](https://github.com/verbose-org/verbose/actions/workflows/ci.yml)

Verbose is an experimental AI-native intermediate representation designed to sit between human intention and machine execution.

It is based on a simple idea: modern AI systems can generate far more detail than traditional human-oriented programming languages can naturally carry — but that extra expressive power must remain **explicit**, **verifiable**, **auditable**, and **compilable**.

Verbose explores that space.

> *Making exhaustiveness a compilable material.*
>
> *"I created this not so the machine replaces us, but so the machine is held accountable to us."*

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
| `vectorizable: "reason"` | Emits SSE4.2 `pcmpgtq` — 2 values per CPU cycle | Requires costly loop analysis |
| `parallel: "reason"` | Uses `fork()` — real multi-core parallelism | Developer must do it manually |
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

## Inspect the Machine Code

No black box. The `--disasm` flag shows the exact x86-64 assembly the compiler produces:

```bash
$ verbosec examples/invoices.verbose --disasm --run important_invoice
```

```asm
cmp    rax, 0x2710        ; compare amount to 10000
setg   al                 ; al = 1 if greater
test   al, al             ; check boolean result
je     0x213              ; if false → print "false\n"
mov    rdx, 0x5           ; length of "true\n"
mov    rax, 0x1           ; sys_write
syscall                   ; write to stdout
```

Every instruction traces back to a Verbose expression. The compiler's work is fully auditable — not just the proofs, but the machine code itself. Trust nothing, inspect everything.

## Getting Started

```bash
git clone https://github.com/verbose-org/verbose.git
cd verbose
cargo test
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

For how to write `.intent` prose that the AI maps reliably to Verbose constructs — which phrasings produce `all` / `any` / `map` / `filter` / `sum`, how to cross-reference rules, what the defaults are when the prose is silent — see [INTENT.md](INTENT.md).

## Why Not LLVM?

LLVM loses the information that makes Verbose unique. When translating to LLVM IR, domain knowledge is stripped: field ranges, optimization hints, purity proofs, overflow bounds — all gone. LLVM then spends dozens of analysis passes trying to re-discover what Verbose already knew.

Verbose native binaries are 400-700 bytes. LLVM would produce 10-50 KB minimum (function prologues, stack protectors, alignment padding, exception handling).

LLVM may become an optional backend for platforms we don't support natively. But the primary path stays direct — that's where the advantage lives.

## Why Not Transpile Rust/Go → Verbose?

A natural question: could we accept programs written in Rust, Go, or another existing language, transpile them into Verbose, and then compile? The appeal is clear — existing users keep their language, and gain Verbose's verification and codegen "for free".

We reject this path for the same reason we reject LLVM: **the source does not contain the information Verbose needs**, and inventing it breaks the model.

| Verbose requires (declared) | Source program provides |
|---|---|
| `reads: [amount]` verified against the AST | No notion of "field read" — any memory access counts |
| `overflow: [0, 1000000]` | At best inferred from type width |
| `termination: structural` with bound | Unannotated loops and recursion |
| `verdict: "critical"` with rationale | Nothing |
| `@intention "..."` tied to a numbered line | Unstructured comments |

A Rust→Verbose transpiler has two options, both bad:

- **Trivial proofs** — emit Verbose with `reads: [*]`, no bounds, no hints. The compiler verifies nothing of value, and the hint-driven optimizations (SIMD from `vectorizable`, parallel fork from `parallel`, constant folding from bounded intervals) never trigger. Verbose becomes a slower path to the same binary.
- **Inferred proofs** — try to deduce the declarations from the source. This is exactly what Verbose refuses by design: *the compiler verifies, it does not guess*. Inference is a trust boundary we explicitly chose not to cross.

There is also a paradigm gap. Verbose is declarative, organized around rules that compose over concepts. Most Rust/Go code is imperative, with ownership, traits, goroutines — constructs that do not map to rules. A transpiler would either restrict to a trivial subset (pure functions over structs, which interests no one) or emit non-idiomatic Verbose that benefits from none of its specific optimizations.

### The "don't upset existing users" concern

The concern is real: a language isolated from every existing ecosystem is hard to adopt. But automatic transpilation does not address it well. A Rust developer happy with Rust will not migrate for a slower, less complete pipeline — they will reasonably ask "why not just `rustc`?" and they will be right.

The healthier answers to the same concern:

- **Binary interop** — Verbose already emits ELF. Verbose binaries can be linked from Rust/Go via FFI. Users keep their language and call Verbose code for the parts where verification matters (business rules, critical paths).
- **Assisted generation, not automatic translation** — a tool that reads a function in another language and *suggests* a Verbose equivalent with proof slots to be completed by a human or an AI. The proofs remain declared and verified, not inferred.
- **Manual module bindings** — importing external functions through an explicit Verbose declaration that states the proofs on our side. The declaration is human-audited, not machine-derived.

The rule stays the same across both questions (LLVM and transpilation from existing languages): **if the proof is not declared, it does not exist**. Anything that fabricates proofs to make the pipeline work is a fiction that corrupts the model.

## On Drawing Lines

Rejecting LLVM and rejecting source-language ingestion will read as arrogant to some — *who are you to decide this?* We decide it, for this POC, because every line a language does **not** draw is a contract it will later be asked to honor. Scoping early is cheaper than retracting scope after users have built expectations on it. Anticipating the question "but why not also…?" before it is asked is the point, not the refusal. These are declarations of responsibility, not superiority.

## On Human Readers

Verbose is designed **by and for** AI. That reorders the human role — it does not remove it. Humans sit second in the *writing* seat, and first in the *auditing* seat.

A language built purely for machines could have been opaque: bytecode, s-expressions, a dense IR with no concession to legibility. Verbose is none of those. The syntax is indented and named, every block carries an `@intention`, every declaration traces back to a numbered line of a plain-language `.intent` file. That readable surface is deliberate — it is where the human disagrees when they should.

Will humans write Verbose directly tomorrow? Probably yes. Not because it is natural, but because it is learnable — the way reading JSON, regex, or unified diffs became learnable for a generation of developers who had never seen them before. The shift required is in how we *think* about code (declaring proofs, bounds, and effects), not in how we *read* it. Verbose does not ask humans to disappear; it asks them to move from authors to auditors, and it makes that move legible on purpose.

## On Evolving the Language

A language evolves. People will ask for shorter forms, friendlier syntax, method chaining, type inference, default hints, macros. Some of these are healthy; some would silently undo what Verbose stands for. We need one test, applied the same way every time.

> A new construct (syntax, builtin, shortcut) is **admitted** if and only if every piece of information that was previously declared explicitly remains declared explicitly afterward. If the novelty shortens the code by *hiding* a declaration, it is refused — even if it is pleasant.

The criterion is not "fewer characters" but "zero declarations lost". Comfort never buys implicitness.

**`.intent` and `.verbose` follow this rule differently:**

- `.intent` is prose. The compiler never verifies it directly — the AI does the translation job. So `.intent` can evolve freely: richer phrasings, recognized patterns, section headers, cross-references. More expressive prose gives the AI more signal without touching the verification model. "Pleasant" is the only criterion that applies here. The recognized prose patterns (what maps reliably to which Verbose construct) are listed in [INTENT.md](INTENT.md).

- `.verbose` is the verified layer. Every addition must pass the test above. A construct that collapses intermediate `@intention` markers, elides `input:`/`output:` through inference, hides a purity proof behind method chaining, or supplies default hints without declaration — rejected.

### Admitted

- **`map(coll, var => expr)` and `filter(coll, var => pred)`** — same proof structure as the existing `sum`/`count`/`all`/`any` (reads, writes, calls, termination all declared). Fills a real expressive gap ("for each X, compute Y", "keep X where Y") without hiding anything.
- **Record construction**: `ConceptName { field: expr, field: expr, ... }` — typed constructor in expression position. The verifier checks the concept exists, that the field set matches the declaration exactly (no missing, no extras), and that each field's expression matches the declared field type. Combined with `map`, this is what unlocks programs that produce *lists of structured results* rather than just lists of primitives.
- **Text composition**: `concat(e1, e2, ...)` — variadic text builder. Scalar arguments only (number becomes decimal digits, bool becomes `true`/`false`, text as-is); collection or record arguments are rejected. Deliberately NOT an overload on `+` — the rejection list refuses operator overloading that hides arithmetic purity, so text composition gets its own audit-visible call. Unlocks dynamic error messages: `Err(concat("customer age ", age, " is under 18"))`.
- **File-writing reactions**: `append_file "/path/to/log" content_expr` as a reaction effect. Path is a string LITERAL at parse time — dynamic paths are refused so the auditor reading the source sees every file this program can ever touch. Content is any text expression (typically `concat(...)`). No implicit newline: the content is exactly what gets written. This is the step from "compute and tell me" to "compute and persist"; without it, a program can only speak to stdout. **Compiles to native**: a reaction with `append_file concat(...)` produces a standalone ELF binary that opens the declared path, builds the dynamic line in a stack buffer (inline `itoa` for numeric fields), writes, and closes. ~700 bytes, zero dependencies, zero heap.
- **Justified hints.** `vectorizable: "no cross-element dependency"` instead of bare `vectorizable: yes`. Adds a declaration (the reason), does not remove one. The *why* becomes part of the audit surface, printed next to the hint at compile time. Already enforced — a bare hint is now a parse error.
- **Stratified rule layers.** `@layer: domain | application | interface` on a rule adds a declaration and a verified constraint. A domain rule can only call domain rules; application can call domain or application; interface can call any layered rule. Layered rules may not call unlayered ones — layered code is a sealed subgraph, so the discipline cannot be transitively escaped. Opt-in: rules without `@layer` are unchecked, preserving backward compat. Already implemented.
- **Typed results.** `Result(T, E)` makes the failure path a declared part of the output instead of an implicit panic. `Ok(value)` and `Err(reason)` are the two arms, both visible to the caller and audited at the IR level. `match_result(r, v => ok_body, e => err_body)` consumes a Result — both arms named and required, no implicit Err-propagation, so the reader sees exactly what happens on failure. Already implemented — a rule can declare `output: Result(number, text)`, return either form, and compose with other Result-returning rules. **Compiles to native**: a `Result(number, text)` rule produces a standalone ELF where `Ok(n)` streams to stdout (one value per record) and `Err(msg)` streams to stderr, following classic UNIX stream separation for shell composition (`./validate 200 17 | consume 2>errors.log`).
- **Source traceability at field level.** `amount : number [0, 1000000] @source "business_rules.intent:7"` extends existing `@source` from rules down to fields. More traceability, nothing lost.
- **Stronger verifier with no new syntax.** Verifying that `amount mod 100` stays within `[0, 99]` given `amount : [0, 10000]` — pure compiler improvement, no language change.
- **Richer `.intent` prose patterns.** Section headers, cross-references, domain templates, documented natural-language patterns the AI maps reliably. `.intent` is never verified directly, so evolution there is unrestricted.

### Refused (and why)

Each of these is useful. None of them preserves the declared chain. Documented here so future contributors (and future sessions) inherit the same answer.

- **Proof inference from the rule body.** The compiler would deduce `reads` / `writes` / `calls` instead of checking them. Violates zero-trust: the proof becomes a fact the compiler itself invents, so "the compiler verifies the compiler". The moment a proof is derived rather than stated, the audit chain breaks.
- **Type inference that elides `input:` / `output:`.** The shape of data becomes a fact derived from usage. An AI hallucination about field names or types would no longer be caught by the type declaration — it would be silently absorbed.
- **Method chaining** (`users.filter(p).map(f).sum()`). Collapses multiple logical steps into one anonymous expression. Intermediate `@intention` markers disappear. The auditor can no longer review each step, only the endpoint.
- **Implicit numeric promotion** (narrow → wide, int → float). Hides a change of overflow domain. The declared range on one side no longer constrains the range on the other side.
- **Default hints** (assume `vectorizable` when no `call` is present, etc.). Hides the optimization decision. The human reading the code can no longer tell what the compiler was authorized to do.
- **Macros / metaprogramming.** Move verification to a layer where "what is being checked" is itself derived from code. The thing the auditor reads stops being the thing the compiler verifies.
- **Operator overloading.** Hides the purity of arithmetic. `a + b` could become a function call with unknown `reads` / `writes` / `calls`.
- **Destructuring that deduces shape** (`let {name, age} = user`). Same category as type inference: the expected shape becomes a fact derived from the pattern instead of from a declaration.
- **Implicit null / optional propagation** (monadic `?` operator, silent `None` → skip). Hides a failure path behind a punctuation mark. Use a typed `Result` / `Optional` where the failure path is declared.
- **Global configuration that changes semantics** (compiler flags that turn features on/off for a file). The meaning of the code starts depending on something outside the code. Unacceptable for audit.

The bar is not "is this useful" — every item above is useful. The bar is "does it preserve the declared chain". When in doubt, the answer is no.

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

**POC / R&D.** 0 dependencies, 4 backends, 84+ tests. All claims backed by code.

```bash
cargo run -- examples/invoices.verbose --benchmark --run important_invoice
```

## Learn How Machines Think

Verbose is also a bridge between human intention and CPU instructions. Write a business rule, run `--disasm`, and see exactly what the processor does. No 800-page textbook needed.

```text
Your rule:        important = i.amount > 10000
The CPU does:     cmp rax, 0x2710    (compare register to 10000)
                  setg al            (store 1 if greater)
                  syscall            (tell the kernel to print)
```

If you've never seen assembly before, you just learned three instructions. That's how every program on earth works — registers, comparisons, and syscalls. Verbose makes it visible.

## Origin

This project started as an open question: *"If AI writes code now, do we still need languages designed for humans?"*

A few hours later, the question had become a working compiler with verified proofs, four backends, SIMD optimization, and a 498-byte HTTP server.

No spec committee. No funding. No team. One human with a vision, one AI that codes, and a question that turned out to have a very concrete answer.

## License

Apache 2.0

## Author

Created by Yoan Roblet ([@Arcker](https://github.com/Arcker)).

The vision, the architecture decisions, and every "no" that kept the project on track came from a human. The Rust code came from an AI. The compiler trusts neither.
