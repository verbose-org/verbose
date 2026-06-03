# Native codegen bug: arena `MatchVariant` binder shadows an input field

Status: **diagnosed, fix proposed, NOT applied.** Confidence: high (root cause
isolated by a single-line source rename that makes native match the interpreter
byte-for-result, on both the minimal mirror and the real `dedents_to`).

Investigated on branch `feat/self-hosting`, with the uncommitted brick-8b work
present in `examples/vexprparse.verbose`. `src/native.rs` and
`examples/vexprparse.verbose` were left byte-identical (md5 verified); the only
new file is this doc.

---

## 1. The confirmed discrepancy (verbatim)

Probe rule appended to `examples/vexprparse.verbose` (entry builds the column
stack with literals, calls `dedents_to`, extracts `count` via `dr_count`):

```
rule probe_dedent_count
  input:  pb : ProbeArg
  output: out : number
  logic:
    out = dr_count(dedents_to(PopArg {
            stk: ColStack::Push { width: 4, rest: ColStack::Push { width: 2, rest: ColStack::Empty } },
            width: 0 }))
```

```
INTERP (--run probe_dedent_count --input [{"seed":0}]):   [0] out = 2
NATIVE (--native ... --run probe_dedent_count, argv "0"):  0
```

Expected: a stack `Push(4, Push(2, Empty))` popped down to width 0 yields a
DedentResult with `count = 2`. The interpreter returns 2; native returns 0.

---

## 2. The minimal reproducer and the trigger

The trigger is **NOT** the 3-field `MkDed(popped, count, err)` variant, **NOT**
the group-typed `popped` field, **NOT** the separate `dedent_bump` transform
rule, and **NOT** the depth of recursion. It is a **name collision between a
`match` arm payload binder and an input record field that is read inside the
same arm**.

Minimal `.verbose` that shows native ≠ interp (single-field result, inline
`+1`, depth-1 recursion is enough):

```verbose
@verbose 0.1.0

concept_group G [max_depth: 4096, max_nodes: 65535]
  @intention: "g"
  @source: vexprparse.intent:27

  concept Stack
    @intention: "stack"
    @source: vexprparse.intent:27
    variants:
      Push of (width : number, rest : Stack)
      Empty

  concept Res
    @intention: "res"
    @source: vexprparse.intent:27
    variants:
      MkRes of (count : number)

concept PArg
  @intention: "arg"
  @source: vexprparse.intent:27
  fields:
    stk : Stack
    width : number [0, 100000]

rule popcount
  @intention: "popcount"
  @source: vexprparse.intent:27
  input:  pa : PArg
  output: out : Res
  logic:
    out = match pa.stk:
      Push(width, rest) => if width > pa.width then Res::MkRes { count: res_count(popcount(PArg { stk: rest, width: pa.width })) + 1 } else Res::MkRes { count: 9 }
      Empty => Res::MkRes { count: 9 }
  proofs:
    purity:
      reads : [pa.stk, pa.width]
      calls : [popcount, res_count]
    termination:
      bound : 4096

rule res_count
  @intention: "count"
  @source: vexprparse.intent:27
  input:  r : Res
  output: out : number
  logic:
    out = match r:
      MkRes(count) => count
  proofs:
    purity:
      reads : [r]
      calls : []
    termination:
      bound : 16

concept ProbeArg
  @intention: "probe"
  @source: vexprparse.intent:27
  fields:
    seed : number [0, 100]

rule probe
  @intention: "probe"
  @source: vexprparse.intent:27
  input:  pb : ProbeArg
  output: out : number
  logic:
    out = res_count(popcount(PArg { stk: Stack::Push { width: 2, rest: Stack::Empty }, width: 0 }))
  proofs:
    purity:
      reads : []
      calls : [res_count, popcount]
    termination:
      bound : 16
```

Base case returns `count = 9`; one recursion adds `+1`, so the correct answer
is `10`.

```
INTERP: [0] out = 10
NATIVE: 9          ← returns the BASE value; the +1 arm never ran
```

**Isolation that pins the trigger** — rename the `Push` binder `width` → `w`
(so it no longer collides with the input field `pa.width`) and change the guard
to `if w > pa.width`. Nothing else changes:

```
INTERP: 10
NATIVE: 10         ← fixed
```

The same one-line rename inside the REAL `dedents_to`
(`Push(width, rest) => if width > pa.width ...` → `Push(w, rest) => if w > pa.width ...`)
flips the original probe from native `0` to native `2` (= interpreter).

### Why the parent's earlier standalone mirror did NOT break

Mirrors built without this collision compile correctly. The collision needs all
of:
1. a **record-concept input** with a **named field** (group-concept inputs have
   no record fields, which is why `sum_chain`/`label_tree`/`token_scan` never
   hit it), and
2. a **`match` arm binder whose name equals that field name**, and
3. the arm body **reads the input field via `<input>.<name>`** (here in the
   recursion guard `width > pa.width`).

`dedents_to` hits all three because its `Push(width, rest)` binder is named
`width` and the guard compares against `pa.width`.

---

## 3. Root-cause localization in `src/native.rs`

The bug is in the **arena `MatchVariant` arm dispatch** (the group-concept path,
guarded by `arena_ctx.is_some()`), inside `emit_eval_expr`.

- `src/native.rs:12145` — `let mut extended_offsets = offsets.clone();`
  The arm starts from a clone of the caller's `offsets` map, which already
  contains the input fields keyed by their **bare field name** (e.g. `width`,
  `stk`).
- `src/native.rs:12209` — `extended_offsets.insert(binder_name.as_str(), binder_cursor);`
  (and the text-binder analogue at `:12191`). Each arm binder is inserted into
  that same map, **keyed by its bare binder name**. When `binder_name == "width"`
  this **overwrites** the input-field entry `width → [rbp-0x10]` with the binder
  slot `width → [rbp-0x18]`.

The two reference forms then collapse onto the same key:

- `src/native.rs:12323-12333` — a bare `Expr::Ident("width")` (the binder
  reference) → `offsets.get("width")`.
- `src/native.rs:10866-10890` — `Expr::Field(Ident("pa"), "width")` (the input
  field access `pa.width`) → also `offsets.get("width")` (the lookup key is the
  bare field name; only `state.field` gets a `__state_` prefix).

So after the binder insert, **both** the binder and `pa.width` resolve to the
binder's slot. In `dedents_to`/`popcount` the guard `width > pa.width` is
emitted as `binder_width > binder_width`, which is always false, so the
recursive arm is never taken and the rule always returns the base-case value.

This corresponds to hypothesis **(iii)** in the brief ("MatchVariant binder
extraction reads the wrong arena offset … when a bound field …"), refined: the
binder extraction itself is correct, but the binder is registered under a key
that **shadows an input field** that the arm body reads.

### Evidence (disassembly of the minimal `popcount`)

Prologue copies the input struct: `stk → [rbp-0x8]`, `width → [rbp-0x10]`.
Arm-0 (`Push`) binds payload: `width → [rbp-0x18]`, `rest → [rbp-0x20]`.
The guard `width > pa.width` emits:

```
mov rax,[rbp-0x18]   ; left  = binder width
push rax
mov rax,[rbp-0x18]   ; right = pa.width  ← WRONG, reads binder width again
pop rcx
cmp rcx,rax
setg al              ; width > width  ⇒  0  (always)
```

`pa.width` should have loaded `[rbp-0x10]`. The guard is therefore always false,
the recursive `MkRes{count+1}` arm is dead, and the base `MkRes{count:9}` arm is
returned.

The rest of the group machinery is sound: with the binder renamed, the recursive
`call`, the `mov rdi, rax` capture of the recursive index, the inlined
`res_count` MatchVariant read, the `+1`, and the fresh `VariantConstruct` of the
outer `MkRes` all produce the correct value end-to-end (verified at depth 1 and
depth 2). The collision is the **sole** defect.

---

## 4. Proposed fix (NOT applied)

### Primary fix — give arm binders a key namespace distinct from field accesses

Size: **small** (one emit site + the two scalar-reference sites; ~10–20 lines).

The interpreter keeps the input record and the match binders in distinct scopes,
so `pa.width` (field) and `width` (binder) never clash. Native must mirror that.
Field accesses always arrive as `Expr::Field(Ident(input), name)`; binder
references always arrive as a bare `Expr::Ident(name)`. So separate their key
spaces in the arm's `extended_offsets`:

1. At the arena binder-insertion site (`src/native.rs:12209`, and the text
   analogue at `:12191` writing into `extended_text_bindings`), insert under a
   reserved key, e.g. `format!("__bind_{}", binder_name)` instead of the bare
   name.
2. In the bare-`Ident` resolution arm (`src/native.rs:12323`), look up
   `__bind_<name>` **first**, then fall back to the bare name (let bindings /
   group-input index). Apply the same precedence in the slice-4a.3 recursive
   `Call`-arg path that evaluates a binder as the next recursion's argument
   (the `other => emit_eval_expr(...)` fallback at `src/native.rs:11487` and the
   `offsets.get(input_name)` pass-through at `:11449`).
3. Leave the `Field` arm (`:10866`) unchanged — `pa.width` keeps resolving to
   the bare field key, which is no longer overwritten.

This makes `pa.width` and a same-named binder coexist, matching the interpreter.

No-regression argument:
- Existing working group examples (`sum_chain` `eval`/`build_chain`,
  `label_tree`, `token_scan` `classify`) have **group-concept inputs with no
  record fields**, so there is no field/binder key to separate; their binder
  references resolve via the new `__bind_` key with identical slot offsets — the
  emitted bytes for those binders are the same loads, just found under a
  different map key. No field-access path in those rules is touched.
- Non-group rules never reach the arena `MatchVariant` path (`arena_ctx` is
  `None`); the Phase-A 4.1 substitution path (`emit_redirect_variant_leaves`,
  `:12284`) is unaffected.
- `state.field` already uses a `__state_` prefix; adding `__bind_` follows the
  same established convention and cannot collide with a user field name (the
  lexer/grammar disallow `__`-leading identifiers in source the same way
  `__state_` relies on).

Recommended guardrail to ship WITH the fix: a targeted regression test that
compiles+runs a record-input recursive rule whose `Push(width, rest)` binder
collides with an input `pa.width` read in the guard, asserting native == interp
(the minimal `popcount` above is exactly this). Without such a test the
collision is invisible — no prior test exercised a binder/field name clash.

### Conservative alternative — refuse the collision (axiom-safe stopgap)

Size: **tiny** (~6 lines, at `src/native.rs:12209`).

If the namespace fix is deferred, at minimum detect the collision at the binder
insertion site (`binder_name` already present in `offsets` as a non-binder input
field that the arm body references) and **refuse** with a breadcrumb
("`match` arm binder `width` shadows input field `pa.width`; rename the binder").
This converts a silent miscompile into a compile-time rejection — consistent
with the compiler axiom (never emit code it cannot prove correct). The cost is
that brick-8b's `dedents_to` would need its `width` binder renamed (e.g. to `w`)
to compile natively until the primary fix lands. Renaming is a pure-source
change and was verified to produce correct native output.

I recommend the **primary fix** (it preserves the natural `dedents_to` spelling
and matches interpreter semantics), with the regression test, and only falling
back to the refusal if the namespace change proves to have an unforeseen
interaction during implementation.

---

## 5. Confidence and residual uncertainty

- **Root cause: high confidence.** A single source rename (binder `width` → `w`)
  flips both the minimal mirror and the real `dedents_to` from native-wrong to
  native-correct, and the disassembly shows the guard's right operand loading
  the binder slot instead of the input-field slot.
- **Fix correctness: medium-high.** The namespace separation matches the
  interpreter's scoping and is local to three emit sites, but it touches the
  bare-`Ident` resolution that several arena/recursive paths funnel through;
  the implementer should grep every `offsets.get(...)` reachable under
  `arena_ctx.is_some()` and confirm each is either a field access (keep bare) or
  a binder/let reference (consult `__bind_` first). The slice-4a.3 recursive
  `Call`-arg path (`:11449`, `:11487`) is the one to watch.
- **Secondary observation (not a bug here):** the unrelated guard at
  `src/native.rs:12104-12107` derives `binder_base` from `min(offsets.values())`.
  After the namespace fix, binder slots will no longer be inserted into the same
  map before this `min` is taken — verify `binder_base` is still computed from
  the input/let slots only (it is, since the `min` runs before the per-arm
  clone+insert), so this remains correct. Worth a glance during implementation.

### Reproduction recipe

```sh
# pristine examples/vexprparse.verbose (brick-8b, uncommitted) must be present
cp examples/vexprparse.verbose examples/_repro.verbose
cat >> examples/_repro.verbose <<'EOF'

concept ProbeArg
  @intention: "probe"
  @source: vexprparse.intent:27
  fields:
    seed : number [0, 100]

rule probe_dedent_count
  @intention: "probe"
  @source: vexprparse.intent:27
  input:  pb : ProbeArg
  output: out : number
  logic:
    out = dr_count(dedents_to(PopArg { stk: ColStack::Push { width: 4, rest: ColStack::Push { width: 2, rest: ColStack::Empty } }, width: 0 }))
  proofs:
    purity:
      reads : []
      calls : [dr_count, dedents_to]
    termination:
      bound : 16
EOF
echo '[{"seed":0}]' > /tmp/probe.json
cargo run --release -- examples/_repro.verbose --run probe_dedent_count --input /tmp/probe.json   # -> 2
cargo run --release -- examples/_repro.verbose --native /tmp/repro --run probe_dedent_count        # build
/tmp/repro 0                                                                                        # -> 0  (BUG)
rm examples/_repro.verbose
```
