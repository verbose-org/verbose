# Self-hosted HTTP service — slice 4: concat response body over req fields

S4 of the capstone. Goal: a handler whose response BODY is built at runtime from
literals + a req field — `resp = HttpResponse { status: 200, body: concat("you
asked for ", req.path) }` — so the server returns request-dependent bodies.
Evidence: NR=native.rs, VP=vexprparse.verbose, main `2724e74`.

## The mechanism: extend S2's serializer (Path B) — NOT copy-to-region
Scoping-corrected. The self-hosted emitter has NO value-position (materializing)
concat — `x86_node`'s concat arm is a 1-byte int3 trap (VP:24536); `length(concat)`
traps too; the only working concat STREAMS to fd 1 with no length prefix
(x86_stream_node VP:25258). So copy-to-region (mmap a region + rep-movsb the field
into a packed-span slot) would make req fields *representable* but STILL wouldn't
give a response body — you'd still need the client fd r13 + a runtime
Content-Length. Copy-to-region + a materializing concat is a LATER, LARGER piece,
needed only when a handler feeds req fields into GENERAL text computation
(substring / byte_at / nested concat).
The minimal S4 is instead a direct extension of S2's serializer (VP:27690, the
AstField echo branch): S2 already parses method/path, selects one field into
**rbx=ptr / r14=len**, itoa's r14 as Content-Length, and writes the body to r13.
S4 generalizes the "body" from one field to an ORDERED SEQUENCE of concat args
streamed to r13, with Content-Length = Σ(literal lengths, compile-time) + r14
(runtime, one field). No region, no mmap, no src_base dependence (inline literals
are RIP-relative like S2 — which also DODGES the Path-A trap that src_base is
computed with `svcs: SvNil`, excluding service_tramp_size, VP:27765).

## Prerequisite — the concat discriminator (MUST-FIX 1)
The ACTUAL fork in x86_service_tramp is `if ast_is_str(body_ast)→S1 ; else if
ast_is_if(hresult)→S3 ; else →S2 (AstField)` (VP:27690) — S2 is the `else`
FALLTHROUGH, not an explicit AstField branch. A concat body has
`ast_is_str==0` and `ast_is_if==0`, so WITHOUT a new discriminator it falls into
the S2 else and runs `is_method = span_is_method(ast_field_fstart(body_ast))` on
an AstCall node → garbage → the server echoes a bogus field, not the concat (the
milestone fails, silently). Insert a concat discriminator BEFORE the S2 else in
BOTH x86_service_tramp AND service_tramp_size, identically. There is no
`ast_is_call` accessor today; the cheapest discriminator uses existing accessors:
`arg_list_len(ast_call_args(body_ast)) > 0` — `1` for AstCall(concat), `0` for
AstField (whose ast_call_args is ArgNil, VP:25694). (Or add an `ast_is_call`
mirror of `ast_is_if`.) Accessors reused: `ast_call_args` (VP:25680),
`arg_first`/`arg_rest`/`arg_list_len`, `bytelit_decoded_len` (VP:6536) — the
`x86_rx_cargs` concat-arg walk (VP:26451) is the template.

## Emit — a fourth body-shape branch in x86_service_tramp
Fork: `ast_is_str`→S1 (verbatim) ; `ast_is_if`→S3 (verbatim) ; **concat
discriminator→ the NEW S4 path** ; else→S2 AstField serializer (verbatim). The S1
/ S3 / S2 emitted bytes stay UNCHANGED (byte-identity — pins
self_hosted_service_constant_response / _echo_path / _router catch a regression).
S4 emit (after S2's parse, which leaves method/path available; select the concat's
req-field arg into rbx/r14 — drive `is_method` from the CONCAT ARG's AstField, not
a top-level body field, the S3 lesson):
1. **Content-Length** = `const_sum` (Σ bytelit_decoded_len of the AstStr args,
   compile-time) + r14 (if a field arg is present): `mov rax, <const_sum imm32>`
   ; if field present `add rax, r14` ; itoa-tail → r13 (the S2 itoa tail, fd=r13).
2. **Status line**: `write(r13, "HTTP/1.0 ", 9)` ; `mov rax, <status imm>` +
   itoa → r13 ; `write(r13, " OK\r\nContent-Length: ", 21)` ; (the Content-Length
   itoa from step 1 goes HERE — reorder so the length is emitted between the
   literals, matching S2's serializer order) ; `write(r13, "\r\n\r\n", 4)`.
3. **Body — stream each concat arg IN SOURCE ORDER**: AstStr arg → inline the
   DECODED bytes (jmp-over-data + `lea rsi,[rip-off]`) → `write(r13, rsi, declen)`;
   AstField(req,F) arg → `write(r13, rbx, r14)`. (The generic byte-span→blob
   helper from S3 places each decoded literal.)
4. close(r13) ; jmp accept_top.
Registers: rbx/r14 (field), rsi/rdi/rdx/rax (write/itoa scratch), r12/r13 (fds)
untouched. r14 SURVIVES field-select → CL itoa → final field write (callee-saved;
the S2 itoa tail uses rax/rcx/rsi/rdi/rdx/rsp, NOT r14/rbx — review-confirmed, no
aliasing). EXACTLY ONE req field per concat in S4 (S2's parse selects one; ≥2
fields — holding both method and path — defers to S4.1). The per-AstStr-arg write
block (`lea rsi,[rip-off]` + `mov rdx, declen` + `write(r13,…)`) is NEW (NOTE 6:
emit_bytes_data places the 4-padded decoded bytes, but the write-to-r13 sequence
is not a verbatim S2/S3 reuse).

## Sizing — TWO HAND-SYNCED WALKS (MUST-FIX 2: not "one-truth")
There is no shared size expression — `service_tramp_size` (a NUMBER) and
`x86_service_tramp` (BYTES) are independent rules across the number/bytes split.
S4 adds `svc_cargs_size` (number, sums per-arg block sizes → feeds blob_end_off →
p_filesz) AND `x86_svc_cargs` (bytes, emits the per-arg writes), HAND-SYNCED
arg-for-arg — exactly the reaction precedent `rx_cargs_size` (VP:26220) /
`x86_rx_cargs` (VP:26443) ("mirrors arm-for-arm so size and emit cannot drift",
VP:26464). Per-arg is closed-form (AstStr → 4-padded bytelit_decoded_len block +
fixed write; AstField → fixed write(r13,rbx,r14); status/CL itoa → fixed —
`rx_carg_size` VP:26166 is the shape). THE BIGGEST RISK (every prior service
slice's SIGSEGV-class bugs lived here): the two walks must agree TO THE BYTE, or
p_filesz truncates the mapped segment → the spawned server SIGSEGVs on spawn.
MANDATE: script-assert each fixed run + an explicit emitted-byte-count assertion.
JUMPS (MUST-FIX 3 — enumerate, each closed-form, sourced from svc_cargs_size):
the two parse-fail `je`→close and the close→accept_top back-jump distances now
span the ENTIRE variable-length body stream (status-line + CL + framing + Σ
per-arg write blocks) → recompute from svc_cargs_size (like S3's six jumps); each
AstStr arg additionally needs its OWN `e9 le32` jmp-over-data (like S3's l4 at
VP:27690).

## Verify — widen svc_body_ok
`svc_body_ok` (VP:13330, reached from service_errors VP:13452) accepts today
`AstStr` (S1) or `AstField(req, method|path)` (S2). S4 ADDITIONALLY accepts
`AstCall(concat, args)` (callee-name check via span_is_concat VP:24536) where
EVERY arg is `AstStr` OR `svc_field_ok` (the S2 field predicate: AstField, base
AstVar==input-param, fname method|path) AND — **MUST-FIX 4 — EXACTLY ONE arg is
an AstField**. Zero-field all-literal concat (`concat("a","b")`) is REFUSED with a
breadcrumb "constant body → use a string-literal body (S1)" (it's under-defined
here: the parse+field-select would fire on a non-field arg → garbage rbx/r14).
**MUST-FIX 5**: the `rx_cargs_ok` template (VP:13719) only checks every-arg-ok
with NO counter — S4 needs a new `svc_cargs_ok` with a field-COUNT accumulator
(exactly-one), not just per-arg shape. Refuse (S4.1/S5 breadcrumb): number args,
nested concat, non-concat calls, req.body, ≠1 field arg, computed status. Zero
lets. `svc_arm_ok`/`svc_if_shape_ok` (S3 router arms) stay literal-only (concat in
a router arm = S3×S4, deferred). Reserved-name / one-service / port checks
unchanged.
TYPE-CHECK (NOTE 7 — confirm, no new work): the handler runs through tcheck_rule
(VP:16824); S3's `field_ty_of` synthetic-field wildcard (name_len==0 → text)
makes `req.path` type as text (2), and `concat(text-lit, text-field)` types text
via concat_args_ty. Milestone MUST assert gen0 does NOT refuse the concat handler
with a type error (a regression signal on the S3 fix).

## Eval
No interpreter path (service is an entry; the concat body is walked only at emit
time). Fixed point: self-source has no service → has_service=0 → branch dead →
gen1==gen2 (VP:27613). The handler is emitted as a normal dead proc by x86_program
(VP:25994); its concat field lowers to the 1-byte `\xcc` trap (VP:24536), size
matching code_size_node's 1 (VP:24158) — dead, never called, size-consistent.

## Milestone / test (spawn/poll/kill)
Compile a service with handler `resp = HttpResponse { status: 200, body:
concat("you asked for ", req.path) }` (ephemeral port) via gen0 → server ELF;
spawn; poll connect:
- `GET /foo HTTP/1.0\r\n\r\n` → `HTTP/1.0 200 OK\r\nContent-Length: 18\r\n\r\nyou
  asked for /foo` (14 + 4 = 18).
- `GET /a HTTP/1.0\r\n\r\n` (same server) → `Content-Length: 16 ... you asked for
  /a` (runtime length tracks).
- a field-THEN-literal variant `concat(req.path, " ok")`.
- a `req.method` variant.
- hand-run: I spawn the emitted server and curl a path myself.

## Gate (clean disk)
1. Proofs check out; suite green; existing binaries byte-identical (S1/S2/S3
   servers unchanged — only a new body-shape branch).
2. two_generation gen1==gen2 + composite demo green.
3. MILESTONE cases above, incl. a hand-run.
4. Verify pins: concat(lit, req.path) emits; concat with a number arg → refused;
   nested concat → refused; ≥2 field args → refused (S4.1); concat in a router arm
   → refused (S3×S4); S1/S2/S3 services still emit byte-identically.

## Explicitly deferred
S4.1 (≥2 req fields in one concat — hold both method and path); computed status
(`status: if … then 200 else 404`); concat inside a router arm; and the general
COPY-TO-REGION + materializing value-position concat (concat→region bump→packed
span) needed for substring/byte_at/length/nested-concat over req fields — the
template already exists for argv text (x86_marshal_fields ty==2 VP:27714), a
service just doesn't mmap a region today. S5-S7 effects in handlers; S8 forked;
S9 state.
