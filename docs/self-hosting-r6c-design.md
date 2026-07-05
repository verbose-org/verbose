# R6c — the interpreter evaluates match/variant (runtime Value model)

## The crux (why R6c is the biggest R6 brick)

`eval_ast_env : number`, `Env.ECons.val : number`, and the call machinery
(`eval_call`/`bind_params`/`build_env`/`eval_rule_decl`) are ALL number-valued.
The interpreter does calls + recursion. To evaluate `AstVariant`/`AstMatch` (today
stubbed to 0) it needs a runtime value that is a number OR a variant (tag +
payload). Since eval has ONE return type, making it return that Value pervades the
WHOLE eval subsystem — this is atomic and the largest brick of the grammar arc.

Payoff: the self-hosted interpreter can `--run` its own match/variant rules — the
compiler executes the constructs it is written in.

## Design

### 1. Value model (new concept_group)
- `Value`: `VNum of (n : number)` | `VData of (tag : number, payload : ValueList)`.
- `ValueList`: `VLCons of (head : Value, tail : ValueList)` | `VLNil`.
- **tag** = a variant identity self-computed from R4's ConceptList (no global table):
  `tag = concept_index * 256 + variant_index` (256 > max variants/concept). Helper
  `variant_tag(concepts, src, concept_span, variant_span)` (reuse R6b's
  find_concept_index + a variant-index lookup in that concept's VariantList).
  At a match, the scrutinee VData carries the tag; extract `concept_index = tag /
  256`, resolve each arm's variant name → arm_tag, compare.

### 2. eval_ast_env : Value (rewrite every arm; thread the ConceptList)
`EvalEnvArg` gains `concepts : ConceptList` (for tags). Arms:
- `AstNum(v)` → `VNum{v}`; `AstBool(v)` → `VNum{0|1}`; `AstStr` → `VNum{0}` (strings
  aren't evaluated in the number interpreter — keep as VNum 0, note it).
- `AstBin/AstNeg/AstNot` → eval operands, take `.n` of each `VNum` (well-typed by
  R6b; non-VNum defensively → 0), compute, wrap `VNum`.
- `AstVar` → env lookup (env now holds Value).
- `AstIf` → eval cond, branch on its `.n`.
- `AstCall` → `eval_call` (now Value-valued).
- `AstVariant(cstart,clen,vstart,vlen,fields)` → `VData{ tag: variant_tag(...),
  payload: eval each field value → ValueList }`.
- `AstMatch(scrut, arms)` → eval scrut → VData; find the arm whose variant_tag ==
  VData.tag; bind its binders POSITIONALLY to VData.payload (extend env with Values);
  eval that arm body. (No match → a defensive VNum 0; R6b guarantees exhaustive-ish
  well-typed input, but be total.)
- `AstErr` → `VNum{0}`.

### 3. Env + call machinery : Value
- `Env.ECons.val : number → Value`. `build_env`/`bind_params` bind Values (params
  from arg Values; lets from eval'd Values). `eval_call` evals args → Values, finds
  the rule, binds params, evals the body → Value. `eval_rule_decl` → Value.

### 4. eval_ast → thin wrapper (kill the duplicate)
`eval_ast` (the no-env/no-calls number evaluator) currently duplicates the arithmetic
arms AND needs AstVariant/AstMatch arms too (exhaustive match). Replace its body with
`vnum_of(eval_ast_env(node, ENil, src, RNil, concepts))` — unwrap the VNum. Removes
the duplicate arm set; keeps `eval_ast : number`. (Equivalent on toy inputs: ENil env
+ RNil prog handle arithmetic identically. Note the change.)

### 5. Drivers unwrap
`eval_main` and any `eval_ast_env` caller expecting a number: unwrap the final Value
via `vnum_of(Value) -> number` (VNum → n; VData → 0 or its tag, pick + document).

## Gate (R6c)
1. vexprparse verifies; suite green (currently 419 + 1 ignored) + new R6c test;
   existing SCALAR eval tests UNCHANGED (eval_main on `1+2`, `if`, recursion over
   numbers still give identical numbers — the wrapper + Value-of-number path must be
   behavior-identical).
2. **MILESTONE** (eval a real match/variant program via eval_main, source via argv):
   - variant + match, no recursion: build `Cons(5, Nil)` and `match it: Cons(h,t) =>
     h  Nil => 0` → **5** (constructs a VData, dispatches, binds h, returns it).
   - RECURSIVE match/variant (the real demonstration): a list-sum — build a small
     list via variant construction across a recursive rule, sum it via a recursive
     match rule → the correct total (e.g. sum of [3,2,1] → 6). This proves Values
     flow through calls + recursion + payload binding.
   - a number-only program (e.g. recursive factorial-ish) → unchanged result.
3. Regression test (src/native.rs) pinning: the non-recursive match → 5, the
   recursive list-sum → its total, and a scalar program unchanged.

## Honest scope
R6c is atomic + the biggest brick (Value pervades eval + Env + calls). No smaller
cut — the value type is one return type across the mutually-recursive eval subsystem.
Deferred: text/string values (VNum 0 placeholder — the number interpreter never
really evaluated strings). After R6c the self-hosted interpreter RUNS match/variant;
R6d (proofs verifier) and R7 (self-hosted codegen — the green-field giant) remain.
