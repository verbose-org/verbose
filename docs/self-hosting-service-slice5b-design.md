# Self-hosted HTTP service — slice 5b: log content from request fields

S5b turns the per-request log from a tick mark into a real ACCESS LOG:
`log: append_file "/tmp/x.log" concat(req.method, " ", req.path, "\n")` — the
`hello_router_logged.verbose` shape. Evidence: NR=native.rs, VP=vexprparse.verbose,
main `c7a0589`. All offsets below were verified empirically (objdump of emitted
service ELFs), not read off comments.

## PREREQUISITE DECISION — CORRECTED after review: writev, not refusal
The self-hosted emitter writes **one `write()` per concat arg** to the log fd;
verbosec buffers the whole line and issues ONE write (NR:7881→7885-7892). Under
S8 (`concurrency: forked`) per-arg writes INTERLEAVE across children and corrupt
the audit trail. The self-hosted emitter cannot buffer without a compile-time max
length, and the self-hosted `FieldList` (VP:23145 `FCons{name_start,name_len,ty,
ty_start,ty_len,rest}`) has **no range slot at all** — so buffering would mean
WIDENING FieldList, not just filling in numbers.
**MY FIRST DECISION ("S8 refuses forked + multi-write log") WAS WRONG ON BOTH
HALVES — corrected by review:**
1. **"Single-arg logs are fork-safe" is an OVER-CLAIM.** POSIX guarantees only
   that `O_APPEND` makes the seek-to-end and the write inseparable — NOT that a
   single `write()` of arbitrary size is atomic or complete. The practical
   non-interleaving on Linux comes from the inode lock in
   `generic_file_write_iter` (an implementation property, not a contract) and does
   NOT hold over NFS. Both emitters run the log under DROP with no return check
   (VP:28427; NR:7885-7892), so a short write silently truncates. Honest framing:
   **parity with verbosec under the same unproven assumption** — not "fork-safe".
2. **The refusal rested on a FALSE DICHOTOMY: buffering is not the only route to
   one syscall — `writev` (syscall 20) is.** The args' (ptr,len) pairs are already
   in registers/literals; the iovec array is `16*N` bytes on the stack below the
   request buffer; `sys_writev` goes through the same locked `write_iter` path, so
   it is exactly as atomic as `write`. No compile-time max length, no FieldList
   widening. **RECORDED DECISION FOR S8: use `writev` for multi-block log content
   (or refuse `forked` only until that lands).** Refusing outright would have made
   this slice's own headline example — `concat(req.method," ",req.path,"\n")` —
   permanently un-forkable, a real product cost I had stated as a footnote.

## (A) S1 handlers have no parse → REFUSE field content on S1 (S5b.2 lifts)
Verified: the S1 branch DOES emit the `read` (objdump s1.elf 0x158: `xor rax,rax;
mov rdi,r13; mov rsi,rsp; mov rdx,0x1000; syscall`) — it reads and discards; only
the PARSE LOOP is missing. verbosec ACCEPTS constant-handler + field-log by
force-routing to its dynamic emitter (NR:18181-18194, keyed on log PRESENCE not
content — verified live: 1034 B server). Growing S1 a parse is mechanically
possible and strictly additive, but doubles the S1 branch's shape surface for a
non-canonical case. **S5b REFUSES `ast_is_str(body_ast) == 1` (the S1
discriminator, VP:28519/28645) AND log content referencing a req field**, with the
gate computing the discriminator from the SAME expression the emitter forks on
(verify/emit acceptance parity — the collections-tier scar). Documented as a
gen-gap vs verbosec, lifted in S5b.2.

## (B) Both spans ARE live at the select → capture them (no field restriction)
Verified from s2.elf: at the field-select all four endpoints are in distinct
registers — `rsi`=method_start (0x16d, never rewritten), `rdx`=method_end (0x18a),
`rdi`=path_start (0x193), `rbx`=path_end (loop exit 0x1bc). The select then
collapses one pair, and it is ASYMMETRIC: the path variant preserves rsi/rdx (so
method stays recoverable) but the method variant DESTROYS rbx (path_end) — so
"read the other field after the select" is not a general answer.
**Emit a span CAPTURE after the parse, BEFORE the handler's field-select.**
**REGISTER JUSTIFICATION — CORRECTED (review MUST-FIX 1): this is a LIVENESS
WINDOW argument, NOT register freedom.** Objdump-verified:
- `r10` and `rbp` are genuinely free in the accept loop (r10 appears only at
  tramp+0x30, setsockopt's arg4, pre-`accept_top`; rbp never appears in tramp CODE
  — the `%rbp` hits at S2/S4+0x115 and S3+0x149/0x195 are inside `e9`-skipped DATA
  blobs).
- **`r8` is NOT free** — it is the parse loop's byte scratch (`44 8a 03 mov
  (%rbx),%r8b` at tramp+0xbf and +0xe5, byte-identical in all three parsing
  branches). It is DEAD after the parse-loop exit.
- **`r9` is NOT free** — it is the itoa's sign save (`49 89 c1` at tramp+0x141 and
  +0x1cc, `4d 85 c9` at +0x16b/+0x1f6), i.e. AFTER the capture point, and
  write-before-read inside each itoa.
The captured values only need to survive `[capture@258, log@267+cap+logsz)`, which
they do. **LOAD-BEARING CONSEQUENCE: the log block can NEVER move later than
immediately-after-the-select** — a post-response-write placement (S5a's rejected
option (a)) would read an itoa-clobbered r9 and emit a garbage path (ptr,len).
This constraint also binds S5c's `on_error` tail and any future post-response hook.
**rbp FRAMING CORRECTED (review SHOULD 2 — I had the risk INVERTED):** emitted
procs `push %rbp; mov %rsp,%rbp` … `mov %rbp,%rsp; pop %rbp` (s3.dis 0x1e3/0x2a5),
i.e. they SAVE AND RESTORE rbp — so a tramp→proc call would NOT destroy it. rbp is
the **safest** of the four, not the riskiest. r14/r15 are casualties because procs
use them as arena state WITHOUT saving. Do NOT record "the invariant extends to
rbp" in CLAUDE.md — that would enshrine a wrong invariant; record instead that the
fragile registers are r8/r9 for LIVENESS reasons.
**THE CAPTURE IS UNCONDITIONAL (review NOTE 3):** it is required even for a
path-only log after a path-select, because the log block ITSELF destroys
rsi/rdx/rdi (`x86_svc_log_block` VP:28427 emits `lea rdi,[rip-…]`, `mov esi,0x441`,
`mov edx,0x1A4` before `open`, and each content block emits `mov rdi,r15`/`lea
rsi`/`mov rdx`). Do not "optimize away" the capture when log-field == handler-field.
Register choice is FORCED: surviving-syscall ∧ free-at-268 leaves exactly
{rbp, r10, r8, r9}.
Capture bytes:
```
method: 49 89 f2 (mov r10,rsi) ; 49 89 d0 (mov r8,rdx) ; 49 29 f0 (sub r8,rsi)  = 9 B
path:   48 89 fd (mov rbp,rdi) ; 49 89 d9 (mov r9,rbx) ; 49 29 f9 (sub r9,rdi)  = 9 B
cap = 9 * (log_uses_method + log_uses_path)     -- 0, 9, or 18
```
The log's field write is then **18 B — byte-length-IDENTICAL to `x86_svc_carg`'s
AstField arm** (VP:28137): `mov rax,1 ; mov rdi,r15 ; mov rsi,r10|rbp ;
mov rdx,r8|r9 ; syscall`.
**CONSEQUENCE: `svc_carg_size` (VP:28027) and `svc_cargs_size` (VP:28073) are
reusable VERBATIM** — S5b adds ZERO new hand-synced number/bytes pairs for
content. (S5a's byte-length-neutral fd swap paying off.) The only new number term
is the closed-form `cap`.
**RULE: no field-count limit, no same-field-as-handler constraint.** The log may
reference method, path, both, or repeat — independently of the handler. Rejecting
"log field must equal the handler's field": it cannot express the two-field access
log (the canonical case) and couples the log to an unrelated part of the source.
**rbp CLAIM — flag it**: this extends the "tramp never transfers control to an
emitted proc" invariant to a FOURTH register (after r14, r15). Every emitted rule
proc uses rbp as a frame register (s1.elf 0x1c2 `push %rbp`), so the invariant is
load-bearing here. Record it in CLAUDE.md beside the r15 note.

## Naming — `req`, not the handler's param name
verbosec's log scope uses HARDCODED synthetic `req`/`resp` (verifier.rs:1128/1144;
scope map NR:19365-19392, rewrite NR:18547-18576) — NOT the handler's param name.
S5b mirrors that with a new **`span_is_req`** (a sibling of span_is_method
VP:6957), and must NOT reuse `svc_field_ok` (VP:13499), which byte-matches the
HANDLER's param name. They coincide in every current example (the param is named
`req`), which is exactly why getting it wrong would go unnoticed.

## Placement — keyed on CONTENT SHAPE (revised; see the audit reconciliation)
- **STATIC content (any branch)** → pre-read (tramp+158), **UNCHANGED from S5a**.
- **FIELD content (S2/S3/S4 only)** → post-field-select (tramp+267+cap), preceded
  by the capture. S1 + field content is REFUSED (§A).
**AUDIT RECONCILIATION (review SHOULD 13 — I reversed the project's own judgment
one slice earlier without arguing it).** S5a's shipped comment (VP:28221-28224)
calls logging every accepted connection *"the more complete audit posture"* and
verbosec's skip-on-parse-fail *"a genuine audit gap"*. My first S5b draft asserted
the opposite. **S5a's judgment STANDS**: for a SIEM/compliance-oriented language,
"connection accepted, request never parsed" (scanners, slow-loris, truncated
requests) is exactly what an audit trail should keep. So static logs KEEP the
pre-read placement and KEEP that coverage.
Field logs are post-parse **by physical necessity, not by policy** — the field
does not exist before the parse. That is a CONSEQUENCE of what you asked to log,
not a downgrade of judgment. Say it that way in the release note.
COST of keying on content shape: **7 log splice sites** (S1 keeps ONE unconditional
pre-read site — a field log on S1 is refused, so its second site would be dead
code) + 3 capture sites = **10**, vs 7 for branch-keying. Accepted: it preserves
**byte-identity for ALL existing static-log services** (not just S1), causes ZERO
behavior change for existing S5a users, and keeps the audit signal where it can be
kept. Drift across splices is about PLACEMENT, not content; the expanded matrix +
per-row byte-count assert is the net.

### Jump recomputation — 9 constants, 2 DE-BAKED
| branch | constant | today | S5b |
|---|---|---|---|
| S1 | back-jmp | `-(87+block+logsz)` | unchanged |
| S2 | je1 **baked 0x18f=399** | 399 | `le32(399+cap+logsz)` — **DE-BAKE** |
| S2 | je2 **baked 0x169=361** | 361 | `le32(361+cap+logsz)` — **DE-BAKE** |
| S2 | back-jmp | `-(470+logsz)` | `+cap` |
| S3 | jump1 (VP:28612) / jump2 (28613) | `187/149+l4+A+B` | `+cap+logsz` |
| S3 | jump3/4/5 (28614-16) | — | unchanged |
| S3 | jump6 (28617) | `-(258+l4+A+B+logsz)` | `+cap` |
| S4 | jump1_4 (28636) / jump2_4 (28637) | `388/350+csize4` | `+cap+logsz` |
| S4 | jump6_4 (28638) | `-(459+csize4+logsz)` | `+cap` |
S2's two hardcoded `je` targets sit inline in the big byte literal (VP:28645) and
must be split into `b"…\x0f\x84", le32(j), b"…"` — structurally identical to what
S3/S4 already do. DE-BAKING IS BYTE-SAFE: Verbose bytes literals are concat-only
(no indexing/slicing anywhere in VP) and the tramp's length comes from the
independent `service_tramp_size`; `le32(399)` reproduces `\x8f\x01\x00\x00` when
cap=logsz=0, so no-log byte-identity holds by construction (verified against the
live 931 B / `ff8f9674…` pin). NOTE: only **8** of the 9 rows actually change
(S1's back-jmp doesn't).
**THE THREE rel8 `je`s (review NOTE 6 — do not miss this):** the second parse
loop's `74 14`@0xec, `74 0e`@0xf2, `74 08`@0xf8 ALL target **0x102 exactly** (the
select). The capture must be spliced at that precise address with NOTHING between
the je targets and it — then their displacements are unchanged for free. That is
why no rel8 appears in the 9-constant table.
**A BUG IN THIS TABLE, CAUGHT AT RUNTIME (recorded — the lesson outlives the
slice).** The row "S2 back `+cap`" is right in intent but S2's back-jump is
**inline** (`le32(0 - (470 + logsz))`), not a named `jump6`-style let like S3/S4 —
so it is easy to miss when applying `+cap`. Missing it made the server answer
request 1 correctly, then jump 9 bytes past `accept_top` into mid-instruction and
SIGSEGV on request 2. **`p_filesz` was CORRECT throughout** (the size walk already
had `cap`), so the size/emit assertion could never have caught it — only a SECOND
request could. TAKEAWAY: byte-count asserts prove the size walk agrees with the
emit walk; they say NOTHING about whether a jump lands where you meant. Any slice
touching loop-back edges must issue ≥2 requests per server in its milestone.
**SIZING PLACEMENT (review MUST-FIX 9 — I stated this wrong):** `logsz` ALREADY
sits OUTSIDE the fork in `service_tramp_size` (VP:28519: `out = logsz + (if
ast_is_str … )`) and **must NOT move** — the size is position-independent;
placement is an EMIT-only concern. Only `cap` goes inside the fork (S1 must
contribute 0, and eager lets still evaluate the size expression on refused
shapes — VP:26276-26278). `x86_service_tramp` is the ONLY rule where placement
changes.

## Parse-fail parity — a DELIBERATE semantic correction of S5a
Post-select, both parse-fail `je`s (s2.elf 0x173/0x199 → 0x308, the close tail)
jump OVER the log block → malformed requests stop being logged. That is exactly
verbosec (all fail patches resolve to close_label at NR:19445, taken AFTER the log
emit NR:19397). **This applies ONLY to FIELD logs** — a stale earlier draft of
this doc claimed it was "a behavior change for existing S5a users on S2/S3/S4";
that is NOT what shipped. Under the content-shape keying above, STATIC logs stay
pre-read and keep logging every accepted connection (S5a's coverage, byte-identical
binaries, zero user-visible change). A FIELD log necessarily sits post-parse — the
field does not exist earlier — so it logs only parsed requests. **Pin BOTH sides**:
a malformed request produces NO line for a field log, and DOES produce a line for a
static log on the same branch.

## Verify
`svc_log_content_ok` (VP:13727) widens from `AstStr => 1` to: AstStr → 1;
`AstField` → `svc_log_field_ok` (base is AstVar spelling exactly `req` via
span_is_req AND fname is method|path); `AstCall` → `span_is_concat` (VP:6738) AND
every arg is AstStr or svc_log_field_ok; every other arm incl. AstErr → 0.
`service_errors` (VP:**13853**; the `log_bad` let is at 13887) gains
`log_field_on_s1_bad = if svc_log_has_field == 1 and ast_is_str(body_ast) == 1`.
**ONE WALK FAMILY (review MUST-FIX 10):** do NOT add a separate
`svc_log_has_field` walk beside `svc_log_uses_method`/`_path` — three walks over
one Ast is the drift class this slice is guarding against. Since
`svc_log_content_ok` already restricts log fields to method|path,
`has_field ≡ uses_method ∨ uses_path` EXACTLY: define the two `uses_*` walks once
and DERIVE `has_field`. Parity is achievable because `service_errors` already
computes `handler`/`hblock`/`hresult`/`hfields` (VP:13869-13873) and uses
`vf_head_val(vf_tail(hfields))` at 13874 — the IDENTICAL expression
`x86_service_tramp` (VP:28560) and `service_tramp_size` (VP:28465) fork on; add
`let body_ast = <that expression>` and gate on it. Confirm `se.prog`/`ss.prog`/
`st.prog` are the same `prog` before relying on find_rule resolving the same
handler in all three.
**PARAM-NAME DECISION (review MUST-FIX 11).** Empirically on this build: a handler
whose param is `r` with a log saying `req.method` — verbosec's VERIFIER ACCEPTS
but its NATIVE FAILS (`concat argument type not yet supported in native`), a live
verify/emit gap inside verbosec. The self-hosted compiler, resolving log fields
from CAPTURED SPANS (param-name-independent), would happily emit it. **DECISION:
(b) ACCEPT it and record a documented self-hosted-ahead-of-verbosec divergence** —
the capture makes the log genuinely independent of the handler's naming, which is
the correct semantics (verbosec's own log scope hardcodes `req`, verifier.rs:1127;
its failure is an emitter limitation, not a design intent). Pin a test row with
param `r` + `req.path` asserting we EMIT and serve correctly.
Still refused: `on_error: abort` (S5c), `resp.status`/`resp.body`, `req.timestamp`,
`req.body`, non-`req` base. NOTE: the self-hosted compiler has NO breadcrumb
strings — a refusal is exit 1 + empty stdout (NR:24472-24476); "breadcrumb" means
a VP comment naming the deferring slice + a test label.

## Reuse summary
| artifact | reuse |
|---|---|
| svc_carg_size / svc_cargs_size | **VERBATIM** (the field write is 18 B either way) |
| x86_svc_log_lit_block (VP:28334) | already the r15 flavor |
| x86_svc_carg (VP:**28123**) / x86_svc_cargs | NO — need r15 mirrors (no register parameterization) |

**THE COUPLING TO DOCUMENT (review NOTE 17):** after S5b, `svc_carg_size` is the
single size truth for **TWO** emitters (the body write and the log write) — a
1-size→2-emit fan-out, so a later change to the body write silently desyncs the
log. Ensure the `p_filesz == file size` assert runs on a **field-log** row, not
only static-log rows. And `x86_svc_log_carg`'s AstField arm must emit 18 B
**unconditionally** (arm-for-arm with `svc_carg_size`'s unconditional 18), which
means an internal `span_is_method` fork PLUS a defined fallback for fields the
gate refuses but eager lets still evaluate (`resp.status`, `req.body`, non-`req`
base) — name the fallback (the path shape) in the comment, per VP:26276-26278.
**Placement safety (review NOTE 19):** `x86_svc_log_block` has no push / `sub rsp`,
so the captured pointers (into the `sub rsp,max_req` buffer) stay valid across it;
it clobbers only rax/rcx/r11/rdi/rsi/rdx/r15, none of which the continuation reads
before re-establishing (S2/S4 reload rdi/rsi/rdx per write; S3 reloads rsi from
rbx, rdi via lea, rcx via mov, and its `cld` still precedes `repe cmpsb`).
New: `span_is_req`, `svc_log_field_ok`, `svc_log_cargs_ok`, `svc_log_uses_method`
/`_path`, `x86_svc_log_capture`, `x86_svc_log_carg`, `x86_svc_log_cargs`. Widened:
svc_log_content_ok, svc_log_content_size (VP:28249), x86_svc_log_content
(VP:28357), service_errors, service_tramp_size, x86_service_tramp. **No MkService
widening** (the content Ast is already parsed by parse_or, VP:11396).

## Milestone / test — the matrix, EXPANDED (review MUST-FIX 14)
Base: `{S1,S2,S3,S4} × {no-log, static-log, field-log}` (field-log REFUSES on S1).
**The base matrix omits every row the capture exists for — ADD:**
- **both fields** in one log, `concat(req.method," ",req.path,"\n")` (cap = 18) —
  otherwise "field-log" only ever exercises cap = 9;
- **log field ≠ handler field**, specifically handler = `req.method` (whose select
  does `mov rbx,rsi`, destroying path_end) + log = `req.path` — the EXACT case the
  capture exists for; without it the matrix cannot fail if the capture is dropped;
- **handler param not named `req`** (param `r` + log `req.path`) — the accepted
  divergence;
- **bare-field log** (`append_file "…" req.path`, one write, no concat);
- S3 with log field ≠ cond field; S4 with log field ≠ the concat's field arg;
- **repeated field** (`concat(req.path, req.path)`) — two reads of one captured pair.
Per spawned server, N=5 requests, assert:
1. log line count == N AND **the logged BYTES equal the requested method/path**
   (not just the line count — a wrong-register capture produces plausible-looking
   bytes);
2. **wire response byte-identical to that branch's no-log oracle** (catches the
   rax/parse-perturbation class — the S5a scar);
3. `p_filesz` == emitted byte count (the size/emit drift net);
4. SHA pin on the no-log S1 ELF (931 B, NR:24283) UNCHANGED + a NEW SHA pin on
   S1+static-log; re-pin S2/S3/S4 static-log after the move;
5. **a malformed request produces NO log line on S2/S3/S4** (the parity assert);
6. field-log on S1 REFUSES (exit 1, empty stdout).
Hand-run: spawn a logged ROUTER with `concat(req.method, " ", req.path, "\n")`,
curl 3×, verify routing still correct, `cat` the log.

## Risks
(a) byte-identity: **preserved for ALL no-log AND ALL static-log services** (the
static placement is unchanged) — an earlier draft said it was "LOST for S2/S3/S4
static-log"; that was the branch-keyed design and is NOT what shipped. Measured:
all 8 `{S1,S2,S3,S4} × {no-log, static-log}` ELFs bit-identical pre/post, plus the
canonical 931 B / `ff8f9674…` pin. (b) sizing: the
smallest yet (content sizing reused verbatim; only `cap` is new). (c) eager-let /
sentinel tolerance: svc_log_uses_* and cap run on SvNil and on log-less services —
all walk an Ast, AstErr → 0 (VP:26240-26241 discipline). (d) 7 splice sites — drift
is about PLACEMENT, not content; the 12-row matrix + per-row byte-count assert is
the net. (e) fixed point: self-source declares no service → dead branch → gen1==gen2.
(f) the rbp claim (§B).

## Deferred
S5b.2 (field log on S1 — emit a conditional parse; strictly additive);
S5c `on_error: abort`; S5d multi-block; `resp.*` / `req.timestamp` / `req.body` in
log content; S6/S7 (service+resource/connection is refused outright today,
VP:13880 — lifting it is structural, not a slice); S8 forked (WITH the multi-write
log refusal decided above); S9 state.
