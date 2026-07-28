# Self-hosted HTTP service — slice 3: routing (if/else + text `==` on req fields)

S3 of the capstone. Goal: a handler that BRANCHES on a text comparison of a req
field against a literal — `resp = if req.path == "/" then HttpResponse{200,"home"}
else HttpResponse{404,"nope"}` — so the server returns different responses per
route. Evidence: NR=native.rs, VP=vexprparse.verbose, main `1b899c6`. Oracle: the
milestone shape below (NOTE 7: `hello_router.verbose` uses `and` + 3-way nested
else-if → S3 REFUSES it; it's an S3.1 wire-behavior reference, not an S3-emittable
oracle).

## The decision: INLINE byte-compare, NOT copy-to-region (scoping-corrected)
The S2-scoping assumed copy-to-region would let req fields flow into "the existing
text equality unchanged." **There is NO text `==` in the self-hosted emitter** —
`eval_ast_env`'s AstBin Eq arm (VP:5760) is purely numeric; the emittable text
primitive set is byte_at/length/substring/concat/le32/le64/read/fetch/min/max
(VP:5773), no streq/starts_with/==. So S3 must HAND-EMIT the byte-compare
regardless of representation. Copy-to-region (mmap a fixed region + rep-movsb the
field into a packed-span slot) only pays off for concat/length — that's **S4**.
For S3's `==`, compare the parsed field's (ptr,len) — already in callee-saved
**rbx (ptr), r14 (len)** from S2's parse — directly against an inline literal.
Zero region, zero mmap, only serves `==` (exactly all S3 needs). Copy-to-region
is deferred to S4, gated so S1/S2/S3 servers stay byte-identical.

## Prerequisite — AST accessors for AstIf + AstBin (small, mechanical)
The tramp currently pattern-matches AstStr (S1) / AstVariant (S2 record). S3's
handler result is an `AstIf`. None of these accessors exist (only ast_is_str,
ast_is_variant VP:21189, ast_is_num VP:21369, ast_field_base/fstart/flen
VP:11678-11800). Add, as mirrors of ast_field_*: `ast_is_if`, `ast_if_cond`,
`ast_if_then`, `ast_if_else`, `ast_bin_op`, `ast_bin_lhs`, `ast_bin_rhs`. (No
dependency on copy-to-region / mmap / any new text primitive.)

## Verify — widen the handler shape gate (review MUST-FIX 2+3)
`service_errors`' `shape_ok` (VP:13063) today demands `ast_is_variant(hresult)`.
S3 ADDITIONALLY accepts `ast_is_if(hresult)`. **Do NOT reuse `svc_body_ok` for
the cond-lhs or the arms** — it accepts an AstStr (via its is_str escape) and an
AstField echo body, both of which S3's static emit CANNOT serialize (they'd
silently emit garbage / empty bodies). Factor the FIELD-ONLY predicate out of
svc_body_ok (base = AstVar == input-param name, fname = method|path) into a
helper `svc_field_ok`, and use `ast_is_str` separately. Then:
- cond = `AstBin(ast_bin_op==1 [Eq], lhs, rhs)` where `svc_field_ok(lhs)` (the
  compared req field — AstField SPECIFICALLY, not svc_body_ok's is_str escape:
  a `"a" == "/"` lhs must be REFUSED, else the field-select runs on an AstStr and
  emits a spurious compare = a compiler-axiom violation) AND `ast_is_str(rhs)`.
- BOTH arms: `ast_is_variant + vfield_len==2 + ast_is_num(status field) +
  ast_is_str(body field)` — the record-level check inlined in shape_ok (VP:13063),
  NOT svc_body_ok (which checks the body field only). S3 arms are LITERAL
  responses; **an AstField echo arm (`then HttpResponse{200, req.path}`) must be
  REFUSED and deferred to S4** — otherwise service_response_blob computes bodylen
  0 (ast_str_start(AstField)=0, VP:21455) → silent `Content-Length: 0` empty body.
  The EXISTING test `self_hosted_service_verify_gate` case f5 (NR:24000) is
  exactly this field-echo-arm shape — it MUST STAY REFUSED (relabel its reason:
  "field-echo arm = S4", not "if/else = S3"). Add a genuine LITERAL-arm accept.
Zero lets, as S1/S2. Anything else (concat cond/arm, `and`/`or` cond, non-`==`
op, req.body, computed status, AstField arm body) → refuse with an S3.1/S4
breadcrumb.

## Emit — a THIRD branch in x86_service_tramp (ast_is_if), fixed-byte style
`x86_service_tramp` (VP:27196) forks: AstStr → S1 blob (verbatim); AstField → S2
parse+runtime-serializer (verbatim); **AstIf → the new S3 path.** Not a general
AST walk — pattern-match `AstIf(AstBin(==, AstField(req,F), AstStr(L)), RecA,
RecB)` and hand-emit fixed bytes:
1. **Parse** (reuse S2's SCANNING CORE, but RECOMPUTE the jumps — MUST-FIX 1):
   method/path scanned off [rsp]; F selected into rbx/r14. **Field-select driven
   from the COND's lhs, NOT a body field** (SHOULD 4): `is_method` from
   `ast_field_fstart(ast_bin_lhs(ast_if_cond(hresult)))` — S2's line keys off
   body_ast (VP:27219), which is the whole AstIf here (→ garbage). **S2's parse
   BAKES its two parse-fail distances as constants (0x18f/0x169, VP:27225) at the
   close tail — which in S3 MOVES (after compare + both arms), so those constants
   would jump into an arm blob. ALL SIX jumps must be RECOMPUTED closed-form**
   (see Sizing) — "unchanged" is wrong.
2. **Byte-compare, LENGTH-FIRST** (the parsed span is NOT NUL-terminated, so the
   NUL-trick cmpsb NR:13330 does NOT port — use the `field == read` length-first
   shape NR:13445):
   - `cmp r14, <declen imm32>` ; `jne else_arm` (declen = `bytelit_decoded_len`
     of L, VP:6536 — the literal is DECODED, NOT raw span length; NOTE 8).
   - `cld` (0xFC — SHOULD 5: verbosec emits it before every cmpsb NR:13367; S3's
     cmpsb is the FIRST string op in a self-hosted server, don't rely on ambient
     DF) ; `mov rsi, rbx` ; inline the DECODED literal via jmp-over-data + `lea
     rdi, [rip-off]` (a small generic byte-span→blob helper — service_response_blob
     is response-specific) ; `mov rcx, <declen>` ; `repe cmpsb` ; `jne else_arm`.
     (Empty literal `== ""`: declen 0 → cmp sets ZF, cmpsb rcx=0 no-op preserves
     ZF — works, no guard.)
3. **then arm**: write the PRECOMPUTED S1-style blob for RecA (service_response_blob
   VP:27111 — status folded at emit time, fully static) → `write(r13, blobA,
   rlenA)` → jmp to close.
4. **else_arm**: same for RecB → `write(r13, blobB, rlenB)`.
5. close(r13) ; jmp accept_top (shared tail).
Both arms are precomputed static blobs (no runtime itoa — the bodies are literals
with compile-time lengths); only the CONDITION is runtime. Registers: rbx/r14
(field), rsi/rdi/rcx (cmpsb scratch), r12/r13 (fds) untouched.

## Sizing one-truth
`service_tramp_size` (VP:27145) adds `else if ast_is_if then (startup + accept +
read + parse + compare_block(lit_len) + branch) + blockA + blockB` where
`blockA/B = 4*((response_len(arm)+3)/4)` (response_len VP:27034, closed-form —
both arms literal). The compare_block size depends on lit_len (the inline literal
+ the `mov rcx, imm`) — compile-time constant. It is now a SUM over two arms
(the change from S2's single term). ONE-TRUTH: the same ast_is_if size expression
feeds blob_end_off (VP:25991, has_service position) AND the emit; every forward
rel32 (`jne else_arm` distance = then-block size; then→close jmp) is closed-form.
THE SIX JUMPS (MUST-FIX 1 — every distance closed-form, sourced from the SAME
blockA/blockB/compare lets the emit uses, NONE bakeable like S2's constants):
(1) method parse-fail `je`→close = `compare + blockA + then-write + blockB +
else-write`; (2) path parse-fail `je`→close = same; (3) length-mismatch
`jne`→else = `compare-tail + blockA + then-write`; (4) cmpsb-mismatch `jne`→else
= same; (5) then-arm `jmp`→close = `blockB + else-write`; (6) close back-edge
`jmp`→accept_top = the whole loop body, negative.
TWO HAND-SYNCED COPIES (SHOULD 6): service_tramp_size (number) and
x86_service_tramp (bytes) independently encode the S3 layout — no shared
sub-helper across the number/bytes split. The ARM BLOCKS are single-sourced via
response_len/service_response_blob; only the fixed glue + the 6 jumps are
dual-encoded (same discipline as S1/S2's hand-derived 224/607). A mismatch is
CAUGHT LOUDLY: service_tramp_size feeds blob_end_off → p_filesz (VP:26193); a
wrong p_filesz truncates the mapped segment → the server SIGSEGVs on spawn,
failing the milestone. MANDATE: script-assert each fixed run + an explicit
emitted-byte-count assertion.

## Eval
No interpreter path (service is an entry). The handler AstIf is walked only at
emit time. Fixed point: self-source has no service → has_service=0 → branch dead →
gen1==gen2 (VP:27185).

## Milestone / test (routing — two requests, two responses)
Rust test (spawn/poll/kill): compile a router service (handler `resp = if
req.path == "/" then HttpResponse{200,"home"} else HttpResponse{404,"nope"}`,
ephemeral port) via gen0 → server ELF; spawn; poll connect:
- `GET / HTTP/1.0\r\n\r\n` → `HTTP/1.0 200 OK\r\nContent-Length: 4\r\n\r\nhome`.
- `GET /x HTTP/1.0\r\n\r\n` (SAME server) → `HTTP/1.0 404 OK\r\nContent-Length:
  4\r\n\r\nnope` (proves the branch is runtime, per-request).
- a `req.method == "GET"` variant router (the method-based form).
- length-first correctness: a path `/xy` must take the else arm (len 3 ≠ 1),
  and `/` exactly the then arm (proves the length check, not a prefix match).
- hand-run: I spawn the emitted router and curl `/` and `/x` myself.

## Gate (clean disk)
1. Proofs check out; suite green; existing binaries byte-identical (S1 AND S2
   servers unchanged — only a new ast_is_if branch).
2. two_generation gen1==gen2 + composite demo green.
3. MILESTONE: the router cases above, verified incl. a hand-run.
4. Verify pins: `if req.path == "/" then RecA else RecB` emits; `req.method ==`
   variant emits; concat in an arm → refused (S4); `and`/`or` cond → refused
   (S3.1); non-`==` op → refused; req.body in cond → refused; S1 literal +
   S2 echo services still emit byte-identically.

## Explicitly deferred
S3.1 (`and`/`or` of two compares — `req.method == "GET" and req.path == "/"`);
S4 (concat/computed responses over req fields → the copy-to-region slice: the
service tramp mmaps a fixed region, rep-movsb method/path into a packed-span slot,
then concat/length consume them unchanged); nested if/else beyond two arms;
S5-S7 effects in handlers; S8 forked; S9 state.
