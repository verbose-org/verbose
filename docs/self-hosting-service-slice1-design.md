# Self-hosted HTTP service — slice 1: parse + synth concepts + constant response

REVISED after adversarial review (2026-07-26): one FATAL + four MUST-FIX folded
in. Goal: `elf_program_src` compiles a `.verbose` with a `service` block whose
handler returns a LITERAL `HttpResponse` into an ELF that LISTENS on the declared
port and returns a fixed HTTP/1.0 response to every connection. No HTTP request
parsing (S2). Evidence as VP=vexprparse.verbose / NR=native.rs / VR=verifier.rs,
main `6ffaf6e`. Oracle: `examples/hello_http.verbose` (handler returns
`HttpResponse { status: 200, body: "Hello from Verbose over HTTP!" }` — body 29 B).

## Structural shift — a new top-level shape (mirror has_reaction's cascade replace)
Every self-hosted emit so far is ONE-SHOT: marshal input → call entry → exit. A
service is a long-running accept loop with NO input marshal. The `has_service`
branch in `elf_program_src` (mirror of `has_reaction` VP:26332) REPLACES the
`entry_rule_*` cascade with the accept-loop bytes, and suppresses the marshal.

### MUST-FIX (Finding 2): has_input is NATURALLY 1 for a service — two matched edits
`has_input` is computed from the head rule's params in TWO INDEPENDENT copies:
`elf_program_src` (VP:26324-26325) and `blob_end_off` (VP:25176-25177). A
service's head rule is the handler (input `req : HttpRequest`), so
`param_list_len > 0` → `has_input == 1` in BOTH → the emit would fire
`x86_stdin_marshal` AND blob_end_off would add `msize`. That marshal must NOT
exist for a service. Add the `has_service` guard to BOTH copies IDENTICALLY (force
has_input/msize to 0 when has_service), plus the cascade replacement in both. The
"has_input==0 already handles it" phrasing was WRONG — it is two edits that must
match, or `blob_end_off` and the emit disagree on marshal-present, `fsz` is wrong,
and `src_base = 0x400000 + blob_end_off` points past the source blob → every
`byte_at` reads garbage. This is the #1 desync risk. There are now TWO
mutually-exclusive cascade replacers (has_reaction, has_service); S1 refuses their
combination; the nesting/dispatch order must be identical in emit AND blob_end_off.

## Emit — the accept loop (verbosec shape NR:20077-20176; MATCH IT, no extra checks)
Registers: **r12 = listen fd, r13 = client fd** (verbosec's; callee-saved, survive
syscalls; the response lea/write uses only rax/rsi/rdi/rdx — no r12/r13 clobber).
```
socket(2,1,0)                 ; rax=41 ; mov r12, rax
setsockopt(r12,1,2,&1,4)       ; push 1 ; mov rax,54 ; mov rdi,r12 ; mov rsi,1
                              ;   (SOL_SOCKET) ; mov rdx,2 (SO_REUSEADDR) ;
                              ;   mov r10,rsp ; mov r8,4 (64-bit `49 C7 C0 04`) ;
                              ;   syscall ; add rsp,8   (NOT pop)
bind(r12,&sockaddr_in,16)      ; rax=49
listen(r12,128)               ; rax=50
sub rsp, max_request          ; drain buffer (once; rsp not restored — long loop)
accept_top:
  accept(r12,0,0)             ; rax=43 ; mov r13, rax
  read(r13, rsp, max_request) ; rax=0 ; drained, ignored in S1
  jmp +resp_block ; <response bytes, 4-packed> ; lea rsi,[rip-…]
  write(r13, rsi, response_len) ; rax=1 ; EXACT length (not the padded block)
  close(r13)                  ; rax=3
  jmp accept_top
```
### MUST-FIX (Finding 5): MATCH verbosec's error policy — NO return-value checks
Verified: `emit_http10_constant_response_bytes` (NR:20077-20176) checks NO syscall
return — not socket, bind, listen, accept, read, or write; there is no `test
rax,rax`/`js exit` anywhere. The earlier draft's startup/accept fail-closed aborts
were a DIVERGENCE (they'd break byte parity and can't be cross-validated — verbosec
lacks them). For S1, emit EXACTLY verbosec's shape (no checks). Fail-closed startup
(abort on bind failure) is a legitimate LATER hardening slice applied to BOTH
backends together — not an S1 divergence. Drop the bind-fail test leg. (This is
faithful porting, not a security compromise: the reference has the same behavior;
hardening both together stays parity-preserving.)

sockaddr_in (16 B): `02 00 | htons(port) | INADDR_ANY(00 00 00 00) | 8×00`, built
INLINE via the connection-block `e9`/`le32` jmp-over-data + `lea` house style
(VP:25945-25954). htons(port) = `(port%256)*256 + port/256` stored little-endian
(slice-3 result). Legal to diverge from verbosec's on-stack build because the
service branch is DEAD on the self-source → gen1==gen2 unaffected; only the emitted
SERVER's observable behavior must match (the response bytes), not the emit bytes.

## Response — PRECOMPUTED at emit time (MUST-FIX Finding 3: new byte-builder needed)
verbosec precomputes (NR:20062-20067), does NOT call the handler:
`format!("HTTP/1.0 {status} OK\r\nContent-Length: {bodylen}\r\n\r\n{body}")` — one
space after `HTTP/1.0`, spaces around status/`OK`, one space after
`Content-Length:`, `\r\n\r\n` before body, reason phrase hardcoded `OK`. For
hello_http the EXACT wire bytes are:
`HTTP/1.0 200 OK\r\nContent-Length: 29\r\n\r\nHello from Verbose over HTTP!`
Emit-time extraction: service Item → handler rule → confirm logic is a single
`HttpResponse { status: <AstNum>, body: <AstStr> }` (AstVariant with cstart==vstart,
VP:3885; walk the VFieldList by name span; body span stripped +1/−2, VP:5757).
**THE NEW MACHINERY (drop "no new decimal machinery")**: `mag_digits`/`mag_digit_at`
(VP:6334/6375) give a digit COUNT and a digit's ASCII VALUE, but there is NO
single-byte emit builtin — only `le32`/`le64`, which 4-pad. status (3 digits) and
bodylen sit MID-response, so the 4-pad trick cannot compose there. Build the whole
response as ONE byte-addressed stream and emit it `src_blob`-style (4-packed,
interior exact, final chunk padding jmp-skipped, `write` uses the exact
`response_len`):
- `response_len(service, concepts, src)` — total wire length = 9 (`HTTP/1.0 `) +
  mag_digits(status) + 4 (` OK\r`… count the literal exactly) + … +
  bytelit_decoded_len(body span) . Compute from the SAME handler-literal
  extraction the emit uses.
- `response_byte_at(i)` — vlength/vbyte_at-shaped (VP:6576/6623): returns wire byte
  i, dispatching across the literal segments, the status/Content-Length decimals
  (via mag_digit_at), and the decoded body (bytelit decode, VP:25950). Reuses
  mag_digit_at + the bytelit decode; NO new decimal, but a NEW addressing pair.

## Sizing one-truth (MUST-FIX Finding 4)
`service_tramp_size` = exact accept-loop byte length = startup + loop skeleton +
inline sockaddr + the 4-padded response block, where the response block length
derives from the SAME `response_len` the emit uses (the one-truth — emit and
sizing must never drift on the response). Splice into blob_end_off's chain
(VP:25185) in the SAME conditional position as the emit dispatch (VP:26332), with
the has_reaction/has_service exclusivity ordering identical in both. Precedents:
reaction_tramp_size VP:24897, connection_marshal_size VP:25895. Mentions-once;
tolerant base cases (runs on the SvNil sentinel under eager lets).

## Concept synthesis (FATAL Finding 1 — must be VISIBLE TO THE VERIFIER)
Append HttpRequest (`method:text[..8]`, `path:text[..256]`, `body:text[..4096]`,
VR:783-799) and HttpResponse (`status:number[100,599]`, `body:text[..4096]`,
VR:816-827) to the ConceptList TAIL (real fields, no variants) via a new
`concepts_append_http(concepts)` gated on has_service.
**DO NOT mirror concepts_append_result's verrs-EXCLUSION.** Result excludes its
synthetic from `verrs` (VP:26283) safely ONLY because Result programs never NAME
it (Ok/Err are their own AST nodes). But the handler NAMES `HttpResponse` in an
AstVariant → `tcheck_rule` → `type_of_env` → `resolve_type(concepts0,
"HttpResponse")` → 999999 → type 3 (ERROR) → `verrs ≥ 1` → `abort_if` REFUSES
emission (VP:26296/26332). So the synthetic concepts MUST be visible to the verify
walk: pass `if has_service then concepts_append_http(concepts0) else concepts0` as
the `concepts` arg to `prog_diags` at VP:26296 (and audit whether
reaction_errors/resource_errors, also fed concepts0, need the same — they don't
name HttpResponse, but confirm). The real `status:number, body:text` fields make
`resolve_type` yield a concept code ≥1000 that matches the handler's declared
output. GATE STEP: empirically confirm the current self-hosted verifier REFUSES
hello_http before the fix and ACCEPTS after (a ~2-minute check).

## Parse (mirror connection/reaction families)
`Service = MkService of (name span, port, max_request, handler span, protocol)` +
ServiceList (SvCons/SvNil) in the concept_group (S1 fields only; log/concurrency/
state join later slices). `parse_services` + `parse_service_decl` (positional:
`listen:` → protocol/port/max_request, then `handler:` rule name); malformed → −1
sentinel. `span_is_service` + add `is_service` to the skip predicate in ALL FIVE
walk rules (parse_program VP:9394, parse_concepts VP:10240, parse_resources
VP:10352, parse_connections VP:10746, parse_reactions VP:11098). Accessors:
svc_name_*, svc_port, svc_max_request, svc_handler_*, svc_protocol. `raw_tcp` must
be RECOGNIZED (stored) so verify can refuse it cleanly.

## Verify
- handler names a real rule; protocol http_1_0 only (raw_tcp refused, breadcrumb);
  port 1..=65535; **max_request min ≥ 64** (verbosec: http_1_0 requires ≥64,
  VR ~20-96) and ≤ 65536 (the slot cap, VP:25930 precedent). Exactly one service.
- S1 handler shape: logic is a single `HttpResponse { status:<AstNum>,
  body:<AstStr> }` AND **no let-bindings** (verbosec refuses handler lets,
  NR:19987-19996 — mirror: block_binds must be empty). Any `req.*` / non-literal /
  let → refuse with an S2 breadcrumb.
- Reserved names: user-declared HttpRequest/HttpResponse refused (VR:29-39 mirror).
- service + (reaction | connection | resource) refused in S1 (a service has no
  marshal; composing effects INTO handlers is S5-S7). Keeps S1 a pure service.
- Fixed point untouched BY CONSTRUCTION: self-source has zero `service ` items
  (grep-confirmed) → has_service==0 → branch dead → gen1==gen2 unaffected.
- NOTE (harmless): appending real-field concepts flips
  `program_uses_arena(concepts)` → the 66-B arena prologue emits and blob_end_off
  counts it (consistent both sides; its `mov r15` doesn't collide with r12/r13).
  Acceptable; a service doesn't need the arena but it's inert. Do not special-case.

## Eval
No interpreter path for a service (top-level entry, not an expression; like
reactions). No eval change; pin "no interpreter oracle for services".

## Milestone / test (the emitted binary is the SERVER)
Rust test (hybrid of the self-hosted emit-and-run NR:21428 and the server test
NR:26858): run `elf_program_src` on a hello_http-shaped source (ephemeral port
baked into a temp source — slice-3 pattern) via gen0 → server ELF; chmod +x;
`Command::spawn` (NOT `.output()` — the loop never returns); poll
`TcpStream::connect_timeout` ~50× for bind; send `GET / HTTP/1.0\r\n\r\n`;
read_to_end; **assert the EXACT bytes** `HTTP/1.0 200 OK\r\nContent-Length:
29\r\n\r\nHello from Verbose over HTTP!` (Finding 6: do NOT spawn verbosec too —
two servers can't share one port; the exact-bytes assertion is cleaner and needs
no second process); `child.kill(); child.wait()`.

## Gate (clean disk)
1. Proofs check out; suite green; existing binaries byte-identical (service-free
   programs never enter the new code).
2. two_generation gen1==gen2 + composite demo green.
3. Empirical Finding-1 check: verifier refuses hello_http before the concept-visible
   fix, accepts after.
4. MILESTONE: gen1-emitted server, spawned, returns the exact wire bytes above to a
   client GET; kill works.
5. Verify pins: missing handler rule → diag; req.*-using / non-literal / let-bound
   handler → refused (S2 breadcrumb); raw_tcp → refused; port 0 / max_request < 64
   → refused; service + reaction/connection/resource → refused; user-declared
   HttpRequest → refused; clean hello_http → emits a server.

## Not in this slice (rest of the capstone)
S2 HTTP request parser (the pivot); S3 dynamic router; S4 concat/computed response;
S5 log-in-handler; S6 read-in-handler; S7 fetch-in-handler; S8 forked concurrency;
S9 mutable state. The service+effects refusal lifts incrementally as S5-S7 land.
