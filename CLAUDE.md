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
  purchase.*       Result(number, text) validator — validate_purchase compiles to a 705-byte native binary (Ok -> stdout, Err -> stderr); discounted_purchase (Phase 2D match_result) at 763 B
  layers.*         @layer stratification — architectural discipline verified
  bonus.*          record construction — map produces collection(BonusReport)
  audit_log.*      append_file reaction with dynamic concat content — compiles to a 724-byte native binary
  audit_simple.*   append_file with static content — compiles to a 464-byte native binary
  audit_user.*     append_file reaction whose log line concatenates a text-typed input field;
                   buffer sized at runtime via per-field strlen, freed via saved-rsp r9 (~847 B)
  enrich.*         Phase 2F: match_result with an enriched Err arm — `enriched` compiles to a ~700-byte native binary
                   (outer Err captures callee's Err into (ptr,len) slots, then concats user context)
  tier.*           Result(text, text) classifier — classify_tier compiles to a 602-byte native binary
  classify.*       Record-output rule — classify_invoice compiles to a ~970-byte native binary that emits one JSON object per record
  greeting.*       Text input field flowing into JSON output — make_report compiles to a ~590-byte native binary
  fullname.*       Record output whose text field is built via concat of input text fields — compose_greeting compiles to a ~758-byte native binary
  compose.*        Phase 2G: text-returning rule call inlined at the call site — name_line delegates to display_name and compiles to a ~529-byte native binary
  log_via_helper.* Phase 2H-a: reaction append_file content is a helper rule call — log_alert compiles to a ~655-byte native binary
  greeting_line.*  Phase 5a: `output: text` per-record — greeting_line compiles to a ~564-byte native binary
  roster.*         Phase 5b: `output: text` via top-level fold — roster_line compiles to a ~708-byte native binary
                   (append-only body: concat(acc, e.name, "=", e.salary, "; "); two-pass sizing, single write per input record)
  ledger_line.*    Phase 2I: non-literal text let bindings. `let tagged = concat(...)`, `let full = concat(tagged, ...)`,
                   return value chains through both lets. Compiles to ~964 B; exercises chained (ptr, len) slot resolution.
  gate_result.*    Phase 2I extended to Result rules. Ok and Err arms each reference a distinct text let binding
                   (`let greeting = concat(...)`, `let rejection = concat(...)`). Compiles to ~750 B; admitted→stdout,
                   rejected→stderr with exit 1.
  payroll.*        Phase 3: four rules on the same input — map to Record (~670 B), filter (~670 B), map to number (~455 B), map to text (~410 B).
                   Phase 4: two reductions on the same input — sum (~486 B), count (~532 B).
                   (purchase.verbose::discounted_purchase compiles to ~750 bytes via Phase 2D match_result inlining)
  logs.*           Log analyzer — event stream analysis with count/sum/fold/all/any (5 rules, 5 compile natively);
                   multi-rule stdin binary: 4 metrics in 2233 B. Phase 6 enabled health_score.
  config.*         Config validator — Result(number,text), Result(text,text), text ==, match_result composition,
                   cross-field bool constraint (5 rules, 5 compile natively);
                   multi-rule stdin binary: 5 validations in 2929 B.
  alert.*          Streaming event filter — --stream mode, reads events line by line,
                   filters by severity + source. 772-byte long-running process.
                   Usage: tail -f events.log | ./alert_filter
  audit_complete.* Phase 8 slices 8b+8c showcase: HTTP service whose JSONL audit
                   log captures req.timestamp + req.method/path + resp.status/body
                   in one line per request. ~1735 B native binary on port 18892.
  echo_path.*      Phase 7 slice 3d showcase: HTTP handler whose body is built at
                   runtime via concat(req.method, req.path, ...). Three routes
                   (GET/POST/other-404) all emit a response body that echoes the
                   request slots. ~1263 B native binary on port 18893.
  method_guard.*   Phase 7 slice 3e showcase: single HttpResponse record with a
                   computed status (200 for GET, 405 otherwise) and a concat body
                   echoing the path. Demonstrates 3d + 3e composing in one record
                   without if/else record duplication. ~921 B on port 18894.
  audit_strict.*   Phase 8 slice 8d showcase: log block with on_error: abort.
                   Server exits with status 1 on any open/write failure in the
                   audit path — fail-closed posture for Article 12 chains.
                   ~1240 B on port 18895.
  access_audited.* AI Act high-risk gate (HTTP-fronted). Combines slices 3d/3e/8b/8c/8d:
                   user-facing reason (resp.body) is bit-for-bit the audit log
                   reason; req.timestamp captured per-request; on_error: abort
                   for fail-closed audit. Worked example for docs/ai-act-usage.md
                   Pattern 2. ~2019 B on port 18897.
  read_config.*    Phase 9 slice 1: top-level `resource` declaration + `read(name)` in
                   a rule's logic. open(O_RDONLY) + read + close emitted once per rule
                   invocation; on_read_error: abort exits with status 1 on any syscall
                   failure. ~541 B native binary; reads /tmp/verbose_demo_config.txt.
  api_gateway.*    Phase 11 slice 2: HTTP service whose handler `body` field is
                   `fetch(upstream, "GET /health HTTP/1.0\r\n\r\n")`. Per request:
                   accept → open outbound socket → connect → send → recv → close →
                   serialize. 1011 B native binary on port 18920. First Verbose binary
                   that's both a server AND a client.
  health_check.*   Phase 11 slice 1: outbound TCP fetch. `connection upstream` +
                   `fetch(upstream, "GET /health HTTP/1.0\r\n\r\n")` in a rule. 623-byte
                   binary makes a real HTTP/1.0 GET to a declared host:port and emits
                   the response to stdout. First Verbose binary that opens an outbound
                   connection. IPv4 literal only, max_response bounded, fail-closed on
                   any socket / connect / write / read error.
  static_file_server.* Phase 9 slices 2+4 + Phase 10: HTTP/1.0 static file server.
                   Composes 3e (computed status) + 8a–8d (audit log, on_error: abort) +
                   9.1 (resource decl) + 9.2 (read in handler) + 9.4 (cache: true) +
                   10 (concurrency: forked). 1730 B (forked + cached). Parent reads the
                   file once at startup, kids inherit via fork's COW — file edits are
                   NOT picked up until restart (the trade-off the operator opts into).
  demo.html        Browser demo (WASM)

tools/
  generate.sh      Intent -> Verbose via Claude API
  benchmark.sh     Reproducible comparison vs gcc
```

## Language Features (current)

- Types: `number`, `bool`, `text`, `collection(Type)`, `Result(T, E)` (declared failure path), named types
- Field ranges: `amount : number [0, 1000000]`, `name : text [..64]` (max byte length; verifier carries the bound, native can exploit for compile-time buffer sizing)
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
- Proofs: purity (reads/calls), termination (bound)
- Hints: `vectorizable: "reason"`, `parallel: "reason"`, `cache_result: "reason"` (justification required, parser rejects bare form), `overflow: [min, max]` (bounds mechanically verified against interval arithmetic)
- Traceability: `@intention` (string), `@source` (file:line), `@layer: domain|application|interface` (optional, sealed-subgraph discipline)
- Modules: `use "stdlib/finance.verbose"`
- Reactions: declared side effects with trigger rules; effects today are `print` (to stdout) and `append_file "path" content` (to a file). Path is a string literal at parse time — dynamic paths are refused so the auditor reads every file the program can ever touch.
- String escapes: `\n`, `\t`, `\\`, `\"` — closed set, unknown escape is a lex error (no silent pass-through for typos).
- Three backends: interpreter (--run), Rust transpiler (--compile), native x86-64 (--native), WASM (--wasm)
- Input modes: argv (default), one-shot stdin (--stdin), streaming line-by-line (--stream)

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
| 2D | `match_result(callee(input), v => Ok(<arith using v>), e => Err(e))` — inlined-callee form. Callee's logic is walked and its Ok/Err leaves are redirected: Ok values bind to a reserved `match_slot` then evaluate the outer Ok arm; Err values write directly to stderr (Err pass-through). Restricted to same-input-concept callees. Pass-through Err arm now routes through the Phase 2F slot path (negligible size overhead vs. the direct-write shortcut). | ~760 B | `purchase.verbose::discounted_purchase` |
| 2F | `match_result` outer Err arm can **transform** the callee's Err value. Two rbp slots (`err_ptr_slot`, `err_len_slot`) represent the bound err_var as a (ptr, len) pair — uniform shape for literals, input-field texts, and concat outputs (the latter aren't NUL-terminated). A third slot (`err_frame_save_slot`) captures pre-capture rsp so any concat buffer the callee's Err allocated gets freed at the end of the outer Err arm via `mov rsp, [rbp+err_frame_save_slot]`. Outer Err body can be any `Err(<text_expr>)` — literal, field, Ident(err_var), or concat mixing any of those. See "Phase 2F design (locked)" below. | ~700 B | `enrich.verbose::enriched` |
| 2G | Text-returning rule call inlined in `emit_text_write_to_fd`. When a text-position expression is `Call(helper, [Ident(input)])`, the emitter recurses on `helper.logic.value` — byte-for-byte equivalent to inlining the helper's body. Same-concept / same-input-name / no-lets restrictions mirror Phase 2D. Unlocks one new code path that flows into every existing text sink (`output: text`, Record field, match_result Err, Result(text,_) arms). See "Phase 2G design (locked)" below. | ~529 B | `compose.verbose::name_line` |
| 2H-a | Same Phase 2G inlining applied to reaction `append_file` content. `emit_append_file_call` factored into `emit_append_write_to_r15`, which recurses on `callee.logic.value` for the Call case with the same restrictions as 2G. The reaction's `open` / `close` bookkeeping stays around the recursion. | ~655 B | `log_via_helper.verbose::log_alert` |
| 2H-b | `Call` as a `concat(...)` argument. Pre-eval loop reserves a `16*N` slot array pointed to by `r11`, evaluates each Call exactly once, stashes `(rax=ptr, rdx=len)` at `[r11 + 16*i + {0,8}]`. Sizing reads `[r11+16*i+8]`; filling copies from `[r11+16*i]`. Final `mov rsp, r9` frees everything. Nested inner concat (callee body = concat) uses `is_nested=true` to skip its own r9 save and refuse further CallText (one level of pre-eval). See "Phase 2H-b design (locked)" below. | ~560–780 B | `compose.verbose::greeting` (772 B) |
| 2I | Non-literal text `let` bindings in `output: text` rules. The prologue's let-eval loop classifies each RHS as text (Concat / text-typed Field / text-returning Call / Ident pointing at a prior text let) or number (everything else). Text bindings get two consecutive rbp slots (ptr, len) — same shape as Phase 2F's err_var — and are registered in a `TextBindings` map carried on `RecordLoopCtx`. The text-output emitter passes that map to `emit_text_write_to_fd`, so `Ident(let-name)` resolves as a BoundText wherever it appears in the logic body or in a later let RHS. The record-loop epilogue's `mov rsp, rbp; pop rbp` frees any concat buffer the lets allocated, once per record. | ~960 B | `ledger_line.verbose::format_line` (964 B) |
| 2I-R | Same as 2I, extended to Result rules (`Result(number, text)` and `Result(text, text)`). `ctx.text_bindings` is threaded through `emit_eval_result_expr` → `emit_match_result_inlined` → `emit_redirect_callee_leaves` so `Ident(let-name)` resolves in Ok, Err, and match_result Err-capture arms. Phase 2F's err_var binding now MERGES with the caller's text_bindings (one clone + insert), letting the outer Err body reference both prior text lets and the captured err_var in the same concat. Scope still pending: service handlers and collection/fold contexts. | ~750 B | `gate_result.verbose::gate` (750 B) |
| 3 | `output: collection(T)` with `map` or `filter` — streaming element emission (one JSON Lines per element), no arena, count-prefixed argv. `filter` uses identity pass-through: predicate false skips emission, predicate true emits the element as-is. See "Phase 3 design (locked)" below. | ~670 B | `payroll.verbose::compute_bonuses` (map) / `::high_earners` (filter) |
| 3.2 | `output: collection(number)` / `collection(text)` — scalar element map. `map(w.employees, e => e.salary)` emits one number per line; text body emits one string per line. No JSON wrapping, so scalar-element binaries are smaller (~400-500 B). | ~455 B | `payroll.verbose::salaries` / `::names` |
| 4 | `output: number` with `fold`/`sum`/`count`/`min`/`max` at the top level — inner loop accumulates into a single stack slot, emits the final value on stdout once per input record. First emitter with cross-iteration state; no arena (the accumulator is one i64). See "Phase 4 design (locked)" below. | ~490–530 B | `payroll.verbose::total_salaries` (sum, 486 B) / `::high_earner_count` (count, 532 B) |
| 5a | `output: text` with a per-record body — literal, input text field, or `concat(...)`. One `write` to stdout + newline per input record; no accumulator, no state carried across iterations. Routes to `emit_text_program`, which reuses `emit_text_write_to_fd` (already serving Phase 2B's Ok-text arm). Fold-over-collection to text is Phase 5b. | ~320 B (literal) / ~330 B (field) / ~560 B (concat) | `greeting_line.verbose` (concat, 564 B) |
| 5b | `output: text` via top-level `fold` — appends into a text accumulator over a collection. Body is strictly append-only: `Concat(Ident(acc), ...rest)` with `acc` absent from `rest`. Two-pass emission (pass 1 sums per-element static + `strlen` per text-field arg into rax; pass 2 fills the buffer). `mov r9, rsp; sub rsp, rax` reserves, `mov rsp, r9` frees. See "Phase 5b design (locked)" below. | ~700 B | `roster.verbose::roster_line` (708 B) |

| 6 | Scalar output (`number`/`bool`) with embedded quantifiers — `if all(xs, p) then X else if any(xs, p) then Y else Z`. Quantifiers are extracted from the expression tree, desugared to folds, and computed in a **single pass** with one accumulator slot per fold. After the inner loop, the remaining scalar expression is evaluated against the fold results. Multi-accumulator design means N quantifiers = 1 pass, not N passes. | ~700 B | `logs.verbose::health_score` (702 B) / `report.verbose::risk_score` (702 B) |
| stdin | `--stdin` flag prepends a stdin reader prologue (~173 B) that reads whitespace-separated tokens from fd 0, tokenizes, and builds an argc/argv layout on the stack so the rule prologue works unchanged. Enables `echo "data" \| ./binary` and `./binary < file.txt`. Adds ~173 B overhead to any phase's binary. Design in `docs/stdin-reader-design.md`. | +173 B | any example with `--stdin` |
| stream | `--stream` flag wraps the rule code in a line-by-line read loop. Reads ONE line from stdin per iteration (byte-by-byte until `\n`), tokenizes, processes, loops. On EOF, exits cleanly. First long-running Verbose binary. Not supported with SIMD-vectorized or parallel rules. | ~770+ B | `alert.verbose::should_alert` (772 B) |
| 7 (3a–3c) | Top-level `service` construct with `Protocol::Http10` / `Protocol::RawTcp`. Slice 3a synthesises the built-in `HttpRequest` / `HttpResponse` concepts at verify time. Slice 3b emits a constant-response binary (handler logic = a literal `HttpResponse` record). Slice 3c emits the dynamic router: a per-accept loop with HTTP parse → handler → HTTP serialize, with the handler's if/else chain producing the response slots. Same shared register convention as the rest of native (r12=server fd, r14 unused here, rbp frame for handler I/O). | ~430 B (constant) / ~1 KB (router) | `hello_http.verbose` / `hello_router.verbose` |
| 7 (3d) | Handler body assembled at runtime via `concat(...)` of literals, request text fields (`req.method`, `req.path`), and numbers. The concat runs through the existing `emit_concat_to_buffer` path (same code serving reaction logs and `Result(text, _)` arms), leaving `(rax=ptr, rdx=len)` which we store in the body slots `[rbp-32]`/`[rbp-40]`. Iteration epilogue frees the buffer — and any log buffer stacked above it — via `lea rsp, [rbp - frame_size]` (7 bytes) right before `jmp accept_top`. Works because nothing between `accept` and the handler touches `rsp`, so pre-handler `rsp` is always `rbp - frame_size`. | ~1.3 KB | `echo_path.verbose::echo_server` (1263 B) |
| 7 (3e) | `status` field accepts any Number-typed expression inside one HttpResponse record (not just a literal). Slice 3c forced you to wrap in `if … then HttpResponse{200,…} else HttpResponse{404,…}` even when the body was identical; 3e lets `HttpResponse { status: if cond then 200 else 405, body: … }` stand alone. Native trusts the verifier's type-check against the declared `status: number [100, 599]` range and dispatches non-literal status through `emit_eval_expr` → `mov [rbp-24], rax`. Number-literal status keeps the 7-byte immediate-store fast path. | ~900 B | `method_guard.verbose::guard_endpoint` (921 B) |
| 8 (8a–8c) | Per-request `log:` block on a service. Closed grammar: `text`/`number` literals, `concat(...)`, plus `req.method`, `req.path`, `req.timestamp`, `resp.status`, `resp.body`. Slice 8a wires the `append_file` between handler and serializer using shared `emit_append_file_call` (same path as reaction logs). Slice 8b enriches the log scope with `resp.status` (number, slot -24) and `resp.body` (text via BoundText, slots -32/-40) — handler-populated, no extra runtime cost. Slice 8c adds `req.timestamp` (number, slot -56) backed by one `clock_gettime(CLOCK_REALTIME)` after `accept` — frame grows by 8 only when timestamp is referenced. The handler never sees these names; the rewrite is local to the log scope so the response stays reproducible from `(method, path)`. | ~1.3 KB (no ts) / ~1.7 KB (with ts) | `hello_router_logged.verbose` (1294 B) / `audit_complete.verbose` (1735 B) |
| 8 (8d) | Optional `on_error: drop \| abort` line in the log block. Drop is the default (slice 8a behaviour, silent on failure). Abort emits a `test rax, rax; js abort_label` (8 bytes) after each fallible log syscall — open and write — branching to a shared `mov rax, 60; mov rdi, 1; syscall` epilogue (16 bytes) at the end of the binary. Cost: zero when policy is Drop (no checks, no label). Lets the operator opt into fail-closed audit semantics when an Article 12 chain requires that no log persisted means no claim of having served the request. Reaction effects (rules) keep the Drop default; the knob is service-level only. | ~1.2 KB | `audit_strict.verbose::strict_endpoint` (1240 B) |
| 9 (slice 1) | Top-level `resource <name>` declaration + `read(<name>)` expression. The path is a literal embedded inline (auditor sees every file the binary can open); `max:` bounds a per-resource stack buffer. `emit_record_loop_prologue` walks the rule's logic for `Read` references, allocates `(ptr_slot, len_slot)` + buffer per unique resource, and emits `open(O_RDONLY) → test+js abort → mov r15, rax → read → store len → test+js abort → close` ONCE per rule invocation (before `loop_top`, so per-record loops don't reread). `text_bindings` registers `name → (ptr, len)` so `Expr::Read` reuses the Phase 2I/2F BoundText path through `emit_text_write_to_fd` / `emit_concat_to_buffer`. Failure routes to a per-rule abort label (sys_exit 1). Slice 1 covers `output: text` rules; collection / fold / record / service-handler contexts still reject. | ~540 B | `read_config.verbose::load_config` (541 B) |
| 9 (slice 2) | `read(<name>)` inside an HTTP service handler body. Same `emit_resource_read_sequence` helper as slice 1, hoisted into the per-accept iteration of `emit_http10_dynamic_bytes` (right after `accept` + the optional `clock_gettime`, before HTTP parse + handler dispatch). `frame_base` grows by `sum(16 + max_padded)` per resource, the read buffer + parser scratch shift below. Resource (ptr, len) registered in a local `text_bindings` threaded through `emit_handler_to_slots`; the body field accepts `Expr::Read(name)` directly (loads (ptr, len) into `[rbp-32]/[rbp-40]`) AND inside `concat(...)` via the existing BoundText classifier path. Per-accept, not per-binary-lifetime: the operator can edit the file and the next request sees the new content. Failure shares the slice 8d abort label. Composes with 3e status, 8b/8c log fields, 8d on_error. | ~1.5–1.6 KB | `static_file_server.verbose::static_server` (1572 B) |
| 10 | Service-level `concurrency: forked` opt-in. Default `Sequential` keeps every prior service binary byte-for-byte identical (purely additive slice). When `Forked`: a one-shot `rt_sigaction(SIGCHLD, SIG_IGN, NULL, 8)` (kernel-ABI 32-byte struct inlined via jmp-over-data) runs before the `listen` syscall — kernel auto-reaps children, no `wait`/`waitpid`, no zombies, zero per-request bookkeeping. Then after each `accept` saves client_fd: `mov rax, 57; syscall` (fork), `test rax, rax`, `js fork_error` (write `"fork failed\n"` to stderr + close client_fd + jmp accept_top), `jz child` (fall through to existing iteration body), parent path closes client_fd + `jmp accept_top`. The iteration body is shared — child falls through; parent skips. At the iteration tail, a `match service.concurrency` swaps the existing `lea rsp + jmp accept_top` for `mov rax, 60; mov rdi, 0; syscall` so the child exits 0 instead of looping. r12 (server fd) survives across fork by kernel guarantee; r15 (resource fd from slice 9.2) is allocated in the child after fork, so no parent/child conflict. Restricted to `Protocol::Http10` (verifier rejects forked raw_tcp). | +~160 B | `static_file_server.verbose` with `concurrency: forked` (1730 B) |
| 11 (slice 2) | `fetch(name, "...")` inside HTTP service handler bodies. Same `emit_connection_fetch_sequence` helper as slice 11.1, hoisted into the per-accept iteration of `emit_http10_dynamic_bytes` (right after the per-accept resource block, before HTTP read). `connection_extra_bytes` extends `frame_base` so per-connection (ptr, len, buffer) slots are below the resource block. r15 reused as outbound socket fd, freed by close before the next iteration. The body field accepts `Expr::Fetch(name, request_expr)` directly via the same BoundText path slice 9.2 wired for `Expr::Read`. Composes with read+fetch in the same handler (resource fd closed before fetch fd opens). Same constraints as slice 11.1: literal-or-concat-of-literals request bytes (slice 11.3 lifts), one fetch per connection per handler, on_connect_error: abort. | ~1.0 KB | `api_gateway.verbose::gateway` (1011 B) |
| 11 (slice 1) | Outbound TCP via `connection <name>` declaration + `fetch(name, request_bytes)` primitive returning `text`. The first OUTBOUND syscall family. New `Item::Connection(Connection)` AST variant; new `Expr::Fetch(name, request_expr)`. Connection block declares `host: "X.X.X.X"` (IPv4 dotted-quad literal — no DNS, no IPv6, no `localhost`), `port: 1..=65535`, `max_response: 1..=64MiB`, `on_connect_error: abort`. Verifier rejects domain names, port out of range, duplicate connection name, `Expr::Fetch` referencing an undeclared connection, or absence from rule's `reads:` proof. Native: `emit_connection_fetch_sequence` runs above `loop_top` (once per rule invocation): socket(AF_INET, SOCK_STREAM, 0) → js abort → mov r15, fd → connect(r15, &sockaddr_in, 16) → js abort → write(r15, request_ptr, request_len) → js abort → read(r15, response_buf, max_response) → js abort → close(r15). Inline 16-byte sockaddr_in literal via jmp-over-data: family=AF_INET, port=htons(literal), addr=inet_aton("X.X.X.X") in network byte order, 8 bytes pad. Response (ptr, len) registered in `text_bindings` so `Expr::Fetch` resolves through the BoundText path. Slice 1 restriction: one fetch per connection per rule, request bytes must be a literal-or-concat-of-literals (no per-record dynamic body), rules-only (service handlers in slice 11.2). | ~620 B | `health_check.verbose::check_health` (623 B) |
| 9 (slice 4) | Opt-in `cache: <true\|false>` on resource declarations (default `false` — purely additive slice, byte-for-byte backward compat). When `true`, the resource's open/read/close sequence is hoisted ABOVE the `accept_top` label in `emit_http10_dynamic_bytes` (between `LISTEN` and `accept_top`) — runs ONCE at server startup. The (ptr_slot, len_slot) populated by that read sit within the prologue-allocated frame and survive every iteration's `lea rsp, [rbp - frame_size]` epilogue. Trade-off: ~3 µs syscall work saved per request, at the cost of staleness (operator edits to the file are NOT picked up until restart). With `concurrency: forked`, the cached read happens once in the parent before the accept loop; children inherit the populated buffer slot via fork's copy-on-write — zero per-child read cost. Best-case efficiency for static assets on a forking server. For rules (non-service): `cache: true` is a syntactic no-op since `emit_record_loop_prologue` already reads above `loop_top` (once per rule invocation). Cached-read failures share the same end-of-binary `sys_exit(1)` abort label as the per-iteration path. | ~1.7 KB (forked + cached + log + timestamp) | `static_file_server.verbose` with `cache: true` (1730 B) |

*Locked designs for each phase (3, 4, 5b, 2F, 2G, 2H-b) are in [docs/native-designs.md](docs/native-designs.md). They're frozen after implementation — consult them for rationale, not for the current state.*

### What native still rejects, and in which priority

- **Result(T, E) with non-scalar T** (e.g. `Result(Record, text)`, `Result(collection, _)`) — each shape needs its own calling convention. Decide shape by shape, never fabricate a "universal Result" that carries unnecessary machinery.
- **Reductions with non-number, non-text output** — Phase 4 covers `output: number` with top-level fold, Phase 5a covers `output: text` per-record, Phase 5b covers `output: text` via fold (append-only body, two-pass sizing). Still refused: `output: Record` with fold-computed fields (needs multi-slot record accumulator), nested folds (acc-slot stack discipline), and non-append-only text fold bodies like `concat(X, acc)` (would force O(N²) memory regardless of strategy — workaround: reorganize into append-only form).
- **Collection-returning rule calls or collection-valued reduction targets** — `map`/`filter` and Phase 4's `fold` target must be a direct `Field(Ident(input), coll_field)`. Composing through an intermediate rule that returns a collection is not supported; the caller has to inline the collection source.
- **Nested `concat(...)` inside a Call arg with its own Call args** — Phase 2H-b unlocked `Call` as a concat argument (one level of pre-eval). The callee's body can itself be a concat, but that inner concat cannot have its own Call args (`is_nested=true` rejects them with a clear message). Two levels of pre-eval would need ad-hoc rbp slots for the outer's r11 across nested pre-evals. Workaround: flatten the composition into a single concat, or an intermediate helper rule.
- **`match_result` with cross-concept callees** — Phase 2D requires callee.input_concept == outer.input_concept (so the rbp slots are reused as-is). Cross-concept calls need argument-passing through additional slots or a real callee frame.
- **Nested `match_result`** — Phase 2D reserves a single `match_slot` in the prologue; nested match_results would collide. Either reserve N slots based on a static walk or switch to a stack-based binding scheme.

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

## Transpilation Strategy (rejected direction)

Rust/Go/other source → Verbose transpilation is **rejected** for the same reason as LLVM: the source does not contain Verbose's declarations (reads, overflow bounds, termination bound, intention). Any transpiler must either emit trivial proofs (losing all verification value and all hint-driven optimizations) or infer them (violating the zero-trust rule that proofs are declared, never guessed).

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
- **Pop sub-agents for exploration-heavy work.** When mapping a refactor across many files, scanning for every touch point of a construct, or investigating a broad "where is X used" question, delegate to a sub-agent (`Agent` tool, `Explore` type for searches, `general-purpose` for mixed read+reasoning). The agent's summary lands in the main context; the raw file reads do not. Reserve the main context for actual edits and conversation with the human. Sub-agents are not a substitute for judgment — always read the code you're about to modify yourself, and verify the agent's claims against the file before acting on them.

## Design Lessons

R&D journal of hard-won insights — documented scars from building a language that doesn't exist yet. Read before proposing large changes: [docs/design-lessons.md](docs/design-lessons.md).


## Running

```bash
cargo run -- examples/collections.verbose                                           # verify
cargo run -- examples/collections.verbose --run client_blocked --input examples/collections.json  # interpret
cargo run -- examples/report.verbose --run total_revenue --input examples/report.json --json  # JSON output
cargo run -- examples/business.verbose --compile /tmp/business                      # transpile to Rust
cargo run -- examples/business.verbose --native /tmp/biz --run critical_invoice     # native x86-64
cargo run -- examples/invoices.verbose --native /tmp/inv --run important_invoice --stdin  # native, reads stdin
echo "15000" | /tmp/inv                                                            # → true
cargo run -- examples/alert.verbose --native /tmp/alert --run should_alert --stream  # streaming
printf "3 auth\n1 web\n" | /tmp/alert                                              # → true\nfalse
cargo run -- examples/invoices.verbose --wasm /tmp/rule.wasm --run important_invoice # WASM
cargo run -- examples/invoices.verbose --benchmark --run important_invoice          # compare all backends
cargo run -- --demo-http /tmp/server                                                 # HTTP server — tier-3 emitter probe, NOT in .verbose (see docs/known-gaps.md)
cargo test                                                                          # 218 tests
make demo                                                                           # full demo
```

## License

Apache 2.0
