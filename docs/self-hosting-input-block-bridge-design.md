# The input:-block bridge — real self-source rule shapes execute and compile

## The gap (grounded)
A rule declared the REAL way (`input:\n    s : St` block, no toy `rule go(s : St)`
params) evaluates to 0 in the interpreter and traps (int3) when compiled. Root
cause is single: `bind_params` (eval) and `param_index`/`x86_proc` (codegen) read
`rd_params` — the TOY param list — which is empty for input:-block rules, so the
input name never binds. The checkers (R6d purity, termination) already read
`rd_input`; the executors never got the bridge. Until now every milestone program
used the toy form; the real self-source uses input: blocks exclusively — this
bridge is the gate to compiling actual self-source rules.

## Design — one accessor, N call sites
New `rule_params_of(rd) -> ParamList`: if `rd_params(rd)` is non-empty → it;
else convert `rd_input(rd)`'s FieldList to a ParamList — new `fields_to_params`
(the inverse of R4's `params_to_fields`; preserve name spans AND ty spans — the
ty span is what slice 4's static_concept_of resolves param concepts from).
Replace `rd_params(...)` at the EXECUTOR call sites: eval's `eval_call`
(bind_params params:), codegen's `x86_proc`/`proc_size`/`x86_node`'s
ByteGenState.params sources (wherever ProcGenState/ByteGenState get params from
the rule). Checker call sites (purity/termination) already handle input directly —
leave them. Toy-form rules: `rule_params_of` returns rd_params unchanged →
byte-identical behavior (the fallback only fires when params is PNil and input is
non-empty).

## Gate (CLEAN disk; every compiled ELF == --run eval_main)
1. vexprparse verifies; suite green (currently 430 + 1 ignored) + a new test.
2. **Bridge basics**: the grounded repro — `go` with `input: s : St`, recursion
   `go(St { n: s.n - 1 })`, full proofs — eval → **15** AND compiled → **15**
   (was 0 / SIGTRAP).
3. **MILESTONE — a rule written EXACTLY as the self-source writes it**: word_length
   in full real shape — `input:\n    s : ScanState` (ScanState: source : text,
   pos : number), `output:`, `logic:` with byte_at/length on s.source, `proofs:`
   purity reads/calls + `termination: bound + increasing : pos` — driven by a toy
   `main` that calls it with a literal-source record. Eval → **5** AND compiled →
   **5**. This composes the bridge + records + text + termination-verified proofs:
   the first REAL self-source rule shape through the whole self-hosted pipeline.
4. Toy-form programs BYTE-IDENTICAL (all existing milestones: scanner 5,
   list-sum 6/15, records 49/15/7, scalar 5 — cmp vs a pre-bridge build).
5. Regression test: the bridge repro (eval+compiled 15) + real-shape word_length
   (eval+compiled 5) + one byte-identity assertion.

## Honest scope
One accessor + one inverse converter + call-site swaps. NOT in scope: mixed
params+input declarations (self-source never mixes; if both present, params wins —
note it), multi-field input blocks BEYOND what already works through the
single-record-param ABI (the self-source's rules take ONE record input — that's
the shape), output:-type enforcement in codegen (the checker owns types).
After this bridge, "compile a real self-source rule" stops being a rewrite
exercise — the actual text of simple self-source rules becomes compilable input.
