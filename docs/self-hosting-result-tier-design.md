# Result tier in the self-hosted compiler — Ok / Err / match_result

## Goal
Let vexprparse compile programs that use `Result(T, E)` — `Ok(e)`, `Err(e)`, and
`match_result(scrut, v => ok_body, e => err_body)`. vexprparse's OWN source uses
zero Result, so this is coverage-broadening (toward a general compiler), not a
self-compile unblock. The oracle is verbosec (`--run` and `--native`).

## Representation — first-class, reusing VData + variant/match codegen
verbosec models Result first-class (`Value::Ok`/`Value::Err`), and the
self-hosted `match` is a keyword while Ok/Err/match_result are not — so this is a
real new tier, not a cheap desugar. A sentinel-desugar (Ok/Err → `AstVariant`
with a fake concept) was rejected: it needs `program_uses_arena` /
`max_payload_fields` to detect Result usage via a rule-walk for arena sizing,
which is fragile and risks byte-identity for existing examples.

Instead, add three first-class AST nodes (mirroring how R3 added AstVariant /
AstMatch), but REUSE the runtime machinery already built for variants:
- `AstOk of (inner : Ast)`
- `AstResErr of (inner : Ast)`  (NOT `AstErr` — that name is the existing error sentinel)
- `AstMatchResult of (scrut : Ast, ok_bstart : number, ok_blen : number, ok_body : Ast, err_bstart : number, err_blen : number, err_body : Ast)`

Value model: NO new Value variant. `Ok(x)` evaluates to `VData(RESULT_OK_TAG, [x])`,
`Err(x)` to `VData(RESULT_ERR_TAG, [x])`, where RESULT_OK_TAG / RESULT_ERR_TAG are
two fixed reserved tags high enough never to collide with a real concept's
`cidx*256+vidx` (e.g. 60000*256 and 60000*256+1 — no program has 60000 concepts).
`match_result` evaluates the scrutinee to a VData, dispatches on the tag, binds
the single payload to the arm binder, evaluates the arm. This REUSES the VData /
payload / binder machinery `eval_match` already has.

Since every self-hosted value is one i64 (number, packed text span, arena index),
a Result payload is one slot regardless of T/E — so one shape serves
Result(number,text), Result(text,text), etc.

## Slices
- **Slice 1 (this) — parse + eval (the oracle).** Add the 3 AST nodes; parse
  `Ok(e)`/`Err(e)` (callee-name recognition in parse_primary, 1 arg) and
  `match_result(scrut, v => ok, e => err)` (custom parse: match_result is NOT a
  normal call — its arms are `binder => body` lambdas the arg parser can't take,
  so intercept the "match_result" ident in parse_primary and parse scrut + two
  lambda arms explicitly, mirroring parse_match). Eval them via VData as above.
  Add STUB arms (int3 / return 0 / recurse) in every other Ast matcher
  (shape_ast, x86_node, x86_stream_node, code_size_node, code_size_stream_node,
  the lint/analysis walks, static_concept_of) — codegen is slice 2. Milestone:
  `--run` a match_result program → the SAME number as verbosec `--run`.
- **Slice 2 — codegen.** Emit `AstOk`/`AstResErr` as a 2-slot arena node (tag +
  1 payload) — reuse the variant-construct emit shape (r15/r14 bump, write tag,
  pop payload, push index). Emit `AstMatchResult` as a 2-way tag dispatch —
  reuse the match dispatch/bind shape. `program_uses_arena` / `max_payload_fields`
  learn AstOk/AstResErr (walk the AST for them, exactly as they walk for
  AstVariant — the AST node IS the usage signal, no separate detection). Milestone:
  a COMPILED match_result program runs == `--run` == verbosec.
- **Slice 3 (later, optional) — top-level Result ABI.** A rule whose OUTPUT is
  `Result(T,E)`: Ok→stdout, Err→stderr+exit1 (verbosec's native ABI). Deferred;
  slices 1-2 cover Result as an internal value (match_result-consumed), the
  common composition pattern (e.g. purchase.verbose::discounted_purchase).

## Gate (each slice, CLEAN disk)
1. `cargo run -- examples/vexprparse.verbose` → all proofs check out; suite green.
2. Existing examples BYTE-IDENTICAL (additive — Result nodes only affect programs
   that use them; the two-generation fixed point must still hold: gen1==gen2).
3. Slice-specific milestone above, oracle = verbosec on the same program.

## Honest scope
First-class nodes reused onto the variant runtime. Slice 1 is parse+eval only
(no compiled Result yet). The top-level Ok→stdout/Err→stderr ABI is slice 3,
deferred. No change to vexprparse's self-compile (it uses no Result) — the
two-generation fixed point stays the invariant.
