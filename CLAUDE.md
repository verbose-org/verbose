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
  audit_simple.*   append_file with static content — compiles to a 464-byte native binary
  audit_user.*     append_file reaction whose log line concatenates a text-typed input field;
                   buffer sized at runtime via per-field strlen, freed via saved-rsp r9 (~847 B)
  purchase.*       Result(number, text) validator — validate_purchase compiles to a 705-byte native binary (Ok -> stdout, Err -> stderr)
  tier.*           Result(text, text) classifier — classify_tier compiles to a 602-byte native binary
  classify.*       Record-output rule — classify_invoice compiles to a ~970-byte native binary that emits one JSON object per record
  greeting.*       Text input field flowing into JSON output — make_report compiles to a ~590-byte native binary
  fullname.*       Record output whose text field is built via concat of input text fields — compose_greeting compiles to a ~758-byte native binary
  greeting_line.*  Phase 5a: `output: text` per-record — greeting_line compiles to a ~564-byte native binary
  roster.*         Phase 5b: `output: text` via top-level fold — roster_line compiles to a ~708-byte native binary
                   (append-only body: concat(acc, e.name, "=", e.salary, "; "); two-pass sizing, single write per input record)
  payroll.*        Phase 3: four rules on the same input — map to Record (~670 B), filter (~670 B), map to number (~455 B), map to text (~410 B).
                   Phase 4: two reductions on the same input — sum (~486 B), count (~532 B).
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
| 1B | Reaction with `append_file "literal_path" concat(...)` — dynamic text via inline itoa + stack buffer. Text-field args (e.g. `concat("user=", p.user, ...)`) sized at runtime via per-field `strlen`; `r9` saves the pre-allocation `rsp` so the buffer is freed via `mov rsp, r9` (3 bytes) after the write. Same path also serves `Result(text, text)` Ok/Err arms that concat a text field. | ~720 B (numbers-only) / ~850 B (with text fields) | `audit_log.verbose` (numbers) / `audit_user.verbose` (text field) |
| 2A | Rule with `output: Result(number, text)` — Ok→stdout, Err→stderr, continuation-passing leaves | ~700 B | `purchase.verbose::validate_purchase` |
| 2B | Rule with `output: Result(text, text)` — Ok(text) writes to stdout (literal or concat); shared `emit_text_write_to_fd` helper | ~600 B | `tier.verbose::classify_tier` |
| 2C | Rule with `output: Named(concept)` (record) — JSON serialization to stdout, one object per record. Streaming emission (no on-stack record). Number/text fields supported; `if/else` between two record arms via continuation-passing. Text fields accept literal / input-field / `concat(...)` values (concat uses the Phase 1B dynamic buffer when text-field args are involved). | ~1 KB (basic) / ~760 B (with concat-text) | `classify.verbose::classify_invoice` / `fullname.verbose::compose_greeting` |
| 2E | Text-typed input fields readable in record outputs — argv pointer stored at the rbp slot, length recovered via `repne scasb` (`emit_strlen`) at each read site. | ~600 B | `greeting.verbose::make_report` |
| 2D | `match_result(callee(input), v => Ok(<arith using v>), e => Err(e))` — inlined-callee form. Callee's logic is walked and its Ok/Err leaves are redirected: Ok values bind to a reserved `match_slot` then evaluate the outer Ok arm; Err values write directly to stderr (Err pass-through). Restricted to same-input-concept callees and `Err(<err_var>)` pass-through outer arm. | ~750 B | `purchase.verbose::discounted_purchase` |
| 3 | `output: collection(T)` with `map` or `filter` — streaming element emission (one JSON Lines per element), no arena, count-prefixed argv. `filter` uses identity pass-through: predicate false skips emission, predicate true emits the element as-is. See "Phase 3 design (locked)" below. | ~670 B | `payroll.verbose::compute_bonuses` (map) / `::high_earners` (filter) |
| 3.2 | `output: collection(number)` / `collection(text)` — scalar element map. `map(w.employees, e => e.salary)` emits one number per line; text body emits one string per line. No JSON wrapping, so scalar-element binaries are smaller (~400-500 B). | ~455 B | `payroll.verbose::salaries` / `::names` |
| 4 | `output: number` with `fold`/`sum`/`count`/`min`/`max` at the top level — inner loop accumulates into a single stack slot, emits the final value on stdout once per input record. First emitter with cross-iteration state; no arena (the accumulator is one i64). See "Phase 4 design (locked)" below. | ~490–530 B | `payroll.verbose::total_salaries` (sum, 486 B) / `::high_earner_count` (count, 532 B) |
| 5a | `output: text` with a per-record body — literal, input text field, or `concat(...)`. One `write` to stdout + newline per input record; no accumulator, no state carried across iterations. Routes to `emit_text_program`, which reuses `emit_text_write_to_fd` (already serving Phase 2B's Ok-text arm). Fold-over-collection to text is Phase 5b. | ~320 B (literal) / ~330 B (field) / ~560 B (concat) | `greeting_line.verbose` (concat, 564 B) |
| 5b | `output: text` via top-level `fold` — appends into a text accumulator over a collection. Body is strictly append-only: `Concat(Ident(acc), ...rest)` with `acc` absent from `rest`. Two-pass emission (pass 1 sums per-element static + `strlen` per text-field arg into rax; pass 2 fills the buffer). `mov r9, rsp; sub rsp, rax` reserves, `mov rsp, r9` frees. See "Phase 5b design (locked)" below. | ~700 B | `roster.verbose::roster_line` (708 B) |

### Phase 3 design (locked before implementation)

Four decisions locked before writing any `emit_collection_program` code, so the implementation has no surprises and philosophical drift is caught at this level, not in commit messages.

**1. Memory model: no arena.** Streaming emission means each element produced by `map` / `filter` is evaluated, serialised to stdout, and discarded before the next iteration. No element ever persists across iterations. No bump allocator, no heap, no `mmap`. Same syscall surface as Phase 2 — only `read argv`, `write fd`, `exit`. This is why `fold` / `sum` / `count` / `min` / `max` on the output side are **not** part of Phase 3: they need an accumulator, which is arena territory; when we get there, the arena decision gets its own design pass.

**2. Output format: JSON Lines, NOT a wrapping JSON array.** One JSON object (or scalar) per line, same convention as Phase 2C record output. Rationale:
- Fewer syscalls per collection — no bracket/comma state machine, so no `[`, no `,`, no `]` static writes. A 1000-element collection is ~1000 syscalls in JSON Lines vs ~2000 with array wrapping.
- Smaller binary — no bracket logic code.
- Uniform format across Phase 2C and Phase 3 native outputs — one object per line, regardless of whether the rule produces one record or a collection of records. Downstream tooling (`jq -r`, `awk`, `grep`) handles both identically.
- If a caller needs an array, `./bin args | jq -s` wraps at ~zero cost. Conversely, producing JSON Lines from a binary that emits arrays is harder. Pick the simpler side.

Cost: per-input-record grouping is lost when the binary processes multiple input records. Acceptable — single-input invocations are the common case, and multi-input can be reconstructed by running the binary separately.

**3. argv format: `<N> <element × N>` count-prefixed, trailing any scalar fields of the input concept.**
```
concept Workforce { employees: collection(Employee{name, salary}) }
./bin <N> <emp1-name> <emp1-salary> ... <empN-name> <empN-salary>

concept BigInput { tag: number, employees: collection(Employee{name, salary}) }
./bin <tag> <N> <emp1-name> <emp1-salary> ... <empN-name> <empN-salary>
```
The count comes right before its elements. No terminator, no dynamic scanning. Predictable at every iteration. If the caller passes the wrong count, the binary reads past the end of argv and exits via the natural argv-bounds-check in the outer loop.

**4. Scope v1 (restrictions rejected with clear messages):**
- Top-level logic must be `map(input.<coll_field>, var => <body>)` or `filter(input.<coll_field>, var => <pred>)`.
- Input concept: zero or more scalar fields + exactly one collection field, which must be the LAST field declared.
- Output: `collection(Record)` or `collection(number | text)`.
- Map/filter body references only the lambda variable, not the outer rule's input (simplifies `input_name` threading).
- No nested collections, no collection-returning rule calls, no `fold`.

**Register convention for collection emitter:** `r15` holds the inner loop counter (number of elements remaining). This role is distinct from its Phase 1A role as the file-descriptor returned by `open()` — the two emitters never coexist in the same binary (a reaction-producing binary and a collection-output binary are different entry points), so the register has a per-emitter role, not a global one. The register table in the "Register conventions" section names this explicitly.

**Why this design holds security pillar #1:** identical attack surface to Phase 2. No new syscalls. No heap. No dynamic parsing. The binary is still a straight machine-code translation of the source: read argv, evaluate declared logic, write declared output, exit. An auditor reading the disassembled binary sees exactly one extra loop compared to Phase 2C, nothing else.

**Filter implementation note — synthesised AST at emit time.** `filter(xs, e => pred)` does not construct a Record in the source; it keeps or drops each element verbatim. The emitter handles this by building a small Vec of synthetic `Field(Ident(<lambda>), <name>)` expressions — one per field of the input element concept — and passing them to the same `emit_record_as_json` the `map` path uses. The Vec exists only during emission, never reaches the verifier, never appears in error messages. The emitted machine code is byte-for-byte identical to what a user would get writing `map(xs, e => Employee { name: e.name, salary: e.salary })` by hand. Zero declaration added or removed at the source contract — pure emitter internals.

**Known perf tradeoff (not yet addressed).** Each element emits N+K syscalls, where N is the field count and K is the number of static JSON skeleton fragments. For a 2-field record that's ~6 syscalls per element. On Linux, each syscall is ~1 µs, so a 1000-element collection takes ~6 ms to emit. At POC scale this is invisible; for 100K+ element collections, a batch-per-element optimisation (stack buffer, single `write` per element) would be ~10-20× faster. The Phase 1B `emit_concat_to_buffer` / `emit_append_file_call` pattern already shows the technique — generalising it to record emission is ~100 lines of plumbing, deferred until someone has an actual case. Documented so it is not forgotten.

### Phase 4 design (locked before implementation)

Phase 4 introduces the first emitter with **cross-iteration state**: a reduction (`fold` / `sum` / `count` / `min` / `max`) over a collection, producing a single scalar number per input record. Four decisions are locked before writing `emit_fold_program`.

**1. Memory model: one stack slot, no arena.** The accumulator lives in a single 8-byte slot at the bottom of the rbp frame (`acc_slot`), reserved unconditionally by the prologue — same discipline as Phase 2D's `match_slot`. The inner loop reads the slot, combines with the current element, writes the slot back. No heap, no growable buffer. State is exactly one i64 wide, for the entire lifetime of one input record, and discarded at outer-loop bottom. Syscall surface stays identical to Phase 3: `read argv`, `write fd 1`, `exit`. Adding "cross-iteration state" at the source level does not add "cross-iteration state" at the syscall level.

**2. Output format: one number per input record on stdout, `\n`-terminated.** Uniform with scalar-output rules (Phase 0). `sum` returns the running total, `count` the number of elements where the predicate held, `min`/`max` the extremum (seeded with `i64::MAX` / `i64::MIN`), generic `fold` whatever the body computes. If the collection is empty, the init value is emitted verbatim — that's the well-defined answer (`sum` of empty = 0, `count` of empty = 0, `min` of empty = i64::MAX, `max` of empty = i64::MIN). The "min/max of empty" edge case is documented behaviour, not a silent bug; if a rule needs "empty is an error", it writes `Result(number, text)` with an explicit Err arm, which is Phase 2A territory, not Phase 4.

**3. Init must be a Number literal at parse time.** All desugarings of `sum` / `count` / `min` / `max` produce literal inits (`0`, `0`, `i64::MAX`, `i64::MIN`), so the restriction is free in practice. A non-literal init would require pre-loop evaluation into acc_slot, which is a (small) separate extension — not in v1. Verified at emit time: if `logic.value` is `Fold(_, init, _, _, _)` and `init` is not `Expr::Number(_)`, the emitter refuses with a clear message ("Phase 4: fold init must be a literal number").

**4. Scope v1 (everything else refused with a clear message):**
- Rule `output_ty` is `Number`.
- Top-level logic is `Fold(Field(Ident(input_name), coll_field_name), Number(init), acc_name, item_name, body)` — exactly the shape sum/count/min/max desugar to.
- Input concept: zero or more scalar fields + exactly one collection field, which must be the LAST field declared (mirrors Phase 3).
- Body is a scalar expression referencing `acc_name`, `item_name.<field>`, and any outer let/field bindings. No nested fold, no rule calls inside the body.
- No side effects in the body (no concat-to-stdout, no append_file, no rule calls that react).

**Frame layout.** Prologue extends Phase 3's layout by one slot:
```
rbp - 8*(nfields+1)             first field slot
...
rbp - 8*(nfields+nlets+1)       last let-binding slot
rbp - 8*(nfields+nlets+2)       acc_slot                       ← Phase 4 addition
```
`acc_name` is treated by the expression emitter as a read/write to `acc_slot` — no distinct register role needed. Accessing `acc_name` from the body goes through the same rbp-slot lookup as a let binding (offset table), keeping the expression emitter uniform.

**Register map: no new reservation.** The inner loop reuses `r15` (inner counter, role inherited from Phase 3) and `r13`/`r14` (argv base, current-record-start), same as Phase 3. No new entry in the register table. The "per-emitter role" policy for `r15` continues: a fold-output binary and a reaction-output binary never coexist, so the fd-vs-counter ambiguity stays resolved.

**Emission flow.**
```
_start:
  prologue (reserve field slots + let slots + acc_slot)
outer_loop_top:
  check r14 (record index) < argc - 1 else exit
  parse scalar input fields (atoi / argv-ptr for text) into field slots
  parse collection count N into r15
  store init literal into acc_slot
  set rcx = argv[ fields_base + 1 ]  (first element pointer)
inner_loop_top:
  test r15, r15; jz after_inner
  parse element fields (atoi / argv-ptr) into item slots (reused across iterations)
  evaluate body with acc=acc_slot, item=item_slots → rax
  store rax back into acc_slot
  advance argv ptr by element_field_count
  dec r15
  jmp inner_loop_top
after_inner:
  load acc_slot into rax
  emit_itoa_inline → stdout
  emit_write_newline(1)
  advance r14 past this record
  jmp outer_loop_top
```

**Why this design holds security pillar #1:** identical attack surface to Phase 3. Same three syscalls. One new stack slot — which is not an attack surface, it's a rbp offset. An auditor reading the disassembled binary sees one extra `mov [rbp-offset], rax` per iteration compared to Phase 3, and one final `itoa + write` at the bottom. Nothing else.

**What Phase 4 does NOT do** (refused with clear messages, deferred to later phases):
- `output: text` with fold producing a text (covered by Phase 5b — different accumulator shape).
- `output: Record` with fields computed by fold (would need multiple acc slots and a final record emission).
- Body containing a nested `fold` (would need acc-slot stack discipline — the outer/inner slots would collide).
- Generic `fold(coll, init_expr, ...)` with a non-literal init (pre-loop evaluation needed).
- `fold` whose target is a rule call returning a collection (Phase 3 already refuses collection-returning rule calls as map/filter targets; Phase 4 inherits the refusal).

### Phase 5b design (locked before implementation)

Phase 5b handles **text-valued folds**: `output: text` with a top-level `fold` that appends to a text accumulator over a collection. It's the first emitter where the accumulator SIZE grows across iterations — Phase 4's accumulator is one i64, Phase 5b's is a contiguous byte buffer whose final length is determined by the element data.

**Core design decision: append-only body shape.** The fold body must be `concat(acc, ...rest)` where `acc` is the FIRST argument and does NOT appear anywhere in `...rest` (nor recursively inside nested expressions). This is a structural invariant the emitter checks at compile time — not a style preference.

Why this restriction is load-bearing, not arbitrary:
- "Append to accumulator" is the semantic of text-fold every realistic use case wants (CSV rows, joined lists, report lines, command builders). A body like `concat(e.name, acc)` or `concat(X, acc, Y)` would mean "rebuild acc each iteration with prefix/interleave" — that's O(N²) memory regardless of strategy, and we'd be paying cost for a shape no one actually needs.
- With `acc` pinned to position 0, per-iteration byte growth = sum of sizes of `...rest` args, none of which reference the accumulator. That makes sizing statically decomposable into "per-element static bytes + per-element runtime-strlen bytes" — computable in one forward pass over the collection.
- The restriction is easy to explain ("append-only"), easy to relax later if needed (add more shapes as distinct cases), and survives cleanly if a future `text [..N]` declaration lands — the append-only shape is still valid, it just becomes faster to size.

**Sizing strategy: two passes over the collection.** The first pass walks argv to compute the total buffer size into rax (static per-element size + runtime strlen of each text-field arg in `...rest`, summed N times). After sizing, `mov r9, rsp; sub rsp, rax` reserves the buffer dynamically (same `r9`-saves-rsp trick as the Phase 1B text-field concat). The second pass walks argv again to actually fill the buffer. One `write(1, buffer, length)` + newline at the end, then `mov rsp, r9` to free.

Two passes is fine: the first pass is pure size arithmetic (strlen + add, no data movement), the second pass is the actual fill. No iteration of user logic is duplicated — the sizing pass doesn't evaluate the fold body, it only asks "how many bytes does each arg contribute?" which is a per-arg-kind lookup plus one strlen call per text field.

**Frame layout.** Phase 3's prologue extended by TWO slots:
```
rbp - 8*(nfields + 1)                 first scalar field
...
rbp - 8*(nfields + nlets + 1)         last let-binding slot
rbp - 8*(nfields + nlets + 2)         count_slot            (N, stored after parsing)  ← Phase 5b addition
rbp - 8*(nfields + nlets + 3)         argv_save_slot        (r14 at first element)     ← Phase 5b addition
```
Both slots are written at the top of the outer-loop body, before the sizing pass. They're read at the start of the fill pass to rewind r14 and r15 to their pre-pass-1 values. After the fill pass the slots are dead until the next outer iteration.

**Register map: no new reservation.**
- `r14` — argv index (walked forward twice, rewound from `argv_save_slot` between passes)
- `r15` — inner counter per pass (reloaded from `count_slot` for pass 2)
- `r10` — buffer base (for the final `write` syscall source)
- `r9`  — saved pre-allocation rsp (for `mov rsp, r9` cleanup)
- `rbx` — buffer write pointer during fill
- `r8`, `rax`, `rcx`, `rdx`, `rsi`, `rdi`, `r11` — ephemeral

No fresh register role, no collision with existing emitters.

**Emission flow.**
```
outer_loop_top:
  check r14 < argc - 1 else exit
  parse scalar input fields into rbp slots
  parse count N → r15 ; store r15 at count_slot ; inc r14
  store r14 at argv_save_slot                    ; remember first-element index

  ; --- pass 1: size ---
  mov rax, static_init_size                      ; init literal's length
size_loop_top:
  test r15, r15 ; jz size_done
  for each text-field arg in body's ...rest:
    mov rsi, [rbp + <field_offset>]              ; field offset relative to element start
    ; wait — we need to point rsi at argv[r14 + field_index]
    mov rsi, [r13 + r14*8 + field_index*8]
    push rax ; emit_strlen ; pop rcx ; add rcx, rdx ; mov rax, rcx
  add rax, static_per_element                    ; literals + 21 × numbers
  advance r14 by n_elem_fields
  dec r15
  jmp size_loop_top
size_done:
  add rax, 7 ; and rax, ~7                       ; round up to 8

  ; --- buffer allocation ---
  mov r9, rsp
  sub rsp, rax
  mov rbx, rsp                                   ; rbx = write ptr
  mov r10, rbx                                   ; r10 = buffer base

  ; --- copy init literal ---
  inline jmp-over-data ; lea rsi, [rip + init] ; mov rcx, init_size ; rep movsb ; mov rbx, rdi

  ; --- rewind r14 and r15 for pass 2 ---
  mov r14, [rbp + argv_save_slot]
  mov r15, [rbp + count_slot]

  ; --- pass 2: fill ---
fill_loop_top:
  test r15, r15 ; jz fill_done
  parse element fields into rbp element slots    ; same dispatch as Phase 3 (Number via atoi, Text stores ptr)
  for each arg in body's ...rest:
    evaluate/copy per kind (literal inline, number via itoa_to_buffer, text-field via strlen + rep movsb)
  advance r14 by n_elem_fields (already done by the per-field "inc r14" in the parse step)
  dec r15
  jmp fill_loop_top
fill_done:

  ; --- write(1, r10, rbx - r10) + newline ---
  mov rsi, r10 ; mov rax, 1 ; mov rdi, 1 ; mov rdx, rbx ; sub rdx, r10 ; syscall
  emit_write_newline(1)

  ; --- free ---
  mov rsp, r9

  jmp outer_loop_top
```

**Why this design holds security pillar #1:** identical syscall surface to Phase 4. Same three: read argv, write fd, exit. No heap. One additional `write` per input record (plus the newline), but that's a static property of the rule shape, not something that varies with input. Two new rbp slots (not an attack surface — just rbp offsets). Buffer size is a function of declared element data, not of arbitrary user input outside the concept's declared range.

**Scope v1 (everything else refused with a clear message):**
- `rule.output_ty == Type::Text`
- Top-level logic: `Fold(Field(Ident(input), coll_field), Text(init_literal), acc, item, Concat(args))`
- `args[0]` is `Expr::Ident(acc)`; `acc` does not appear anywhere in `args[1..]` (or nested within).
- `init` is a `Text` literal (analogous to Phase 4's Number-literal init).
- Input concept: scalars* + ONE trailing collection(Concept) field (same shape as Phase 3 / 4).
- Element concept: number/text fields only.
- Each arg in `args[1..]` classifies as either text literal, number expression, or text field (via the same `classify_concat_arg` Phase 1B uses).

**What Phase 5b does NOT do** (refused with clear messages, deferred):
- Non-append body shapes (`concat(X, acc)`, `concat(X, acc, Y)`, `if ... then concat(acc, ...)  else concat(acc, ...)`). Workaround: reorganize into append-only, use a trailing-separator pattern with an initial-empty init.
- Nested `fold` inside the body.
- `fold` target that is a rule call returning a collection.
- Bodies that are not a top-level `concat` (e.g. just `Ident(acc)` — a no-op fold producing the init unchanged; the verifier should probably reject this anyway).

### What native still rejects, and in which priority

- **Result(T, E) with non-scalar T** (e.g. `Result(Record, text)`, `Result(collection, _)`) — each shape needs its own calling convention. Decide shape by shape, never fabricate a "universal Result" that carries unnecessary machinery.
- **Reductions with non-number, non-text output** — Phase 4 covers `output: number` with top-level fold, Phase 5a covers `output: text` per-record, Phase 5b covers `output: text` via fold (append-only body, two-pass sizing). Still refused: `output: Record` with fold-computed fields (needs multi-slot record accumulator), nested folds (acc-slot stack discipline), and non-append-only text fold bodies like `concat(X, acc)` (would force O(N²) memory regardless of strategy — workaround: reorganize into append-only form).
- **Collection-returning rule calls or collection-valued reduction targets** — `map`/`filter` and Phase 4's `fold` target must be a direct `Field(Ident(input), coll_field)`. Composing through an intermediate rule that returns a collection is not supported; the caller has to inline the collection source.
- **`match_result` with non-pass-through Err arm** — Phase 2D handles `Err(<err_var>)` pass-through (the value flows directly to stderr without being bound). Richer Err arms (using err_var inside concat, or transforming it) need a real text-binding mechanism — two rbp slots per text bound var (ptr + len) since Err values from concat aren't NUL-terminated.
- **`match_result` with cross-concept callees** — Phase 2D requires callee.input_concept == outer.input_concept (so the rbp slots are reused as-is). Cross-concept calls need argument-passing through additional slots or a real callee frame.
- **Nested `match_result`** — Phase 2D reserves a single `match_slot` in the prologue; nested match_results would collide. Either reserve N slots based on a static walk or switch to a stack-based binding scheme.
- **Record fields with text-typed value coming from a text-returning rule call** — text literals, input-field texts, and `concat(...)` (including concat with text-field args via the dynamic buffer) all work as Record field values today (`fullname.verbose` exercises concat-in-Record-field). What's still refused is calling a rule that returns text and placing the result into a Record field — `emit_text_write_to_fd`'s fallback rejects `Call(...)`. Unlocking it needs a text-return calling convention for native (pointer + length in two registers, or an outparam stack slot), which is its own small phase.

### Register conventions across emitters

Emitters that span multiple syscalls or phases share a register layout. Adding a new cross-phase register use requires either claiming a currently-unused register or saving/restoring on the stack — do not casually reassign any of these without auditing every caller.

| Register | Used by | Introduced |
|---|---|---|
| `r12` | argc (read at `_start`) | Phase 0 |
| `r13` | argv base pointer | Phase 0 |
| `r14` | current record index inside the main loop | Phase 0 |
| `rbp` | field-slot frame base (fields + let bindings at `rbp - 8*(i+1)`) | Phase 0 |
| `r15` | (per-emitter role — one or the other, never both in the same binary): file descriptor from `open()` in reaction emitters (Phase 1A) / inner loop counter in collection emitters (Phase 3) | Phase 1A / 3 |
| `r10` | concat buffer base for later length calculation | Phase 1B |
| `rbx` | concat write pointer (advances as args are written) | Phase 1B |
| `r9`  | saved pre-allocation `rsp`, used by the dynamic-sized concat path to free the buffer via `mov rsp, r9` (Linux `write` takes only 3 args, so `r9` survives the syscall). Set only when at least one concat arg is a text field. | Phase 1B (text-field) |

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
