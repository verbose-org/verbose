# Self-hosted HTTP service — slice 5a: per-request `log:` block (static content)

S5 of the capstone, the first COMPOSITION slice: wire the reaction tier's
`append_file` machinery into the service accept loop so the server appends a line
per request. Evidence: NR=native.rs, VP=vexprparse.verbose, main `6b3776d`.
Oracle shape: `log: append_file "/tmp/x.log" "hit\n"` on a service.

## The register hazard — RESOLVED: fd in r15 (not rbx)
The reaction's append_file parks its fd in **rbx** (VP:26665, `mov rbx, rax` after
open; `mov rdi, rbx` at write/close) — chosen because in a REACTION all of
r12-r15 are taken. But the SERVICE tramp has used **rbx = the parsed field ptr**
since S2 (field-select `mov rbx, rsi`, VP:27978; S4's body write is
`write(r13, rbx, r14)`). Naive reuse would clobber the field mid-request.
**Use r15.** Byte-scan of `x86_service_tramp` proves r15 is unreferenced in the
accept loop: zero hits for `49 89 c7` (mov r15,rax), `4c 89 f8`/`4c 89 ff` (r15
as source), `4c 01 f8` (add rax,r15), `4d 85 ff` (test r15,r15). r15 IS written
once at startup by the arena prologue (`mov r15, rax` = the mmap base, VP:28428 —
`uses_var` is 1 for every service because concepts_append_http adds field-bearing
concepts), but **the accept loop never allocates an arena node** (the handler is
resolved at compile time), and the rule procs that do use r15 are dead code in a
service ELF. The tramp already clobbers r14 (the node counter) for the same reason,
since S2.
Two further wins: it MATCHES VERBOSEC (emit_append_file_call parks the fd in r15,
NR:7796/7864/7889, and deliberately does not save/restore it, NR:19348), and the
swap is **byte-length-neutral** — `mov r15,rax` (49 89 c7) and `mov rbx,rax`
(48 89 c3) are both 3 B; `mov rdi,r15` (4c 89 ff) and `mov rdi,rbx` (48 89 df)
both 3 B — so **every reaction sizing mirror transfers with ZERO edits**
(rx_lit_block_size VP:26242, rx_carg_size VP:26279, rx_cargs_size VP:26333,
rx_content_blocks_size VP:26359).
**NEW INVARIANT to document (CLAUDE.md register table, beside the r11 note),
stated precisely (review NOTE 9):** *the service tramp never transfers control to
any emitted proc — the accept loop is closed (every branch tail is `e9
<negative>`) and the tramp contains ZERO `e8` call bytes.* That is the real
load-bearing property (broader than "never allocates a node"). r14 (the arena
node counter) is ALREADY clobbered by S2's field-select (`49 89 d6`/`49 29 f6`),
so the arena is already unusable in a service ELF; r15 is the second casualty of
the same fact, not a new one. This breaks the day a service tramp calls an
emitted proc.
Rejected alternatives: (a) "log after the response write" fixes nothing (rbx/r14
are live the whole iteration either way); (c) push/pop rbx costs 2 B AND destroys
the ability to reference the parsed field in log content — a dead end for S5b.

## Placement — POST-ACCEPT, BEFORE THE READ (review FATAL: not after the read)
**FATAL as first drafted**: emitting between the `read` and the fork would break
S2/S3/S4 100% of the time. `rax` is LIVE across that point — it carries the
`read()` byte count and IS the parse loop's remaining-length counter (`test rax,
rax ; je parse-fail`, then `dec rax` per byte, VP:28066). The log block ends with
`close()`, which returns **0 in rax** → ZF set → the `je` fires → every request
takes the parse-fail path and gets NO response bytes. And S1 is immune (no parse;
`rax` is dead there) — so **the S1-only milestone would have gone GREEN on a
broken compiler** (the "milestone diversity is coverage" scar).
**CORRECTED PLACEMENT (zero-cost)**: emit the block between `mov r13, rax`
(post-`accept`) and the `read` sequence. Register liveness at that insertion
point — LIVE: `rsp`, `r12` (listen fd), `r13` (client fd); DEAD: `rax` (about to
be zeroed by `48 31 c0` for sys_read), `rbx`, `rsi`, `rdi`, `rdx`, `r14`, `r15`.
`rdi`/`rsi`/`rdx` are all re-established by the read sequence. Every back-jump
delta is unchanged (`+logsz` either way — still inside the accept_top→loop-end
span) and `logsz` is unchanged. (Rejected: `push rax`/`pop rax` around the block
— +2 B and makes logsz `44 + …`.) Semantic effect: we log on ACCEPT rather than
after the client's first write — indistinguishable for S5a's static content and
consistent with the "logs per accepted connection" divergence below.
The block is COMMON to all four branches (S1 blob / S2 field / S3 router / S4
concat). Every forward jump measures from the PARSE onward, so the block leaves
them untouched:
| branch | jumps affected |
|---|---|
| S1 | back-jmp `-(87+block)` → `-(87+block+logsz)` |
| S2 | **baked** 0x18f/0x169 parse-fail jumps UNCHANGED (no de-baking!); back-jmp -470 → -(470+logsz) |
| S3 | jump1..jump5 UNCHANGED; jump6 += logsz |
| S4 | jump1_4/jump2_4 UNCHANGED; jump6_4 += logsz |
**Total delta: 4 back-jump constants + one additive size term.** (Verbosec's
placement — between field-select and serialize — would grow 10 constants AND
force de-baking S2's two hardcoded je targets.)
**DOCUMENTED DIVERGENCE**: this logs per ACCEPTED CONNECTION, including malformed
requests. Verbosec does NOT log dropped requests (all three parse-fail patches
resolve to close_label AFTER the log emit, NR:19445 — a genuine audit gap on
verbosec's side). For static content, logging every accepted connection is
arguably the more complete audit posture. State it; don't hide it. S5b's move to
post-field-select restores verbosec parity automatically.

## Emit — reuse the reaction block with the fd swapped
Three new r15-flavored rules (Verbose has no register parameterization):
`x86_svc_log_lit_block` / `x86_svc_log_carg` / `x86_svc_log_cargs`, each a copy of
`x86_rx_lit_block` (VP:26442) / the carg / cargs rules with `48 89 df` → `4c 89 ff`
and `48 89 c3` → `49 89 c7`. Sequence (the reaction shape, VP:26665):
```
e9 le32(path_block) ; <NUL-terminated 4-padded path bytes> ; lea rdi,[rip-(path_block+7)]
mov eax,2 ; mov esi,0x441 ; mov edx,0x1A4 ; syscall     ; open(O_WRONLY|O_CREAT|O_APPEND,0644)
mov r15, rax                                            ; fd (49 89 c7)
<content literal block: e9 le32(cblock) ; bytes ; lea rsi,[rip-(cblock+17)] ;
 mov eax,1 ; mov rdi,r15 ; mov edx,dlen ; syscall>      ; write(fd, lit, dlen)
mov eax,3 ; mov rdi,r15 ; syscall                       ; close
```
NO syscall return checks (drop policy = the reaction default; `on_error: abort`
needs a sys_exit(1) tail the tramp lacks — S5c).
**The field-arg arm must NOT be the reaction's** (S5b note): `x86_rx_carg`'s
AstField arm (VP:26517) loads from an ARENA RECORD (`mov rax,[rsp]` → index →
imul → add r15 → slot); a service has NO marshal (has_input forced 0, VP:26702) —
no pushed index, no record. The correct arm is `x86_svc_carg`'s
`write(fd, rbx, r14)` (VP:27829, 18 B) with fd swapped. S5a is literal-only, so
this only matters for S5b — but write it down now.

## Sizing
**FOUR INSERTIONS, NOT ONE (review NOTE 7)**: the tramp's shared prefix is inlined
VERBATIM per branch (VP:28060-28062 — "no shared bytes-let, a concat-valued let is
refused inside this recursive SCC"), so `x86_svc_log_block(...)` must be spliced
into ALL FOUR `concat(...)` arms of VP:28066. Drift between the four is a live bug
class — assert all four.
`logsz = if log_present == 1 then 42 + path_block + cblocks else 0`, where
`path_block = 4*((plen+4)/4)` and `cblocks = rx_content_blocks_size(...)` — the
`gate_rel = 42 + path_block + cblocks` expression at VP:26662 IS the block size
(open-fixed 32 + path_block + cblocks + close 10). Add `logsz` to
`service_tramp_size` (VP:27889) → flows to blob_end_off (VP:26710) → p_filesz.
TWO HAND-SYNCED WALKS (the number/bytes split — no shared truth): size and emit
must agree TO THE BYTE or p_filesz truncates the segment and the spawned server
SIGSEGVs at exec (every prior service slice's bug class). Mandate an explicit
emitted-byte-count assertion in the test. `logsz = 0` when no log block →
**S1-S4 servers byte-identical by construction** (pin it).

## Parse — MkService widens 7 → 12 fields (arena cost ZERO)
`MkService` (VP:984) + its 7 positional accessors (VP:10998/11016/11034/11052/
11070/11088/11106) + the SvNil sentinel (VP:11150) + `parse_service_decl`
(VP:11201) — **10 sites total** — all widen together, adding **`log_present`**,
`log_path_start`, `log_path_len`, `log_content : Ast`, `log_on_error` (flat shape;
`logs : LogList` defers to S5d/multi-block).
**MUST-FIX (review): a single `-1` sentinel CANNOT express both "absent" and
"malformed"** — `parse_reaction_decl` collapses them (VP:11517) because a
reaction block is never optional, but a `log:` block IS. Without a presence flag,
a malformed log decl is SILENTLY IGNORED instead of refused (gate item 4 becomes
unsatisfiable). Three-state gate: `log_present == 0` → no log, no error;
`log_present == 1 && log_path_len < 0` → verify ERROR; else emit. 12 ≤ 13 so the
arena-cost-zero argument still holds.
**MUST-FIX (same severity): PARSE `on_error:` in S5a even though only the refusal
is implemented.** If it's unparsed, `parse_services`' structural
`skip_indented_block` (VP:11238) swallows it and the emitted server runs **drop**
while the source declares **abort** — for a fail-closed audit feature, a silent
policy downgrade is the worst possible failure mode.
**Arena cost is zero**: `max_payload_fields` (VP:22349) is a max over all concepts;
current maxima are MkRule 12 and CollRecState 13, so entry_size is already
`8*(13+1)` — MkService can reach 13 fields without changing entry_size/esize/any
emitted disp32.
Parse: continue `parse_service_decl`'s positional walk past `handler:` with
**`skip_seps` (VP:5386), NOT `skip_seps_dedent` (VP:9298)** — review SHOULD 4:
skip_seps_dedent also skips kind 800 (Dedent), so a service WITHOUT a log would
skip past the block-closing Dedent and land on the next top-level declaration.
`log:` is a SIBLING of `handler:` at the same indent (only a Newline between), so
skip_seps reaches it and stops at the Dedent otherwise. Then `log` / `:` /
Newline+Indent / `append_file` / Str path / `parse_or` content — **verbatim
`parse_reaction_decl`'s tail (VP:11494)**. NOTE (SHOULD 5): the eager `parse_or`
runs even for log-less services (allocating arena nodes on whatever follows) —
inert because gated by `log_present`, and bounded because skip_seps parks the
cursor on a Dedent.
New: `span_is_log` (mirror of span_is_effects VP:7272). Reuses
`span_is_append_file` (VP:7295).
**THE ONE NEW PARSE SUBTLETY: absent ≠ malformed.** No `log:` line → `log_path_len
= -1` (the "no log" sentinel), NOT a malformed-decl error. Get this wrong and
EVERY existing service refuses.

## Verify
`service_errors` (VP:13544) gains a `log_bad` term from a new `svc_log_ok`, on the
THREE-STATE gate: `log_present == 0` → 0 (the byte-identity path);
`log_present == 1 && log_path_len < 0` → ERROR (malformed, NOT silently ignored);
else path must be a string literal AND content `ast_is_str == 1` for S5a — refuse
AstCall/AstField/AstNum/anything else with an S5b/S5c breadcrumb, and refuse
`on_error: abort` with an S5c breadcrumb (parsed, so it can't silently downgrade).
Template: rx_content_ok VP:13857 / rx_carg_ok VP:13774 (full 26-arm match with an
explicit `AstErr => 0`). KEEP the existing service+reaction/connection/resource
refusal (**VP:13571**, the `effects` term — VP:13570 is `reserved`).
The emit walker must mirror the size walker arm-for-arm INCLUDING refused shapes
(the discipline text is at **VP:26276-26278 / 26495-26496 / 26577-26578** — under
eager lets, refused shapes are still evaluated before abort_if fires).
**SENTINEL TOLERANCE (review SHOULD 6)**: extend the `SvNil` sentinel (VP:11150)
with `log_present: 0, log_path_len: 0 - 1, log_content: Ast::AstErr`; the new
accessors + `logsz`'s lets run on it AND on log-less services under eager lets, so
they must be tolerant (the reaction precedent states this at VP:26240-26241 —
`bytelit_decoded_len` floors at 0).

## Eval / fixed point
No interpreter path (service is an entry). Self-source declares no service →
has_service=0 → dead branch → gen1==gen2 (the S1 precedent).

## Milestone / test — {S1,S2,S3,S4} × {log, no-log} (review MUST-FIX 3)
An S1-ONLY milestone is INADEQUATE: S1 has no parse, so it cannot catch the rax
FATAL class — it would go green on a compiler that breaks every other branch. The
matrix is mandatory. For EACH of the four body shapes, spawn a server WITH a log
block, issue **N=5 requests over the same process**, and assert:
1. the log file has EXACTLY N lines (per-request firing, not once-at-startup);
2. **the WIRE RESPONSE is byte-identical to that branch's existing no-log assert**
   (this is what catches the rax class — the log must not perturb the parse);
3. the four existing no-log pins (NR:23743/23829/23934/24043) still pass, and a
   no-log service emits a **byte-identical ELF** (SHA-256 vs a pre-change gen0);
4. an explicit emitted-byte-count assertion (size/emit agreement) + p_filesz ==
   actual file size (the S4 net).
Plus a hand-run: I spawn a logged ROUTER (not just S1), curl it 3×, check the
routes still answer correctly, and cat the log file.

## Gate (clean disk)
1. Proofs check out; suite green; existing binaries byte-identical.
2. two_generation gen1==gen2 + composite demo green.
3. MILESTONE above incl. the hand-run.
4. Verify pins: static-literal log emits; concat content → refused (S5b);
   field content → refused (S5b); `on_error: abort` → refused (S5c); a service
   with no log still emits (byte-identical); malformed log decl → refused.

## Explicitly deferred
S5b field/concat content (move the block post-field-select → restores verbosec
parse-fail parity, pays the forward-jump recomputation, reuses svc_carg_size);
S5c `on_error: abort` (needs a sys_exit(1) tail); S5d multiple blocks (`logs :
LogList`); `resp.status` in content (an AstNum itoa arm — rx_carg_size's
`AstNum => 0` refuses it today); **`resp.body`** (NOT re-readable — the server
STREAMS its response, no buffer, and value-position concat is an int3 trap;
needs copy-to-region + a materializing concat — the large deferred piece);
`req.timestamp` (new clock_gettime + a frame slot the tramp has no rbp for);
`req.body`. NOTE for the S8 forked slice: the self-hosted writer emits one
`write` per concat arg (vs verbosec's single buffered write) — identical file
bytes single-process, but NOT interleave-safe under a forked server.
