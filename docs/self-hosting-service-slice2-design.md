# Self-hosted HTTP service — slice 2: the HTTP request parser (the arc's pivot)

The one genuinely new subsystem of the capstone. Goal: the emitted server parses
each request's `method` and `path` off the wire and a handler can reference them
as `req.method` / `req.path`; S2 proves it end-to-end by echoing one req field.
Everything downstream (S3 router, S4 concat/computed, S5-S7 effects-in-handler)
depends on this. Evidence: NR=native.rs, VP=vexprparse.verbose, VR=verifier.rs,
main `30c7689`. Oracle: `examples/echo_path.verbose` (+ verbosec's
`emit_http_parse_method_path` NR:19517 / `emit_http10_dynamic_bytes` NR:18683).

## What S1 gave us / what S2 changes
S1's `x86_service_tramp` (VP:26987): `sub rsp, max_request` once (before
accept_top), then per accept: `accept → read(r13, rsp, max_request)` [rax =
bytes_read] → jmp-over precomputed-response-blob → `write(r13, blob, rlen)` →
close → jmp accept_top. The response is PRECOMPUTED at emit time from a literal
`HttpResponse{status, body}`.

**MUST-FIX 1+2 (review): S2 is a SEPARATE branch, gated on the handler body's
AST shape — the S1 AstStr path stays verbatim + byte-identical.** BOTH
`x86_service_tramp` AND `service_tramp_size` gain an IDENTICAL dispatch:
- body = `AstStr` → the EXISTING S1 path (parse NOT emitted; precomputed blob).
  Untouched, byte-identical (pinned by `self_hosted_service_constant_response`
  NR:23743). Note S1's size rule extracts the body via `ast_str_start(body_ast)`
  (VP:26956) — that accessor GARBAGES an AstField, so the dispatch must fork
  BEFORE it, in both copies. A one-byte disagreement between the size copy and
  the emit copy breaks p_filesz/src_base for the whole binary (the S1 Finding-2
  desync class).
- body = `AstField(req, method|path)` → the NEW S2 path: insert the parse block
  after the read, then a runtime serializer (below). Parse AND serializer emit
  ONLY here.

## The parse (port of NR:19517, with one deliberate simplification)
Entry: rax = bytes_read, the request bytes at [rsp]. Registers: **rbx = cursor,
rax = bytes remaining, r8b = byte scratch** (verbosec's exact choices; r12=listen
fd, r13=client fd untouched — all callee-saved, survive syscalls). Algorithm:
- `mov rbx, rsp` (cursor = buffer start); save method_ptr = rsp.
- scan_method: `while rax != 0 && [rbx] != ' ' (0x20) { inc rbx; dec rax }`.
  On rax==0 (no space found) → **parse_fail** (see policy below).
- method_len = rbx − method_ptr. Store method (ptr, len).
- `inc rbx; dec rax` (skip the space); path_ptr = rbx.
- scan_path: `while rax != 0 && [rbx] not in {' ', '\r'(13), '\n'(10)} { inc rbx;
  dec rax }`. On rax==0 → parse_fail.
- path_len = rbx − path_ptr. This is the echoed field → hold it (below).
DELIBERATE SIMPLIFICATION (legal — only observable server bytes must match, the
S1 precedent VP:26975): verbosec stores NUL-terminated POINTERS + lazy strlen; we
compute **len = cursor − start DURING the scan** and carry (ptr, len) — no NUL
mutation, no strlen. The serializer needs len anyway.
**MUST-FIX 3 (review): hold (field_ptr, field_len) in CALLEE-SAVED REGISTERS,
NOT a sub-rsp slot.** S1's tramp does `sub rsp, max_request` ONCE then reads with
`mov rsi, rsp` — the buffer IS at `[rsp]`, and `mov rbx, rsp` (cursor init) is
only correct in that layout. An extra `sub rsp` for slots would shift rsp so the
read fills a different address than the slots — the "sub-rsp slots" alternative
is WRONG; drop it. S2 echoes ONE field → two values: hold field_ptr in **rbx**
(free after the parse) and field_len in **r14** (unused; r12=listen fd, r13=client
fd, r15=arena base from S1's program_uses_arena prologue — avoid all three). Both
survive the literal-write + itoa blocks (syscalls clobber only rax/rcx/r11; itoa
balances its own `sub/add rsp,0x20` BELOW the buffer, so field_ptr — an absolute
address — is unaffected, confirmed no aliasing). Nothing pops the buffer until the
loop tail.

### Parse-fail policy — drop the connection, KEEP SERVING (verbosec parity)
Buffer exhausted before a delimiter → NOT an abort. jmp to the close+jmp-accept_top
tail (drop this client, keep the loop). Matches verbosec (NR:19132-19136).

## Handler body — ONE new arm (no rule call, no arena)
S1 emits no handler body (precomputes a literal). verbosec's
`emit_handler_to_slots` (NR:19614) is an emit-time AST walk, not a call. S2 widens
the tramp's handler extraction with ONE arm: body =
`AstField(AstVar(req), <field>)` (AST shape VP:252). **TRAP (scoping 5e): the
synthetic HttpRequest fields have BOGUS name spans (VP:22080 — the verifier only
resolves the concept NAME, never field types), so `field_index_of` CANNOT resolve
`req.path`.** Resolve by BYTE-MATCHING the AstField's field-name span (fstart,
flen — from the HANDLER-BODY ident `req.path`, real source bytes, VP:3734) against
`"path"` / `"method"`. `span_is_path` EXISTS (VP:6922); **`span_is_method` does
NOT — S2 must add it** (review SHOULD 5). ALSO (review SHOULD 5): verify the
AstField BASE is `AstVar` whose span byte-matches the handler's INPUT PARAM name —
else `undefinedvar.path` is mis-accepted. method → (rbx,r14) of the method span;
path → (rbx,r14) of the path span.

## Runtime serializer — FIXED-SIZE (review SHOULD 4: runtime-itoa the status too)
verbosec serializes as sequential writes, no response buffer (NR:19852). The
ORIGINAL plan folded status into a precomputed literal — but the self-hosted
`service_errors` gate does NOT range-check status (VP:12861, only `ast_is_num`),
so status could be 1-5 digits → a `mag_digits(status)`-dependent, 4-pad-non-linear
literal → the parse-fail rel32 becomes status-magnitude-dependent (the arc's
biggest desync risk). ADOPT verbosec's shape (NR:19857-19862): keep THREE FIXED
literals and itoa BOTH numbers at runtime → the serializer is a FIXED-SIZE block,
the parse-fail rel32 is a compile-time CONSTANT, no new addresser needed:
1. `write(r13, "HTTP/1.0 ", 9)` — fixed literal, inline (jmp-over-data + lea).
2. **itoa(status) → r13**: `mov rax, <status imm32>` then the itoa TAIL. status is
   a compile-time literal but itoa'd at RUNTIME so the code is fixed-size
   regardless of magnitude.
3. `write(r13, " OK\r\nContent-Length: ", 21)` — fixed literal.
4. **itoa(field_len=r14) → r13**: `mov rax, r14` then the itoa TAIL.
5. `write(r13, "\r\n\r\n", 4)` — fixed literal.
6. `write(r13, rbx(field_ptr), r14(field_len))` — the echoed field, straight from
   the request buffer.
Then close + jmp accept_top.
**REUSE ONLY THE itoa TAIL** (review NOTE 6): `x86_rx_carg`'s AstField arm
(VP:25792) begins with a reaction-specific arena-load prefix (`mov rax,[rsp];
imul; add r15; …`) — S2 does NOT reuse that; it reuses only the itoa tail (`sub
rsp,0x20 … digits … add rsp,0x20`, value in rax). The itoa tail emits NO trailing
byte (write count = digit bytes only — confirmed VP:25774 "no newline"), so it is
HTTP-framing-safe. fd swap: BOTH the literal-write blocks AND the itoa tail write
to rbx today (`48 89 df`) → swap to `mov rdi, r13` (`4c 89 ef`, 3 B, size-stable).

## Sizing one-truth — now TRIVIAL (fixed-size serializer)
With the runtime-itoa serializer, `service_tramp_size`'s AstField path =
`startup + parse_block + serializer + loop_tail`, EVERY term a compile-time
CONSTANT (no mag_digits(status), no runtime-length dependence — itoa code is
fixed-size). The parse-fail `jmp rel32` distance (fail-jump → close+loop tail) is
therefore a compile-time CONSTANT. Every forward jump is closed-form (review Q3:
tractable, and #4 makes it constant): parse-fail jz's → close label; internal
parse/itoa/lit jumps are local; loop back-jump is `0−(…)` as in S1. ONE-TRUTH: the
same AstField-path size expression feeds `blob_end_off` (VP:25991, has_service
position, has_reaction/has_service mutually-exclusive-first IDENTICAL in both) AND
the emit — assert each fixed run's length with the script-check discipline; a
one-byte drift breaks p_filesz/src_base (Finding-2 class).

## Verify — widen the S1 shape gate
`service_errors` (VP:12861) today accepts `HttpResponse{status:<AstNum>,
body:<AstStr>}` + zero lets. S2 ADDITIONALLY accepts `body:
AstField(AstVar(req), method|path)` (still zero lets, still status a literal). The
field name must byte-match `method` or `path` (unknown field → refuse). Anything
else — `req.body`, concat, if/else, computed status — refused with an S3/S4
breadcrumb. Reserved-name + one-service + protocol/port/max_request checks
unchanged from S1. Fixed point untouched by construction (self-source declares no
service; has_service=0 → branch dead → gen1==gen2).

## Eval
No interpreter path (service is an entry). The handler AstField(req, …) is only
walked at emit time. No eval change.

## Milestone / test (per-request re-parse, not precompute)
Rust test (spawn/poll/kill, the S1 pattern): compile an echo-path service (handler
`resp = HttpResponse { status: 200, body: req.path }`, ephemeral port) via gen0 →
server ELF; spawn; poll connect:
- `GET /foo HTTP/1.0\r\n\r\n` → response body EXACTLY `/foo`, with
  `Content-Length: 4`, framing `HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\n/foo`.
- second request `GET /bar/baz HTTP/1.0\r\n\r\n` on the SAME server → body
  `/bar/baz`, `Content-Length: 8` (PROVES per-request re-parse, not precompute —
  the whole point of S2).
- a delimiter-free malformed request (`ZZZZ` no space) → connection dropped, and a
  subsequent well-formed GET still served (proves parse-fail keeps the loop alive).
- hand-run: I spawn the emitted server and curl it myself.
Oracle cross-check: verbosec compiling the same handler shape emits the identical
wire framing (NR:19855 == the S1 constant `"HTTP/1.0 … Content-Length: "`).

## Gate (clean disk)
1. Proofs check out; suite green; existing binaries byte-identical (non-service
   programs untouched; S1 constant-response services still byte-identical — the
   literal-body path must not change).
2. two_generation gen1==gen2 + composite demo green.
3. MILESTONE: the four request cases above, verified from clean disk incl. a
   hand-run.
4. Verify pins: `req.method` echo works; unknown req field → refused; concat body
   → refused (S4 breadcrumb); if/else handler → refused (S3 breadcrumb); S1
   literal-body service still emits byte-identically.

## Explicitly deferred
S3 (if/else on req.method/req.path, text equality); S4 (concat with req fields,
computed status) — where the text-representation gap becomes real and
copy-to-region (a fixed-region mmap in the service tramp) is the answer;
`req.body`; S5-S7 effects in handlers; S8 forked; S9 state.
