# Native call convention — design doc (Phase A slice 5.1+)

**Status:** design phase, no implementation. Slice 5.0 (the safety floor) shipped — recursive rules are refused at compile time with a clear breadcrumb. This doc explores what slice 5.1 should ship to lift that refusal.

**Motivation:** every native emit path today inlines callees (Phase 2D match_result, Phase 2G text-returning call, Phase 2H-a/b reaction logs, Phase A slice 4.1/4.2 MatchVariant). Inlining is fast and gives a clean audit story for non-recursive code — every call site shows the callee's body in situ. But a recursive call graph cannot be inlined: the expansion is infinite. Real call instructions (`call <label>` / `ret`) are needed for any rule whose call graph contains a cycle.

This unblocks Phase B (recursive types — the AST needs them) and Phase C (structural recursion in rules — `sum_constants(Binary(_, l, r)) = sum_constants(l) + sum_constants(r)` is the canonical shape). Both are self-hosting prerequisites.

**Filter:** every choice in this doc must respect the five pillars (verifiability, exploitability, safety, traceability, readability) AND the compiler axiom ("controls and applies, never guesses"). When two design points are otherwise equivalent, the one that keeps the inlining audit story coherent wins.

**Revision history:** v0 (initial draft); v1 (this version) incorporates a fresh-context subagent review per [[feedback_subagent_for_strategic_review]]. Six findings from that review changed substantive design points: (a) the "Phase B reuses this ABI" claim was wrong and got deleted (Phase B uses arena indices, not raw pointers); (b) slice 5.1 scope was misshapen — `fact(N{v:n.v-1})` smuggles a new Record-at-call-site emit capability that should be split; (c) the verifier warning for unproven termination is now MANDATORY, not optional, plus a cheap "argument provably equals input" rejection lands ahead of structural recursion; (d) the audit story honestly admits a degradation rather than papering over it; (e) the recursion depth bound argument was UNSOUND — runtime input fields aren't clamped to declared ranges today; surfaced as a prerequisite slice; (f) per-emitter integration audit replaces the breezy one-liner. The doc below is the post-review state.

---

## 1. What slice 5.0 left on the table

Slice 5.0 (PR #32) detects cycles in the native call graph reachable from the entry rule's logic and refuses with a breadcrumb. Concretely:

```
rule 'fact' is part of a recursive call graph (cycle through 'fact');
native lowering needs Phase A slice 5.1+ (real call convention).
Use --run for now.
```

What's missing: a real call convention. Every Verbose rule call today is either inlined into the caller's body or rejected. Slice 5.1+ must define how a callee that CAN'T be inlined gets emitted, called, and returned from.

## 2. The pillars filter, applied

**Verifiability.** Each recursive rule needs a termination story. The current state of the language:
- Existing `bound : N` proof field counts AST operations and is enforced statically against the source body. It does NOT prove termination for recursion — for a non-recursive rule, the bound is mechanical (no loop ⇒ N ops ⇒ ≤ bound). For recursion, the bound bounds *one call's local work*, not the total depth of recursion.
- `termination: structural : <var>` is the Phase C shape that proves "each recursive call passes a strictly smaller subterm of `<var>`". Mechanical via pattern destructuring (the design doc for recursive types nails this).
- Slice 5.1 ships BEFORE Phase C. So at slice 5.1's landing, the verifier still doesn't prove recursion terminates. The compromise: accept the existing `bound : N` as a *local* bound and document that runtime termination is the user's responsibility until Phase C. Stack-overflow on a non-terminating recursion is the runtime witness (sys_exit via SIGSEGV).

**Exploitability.** Whatever ABI we pick must serve future slices — Phase B's recursive type traversal will lean on it; Phase C will refine the termination proof but reuse the same calling convention. The ABI choice is load-bearing for the whole self-hosting trajectory.

**Safety.** A stack overflow on a non-terminating recursion must abort the binary fail-closed, not corrupt memory. Linux's default 8 MiB stack and SIGSEGV-on-overflow give that for free. A non-recursive caller invoking a recursive rule must not pollute its own frame layout.

**Traceability.** Today, an inlined call's audit story is "the callee's body is right there in `addr2line`." A real call's story becomes "the `call <addr>` instruction points at the callee's label; the auditor follows it once." Slightly more indirection, but the callee body lives at a single fixed location, which is actually better for code review than N inlined copies.

**Readability.** No new syntax in the `.verbose` source. Recursive rules look exactly like non-recursive rules — the `calls:` proof already names the callee, and self-references in that list are how the verifier (and emitter, and now this slice) recognize recursion. The auditor sees no special marker.

## 3. The design space

### 3.1 Which rules become callable functions

Three candidate predicates for "this rule must be a callable, not inlined":

- **(a) Anything on a cycle in the call graph.** Self-recursive rules and any node in a mutual-recursion cycle. Slice 5.0's existing detector identifies these.
- **(b) Anything called by 2+ distinct call sites.** Even non-recursive — saves code size when the callee is large and reused.
- **(c) Anything the user opts in via `@no_inline`.** Explicit, audit-readable.

For slice 5.1, **(a) is the minimum** — anything cyclic MUST be callable. (b) and (c) are size optimizations that should come later if at all. The default stays "non-recursive = inlined" because inlining keeps the audit story flat (the callee's body is visible at the call site).

### 3.2 Argument passing convention

The rule's input is a Named concept — a record of typed fields. How are those fields handed to the callee?

**Option A: Pointer to a fields struct.** Caller materializes the fields into a contiguous memory region (stack or heap), passes the pointer in rdi. Callee dereferences. Pros: one register, scales to many fields, mirrors how the `_start` argv layout works. Cons: requires the caller to lay out the struct (more bookkeeping than today's inline shape).

**Option B: Individual fields in System V registers (rdi, rsi, rdx, rcx, r8, r9 — up to 6).** Caller loads each field's value into the next register, callee unloads at prologue. Pros: simplest for small concepts; no memory traffic. Cons: caps the input concept at 6 number fields (text fields cost two registers — (ptr, len) — so 3 text fields max). Doesn't scale to large records.

**Option C: Pre-loaded rbp frame, callee inherits parent's field offsets.** Like Phase 2D inlining, but with a real `call` instead of textual inlining. Caller leaves its frame as-is; callee assumes rbp + standard offsets are valid. Pros: no marshalling cost. Cons: makes the callee NOT a real function — its calling convention depends on every caller using the SAME frame layout. Defeats the point of having a real callable; breaks composability with non-Verbose callers (eventually).

**Recommendation:** **Option A** as the slice 5.1+ ABI. Pointer to a fields struct in rdi. The struct layout is fixed by the input concept's declaration order: `[field0 (8B), field1 (8B), ...]` for number fields; `[ptr (8B), len (8B)]` for each text field. The caller materializes onto its own stack just below rsp before the `call`. Callee reads the struct via `[rdi + offset]` at prologue start.

Why A over B: scales to large concepts uniformly; one register cost regardless of field count. (An earlier v0 of this doc claimed Option A also "matches how Phase B's recursive types will need to pass tree nodes" — that claim was wrong. Phase B's recursive types use *indices into an arena* (per `docs/recursive-types-design.md`, "Tree references are 16-bit indices when `max_nodes ≤ 65535`, 32-bit otherwise"), not raw pointers, so Phase B will need a distinct ABI of (rdi=arena_base, rsi=node_idx) or a wrapped struct. Picking A for slice 5.1 stands on uniform-layout merits alone, not Phase B reuse.)

For slice 5.1's actual scope (single Number-field input), Option B (register-only) is *strictly simpler* to implement: one `mov rdi, <value>; call <label>` at the call site, one `mov rax, rdi` (or read directly via rdi) at the prologue. No fields struct, no marshalling. The trade-off is that B forces a re-design for multi-field input in slice 5.3. Pick: **B for slice 5.1**, switch to A in slice 5.3 when multi-field arrives. This minimises the slice-5.1 emit work without locking the future in.

Why neither A nor C: C (callee inherits caller's frame) makes the callee NOT a real function. Defeats the point — slice 5.1's whole job is to have a callable with a stable ABI.

### 3.3 Return value convention

Mirrors the existing per-output-type conventions:
- `Type::Number` / `Type::Bool` → return in rax (already standard for inlined number-output)
- `Type::Text` → return as (rax = ptr, rdx = len) (the BoundText pair shape)
- `Type::Named(C)` (Record) → caller passes a hidden first arg pointing to a destination buffer; callee writes the record there and returns the pointer in rax. This is the "sret" convention from System V's struct-return rule.
- `Type::Result(T, E)` → (rax = tag, rdx = ok_value, r8 = err_ptr, r9 = err_len) — same shape as the existing match_result frame slots but lifted into registers.

For slice 5.1's initial scope, ship Number/Bool only. Text, Record, and Result returns are follow-ups (one per slice).

### 3.4 Frame layout

Each callee has its own rbp frame, allocated at call time:

```
push rbp
mov rbp, rsp
sub rsp, frame_size   ; field slots + let slots + match slots
; ... callee body ...
mov rsp, rbp
pop rbp
ret
```

The callee's frame_size is computed identically to a non-recursive rule's prologue — input field slots + let bindings + match_result slots (if used). The PARENT's frame is preserved below the callee's; rsp restores it on `ret`.

Stack budget is bounded *per call* by frame_size, but the per-call frame_size is NOT a simple constant if the recursive body contains text-let bindings or `concat(...)` buffers (Phase 2I): those allocate via `sub rsp, N` inside the body, growing the per-call footprint by up to `max_text_len + N`. At depth K, the total stack consumed is `K × max(frame_size + dynamic_growth)`, which can blow past Linux's 8 MiB faster than a naive `K × frame_size` calculation suggests. **Slice 5.1 takes the conservative line from `docs/recursive-types-design.md` section 6 ("concat in recursive rules") and FORBIDS text-let, `concat`, and reactions inside the body of any recursive rule.** A recursive rule whose body uses these is rejected with a clear breadcrumb pointing at "use a fold over a children list with an accumulator" as the workaround. Lifting this restriction is a follow-up slice with explicit stack-budget accounting (the "concat caveat" from Phase B planning).

For the slice-5.1-allowed shapes (Number / Bool body, no text-let, no concat in the recursive body), frame_size is a small constant (≤ 64 bytes typically: 8B for the single Number field × 1, plus ~5 quartets of headroom for match_slots if any). Linux's 8 MiB default holds N ≈ 100k frames, which is more than ample.

### 3.5 Where the callee's code lives in the binary

Today's inlined emit puts the callee's instructions at every call site. A real callable needs to live ONCE at a known address.

Strategy: emit each callable rule as a separate section AFTER the entry rule's code, before the shared sys_exit epilogue. Layout:

```
_start:
  ; ... entry rule loop body ...
  ; ... call <callee_label> ...    <- relative call using known offset
  ; ... more entry rule body ...
  jmp .exit

callee_label:
  push rbp
  ; ... callee body ...
  pop rbp
  ret

.exit:
  mov rax, 60
  mov rdi, [rbp + exit_flag_slot]
  syscall
```

The `call <callee_label>` instruction uses a rel32 displacement patched after the callable's address is known. Standard forward-patch pattern, already used by the if/else emitters and the match_result inliner.

### 3.6 Termination — what we don't ship in slice 5.1, and what we DO ship

The verifier in slice 5.1 does NOT prove that recursion terminates. The existing `bound : N` proof field is checked statically against the AST operation count of the rule's body. For a non-recursive rule that's mechanical and meaningful. **For a recursive rule it's unenforceable as written** — the static body has a trivial op count, the actual cost is whatever the recursion does at runtime. Slice 5.1 should be honest about this rather than pretend `bound:` carries weight in the recursive case. The compiler accepts the bound; the runtime stack-overflows if recursion is unbounded.

This is a deliberate compromise to ship slice 5.1 BEFORE Phase C. The follow-up slice (Phase C structural recursion) adds a real verifier check via `termination: structural : <var>`; until then, recursive native binaries are no safer than recursive interpreter runs (both stack-overflow on non-terminating input).

**Two mitigations that DO ship in slice 5.1:**

1. **MANDATORY breadcrumb on every recursive rule compile.** Not optional, not "could emit." The verifier MUST emit a diagnostic on stderr at compile time whenever a recursive rule is encountered, of the shape:

       rule 'fact' is recursive (cycle through 'fact'); the declared `bound:`
       is NOT a termination proof for recursion. Runtime stack-overflow is
       the only safety signal until Phase C ships structural recursion. Use
       `--run` (interpreter) if you need an audit trace.

   A silent recursive native compile would betray the compiler axiom ("controls and applies, never guesses"). The breadcrumb is a hard requirement.

2. **Cheap "obvious non-termination" rejection.** Before accepting a recursive rule, the verifier walks the body for self-calls whose argument is provably identical to the input — e.g., `fact(n)` directly, or `fact(N { v: n.v })` where every field expression is literally a Field of the input. Such a call cannot terminate (no progress on the argument). Reject with a clear breadcrumb naming the offending call site. This catches the trap that bit me while authoring the slice 5.0 test rule (where I wrote `fact(n)` and it silently infinite-looped the interpreter), without committing to full structural recursion.

These two together close the most common foot-gun — silent acceptance of "fact(n) — really" — while leaving Phase C's mechanical proof as a future, separable slice.

### 3.7 Interaction with existing inlining paths

Five distinct emit paths currently inline rule calls. Each needs its own branch for slice 5.1 — a one-line "add a check in emit_eval_expr" is not enough. The per-emitter audit:

| Emitter | Today's shape | Slice 5.1 change |
|---|---|---|
| `emit_eval_expr`'s `Expr::Call` arm (Phase 0/scalar) | Looks up callee, walks its body inline as if part of current expression | Branch: if callee is on a cycle, emit `mov rdi, <value>; call <label>; <rax has return>`; else inline as today |
| `emit_match_result_inlined` (Phase 2D) | Walks callee's logic, redirects Ok/Err leaves into outer arms | UNCHANGED IF callee is non-recursive. If callee is on a cycle, refuse with breadcrumb — Phase 2D's inlining contract assumes a leaf-by-leaf rewrite that fails on recursion. Lifting this is a Phase-2D-extension slice, not slice 5.1. |
| `emit_text_write_to_fd` Phase 2G (text-returning call inline) | Recurses on `callee.logic.value` byte-for-byte | Same as above: refuse cyclic callee here with breadcrumb |
| `emit_concat_to_buffer` Phase 2H-b (Call as concat arg) | Pre-evaluates each Call, stashes (ptr, len) in scratch slots | Same — refuse cyclic callee in slice 5.1 |
| `emit_redirect_variant_leaves` / `_text` (Phase A slice 4.1/4.2 MatchVariant) | Walks callee's body redirecting VariantConstruct leaves | Same — refuse cyclic callee |

So slice 5.1 lifts ONE refusal (the top-level `Expr::Call` arm) and leaves four refusals in place. This is honest: real-call semantics only land where they're independently sound; Phase 2D/2G/2H/4.x interactions with recursion are their own slices.

The callable's prologue handles its own field loading from rdi (Option B for slice 5.1: rdi holds the single Number field directly). The caller doesn't need to know the callee's frame layout.

## 4. Slice 5.1 scope (split into 5.1a + 5.1b)

The original v0 plan packed two new emit capabilities into one slice: (a) the call/ret + label-patching ABI, and (b) inline `Record` construction at the call site (materializing `N { v: n.v - 1 }` into a fresh fields struct before the call). Capability (b) is brand-new emit work — today nothing constructs a Record into stack memory at a call site. Packing both into one slice would entangle the ABI debate with the marshalling debate. Split honestly:

### Slice 5.1a — call/ret ABI for pass-through self-recursion

- **Input:** single-Number-field concept. `concept N fields: v : number`.
- **Output:** Number or Bool (rax convention).
- **Recursion shape:** direct self-recursion where the recursive call's argument is the SAME input value, NOT a constructed Record. Concretely, the only legal recursive call in 5.1a is `self(n)` where `n` is the input ident. This rules out the obvious-non-termination case (caught by the section 3.6 check) — so 5.1a's worked example is a *recursive rule that early-exits*, e.g., a count-down via a separate decreasing field, OR a rule that always returns immediately on the first call (degenerate but exercises the ABI).
- **Goal of 5.1a:** ship the call/ret + label-patching + Option-B ABI (rdi=Number value). Exercise the emit infrastructure in isolation. The worked example is intentionally not "real-world useful" — the ABI is what's load-bearing here, not the recursion.
- **No let bindings, no resources, no reactions, no text-let, no concat inside the recursive body** (the Phase 2I/concat-in-recursion forbid-list from section 3.4).
- **No Record / Result / Text returns.**
- **No mutual recursion.**

### Slice 5.1b — inline Record construction at the recursive call site

Builds on 5.1a's ABI. Lifts the "argument is the same input" restriction. Adds the marshalling: caller evaluates a `Record { field: expr, ... }` constructor, lays the fields into the rdi-pointed struct (or Option-B's registers if we keep B for single-field), then calls.

Worked example target for 5.1b: factorial as originally drafted.

```verbose
concept N
  fields:
    v : number [0, 10]

rule fact
  input:  n : N
  output: out : number
  logic:
    out = if n.v == 0 then 1 else n.v * fact(N { v: n.v - 1 })
  proofs:
    purity:
      reads : [n, n.v]
      calls : [fact]
    termination:
      bound : 100
```

After 5.1b:
- `fact` compiles to a real callable: prologue loads `v` from rdi, body evaluates `if/else`, recursive call evaluates `n.v - 1` to rdi, calls `fact`, multiplies the return by n.v, returns.
- A non-recursive caller (`rule entry: out = fact(n)`) emits `mov rdi, [rbp + n.v_slot]; call fact; <rax has result>`. The Call expression's emit dispatches to the "real callable" branch instead of inlining.

### Why split

5.1a is a tight slice with a self-contained ABI deliverable. 5.1b adds one new capability (Record materialization at call site) on top of an already-shipped ABI. Each is reviewable in isolation. If 5.1b's Record marshalling design surfaces issues, 5.1a still stands — the ABI itself has shipped.

If we shipped a single combined slice and discovered a problem with Record materialization, we'd have to unwind both halves together. Splitting protects against that.

### Honesty about 5.1a's worked example

5.1a's recursive rule is necessarily contrived. The "argument is the same input" restriction (section 3.6 catches it) means the recursive call doesn't actually progress. The legal slice-5.1a recursion is something like:

```verbose
rule terminate_immediately
  input:  n : N
  output: out : number
  logic:
    out = if n.v >= 0 then n.v else terminate_immediately(n)
```

The recursive arm is statically unreachable (with `v >= 0` always true for the declared range — the same trap as [[feedback_optimizer_default_field_range]] BUT in our favor here). The compiler emits the real-call path; the runtime never takes it. We compile + run + assert output without ever exercising the recursive frame. The ABI is exercised by the emit machinery and the dead-code path (objdump shows the call/ret), even though it never executes.

This is intentionally degenerate. The point of 5.1a is to ship the ABI without entangling marshalling design. 5.1b ships the first runtime-recursive native binary.

## 5. What slice 5.1 does NOT ship

Each of these is a deliberate later slice:

- **Slice 5.2:** Text return (`(rax, rdx)` ABI).
- **Slice 5.3:** Multi-field input concepts (fields struct of N slots).
- **Slice 5.4:** Mutual recursion (two or more rules in the same cycle).
- **Slice 5.5:** Record return (sret convention with hidden first arg).
- **Slice 5.6:** Result return (tagged tuple ABI).
- **Phase C:** Structural recursion proof in the verifier — `termination: structural : <var>` becomes mechanical.

Each follow-up slice is additive — slice 5.1's ABI stays unchanged, only the supported shapes grow.

## 6. Audit story comparison — the honest version

**Today (inlined, non-recursive):**
- Auditor reads `examples/<rule>.verbose`, sees the rule's `logic`.
- In the native binary, every Call site has the callee's body INLINED at that site. `objdump -d` shows one contiguous .text block per source rule. The 1:1 mapping between source-line and instruction-region holds.
- This is the "small + auditable surface" half of the two-execution-modes story in CLAUDE.md.

**Slice 5.1 (recursive rule → real call, mixed-mode binary):**
- Same `.verbose` source. Compiles natively.
- `objdump -d` now shows a MIX: non-recursive callees are still inlined at their call sites (Phase 2D/2G/2H-b/4.x emitters unchanged), and recursive callees live ONCE at a fixed label with `call <label>` / `ret` at the call site.
- An auditor can no longer say "every call's body is visible at the site." For recursive rules, the auditor follows one extra hop to the callable's label.

**This IS a real concession**, not a wash. The v0 of this doc claimed the real-call story was "actually better for code review" — that overstated it. The honest framing:

- For NON-RECURSIVE rules, inlining stays. The audit story is unchanged.
- For RECURSIVE rules, the audit story takes one extra hop. In exchange, the binary doesn't infinitely expand at compile time (today's stack overflow), and the recursion is mechanical (no compiler trick that smells of "guessing"). The two-modes story in CLAUDE.md tolerates this slight regression specifically because the alternative is "no recursion in native at all," which closes the door to self-hosting.

The trade-off is asymmetric: small audit cost on the recursive rules' call sites (one indirection per call), big capability gain (recursion compiles natively at all). Acceptable, documented.

**Runtime safety signal (recursive rule, stack overflow):**
- `strace` shows `--- SIGSEGV ---` with the offending instruction pointer in the recursive call's body.
- The auditor's debugger can walk the rbp chain to count the active depth.
- This is the same fail-closed posture as every other Verbose runtime bound (substring out-of-range, parse_int bad input, on_read_error: abort): a sys-exit signal that's externally observable, not a silent corruption.

## 7. Risks and open questions

1. **Stack-overflow detection.** Linux delivers SIGSEGV when the stack guard page is touched; the binary exits with status 139 (128 + 11). Is that audit-acceptable as a fail-closed signal? Or do we want a check-then-recurse pattern (e.g., compare rsp against a low-water mark before each recursive call)? **Probably accept SIGSEGV as the signal** — it's the standard Linux contract, and the only alternative is a per-call rsp check that costs cycles and adds noise.

2. **What about the existing `gather_transitive_callee_reads` and friends?** Those walkers assume callee bodies fold into the caller's reads/fetches proof. With a real call, the callee runs in its own context — its reads still need to appear in the caller's `reads:` proof (the auditor wants to see every file touched by the chain). The walkers stay; they just need to follow `Call` edges into recursive callees without infinite-looping. Slice 5.0's cycle detector gives us the helper.

3. **Recursive call materialization of the input record.** For `fact(N { v: n.v - 1 })`, the caller builds a fields struct, passes its pointer. Where does the fields struct live? Two options:
   - (a) On the caller's stack, just below rsp, alive for the duration of the call.
   - (b) In a dedicated rbp-relative slot reserved by the prologue.
   
   Option (a) scales naturally to nested recursive calls (each call grows its own struct). Option (b) needs as many slots as max nesting depth. Default to (a): allocate via `sub rsp, sizeof(fields)`, fill, call, `add rsp, sizeof(fields)` after return.

4. **Interaction with `--stream` and `--stdin`.** The streaming reader prologue (docs/stdin-reader-design.md) sits before the rule code. A callable that's invoked from streaming context inherits the streaming setup automatically — its rdi is set by the caller, its rsp is the streaming-adjusted stack. No special handling needed in slice 5.1 IF the callable doesn't itself touch the streaming machinery (and slice 5.1's scope says it doesn't — no resources, no reactions).

5. **Output: what about Bool encoding?** Today's Number/Bool emit uses rax with `test al, al` for boolean dispatch. The callable's return value in rax must follow the same convention — `0` for false, non-zero for true. No new bookkeeping.

6. **What does `objdump -d` print for the callee?** With ELF symbol info, the callable's label would show up. We currently don't emit ELF symbols beyond `_start`. Adding a symbol per callable is a follow-up — not blocking slice 5.1's emit but worth doing for the audit story.

7. **Recursion depth bound — UNSOUND in the v0 draft, surfaced as a prerequisite slice.** The v0 of this doc claimed "the depth bound is implicit in the input concept's number range (e.g., `v : number [0, 10]` ⇒ at most 11 recursive calls)." Subagent review (point 5) verified against the current `src/native.rs::emit_atoi_inline` that **runtime input fields are NOT bounds-checked against declared ranges**. A user passing `1000000` to a rule whose field is declared `v : number [0, 10]` produces 1000000 recursive calls and SIGSEGV at depth ~10k. The declared range is checked statically against literals; runtime input slips through.

   Two possible responses:
   - (a) Treat this as a prerequisite slice. Before slice 5.1b ships, emit a runtime clamp at field-load when a declared range exists: `mov rax, <parsed value>; cmp rax, <max>; jg .out_of_range_abort` + analogous for min. This is a useful general safety upgrade, not just for slice 5.1 — it surfaces a latent gap in the existing native binaries.
   - (b) Document the gap honestly in the slice 5.1 release notes and CLAUDE.md, and rely on the user's `bound: N` for the recursion-depth claim (which we just admitted is meaningless for recursive rules anyway).

   **Recommendation: (a).** A prerequisite slice "runtime input bounds-check" lands BEFORE slice 5.1b (the runtime-recursive slice). Slice 5.1a doesn't depend on it (5.1a's recursive arm is statically unreachable). The order is: 5.0 (refusal) → 5.1a (ABI) → prerequisite (bounds-check) → 5.1b (runtime-recursive).

## 8. Filter check

Before this design is locked, every pillar must accept it:

- **Verifiability:** PARTIAL — local `bound:` is mechanical; recursion termination is best-effort until Phase C. Document the gap, don't paper over it.
- **Exploitability:** YES — the ABI feeds Phase B (recursive types pass tree pointers) and Phase C (structural recursion uses the same calling convention).
- **Safety:** YES — fail-closed via SIGSEGV on stack overflow, no UB, no silent corruption.
- **Traceability:** YES — one callable per rule, `addr2line` resolves, audit hop is bounded.
- **Readability:** YES — no new syntax; existing `calls:` proof self-references mark recursion.

The Verifiability gap is the most honest weak point. The compromise is acceptable because (a) slice 5.0 already makes the runtime contract explicit ("Phase C will prove termination; until then, trust your bound"), and (b) the user gets a real native binary instead of "use --run for now." Phase C closes the gap when it ships.

## 9. Order of slices

The post-review slice graph:

1. **Slice 5.0** (PR #32, shipped) — refuse recursive rules at compile time with a breadcrumb. Safety floor.
2. **Slice 5.1a** — call/ret + label-patching ABI (Option B: single Number field in rdi). Pass-through recursive arm only (statically unreachable). Mandatory verifier breadcrumb (section 3.6) + "arg provably equal to input" rejection. The recursive body is forbidden from text-let / concat (section 3.4 forbid-list). Worked example is a deliberately degenerate rule that exercises the emit machinery without running the recursive path.
3. **Prerequisite slice — runtime input bounds-check** (section 7 Q7). Field-load clamps input against declared range; sys-exit(1) on out-of-range. Surfaces a latent gap in current native binaries.
4. **Slice 5.1b** — inline Record construction at the recursive call site. First runtime-recursive native binary (factorial worked example).
5. **Slice 5.2** — Text return ((rax, rdx) ABI).
6. **Slice 5.3** — Multi-field input concepts (switch from Option-B-registers to Option-A-pointer ABI).
7. **Slice 5.4** — Mutual recursion (two or more rules in the same cycle).
8. **Slice 5.5** — Record return (sret).
9. **Slice 5.6** — Result return.
10. **Phase C** — Structural recursion proof in the verifier (`termination: structural : <var>` becomes mechanical).

Each follow-up is additive — once the Option-B ABI ships in 5.1a it stays; once Option-A is needed for multi-field (5.3), it becomes the new default and 5.1a's single-field path continues to work as a special case.

## 10. Implementation order — what slice 5.1a actually touches

Concrete file-level plan for 5.1a alone:

- `src/native.rs::compile_native_code` — detect cycles (already done in slice 5.0). Branch: for cyclic rules, dispatch to `emit_recursive_callable` instead of one of the existing emit_*_program functions. The wrapping `_start` calls into the callable once.
- New: `emit_recursive_callable(rule, ...)` — emits the `push rbp / mov rbp, rsp / sub rsp, frame_size / <body> / mov rsp, rbp / pop rbp / ret` shape, with the body using `rdi` as the single Number input value (no field-slot store; load directly from rdi each time, since the slice forbids let bindings).
- `src/native.rs::emit_eval_expr` Call arm — when the callee is the current rule (self-recursive), emit `mov rdi, <value> ; call <label>`. The label is the start of the callable's prologue; rel32 patched.
- `src/verifier.rs` — add the mandatory diagnostic + the "arg provably equal to input" rejection (section 3.6).
- New regression test: compile the degenerate worked example, assert objdump shows a `call` instruction with the right offset, assert the program runs and returns the expected non-recursive arm's output.

Out of scope for 5.1a (explicit deferrals):
- Record construction at call site (5.1b).
- Multi-field input (5.3).
- Text / Record / Result returns (5.2 / 5.5 / 5.6).
- Mutual recursion (5.4).
- Phase 2D/2G/2H-b/4.x interactions with cyclic callees — those stay refused.

## 11. Filter check (post-revision)

- **Verifiability:** PARTIAL — mandatory breadcrumb + "arg-equals-input" check catch the obvious cases. Mechanical proof of recursion termination still waits for Phase C. The honest framing is in section 3.6.
- **Exploitability:** YES — Option B ABI (rdi=value) feeds 5.1b directly; the switch to Option A for multi-field in 5.3 is a clean upgrade.
- **Safety:** YES — fail-closed via SIGSEGV on stack overflow; no UB; runtime bounds-check prerequisite (slice 3 above) closes the silent-input-grow gap.
- **Traceability:** ACCEPTABLE — one extra hop for recursive calls; admitted as a concession in section 6.
- **Readability:** YES — no new syntax; the audit reader sees the same source as before.

This v1 of the doc is the post-review state. Ready for implementation as slice 5.1a, with prerequisite slice (runtime bounds-check) sequenced before slice 5.1b.
