# The fixed point — vexprparse emits vexprparse (attempt + the last codegen gap)

## Goal
Run the self-hosted emitter on its OWN 855 KB source and get an ELF: the fixed
point. Blocked by argv's 128 KB cap, so the driver reads the source via `read()`:

```
rule emit_self
  input: u : EmitUnit          # dummy
  logic:
    let src = read(selfsrc)    # resource -> the file, no size cap
    out = elf_program_src(ScanState { source: src, pos: 0 })
```

`emit_self` verifies, but native codegen refuses with:
`text field 'source' in Record constructor for recursive call to 'elf_program_src':
only input field pass-through, BoundText, and text literals are supported`.

## The gap (minimally isolated, reproducible)
NOT elf_program_src-specific. Minimal repro (`/tmp/min.verbose`): `let s = read(r);
out = lenof(Rec { txt: s, k: 0 })` where `lenof` takes a 2-field record — same error.
Any multi-field-record call whose text field is a text `let`/`read` (BoundText).

Diagnosis (src/native.rs): the code is in `emit_eval_expr`'s multi-field-record call
path (~13313-13377, the struct-marshalling "recursive call" ABI used for ANY
multi-field record arg, not only recursion). Line 13330 ALREADY resolves
`Expr::Ident` when `text_bindings.contains_key(name)` — so the fix is that the
CALLER's text `let`s (esp. `read`-lets) must be IN the `text_bindings` passed to
emit_eval_expr for the caller's body. The callable/record-loop let-prologue stores
these lets in rbp slots but does not register them in that `text_bindings` map at
this call site. Fix = register caller text-`let`s (Read / concat-BoundText / text
field aliases) in the `text_bindings` threaded to the call-arg emit, so 13330 finds
them. Byte-identical for all existing programs (they don't hit this arm today).

## Two outcomes, both valuable
1. **The codegen fix ships regardless** — passing a text `let`/`read` as a
   multi-field-record call argument is a real capability gap (min repro -> 5).
2. **Then attempt the fixed point**: `--native --run emit_self` -> run it -> does it
   emit an ELF for the full 855 KB source? MEASURE peak RSS. Post-dedup the estimate
   is ~5.3M parse + linear codegen; the emitted-ELF arena is 1 GiB (~10M nodes) but
   the RUST-backend `--native` binary (emit_self) has its OWN arena — measure it, and
   if it walls, report the real node peak (the decisive number). If it emits: verify
   the self-emitted ELF RUNS (e.g. it's count_rules-shaped? no — it's the whole
   compiler; at minimum check it's a valid ELF and non-empty).

## Gate
1. min repro (`let s = read(r); lenof(Rec{txt:s})`) -> 5.
2. suite green + byte-identity for existing multi-field-record calls (they don't use
   BoundText text fields today, so unchanged).
3. emit_self attempt: emits an ELF (report size) OR the measured arena peak if it
   walls. Either is a reported result.

## Honest scope
The fix is a targeted codegen slice. The fixed point itself may still hit the arena
(unmeasured at full emission scale) — this attempt MEASURES it. Even if it walls, the
codegen gap is closed and the number scopes the remaining arena work.
