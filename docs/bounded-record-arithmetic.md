# Proved arithmetic in bounded record composition

Design recorded before implementation. The bounded text/flat-record subset and
source pipelines can already carry numeric fields, compare them and format them.
This slice lets those same compositions calculate numeric values, with every
intermediate checked before native emission. An invoice total, normalized reading
or compiler offset can be prepared once, passed in a checked record and rendered
without a separate runtime or aggregate allocation.

## Contract

The existing opt-ins remain sufficient: a bounded `text [..N]` result activates
its checked call graph, and `mode: pipeline` activates its declared composition.
There is no new hint or source proof fabricated by the compiler. Supported
numeric expressions gain `+`, `-`, `*`, `/`, `%`, unary minus, `abs`, `min` and
`max`. Each operand must be a number. These operations compose with the existing
lets, flat records, branches, `length`, concat and acyclic calls.

Every intermediate must fit signed i64. Division and remainder must exclude a
zero divisor and `MIN / -1` or `MIN % -1`; division truncates toward zero and
remainder has the dividend's sign. Negation and `abs` must exclude MIN. Conditions,
eager unused lets, both arms of an `if` and both boolean operands are checked,
even when another part of the expression fixes the final value. Unsafe or
unknown analysis refuses before optimization or artifact creation.

Input fields start with their enforced declared intervals, or the complete i64
domain. Numeric facts use one closed interval in this composition subset; `if`
joins both results and does not refine fields from its condition. There is no
new relation solver or repeated-value identity analysis. In particular, a guard
`x != 0` alone does not make division by an otherwise unrestricted x provable
here. Authors can declare a suitable input domain or clamp values explicitly
with `min`/`max`. The richer scalar `hints.overflow` analysis keeps its own scope
and is not combined with record/text composition in this slice.

The binary interval calculation reuses the existing strict numeric arithmetic
helper, using i128 endpoints before checking i64 representability. Multiplication
and division consider all four endpoint pairs; division first excludes its
exceptional inputs. Unary and min/max bounds use their monotone/extremal bounds.
Facts follow aliases, shadowing, constructed fields and checked returns. The
existing call-input check then proves they fit each consumer's public domain.
Every callee is checked against its declared input, not a convenient call site.
This does not add field-range enforcement to unrelated legacy record outputs.

Examples: quantities in [0,1000] and unit prices in [0,1000000] produce a total
in [0,1000000000]. Passing that total to a consumer limited to [0,999999999]
refuses. Multiplying two unbounded numbers refuses even if the result is later
clamped. Clamping the inputs first can establish a safe product.

## Interpreter and native emission

The existing interpreter is the original-AST behavioral reference. Its numeric
operations already have these semantics for the proved input domains. Existing
bounded input checks and pipeline phase input checks remain the premises of the
proof; no unsafe program reaches ordinary arithmetic evaluation through the
verified CLI. No interpreter arithmetic rewrite is required.

Native emission evaluates operands left to right into invocation-owned scalar
slots, retaining the left value across evaluation of the right. It loads them
into rax/rcx, emits add/sub/imul or signed cqo/idiv, and stores rax (or rdx for
remainder) as one new scalar value. `abs` uses a negated rcx candidate and a
signed conditional move; `min`/`max` use signed compare/conditional move. Unary
minus keeps the existing neg emission, now admitted for proved nonliteral
values as well. No runtime overflow check is needed after this proof, and no
legacy constant-division shortcut is used.

These operations do not adjust rsp or introduce a call/allocator. Existing word
last-use placement accounts for their operands/results and preserves live text
owners. The enclosing stack report and budget use that same final layout.
`concat` keeps its conservative 20-byte capacity per formatted number. Old
accepted expressions retain their emitter paths and bytes. The source optimizer
continues to preserve checked text and pipeline rules, so it cannot erase a
failed obligation in an unused let or untaken branch.

## Scope and validation

This is reusable bounded composition: ordinary checked rules, source pipelines,
and existing bounded HTTP handler fragments use the same arithmetic analysis
and emitter. It adds no HTTP protocol/lifecycle or whole-service memory contract.
Existing log-expression restrictions, context/effect/recursion restrictions,
pipeline transport refusals and workload restrictions remain in force. WASM and
self-hosted output continue to refuse bounded-text/pipeline declarations. The
self-hosted compiler source need not change.

Validate before delivery with original-AST/native differentials over signed
operands, i64 boundaries, division/remainder signs, min/max/abs, nested calls,
aliases, shadowing, arithmetic inside record fields and formatting, final scalar
and boolean consumers, and borrowed text surviving numeric work. Check unsafe
intermediates, zero/MIN exceptional cases, type mistakes, conservative branch
refusals, transfer ranges and unselected obligations without overwriting an
artifact. Exercise one pure HTTP handler through its existing transport path.
Independently check emitted stack depth, exact/one-byte-small budgets and
deterministic output. Run the serialized Rust suite, CLI suites, CIDX and CI
bootstrap; compare all pre-existing examples against the reference compiler.

Performance measurements and clock calibration remain deferred. Static storage
and arithmetic safety checks are the claims of this slice, not a measured gain.
