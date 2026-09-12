# Proof Grammar Classification

Every declaration in a `.verbose` file falls into one of two categories: **mechanical** or **semantic**. This doc spells out which is which and states the rule that refuses anything that is neither — the discipline that killed `writes:`, the unused proof enum variants, and the single-value `verdict: / form: / determinism:` fields during Phase A sanitize.

## The two categories

**Mechanical** — the value is derivable from the AST alone. The compiler walks the code and produces the same value independently; the declaration exists so a human auditor reads the conclusion without running the walker in their head. The verifier checks **consistency** (declaration ↔ AST-derived fact) and rejects drift.

**Semantic** — the declaration carries information the AST cannot produce. Either a claim the verifier then checks by a stronger method (interval arithmetic, layer discipline, external file existence), or an input that drives codegen and dispatch.

Both categories carry weight. Mechanical declarations are audit scaffolding: they fail loudly when code drifts away from their stated reads/calls, which is the whole point. Semantic declarations are where the compiler gains information the AST didn't already have — optimization, safety claims, architectural discipline.

The thing to refuse is a declaration that is **neither**: the compiler cannot verify it *and* it carries no information the AST lacks. That is false explicitation, and Phase A removed about 845 lines of it.

## Classification

### Text output capacity

`output: out : text [..N]` is a checked semantic claim about the maximum byte
length of a returned value. In the supported pure acyclic subset, the verifier
adds concat capacities, joins branches and checks callee contracts with lexical
binding types. Both an excessive bound and an unknown analysis cause refusal.
The claim depends on enforced input bounds and trusted compiler correctness;
it does not establish allocation ownership, temporary memory or total RSS.
See [the contract and support matrix](bounded-text-output.md).

### Purity block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `reads: [...]` | mechanical | Collects input/context accesses and supported resource, connection, and entropy dependencies from the AST, then compares them with the declaration. Drift is an error. | `src/verifier.rs:check_purity` |
| `calls: [...]` | mechanical | Walks the AST, collects every rule call, diffs against the declaration. Drift is an error. | `src/verifier.rs:check_purity` |

### Termination block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `bound: N` | semantic | Checks `N ≥ count_operations(logic)`, a structural AST count. Calls and reductions do not expand into their total runtime work. Recursion declarations are checked separately. | `src/verifier.rs:check_termination`, `count_operations` |

### Hints block

| Field | Category | What the compiler does | Source |
|---|---|---|---|
| `overflow: [min, max]` | semantic | Checks `[min, max]` covers the interval when analysis can compute it. An unknown interval is currently accepted without establishing the hint; see the limitations below. | `src/verifier.rs:compute_range` |
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

### Rule-call arguments

The Rust verifier compares known argument types with the called rule's declared
input. Both branches of conditional arguments are checked, as are their boolean
conditions when local binder types are not needed. Known constructors
are checked for their required fields and field types. A separate traversal reaches
calls in let RHSes, reduction bodies, and match scrutinees as well as the final
expression. Top-level aliases are resolved
in source order. A call using a locally bound lambda/match variable whose type
is not tracked remains unchecked by this comparison; it must not inherit the
type of a shadowed outer variable. Arity and callee existence have separate checks.

## Current interpretation and limitations

This classification describes the Rust-written verifier. The self-hosted compiler
has its own coverage; see [current status](current-status.md) and the
[self-hosting journal](self-hosting.md).

`termination.bound` is a structural metric. A fold contributes its collection,
initial value, and body expression counts once; a call contributes its arguments
and one call node. It is not a bound on total instructions, iterations, elapsed
time, or the lifetime of a service. The `structural`, `decreasing`, and `increasing`
declarations are separate recursion checks.

Overflow analysis can return no interval, including for unsupported expression
shapes and arithmetic it cannot bound. `check_hints` currently accepts that case;
an accepted hint is therefore not necessarily an established range. The global
`all proofs check out` message must be read within this limitation.
For numeric input fields without a declared range, this pass currently assumes
`[0, 2147483647]`; it does not establish that arbitrary signed input satisfies
that assumption. Explicit field ranges remain premises of the interval analysis.

A **signed-modulo counterexample was reproduced on 2026-09-05**: with
`value : number [-20, 20]`, a rule returning `value % 10` was accepted with
`overflow: [0, 9]`, but returned `-1` for input `-1` in both the interpreter and
the native binary. This defect is now fixed in the Rust verifier: signed remainder
bounds follow the dividend's sign and use the largest absolute divisor endpoint.
The example computes `[-9, 9]`, rejecting `[0, 9]` and accepting `[-9, 9]`.
Regression tests exhaust small intervals and cover 64-bit extremes. Divisor ranges
containing zero or a possible `i64::MIN % -1` return an unknown interval; negation
that cannot fit in an `i64` does too. The unknown-interval limitation above still
applies. The self-hosted verifier is unchanged.

`@intention` and hint justification strings carry human-readable meaning. The
compiler does not prove that those explanations are true. Likewise, a literal
path makes a file operation visible in source; it does not establish that the
operation was appropriate for the user's intention.

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

The verifier checks the supported source-level obligations. Correct optimization and lowering remain part of the trusted implementation; the verifier does not independently prove that the emitted binary matches the logic expression. It does **not** verify that the `.verbose` is a faithful translation of its prose `.intent` — that bridge is a human / AI concern, by design. Asking the compiler to verify English prose against a formal spec would require solving NLP, and the declarations the compiler verifies could not stay mechanically-checkable under that demand. See the 2026-04-19 entry in `docs/vision-journal.md` for the thesis: the verifier is the floor that doesn't move; the `.intent → .verbose` translation rides the AI capability curve and is audited by humans reading both files side by side.

## Bounded-result obligations

The [checked literal lookup contract](try-byte-at.md) tracks each new bounded
result through lexical aliases and branches. A result must be returned or matched
on every analyzed path. Unknown forms and excessive analysis expansion are
refused. This check is limited to the documented pure acyclic subset; it does
not strengthen acceptance of legacy `Result(_, text)` declarations.


### Bounded HTTP transport declarations (2026-09-09)

`service.request_timeout` and `service.response_timeout` are semantic inputs:
paired integer seconds in 1..3600 select complete bounded HTTP request assembly
and complete response sends. The verifier rejects other protocols and
`max_request` outside 1..1048576. Native emission uses absolute monotonic phase
deadlines; the attributes do not prove handler CPU time, resource/log latency,
response-size bounds, or total connection count. See [the contract](http-bounded-io.md).

`service.max_connections` is an optional admission bound for forked HTTP with
both socket deadlines. Verification checks its range and supported context;
native admission counts children until their exits are reaped and closes new
clients at capacity. It bounds admitted handler children, with one temporary
admission socket in the parent. It does not establish handler lifetime, fairness,
shared-state synchronization, or a bound on the kernel backlog. See the
[admission contract](bounded-service-concurrency.md).

`service.workers` together with `concurrency: pooled` fixes the number of isolated
HTTP worker processes (1..64). Both phase deadlines are required. The verifier
refuses `after:` mutations and the incompatible `max_connections` admission
policy. Each worker restores its request stack before accepting again; its fixed
frame persists. This establishes neither a whole-service memory quota nor a
queue-wait deadline. See the [pool contract](pooled-http-workers.md) and the
[memory design criterion](../ARCHITECTURE.md#memory-as-a-language-design-criterion).

`service.shutdown_timeout` is an optional SIGTERM grace period (1..3600 seconds)
for bounded HTTP pools. Verification checks the declaration's context and range;
native code disables the listener, waits for accepted work, then requests forced
termination at an absolute monotonic deadline. It is an enforced runtime policy,
not a proof of handler completion, effect rollback, client receipt, or OS scheduling
latency. See the [shutdown contract](http-pool-shutdown.md).
