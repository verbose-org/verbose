# Recursive types — design doc

**Status:** design phase — no implementation yet.
**Motivation:** self-hosted Verbose compiler (lexer in Verbose → parser in Verbose → verifier in Verbose → emitter in Verbose). The parser needs an AST; an AST is inherently a recursive tree.
**Filter:** every choice in this doc must respect the five pillars (verifiability, exploitability, safety, traceability, readability) AND the compiler axiom ("controls and applies, never guesses"). If a design point requires the verifier to infer something it can't mechanically prove, it's refused.

This document is not a slice plan. It's an exploration of the design space, with the constraint that the answer must be expressible in Verbose-the-language as a *deliberate, declared* feature — not as a workaround.

**Revision history:** v0 (initial) passed through fresh-context subagent review; v1 (this version) adopts the subagent's three counter-proposals (max_nodes alongside max_depth, `concept_group` for mutually recursive types, `termination: structural` shape) and adds six previously-missing open questions surfaced by the review.

---

## 1. The problem

A compiler's AST node is recursive:

```
type Stmt = If(Expr, Stmt, Stmt) | Return(Expr) | Block(list<Stmt>) | ...
type Expr = Binary(Op, Expr, Expr) | Call(name, list<Expr>) | Int(number) | Var(name) | ...
```

A `Stmt` contains `Expr`s, which contain `Expr`s, which may eventually contain `Stmt`s (inside `Block`). The structure is mutually recursive AND sum-typed, with arbitrary nesting depth.

Verbose today has none of:
- Recursive type references (a `Record` can't contain itself or a mutually-recursive other `Record`)
- Sum types with data (only `Result(T, E)` is built-in, with hard-coded shape)
- Heap allocation (everything is stack / argv / rbp-frame slots)
- Termination measures for non-fold non-quantifier recursion (the only "loops" are `fold`/`map`/`filter`/`fold_bytes`, all over bounded inputs)
- Genuine match-exhaustiveness verification (today's `match_result` exhaustiveness is enforced by *parser construction* — the grammar mandates both Ok and Err arms — not by a verifier check at all)

For a parser to produce an AST, ALL FIVE are needed in some form.

This document is about the first one (recursive types) and the second one (sum types). The other three are addressed at the boundary — heap allocation is replaced by a compile-time-sized arena; termination is replaced by structural recursion (mechanical); exhaustiveness becomes a real verifier pass for N-arm matches.

---

## 2. The pillars filter, applied

Before exploring designs, what does each pillar demand?

**Verifiability.** Every recursive type must carry a *mechanical* statement of how big it can grow. Not "the developer will be careful" — the verifier must be able to compute an upper bound from declarations alone, and reject anything that could exceed it. If we can't prove the bound mechanically, the feature is refused at design time.

**Exploitability.** The size bounds must be USED — both by the verifier (to accept the program) and by the emitter (to size arenas, plan stack frames). Bounds that exist only "for the auditor to read" don't qualify; they need to feed into codegen.

**Safety.** Exceeding the declared bounds at runtime cannot produce undefined behavior. It must abort fail-closed (sys_exit(1)), like every other declared bound (substring out-of-range, parse_int bad input, on_read_error: abort). No mode where the binary silently corrupts or accesses uninitialized memory. The abort must include a breadcrumb sufficient for an auditor to identify which bound was breached.

**Traceability.** An auditor reading the `.verbose` source sees the bound declarations next to the recursive type. The emitter's audit-log lines (or `strace` of the binary) make any bound-exceeded abort visible AND attributable.

**Readability.** The syntax for declaring a recursive type must be no more elaborate than the existing `text [..64]`. Same mental model — declared upper bound, treated as a contract.

Plus the **compiler axiom**: every bound is declared, never inferred. The compiler does not analyze "well, this rule recurses 3 times in the worst case so depth=3." It reads the declaration and verifies the construction sites against it.

---

## 3. Design space

Five candidate shapes for recursive types in Verbose. Each is evaluated against the pillars.

### 3.1 Self-referencing record with declared bounds

```
concept Tree [max_depth: 50, max_nodes: 10000]
  @intention: "binary tree with values at internal nodes"
  fields:
    value : number
    left  : Tree
    right : Tree
```

The `Tree` concept references itself in two fields. **Both bounds are required**: `max_depth` controls the recursive call stack (and structural-recursion termination); `max_nodes` controls the arena memory. They are NOT redundant — see Section 6 for the cost story.

**Pillars:**
- Verifiability ✓ — verifier checks every construction expression computes a bounded depth AND that the total node count stays within `max_nodes`.
- Exploitability ✓ — emitter pre-allocates an arena of `max_nodes × max_variant_size` bytes and uses `max_depth` to size structural-recursion call stack.
- Safety ✓ — exceeding either bound → sys_exit(1) with a per-type abort label so the auditor knows which bound was breached.
- Traceability ✓ — both `max_depth` and `max_nodes` are grep-able.
- Readability ✓ — same shape as `text [..64]` with two bounds instead of one. The two-bound form is necessary; one bound alone is incomplete.

### 3.2 Mutually recursive types via `concept_group`

```
concept_group AST [max_depth: 30, max_nodes: 5000]
  concept Stmt
    variants:
      If    of (cond: Expr, then_: Stmt, else_: Stmt)
      Block of (stmts: list<Stmt> [..100])
      Return of (e: Expr)

  concept Expr
    variants:
      Binary of (op: Op, lhs: Expr, rhs: Expr)
      Call   of (name: text, args: list<Expr> [..16])
      Int    of (n: number)
      Var    of (name: text)
```

Mutually recursive types are declared **at the strongly-connected-component level**, not per-type. The bounds apply to the *combined* tree: any path Stmt→Expr→Stmt→... contributes to the same depth count, and any allocation across both contributes to the same node count.

**Why combined and not per-type:** a path Stmt→Expr→Stmt→...→Stmt of length 60 should NOT be allowed when both `Stmt [max_depth: 30]` and `Expr [max_depth: 30]` are individually capped at 30. The compiler axiom forbids the verifier from inferring "the user probably meant per-type." So we force the user to declare at the SCC level, removing the ambiguity.

**Pillars:**
- Verifiability — needs an SCC analysis pass at the verifier level. Verbose's verifier already does whole-program walks for purity cross-references; this extends that machinery.
- Exploitability — emitter pre-allocates a single arena per `concept_group`, sized to fit the worst case variant of any type in the group.
- Safety, Traceability, Readability — same as 3.1. The `concept_group` keyword adds one new top-level item type; the audit story is "one declaration site for the AST" instead of two synchronized declarations.

A single-type recursive concept (3.1) is shorthand for a `concept_group` of size 1. The parser admits both syntaxes; the verifier normalizes the single-type case into a degenerate SCC.

### 3.3 List/sequence shapes

```
concept TokenList [max_length: 1000]
  fields:
    head : Token
    tail : TokenList   -- recursive
```

**Verdict:** redundant with `collection(T)` (which Verbose already has). A `collection(Token) [..1000]` already serves the use case. **Reject** — don't add overlapping mechanisms.

### 3.4 Type-erased opaque handles

**Verdict:** **reject.** This is the "C pointers in a fancy wrapper" approach. Verbose's pitch is "everything declared, everything visible in the IR." Opaque handles defeat that.

### 3.5 Functional / structural recursion via pattern matching

```
rule sum_constants (e : Expr) -> number
  match e:
    Binary(_, l, r) => sum_constants(l) + sum_constants(r)
    Call(_, args)   => fold(args, 0, acc, arg => acc + sum_constants(arg))
    Int(n)          => n
    Var(_)          => 0
  proofs:
    purity:
      reads : [e]
      calls : [sum_constants]
    termination:
      structural : e
```

A rule that pattern-matches on a recursive type and recurses on subterms. The termination measure is *structural*: each recursive call is on a strictly smaller subterm (a child of the matched variant).

**Pillars:**
- Verifiability — needs a "structural recursion" check: the verifier proves each `sum_constants(...)` recursive call is on a proper subterm of `e`. Mechanical via pattern destructuring (when you match `Binary(_, l, r)`, both `l` and `r` are strictly smaller than `e`).
- Exploitability — the structural measure feeds into the emitter's stack-depth analysis. Max stack depth = max_depth of the `concept_group`.
- Safety — combined with the depth bound, this gives bounded termination AND bounded stack.
- Readability — pattern matching is well-understood in audit contexts.

**This is the only form of recursion in rules we should allow.** General recursive function calls (with arbitrary argument changes) would require a custom termination measure per rule, which the verifier can't always prove mechanically. Structural recursion on a typed recursive structure is mechanical.

**Limitation surfaced by v1 review:** structural recursion does NOT cover all real cases. Two specifically:
- A rule that threads an *environment* through recursion (`eval(e, env)` where `env` grows in some branches). The `env` argument doesn't shrink. Workaround: express it as a `fold` over a children list with the env as the accumulator. This is more awkward than direct recursion but stays mechanical.
- Mutual recursion across two types (`type_check_stmt` calls `type_check_expr` calls `type_check_stmt`). The verifier must recognize that the COMBINED measure (depth in the concept_group's SCC) decreases on each cross-type call. The `concept_group` declaration is what makes this proof tractable.

---

## 4. The pick

**Recursive types are declared inside a `concept_group` block with `[max_depth: N, max_nodes: M]`. Variants are sum-typed. Recursion in rules is restricted to structural recursion via pattern match, declared via `termination: structural : <input_var>`.**

Concretely (the canonical AST shape):

```verbose
concept_group AST [max_depth: 30, max_nodes: 5000]

  concept Stmt
    variants:
      If    of (cond: Expr, then_: Stmt, else_: Stmt)
      Block of (stmts: list<Stmt> [..100])
      Return of (e: Expr)

  concept Expr
    variants:
      Binary of (op: Op, lhs: Expr, rhs: Expr)
      Call   of (name: text, args: list<Expr> [..16])
      Int    of (n: number)
      Var    of (name: text)
```

The verifier checks:
1. Every construction site of a type in the group produces a tree whose depth ≤ `max_depth` AND whose node count (summed across types) ≤ `max_nodes`. Statically when possible (compile-time tree-shape analysis); with runtime checks otherwise.
2. Every match on a variant type is exhaustive (covers all variants OR has a wildcard).
3. Every recursive rule with `termination: structural : <var>` has each recursive call on a strict subterm of `<var>`, validated by pattern decomposition.

The emitter:
1. Computes `arena_size = max_nodes × max_variant_size` at compile time (where `max_variant_size` is the max across all variants of all types in the group).
2. Allocates the arena in the rule's rbp frame OR, for cross-rule trees, in a declared shared arena (see Section 6).
3. Tree references are *indices* into the arena (compactly typed — 16 or 32 bits depending on `max_nodes` magnitude). Not raw pointers.
4. Structural recursion in rules emits as a real call (NOT inlined — see Q9). Stack depth = `max_depth`.
5. Exceeding `max_depth` or `max_nodes` at runtime → sys_exit(1), via per-type abort labels (so the auditor can attribute which bound was breached).

---

## 5. Why not other choices

**Why not heap allocation?** Because heap means an allocator, an allocator means a runtime, and a runtime means the verifier can no longer prove every byte the program will touch. Verbose's pitch is "every effect declared." A free-for-all heap defeats that. Arena allocation with declared bounds preserves it.

**Why not general recursion?** Because termination would require either inference (compiler-axiom violation) or per-rule termination measures (verbose, error-prone, and the verifier still has to check them mechanically). Structural recursion is mechanical: "this subterm is smaller than the input I matched" is provable from the AST node itself.

**Why not opaque handles?** Because they hide the structure from the auditor.

**Why not linear types?** Because they're a significant new type-system addition for a payoff that doesn't match Verbose's audience.

**Why two bounds (`max_depth` and `max_nodes`) and not one?** Because they control DIFFERENT costs. `max_depth` is one-dimensional: it bounds the stack depth of structural recursion. `max_nodes` is the actual memory budget: a binary tree at depth 30 can have up to 2³⁰ ≈ 10⁹ nodes. Declaring only `max_depth: 30` and computing arena size from it undersizes by an exponential factor. Declaring only `max_nodes: 5000` doesn't bound the call-stack depth (a degenerate left-spine tree of 5000 nodes is depth-5000). Both bounds matter; the verifier requires both to be declared on every recursive type.

---

## 6. Memory layout (native)

For a rule that constructs values from a `concept_group AST [max_depth: 30, max_nodes: 5000]`, the rbp frame includes:

```
rbp - X    : <input field slots>
rbp - X-8  : <let binding slots>
rbp - X-N  : <arena base>   (max_nodes × max_variant_size bytes)
rbp - X-...: <other rule slots>
```

The arena is a contiguous byte region. Each entry is a discriminated union: a tag (1 byte minimum, padded for alignment) + the variant data, padded to `max_variant_size` so every slot has the same size. This makes index arithmetic trivial (`arena_base + idx × entry_size`).

Tree references are 16-bit indices when `max_nodes ≤ 65535`, 32-bit otherwise. The verifier computes the width at compile time. Smaller indices reduce per-node memory cost.

**Construction emission.** Each `Binary { op, lhs, rhs }` construction site emits:
```
; (assume children indices are in r10 and r11 from prior subterm construction)
mov rax, [rbp + node_count_slot]    ; current node count
cmp rax, max_nodes
jae .arena_full_<type_abort>         ; per-type abort label
inc qword [rbp + node_count_slot]
imul rax, max_variant_size
add rax, [rbp + arena_base_slot]
mov byte [rax], TAG_BINARY
; ... write op, lhs index, rhs index into the variant payload
; rax = pointer to this node; caller uses (rax - arena_base) / entry_size as the index
```

**Pattern match emission.** `match e: Binary(...) => ... ; Int(n) => ... ; ...` emits a tag-dispatch:
```
movzx rax, byte [arena_base + idx × entry_size]   ; tag
cmp rax, TAG_BINARY
je  .binary_arm
cmp rax, TAG_INT
je  .int_arm
; ... fallthrough to wildcard or trap (verifier ensures one arm matches)
```

**Recursive call emission.** Recursive structural calls emit as REAL calls (push-pop, ret), NOT inlined. The Phase 2H-b path that inlines every text-returning call (`docs/native-designs.md:274-356`) MUST refuse self-recursive callees — extending the existing inliner to detect cycles and emit a real call convention for recursive rules. This is a non-trivial emitter shift. See Q9.

**Stack budget.** Maximum stack used by a structural recursion is `max_depth × stack_frame_size`. The verifier computes this and the emitter sizes the program's main stack accordingly (Linux default is 8 MiB; depth 30 with 256 B frames is 7.5 KiB — well within budget; depth 1000 with 1 KiB frames would need adjustment).

**Concat-in-recursive-rule caveat.** A recursive rule that emits text via `concat(...)` inside the body allocates a stack buffer per call (`sub rsp, N`). At depth 30, that's `30 × buffer_size` of cumulative stack. The verifier must compute `max_depth × max_per_call_concat_size` as part of the stack budget AND the program must have stack reserve to absorb this. For trees deeper than 30, or for very large concat buffers, this matters. The conservative answer: forbid concat in structurally-recursive rules, force the user to express text composition via fold or pre-compute. Less conservative: account for it in the budget. **Decision: forbid in Phase B, lift in a follow-up slice if a real use case appears.**

---

## 7. Audit story

An auditor reading a `.verbose` file with recursive types sees:

```
concept_group AST [max_depth: 30, max_nodes: 5000]
  concept Stmt variants: ...
  concept Expr variants: ...
```

What this means for the binary:
- The binary can construct at most 5000 nodes total across the AST group.
- Any single root-to-leaf path is at most 30 nodes deep.
- The arena's runtime size is bounded: `5000 × max_variant_size` bytes.
- Any attempt to exceed either bound aborts with `sys_exit(1)`. The abort label is unique per type (`<type_name>_<bound_kind>_abort`), so `addr2line` on the abort instruction pointer attributes the failure.
- Pattern matches on each type are verifier-checked for exhaustiveness.

A `strace` of the binary shows the abort syscall. Cross-referenced with `addr2line` and the per-type abort labels, the auditor knows exactly which bound was breached. A `readelf` shows the .text size including the arena dispatch bookkeeping.

The auditor never needs to chase a pointer. The IR shows the variant tag and the field decomposition explicitly. The native code shows the index computation explicitly.

---

## 8. Composition with existing features

**`fold`/`map`/`filter`.** A `list<Expr> [..16]` inside a recursive variant IS a `collection(Expr)`. Folding over a children list to traverse them is the natural pattern. Worked example:

```verbose
rule sum_constants (e : Expr) -> number
  logic:
    total = match e:
      Binary(_, l, r) => sum_constants(l) + sum_constants(r)
      Call(_, args)   => fold(args, 0, acc, arg => acc + sum_constants(arg))
      Int(n)          => n
      Var(_)          => 0
  proofs:
    purity:
      reads : [e]
      calls : [sum_constants]
    termination:
      structural : e
```

Two new things this surfaces vs current Verbose:
1. **`match <expr>:` with multiple variant arms** — generalization of `match_result`. New AST node `MatchVariant(scrutinee, arms)` where each arm is `(variant_name, field_bindings, body)`. Exhaustiveness becomes a real verifier check (not parser-enforced as `match_result` is today). New slice in Phase A.
2. **`termination: structural : <var>`** — new termination proof shape. The verifier walks the rule body and confirms every recursive call to the same rule passes an argument that is a destructured subterm of `<var>`. Existing `bound: N` shape stays for non-recursive rules; `structural: <var>` is the recursive form.

**`match_result`.** Result(Ok, Err) is grandfathered as the only built-in sum type. New sum types via `variants:` use the new `match <expr>: <variant>(...) => ...` syntax. The two coexist; we don't migrate `match_result` callers.

**`Record`.** A variant `Binary of (op: Op, lhs: Expr, rhs: Expr)` IS a record with three fields. The construction syntax extends naturally: `Binary { op: <op>, lhs: <expr>, rhs: <expr> }`.

**Phase 2H-b Call inlining.** Recursive rules MUST be detected and refused for inlining (otherwise the inliner loops). The detection is mechanical (the rule appears in its own `calls:`). For inlined rules: unchanged behavior. For recursive rules: emit as a real call. This is a real new emitter capability — see Q9.

**Resources / connections / fetches.** Orthogonal. A rule that reads a file AND constructs a recursive AST does both effects declaratively. The arena allocation is just another rbp slot.

---

## 9. Open questions

These are real questions for which I don't yet have an answer. Listed so future-me knows the gaps.

1. **Single-binary vs pipeline self-host.** Does the self-hosted compiler run as one big binary (parser+verifier+emitter in one) or as a pipeline (each phase a separate binary, chained via pipes)? Affects whether the arena lives in one rule or crosses rule boundaries — pipeline mode means trees are serialized to a wire format between binaries (no shared memory). **This question is upstream of Phase A** and should be resolved before starting. Recommended: pipeline mode (one binary per compiler phase) — matches how rule binaries already chain via shell, avoids cross-rule arena lifetime complexity, gives each phase its own audit story.

2. **Index width — automatic by `max_nodes` magnitude.** 16-bit covers 65535 entries; 32-bit covers 4 billion. The verifier picks the smallest width that fits `max_nodes`. Implementation detail; pinning here.

3. **Variant size disparity.** If one variant is `Int(n)` (8 bytes) and another is `Binary(Op, Expr, Expr)` (16+ bytes), the arena pays for the worst case at every slot. Accept this cost for simplicity. The alternative (per-variant separate arenas) buys 30-50% memory at the cost of harder dispatch and a worse audit story. **Decision: pad to max variant size, accept the waste.**

4. **Multiple `concept_group`s in one rule.** Does the rule allocate one arena per group or one combined arena? Per-group (clearer audit story; each group is an independent "type universe").

5. **Mutual recursion depth bound discipline — RESOLVED.** Per Section 3.2, the SCC-level declaration via `concept_group` removes the per-type-vs-joint ambiguity. Use SCC-level bounds.

6. **Structural recursion proof on `list<T>`.** When a rule recurses over `list<Expr>` (say, evaluating a function call's args), the "subterm" is each list element. The verifier needs to recognize this as structural. Probably mechanical via the fold machinery (a fold body's `arg` is treated as a subterm of the list, hence of the parent variant). Pinning: yes, `fold(args, ...)` inside a structural rule counts as structural for elements.

7. **WASM backend.** WASM's linear memory IS an arena conceptually. Mapping should be clean: the arena is just a byte region the rule allocates and indexes into. Same shape as native. Phase D (the self-host campaign) is where this gets tested.

8. **`termination: structural : <var>` semantics for recursive rules.** New proof shape. The verifier walks the rule body, finds every recursive call to the same rule, and confirms the argument is a destructured subterm of `<var>` via pattern. Concrete check: argument must be a field name introduced by a `match <var>: <variant>(<fields>) => ...` arm. Anything else (a fresh expression, a different rule's input) fails the check. **Mechanical.**

9. **Phase 2H-b inlining refusal for recursive rules.** The current emitter inlines every text-returning Call site (`docs/native-designs.md:274-356`). A self-recursive callee would cause the inliner to loop at compile time. The fix is twofold:
    - **Cycle detection:** when the inliner walks a callee body and encounters a Call back to a rule already in the inlining stack, refuse to inline.
    - **Real call convention:** non-inlined rules need a calling convention (push arguments, call, ret). Today, every rule is inlined; there is no real call convention. Designing one is a non-trivial new emitter capability — register allocation, return-value placement, prologue/epilogue. **Slice scope: this is probably its own multi-week phase, blocking structural recursion.**

10. **Error messages when a bound is exceeded.** Per the pillar-4 critique: which `max_depth: N` or `max_nodes: N` was breached? Per-type abort labels (e.g., `_Stmt_max_depth_abort`, `_AST_max_nodes_abort`) plus `addr2line` attribution. Encoded into the emit: each construction site jumps to a label specific to its type and the bound being checked.

11. **Arena lifetime across rule boundaries within ONE rule call chain.** A rule that calls another rule which produces a recursive structure: does the callee's arena get freed before the caller continues, or does the caller need to keep reading from it? Today the existing leaf-emission convention is "leaves write directly, no value materializes" (continuation-passing). A tree value is the OPPOSITE — it must materialize somewhere. The clean answer with pipeline mode (per Q1): rules don't return trees in-memory; they serialize trees to a wire format on stdout, the next rule deserializes. Same as today's number/text returns. **Resolves naturally if Q1 picks pipeline.**

12. **Variant exhaustiveness check.** Today `match_result` exhaustiveness is parser-enforced (both arms mandatory by grammar). For N-variant matches, exhaustiveness is a real verifier pass: walk the arms, confirm every declared variant is covered OR a wildcard arm exists. This is new work for the verifier — not just a generalization of an existing check. Slice scope: medium, lands in Phase A.

13. **Empty arena edge case.** A rule whose input is a recursive type but which constructs no new nodes (e.g., a pure analysis rule). Does the arena still get allocated? Per the static analysis: the arena is sized by *construction sites*, not by *input usage*. A rule that consumes but doesn't construct allocates an empty arena (zero bytes). The frame-size calculation handles this naturally — `max_nodes_constructed = 0` → arena_size = 0. Cost-free.

---

## 10. Roadmap

This document blocks the next concrete implementation. Once the open questions are resolved (or accepted as known-deferred), the implementation is a sequence of slices.

**Phase A: Sum types with data (no recursion yet).** 5-10 PRs.
- New syntax: `concept Foo variants: VarA of (x: number) | VarB of (y: text) | ...`
- New AST node: `Expr::Variant(concept_name, variant_name, field_values)`
- New AST node: `Expr::MatchVariant(scrutinee, arms)` for N-arm pattern match
- Verifier: exhaustiveness check (Q12), type check per variant, exhaustive-or-wildcard validation
- Native: tagged union layout, dispatch via tag
- Composes with existing `Record` shape: a variant IS a record with one extra tag byte
- Ships value independently: token kinds for the existing tokenizer work get sum types BEFORE recursion lands
- **Roughly 4-6 weeks of focused work, leveraging the existing `Result` machinery as template.**

**Phase A.5: Real call convention for non-inlined rules.** 3-6 PRs.
- Prerequisite for Phase C. Needs designing on its own (register allocation across call, return-value placement, prologue/epilogue).
- Touches every Call site in the emitter: must distinguish "inline this" from "make a real call."
- **Roughly 3-5 weeks. Could land in parallel with Phase B if scoped carefully.**

**Phase B: Recursive type references with bounded arena.** 8-15 PRs (revised up from v0).
- New syntax: `concept_group <name> [max_depth: N, max_nodes: M] ... `
- Verifier: SCC analysis, depth-bound + node-bound propagation through construction sites
- Native: arena allocation in rbp frame, index-based references with auto-sized index width
- Per-type abort labels (Q10)
- Forbid concat-in-recursive-rule for slice 1 (Section 6 caveat)
- **Roughly 8-15 weeks. The arena work alone is a new memory model — every prior stack-discipline slice (1B, 2F, 2H-b) took a full session each; the arena is bigger.**

**Phase C: Structural recursion in rules.** 5-10 PRs.
- New `termination: structural : <var>` proof shape (Q8)
- Verifier: subterm-decrease check via pattern destructuring
- Native: emit as a real call to the recursive rule (uses Phase A.5)
- Cycle detection in the inliner (Q9)
- **Roughly 5-8 weeks, depending on how cleanly Phase A.5 lands.**

**Phase D: Self-hosting integration.** Multi-month campaign.
- Lexer in Verbose (uses Phase A's sum types for tokens)
- Parser in Verbose (uses Phase A+B for AST, Phase C for traversals)
- Verifier in Verbose (uses Phase C for the walk)
- Emitter in Verbose (uses C for traversal, plus all the existing native emission patterns expressed declaratively)
- **Roughly 6-12 months. The campaign itself, not the language work.**

**Revised total: 9-12 months for Phases A through C** (the language work). Phase D is a separate campaign on top. Earlier "6-18 months" estimate revised down at the high end (Phase D split out) and up at the low end (arena work is real).

---

## 11. Decision pending

This doc is a recommendation, not a commitment. Before implementing Phase A, the user should confirm:

1. **The pick.** Sum types + `concept_group [max_depth, max_nodes]` recursive references + structural recursion + arena allocation. Is this the right shape?
2. **The deferred questions.** Most can wait for Phase A/B to surface their answers. **Q1 (single-binary vs pipeline)** is upstream of Phase A and should be resolved first. Recommended: pipeline mode.
3. **The roadmap commitment.** 9-12 months of language work (Phases A through C), then a multi-month self-hosting campaign. This is in the same range as the earlier "10-15 years for full UI" estimate, just narrowed to self-hosting alone.

If any of those is "no", we revisit. If they're "yes", Phase A starts with sum types — which incidentally are useful for token kinds in the existing tokenizer work, so they ship value before recursive types even land.

---

*v0: 2026-05-15. v1: 2026-05-15, after fresh-context subagent review surfaced load-bearing gaps in arena sizing, mutual-recursion bound semantics, and the inlining-vs-recursion conflict. v1 adopts the subagent's three counter-proposals and integrates six previously-missing open questions.*
