# Bounded errors and failure boundaries

Status: **design proposal for discussion, not implemented syntax or guarantees**.
Written 2026-09-08 against checkout `9864170`, including PR #211's service recovery.
PR #211 remains a scoped implementation; this proposal does not widen its gate.
This document remains a general proposal. Update on 2026-09-09: the first pure
slice is implemented in PR [#212](https://github.com/verbose-org/verbose/pull/212).
Its separate [checked byte lookup contract](try-byte-at.md) fixes the spelling
`try_byte_at`, unit `BoundsError`, alias/branch obligations, and backend refusals.
It does not implement the general boundary, ownership, or effect model below.

## Objective

Make failures as explicit and checkable as inputs, dependencies, and resource
bounds. For each computation, an author and compiler should be able to identify:

- Which recoverable errors can escape, with bounded representations.
- Which runtime failures terminate a declared execution boundary.
- Which resources that boundary owns and how they are relinquished.
- Which results and effects can already be observable when it fails.
- What additional failures, work, and storage handling itself can require.

A finite set of error names alone does not bound the cost or consequences of an
error. A request, file-processing job, batch item, and command invocation should
use the same conceptual contract, with different boundary implementations.

## Original inventory (2026-09-08)

| Mechanism | Observed implementation | Missing general contract |
|---|---|---|
| `Result(T, E)`, `Ok`, `Err`, `match_result` | First-class AST types/expressions. The interpreter evaluates the target before selecting exactly one arm. An evaluation `RuntimeError` escapes before that selection. | Complete propagation/consumption checks and bounded error payloads across supported types/backends. `Ok(f(x))` does not catch failures evaluating `f(x)`. |
| Runtime bounds/parse checks | Native primitives have explicit failure branches. Some handler failures close a client; standalone rule failures may exit. | A source-level, compositional summary of those exits and their boundary. |
| PR #211 | Service record callable failures from emitted `byte_at`/`substring` checks restore the handler frame and close the client. | General nested-call recovery, ownership cleanup, and an error-return ABI. Its numeric-only, effect-free gate remains essential. |
| Effect policies | AST log policy includes `Drop` and `Abort`; resources/connections/entropy have scoped policies and backend implementations. | Uniform classification, partial-effect outcomes, and transitive boundary checks. |
| `abort_if` | Explicit fail-closed streaming-bytes gate, used before self-hosted compiler output. | It must not silently become a catchable domain error. It cannot retract bytes already streamed. |
| Arena scopes | Existing scoped allocation/reclamation mechanisms. | Proof that an escaping error retains no pointer into reclaimed storage, and bounded exceptional cleanup. |

Source anchors: [AST](../src/ast.rs) (`Type::Result`, `Expr::MatchResult`,
`ErrorPolicy`, `Expr::AbortIf`), [interpreter](../src/interpreter.rs)
(`eval_expr`), [native emitter](../src/native.rs) (`ClientAbortScope`,
`emit_service_record_callable`, `emit_parse_int`), and
[optimizer](../src/optimizer.rs) (`optimize_expr`). See also the
[effect model](effect-model.md), [proof limitations](spec-proofs.md), and
[self-hosted Result design](self-hosting-result-tier-design.md). Dated design
notes are not evidence of current backend parity.

## Recommended semantic split

| Class | Example | Proposed treatment |
|---|---|---|
| Invalid program or unsupported lowering | Unknown error type; backend cannot implement the cleanup contract | Compilation refusal with a precise obligation; never a runtime success placeholder. |
| Recoverable outcome | Invalid application input; checked lookup out of bounds; a dependency explicitly allowed to be unavailable | A typed `Err` through `Result`; explicitly consume, map, or propagate it. |
| Controlled boundary failure | A mandatory guard fails before an invalid access; a boundary-owned capacity budget is exhausted | Terminate the declared boundary only if its isolation and cleanup obligations are established. No fabricated `Err` after arbitrary unwinding. |
| Lost execution integrity | Corrupted frame, impossible runtime tag, untrusted allocator metadata | No language-level recovery using that state. Terminate the containing isolation domain. |
| External interruption | Process kill, machine loss, operating-system termination | Outside an in-process cleanup guarantee. A separate supervisor or durable protocol is needed for stronger guarantees. |

Recovery is a property of the operation, execution state, and boundary, not just
an error label. A bounds check that fires *before* an access can preserve integrity;
an already-corrupted stack cannot. Dependency failure is not universally fatal or
universally recoverable. Classify it according to an explicit operation contract.

Two channels remain distinct:

1. **Returned outcomes:** `Result(T, E)` carries a value the caller can inspect.
2. **Terminating failures:** execution stops at an authorized boundary; no normal
   return value is delivered to the abandoned caller.

Do not add a second general-purpose exception value system alongside `Result`.
A checked operation may deliberately expose a failure as `Err` before unsafe work
occurs. Existing aborting operations retain their behavior until explicitly
migrated; wrapping them in `Ok`, a match, or a new annotation cannot change it.

## What must be bounded

| Dimension | Proposed obligation |
|---|---|
| Categories | A closed, finite set of error variants at each declared interface. Error codes are deterministic, never dependent on hash iteration or message text. |
| Payload | A checked maximum encoded size; bounded text/bytes/collections and nesting, or a refusal. No unbounded causal chain or copied request in an error. |
| Propagation | Every call accounts for returned errors and boundary exits, including errors introduced by argument evaluation and handlers. Stack/depth limits remain separate from the finite set of categories. |
| Handling work | Cleanup and fallback have bounded steps and resource needs. Retries need an explicit finite attempt budget and a safe policy for repeated effects. |
| Storage | Space for the error and mandatory cleanup state is reserved before the work that can fail. Construction must not depend on the exhausted arena. |
| Effects | Declare which effects may precede failure and what completion is known. A failed write may have written a prefix; a failed remote operation may have an unknown outcome. |
| Diagnostics | Bounded, explicitly selected context. No automatic payload/secret logging; inability to log must not prevent mandatory cleanup. |

A work bound is not a wall-clock deadline. Blocking I/O and operating-system
scheduling need separate mechanisms. The current structural `termination.bound`
does not establish either total handling work or elapsed time.
Budgets apply to a declared invocation/boundary, not to the entire lifetime of a
server. Handling finitely many error categories does not limit how many requests
can fail over that lifetime.

## Composition and verifier obligations

Maintain a computed summary with at least three separate components:

- Recoverable variants that can escape in a returned `Result`.
- Terminating failures and the boundaries they require.
- Possible effects and owned resources along success and failure paths.

These are semantic analysis components, not proposed source attribute names.
`Result` supplies the returned-error type; do not duplicate it in a decorative
list. A new boundary declaration is justified only where it constrains behavior
that is not already specified by an existing type or effect policy.

For a sequence, account for each reachable operation in evaluation order and
stop at the first failure. Eager lets execute before the body; an unused binding
is not a justification for erasing its possible failure or effect. Branch analysis
unions possible outcomes unless a supported proof eliminates a path. A match
handles the variants it consumes, then contributes the possible errors/exits of
the selected handler. It does not remove a terminating guard failure while
computing its target. Nested calls compose transitively; recursion needs a finite
summary computation plus its own runtime depth/resource obligations.

Under this proposed contract, refuse an interface when:

- A possible escaping variant is outside its declared error type.
- A `Result` can be discarded without explicit disposition. Storage/passing a
  result transfers the obligation; it does not silently discharge it.
- A guard is neither proved unnecessary under enforced premises nor assigned a
  supported failure behavior.
- The destination boundary or required ownership/cleanup cannot be established.
- Error construction or handling lacks the required bounds.
- A backend cannot implement the accepted contract.

Unknown analysis is not evidence of a nonfailing operation. The [strict numeric contract](numeric-overflow.md) now refuses unknown overflow
intervals and uses the full i64 domain for unannotated numeric inputs. These
properties remain scoped; acceptance of a legacy expression must not be reused
to certify stronger obligations. Initial support may refuse
construct combinations rather than claim complete inference.

A fallback converting `Err` to a normal value is allowed only as explicit source
logic. An intentionally ignored optional log error is a disposition with an
observable policy, not evidence the log succeeded. Adding an error variant to a
public interface must force a review of exhaustiveness; avoid a catch-all silently
absorbing newly introduced variants in the initial design.

## Boundaries, ownership, and effects

A boundary must define its success point, failure destination, owned resources,
cleanup order, escaping values, and isolation assumptions. The same notion can
support a CLI invocation, a single file-processing job, or an HTTP request. A
boundary may use memory or state owned by an outer scope, but cannot invalidate
that outer state and still claim the scope can safely resume.

On a controlled failure, the proposed order is:

1. Stop producing the success value. A partially written return record is invalid.
2. Preserve the bounded failure descriptor in storage that outlives cleanup, if
   this boundary's interface reports one.
3. Release owned resources in a compiler-established order. Do not close borrowed
   descriptors or reclaim arenas containing still-live outer values.
4. Reclaim owned transient storage only after needed cleanup data is no longer used.
5. Transfer to the declared boundary outcome, with no execution of abandoned
   success-only code.

No universal rollback is promised. An external write, an emitted response prefix,
or a committed state update remains observable. Atomic updates require a separate
transaction or prepare/commit mechanism; compensation is a new fallible effect,
not an automatic inverse. Automatic retry after an uncertain remote outcome is
outside the first slice.

Mandatory cleanup should initially use compiler-owned actions, not arbitrary
user callbacks. A cleanup failure records a bounded secondary status and escalates
if isolation cannot be maintained; it must neither recurse into an unbounded error
handler nor fabricate successful cleanup. The primary failure is retained where
safe. A process-level failure cannot promise cleanup after external termination.

The initial recoverable capacity case, if later admitted, must be detected before
unsafe allocation/access and must reserve its recovery storage beforehand. Do not
recover from stack overflow using the exhausted stack or allocate an error inside
an arena whose metadata is invalid.

## Example contracts (conceptual, not new Verbose syntax)

**Checked lookup:** input is a bounded byte sequence and an index. The output is a
`Result` whose success payload is a number and whose only failure variant is
`Bounds`. The check occurs before reading. `Bounds` needs no request copy. The
caller explicitly matches it or returns it; an unhandled variant is a refusal.
This example specifies a future checked operation, not today's `byte_at` behavior.

**File-processing job:** a malformed record returns a bounded `InvalidRecord`
variant. The job decides whether to record it and continue. An output-write
failure separately states whether a prefix may have been written. Continuing
must not imply that the failed record was durably written.

**HTTP request:** application validation may return an explicit error response.
A terminating bounds guard may instead close the connection. If a response has
already started, the boundary can close the connection but cannot replace the
previous bytes with a clean error response. The listener resumes only if its own
state and resources remain valid. PR #211 establishes a limited version with no
callee effects and a failure before response/log/after processing.

## Lowering and optimization constraints

Start with explicit typed returns for checked operations, reusing supported
`Result` representations. Do not choose an exact register layout or universal
unwinding scheme before the supported input/output and nesting subset is fixed.
The service-specific stub in #211 is not that ABI: its assumptions exclude nested
calls, effects, and arbitrary resource ownership.

Optimizer equivalence includes selected error, exit boundary, effect ordering,
and output prefixes, not just successful values. No moving a fallible operation
across a branch, swallowing a guard, or retrying an effect without a proof of the
same observable behavior. Define and test evaluation order where current backends
differ before relying on it in a new guarantee.

The interpreter must model the contract explicitly before an emitter implements
it. Its existing Rust `RuntimeError` is a host evaluation failure, not a substitute
for a Verbose typed error or evidence of native recovery semantics. WASM and the
self-hosted compiler may refuse a new subset until they implement its contract;
backend support and semantic version changes must remain visible.
Existing `Result(_, text)` programs do not acquire a finite-category guarantee
from this proposal and are not silently made invalid. Adoption of the stronger
contract needs an explicit, versioned migration path.

## Implementation sequence and acceptance gates

1. **Inventory and semantic agreement.** Catalogue every failure site: trigger,
   pre-access integrity, current backend behavior, effects already possible, and
   owning boundary. Include arithmetic, range enforcement, allocation, parsing,
   I/O, explicit abort, and stream output. Pin differences rather than describing
   all runtime failures as existing checked errors. Agree on the two channels,
   no implicit rollback, and unknown-analysis refusal for new contracts.
2. **Small pure Result slice.** Specify one checked lookup with a finite bounded
   error representation, explicit consumption/propagation, and no I/O or cleanup
   callbacks. Select the smallest supported type/ABI subset. Add negative cases
   for dropped/unhandled results and unknown obligations. Define interpretation
   first, then native lowering, with existing behavior opt-in and unchanged.
3. **Composition.** Add nested calls, branch/let positions, bounded payload
   lifetimes, and failure-summary propagation. Refuse unsupported recursion and
   representation combinations until their obligations are implemented.
4. **Controlled boundaries.** Generalize isolated recovery only with proven frame
   restoration and compiler-owned cleanup. Demonstrate both a nonserver job and
   a service request; keep explicit process abort distinct.
5. **Effects and capacity.** Extend to resource ownership, partial I/O, cleanup
   failure, and pre-reserved recovery storage. Retries, compensation, deadlines,
   and transactional state are independent later designs.
6. **Backend and self-hosting coverage.** Publish a supported-contract matrix and
   differential tests as coverage lands. Preserve fixed-point checks when changing
   the self-hosted compiler; do not claim parity from a successful self-build.

Each implementation slice needs success/error twins, errors in both branch arms,
eager lets and argument evaluation, handler errors, and unsupported-shape refusals.
Compare interpreter/native outcomes, selected variants, boundary destination,
output/effect traces, and resource/state outcomes. Test the minimal and maximal
payloads, recovery without further allocation, and failures during cleanup when
those features enter scope. Repeated requests alone cannot demonstrate correct
nonserver semantics or full resource cleanup. Unaffected examples must retain
behavior; binary identity is an additional scoped regression check.

## Original review questions and subsequent decision

The original recommendations were:

- Reuse `Result`; no implicit catch of runtime traps or `abort_if`.
- A finite error type with bounded payloads; no free-form text as the only category.
- Refuse unproved new failure obligations; do not inherit existing permissive hints.
- Typed checked operations first; general nested recovery and effects later.
- No implicit rollback, retry, or user-defined cleanup callbacks in the first slice.

PR #212 settles the first operation's representation, spelling, alias obligations,
and evaluation order; aggregates remain explicitly unsupported in that slice.
General boundary syntax, bounded payload lifetimes, and effect recovery are still
open. Do not add parser attributes until their mechanical obligations and refusal
cases are written. This proposal does not make those broader guarantees available.
