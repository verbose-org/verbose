# Recursive types — design doc

**Status:** design phase — no implementation yet.
**Motivation:** self-hosted Verbose compiler (lexer in Verbose → parser in Verbose → verifier in Verbose → emitter in Verbose). The parser needs an AST; an AST is inherently a recursive tree.
**Filter:** every choice in this doc must respect the five pillars (verifiability, exploitability, safety, traceability, readability) AND the compiler axiom ("controls and applies, never guesses"). If a design point requires the verifier to infer something it can't mechanically prove, it's refused.

This document is not a slice plan. It's an exploration of the design space, with the constraint that the answer must be expressible in Verbose-the-language as a *deliberate, declared* feature — not as a workaround.

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

For a parser to produce an AST, ALL FOUR are needed in some form.

This document is about the first one (recursive types). The other three are connected — sum types are addressed in a companion doc; heap / termination are touched here at the boundary.

---

## 2. The pillars filter, applied

Before exploring designs, what does each pillar demand?

**Verifiability.** Every recursive type must carry a *mechanical* statement of how deep it can go. Not "the developer will be careful" — the verifier must be able to compute an upper bound from declarations alone, and reject anything that could exceed it. If we can't prove the bound mechanically, the feature is refused at design time.

**Exploitability.** The depth bound must be USED — both by the verifier (to accept the program) and by the emitter (to size memory, allocate arenas, plan stack frames). Bounds that exist only "for the auditor to read" don't qualify; they need to feed into codegen.

**Safety.** Exceeding the declared depth at runtime cannot produce undefined behavior. It must abort fail-closed (sys_exit(1)), like every other declared bound (substring out-of-range, parse_int bad input, on_read_error: abort). No mode where the binary silently corrupts or accesses uninitialized memory.

**Traceability.** An auditor reading the `.verbose` source sees `max_depth: N` next to the recursive type. The emitter's audit-log lines (or `strace` of the binary) make any depth-exceeded abort visible. The IR shows the depth used at each construction site.

**Readability.** The syntax for declaring a recursive type must be no more elaborate than the existing `text [..64]`. Same mental model — declared upper bound, treated as a contract.

Plus the **compiler axiom**: the depth bound is declared, never inferred. The compiler does not analyze "well, this rule recurses 3 times in the worst case so depth=3." It reads the declaration and verifies the construction sites against it.

---

## 3. Design space

Five candidate shapes for recursive types in Verbose. Each is evaluated against the pillars.

### 3.1 Self-referencing record with declared depth

```
concept Tree [max_depth: 50]
  @intention: "binary tree with values at internal nodes"
  fields:
    value : number
    left : Tree
    right : Tree
```

The `Tree` concept references itself in two fields. The `max_depth: 50` declaration says: any construction-site walks at most 50 levels deep.

**Pillars:**
- Verifiability ✓ — verifier checks every construction expression (a recursive `Tree { ... }` literal) computes a bounded depth.
- Exploitability ✓ — emitter pre-allocates an arena of size `50 × sizeof(Tree)` bytes.
- Safety ✓ — exceeding 50 → sys_exit(1).
- Traceability ✓ — `max_depth: 50` is grep-able.
- Readability ✓ — same shape as `text [..64]`.

**Open questions:**
- What's `sizeof(Tree)` when one variant is "leaf" (value only) and another is "internal" (value + 2 children)? Need sum types.
- How does the verifier know the depth of a runtime-constructed tree? If a recursive rule builds it, the rule's recursion must be bounded by the same N.

This is the strongest candidate so far. Continue.

### 3.2 Mutually recursive types

```
concept Stmt [max_depth: 30]
  variants:
    If    of Expr × Stmt × Stmt
    Block of list<Stmt> [..100]
    Return of Expr

concept Expr [max_depth: 30]
  variants:
    Binary of Op × Expr × Expr
    Call   of name × list<Expr> [..16]
    Int    of number
    Var    of name
```

`Stmt` and `Expr` reference each other. The depth is the COMBINED depth of the cycle.

**Pillars:**
- Verifiability — needs a fixpoint pass. The verifier walks both types, computes the strongly-connected component, and assigns a shared depth bound. The user declares `max_depth: 30` on each (they MUST agree, or the verifier rejects).
- Exploitability — emitter pre-allocates a single arena sized to fit the worst case for each variant.
- Safety, Traceability, Readability — same as 3.1.

Mutually recursive types are how real ASTs look. Without them, we'd be stuck with single self-recursion (which doesn't model `Stmt` containing `Expr` containing `Stmt`).

### 3.3 List/sequence shapes

```
concept TokenList [max_length: 1000]
  fields:
    head : Token
    tail : TokenList   -- optional, null-terminated by depth
```

Linked-list shape via recursion. Alternative to `collection(Token)`.

**Verdict:** redundant with `collection(T)` (which Verbose already has). A `collection(Token)` with `[..1000]` bound already serves the use case. **Reject** — don't add overlapping mechanisms.

The recursive types we DO need are *trees* (multiple children per node) and *sum types with data* (variant + content). Lists are flat — they're collections.

### 3.4 Type-erased opaque handles

```
type AstHandle = opaque(arena_idx: number)
```

Recursive references stored as opaque indices into a runtime arena. The user never sees the index; it's wrapped in a typed handle.

**Pillars:**
- Verifiability — somewhat ✓; the verifier sees `AstHandle` but doesn't verify the arena's contents.
- Exploitability — the index is used by the emitter, but the type system is mostly "trust me."
- Readability — opaque types are a step backward; the auditor reads `AstHandle` and has to chase the arena layout elsewhere.

**Verdict:** **reject.** This is the "C pointers in a fancy wrapper" approach. Verbose's pitch is "everything declared, everything visible in the IR." Opaque handles defeat that.

### 3.5 Functional / structural recursion via pattern matching

```
rule eval (e : Expr) -> number
  match e:
    Binary(Op::Add, l, r) => eval(l) + eval(r)
    Binary(Op::Sub, l, r) => eval(l) - eval(r)
    Int(n)                => n
    Var(_)                => 0
```

A rule that pattern-matches on a recursive type and recurses on subterms. The termination measure is *structural*: each recursive call is on a strictly smaller subterm (a child of the original `e`).

**Pillars:**
- Verifiability — needs a "structural recursion" check: the verifier proves each `eval(...)` recursive call is on a proper subterm of `e`. Mechanical via pattern destructuring (when you match `Binary(Op::Add, l, r)`, both `l` and `r` are strictly smaller).
- Exploitability — the structural measure feeds into the emitter's stack-depth analysis. Max stack depth = max_depth of the type.
- Safety — combined with the depth bound, this gives bounded termination AND bounded stack.
- Readability — pattern matching is well-understood in audit contexts (functional programmers parse it fluently; auditors of regulated software have seen it in ML/Haskell).

This is the *only* form of recursion in rules we should allow. General recursive function calls (with arbitrary argument changes) would require a custom termination measure per rule, which the verifier can't always prove mechanically. Structural recursion on a typed recursive structure is mechanical.

**This is the verdict for HOW recursion enters the language: structural recursion on declared recursive types, paired with depth-bounded construction.** Not "general recursion" — that opens the door to non-terminating rules.

---

## 4. The pick

**Recursive types are declared with `max_depth: N`. They form a closed system via sum types (variants). Recursion in rules is restricted to structural recursion on those types — each recursive call decomposes the input into one of its declared variants.**

Concretely:

```
concept Expr [max_depth: 30]
  variants:
    Binary of (Op, Expr, Expr)
    Call   of (name : text, args : list<Expr> [..16])
    Int    of (n : number)
    Var    of (name : text)
```

- `max_depth: 30` declares the upper bound on nesting.
- `variants:` lists the sum-type cases, each with its own field shape.
- `list<Expr> [..16]` is the existing `collection` shape, bounded.
- Recursion in rules is *structural*: a rule that recurses on `Expr` must pattern-match and recurse only on children of the matched node.

The verifier checks:
1. Every construction site of a recursive type produces a tree of depth ≤ N (statically when possible, with a runtime check otherwise).
2. Every recursive rule's recursive call is on a strict subterm.
3. The combined memory budget (depth × max-variant-size × number-of-construction-sites) fits within the rule's declared frame.

The emitter:
1. Computes `arena_size = max_depth × max_variant_size` at compile time.
2. Allocates the arena in the rbp frame (or in a per-program arena slot for cross-rule trees — see open questions).
3. Tree pointers are *indices* into the arena, not raw pointers. Index width: enough bits to address `max_depth × max_construction_sites` entries.
4. Structural recursion in rules emits as a tail call (when possible) or a bounded recursive call (with stack depth = max_depth).
5. Exceeding `max_depth` at runtime → sys_exit(1), same fail-closed posture as substring bounds.

---

## 5. Why not other choices

**Why not heap allocation?** Because heap means an allocator, an allocator means a runtime, and a runtime means the verifier can no longer prove every byte the program will touch. Verbose's pitch is "every effect declared." A free-for-all heap defeats that. Arena allocation with a declared bound preserves it.

**Why not general recursion?** Because termination would require either inference (compiler-axiom violation: "the compiler guesses") or per-rule termination measures (verbose, error-prone, and the verifier still has to check them mechanically). Structural recursion is mechanical: "this subterm is smaller than the input I matched" is provable from the AST node itself.

**Why not opaque handles?** Because they hide the structure from the auditor. The IR should be "what you read is what runs" — opaque handles break that.

**Why not linear types?** Because they're a significant new type-system addition for a payoff that doesn't match Verbose's audience. Verbose is for AI-authored, human-audited code; linear types add cognitive load on both sides. The arena approach gives us bounded memory and structural recursion with less new machinery.

---

## 6. Memory layout (native)

For a rule with a declared recursive type `Expr [max_depth: 30]` and N construction sites at runtime, the rbp frame includes:

```
rbp - X    : <input field slots>
rbp - X-8  : <let binding slots>
rbp - X-N  : <arena base>  (max_depth × max_variant_size × N_sites × 8)
rbp - X-...: <other rule slots>
```

The arena is a contiguous byte region. Each entry is a discriminated union: 1 byte (or 4 for alignment) tag + variant data padded to the max variant size.

Tree references are 32-bit (or 64-bit, conservative) indices into the arena. To traverse a child, the emitter does:
```
mov rax, [rbp + arena_base + idx × entry_size]  ; load entry
movzx rcx, byte [rax]                            ; load tag
; dispatch on rcx to the right variant decoder
```

Depth-exceeded check: each construction-site emit includes `cmp current_depth, max_depth` and `ja .abort` if violated. The current depth is tracked in a register or a dedicated slot.

The arena lives for the rule invocation. On rule exit (`mov rsp, rbp; pop rbp`), it's freed automatically — no separate free/dealloc needed.

For cross-rule trees (one rule produces a tree, another consumes it), the arena must persist past one rule's frame. Two options:
- BSS segment with a program-wide arena (single allocation, all rules share)
- Stream-based: the producing rule writes the tree to stdout in a binary format, the consuming rule reads from stdin (no in-memory persistence)

Both are options. The BSS approach matches how text and number outputs already cross rule boundaries in service binaries (the bytes are in the request/response buffer). The streaming approach matches how rules currently chain via shell pipes. **Decision deferred** — depends on whether the self-hosted compiler is a single binary or a pipeline.

---

## 7. Audit story

An auditor reading a `.verbose` file with recursive types sees:

```
concept Expr [max_depth: 30]
  variants:
    Binary of (Op, Expr, Expr)
    Int    of (n : number)
    ...
```

What this means for the binary:
- The binary can construct at most 30 levels of `Expr` nesting.
- The arena's runtime size is bounded: `30 × max_variant_size × max_construction_sites`.
- Any attempt to build a deeper tree aborts the process with `sys_exit(1)`.
- Pattern matches on `Expr` cover all variants (verifier enforces exhaustiveness — same mechanical check we already have for `match_result`).

A `strace` of the binary shows the abort syscall when depth is exceeded. A `readelf` shows the .text size including the arena bookkeeping.

The auditor never needs to chase a pointer. The IR shows the variant tag and the field decomposition explicitly. The native code shows the index computation explicitly.

---

## 8. Composition with existing features

- **`fold`/`map`/`filter`** — these work on `collection<T>`. A `list<Expr> [..N]` inside a recursive variant IS a `collection(Expr)`, so fold composes naturally.
- **`match_result`** — pattern-matches on `Result(Ok, Err)`. Generalizing to sum types means: `match e: variant1(...) => ... ; variant2(...) => ...`. The match exhaustiveness check generalizes the existing 2-arm check.
- **`Record`** — a record with multiple typed fields. A recursive sum-type variant `Binary of (Op, Expr, Expr)` IS a record with three fields, one of which is the recursive type. The Record construction syntax extends naturally.
- **Resources / connections / fetches** — orthogonal. A rule that reads a file AND constructs a recursive AST does both effects declaratively.

The composition is mostly mechanical. The new machinery is: depth-bounded recursive references + sum types + structural recursion. Everything else extends.

---

## 9. Open questions

These are real questions for which I don't yet have an answer. Listed so future-me (or the user) knows the gaps.

1. **Single-binary vs pipeline.** Does the self-hosted compiler run as one big binary (parser+verifier+emitter in one) or as a pipeline (each phase a separate binary, chained via pipes)? Affects whether the arena lives in one rule or crosses rule boundaries.

2. **Index width.** 32-bit indices cover 4 billion entries — overkill. 16-bit covers 65k — probably enough for an AST. But the verifier must compute the worst case from `max_depth × construction_sites` and pick the right width. Implementation detail but worth nailing early.

3. **Variant size disparity.** If one variant is `Int(n)` (8 bytes) and another is `Binary(Op, Expr, Expr)` (16+ bytes), the arena pays for the worst case at every slot. Acceptable cost for the simplicity, but worth measuring on a real AST.

4. **Multiple recursive types in one rule.** Does the rule allocate one arena per type or one combined arena? Likely per-type (clearer audit story).

5. **Mutual recursion: depth bound discipline.** If `Stmt [max_depth: 30]` and `Expr [max_depth: 30]` mutually reference, is the depth measured per-type or jointly? Joint is safer (a path Stmt→Expr→Stmt→... contributes to both). Probably the answer is: each declared bound is the per-type cap; the combined arena fits both.

6. **Structural recursion proof on `list<Expr>`.** When a rule recurses over `list<Expr>` (say, evaluating a function call's args), the "subterm" is each list element. The verifier needs to recognize this as structural — same mental shape as `fold` body's variable. Probably mechanical via the fold/map machinery.

7. **WASM backend.** WASM doesn't have heap-like memory in the same shape. The arena is just bytes in the linear memory, which WASM has. Should map cleanly.

---

## 10. Roadmap

This document blocks the next concrete implementation. Once the open questions are resolved (or accepted as known-deferred), the implementation is a sequence of slices.

**Phase A: Sum types with data (no recursion yet).**
- New syntax: `concept Foo variants: VarA of (x: number) | VarB of (y: text) | ...`
- AST node: `Expr::Variant(concept_name, variant_name, field_values)`
- Pattern match generalization: `match e: VarA(x) => ... ; VarB(y) => ...`
- Verifier: exhaustiveness check, type check per variant
- Native: tagged union layout, dispatch via tag
- Slice scope: 5-10 PRs, foundation for everything that follows

**Phase B: Recursive type references with depth bound.**
- New syntax: `concept Expr [max_depth: 30]` plus self-references in `variants:`
- Verifier: depth-bound propagation through construction sites
- Native: arena allocation, index-based references
- Slice scope: 5-15 PRs, depending on how much fold/map composition we land

**Phase C: Structural recursion in rules.**
- Recursive rule calls on subterms of a matched variant
- Verifier: subterm-decrease check
- Native: stack-depth-bounded recursive emit (or trampoline if depth exceeds reasonable stack)
- Slice scope: 5-10 PRs

**Phase D: Self-hosting integration.**
- Write the lexer in Verbose (uses Phase A's sum types for tokens)
- Write the parser in Verbose (uses Phase A+B for AST)
- Wire the verifier and emitter (uses Phase C for structural walks)
- Slice scope: long campaign, measured in months

**Total: 6-18 months for the language work, then the self-hosting campaign on top.** This is in line with the earlier honest estimate (10-15 years for full UI; 5-8 years for self-host alone).

---

## 11. Decision pending

This doc is a recommendation, not a commitment. Before implementing Phase A, the user should confirm:

1. The pick (sum types + depth-bounded recursive references + structural recursion) is the right shape.
2. The deferred questions (especially single-binary vs pipeline) can wait until Phase A/B reveals their answers.
3. The 6-18-month language work is acceptable as a roadmap commitment.

If any of those is "no", we revisit. If they're "yes", Phase A starts with sum types — which incidentally are useful for token kinds in the existing tokenizer work, so they ship value before recursive types even land.

---

*Last updated: 2026-05-15. Author: design exploration session.*
