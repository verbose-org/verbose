# Effects tier slice 3 — `connection` + `fetch` in the self-hosted compiler

REVISED after adversarial review (2026-07-25). Architecture VERIFIED sound
(region math, syscall/sockaddr parity vs verbosec, htons/IPv4 arithmetic, the
TcpListener milestone). Seven amendments folded in — all additive, none change
the architecture; each is the difference between the first attempt compiling its
own oracle or not. The first OUTBOUND syscall family in the self-hosted subset;
structurally slice 1 (`read`) pointed at a socket. Evidence as
vexprparse.verbose:line at main `4e124a3` (the doc's earlier `4a128a1` pin was
stale — line refs will drift, names are stable). Oracle:
`examples/health_check.verbose` — `connection upstream` (host "127.0.0.1", port
19000, max_response 1024, on_connect_error abort), rule `check_health` with
`body = fetch(upstream, "GET /health HTTP/1.0\r\n\r\n")` and `reads:[upstream]`.

## Goal
gen1 compiles a connection+fetch program to an ELF whose runtime behavior
matches verbosec `--native`: same stdout against the same live listener, exit 1
(fail-closed) when the endpoint is unreachable.

## Value model — the read(–) pattern, verbatim (VERIFIED region math)
- Connection j owns a 64 KiB slot at region offset **0x500000 + j*0x10000**.
  Resources occupy 0x400000..0x500000 (base 0x400000 stride 0x10000, capped ≤16
  at :11395 — so resources end EXACTLY at 0x500000, zero slack: pin this
  coupling in a comment + test; a future resource-cap lift silently collides).
  `[slot+0..8)` packed value cell; `[slot+8..)` response bytes. Refusals:
  `max_response > 65520`, > 16 connections.
- mmap growth (both channels, slice-1's `le32(region_len)` split): **`n_conn` =
  the DECLARED count** (`connection_list_len`), NOT the referenced count —
  because slot index = declared index (mirror of resources: length uses
  resource_list_len :24597 while the mmap GATE is referenced-ness `rms>0`). So:
  region_len = `0x500000 + connection_list_len*0x10000` when any connection is
  referenced, else the slice-1 formula UNCHANGED (byte-identity — SHA gate).
  argv's default 0x100000 mmap MUST grow (health_check routes to the argv
  channel — Tick is number-only). Max last byte = 0x500000 + 16*0x10000 − 1 =
  0x5FFFFF, inside the region; the arena is a SEPARATE kernel-chosen NORESERVE
  mapping (r15 base) — cannot collide.
- **Trampoline connection block** — after `x86_resource_marshal`, before the
  trampoline `call` branch (elf_program_src concat order ~:24851; keeps every
  `call` rel32 constant — the :24390 invariant). Once per exec, per referenced
  connection. Shape MIRRORS `x86_resource_block` (:24473) — ZERO stack ops (so
  [rsp] survives, though that only matters if reactions were allowed — they
  aren't, below):
  ```
  socket(2,1,0)           ; rax=41 ; args rdi=2 rsi=1 rdx=0
  test rax,rax ; jns +12 ; exit(1)     ; INLINE abort (house shape, NOT a
                                       ; shared patch tail — 48 85 c0 79 0c
                                       ; bf 01 00 00 00 b8 3c 00 00 00 0f 05,
                                       ; 17 B, per :24473 / argc-guard :24396)
  mov rbx, rax            ; fd in RBX (r15=arena base, UNAVAILABLE; rbx is the
                          ; established fd home — syscalls clobber only
                          ; rax/rcx/r11, comment :24143)
  connect(rbx,&sa,16)     ; rax=42 ; sockaddr INLINE via e9+le32 jmp-over-data
                          ; (house form, NOT EB imm8), 16 B:
                          ; 02 00 | htons(port) LE | 4 addr bytes | 8 pad
  test rax,rax ; jns +12 ; exit(1)
  write(rbx, req, req_len); rax=1 ; request bytes INLINE (e9+le32), UNESCAPED at
                          ; compile time via emit_bytes_data (\r→13 from 2a,
                          ; 4-padded)
  test rax,rax ; jns +12 ; exit(1)     ; (write short-write undetected —
                                       ; verbosec parity)
  read(rbx, slot+8, max)  ; rax=0 ; single-shot, short-read-becomes-len (no
                          ; EINTR retry — verbosec parity)
  test rax,rax ; jns +12 ; exit(1)
  <store len> ; close(rbx) best-effort
  pack ((0x20000000 + slot_off + 8 − src_base) << 32) | len  → [slot+0]
  ```
  FOUR fallible syscalls → 4 × 17-byte inline aborts; connection_marshal_size
  must count them. on_connect_error: abort is the ONLY accepted policy.
- **`fetch(name, req)` expression site**: `span_is_fetch` dispatched in the same
  FOUR arms `read` uses — `x86_node` (:22150, value: movabs slot_addr / mov
  rax,[rax] / push, 14 B), `code_size_node` (:21803, 14), `x86_stream_node`
  (:22931, streaming unpack-and-write, 50 B), `code_size_stream_node` (:22429,
  50). The request arg is consumed at TRAMPOLINE time; the expression site
  ignores it (arity: exactly 2 args, second AstStr).

## State threading — THE largest, most dangerous part (review MUST-FIX; was absent)
A `ConnectionList` must ride, mirroring slice 1's `ress`:
- `ByteGenState` `conns` field — threaded across EVERY `ByteGenState { ... }`
  construction (the read arms at :21803, :22150, :22429, :22931 are the template;
  ~150-300 mechanical edits, slice-1 scale).
- `ProgGenState` `conns` field — `blob_end_off` (:24197 constant chain) adds
  `connection_marshal_size`.
- **THE TRAP (do NOT copy the read arm's dodge)**: the deep sites in
  `x86_node`/`x86_stream_node` reconstruct `ProgGenState` with `rxs:
  ReactionList::RxNil` HARDCODED (:22150, :22931). That is safe for `rxs` ONLY
  because reaction programs refuse every src_base construct. For `conns` a
  `ConnNil` hardcode is **NEVER safe**: blob_end_off depends on
  connection_marshal_size, and any connection+text program (health_check's fetch
  stream arm embeds src_base) would compute src_base short → every span reads
  garbage. Thread `bg.conns` for real at every deep reconstruction; no sentinel.
- `ConnErrState` joins verrs (~:24816), mirror of RxErrState.
- **Request-extraction walker** (new, ~6-rule recursive family): the trampoline
  needs each connection's fetch REQUEST literal, which lives in the RULE BODIES,
  not the declaration (verbosec's `first_fetch_for`, native.rs:2770). Walk
  rules→binds→args→arms for `fetch(<name>, AstStr)`; mentions-once, tolerant
  bases (runs on sentinels under eager lets — the 2b lesson).

## Parse
`Connection = MkConnection of (name span, host span, port, max_response)` +
ConnectionList; `parse_connection_decl` (positional: `host:` string literal,
`port:` number, `max_response:` number, `on_connect_error:` abort-only); malformed
→ host_len:-1 sentinel. **`connection` joins FOUR skip sets** (review MUST-FIX,
not one): parse_program is_skip AND parse_concepts AND parse_resources AND
parse_reactions — each is an independent token-stream walk; an unskipped
`connection` block corrupts those captures (slice-1 precedent :6849-6850).

## Verify — the full surface (review: several were unlisted)
DISPATCH INVENTORY — `span_is_fetch` (and where noted, the endpoint/purity
checks) must be added in ALL 13 rules that handle `span_is_read`, each with its
line + the mentions-once discipline (2nd-review: naming them prevents the
verify↔emit drift scar): eval_ast_env:5721, span_is_primitive:7029,
count_reads_named_ast:10900, count_bad_reads_ast:11099, ast_uses_src_base:11451,
count_undef_ast:12675, call_result_type:14111, count_undecl_read_ast:15971,
undef_span_ast:17871, code_size_node:21803, x86_node:22150,
code_size_stream_node:22429, x86_stream_node:22931. (count_bad_reads_ast is the
declared-resource shape check; count_undecl_read_ast is the reads:-membership
check — fetch needs a sibling term in BOTH, plus the connection-specific
`connection_errors` for host/port/dup/collision.)

- `call_result_type` (:14111) — add a `span_is_fetch` term typing fetch as TEXT,
  beside read's. WITHOUT this, fetch → find_rule sentinel → number(0), and
  health_check's `body : text = fetch(...)` type-mismatches → verrs>0 → the
  self-verify gate refuses emitting the oracle. (MUST-FIX — this alone blocks the
  milestone.)
- `count_undef_ast` (:12675) AND `undef_span_ast` (:17871) — exempt fetch's
  first arg (`upstream` is a bare name, not a variable — mirror read's
  exemption at :12673). Without both, `upstream` is flagged undefined → refusal.
- `span_is_primitive` (:7029) — fetch joins (badcall exemption; primitives fail
  rule_named so calls-coverage never flags them, :7017).
- `fetch(<name>, _)` with undeclared `<name>` → diagnostic.
- **Audit-integrity checks verbosec enforces (2nd-review MUST-FIX — were absent)**:
  (a) DUPLICATE connection name → diagnostic (verifier.rs:128-131).
  (b) connection name COLLIDING WITH A RESOURCE name → diagnostic
  (verifier.rs:136-138). CRITICAL for gen1: the purity pass resolves `reads:`
  entries against BOTH namespaces, so a shared name lets one `reads:` entry
  satisfy both checks → the audit surface LIES about what the program touches.
  Pillar-1 concern — must be an active check, not an omission.
  (c) **ONE fetch per connection per rule → active diagnostic** (verifier.rs:237),
  NOT merely a "not in this slice" absence. The trampoline is find-FIRST (one
  request per connection, from the request-extraction walker), so a second fetch
  site on the same connection would SILENTLY receive the first request's
  response — silent-wrong-data, not a refusal. This is also what makes
  connection_marshal_size well-defined (exactly one request literal per
  connection to size).
- Purity: a fetch site requires `name` in `reads:` — widen `count_undecl_read_ast`
  (:15971, the reads:-membership check — NOT count_bad_reads which is the
  declared-resource check) with an additive fetch term. verbosec parity: `reads:`
  greps every endpoint.
- **`ast_uses_src_base`'s AstCall arm (:11451) — fetch joins it** (review
  SHOULD, corrected mechanism). fetch is definitionally src_base-dependent (its
  pack is src_base-relative). Membership makes `reaction_errors` (:11783) refuse
  fetch+reactions AUTOMATICALLY, exactly as read+reactions is refused today — the
  correct, minimal, precedent-following guard. (The earlier draft's separate
  "connections+reactions structural refusal" was over-restrictive — it'd refuse a
  declared-but-unreferenced connection beside a reaction — and rationalized by a
  non-hazard: the connection block has zero stack ops, so FACT-1 is NOT
  threatened. Keep a structural check only as belt-and-suspenders, if at all.)
- Request must be an AstStr LITERAL (concat-of-literals deferred — narrower than
  verbosec 11.1, breadcrumbed). Host = 4 octets 0..255; port 1..=65535;
  ≤16 connections; on_connect_error abort only.
- **Documented divergences from verbosec (2nd-review — narrower, state + pin, do
  not silently diverge)**: (i) `on_connect_error` is OPTIONAL in verbosec
  (default abort, parser.rs:2101); gen1's positional parse (mirror of
  parse_resource_decl :10159) makes the line MANDATORY — a verbosec-valid
  connection omitting it is gen1-refused. Same class slice 1 introduced for
  on_read_error; pin it. (ii) `max_response` ceiling: verbosec accepts ≤64 MiB
  (verifier.rs:754); gen1 refuses `> 65520` (the 64 KiB slot). Honest capacity is
  actually 65528 (connections store only the response — no path+NUL like
  resources); mirroring the resource cap 65520 for uniformity is fine but SAY so.
- **Blob-inclusion gate (2nd-review minor c)**: fetch's request literal lives in
  the blob, so the src_blob must be emitted. Slice 1 ORed `uses_res` into the
  blob gate BY CONSTRUCTION (:24834) precisely because "texty anyway" is luck.
  OR `program_uses_connections` into the same gate — do NOT rely on the request
  being an AstStr making the program incidentally texty.
- **input-less entry + fetch REFUSED** (no region mmap exists — slice-1 class).
- Fixed point untouched BY CONSTRUCTION: self-source has zero `connection `
  items / `fetch(` sites (only the English word in a comment :14486) → every new
  walk returns 0.

## Compile-time endpoint constants
- IPv4 walker: 4 dot-separated decimal octets 0..255 → 4 address bytes in SOURCE
  order (= network order for dotted quads; "127.0.0.1" → 7F 00 00 01). Malformed
  → diagnostic (no DNS/IPv6/localhost — verbosec refuses too, permanent posture).
- htons(port) = `(port%256)*256 + port/256`, STORED LITTLE-ENDIAN in the struct
  (the le32/le64 packing does this) → the big-endian wire pair. Worked: 19000 →
  0x384A LE → bytes 4A 38 = verbosec's `to_be_bytes(19000)`. **State the LE
  coupling** so nobody emits the pair big-endian and double-swaps.
- Walkers: mentions-once (2^N), tolerant base cases.

## Eval
`fetch` → empty-span text sentinel `VText{0,0}` (:5721 read precedent) — the
compile-only split; the interpreter opens no sockets.

## Sizing one-truth
`connection_marshal_size` = Σ per DECLARED-and-referenced connection (fixed
block + 4×17 inline aborts + 16 sockaddr + 4-padded(req_len_decoded)) — ONE
helper for blob_end_off AND emit. Gated on program_uses_connections; zero when
none (byte-identity).

## Gate (clean disk)
1. Proofs check out; suite green; ALL existing binaries byte-identical.
2. two_generation gen1==gen2 + composite demo green.
3. MILESTONE (native.rs:31286 TcpListener precedent): bind an EPHEMERAL port,
   bake it into a temp health_check-shaped `.verbose` (rewrite, not the fixed
   19000 — parallel-test-safe), compile via verbosec `--native` AND gen1, run
   BOTH against the listener (accept loop, 2 accepts, sequential) → stdout
   identical; then drop the listener → both exit 1, empty stdout (fail-closed).
   Composition probes: `length(fetch(...))` (value arm), `concat(..., fetch(...))`
   (stream arm — concat is stream-only, x86_node concat = int3).
4. Verify pins: undeclared connection → diag; fetch without `reads:` → purity
   violation; non-literal request → refused; malformed host / port 0 / oversized
   max_response → refused; fetch + reaction → refused (via ast_uses_src_base);
   clean health_check → 0 diags.

## Not in this slice
concat request bytes; fetch in reactions / service handlers; multiple fetches
per connection; DNS/IPv6 (permanent refusal).
