# Effects tier slice 2a — reaction + `append_file` (static content) in the self-hosted compiler

REVISED after fresh-context strategic review (2026-07-23), which found one FATAL
(the `\n` escape, empirically confirmed) + five MUST-FIX mis-descriptions against
the code. All corrections folded in. Evidence cited as vexprparse.verbose:line or
native.rs:line. Slice 2 splits into 2a (static content, THIS DOC) and 2b (concat
content, follow-on).

## Goal
A `.verbose` program with a `reaction <name>` whose single `append_file` effect
has a STRING-LITERAL content compiles under gen1 to an ELF whose file effect
matches verbosec `--native`: same appended bytes on the same one-record input,
same exit code. Oracle: `examples/audit_simple.verbose` (trigger `is_suspicious`
= `p.customer_age < 18`; content = literal `"suspicious purchase detected\n"`).

EMPIRICAL ORACLE (verified in review): verbosec appends exactly 29 bytes ending
in `0x0a` for `./a.out 5000 17`; nothing for `./a.out 5000 25` (exit 0 both).

## The FATAL the review caught — content must be UNESCAPED at emit time
verbosec's lexer unescapes the closed set (`\n \t \\ \"`) — `\n` → one byte
`0x0a` (lexer.rs:296). The raw source span between the quotes is TWO bytes
(`\` `n`). So the content write CANNOT be a raw-span blit; it must scan-and-
translate the closed escape set, producing the UNESCAPED bytes. Two consequences
threaded through the whole design:
- The inline content bytes are the UNESCAPED bytes, emitted at COMPILE time by a
  Verbose helper that walks the content span byte-by-byte (byte_at over the
  source — the AstBytes emit precedent reads source bytes at emit time) and emits
  either the byte or its translation.
- `reaction_tramp_size` and the write's `mov edx, imm` use the UNESCAPED length
  (`content_unescaped_len`), NOT `content_len`.
The same unescape applies to the PATH (verbosec unescapes it too via the same
lexer) — apply the one helper to both. Escaped content is therefore SUPPORTED,
not refused (the original doc's refusal contradicted its own oracle).

REUSE (re-review 3): the machinery is LARGELY ALREADY BUILT — `emit_bytes_data`
(21676) reads a source span at emit time and splices decoded bytes into the
emitted Vec; `bytelit_unit_byte` (6415) already decodes `\n`→10 / `\t`→9 /
`\\`→92 / `\"`→34 / `\xNN`; `bytelit_decoded_len` (6436) gives the decoded
length. The content/path decode reuses these on the string-literal span (same
span shape as `b"…"`).
ESCAPE SET (re-review 3, SHOULD-FIX): verbosec's STRING lexer set is
`\n \r \t \\ \"` (lexer.rs:302) — it INCLUDES `\r`→13, which `bytelit_unit_byte`
LACKS (`\r` falls through to 114 = 'r'). Before reuse, ADD a `\r`→13 case (either
into bytelit_unit_byte or a forked string-escape variant). verbosec's string
lexer REJECTS unknown escapes and does NOT accept `\xNN` in a string literal, so
a `\xNN` in reaction content has no verbosec oracle — restrict slice 2a content
to the 5 string escapes (`\n \r \t \\ \"`) and note `\xNN`/unknown as
out-of-scope (unreachable: no verbosec-valid program produces it).
NOTE: the self-hosted emitter does NOT currently unescape text literals in the
streaming path (x86_stream_node AstStr writes raw src bytes, 22023) — no existing
test uses an escape in a streamed literal, so it's a latent gap, not exercised.
This slice adds unescaping ONLY for the reaction path/content (compile-time,
inline). Generalizing it to AstStr is out of scope.

## Why static-content-first
Two cruxes (scoping): (1) entry selection is strictly positional — the first
RuleList rule is the entry (x86_program:22759; elf_program_src reads the head at
23517); reactions aren't parsed at all (parse_program is_skip at 9121 is
concept|concept_group|resource only). (2) Every streamed write hardcodes
`mov rdi, 1` (x86_stream_node 22023, itoa_proc); no fd parameter, no buffer
emitter. Static content sidesteps (2): a bespoke inline write to the open fd
(the x86_stream_node AstBytes idiom, below). Concat content needs an
fd-parameterized writer → slice 2b.

## Runtime shape (verbosec oracle — native.rs emit_reaction_program)
Per record: prologue → eval trigger → gate → `open(O_WRONLY|O_APPEND|O_CREAT,
0644)/write(content)/close`. Reactions ride `ErrorPolicy::Drop`
(native.rs:12503): NO abort on any syscall failure, exit 0 regardless. Slice 2a
scope: ONE record per exec (x86_argv_marshal builds exactly one record — argc
guard 23275; verbosec loops. For `./a.out 5000 17` they agree bit-for-bit;
N-record parity is a documented divergence needing a per-record loop trampoline —
later).

## Parse (mirror parse_resources, 10064)
- New group concepts (beside Resource at 876):
  `Reaction = MkReaction of (name_start, name_len, trigger_start, trigger_len,
  path_start, path_len, content_start, content_len)` — spans; 2b widens content
  to an Ast.  `ReactionList = RxCons(head, tail) | RxNil`.
- `parse_reactions` walks top-level, conses reactions, skips
  rule/concept/concept_group/resource blocks.
- `parse_reaction_decl` mirrors parse_resource_decl (10025): positional read of
  `trigger:` (rule name) then `effects:` → `append_file "<path>" "<content>"`.
  Malformed → `path_len = -1` sentinel. Path/content are quote-stripped raw
  source spans (unescaped at EMIT time, not parse time).
- span recognizers: span_is_reaction / span_is_trigger / span_is_effects /
  span_is_append_file (mirror span_is_resource, 6791). **span_is_append_file must
  full-byte-compare** — `append_toks` is a real self-source rule sharing the
  `append_` prefix (differs at byte 7: `f`=102 vs `t`=116). Pin with a test.
- parse_program is_skip (9121) gains `reaction`. CORRECTED RATIONALE (review #9):
  its real effect is preventing a rule declared AFTER a reaction from being
  dropped (parse_program RNils on an unrecognized top-level keyword, 9126). It is
  NOT about the trigger becoming head. For audit_simple (reaction last) it's a
  no-op, but required for correctness.

## Entry selection — the `has_reaction` override
- `has_reaction` = ReactionList non-empty. A reaction POC has exactly one
  reaction: "the (only) reaction is the entry when present".
- elf_program_src's trampoline cascade (entry_rule_result → _bytes → _collection
  → _texty → number, ~23515) gains a FIRST branch: if has_reaction, emit the
  reaction trampoline INSTEAD of the cascade (replaces it entirely — the head
  rule is NOT also emitted as a standalone number-trampoline; review #6/F).
- ENTRY CONCEPT (CORRECTED, review #5): the marshal + blob_end_off's `msize`
  read the HEAD rule (rl_head_params, 23517/22866), NOT the trigger by name.
  Slice 2a therefore REQUIRES `trigger == head rule` (enforced in
  reaction_errors). audit_simple satisfies this (is_suspicious is the sole rule).
  A trigger that isn't the head → refuse with a breadcrumb (re-driving marshal
  from the trigger's concept is a later lift). The one-record argv marshal
  (x86_argv_marshal, 23254) then runs unchanged on the head/trigger concept.

## Emit — the reaction trampoline (2a)
MODELLED ON THE EXISTING NUMBER TRAMPOLINE: reuse its marshal + `call trigger`
shape, swapping only the TAIL (gate+effect instead of itoa+print). The trigger IS
the head/entry proc.
CALL rel32 (CORRECTED, re-review 1b): NOT byte-identical to the number tramp's
`96` — that constant is exactly the itoa tail that sits between end-of-call and
the first proc (blob_end_off's bool term `101` = 5-byte call + 96-byte tail). The
reaction tail is a different size, so the first proc sits at a different distance.
The call rel32 = `reaction_tramp_size − 5 (+ 86 if anytx)`; a valid trigger has
anytx==0 (it uses no text/number-stream), so rel32 = `reaction_tramp_size − 5`.
Hand-derive it exactly as the `96`/`101` pair is hand-matched (proc_offset is
unavailable at top-level emit). Model reaction_tramp_size on the REAL shared
helper resource_marshal_size (used in both blob_end_off:22875 and the emit) so
the rel32 and the size read from ONE truth.
Sequence after `call trigger` leaves the result in rax:
1. Gate: `test rax, rax ; jz rel32 -> skip`. rel32 to the skip label = a
   compile-time constant from the fixed tail layout. NOTE (re-review 1a): the
   marshalled index pushed by x86_argv_marshal (`push r14`, 23275) is NOT
   consumed by the call — the callee `ret`s plainly (cdecl caller-cleanup,
   21488) and the entry trampoline simply SKIPS cleanup and exits. So the index
   leaks on the stack; harmless in 2a because the tail `sys_exit`s immediately.
   THE FUTURE PER-RECORD LOOP TRAMPOLINE MUST add `add rsp, 8` after the call —
   the leak becomes unbounded stack growth in a loop.
2. open(append): the x86_stream_node AstBytes idiom (22024) for the inline path,
   but with the open-specific shape (re-review 4/5): `jmp +(plen+1)` — the jump
   MUST clear the path bytes AND the trailing NUL, else execution runs into the
   NUL — `; <unescaped path bytes> ; 0x00 ; lea rdi,[rip-(…)]` with the open
   tail's OWN prefix length in the lea disp (NOT the write's `21`) — then
   `mov eax,2 ; mov esi,0x441 ; mov edx,0x1A4 ; syscall ; mov rbx,rax`. Flags
   0x441 (O_WRONLY|O_APPEND|O_CREAT) + mode 0x1A4 (0644) verified against verbosec
   emit_open_append (native.rs:8503). (NOT x86_resource_block, whose open uses
   absolute src_base addressing that needs the source blob embedded — audit_simple
   has uses_text==0 so no blob exists.) NO abort check (Drop policy).
3. write(content) to the open fd: the SAME AstBytes idiom for the inline
   UNESCAPED content — `jmp +clen ; <unescaped content bytes> ; lea rsi,[rip-(…)]`
   — then `mov rdi, rbx ; mov edx, clen ; mov eax, 1 ; syscall`. The delta vs
   x86_stream_node's inline write is exactly `mov rdi, rbx` instead of the
   hardcoded `mov rdi, 1` (review #8). rbx survives (syscall clobbers only
   rax/rcx/r11). NO abort (Drop).
4. close: `mov eax,3 ; mov rdi,rbx ; syscall`. NO abort (Drop).
5. skip: `mov eax,60 ; xor edi,edi ; syscall` (exit 0).
POLICY (CORRECTED, review #7): full Drop — no `js abort` anywhere. On an
unwritable path, open returns -1, write(-1,…)/close(-1) fail silently, exit 0 —
byte+exit-code identical to verbosec. (This is SIMPLER than the original doc: no
abort machinery at all for the effect. Diverges from slice 1's fail-closed read,
deliberately, to match verbosec's reaction semantics.)

## Sizing one-truth + threading (CORRECTED, review #2 + re-review 2)
`reaction_tramp_size(rx)` = `5 (call) + fixed reaction tail bytes +
plen(unescaped) + clen(unescaped)` — a per-program constant over UNESCAPED
lengths. It EXCLUDES the argv marshal: `msize` stays its own separate,
unconditional term in blob_end_off (22870), and the marshal is reused by
RE-EMITTING x86_argv_marshal verbatim (23521), NOT by folding its size into
reaction_tramp_size. Folding msize in while the `+ msize` term stays would make
p_filesz `msize` bytes too large. reaction_tramp_size is used by BOTH the emit
and the size pass (the resource_marshal_size discipline — a REAL shared helper,
used in both blob_end_off:22875 and the emit:23273/23513; model on it).
blob_end_off (22875) is called UNCONDITIONALLY at top level for p_filesz
(23515); its trampoline-variant term for a bool entry is the constant 101. When
has_reaction, `reaction_tramp_size` REPLACES that 101-cascade term (does NOT add;
the `+ msize` term stays). To make blob_end_off compute this it needs
the ReactionList: thread `rxs` into `ProgGenState` (~33 construction sites,
`grep -c 'ProgGenState {'`), the trampoline term becoming
`if has_reaction then reaction_tramp_size else <existing cascade>`. HONEST COST:
this is ConceptList-scale (~33 mechanical mirror edits + the field), NOT "a
handful". The deep byte_at site in x86_node that builds a ProgGenState passes
`RxNil` — harmless because reaction_errors refuses byte_at (and every other
src_base-dependent construct) in the trigger, so that site never fires for a
reaction program. This keeps "no deep src_base site fires" an ENFORCED invariant
(construction, not luck). rxs does NOT need the deeper ByteGenState family (that
was slice 1's ~400-site burden; a reaction is consumed only at the top level +
blob_end_off).

## Verify — reaction_errors (mirror resource_errors, 10932; added to verrs 23490)
1. `trigger` names a real rule (find_rule non-sentinel, 5954) AND is the HEAD
   rule (the marshal-drives-from-head constraint).
2. `append_file` path is a literal (path_len != -1).
3. content is a literal (2a scope — a concat-shaped content REFUSED with a
   breadcrumb naming slice 2b).
4. THREADING-SOUNDNESS INVARIANT: the trigger's transitive logic uses NO
   src_base-dependent construct — text literal (AstStr) / read / byte_at /
   substring / collection / concat / AstBytes / le32 / le64. (Review B: the
   original refusal list missed AstBytes/le32/le64 — include them.) Refuse with a
   breadcrumb ("slice 2a trigger must be arithmetic/boolean").
5. effect must be `append_file` (a `print` effect refused — fd-1 stdout, out of
   scope).
6. exactly one reaction, exactly one effect (POC shape; multi is additive later).
The trigger's purity + termination are ALREADY verified (it's a normal rule in
prog — prog_diags/purity_list/term_list, 23490). Fixed point stays clean BY
CONSTRUCTION (review #10 confirmed): the self-source has zero reaction/trigger
tokens, so parse_reactions → RxNil → reaction_errors returns 0 (base case) and
has_reaction is false → cascade unchanged.

## Eval (oracle split)
N/A for the reaction — it's an entry, not an expression; absent from RuleList; the
interpreter has no reaction path (verbosec's --run reaction is a bespoke driver,
not eval). The trigger is a normal eval-able rule. Parity is on the emitted
binary's FILE effect. No new eval code; pin the "no interpreter oracle for
reactions" split.

## code_size mirror — N/A (review #12)
The reaction trampoline is emitted at top level (elf_program_src), OUTSIDE the
x86_proc/x86_node/code_size_node two-pass. Its internal jumps (gate jz,
jmp-over-data) are compile-time constants from the fixed layout;
`reaction_tramp_size` is the required top-level size mirror (analogue of
resource_marshal_size). The trigger proc is sized by the existing proc_sizes.
State this explicitly so nobody adds a spurious code_size_node arm.

## Traps
- Every new recursive helper (parse_reactions, reaction_errors walks,
  the unescape-and-emit helper, unescaped_len, reaction_tramp_size) mentions its
  recursion AT MOST ONCE (2^N eager-double-mention scar).
- Countdown/length walks use `== 0` base cases, never `< 0`; explicit ranges on
  new bounded span fields compared against 0 (optimizer default-range scar).
- The unescape helper's byte scan advances by 2 on `\` and 1 otherwise — ensure
  the recursion is well-founded (pos strictly increases; guard pos < end).

## Refusals (breadcrumbed, slice 2a)
- concat (non-literal) content → 2b.
- `print` effect → out of scope.
- Dynamic (non-literal) path.
- Trigger whose logic uses any src_base-dependent construct (incl.
  AstBytes/le32/le64).
- trigger != head rule.
- More than one reaction, or a reaction with != 1 effect.
(Escaped chars in path/content are NOW SUPPORTED via the unescape helper — the
original refusal is removed.)

## Gate (clean disk)
1. Proofs check out; suite green; ALL existing binaries byte-identical
   (has_reaction false → cascade emits identical bytes; the rxs=RxNil ProgGenState
   thread must reproduce old bytes exactly — SHA/cmp pin).
2. two_generation gen1==gen2 (self-source declares no reactions) + composite demo.
3. MILESTONE: gen1 compiles audit_simple.verbose → `./a.out 5000 17` appends
   exactly the 29 bytes `suspicious purchase detected\n` (ending 0x0a) to
   /tmp/audit_simple.log == verbosec --native's reaction binary on the same input
   (truncate the file between the two runs; diff bytes + exit code). Non-firing
   `./a.out 5000 25` → nothing appended, exit 0, both compilers.
4. Verify pins: trigger naming a missing rule → diagnostic; trigger != head →
   diagnostic; non-literal path → diagnostic; concat content → refused with the
   2b breadcrumb; src_base-dependent trigger → refused; clean audit_simple → 0.
5. Unescape pins: content `"a\nb"` emits 3 bytes `61 0a 62` (not 4); content
   `"a\rb"` emits `61 0d 62` (the `\r`→13 case the byte-literal decoder lacked).
6. append_toks-vs-append_file collision pin.

## Slice 2b preview (NOT this slice)
Concat content needs dynamic text to the open fd. The streaming path is
fd-1-hardwired, so 2b adds a bespoke reaction-concat writer: RIP-relative inline
UNESCAPED literals + an fd-parameterized inline itoa for number args, each
written to the open fd. (dup2(fd,1) + reuse x86_stream_cargs is rejected — it
re-imports the src_base/appended-source dependency.) Milestone: audit_log.verbose.
