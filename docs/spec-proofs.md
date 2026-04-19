# Proof Grammar Classification

Every declaration in a `.verbose` file falls into one of two categories: **mechanical** or **semantic**. This doc spells out which is which and states the rule that refuses anything that is neither — the discipline that killed `writes:`, the unused proof enum variants, and the single-value `verdict: / form: / determinism:` fields during Phase A sanitize.

## The two categories

**Mechanical** — the value is derivable from the AST alone. The compiler walks the code and produces the same value independently; the declaration exists so a human auditor reads the conclusion without running the walker in their head. The verifier checks **consistency** (declaration ↔ AST-derived fact) and rejects drift.

**Semantic** — the declaration carries information the AST cannot produce. Either a claim the verifier then checks by a stronger method (interval arithmetic, layer discipline, external file existence), or an input that drives codegen and dispatch.

Both categories carry weight. Mechanical declarations are audit scaffolding: they fail loudly when code drifts away from their stated reads/calls, which is the whole point. Semantic declarations are where the compiler gains information the AST didn't already have — optimization, safety claims, architectural discipline.

The thing to refuse is a declaration that is **neither**: the compiler cannot verify it *and* it carries no information the AST lacks. That is false explicitation, and Phase A removed about 845 lines of it.

## Classification

### Purity block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `reads: [...]` | mechanical | Walks the logic AST, collects every field access on input/context, diffs against the declaration. Drift is an error. | `src/verifier.rs:check_purity` |
| `calls: [...]` | mechanical | Walks the AST, collects every rule call, diffs against the declaration. Drift is an error. | `src/verifier.rs:check_purity` |

### Termination block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `bound: N` | semantic | Verifier checks `N ≥ count_operations(logic)`. The claim is the auditor's yardstick: a bound much larger than the actual op count flags estimation error even if mechanically accepted. | `src/verifier.rs:check_termination`, `count_operations` |

### Hints block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `overflow: [min, max]` | semantic | Runs interval arithmetic on the logic, checks `[min, max]` covers the computed range. Verified hints let the native backend skip runtime overflow checks. | `src/verifier.rs:compute_range` |
| `vectorizable: "reason"` | semantic | Verifier enforces "no calls" (independence) + pure logic shape. Native can emit SIMD. The justification string is audit surface — why the AI / human believes SIMD is safe here. | `src/verifier.rs:check_hints` |
| `parallel: "reason"` | semantic | Same pattern: independence claim, justification is audit surface. | `src/verifier.rs:check_hints` |
| `cache_result: "reason"` | semantic | Memoization claim, justification is audit surface. | `src/verifier.rs:check_hints` |

### Traceability

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `@intention: "prose"` | semantic (audit-only) | Not mechanically checked. The auditor compares it against the corresponding line in the `.intent` file. | — |
| `@source: file:line` | mechanical | Opens the referenced file, checks the line exists. Rejects dangling references. | `src/verifier.rs:verify_source_ref` |
| `@layer: domain / application / interface` | semantic | Enforces sealed-subgraph discipline on the call graph (domain → domain only, application → domain+application, interface → anything). | `src/verifier.rs:check_layer_discipline` |

### Concepts and rules

| Declaration | Category | Why |
|---|---|---|
| Field type (`amount: number`, `name: text`, `xs: collection(Item)`, ...) | semantic | Drives type checking and native backend dispatch per type. |
| Field range `[min, max]` / `[..N]` | semantic | Flows into `compute_range` for interval arithmetic, and into native compile-time buffer sizing for text fields. |
| Rule `input: x : Concept` | semantic | Drives type checking and native prologue layout (field-to-slot mapping). |
| Rule `output: name : Type` | semantic | Drives type checking + backend dispatch (the native emitter routes by output type). |
| Rule `context: c : Concept` | semantic | Optional second input read once per program (not per record); sits in different prologue slots than per-record fields. |

### Reactions

| Field | Category | Why |
|---|---|---|
| `trigger: rule_name` | mechanical | Must reference a declared rule. Verifier checks existence. |
| `append_file "literal_path" content` | semantic (discipline) | The path is required to be a string literal at parse time — the parser refuses field references or `concat(...)` expressions. The literal-path rule is itself the semantic claim: the auditor can grep the source and see every file the program could ever touch. |

### Modules

| Field | Category | Why |
|---|---|---|
| `use "path"` | mechanical | Resolution happens at load time; the referenced `.verbose` must exist and parse. |

## What was refused (Phase A sanitize)

Removed in commits `4bb640e`, `8ae62a9`, `94595f3`:

- **`purity.writes: []`** — always empty; POC grammar has no write operations. The verifier rejected non-empty values with "must be empty", a tautology check. Pure ceremony.
- **`purity.verdict`** — only `Pure` was valid. `Impure` (ran a minimal check) and `PureExcept` (ran a real check) were never used in any rule; `Impure`'s real check was also exercised by zero examples.
- **`termination.form`** — only `ConstantBound` was valid. `VariableBound` and `DecreasingRecursion` always errored as "not supported by POC grammar"; `Unproven` was silently accepted and never used.
- **The whole `determinism:` block** — only `form: total` was valid, and the check was a no-op noting "transitive determinism is a Phase 2 feature".

All four fit the same pattern: a declaration with no remaining information the compiler couldn't already see in the AST or the trivially-empty value. Total removal: ~845 lines across the codebase and 36 example files, no compiler guarantee lost.

## Rule for new declarations

Before adding a field to any block, check:

1. **Can the compiler derive the value from the AST?** If yes, the field is mechanical. Add it only if its audit value justifies the visual weight in every rule. Its job is to **fail loudly** when code drifts — never to silently accept non-matching values.

2. **Can the compiler verify a stronger claim than AST derivation?** (Interval arithmetic, layer enforcement, file existence, cross-artifact check.) If yes, the field is semantic. Ensure the value space has more than one meaningful option: a forced-single-value declaration is the precursor to the ceremony we just removed.

3. **If neither applies, do not add the field.** It is ceremony. This is the rule `CLAUDE.md` calls "no false explicitation" — every declaration must serve verification or optimization.

## Scope boundary (by design)

The verifier proves that the `.verbose` is internally consistent and that the emitted binary matches the logic expression. It does **not** verify that the `.verbose` is a faithful translation of its prose `.intent` — that bridge is a human / AI concern, by design. Asking the compiler to verify English prose against a formal spec would require solving NLP, and the declarations the compiler verifies could not stay mechanically-checkable under that demand. See the 2026-04-19 entry in `docs/vision-journal.md` for the thesis: the verifier is the floor that doesn't move; the `.intent → .verbose` translation rides the AI capability curve and is audited by humans reading both files side by side.
