# Vision Journal

Chronological record of strategic thinking about what Verbose is for, who it's for, and why it might matter. Different from `design-lessons.md` (technical scars) and `known-gaps.md` (to-do): this is about *positioning, thesis, use cases, doubts*. Most recent entries on top.

The vision is the author's. These entries are written by the AI assistant under his direction — he leads the thinking, pushes back on drift, and holds the compass; the AI reformulates, explicates, and writes down what was said. When a reframing lands in these pages, it comes from his correction, not from AI synthesis. The point of the journal is to keep the trail readable months later, including the wrong turns the AI tried and he rejected.

---

## 2026-04-20 (afternoon) — Phase 7 slices 1 + 2a + 2b: first .verbose-described TCP server

### Context

Same day as the AI Act consolidation. Creator validated option B (add
`bytes` as a first-class type) with Option 1 for service/handler bound
matching (strict equality, no magic). Direction: take the time to do
Phase 7 properly in small slices, each auditable independently.

### What shipped

Three commits, each one slice of Phase 7:

1. **Slice 1** (70f616e): `service` top-level construct — AST, parser,
   verifier (source ref exists, port in [1, 65535], max_request > 0,
   handler rule exists). Closed protocol set with one variant: `raw_tcp`.
   No emitter yet.

2. **Slice 2a** (adfe648): `Type::Bytes` as first-class language type.
   Type-isolated from Text by design (raw sockets carry NUL, binary
   data, invalid UTF-8 — calling that "text" is a semantic lie the
   auditor would pay for). Parser accepts `bytes [..N]` on concept
   fields via the same mechanism as text. Service verifier enforces a
   strict shape for RawTcp handlers: input and output each a Named
   concept with exactly one bytes field whose bound equals the
   service's `max_request`.

3. **Slice 2b** (67cfbd9): RawTcp emitter. `compile_service` dispatches
   on protocol; for RawTcp with an identity handler it calls
   `emit_raw_tcp_echo_bytes(port, max_request)`, the shared emission
   body now used by both `compile_echo_server` (tier 3) and the new
   service path (tier 1). Wired into `--native --run <service_name>`
   dispatch in main.rs.

### Concrete result

```
$ cargo run -- examples/raw_tcp_echo.verbose --native /tmp/tcp_echo --run echo_server
verified: 1 concept(s), 1 rule(s); all proofs check out
service: /tmp/tcp_echo (358 bytes, port 7777)

$ /tmp/tcp_echo &
$ echo "Hello from Verbose!" | nc -N localhost 7777
Hello from Verbose!
```

**358 bytes — exactly the size of the tier-3 probe.** A regression test
asserts bit-for-bit equivalence between the two paths. The tier-3 → tier-1
collapse that `docs/phase-7-design.md` promised, delivered.

### Why this matters

The binary is unchanged; what changed is the **source of authority**.
Before: `compile_echo_server` in Rust was what made the echo work, and
the auditor had to read Rust to understand the binary. Now: the
`.verbose` source is what makes the echo work, verified mechanically by
the compiler, and the auditor reads the `.verbose` file. The Rust code
in native.rs is still the emitter implementation, but the emitter is
trusted-once and the per-binary trust has moved to the source.

This is the first proof that Phase 7 actually delivers on its promise.
Future slices (HTTP/1.0, more handler operations) add mechanical
expansion on top of the same pattern.

### Scope discipline in evidence

Every slice was deliberately narrow:
- Slice 1 shipped grammar but no emission (testable alone).
- Slice 2a shipped the type but no emitter for it (testable alone).
- Slice 2b shipped the emitter only for identity handlers, with a
  clear error message for anything else.

Each slice added 5–10 tests and broke zero existing ones. 161 → 173
across the three slices. The no-TODO discipline held.

### Decisions locked

- Bytes and Text are type-isolated, permanently. No implicit conversion
  between them. An audit reviewing a handler declared on `bytes` knows
  the code is not accidentally treating the data as a string.
- Service/handler bound matching is strict equality, not subset.
  Reasoning: any looser rule (handler bound > max_request) means the
  handler expects bytes that will never arrive, which is false
  explicitation. Symmetry is the simpler rule.
- Identity-handler-only for slice 2b is an explicit restriction, not
  an oversight. Non-identity handlers require byte operations (len,
  concat, literal) that land one at a time in later slices, each with
  its own proof obligations.

### Open for next slice

- **Slice 2c (incremental, bytes operations):** add bytes literals
  (`b"..."` syntax), then `concat(bytes, bytes, ...) -> bytes` with
  bound arithmetic (sum of input bounds ≤ max_request), then
  equality `bytes == b"..."`. Each earns its place in the language
  only if a concrete handler use case demands it.
- **Slice 3 (strategic leap, HTTP/1.0):** introduce `Protocol::Http10`
  with compiler-provided `HttpRequest` / `HttpResponse` concepts and
  built-in parser + serializer in native.rs. This is the bigger leap —
  it unlocks "Verbose-described HTTP service" which is the visible
  demo target — but it depends on nothing from slice 2c (bytes stay
  out of the HTTP handler's sight; the compiler parses bytes into
  HttpRequest before the handler runs).

Slice 3 is the more impactful next step, but it is also the bigger
chunk. Slice 2c is the safer continuation.

---

## 2026-04-20 — AI Act pattern validated at two instances; wrapper is domain-agnostic

### Context

First full session after the doc-and-demo sprint of 2026-04-19. Goal: turn the
one-off `loan_decision` demo into a reusable pattern, then verify the pattern
generalises by applying it to a second Annex III category.

### What was built

- `docs/ai-act-usage.md` — first user-facing doc in the repo. Five-point
  pattern, per-article mapping table, ~20-line Article 12 shell wrapper
  (`audit-log.sh`), explicit out-of-scope list (Article 10 data governance,
  Article 14 human oversight, Article 72 drift detection, GDPR 22, conformity
  assessment). Verified wrapper runs on the loan decision binary and produces
  clean JSONL audit records.
- `examples/cv_screening.verbose` — second AI Act case, Annex III point 4(a)
  (recruitment / candidate selection). 4 criteria (experience, diploma,
  skills match, language), Result(number, text) with priority score on the
  Ok arm and one Err branch per failure mode. 1569 B native streaming binary,
  6/6 verdicts correct on synthetic inputs.
- README updated with a "Worked example: EU AI Act high-risk decisions"
  section pointing to the doc and both examples. `ai-act-usage.md` now
  discoverable from the entry page, not buried in `docs/`.

### Concrete findings

1. **Pattern duplication is a 30-minute exercise**, as claimed in the doc.
   Writing `cv_screening` from scratch (intent + verbose + test + compile)
   took about that long, following the template literally. The ~30-minute
   claim in the doc is now verified empirically on one data point.

2. **Binary size is predictable for four-criterion decisions.** `loan_decision`
   compiled to 1554 B, `cv_screening` to 1569 B — within 15 bytes of each
   other despite different domains. The enveloppe holds: a `Result(number, text)`
   rule with four `Err` branches and a small arithmetic Ok lands in the
   1.5 KB band.

3. **The Article 12 wrapper is truly domain-agnostic.** `audit-log.sh`,
   written for `loan_decision`, worked on `cv_screening` without a single
   edit. The wrapper depends only on the `Result(T, text)` shape —
   specifically on the stdout (approvals) / stderr (refusals+reasons)
   split that every Verbose binary of this pattern produces. This is
   stronger evidence than the 30-minute duplication: it means a compliance
   officer monitoring many high-risk decisions gets one audit log schema
   across all of them.

4. **Homogeneous audit output matters for compliance ops.** A single JSONL
   schema (`ts`, `input`, `verdict`, `value` | `reason`) covers finance and
   hiring decisions identically. One parser, one retention policy, one set
   of SIEM rules monitoring the audit trail — regardless of how many
   high-risk decisions the organisation runs.

### Decisions

- Phase B is validated as a pattern, not just as a one-off demo. Two
  instances across two domains, predictable size, reusable wrapper.
  Further AI Act cases are mechanical replication — do them when a
  specific operational need shows up, not to pad the demo count.
- The doc (`ai-act-usage.md`) is the flagship user-facing artefact for
  now. Future sessions should update it as the pattern evolves, not let
  it diverge from what the examples actually do.

### Open for next session

Candidates, roughly ordered by strategic value:

- Phase 7+ design sketch — what `listen_tcp`, `accept_connection`,
  `read_until`, `append_log` would look like in `.verbose` grammar. Not
  implementing, just producing a design doc that makes the north star
  concrete enough to plan against.
- Full-pipeline `make` targets or a `demo/` directory showing end-to-end
  runs of the two AI Act examples side by side, as a reviewable artefact.
- A third Annex III case (insurance scoring, public benefits, or similar)
  — only if there is a concrete reason to add it beyond demo padding.
- Native backend gap closure work (contains / starts_with primitives) —
  useful for broadening SIEM reach but tangential to the AI Act angle
  that is carrying the pitch today.

---

## 2026-04-19 — Phase B kept; Phase 7+ (network-in-.verbose) locked as north star

### Context

While prepping Phase B's SIEM demo, the creator asked to verify whether
Verbose already had a `.verbose` example for the HTTP server — he remembered
having one, and the AI's two previous responses had drifted toward calling
the existing 498-byte HTTP demo "proof that Verbose can describe anything".
Verification revealed the existing `--demo-http` and `--echo-server` binaries
are hand-emitted by Rust code in `src/native.rs` — they have **no `.verbose`
source at all**. The `--http-server <file.verbose>` mode is hybrid: the rule
logic is in `.verbose` and verified, but the network plumbing around it is
hardcoded Rust emitting raw x86-64.

### What this means

The creator's vision is "everything the program does is declared in `.verbose`
and mechanically verified". Network syscalls are the biggest remaining slice
not yet describable. The existing probes prove the **native backend** can
produce tiny network binaries (~500 B) — they don't prove the **language**
can describe them. These are two different claims and the AI had been
conflating them.

### Decisions locked

- **Phase B continues** (SIEM-style demo, SIEM rules compiled through the
  regular tier-1 pipeline). Concrete, already shipping artifacts.
- **Phase 7+ (declarable network primitives) is the north star.** Long-term
  target: collapse tiers 2 and 3 into tier 1 so that `listen_tcp`, `accept`,
  `read_until`, `write_bytes` are declared reactions with their own proofs,
  and HTTP-server-in-.verbose stops being hardcoded Rust. Substantial scope:
  new AST constructs, new verifier rules, new codegen paths, new test coverage.
- **Not attacking Phase 7 now.** Phase B must deliver concrete proof-in-practice
  before investing a month+ into the network-primitive design. Keeping the
  compass visible without letting it divert current work.

### Cleanup performed

Three tiers of native output are now documented canonically in
`docs/known-gaps.md` ("Three tiers of native output"). Every ambiguous mention
across the repo has been updated to point there:

- `src/native.rs`: doc comments on `emit_http_demo`, `compile_echo_server`,
  and `compile_http_server` each state their tier and the scope boundary.
- `src/main.rs`: flag handlers are labeled with their tier in inline comments;
  the usage/help text now names each mode's tier so a first-time reader is
  not misled.
- `CLAUDE.md`: the `--demo-http` example in the "Running" section is tagged
  as a tier-3 probe with a pointer to `known-gaps.md`.
- `README.md`: the Origin section's "498-byte HTTP server" line is qualified
  as a feasibility probe, not a Verbose-described program.
- `Makefile`: the `http:` target prints a banner before running the probe so
  anyone running `make http` sees the scope boundary immediately.

No code was deleted — the probes are real capability proofs and should stay
as they are, just labeled correctly. The goal is future sessions (AI or
human) cannot accidentally read them as "HTTP server described in Verbose".

### Meta note

The drift pattern this exposes: when evidence is described ("498 B HTTP
server") the AI tends to amplify the scope of what the evidence supports.
The 498 B number is real; what it supports is "native emitter capability",
not "language expressiveness". Future responses should separate "the backend
can emit X" from "the `.verbose` language can describe X" — these are
orthogonal claims and conflating them leads to overclaiming the scope Verbose
has reached.

---

## 2026-04-19 — Phase B framing: OS is the supervisor, Verbose ships no multiplexer

### Context

While planning Phase B (SIEM flagship demo), the AI proposed a "supervisor" process that would multiplex an event stream to N rule binaries — framed as a *moyen terme* between monolithic rule binaries and Falco-style "big daemon + rules.yaml". The creator pushed back with five words: *"sécurité first"*. That's the correct rejection and this entry locks the reasoning.

### Why the supervisor proposal was wrong

A Verbose-shipped supervisor would have:
- Sat at the **attack-surface frontier** between event source and verified rules
- Necessarily been non-Verbose code (Rust/C + libc) to handle IPC, process management, routing, buffering, and dynamic rule loading
- Introduced: pipes, queues, signal handling, spawn/exec, queue overflow policies, discovery — each a C-sized attack surface
- Made the verifier's guarantee partial: *"we verify each rule, but the thing orchestrating them is opaque"*

CLAUDE.md already rules this out: *"forcing native to grow to 'completeness' would add a C-sized attack surface and defeat the point"*. The AI drifted because an adoption-friction concern was allowed to soften the security-first principle. Even surfacing the option was the drift — listing compromise options shifts the conversation's center of gravity.

### The correct architecture

Verbose ships **no supervisor**. Each rule is a standalone binary that reads `stdin` line-by-line and writes verdicts on `stdout` — already the shape of `alert.verbose --stream`, Phase 0 of the native backend. Orchestration is **delegated to the operator's audited tooling**:

- `tail -f events.log | tee >(./rule_a.bin) >(./rule_b.bin) >(./rule_c.bin)` for demo
- systemd services with `StandardInput=socket` for prod Unix
- One-container-per-rule DaemonSets for Kubernetes
- Each rule isolated by OS-provided primitives (seccomp, cgroups, namespaces)

Bénéfices : zero new code in Verbose, zero new attack surface at the frontier, each rule independently auditable and independently compromis-able (no shared state), hot-swap via binary replacement (an audit-trail-generating event — feature, not friction).

### Market corollary

The "easy adoption" market (Falco-shaped: one big daemon, YAML rules hot-reloaded) is **not Verbose's market** — confirmed. That market already has good tools. Verbose targets users who value mechanical auditability enough to accept the operational friction of per-rule binaries: compliance-regulated environments, safety-critical detection, 10–50 rules that gate prod decisions rather than 3000 YAML rules consumed at scale. Narrower market, coherent with every other decision in the project.

### Phase B, corrected shape

Exactly as originally planned, stripped of the supervisor speculation:

1. Pick **one** public Sigma rule (the first will be a basic login-failure detector for pedagogical clarity)
2. Write the `.intent` (numbered sentences)
3. Hand-write the `.verbose` (reads, calls, termination bound, overflow if applicable)
4. Compile with `--native --stream`
5. Pipe synthetic events: `cat events.log | ./rule.bin`
6. Compare verdicts against what Sigma's spec says should match

No multiplexer, no orchestration layer, no framework. One binary, one stream, one verdict per input line. If the pitch resonates after one rule, the next rules just reuse the same shape.

### Decisions locked

- Verbose will never ship a process that orchestrates rule binaries. Multiplexing is the OS's job.
- When a future proposal involves "a Verbose process that manages other Verbose processes", refuse it at the pitch stage.
- Adoption friction that can only be reduced by weakening audit guarantees is a friction we keep — it's consistent with Verbose's stated market.

### Meta note for the AI

The drift pattern here (proposing a compromised middle ground to address an adoption concern, rather than first checking whether the principle rules it out) is worth remembering for future exchanges. CLAUDE.md's filter — *"what attack surface does this add?"* — belongs **before** the ergonomics filter, not after. When a stated principle rules out a design, the right move is to skip that design entirely, not list it as an option.

---

## 2026-04-19 — Code read, scope clarification, sanitization plan

### Context

Creator told the AI to stop theorizing and read the code. *"mec. lis le code. et on avise"*. Up to that point the AI had been riffing on `CLAUDE.md` alone. The read surfaced both confirmations and a framing error the AI had been carrying.

### What the code read confirmed

- The verifier is **not ceremonial**. `check_purity` (`src/verifier.rs:755`) does set-diff between declared reads/writes/calls and facts collected from the AST via `collect_logic_facts`. Lying AI gets caught with "missing: [...], extra: [...]" messages.
- `check_termination` (`src/verifier.rs:839`) compares declared `constant_bound` to `count_operations(&rule.logic.value)` — the actual op count of the expression tree. Not a rubber-stamp.
- `compute_range` (`src/verifier.rs:927`) is interval arithmetic for overflow hint verification. This is what lets Verbose skip runtime overflow checks without blind trust.
- `native.rs` at 7440 lines is a real commitment. 33 example `.verbose` files exist; 161 tests including `all_example_verbose_files_parse_and_verify` stop regressions.

### Framing error the AI was carrying

The AI had treated "the verifier doesn't check `.intent → .verbose` semantic fidelity" as a *weakness*. Creator corrected this: that boundary is **deliberate scope, not a gap**.

- The compiler's job is: verify `.verbose` is internally consistent, and emit a binary that matches exactly what the `.verbose` says.
- The `.intent → .verbose` step is the human/AI's responsibility. Asking the compiler to verify English prose against a formal spec would be asking it to solve NLP, which no one can.
- Forcing the compiler into that role would be exactly the "false explicitation" that `CLAUDE.md` rejects — adding declarations that aren't mechanically verifiable.
- The correct pitch: *"write a clear intent; the binary does exactly what the `.verbose` says; the `.verbose` is inspectable."* Net, tenable, defensible.

This preserves the thesis from 2026-04-18: the verifier is a floor with a specific scope, not a pretense of end-to-end guarantee.

### Ceremony vs mechanical power (a useful distinction)

Reading `.verbose` files (`invoices.verbose`, `alert.verbose`, `config.verbose`) surfaced that not all declarations carry equal weight. Two categories:

- **Mechanical** (derivable from the AST; the compiler already computes them in `collect_expr_facts`, then verifies the declaration matches): `reads`, `writes`, `calls`, `constant_bound` with a numeric bound. These exist so a human auditor can read the proof block without parsing the expression. The compiler checks for *consistency*, which is real value, but not *extra information beyond the AST*.
- **Semantic** (carries information the AST alone cannot express): `overflow: [min, max]`, `hints.vectorizable/parallel/cache_result` with justification strings, `@layer`, field ranges like `amount: number [0, 1000000]`, `@intention`. These are where the compiler gains *unique* optimization and audit leverage — overflow bounds skip runtime checks, ranges size buffers at compile time, layers seal call graphs.

Both are useful. Confusing them weakens the pitch. The pitch should talk about mechanical power in terms of the semantic declarations, and frame the mechanical ones as audit scaffolding.

### Known unfinished corners (already acknowledged in code)

- `determinism.form: total` is not transitively checked (`src/verifier.rs:909`, comment: *"For now we trust this — transitive determinism checking is a Phase 2 feature"*).
- `pure_except` does not track whether the listed calls are themselves pure.
- `writes: []` is always empty in the POC grammar — 100% ceremony today.
- WASM backend lags behind native (Phase 0 only).

None are fatal. All weaken the pitch until addressed.

### Sanitization plan (agreed with creator)

Order is deliberate: positioning first (cheap, clears the head), then demo (validates the wedge before more compiler investment), then verifier gap closure (strengthens pitch after direction is confirmed).

**Phase A — Positioning sanitize (2-3 days, docs only)**
1. Document the mechanical/semantic classification of `proofs:` fields in a spec section.
2. Drop `writes:` from the grammar until a write op exists. Pure ceremony today; removal does not lose a single guarantee.
3. Rewrite README top section (~200 words) with explicit scope: *"the verifier proves `.verbose` internal consistency and binary-to-logic equivalence. Semantic fidelity of prose intent to `.verbose` is by design a human/AI concern."*
4. Surface the "floor vs average" thesis publicly in README or ARCHITECTURE, not only in the AI's private memory.

**Phase B — Flagship demo (2-3 weeks, code + writing)**

One real-world wedge, not didactic. Candidate: take **one public Sigma rule** (suspicious authentication pattern), write the `.intent`, produce the `.verbose`, compile to native, stream-match against a real log feed side-by-side with Sigma. Target artifacts: binary < 1 KB, verdicts equal to Sigma, cold start microsecond-scale, audit trail readable by a non-Verbose engineer.

The goal is not to replace Sigma but to have *one* concrete answer to "show me a case where Verbose is the best answer". Without this, the pitch stays abstract.

**Phase C — Verifier gap closure (1-2 weeks, after B lands)**
1. Transitive determinism check (the Phase 2 TODO).
2. Transitive purity check (listed calls carry their own purity obligations).
3. Range propagation through `compute_range` — e.g. `if c.max_conn >= 1 and c.max_conn <= 10000 then Ok(c.max_conn)` should infer `Ok` value range `[1, 10000]`, flowing into native codegen.

### Decisions locked

- The vision is possible. The scope is correct. The framing error ("semantic fidelity is a gap") is retracted.
- No feature work resumes before Phase A is done.
- After Phase A, Phase B (SIEM demo) is the next commit, not more language constructs.
- eBPF-frontend remains a candidate for later; do not expand scope before Phase B validates the basic wedge.

### Open questions (to revisit)

- After Phase A, is there a `--minimal` mode worth exposing that elides mechanical declarations for terser `.verbose` files, with `--show-inferred` to display what the compiler derived? Only worth it if `.verbose` verbosity becomes a real adoption blocker in Phase B.
- The SIEM demo needs a live event source. Use public datasets (e.g. published incident logs, or a synthetic generator) rather than live production feeds.

---

## 2026-04-18 — Thesis reframing + first concrete use-case candidates

### Context

Conversation triggered by "tu penses quoi de ce projet". The project has shipped a lot (native phases 0-6, HTTP server demo, 161 tests) but the creator is in doubt about concrete applications: *"la vision, c'est beau, la réalité, c'est mieux. [...] Sinon tout ce projet, tout ce temps, ne sert à rien"*. The doubt is not technical (the architecture is sound) — it's about whether there's a real-world wedge.

### Thesis reframing — verifier as durable artifact

Initial framing by AI was flawed: "the project lives or dies on generator quality" was called a *risk*. Creator corrected it:

> "vu que cette generation est de plus en plus performante, c'est ... le but"

The reframing is important enough to lock in:

- The verifier is the **stable, durable artifact**. Once a construct is covered, trust in it doesn't depend on who or what produced the `.verbose`.
- The generator rides the AI capability curve **upward**. That's not a risk to mitigate — it's the input the architecture is designed to exploit.
- Most AI-assisted tooling depends on *average* model quality and degrades when models hallucinate. Verbose inverts this: a bad generator produces a rejected `.verbose`, never a wrong binary. Trust is anchored in the **floor**, not the average.
- Consequence: the right roadmap question is never "is the generator good enough?" but "does the verifier/native cover enough surface that good generation is useful?" — a linear engineering question, not a bet.

This reframing also composes with the air-gap property: even without any AI, a hand-written `.verbose` is verified by the same floor. The architecture survives both scenarios (strong AI / no AI), which is rare.

### The real risk is narrative, not technical

Architecture is sound. Discipline is real (zero deps, documented phases, rejection rules applied). The gap is:
- No use case where *nothing else works*. HTTP server in 700 B is cute but nginx exists. The wedge needs to be a scenario where audit line-by-line + tiny surface are **mandatory**, not just nice.
- No 30-second demo that makes the thesis obvious to someone who hasn't read `CLAUDE.md`.

The project doesn't need a better compiler. It needs *the* sentence and *the* use case that make a security engineer say "ah, obviously" instead of "interesting".

### Three use-case candidates (ranked by solo-accessibility)

**1. SIEM / SOC detection rules** — highest fit for where Verbose already is.
- Market: detection-as-code. Incumbents: Sigma (YAML, no verification), Falco (Lua), YARA. All suffer from rule drift, unaudited false positives.
- Verbose edge: each rule is a `.verbose` with declared proofs + a ~700 B streamable binary. `alert.verbose` already delivers 80% of the mechanics.
- Killer demo: take 10 public Sigma rules, rewrite as `.intent`, compile, run on a real feed side-by-side with Sigma. Same verdicts, line-by-line audit possible, microsecond latency.
- Why it's the right first bet: technical self-serve audience, no sales cycle, existing pain, streaming mode already works, regulatory pressure (SOC 2, ISO 27001) makes "auditable detection" a real buyer.

**2. CI/CD policy engine (OPA/Rego alternative)** — natural second step.
- Market: OPA dominates and is widely disliked. Rego is hard to read, cold-start is slow, policies resist non-specialist audit.
- Verbose edge: `config.verbose` already demonstrates validation. A policy "no secrets committed / images from registry X only" becomes 5 rules × 500 B each.
- Killer demo: take 5 public Kyverno or Conftest policies, reimplement as `.intent`, show identical verdicts, 100× faster cold start, readable `.intent` that a PM can review.

**3. EU AI Act high-risk algorithmic decisions** — highest ceiling, longest cycle.
- The Act requires explainability for high-risk algorithmic decisions (credit, insurance, HR). Nobody has a clean answer.
- Verbose edge: `.intent` = the explanation, `.verbose` = the proof, binary = the execution. Three artifacts mechanically linked. That's approximately what regulators are going to demand.
- But: B2B regulated sales, 18-month cycle, needs co-founder or early traction. Not a solo starting point.

### Decision: start with SIEM

Chosen because:
- Technical self-serve audience reachable without sales machine.
- Streaming mode (`alert.verbose`, Phase `--stream`) is already the substrate.
- Existing pain is well-known in the security community; "our Sigma rules have bugs we don't catch" is a real complaint.
- Doesn't preclude #2 or #3 later; in fact #2 follows naturally if #1 establishes the "small verified rule" pattern.

Next concrete step (to be confirmed with creator): pick 5-10 public Sigma rules, prototype a side-by-side demo that shows verdict equivalence + tiny binary + auditable intent. 2-week scope.

### Open questions (to revisit)

- The generator tooling is declared separate from the compiler. For the SIEM demo, how much of the `.intent → .verbose` flow is assumed manual vs. AI-assisted? The demo narrative depends on this.
- WASM is lagging behind native. For SIEM, does this matter? (Probably not — agents are native binaries.) For future web-facing demos, yes.
- The "adoption resistance to config-in-binary" memory note still applies: SIEM might soften this because security people already ship compiled agents (osquery, Falco, Sysmon).

---
