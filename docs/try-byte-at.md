# Checked literal byte lookup

Implemented contract, 2026-09-08. This slice is independent of the service
recovery work in #211. It does not provide general exception recovery.

```verbose
out = match_result(
  try_byte_at(b"abc", i.index),
  value => value,
  error => 0
)
```

`try_byte_at(table, index)` returns `Result(number, BoundsError)`. The table must
be a literal `bytes` or `text` expression. Text is indexed by UTF-8 **byte**,
exactly like `byte_at`; an index can select a continuation byte. The index is
an `i64`, evaluated once. An index in `[0, byte_length)` produces `Ok(byte)`
with a number from 0 through 255. Every other index produces `Err` carrying
the sole `BoundsError` value, including all indices into an empty table.

`BoundsError` is a reserved built-in type with no payload, public constructor,
or implicit conversion to number or text. This slice allows it only as the
error type of a rule's `Result(number, BoundsError)` output. A match error
binder can propagate it with `Err(error)`; a handler can explicitly choose a
fallback. Existing `Result(_, text)`, `byte_at` and `abort_if` retain their
contracts. An internal failure while evaluating the index escapes normally:
`try_byte_at` does not catch it or turn it into `BoundsError`.

## Obligations and supported composition

Each produced result must be returned or consumed by `match_result` on every
analyzed path where it exists. An alias refers to the same obligation. Renaming,
copying, shadowing or abandoning a result does not discharge it. Matching either
alias discharges that result's obligation; matching it twice is permitted.
Matching a result requires both branches to produce the same type and binds the
success variable as `number` and the error variable as `BoundsError`.

The initial subset consists of acyclic pure rules taking the same named concept
whose fields are all numeric. Calls must have the form `callee(input)`; caller
and callee may name their input variables differently. Numeric and boolean
expressions, conditionals, eager lets, aliases, nested matches and direct or
matched propagation are supported. Rule outputs are `number`, `bool` or
`Result(number, BoundsError)`. Literal `byte_at` is also available inside an
index calculation, retaining its terminating failure behavior.

The contract is checked across callers and their dependencies. Aggregate or
collection storage, nested results, recursion, context inputs, effects and
service contexts are refused. Other expression forms receive an explicit
unsupported-analysis diagnostic. The analysis examines both branches without
assuming correlations between separate conditions; it may conservatively refuse
programs requiring such a proof. It refuses after 100,000 analysis steps rather
than accepting an unknown obligation. These restrictions do not migrate the
old `Result(_, text)` contract.

## Backend support

| Path | Support |
|---|---|
| Interpreter | Typed result and separate internal evaluation errors |
| Native Linux x86-64, argv records | Supported subset above |
| Native stdin, raw stdin, stream | Explicit refusal in this slice |
| WASM | Explicit refusal before writing a module |
| Self-hosted `elf_program_src` / `x86_program_src` | Reserved-identifier refusal before emitting bytes |

The native value consists of two 64-bit words: tag 0 plus a numeric payload for
success, or tag 1 plus zero for error. Lexical bindings and temporaries use
frame slots. Acyclic calls expand into the entry frame with fresh lexical
bindings; no general Result calling convention is introduced. Expansion is
limited to 100,000 nodes. The unsigned bounds comparison precedes the byte load,
and the error branch performs no allocation or table access.

At native entry, `Ok(n)` prints `n` and a newline to stdout. `Err` prints
`Bounds` and a newline to stderr and sets the sticky exit status to 1; subsequent
argv records still run. This matches the existing native Result output policy.
The interpreter CLI retains its existing labeled value display (`Ok(n)` or
`Err(Bounds)`); differential tests compare interpreter values with the native
stdout, stderr and exit-status policy, not identical CLI presentation.

Optimization currently preserves participating rule bodies intact. This keeps
eager evaluation and explicit handling intact, including for constant invalid
indices, which remain runtime errors returned as values. It also prevents hints
from changing this subset's accepted inputs or observable behavior.

See [the executable examples](../examples/try_byte_at.verbose) and
[obligation/differential tests](../src/bounds.rs). The compiler and emitter remain
trusted: the checks do not constitute an independent proof of emitted code.
