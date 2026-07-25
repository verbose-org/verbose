# Effects tier slice 2b — reaction `append_file` with concat content

REVISED after adversarial review (2026-07-25): both runtime facts VERIFIED
against the code; the entry_size global-byte-identity concern is NOT real (group
max payload = 12, uniquely MkRule at :643; MkReaction moves 8→7 under the
replace; and emitted binaries derive entry_size from the COMPILED program's
concepts — max_payload_fields(at.concepts) :24058 — never from vexprparse's own
group). All amendments folded in below.

Extends slice 2a (PR #127: reaction + static-content append_file). Evidence as
vexprparse.verbose:line at main `50469fb`. Oracle: `examples/audit_log.verbose` —
trigger `is_suspicious` (`p.customer_age < 18`), content
`concat("suspicious purchase: amount=", p.amount, " age=", p.customer_age, "\n")`.

## Goal
gen1 compiles audit_log.verbose; `./a.out 5000 17` appends exactly
`suspicious purchase: amount=5000 age=17\n` to /tmp/audit.log == verbosec
`--native --run audit_suspicious` on the same input (truncate between runs; diff
bytes + exit code). Non-firing input appends nothing, exit 0 both.

## What 2a already gives us (shipped shape, :23615)
The reaction trampoline: `call trigger` (rel32 = reaction_tramp_size − 5 +
anytx?86) → gate `test rax,rax; jz exit` → ONE inline content block
(jmp-over-data of the unescaped literal + `write(rbx, data, clen)`) → close →
exit 0. fd in rbx; syscalls clobber only rax/rcx/r11 so rbx survives. Path and
content are unescaped at COMPILE time via `bytelit_decoded_len` /
`emit_bytes_data` (with the `\r`→13 case added in 2a).

## The two runtime facts 2b builds on (verify in review)
1. **The record index is at `[rsp]` throughout the effect sequence.** The argv
   marshal ends `push r14 / inc r14` (:24046 tail — pushes the node index);
   `call trigger` pushes/pops only the return address; the entry trampoline never
   does caller-cleanup (the 2a review established the arg "leaks" — harmless
   because the tail exits). The 2a effect bytes contain NO stack ops (jmp / lea /
   mov / syscall only, :23630). So at effect time `mov rax, [rsp]` yields the
   record's arena index, and `node_addr = r15 + idx*entry_size`, field i at
   `[node_addr + 8 + 8*i]` — the standard record layout.
2. **The itoa template exists**: the number trampoline's 96-byte tail (the
   `\x48\x83\xec\x20...` block in elf_program_src) converts rax → decimal
   backward into a 32-byte stack buffer, handles negatives, writes to fd 1, and
   exits. 2b needs a NO-EXIT, NO-NEWLINE, fd=rbx variant (the oracle's `\n`
   lives in the last literal arg, not in the itoa).

## Parse — content becomes an Ast
`MkReaction` (:928) today carries `content_start/content_len` spans (8 number
fields). Widen: replace the two content span fields with **`content : Ast`**.
`parse_reaction_decl` calls the EXISTING expression parser at the token after
the path string → `Parsed { node, next }`:
- a bare string literal parses as `AstStr` (token span INCLUDES quotes — the
  emitter uses start+1 / len−2, the established AstStr discipline),
- `concat(...)` parses as `AstCall(concat-span, args)`.
All MkReaction accessors/matchers update (rx_content_start/rx_content_len become
span reads on the AstStr case or disappear into the emit walk). **2a
byte-compat**: an AstStr content must emit byte-for-byte what 2a's span path
emitted — pinned by the existing `self_hosted_reaction_matches_verbosec` +
unescape pins re-passing unchanged.

## Emit — per-arg content blocks (replaces the single content block)
For content = AstStr: one block, exactly 2a's (byte-compat).
For content = concat(args): emit per arg, in order:
- **AstStr arg** → 2a's literal block shape with 2a's EXACT ENCODINGS (review
  MUST-FIX: the disp arithmetic only holds for these): `e9` jmp (never `eb`),
  `<unescaped bytes, 4-padded>`, then `48 c7 c0 01 00 00 00` (mov rax,1 — the
  7-BYTE form, not the 5-byte b8 form), `48 89 df` (mov rdi,rbx), `48 8d 35
  le32(-(block+17))` (lea rsi — 17 = 7+3+7), `48 c7 c2 le32(dlen)`, `0f 05`.
  (No NUL — that's path-only. dlen = decoded length, write length = dlen.)
- **AstField(AstVar(input), f) with f : number** → the field-itoa block,
  per-field constant **102 B** = 21 (load) + 81 (K_itoa):
  ```
  48 8b 04 24                 ; mov rax,[rsp]  — record arena index (fact 1)
  48 69 c0 imm32              ; imul rax, entry_size = 8*(max_payload+1)
  4c 01 f8                    ; add rax, r15
  48 8b 80 disp32             ; mov rax,[rax + 8+8*i] — FIXED disp32 form
                              ; (house form, ~:21974; keeps the block size
                              ; independent of i so the size walker stays
                              ; concepts-free)
  <K_itoa = 81 B>             ; base on the 86-byte itoa_proc (review: it is
                              ; ALREADY no-newline and balanced — only two
                              ; deltas: mov rdi,1 (7B) -> 48 89 df mov rdi,rbx
                              ; (3B), and drop the ret; keep sub/add rsp,0x20
                              ; balanced so [rsp] is the index again after)
  ```
  Assert K_itoa = 81 and the per-field 102 with the script-check discipline.
  During the itoa the scratch occupies [rsp..rsp+0x20); all stores are below
  rsp+0x20 (first at rsp+0x1f), so the index slot (old [rsp], now [rsp+0x20])
  is never touched.
Close + exit-0 tail unchanged. gate_rel becomes `fixed + Σ per-arg block sizes`.
Single-AstStr contents reproduce 2a's `96 + P + C` arithmetic exactly under the
per-arg scheme (5+P+27 open, 5+C+26 arg, 10 close — verified in review §15).

## Sizing one-truth
`reaction_tramp_size` (:23572) becomes content-shape-dependent: fixed prefix +
Σ per-arg (literal → 4-padded block + write-seq constant; field → the constant
102) + close/exit. ONE new recursive walker over the concat args (e.g.
`rx_content_blocks_size(args)`) used by BOTH reaction_tramp_size and
x86_reaction_tramp — the argv_marshal_size discipline. Mentions-once (2^N trap).
BASE CASES: use the TOLERANT form (safe on `len <= 0`), NOT strict `== 0` — the
RxNil sentinel path runs these walkers under EAGER LETS even when n == 0 (the
2a lets at :23580-23586 survive only because bytelit_decoded_len tolerates
len <= 0, :6489; the new walkers inherit that obligation). blob_end_off consumes
reaction_tramp_size already (2a) — no new wiring. The fixed disp32 field-load
form keeps RxSizeState (:23555) UNWIDENED (rxs + src only — no concepts needed
to size).

## Sentinel / malformed-content migration (review MUST-FIX)
Three consumers of the dying content_start/len fields need a coordinated story:
- `rx_head`'s RxNil arm (:10592) constructed `content_len: -1` — the Ast-typed
  sentinel becomes **`Ast::AstErr`** (the parse_primary fallback shape, :3855).
- `parse_reaction_decl` (:10677) signaled malformed via `content_len: -1` —
  malformed signaling consolidates onto **`path_len: -1`** (and/or AstErr
  content); parse_or runs under eager lets even on malformed decls — safe, its
  fallback returns AstErr without advancing.
- `reaction_errors`' `content_ok` check (:11624) becomes the Ast shape-walk and
  MUST refuse `AstErr` explicitly — else malformed content silently passes.
Full consumer list of content_start/len (all must migrate in lockstep):
accessors :10521-10548, constructor :10677, sentinel :10592, reaction_errors
:11624+:11630, reaction_tramp_size :23582-23583+:23590, x86_reaction_tramp
:23624-23625+:23634. Nothing else (verified).

## State-concept widenings (review §19 — budgeted)
- `RxTrampState` (:23599: rxs/src/anytx) gains `concepts` + the entry concept
  index (to resolve field name → index and entry_size). Call site :24320 has
  concepts/prog/ecidx in scope — local edit + purity list.
- `RxErrState` (:11585: prog/rxs/src) gains `concepts` (for the "f is a NUMBER
  field" check). Call site :24285 has concepts0 in scope.
- `RxSizeState`: NO widening (fixed disp32 keeps sizing concepts-free).

## Verify — reaction_errors widens
Content accepted iff: AstStr, OR AstCall(concat) whose every arg is AstStr or
AstField(AstVar(<trigger's input name>), f) with f a NUMBER field of the
trigger's input concept. Refused (breadcrumbed): nested concat, TEXT fields
(runtime strlen — a later slice), rule calls, anything else. Everything else
from 2a stands: trigger == head, exactly one reaction/effect, no
src_base-dependent trigger logic (unchanged — content literals are emitted
inline, and field reads go through the arena record, so 2b adds NO src_base
dependence).

## Eval
Unchanged: reactions have no interpreter path (2a pin). The content Ast is never
eval'd.

## Traps checklist
- The new size/emit walkers mention their recursion ONCE (2^N). (Verified: the
  block walker runs twice per compile — size + gate_rel — linear per arg.)
- TOLERANT base cases (len <= 0 safe), per the sentinel-under-eager-lets note
  above; explicit ranges on any new bounded fields (optimizer trap).
- The itoa block must be byte-identical between emit and size (assert 81/102
  with the script check discipline).
- entry_size and field index i are compile-time constants from the trigger's
  input concept (same resolution the marshal already does).
- **verify↔emit acceptance parity** (third-consumer drift): reaction_errors'
  shape-walk must accept EXACTLY the shapes the emit walker handles and nothing
  more — pin with refusal tests both ways (a shape verify accepts but emit
  int3s = the collections-tier scar).

## Documented divergences (inherited + introduced)
- FACT 1's safety has an UNSTATED PRECONDITION now stated: x86_resource_marshal
  is emitted BETWEEN the marshal's push and the call (:24320) — vacuously safe
  today because reaction_errors check 4 refuses any src_base-dependent trigger
  (read( trips ast_uses_src_base :11461) and unreferenced resources size to 0.
  A future "resources + reactions" lift MUST revisit FACT 1.
- verbosec builds ONE buffer and issues ONE write (emit_append_write_to_r15
  Concat arm, native.rs:7877-7895); gen1 issues N writes. File bytes identical
  on the happy path (single process, O_APPEND, sequential). Divergences: under
  Drop policy a mid-sequence failure (e.g. ENOSPC after arg 1) leaves a content
  PREFIX under gen1 but not necessarily under verbosec (both exit 0); strace
  differs (N+2 vs 3 syscalls). Neither breaks the stated gate; documented like
  the N-record divergence.

## Gate (clean disk)
1. Proofs check out; suite green (expect 476); existing binaries byte-identical.
2. two_generation fixed point + composite demo green.
3. 2a byte-compat: `self_hosted_reaction_matches_verbosec` + unescape pins pass
   UNCHANGED (AstStr content path reproduces 2a's bytes exactly).
4. MILESTONE: gen1 compiles audit_log.verbose → `./a.out 5000 17` appends
   exactly `suspicious purchase: amount=5000 age=17\n` == verbosec's binary;
   `./a.out 5000 25` → nothing, exit 0 both. Edge probes: amount=0 (single
   digit), a two-literal-adjacent shape, a content ending in a field (no
   trailing literal).
5. Verify pins: text-field arg refused; nested concat refused; call arg refused;
   clean audit_log → 0 diags.

## Not in this slice
Text-typed field args (runtime strlen + copy — needs a length-at-runtime write,
different sizing model); multiple effects; print effect; non-head triggers.
