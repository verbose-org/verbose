# Phase B slice 4a — scope memo

**Status:** design proposal, no implementation yet. Drafted by a fresh-context subagent reviewing `docs/recursive-types-design.md`, the Phase A native lowerings, and the existing B.1/B.3 work. The memo proposes the smallest useful B.4 slice that lifts the current `concept_group` refusal at `src/native.rs:127-142`. Three concrete pushback points against the design doc §6 are flagged at the end.

**Target binary:** `examples/sum_chain.verbose::sum_seed` compiles natively with byte-for-byte same observable output as `--run` (produces 0, 6, 15, 55 for n ∈ {0, 3, 5, 10}). Estimated binary size ~1.4–1.8 KB; full slice scope ~5–8 PRs (comparable to Phase A slice 5.1a).

---

## 1. Scope

**Accept:**
- Programs containing exactly one `concept_group` of exactly **one** sum-type concept.
- Variants carry **only** Number-typed payload fields and/or self-references to the same concept (`lhs : Expr`).
- Up to 2 recursive payload fields per variant (covers `Add(lhs, rhs)`-shape trees; lifts later).
- Rules whose input OR output is the group concept compile as recursive callables via the existing Phase A 5.x machinery.
- Number-output rules that pattern-match on the group concept compile (`MatchVariant` in a callable context).

**Refuse with breadcrumbs pointing at the right follow-up slice:**

| Refused shape | Breadcrumb / follow-up slice |
|---|---|
| Multi-concept group | `B.4b — mutual recursion in arena` |
| Text payload in variants | `B.4c — text payload needs (ptr, len) in arena slot` |
| `collection<Expr>` payload | `B.4d — collections in variants` (verifier already refuses since B.1) |
| Variant arity > 2 self-refs or > 2 Number fields | `B.4-followup — arity > 2` |
| `concat` / text-let in recursive body | Already refused (5.1a forbid-list) |

**Worked-example check.** sum_chain.verbose hits every accepted shape and no refused shape:

- `concept_group AST [max_depth: 30, max_nodes: 100]` with one concept `Expr`. ✓
- Variants: `Int(value: number)` and `Add(lhs: Expr, rhs: Expr)` — Number + self-refs only, arity ≤ 2. ✓
- `build_chain(s: Seed) → Expr` — self-recursive constructor. ✓
- `eval(e: Expr) → number` — self-recursive `match`. ✓
- `sum_seed(s: Seed) → number = eval(build_chain(s))` — non-recursive composition. ✓ via existing inlining.

**Verdict:** sum_chain compiles under this B.4a scope. NOT a stepping stone — the first .verbose binary that builds and walks a runtime tree.

## 2. Arena layout

- **Entry size:** `max(1 + 8 * arity)` over all variants, rounded up to 8-byte alignment. For sum_chain: `max(1+8 for Int, 1+8+8 for Add) = 17` → pad to **24 bytes**. Int leaves offset 16 unused — accepted per design doc §9 Q3.
- **Index width:** 16-bit (B.1's `max_nodes ≤ 65535` ceiling). Indices ride in 8-byte registers/slots zero-extended. No bit-packing — extra cost, no audit value.
- **Where it lives:** in the `_start` callable's rbp frame, NOT in each recursive callee. `emit_record_loop_prologue` reserves `arena_size = max_nodes * entry_size` after the existing field/let slots. For sum_chain at `max_nodes=100`: 2400 bytes.
- **Bounds check** before each `VariantConstruct`: `mov rax, [rbp + node_count_slot] ; cmp rax, max_nodes_imm ; jae .arena_full_<group>_abort`. Shared per-group abort label jumps to a single `mov rax, 60 ; mov rdi, 1 ; syscall` (~10 B + 8 B per check site). Mirrors slice-9.1 `on_read_error: abort`.
- **`max_nodes` exhaustion** = `sys_exit(1)`. Audit story stronger than 5.1a's stack-overflow story: mechanically enforced, attributed via `addr2line` on the per-group abort label.

## 3. Register convention extension

Add one row to CLAUDE.md's register table:

| Register | Used by | Introduced |
|---|---|---|
| `r11` | arena_base, persistent across recursive callables in a concept_group rule | Phase B slice 4a |

`r11` is currently unused across Phase A's recursive emit machinery (`grep "r11" src/native.rs` — confirmed safe). Set up once at `_start` prologue via `lea r11, [rbp + arena_slot]`; survives across `call`/`ret` (System V caller-saved but Verbose's recursive callables don't clobber it). Pin via a regression test that exercises r11-survival across the recursive call boundary.

**`node_count`** lives in a dedicated rbp slot (`node_count_slot`), materialized into a register only briefly around each `VariantConstruct`. Cannot live in a register across `call` boundaries.

## 4. VariantConstruct emit shape

For `Expr::Add { lhs: <e1>, rhs: <e2> }` where `<e1>`/`<e2>` evaluate to indices:

```
; Subterms first — depth-first traversal; sub-indices spill to tmp slots
<emit e1>  →  rax = idx_lhs
mov [rbp - tmp_lhs_slot], rax
<emit e2>  →  rax = idx_rhs
; Bounds check
mov rax, [rbp - node_count_slot]
cmp rax, max_nodes_imm
jae .arena_full_AST_abort
; Compute entry address = arena_base + idx * entry_size
mov rcx, rax
imul rcx, entry_size_imm       ; 7B (imul r64, imm32)
add rcx, r11                   ; 3B
; Write tag
mov byte [rcx], TAG_Add        ; 3B
; Write payload slots
mov rdx, [rbp - tmp_lhs_slot]
mov [rcx + 8], rdx             ; 4B
mov rdx, [rbp - tmp_rhs_slot]  ; (rhs also spilled if not last)
mov [rcx + 16], rdx            ; 4B
; Bump count — rax already holds the index to return
inc qword [rbp - node_count_slot]   ; 5B
```

**Total per site:** ~35–50 B depending on subterm shapes. Push back on design doc's ~25 B estimate — the bounds-check + per-payload field-write are honest. Pin at ~40 B in the evolution table.

**Subterm tmp slot pool:** fixed-size, `max_variant_arity = 2` for sum_chain → 16 B of tmp slots reserved at prologue. NOT a recursion stack — each `VariantConstruct` finishes before its parent emits.

## 5. Pattern match emit shape

For `match e: Int(value) => value ; Add(lhs, rhs) => eval(lhs) + eval(rhs)`:

```
mov rax, [rbp + e_slot]        ; rax = idx (from rdi spill)
imul rax, entry_size_imm
add rax, r11                   ; rax = entry pointer
movzx rcx, byte [rax]          ; tag
cmp rcx, TAG_Int
jne .next_arm_1
  mov rdx, [rax + 8]
  mov [rbp - value_binder_slot], rdx
  <body using value_binder_slot>
  jmp .match_end
.next_arm_1:
cmp rcx, TAG_Add
jne .match_trap                ; exhaustive — should be unreachable
  mov rdx, [rax + 8]
  mov [rbp - lhs_binder_slot], rdx
  mov rdx, [rax + 16]
  mov [rbp - rhs_binder_slot], rdx
  <body — `eval(lhs)` loads lhs_binder_slot into rdi and calls eval>
  jmp .match_end
.match_trap:
  mov rax, 60 ; mov rdi, 2 ; syscall   ; tag corruption — exit 2
.match_end:
```

**Binder semantics — load-bearing observation:** payload slot of a Number field holds the Number; payload slot of a recursive field holds the 16-bit INDEX zero-extended. So `eval(lhs)` evaluates as `mov rdi, [rbp - lhs_binder_slot] ; call eval` — **using exactly Phase A 5.1a's single-Number-field rdi=value ABI, because an index IS a Number.** No new ABI work for recursive calls in B.4a.

**Per match site:** ~30 B dispatch + ~25 B per arm. Two-arm match: ~80 B scaffold.

## 6. Recursive call ABI

YES — reuse Phase A's rdi=i64 ABI. `eval(lhs)` passes an index in rdi; the callable's prologue spills rdi to `e_slot` then dispatches via match. `build_chain(Seed { n: s.n - 1 })` follows Phase A 5.1b (single-Number-field Record at call site, materialized into rdi).

`r11` must survive across calls — already true for current emit (it doesn't touch r11). Pin via regression test.

## 7. Stack budget

For sum_chain `[max_depth: 30, max_nodes: 100]`:
- Arena: `100 * 24 = 2400 B` (one allocation at parent's prologue)
- Per-frame: ~64 B (single-field + match binders + tmp slots) × 30 = `1920 B`
- **Total: ~4.3 KB** — Linux 8 MiB stack → 99.95% headroom.

Aggressive case `[max_depth: 1000, max_nodes: 65535]` with 24 B entries: 1.5 MiB arena + 64 KB stack = ~1.6 MiB. Within budget but noticeable. **Recommend verifier emits a stderr breadcrumb when `arena_size + max_depth * estimated_frame_size > 4 MiB`** so the operator knows. Cheap; no new verifier capability.

## 8. Audit story

`strace ./sum_chain_binary < input.json` shows the normal accept/read/exit pattern plus `exit_group(1)` on `max_nodes` exhaustion. `addr2line` against the per-group abort label resolves to the group declaration line. SIGSEGV on arena overrun is impossible by construction (bounds-check fires before write).

B.4a's audit story is **stronger** than 5.1a's: the `max_nodes` bound is mechanically enforced; the `max_depth` bound stays soft (relies on input shape — Phase C will mechanize it).

## 9. Two-pass emit

**Already in place.** `emit_self_recursive_program` (native.rs:2659-2682) does two-pass label resolution for slice 5.4 mutual recursion. B.4a piggybacks: arena base register (r11) is set up once at parent prologue; node_count_slot is rbp-relative (no forward refs); match-arm jump-table patches are local (same as if/else's existing forward patches). **No new pass needed.**

## 10. What B.4a does NOT ship

- Multi-concept groups (mutual type recursion) → B.4b
- Text payloads in variants → B.4c
- `collection<Expr>` payloads → B.4d
- Structural recursion proof → Phase C
- Cross-rule arena lifetime → Q11 in the design doc (single-rule-invocation arena lifetime is sufficient for sum_chain since `sum_seed` evaluates `build_chain` then `eval` within one top-level invocation)

## 11. Three pushbacks against design doc §6

1. **Inc/imul ordering.** Doc shows `mov rax, [node_count_slot] ; cmp ; jae ; inc qword [node_count_slot] ; imul rax, max_variant_size ; add rax, arena_base`. The `inc` must happen AFTER the `imul` — otherwise rax holds the post-inc index. Either reorder OR explicitly save the pre-inc value to a register before incrementing.

2. **`arena_base` doesn't need an rbp slot.** Keep it in r11. Cuts one load per VariantConstruct + match dispatch (which is at least 4 per recursive eval frame). Documented in the register table.

3. **Return INDEX, not pointer.** Doc says "rax = pointer to this node; caller uses (rax - arena_base) / entry_size as the index" — two arithmetic ops per use. Cleaner: VariantConstruct returns the INDEX in rax (post-cmp, pre-inc result). Callers pass the index directly via Phase A's rdi=i64 ABI to recursive calls. The pointer is recomputed inside the callee from `r11 + idx * entry_size` exactly when needed. ONE imul + add per pattern match instead of TWO arithmetic ops per use.

## 12. Implementation slicing (estimated ~5–8 PRs)

Suggested merge order:
1. **B.4a.1** — per-group abort label + `r11`/`node_count_slot` allocation in `emit_record_loop_prologue` (gated on "this rule's transitive Calls touch a concept_group"). Smallest reviewable wedge. No new emit yet; abort label sits unused.
2. **B.4a.2** — VariantConstruct emit. Lifts the concept_group refusal; rules can construct values; pattern match still refuses.
3. **B.4a.3** — MatchVariant emit on arena indices. The two come together if you prefer fewer PRs.
4. **B.4a.4** — wire `r11` survival across recursive `call`s; regression test pinning the convention.
5. **B.4a.5** — runtime regression test against `sum_chain.verbose::sum_seed` + golden output check + binary-size check.

Pin sum_chain native runtime as the slice's load-bearing test.

---

## Reviewer recommendation

**Proceed to B.4a implementation under this scope.** The three pushbacks above against doc §6 are non-trivial — apply them. The sum_chain target is concrete enough that the slice's "done" criterion is unambiguous. Single-concept restriction is the right call; multi-concept (B.4b) needs its own design pass for tag namespace and arena partitioning.

Next concrete next step: open a follow-up PR with `B.4a.1` (the smallest wedge above) — the per-group abort label + arena/node_count slot allocation. No new emit yet; this just sets up the frame layout for subsequent PRs to plug VariantConstruct into.
