# Vision Journal

Chronological record of strategic thinking about what Verbose is for, who it's for, and why it might matter. Different from `design-lessons.md` (technical scars) and `known-gaps.md` (to-do): this is about *positioning, thesis, use cases, doubts*. Most recent entries on top.

Entries are shared authorship — the human leads, the AI documents what was said and thought, both push back. The point is to keep the trail readable months later, including the wrong turns.

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
