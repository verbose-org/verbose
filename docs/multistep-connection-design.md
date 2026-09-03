# Multi-step `raw_tcp` connections — design note (slices `rawtcp-inspect-0` and `multistep-1`)

> **Status: DESIGN ONLY. Nothing in this note is implemented.** Written against `main = c5e46b0`,
> clean tree. Every `file:line` below was re-grepped at that commit — line numbers in
> `src/native.rs` shifted this week, so none is remembered.
>
> **Revision 2 (adversarial review verdict on revision 1: "do not commit — redesign the middle").**
> Three code premises revision 1 built on were false. §3 states them plainly and keeps them there,
> because the project keeps its scars. The outer shape survived; the middle is rebuilt, and the
> single slice became two.

---

## 0. Executive summary

A `service` today handles **one exchange per connection**, and a `raw_tcp` handler cannot compute
anything at all. A framed stream protocol — TLS is the loud consumer, but the language needs it for
any length-prefixed or line protocol — is the opposite shape: read a frame → respond → read again →
… → close, carrying per-connection state across those steps.

**Revision 1 proposed one slice. Reading the code says two, and the first one is not about
connections.**

- **Slice 0 — `rawtcp-inspect-0`: a `raw_tcp` handler can inspect its input.** Today it cannot. The
  verifier's byte-addressed operand gate admits only a `b"…"` literal and a `random()` draw
  (`src/verifier.rs:2888`), so `byte_at(req.data, 0)` on a `bytes` FIELD is a *verify error*; there
  is no bytes `substring`; and the emitter has no rbp frame to put an offsets map in
  (`emit_raw_tcp_echo_bytes`, `src/native.rs:20217`, contains **zero** occurrences of `rbp` or the
  `0x55` push — measured). So a `raw_tcp` handler can pass its input through or wrap it in a bytes
  `concat` of literals and `le32`/`le64`, and nothing else. Slice 0 gives the emitter the HTTP frame
  discipline, registers the input field as a composite-keyed BoundText pair, and widens the
  `byte_at` / `length` gate to a bytes FIELD. **It is a language capability with its own worked
  example, one read and one write, no loop** — any framed protocol needs it, and so does
  self-hosting's binary parsing.

- **Slice 1 — `multistep-1`: the step loop + per-connection state**, on top of slice 0. A
  per-connection inner loop around read → handler → write, forked-only, exiting on EOF, on a
  declared `max_steps`, and on a declared `read_timeout`. **It reuses `state:` / `after:` rather
  than inventing `session:` / `next:`** (§6.2 re-argues this on the *accepted-program* set, which is
  what revision 1 got wrong), and it needs **no new init site**: the shipped `STATE INIT`
  (`src/native.rs:21268`) already runs once before `listen`, and `fork()` hands every child a
  copy-on-write copy — so "reset per connection" costs zero instructions.

**What neither slice does is unblock TLS.** §7 enumerates the eight things still missing and is
blunt that the connection machinery is one of them, not the last of them.

---

## 1. The gap, and three non-TLS consumers

The project rule is that applications never drive language design (`CLAUDE.md`, "POCs do not drive
language features"). TLS is the proof, not the reason.

### 1.1 What exists

Both protocols are one exchange per connection *as a handler contract*, though for different
reasons:

- **`Protocol::Http10`** — `emit_http10_dynamic_bytes` does, per accept: optional `clock_gettime`,
  non-cached resource reads, fork dispatch (`src/native.rs:21461`), **one** `read`
  (`src/native.rs:21596`), HTTP parse (`:21612`), connection fetches, entropy draws (`:21729`),
  handler lets (`:21748`), the handler body, `log:` blocks, **one** response write
  (`emit_http_serialize`, `:21926`), the `after:` block (`:21929`), `close` (`:22027`), then either
  `lea rsp, [rbp - frame_size] ; jmp accept_top` or, forked, `sys_exit(0)` (`:22039`).
- **`Protocol::RawTcp`** — `emit_raw_tcp_echo_bytes` (`src/native.rs:20217`) **does loop reads on
  one connection**: `echo_top` (`:20295`) is `read → test rax,rax → jle close_client (:20305-20306)
  → write → jmp echo_top (:20317)`, with `close_client` at `:20323` closing and jumping back to
  `accept_top` (`:20284`). But the handler is pinned to the identity shape by `check_raw_tcp_binding`
  (`src/verifier.rs:2193`, called at `:1166` and `:1175`) — input and output each a Named concept
  with exactly one `bytes [..max_request]` field whose bound equals `max_request` — and
  `compile_service` then refuses a handler with `let` bindings (`src/native.rs:20500`) or a
  non-identity body (`:20516`). The loop echoes each chunk back with no transformation and no state.

Neither protocol can (a) run a real handler per step, (b) carry bytes from step N−1 into step N of
the same connection, or (c) treat an iteration as a protocol *frame* rather than whatever `read`
happened to return.

### 1.2 Three consumers, none of them TLS

- **A line-oriented command protocol (Redis-RESP / memcached / a toy KV store).** Read a command
  line, answer it, keep the connection, carry per-connection state: an authenticated flag a `LOGIN`
  sets and later commands read. The **line-framed, small-state** shape. Needs slice 0 to find the
  `\n` at all, and slice 1 for the flag.

- **A length-prefixed binary RPC (`[len:2][payload]`, or a ping/pong keepalive).** Each frame's
  length is in its header; a frame may span two reads and two frames may arrive in one read. The
  **binary-framed** shape, and the one that makes reassembly load-bearing. Slice 1's worked example
  is this shape, minus reassembly (§6.4).

- **HTTP/1.1 keep-alive.** Multiple exchanges on one connection is *the* difference between
  HTTP/1.0 (shipped, one-shot) and HTTP/1.1. It consumes the same step loop under a different
  protocol, and — see §6.2 — it is the one shape that would make a per-process and a per-connection
  lifetime observable **at once in the same program**, which is the honest argument for ever
  splitting `state:`.

A fourth argument has no consumer at all: **`raw_tcp` is the one protocol where the handler cannot
compute anything.** Every computational primitive the language has is inert inside it. That is a
hole in the uniformity of what a handler may compute, and closing it is slice 0's whole
justification — independent of loops, state, and TLS.

### 1.3 The self-hosting consumer

`examples/vexprparse.verbose` parses text. A binary protocol parser reads *bytes*: a length field,
a tag byte, a payload slice. Slice 0's `byte_at` / `length` over a bytes field is the same primitive
pair the text scanner already has (`scan_word.verbose`, `token_scan.verbose`), one type over. This is
named because it is the reason slice 0 stands alone rather than being folded into a connection
feature: **nothing in slice 0 mentions a connection.**

---

## 2. What already exists that this reuses — measured

Nothing below is proposed. All of it is at `c5e46b0`.

### 2.1 The accept-loop-with-inner-loop control flow

`emit_raw_tcp_echo_bytes` (`src/native.rs:20217`) is socket / `setsockopt(SO_REUSEADDR)` (`:20232`) /
bind / listen / read-buffer `sub rsp, imm32` (`:20277-20281`) / `accept_top` (`:20284`) /
`mov r13, rax` (`:20291`) / `echo_top` (`:20295`) / `close_client` (`:20323`) / `jmp accept_top`. It
is pinned at **358 bytes** by `phase7_service_matches_echo_probe_size` (`src/native.rs:37032`,
assertion at `:37053`), driven from `examples/raw_tcp_echo.verbose`.

**What transfers is the control-flow shape (~20 bytes of `jle` / `jmp` edges and the
`accept_top` / `close_client` labels), not the frame.** §3 (W3) is why.

### 2.2 The forked accept loop and its auto-reap

`ConcurrencyMode::Forked` (`src/ast.rs:395`) is a shipped service knob. The HTTP dispatch is at
`src/native.rs:21461`: `rt_sigaction(SIGCHLD, SIG_IGN)` once before `listen`, then per accept
`fork()`, parent `close + jmp accept_top`, child falls through, child tail `sys_exit(0)`
(`:22039`+). No `wait`, no zombies. It is refused for `raw_tcp` today
(`src/verifier.rs:1276-1281`, *"Phase 10: concurrency: forked currently restricted to http_1_0
services"*) — a **scope decision the comment says so in as many words**, not an impossibility.
`docs/effect-model.md`'s "fork() per accept" row documents the shape.

### 2.3 The state machinery, and the persistence property that is already load-bearing

Slice `text-state-1` gives a `state:` field its slots in the service frame:

- **Layout**: `state_slots_bytes` (`src/native.rs:21198`) — a Number field 8 bytes, a Text field
  `16 + ((N+7) & !7)`; offsets from a **descending cursor over `service.state_fields`, a `Vec`**
  (`:21220-21243`), never a HashMap, because those offsets reach emitted bytes; folded into
  `frame_base_fixed` (`:21214`) and thence `frame_base` (`:21250`).
- **Init**: `STATE INIT` at `src/native.rs:21268`, emitted **immediately after the prologue
  (`:21262-21266`) and before `setsockopt` (`:21333`) and `accept_top` (`:21452`)** — i.e. once at
  process startup, before the socket exists.
- **Mutation**: the `after:` block (`:21929`) **copies** with `rep movsb` and never aliases; the
  comment at `:21943-21949` records why (an alias returns the *next* request's bytes).
- **Read**: registered under the composite key `__state_<field>` (`:21247`, `:21779`), resolved by
  seven `Ident(_) if _ == "state"` base matches across the emitter (`src/native.rs:8124`, `:8892`,
  `:14186`, `:15297`, `:17480`, `:21057`, `:22354` — count measured by grep). The composite key is
  mandatory: `req.body` is registered under the **bare** name `"body"` (`:21676`), and the collision
  is measured, not hypothetical (`:21773-21778`).
- **Overflow**: proved at **compile time** by `text_source_worst_case` (`src/verifier.rs:1666`), with
  a 13-byte runtime backstop as defence in depth.

**The init placement is the finding that shrinks slice 1.** Revision 1 designed a "SESSION INIT,
child only, once per connection" emit site. It is unnecessary: `STATE INIT` runs in the parent
before `listen`, and `fork()` hands every child a copy-on-write copy of the initialised slots. So
under forked, **per-connection reset is free — it is the fork**, and slice 1 adds no init site at
all.

### 2.4 The randomness effect and where its draw sits

`emit_entropy_draw` places one `getrandom(318, flags 0)` per name per evaluation. In a service it is
at `src/native.rs:21729` — **after the fork dispatch and after the HTTP parse**, and the comment at
`:21730-21740` states why: a forked child must draw its own bytes, and a malformed request that is
dropped must not spend a draw. A draw in a `raw_tcp` handler is refused by name
(`src/native.rs:20481`, breadcrumb naming slice `entropy-2`); a draw inside an `after: set` is
refused by name (`src/verifier.rs:1404-1405`, breadcrumb naming `entropy-6`, on the grounds that
*"state is not secret"*).

### 2.5 The bytes value machinery

- `emit_write_bytes_literal(code, bytes, fd)` (`src/native.rs:7518`) is **fd-parameterised**.
- `emit_streaming_bytes_body` (`src/native.rs:7633`) streams a bytes expression left-to-right —
  `Bytes(b)` → literal blob, `Concat(..)` → each arg in order, `Le32`/`Le64` → an 8-byte stack
  scratch, `Random(name)` → the draw's registered `(ptr, len)`. Its doc comment says **"no buffer,
  no sizing pass"** (`:7626-7627`), and every call site passes fd `1`.
- A bytes `concat` requires **every** argument bytes-typed (`src/verifier.rs:3407-3420`).

### 2.6 What the interpreter and WASM do with services

Zero. `grep -c Service src/interpreter.rs src/wasm.rs` returns `0` and `0`; `Item::Service` is
dispatched only from `src/main.rs` to `native::compile_service`. Services are already native-only,
structurally. Neither slice adds a backend asymmetry; both inherit one.

---

## 3. Where the previous draft was wrong — three code premises, and a corollary

Revision 1's §4.2, §4.4, §5, §7 and §8.2 all rest on these. They are kept here rather than deleted
because the project keeps its scars, and because each was plausible from a summary of the code and
false in the code.

### W1 — a `bytes` FIELD is not inspectable. There is no bytes `substring`.

`check_byte_addressable_operand` (`src/verifier.rs:2872`) admits exactly two operands:

```rust
if matches!(expr, Expr::Bytes(_) | Expr::Random(_)) { return; }
check_expr_against(expr, &Type::Text, /* … */);
```

and its own comment (`:2881-2887`) says the criterion is a **compile-time length** — *"which is the
criterion that keeps a bytes-typed FIELD (`req.data : bytes [..N]`, a runtime length) out."* It is
the gate for both byte-addressed primitives: `byte_at` (`src/verifier.rs:3319`) and `length`
(`:3197`). So `byte_at(req.data, 0)` in a `raw_tcp` handler is a **verify error** —
`expression has type 'bytes' but context expects 'text'` (the message at `src/verifier.rs:3055`).
`substring` is not in the gate at all: it checks its operand against `Type::Text` unconditionally,
so there is no bytes slice in the language.

**What a `raw_tcp` handler can do with its input today: pass it through, or place it whole inside a
bytes `concat` alongside literals / `le32` / `le64`. It cannot read a byte, measure a length, or
slice.** Everything revision 1 said about handler-managed framing, its ping/pong example, and its
negative control (c) falls with this. It is the reason slice 0 exists.

### W2 — bytes values are streamed, never materialised.

`emit_streaming_bytes_body` writes to a file descriptor as it walks; there is **no bytes sibling of
`emit_text_produce_ptrlen`** (`src/native.rs:8790`, which is text-only — it type-checks every `Call`
callee against `Type::Text`). So `set <bytes state field> = <bytes rule result | bytes concat>` has
no source pointer to `rep movsb` from. The only `(ptr, len)` bytes sources that exist are the read
buffer, another state field's buffer, and a `b"…"` literal.

A record rule like `Digest` is **32 Number fields**, not bytes; packing one into `bytes [..32]`
needs a byte-width encoder that does not exist (`le32`/`le64` write 4 and 8 bytes, and the fields
are one byte each).

**The one place this is NOT a blocker is the response**, and that is worth stating because it looks
like one: `raw_tcp` has no `Content-Length` for the compiler to compute, so the response never needs
materialising — streaming it straight to the client fd is the *natural* writer, and
`emit_write_bytes_literal` is already fd-parameterised. Threading an `fd` parameter through
`emit_streaming_bytes_body` is mechanical. So W2 bites on a bytes **`set`**, not on a bytes
**response**.

### W3 — the `raw_tcp` emitter has no rbp frame.

`emit_raw_tcp_echo_bytes` does `sub rsp, 16` for the sockaddr_in, then `sub rsp, imm32` for the read
buffer (`:20277-20281`), and addresses the buffer as `rsp`. Measured: the whole function
(`20217-20345`) contains **zero** occurrences of `rbp` or the `0x55` push-rbp opcode. The HTTP
emitter's prologue is `push rbp ; mov rbp, rsp ; sub rsp, imm32` at `src/native.rs:21262-21266`.

Every handler-side emitter assumes the HTTP discipline: `emit_eval_expr` resolves fields through an
`offsets: HashMap<&str, i32>` of **rbp**-relative slots, BoundText lookups are `(ptr_slot, len_slot)`
rbp offsets, `emit_text_produce_ptrlen` leaves values in registers whose backing buffers live in an
rbp frame, and the state copy reads `[rbp + buf_off]`.

**So revision 1's "replace the identity write and keep everything else" is impossible.** The new
emitter adopts the HTTP frame discipline wholesale and reuses only the control-flow edges of §2.1.
§4.1 and §5.3 redraw the layout on that basis.

**`client_fd` — resolved explicitly, and it is `[rbp-48]`.** The raw_tcp skeleton keeps it in `r13`;
the HTTP emitter keeps it in a slot (`mov [rbp-48], rax`, `src/native.rs:21458`, read back at
`:21598` and `:22036`). The new emitter takes **the slot**, for a reason that is not symmetry:
handler-reachable code makes syscalls (a state copy's `rep movsb` does not, but a streamed response
write does, and slice 0's `length` scan does not while a future `read()` in a handler would), and
the register audit below shows `r13` is not free once real handler code runs.

**Register audit across the handler-reachable helpers — done, not asserted.** Linux syscalls clobber
`rax`, `rcx`, `r11` by ABI. `CLAUDE.md`'s register table assigns, in *rule* binaries: `r12` argc,
`r13` argv base, `r14` record index, `r15` reaction fd / collection inner counter, `r10` concat
buffer base, `rbx` concat write pointer, `r9` saved pre-allocation rsp, `r11` arena base. In the
HTTP service emitter `r12` is the server fd and `r15` is the resource/outbound fd
(`emit_resource_read_sequence`, `emit_connection_fetch_sequence`). **`r13` and `r14` are
handler-reachable scratch** — `emit_concat_to_buffer` and the collection emitters use them — so
parking `client_fd` there would be live across exactly the code slice 0 is about to admit. The slot
costs 4 bytes per read (`mov rdi, [rbp-48]`) and removes the question. Take the slot.

### W4 — corollary: revision 1's §2.1 "the frame register conventions are already in place" is false

They are in place *for an emitter that computes nothing*. Once the handler computes, `r13` and `r14`
are contested and `rsp`-relative addressing does not survive a `sub rsp` for a concat buffer. This
corollary is stated separately because it is the sentence that made revision 1's slice look small.

---

## 4. Slice 0 — `rawtcp-inspect-0`: a `raw_tcp` handler can inspect its input

**Independently shippable. No loop, no state, no concurrency change. One read, one write, exactly as
today.** What changes is that the handler may compute.

### 4.1 Scope

1. **The `raw_tcp` service emitter gains the HTTP frame discipline.** A new
   `emit_raw_tcp_dynamic_bytes`, structured like `emit_http10_dynamic_bytes` minus the HTTP parse
   and serialize: `push rbp ; mov rbp, rsp ; sub rsp, frame_size`; socket / `setsockopt` / bind /
   listen; `accept_top`; `mov [rbp-48], rax`; `read(client_fd, rbp+buf_off, max_request)`;
   `test rax,rax ; jle close`; store the read count; handler; response write; `close`;
   `lea rsp, [rbp - frame_size] ; jmp accept_top`. The identity-shaped service (no computation in
   the handler) keeps `emit_raw_tcp_echo_bytes` **untouched**, so `examples/raw_tcp_echo.verbose`
   stays byte-identical at 358 B (§4.6-4).

2. **`req.<field>` registers as a BoundText `(ptr, len)` pair under a COMPOSITE key.** The pointer is
   `lea rax, [rbp + buf_off]`; the length is the `read` return value, stored to a slot. Key:
   `__req_<field>`.

   **The composite key is mandatory here for a sharper reason than in HTTP.** `req.body` collides
   with a state field named `body` because `body` is a fixed built-in name — the author has to
   choose the collision. In `raw_tcp` the input field name is **author-chosen**:
   `check_raw_tcp_binding` (`src/verifier.rs:2193`) requires exactly one bytes field and says nothing
   about its name. So an author who names the input field `seq` and a state field `seq` — a natural
   thing to do — gets, under bare-name keying, `req.seq` and `state.seq` resolving to the same slots:
   a plausible value, rc 0, no diagnostic. This is text-state's NC-3 (`docs/text-state-fields-design.md`
   §6.6) with the collision moved from "one reserved name" to "any name at all".

3. **The `byte_at` / `length` operand gate admits a `bytes` FIELD.** `check_byte_addressable_operand`
   (`src/verifier.rs:2872`) grows a third admitted shape: a `Field` whose resolved type is
   `Type::Bytes` **and which is registered as a BoundText pair** — i.e. its length is a runtime slot,
   not a compile-time constant.

   **The gate's stated criterion ("a compile-time length") is not weakened; it is met differently,
   and the difference is a fact rather than an assertion.** For `b"…"` the length is the literal's;
   for `random(k)` it is the declared `bytes: N` plus the `getrandom(2)` contract. For `req.<field>`
   it is **the `read` syscall's return value**, which the emitter stores and every reader loads —
   the same shape as `read(<resource>)`'s `len_slot`, which `emit_length` (`src/native.rs:17447`)
   already consumes without a compile-time constant. So the honest restatement of the criterion is
   **"a length the emitter knows, either as a constant or in a slot it owns"** — and `req.<field>`
   qualifies where a general bytes expression (a bytes `concat`, streamed with no sizing pass — W2)
   does not. The gate must therefore be widened to *BoundText-registered bytes*, never to
   `Type::Bytes` at large; widening it to the type would admit a streamed concat, whose length is
   nowhere.

   `byte_at`'s bounds check is unchanged and already fail-closed: `index >= length` → `sys_exit(1)`,
   negative index caught by the same unsigned compare. Against a runtime length that check is doing
   real work for the first time.

4. **The handler may be a real rule.** The identity gate (`src/native.rs:20508-20524`) and the
   let-bindings gate (`:20500`) are lifted for the dynamic path. Number/text `let`s work by the
   existing `let_rhs_is_text` classification (`src/native.rs:21175-21191`). The response field is a
   bytes expression written by `emit_streaming_bytes_body` **with an `fd` parameter threaded
   through** (§3 W2) so it streams to the client fd instead of fd 1.

5. **`state:` is NOT lifted for raw_tcp** (`src/verifier.rs:1288-1293` stands), **`concurrency:
   forked` is NOT lifted** (`:1276-1281` stands), and there is no loop. All three are slice 1.

6. Native only (§2.6).

### 4.2 What slice 0 deliberately does NOT include: a bytes `substring`

The brief asked for a decision and a reason. **Deferred to slice `rawtcp-inspect-0b`**, and the line
is drawn at what the value's *type* is:

- `byte_at` and `length` produce **Numbers**. Every sink in the language already accepts a Number.
  Admitting them costs two operand-gate arms and no new value shape.
- A bytes `substring` produces a **bytes VALUE**, whose only sinks are the response field and a bytes
  `concat`. The bytes concat is **streamed with no sizing pass** (W2), so a slice arm there is an
  emitter question about the streaming ABI, not an operand-gate question. It is the same seam
  `entropy-2` is already parked behind (`docs/randomness-effect-design.md` §7: *"it touches the
  streaming ABI, whose interaction with a per-record buffer is unanalysed"*).

Mechanically the slice itself is cheap — `emit_substring_bounds_and_slice` (`src/native.rs:8707`) already yields
`(rax = slice_ptr, rdx = slice_len)` into the same buffer, and `infer_expr_type` already has the
precedent for a type-directed result (`Expr::Concat` answers Bytes if any argument is bytes,
`src/verifier.rs:3926-3928`). It is deferred for scope, not difficulty.

**The honest consequence, stated so nobody discovers it at implementation time: after slice 0 a
handler can DECIDE on its input — route on a tag byte, gate on a length, validate a magic number —
but it cannot RESHAPE it.** The response is built from literals, `le32`/`le64` of computed Numbers,
and the whole input chunk. That is enough for slice 0's example and slice 1's, and it is not enough
for an echo-the-payload RPC.

### 4.3 Worked example — `examples/tag_probe.verbose` (one read, one write, no loop)

A `raw_tcp` service that answers by its input's first byte and its length. Every construct is
slice-0 legal: `byte_at` and `length` are Numbers, `le32` turns a Number into bytes, and the bytes
`concat`'s arguments are all bytes.

```
concept Frame
  fields:
    data : bytes [..256]

rule probe
  input:  req  : Frame
  output: resp : Frame
  logic:
    resp = Frame {
      data: if byte_at(req.data, 0) == 1
              then concat(b"\x01", le32(length(req.data)))
              else concat(b"\xff", le32(0))
    }
  proofs:
    purity:      reads : [req.data] ; calls : []
    termination: bound : 9

service prober
  listen:  protocol: raw_tcp ; port: <ephemeral> ; max_request: 256
  handler: probe
```

Three inputs, three answers, and the fourth row is the bounds check:

| input | response | proves |
|---|---|---|
| `01 41 42` | `01 03000000` | `byte_at` reads the field; `length` is the runtime `read` count, not `max_request` |
| `02 41` | `ff 00000000` | the false arm; the two arms are distinguishable |
| `01` (1 byte) | `01 01000000` | length is the *actual* count |
| `` (0 bytes) | — | a zero-byte read is EOF (`jle close_client`), so `byte_at(req.data, 0)` is never reached with an empty buffer |

The last row is worth its line: it is why `byte_at`'s fail-closed bound is not the only thing
standing between an empty read and an out-of-range index.

### 4.4 Refusals, each naming the offender and the lifting slice

| # | Shape | Breadcrumb |
|---|---|---|
| 1 | `substring(req.data, a, b)` — a bytes operand | `substring: 'req.data' is bytes; a bytes slice produces a bytes value whose only sinks are the response field and a bytes concat, and the bytes concat is streamed with no sizing pass. Slice rawtcp-inspect-0b.` |
| 2 | `starts_with` / `ends_with` / `contains` / `json_escape` / `parse_int` / text `==` on a bytes field | `<prim>: 'req.data' is bytes; this primitive checks its operand against text and the bytes/text isolation is deliberate. Convert explicitly with byte_at, or wait for slice rawtcp-inspect-0b.` |
| 3 | `byte_at` / `length` on a bytes expression that is not BoundText-registered (a bytes `concat`, `le32(...)`) | `byte_at: operand has no length the emitter can load — a bytes concat is streamed with no sizing pass (native.rs:7627). Admitted bytes operands: a b"..." literal, random(<name>), and a raw_tcp input field.` |
| 4 | a bytes-typed `let` in a handler | `handler '<h>': let bindings are number- or text-typed (let_rhs_is_text, native.rs:21175); a bytes let needs a materialised bytes value, which does not exist (no bytes sibling of emit_text_produce_ptrlen). Slice rawtcp-inspect-0b.` |
| 5 | a record-valued `let` in a handler | see §7 item 6 — a **pre-existing** misleading breadcrumb, fixed here rather than inherited. |
| 6 | `random(k)` in a `raw_tcp` handler | unchanged — `src/native.rs:20481`, naming `entropy-2`. |
| 7 | `state:` / `after:` / `concurrency: forked` on `raw_tcp` | unchanged — `src/verifier.rs:1288`, `:1320`, `:1276`. Slice `multistep-1`. |

### 4.5 A pre-existing misleading breadcrumb, fixed in its own PR (not in either slice)

Measured on `main`: a record-valued `let` in an HTTP handler — `let p = swap2(req)` where `swap2`
returns a record — **verifies clean** and then dies natively with
`unknown rule 'swap2' for native inlining` (`src/native.rs:16119`). The rule is perfectly known; what
is missing is a record arm in the handler-let path, which classifies every let as text-or-number
(`let_rhs_is_text`, `src/native.rs:21175-21191`) and sends a non-text let to `emit_eval_expr`, whose
`Call` arm tries to *inline*. The message sends a reader looking for a parse or scope problem.

It is not caused by either slice and it is not fixed by either — but slice 0 is the first slice whose
handlers can call rules at all, so it would inherit the message. It is being replaced **separately,
as a diagnostic-only PR with zero emitted bytes**, at the handler-let site, by a breadcrumb that names
the service, the handler, the let, the callee, the fact that the callee returns a record, and the
lifting slice `agg-svc-1` (a record-valued `let` inside a service handler is the aggregate-composition
arc — agg-2c shipped the rule-binary half — not a connection feature). Slice 0 inherits the truthful
message and adds nothing to it.

### 4.6 Acceptance tests

Alongside `text_state_drive` (`src/native.rs:41655`), whose ephemeral-port / spawn / poll-for-bind /
one-TCP-conversation shape is the template.

1. `rawtcp_inspect_byte_at_and_length` — the §4.3 table, all four rows, over real TCP.
2. `rawtcp_inspect_length_is_the_read_count_not_max_request` — rows 1 and 3 alone, asserted as
   distinct answers. (Split out because a build that returned `max_request` passes row 1 by accident
   only if `max_request` is 3.)
3. `rawtcp_byte_at_out_of_range_is_fail_closed` — a handler indexing past the read count exits 1 with
   no response; the connection closes without bytes.
4. `rawtcp_identity_service_is_byte_identical` — `examples/raw_tcp_echo.verbose` still compiles to
   exactly 358 B through the unchanged `emit_raw_tcp_echo_bytes`.
5. Verifier units for refusals 1–4 and 7 of §4.4, **each with a minimally corrected twin that must
   still verify**, so the refusal is attributable rather than "this compiler refuses everything".
6. `rawtcp_input_field_composite_key` — a service whose input field and a *handler let* share a name;
   both must resolve to their own slots. (State fields are slice 1, so the collision is exercised
   against the let namespace here and against state in slice 1.)

### 4.7 Negative controls

| # | Break | Fixture | Must | Why not the obvious fixture |
|---|---|---|---|---|
| **NC-0a** | Store `max_request` in the length slot instead of the `read` return | §4.3, rows 1 **and** 3 | FAIL | Row 1 alone passes if the payload happens to be `max_request` bytes; a *short* read is the discriminator, so the control needs two payload lengths. |
| **NC-0b** | Register `req.<field>` under the **bare** field name | the §4.6-6 fixture, where a handler let shares the input field's name | FAIL | The §4.3 fixture passes bare-name registration — nothing collides with `data`. The control is only discriminating on a colliding name, and the non-colliding fixture must **still pass in the same run**, or the control proves nothing about keying (text-state NC-3's exact shape). |
| **NC-0c** | Delete `byte_at`'s `jae` bounds branch | §4.6-3's out-of-range fixture | FAIL — reads past the buffer instead of exiting 1 | An in-range fixture passes the broken build, and every §4.3 row is in range. |
| **NC-0d** | Emit the response through `emit_streaming_bytes_body` with fd hardcoded to `1` | any §4.3 row | FAIL — the client receives nothing; the bytes land on the server's stdout | This is the one-line mistake the fd threading exists to prevent, and a test that only asserts the server does not crash passes it. |

---

## 5. Slice 1 — `multistep-1`: the step loop and per-connection state

Built on slice 0. **Forked-only.**

### 5.1 Scope

1. A `raw_tcp` service may declare **`max_steps : N`** (`1 ≤ N ≤ 65535`) and **`read_timeout : S`**
   (seconds, `1 ≤ S ≤ 3600`). Declaring either makes the service multi-step; both are then
   **mandatory** (§5.5). Absent both, the service is slice 0's one-shot shape (or, with an identity
   handler, the untouched 358-byte echo).
2. A multi-step service **must** declare `concurrency: forked` (§5.4). `src/verifier.rs:1276-1281`
   is re-scoped from "forked is http_1_0 only" to "forked is http_1_0, or raw_tcp with a step loop".
3. **`state:` and `after:` are lifted for a multi-step `raw_tcp` service** —
   `src/verifier.rs:1288-1293` re-scoped — and are **reused, not renamed** (§6.2). `state:` fields
   stay `number | text [..N]`; **`bytes [..N]` state is deferred** (§6.4).
4. PR #194's refusal (`src/verifier.rs:1320`) is **re-keyed**, not reused: refuse `after_sets` +
   `Forked` **when the service has no step loop**. §6.1.
5. Framing is **one frame per read**. A frame spanning two reads is out of scope and is a runnable
   negative control (§5.9, NC-1f).
6. Native only.

### 5.2 The shape

```
  push rbp ; mov rbp, rsp ; sub rsp, frame_size        (slice 0's prologue)
  ── STATE INIT ──                                     (shipped: native.rs:21268, once, in the parent)
  socket / setsockopt(SO_REUSEADDR) / bind / listen
  rt_sigaction(SIGCHLD, SIG_IGN)                       (shipped: native.rs:21358, forked only)
accept_top:
  accept → mov [rbp-48], rax
  fork()                                               (shipped dispatch: native.rs:21461)
    parent: close([rbp-48]) ; jmp accept_top
    child:  fall through — inherits the initialised state slots by COW
  setsockopt(client_fd, SOL_SOCKET, SO_RCVTIMEO, &tv, 16)      (NEW, §5.4)
  mov qword [rbp + step_slot], 0
step_top:                                              (NEW — the per-connection step loop)
  read(client_fd, rbp+buf_off, max_request) → rax
  test rax, rax ; jle close_client                     (EOF, error, AND -EAGAIN from the timeout)
  mov [rbp + reqlen_slot], rax
  inc qword [rbp + step_slot]
  cmp qword [rbp + step_slot], max_steps ; ja close_client
  ── HANDLER ──          (req.<f> = (buf, reqlen); state.* readable)
  ── RESPONSE WRITE ──   (emit_streaming_bytes_body → client_fd)
  ── AFTER BLOCK ──      (shipped: native.rs:21929 — the innermost-loop mutation, §6.1)
  ── STEP TAIL ──        lea rsp, [rbp - frame_size] ; jmp step_top
close_client:
  close([rbp-48])
  sys_exit(0)                                          (child exits, as the HTTP forked tail does)
```

### 5.3 Frame layout — and why there is only one reset

Revision 1 posited two reset points and placed a new "session block" between them. There is **one**,
and the existing layout already sits on the correct side of it.

The step tail's `lea rsp, [rbp - frame_size]` restores `rsp` to the post-prologue invariant. Every
state slot is **rbp**-relative and inside `frame_size`, so `lea rsp` moves the stack pointer *below*
them and never writes them — which is exactly the argument `src/native.rs:21269-21272` already makes
for surviving the *accept* tail, and it makes no reference to which loop it is. The step tail
therefore preserves state for free, and the accept tail is gone from the child's path entirely
(the child `sys_exit(0)`s).

```
  [rbp-8 .. rbp-48]   fixed handler block (client_fd at -48)
  reqlen_slot         ]  the read count for this step
  step_slot           ]  the step counter
  handler let slots   ]  all above the step tail's lea rsp — persist across steps
  ┌─── STATE BLOCK ───┐     number: 8 B · text: 16 B (ptr,len) + N↑8 buffer
  │  descending cursor over service.state_fields, a Vec (native.rs:21220)      │
  └───────────────────┘
  resource / connection / entropy block   (unchanged relative order)
  ── everything below is re-derivable each step ──
  read buffer (max_request)
  handler transients (concat buffers, allocated by sub rsp, freed by the step tail's lea rsp)
```

**Per-connection lifetime is the fork, and it needs no code.** `STATE INIT` runs once in the parent
before `listen`; every child gets a copy-on-write copy of the initialised slots; the child mutates
its own pages and exits. Two concurrent connections are two children in two address spaces —
`docs/effect-model.md`'s refusal of shared memory is what makes cross-connection leakage impossible
by construction, which is also why negative control (b) had to be rewritten (§5.9).

### 5.4 The DoS story, told honestly

Revision 1 justified forked-only with *"a single client that opens a connection and stops sending
would block the entire sequential server forever: a one-packet denial-of-service."* Two corrections,
both measurable from the shipped emitter.

**(a) The idle-client exposure is INHERITED, not introduced.** A forked HTTP/1.0 child already blocks
on its one `read` (`src/native.rs:21596`) with no timeout, for as long as the client cares to hold
the connection. Slice 1 does not create that exposure; it extends its duration from one exchange to
one conversation.

**(b) The zero-packet DoS already exists against every SEQUENTIAL service today.** Connect to a
sequential `http_1_0` service, send nothing: the single process blocks in `read` and serves nobody.
Same for the sequential `raw_tcp` echo. So "a one-packet DoS" is the status quo, and it cannot be
the argument for anything slice 1 does.

**What the step loop genuinely changes is the WELL-BEHAVED SLOW client.** Under one-shot, a
sequential server is monopolised for the duration of one exchange — bounded by how fast the client
sends one request. Under a step loop it is monopolised for the duration of a whole *conversation*,
which the client chooses. That is a qualitative change in how long a legitimate, non-malicious peer
can hold the only server, and it is a sufficient reason for **forked-only**. Sequential multi-step
needs `poll`/`epoll` multiplexing, which is a large arc and cuts against the OS-as-supervisor
memory.

**`max_steps` bounds WORK, not TIME — so it is not a resource bound on its own, and shipping it
alone would be false explicitation.** There is no socket read timeout anywhere in the emitter. The
emitter contains exactly five `setsockopt` emissions — `src/native.rs:19283`, `:20237`, `:21335`,
`:22771`, `:22914` — and **every one is SO_REUSEADDR**; `grep -rn 'RCVTIMEO|SNDTIMEO|timeval' src/`
returns nothing at all. So slice 1 adds one:

```
setsockopt(client_fd, SOL_SOCKET=1, SO_RCVTIMEO=20, &timeval{S,0}, 16)
```

after `accept` (in the child, so the listening socket is untouched). It is ~40 bytes plus a 16-byte
`timeval` on the stack, and it needs **no new branch**: a timed-out `read` returns `-EAGAIN`, which
the existing `test rax,rax ; jle close_client` already treats as close. `read_timeout : S` is
mandatory on a multi-step service, so the declared per-connection ceiling is
`max_steps × read_timeout` seconds and both factors are in the source.

**What that does NOT close, named rather than glossed:**

- **`SO_RCVTIMEO` covers `read`, not `write`.** A client that never reads can stall the child in the
  response `write` on a full socket buffer, unbounded. `SO_SNDTIMEO` is the symmetric fix and is
  *not* in slice 1 — because the streamed response (§3 W2) is many small writes, and "what does a
  partial write mean when the bytes are already gone" is a real semantic question, not a knob.
  Stated as a residual, with `rawtcp-inspect-0b` / a write-policy slice as the owner.
- **Total exposure is `max_steps × read_timeout` per connection × how many children the OS lets you
  fork.** The last factor is `ulimit`/cgroup, i.e. the operator's, which is consistent with
  OS-as-supervisor and is not a language claim.
- So the claim slice 1 may make is: **"a multi-step connection's work and its wall-clock lifetime are
  both bounded by declarations in the source, and both bounds are visible with `grep`."** The claim
  it may **not** make is "nothing unbounded compiles" — that is false on the write axis.

### 5.5 Refusals, each naming the offender and the lifting slice

| # | Shape | Breadcrumb |
|---|---|---|
| **1** | `state:` or `after:` on a `raw_tcp` service with **no `max_steps`** | `service 'S': a raw_tcp service declaring state must also declare 'max_steps' (and 'concurrency: forked'). Without a step loop the one-shot emitter would compile this to the identity echo and DROP the state declaration silently.` |
| **2** | `max_steps` without `read_timeout`, or the reverse | `service 'S': a multi-step service must declare both 'max_steps' (a work bound) and 'read_timeout' (a time bound). max_steps alone bounds how many frames a client may send, not how long it may hold a child.` |
| **3** | multi-step with `concurrency: sequential` (or default) | `service 'S': a multi-step raw_tcp service must declare 'concurrency: forked'. A sequential server holding one connection open across a whole conversation is monopolised for as long as the peer chooses; forked isolates each conversation to one child.` |
| **4** | `max_steps` on an `http_1_0` service | `service 'S': max_steps applies to raw_tcp multi-step services; http_1_0 is one request per connection. HTTP/1.1 keep-alive is a later slice.` |
| **5** | `after: set f = concat(state.f, …)` (append) | `after: set 'f': append-accumulation into a state buffer has no compile-time worst case; it needs a declared overflow policy — slice text-state-2.` (inherited verbatim from `docs/text-state-fields-design.md` §6.2 #6) |
| **6** | an `after:` source with no compile-time bound | inherited from `text_source_worst_case` (`src/verifier.rs:1666`); the message gains `req.<field>` to its accepted-source list. |
| **7** | a `bytes`-typed state field | `state field 'f': bytes state is deferred. It needs (a) a bytes source that can be COPIED — every bytes value today is streamed, not materialised (native.rs:7627) — and (b) the state read path threaded into infer_expr_type, without which concat("k=", state.f) over a bytes field passes the verifier and is emitted as text. Slice multistep-2, gated on slice state-read-typing.` (§6.4) |
| **8** | `state.<f>` in `starts_with` / `substring` / `json_escape` / `parse_int` / text `==` | inherited unchanged — `docs/text-state-fields-design.md` §6.2 #8/#9. Worth restating: `emit_starts_with_load_text` (`src/native.rs:17852-18118`) has **no** `state` arm (measured: zero `"state"` occurrences in that range), while `emit_length` does (`:17480`). So `length(state.f)` works and `byte_at(state.f, i)` does not, because `byte_at` loads through the former. |
| **9** | a declared framing block | `service 'S': declared framing is slice multistep-3; slice 1 is one frame per read.` |
| **10** | `random(k)` in a multi-step handler | unchanged — `src/native.rs:20481`, naming `entropy-2`. And see §7 item 4: even once `entropy-2` lands, the **sampling unit** for a step loop is undesigned. |

Refusal **1** is the one that matters most, and it is not hypothetical. `compile_service` calls
`emit_raw_tcp_echo_bytes(service.port, service.max_request)` at `src/native.rs:20526` — it is handed
**two scalars** and cannot see a `state:` block at all. This is precisely the hazard text-state
already hit on the HTTP side, where an `after:` block now forces the dynamic path because *"the
constant fast path emits no state init and no after block, so the mutation would be SILENTLY
DROPPED"* (`src/native.rs:20428-20438`). Same defect, same fix, one protocol over.

### 5.6 Worked example — `examples/step_counter.verbose`

Deliberately **not** TLS, and deliberately within slice 0's inspect-but-don't-reshape limit (§4.2):
a per-connection step counter, echoed back with the length of each frame. Every construct is
shipped-or-slice-0: `state.seq` is a **Number** field (so no bytes state, no §6.4 prerequisite),
`length(req.data)` is slice 0, and both `le32` arguments make the bytes `concat` all-bytes.

```
concept Frame
  fields:
    data : bytes [..256]

rule step
  input:  req  : Frame
  output: resp : Frame
  logic:
    resp = Frame { data: concat(le32(state.seq), le32(length(req.data))) }
  proofs:
    purity:      reads : [req.data, state.seq] ; calls : []
    termination: bound : 6

service counter
  listen:  protocol: raw_tcp ; port: <ephemeral> ; max_request: 256
  concurrency:  forked
  max_steps:    100
  read_timeout: 5
  handler: step
  state:
    seq : number = 0
  after:
    set seq = state.seq + 1
```

One connection, three frames of **different lengths** (`AA` / `BBBB` / `C`):

| step | payload sent | response (LE u32 pair) | proves |
|---|---|---|---|
| 1 | 2 bytes | `0, 2` | init reached the slot; the mutation is not visible to its own step |
| 2 | 4 bytes | `1, 4` | the value survived the step tail's `lea rsp` — a live cell across steps |
| 3 | 1 byte | `2, 1` | still live; and the second field tracks the payload, not a constant |

The differing payload lengths are not decoration — they are what makes negative control (a)
non-vacuous (§5.9).

### 5.7 Semantics

- **One step = one handler invocation** over one `read` chunk.
- **`after:` runs at the bottom of the innermost loop** — that is the one rule (§6.1), and here the
  innermost loop is the step loop. So a value `after:` sets is visible from the **next step** onward,
  never to the step that set it. Same reproducibility argument text-state makes
  (`docs/text-state-fields-design.md` §5.3): a step's response is a function of
  `(req.<field>, state-as-of-step-entry)` plus declared effects. "Echo what I just stored" is a
  handler `let`, not a state field.
- **The `after:` block still runs before the step tail's `lea rsp`.** This is the invariant
  `src/native.rs:22044-22050` records in writing, and it is what makes the copy expressible at all:
  every transient a `set` may copy FROM — a handler concat buffer, the read buffer itself — is still
  live at the copy point. Moving the free earlier, or the after block later, breaks it *silently*
  (the copy reads freed stack). §5.9 NC-1e is the runnable form of that dependency.
- **Per-connection isolation is the fork** (§5.3).
- **Fail-closed is per CONNECTION.** A bad frame aborts the **child** via the shared `sys_exit(1)`
  tail; the parent keeps accepting. That is the correct unit, and it is
  `docs/text-state-fields-design.md` §3.3's argument applied to a conversation.

### 5.8 Acceptance tests

The `text_state_drive` template (`src/native.rs:41655`) with one change: the driver holds **one
connection open** and sends several frames on it.

1. `multistep_state_persists_across_steps` — §5.6, all three rows, one connection.
2. `multistep_state_resets_per_connection` — connection A: two frames (`0`, `1`); connection B: one
   frame, must answer `0`. (Under forked this is guaranteed by COW rather than by new code — see
   NC-1b for what actually tests it.)
3. `multistep_max_steps_caps_the_conversation` — send `max_steps + 1` frames; the connection closes
   after exactly `max_steps` responses.
4. `multistep_read_timeout_closes_a_silent_client` — connect, send one frame, then stall past
   `read_timeout`; the child closes and exits without a further response, and the parent still
   answers a fresh connection immediately afterwards.
5. `multistep_sequential_refused` / `multistep_max_steps_without_timeout_refused` /
   `multistep_state_without_max_steps_refused` — refusals 1, 2, 3, each with a corrected twin that
   must compile and run.
6. `multistep_one_shot_rawtcp_is_byte_identical` — `examples/raw_tcp_echo.verbose` still 358 B; a
   slice-0 dynamic service with no `max_steps` still compiles through the slice-0 path.
7. `multistep_input_field_and_state_field_share_a_name` — the composite-key control, now against the
   *state* namespace (§4.1-2's author-chosen-name hazard).

### 5.9 Negative controls — rewritten, because two of revision 1's three were unsound

| # | Break | Fixture | Must | Why this fixture |
|---|---|---|---|---|
| **NC-1a** | Replace the `after:` `rep movsb` with an alias (store the source pointer) | a **text** state field sourced from a handler concat, driven ≥ 2 steps **with different payloads**, and the handler must **echo the state buffer back** | FAIL — step 2 returns step 2's own value | Revision 1's version was unsound twice. A *literal*-init or Number-only fixture passes an aliased build (a literal lives in `.text`; a Number is copied by value), **and** a fixture that never reads the buffer back cannot observe the aliasing at all. Both conditions are required, and §5.6's Number-only example satisfies neither — so this control needs its own fixture. |
| **NC-1b** | Move `STATE INIT` **inside** the step loop (or delete it) | §5.6, one connection, ≥ 2 steps | FAIL — row 2 returns `0` instead of `1` (init) or garbage (deleted) | Revision 1's control here was *"register state in the parent's frame instead of per-child"*, driven by two interleaved connections — and that is **vacuous under forked**: two children are two address spaces, so cross-connection sharing is impossible by construction and the break cannot be expressed. The thing that CAN break is the *single-connection* one: where init runs relative to the loop. |
| **NC-1c** | Delete the `cmp step_slot, max_steps ; ja` | §5.8-3's fixture | FAIL — the server answers `max_steps + 1` frames | A conversation shorter than `max_steps` passes the broken build, and §5.6's three-frame driver is one. |
| **NC-1d** | Delete the `SO_RCVTIMEO` setsockopt | §5.8-4's stalling fixture, **paired in the same run with §5.6's well-behaved fixture** | FAIL — the stalling test hangs to its harness timeout | A well-behaved client never times out. The pairing is what distinguishes "the timeout works" from "the server is broken and closes everything": both assertions must hold in one run. |
| **NC-1e** | Move the `after:` block **below** the step tail's `lea rsp` | §5.6, or NC-1a's text fixture | FAIL — the copy reads freed stack | text-state's NC-5, re-aimed at the step tail. The ordering invariant at `src/native.rs:22044-22050` is a paragraph until this control makes it mechanical. |
| **NC-1f** | Feed a frame whose length prefix exceeds what one `read` returned (header in one `write`, payload in a second after a delay) | a length-prefixed fixture, **paired in the same run with a single-segment twin that must SUCCEED** | the child fails closed (rc 1, no partial response) **or** the compiler refuses the reassembly shape naming `text-state-2` + `multistep-3` | Revision 1's version had two defects. It needs slice 0 to express the length read at all (W1), and its "OR the compiler refused" disjunct **passes on a compiler that refuses everything** — hence the mandatory positive twin. This is §6.4's deferral made runnable. |

### 5.10 Byte-identity — how it is proved

The house method (`CLAUDE.md` corpus sweeps): a compiler from `c5e46b0` and one from the branch,
all `examples/*.verbose` × every rule / service / reaction, compared by size + sha256, with the
**baseline-vs-baseline control run FIRST and required empty**.

Expected: only the new examples' rows change. `raw_tcp_echo.verbose` stays 358 B through the
untouched `emit_raw_tcp_echo_bytes`; `counter_service` (975 B), `last_path_service` (1027 B),
`nonce_service` (3659 B) and every other HTTP service are untouched because slice 0 adds a *new*
emitter and slice 1's re-keying of `src/verifier.rs:1276`/`:1288`/`:1320` only *widens* what is
accepted. The one structural risk to watch is §4.5's breadcrumb change, which must be a message
change only.

gen0: expected to refuse both new examples at every index (no `max_steps` / `read_timeout` keywords,
no bytes-field `byte_at`) — the safe direction, and a new gaps-table row rather than a moved figure.

---

## 6. The decisions the review sent back

### 6.1 `state:` / `after:` are REUSED. There is no `session:` / `next:`.

Revision 1 rejected reuse by counting three lifetimes for one spelling. **Two of the three are
REFUSED shapes**, so they are not lifetimes any program can have:

| shape | lifetime revision 1 claimed | status at `c5e46b0` |
|---|---|---|
| sequential http_1_0, no loop | per-process | **accepted** |
| forked http_1_0, `after:` | "unobservable" | **refused** — `src/verifier.rs:1320` |
| forked + a step loop | per-connection | **refused** — the step loop does not exist |

Among **accepted** programs, `state:` has exactly **one** lifetime. Slice 1 takes it from one to two,
not one to three, and the second is reached only through two declarations the author had to write
(`concurrency: forked` and `max_steps`).

**The single rule that replaces the counting: `after:` runs at the bottom of the innermost loop the
service declares, and a `state:` field lives for the frame that encloses that loop.** One sentence,
mechanically checkable, and it is what the emitter already does — the HTTP `after:` block sits at
`src/native.rs:21929`, immediately above the iteration tail, which *is* the bottom of the only loop
that service has.

**A shipped precedent in this codebase argues the same way.** `read(<resource>)` means "once per rule
invocation" in a rule binary (slice 9.1), "once per accept" in a service (slice 9.2), and "once at
startup" with `cache: true` (slice 9.4) — one spelling, re-evaluation frequency a function of
context plus one declared knob, and the project shipped that without inventing a second keyword.
**The disanalogy, stated so the precedent is not oversold:** a `read` result is not *carried* across
invocations, so for `read` "lifetime" only means freshness, whereas a state field's whole point is
that a value survives. The precedent shows context-determined scoping is house style; it does not by
itself settle a persistence question.

**PR #194 is re-keyed, not reused.** Its refusal exists because *without a loop* a forked child's
mutation is observed by nobody — the comment at `src/verifier.rs:1294-1319` says exactly that, twice,
and cites the measurement (`count:0` to every request). With a step loop the mutation **is**
observed, by the next step of the same connection, in the same child. So the condition becomes
`after_sets && Forked && no step loop`, and the refusal message keeps its measurement and gains one
clause. It is still keyed on `after_sets` and never on `state_fields`, for #194's own reason: a
`state:` block with no `after:` is a constant, and a constant reads identically under every mode.

**The cost of the split, measured, which is the other half of the argument.** A `session:` block
would need a twin (or a shared `base → key` helper) at: **seven** `Ident(_) == "state"` base matches
in `src/native.rs` (`:8124`, `:8892`, `:14186`, `:15297`, `:17480`, `:21057`, `:22354`), plus key
construction (`:21248`) and registration (`:21779`); and **five** sites in `src/verifier.rs`
(`:1479` the synthetic concept, `:1619` and `:4848` the `reads:` path check, `:1711` and `:1824` the
overflow prover's base resolution). Plus a parser block (`src/parser.rs:1955` / `:2049` are the
templates), an AST pair, and a second copy of `text_source_worst_case`'s base arm. That is a lot of
surface whose only payload is a lifetime distinction no accepted program can currently observe.

**The honest steelman, and the exact condition that flips this decision.** Sequential HTTP/1.1
keep-alive (§1.2) is a program where a per-process lifetime and a per-connection lifetime are **both
observable at once**: one process, many connections, many steps per connection. One spelling cannot
name both. **The day a service can have two enclosing loops, the split becomes mandatory** — and at
that point it is a rename of a known thing with known semantics, not speculative surface. Recorded
here so the decision is revisited by a trigger rather than by taste, and noted as a real cost of
reuse: programs written between now and then would need `state:` → `session:` where they meant
per-connection.

### 6.2 The typing gap is a PREREQUISITE, not a footnote

`state.<f>` is invisible to `infer_expr_type`: its base is neither the input name nor a binding, so
the `Field` arm answers `None`. `CLAUDE.md` records this as a known, **non-disclosing** gap — every
misuse today is refused by the *emitter* (`unknown field 'last'`, `length: argument must be a text
literal…`), so it fails closed with no wrong answer.

**That safety is accidental, and bytes state is what would end it.** `http_text_bindings` is a
`HashMap<&str, (i32, i32)>` — a slot pair, carrying **no type**. So a bytes-typed state field
registered under `__state_<f>` classifies as BoundText exactly like a text one, and
`concat("k=", state.key)` would pass the verifier (no type to check against) and be **emitted as
text** — putting raw key bytes into a text sink. That is precisely the isolation the `bytes`/`text`
separation exists to enforce, and the isolation the randomness effect leans on for its entire
secrecy argument (`docs/randomness-effect-design.md` §5.4: every text sink refuses a bytes value, so
exposing a draw is always an explicit, visible `byte_at`).

So: **slice `state-read-typing` is a prerequisite of bytes state**, and bytes state is therefore not
in slice 1 (refusal #7). The shape is known and half-built: `verify_service` already constructs a
synthetic state concept and registers it in `set_bindings` under `"state"`
(`src/verifier.rs:1479`) so the `after:` RHS can be type-checked. The gap is that this is scoped to
the `after:` block and never reaches the **handler** — `verify_rule` is a different plumbing point,
which `CLAUDE.md` already names as "its own slice". This note does not widen that; it makes the
dependency explicit and orders the two.

Note what this also means for slice 1 as shipped: text and Number state on `raw_tcp` are safe
*because* the emitter refuses what the verifier misses, which is a fail-closed accident, not a
design. It is fine to ship on, and it is not fine to build bytes on.

### 6.3 The TLS sketch, rewritten so every line is annotated

Revision 1's sketch violated four shipped rules: two-argument rule calls (arity is exactly 1,
`check_call_arity`, `src/verifier.rs:4466`), `random()` in a `raw_tcp` handler (refused, `src/native.rs:20481`), `random()`
inside a `set` (refused, `src/verifier.rs:1404`, *"state is not secret"*), and "drawn once per
connection", which is not a sampling unit the language has.

Rather than pretend, here is the same data flow with **every line marked by the slice that would make
it legal**. Lines marked ✅ are legal today or after the slice named in §5; every other line names its
blocker.

```
service tls_endpoint
  listen:  protocol: raw_tcp ; port: 8443 ; max_request: 16384        ✅ shipped
  concurrency:  forked                                                ✅ multistep-1 (§5.5-3)
  max_steps:    8                                                     ✅ multistep-1
  read_timeout: 10                                                    ✅ multistep-1
  handler: tls_step

  entropy server_rand                                                 ⛔ entropy-2 (raw_tcp draw)
    bytes: 32                                                         ⛔ and §7-4: the STEP sampling
                                                                         unit is undesigned

  state:
    phase : number = 0                                                ✅ multistep-1
    s_key : bytes [..16] = b""                                        ⛔ multistep-2, gated on
    s_iv  : bytes [..12] = b""                                           state-read-typing (§6.2)
    transcript : bytes [..8192] = b""                                 ⛔ multistep-2 AND text-state-2
                                                                         (it is an append accumulator)

rule tls_step
  input:  req  : Frame          -- Frame { data : bytes [..16384] }   ✅ shipped
  output: resp : Frame                                                ✅ shipped
  logic:
    resp = Frame { data:
      if state.phase == 0                                             ✅ multistep-1 (Number state)
        then server_hello(HelloIn { chunk: req.data })                ⛔ needs a bytes value to pass
        else finish(FinishIn { chunk: req.data, key: state.s_key })      as a FIELD — and passing a
      }                                                                  whole record as a call arg
                                                                         is itself out of scope
                                                                         (bytes-value-return-design §7)
  proofs:
    purity: reads : [req.data, state.phase, state.s_key] ; calls : [server_hello, finish]

  after:
    set phase = state.phase + 1                                       ✅ multistep-1
    set s_key = derive_key(KeyIn { chunk: req.data })                 ⛔ W2: no bytes value can be
                                                                         COPIED into a buffer
```

Note what the annotation exposes that revision 1's prose hid: **the arity fix alone (one argument,
so a constructed record) drags in "pass a whole record as a call argument", which
`docs/bytes-value-return-design.md` §7 lists as out of scope for the aggregate arc.** The sketch is
not one blocker away from legal; it is six.

### 6.4 Framing stays handler-managed and one-frame-per-read — with a corrected reason

Revision 1's option table is still right in its conclusion and wrong in one premise: it said slice 1
"can express one frame per read" using `byte_at` / `substring`, which W1 says it cannot. After
slice 0 it can express the *length check* (`byte_at`, `length` → Numbers) but not the *payload
extraction* (§4.2). So the accurate statement is:

- **Slice 1**: a handler can validate and route on a frame that arrives whole in one read, and can
  answer with computed Numbers and literals. It cannot echo a payload.
- **Reassembly across reads** is an append-accumulation into a state buffer, which
  `text_source_worst_case` (`src/verifier.rs:1666`) refuses for the reason
  `docs/text-state-fields-design.md` §6.2 #6 records: `worst_case = N + chunk > N` at *every* bound,
  so it needs a declared `on_overflow` policy (`text-state-2`) — not a bigger number.
- **Declared framing** (`frame: length_prefixed { header, length_at, length_bytes }`, the emitter
  reading exactly one frame per step and reassembling in a compiler-owned buffer) is the right
  long-term shape and is `multistep-3`. It is verifiable — the framing is a declaration the emitter
  applies, not a guess — and it takes reassembly out of the overflow prover's way, because the
  compiler owns the buffer and sizes it from `max_request`. It is also a length-field parser plus a
  partial-frame accumulator, which is the bulk of a protocol framer.

NC-1f is what keeps this deferral honest: a frame spanning two reads must be **refused at compile
time or fail closed at runtime**, never silently truncated or mis-parsed.

### 6.5 Secrecy of a bytes state field — argued now, because refusal #7 rests on it

Slice 1 defers bytes state (§5.5 refusal #7), so this is the argument the *lifting* slice has to
make. It is written here rather than deferred with the feature, because "a bytes state field would
hold a TLS write key" is the motivating use, and a deferral whose secrecy story is unexamined is a
deferral nobody can price.

**The buffer is never zeroed.** No emitter path emits a `rep stosb` over a state or entropy buffer at
any tail. `docs/randomness-effect-design.md` §7 already lists *"buffer zeroing at the iteration
tail"* as deferred hygiene for the entropy buffer, with the same reasoning; a bytes state buffer
joins that item rather than needing its own.

**What that does and does not expose:**

- **Not to a sibling connection.** Two connections are two children in two address spaces (§5.3).
  A key in one child's frame is unreachable from another — and it is unreachable *by construction*,
  because `docs/effect-model.md` refuses shared memory on principle, not by a check that could
  regress.
- **Not to a later process.** The child's pages return to the kernel at `sys_exit(0)`, and the kernel
  zeroes a page before handing it to a different process. So "the buffer is never zeroed" is a
  statement about *this* process's lifetime, not about leakage across processes.
- **Yes to anything that can read this child's memory**: a core dump, `ptrace` by the same uid,
  `/proc/<pid>/mem`. That is **exactly the exposure the entropy buffer already has** — so bytes state
  adds no new exposure *class*; it extends an existing one's duration from one request to one
  conversation. Worth stating plainly rather than discovering: a longer-lived secret in the same
  place is a real change in degree, and the answer to it is the zeroing item above, not a new
  mechanism.
- **Yes, within the child, to a read past a length.** The buffer persists across every step — that is
  the point — so an out-of-range read reads key bytes rather than garbage. The mitigation is
  `byte_at`'s fail-closed bound, and slice 0 is what first puts a *runtime* length under that check
  (§4.1-3). Before slice 0 there was nothing to bound; after it, there is a bound and it is checked.

**The accidental-echo hazard is the one that is NOT closed by any of the above, and it is why §6.2 is
a prerequisite rather than a nicety.** The property that normally prevents a secret reaching a client
is the bytes/text isolation — every text sink refuses a bytes value, so exposing one is always an
explicit, visible `byte_at` (`docs/randomness-effect-design.md` §5.4). That property is carried by
`infer_expr_type`, and `state.<f>` is invisible to it. So until the typing slice lands,
`concat("k=", state.key)` over a bytes state field is accepted by the verifier and emitted as text —
the key on the wire, rc 0, no diagnostic.

**Summary for the lifting slice: a bytes state field gets no secrecy property beyond "it lives in
this child's frame and the child exits", and it gets that only once §6.2's typing slice restores the
isolation the rest of the language relies on.** Both halves must be true before refusal #7 comes off.

---

## 7. What TLS still needs — the honest list

`docs/tls-io-statemachine-design.md` §5 says the in-Verbose driver needs *"exactly one language-infra
slice: per-connection persistent byte buffers + a multi-step handler"*. §7 of that same document
already revised it to three (*"streaming/buffer emit, per-connection buffers + multi-step handler,
and a `getrandom` effect"*). **It is more than three, and this note must not imply that shipping
slices 0 and 1 gets there.**

1. **Bytes-field inspection** — `byte_at` / `length` over a `bytes` field. Slice 0. Without it a TLS
   handler cannot read a record header. *(Not previously counted at all.)*
2. **Bytes materialisation, or record→bytes packing.** W2: every bytes value is streamed, so nothing
   can be copied into a state buffer; and a `Digest` is 32 **Number** fields, so feeding a crypto
   result into a byte buffer needs a byte-width encoder that does not exist. One of the two must be
   built. *(Revision 1 assumed a bytes `set` just worked.)*
3. **Framing / reassembly across reads.** A ClientHello can split at the MSS — `docs/tls-io-statemachine-design.md`
   §7's MINOR note measures a browser HRR key_share at ≈1216 B and says the parser "must reassemble a
   multi-record ClientHello" — and a client Finished can coalesce with the first application record
   in one segment. `multistep-3` + `text-state-2`.
4. **The entropy sampling unit for a step loop.** `random()` today is "once per name per
   **evaluation**" (`docs/randomness-effect-design.md` §5.1), which in a step loop means **per
   STEP**, inside `step_top`. TLS wants `server_random` drawn **once per connection** — a unit that
   does not exist and is not merely a placement choice: a draw hoisted above `step_top` would make
   every step of one connection share bytes, which §5.1's own argument forbids for records. Designing
   a per-connection unit is a slice, not a line move.
5. **`entropy-2`** — a draw in a `raw_tcp` handler at all (`src/native.rs:20481`), and a draw as a
   bytes-`concat` argument (which is what puts `server_random` into a transcript).
6. **Record `let`s in a service handler.** Measured on `main`: `let p = swap2(req)` in an HTTP
   handler verifies clean and dies with `unknown rule 'swap2' for native inlining`
   (`src/native.rs:16119`) — a **real** refusal behind a **misleading** breadcrumb (§4.5). This is
   agg-shaped, lives in the handler-let path, and is the single blocker between the shipped
   single-spawn crypto rules and a handler that can call them.
7. **Bounded append** (`text-state-2`) for the transcript hash.
8. **State read-path typing** (§6.2), a prerequisite of (2)'s state half.

**None of items 2–8 is a connection feature.** The honest framing: slices 0 and 1 deliver a working
multi-step service and close two of the eight; TLS-in-Verbose remains gated on six more, across three
unrelated arcs (aggregate composition, the streaming/bytes ABI, and the entropy unit).

`docs/tls-io-statemachine-design.md` §5 should be amended to point here rather than to "exactly one
slice" — noted, not done, because this note does not edit that file.

---

## 8. Not in these slices, and the slice that lifts each

| slice | lifts | why separate |
|---|---|---|
| **`rawtcp-inspect-0b`** | a bytes `substring`; a bytes-typed handler `let` | produces a bytes VALUE, whose sinks are streamed with no sizing pass (§4.2) |
| **`state-read-typing`** | `state.<f>` visible to `infer_expr_type` (thread the synthetic concept into `verify_rule`) | a prerequisite of bytes state, and a different plumbing point from `verify_service` (§6.2) |
| **`multistep-2`** | `bytes [..N]` state fields | gated on both `state-read-typing` and a materialisable bytes value (§6.4, refusal #7) |
| **`multistep-3`** | declared framing + emitter-owned reassembly | a length-field parser and a partial-frame accumulator (§6.4) |
| **`text-state-2`** | `set f = concat(state.f, …)` with `on_overflow:` | inherited unchanged; the transcript buffer needs it |
| **write-stall policy** | `SO_SNDTIMEO`, or a partial-write semantics for a streamed response | §5.4's named residual; "what does a partial write mean when the bytes are gone" is a semantic question |
| **aggregate composition in handlers** | `let dig = x25519_finish(…)` in a service handler | §7 item 6; agg-shaped, not connection-shaped |
| **`entropy-2` + a per-connection sampling unit** | a `raw_tcp` draw, and "once per connection" as a declared unit | §7 items 4–5 |
| **HTTP/1.1 keep-alive** | the step loop under `http_1_0` | also the trigger that makes §6.1's `session:` split mandatory |
| **sequential multi-step** | multi-step without forced fork | needs `poll`/`epoll`, against the OS-as-supervisor memory (§5.4) |
| **gen0 mirror** | `max_steps` / `read_timeout` parsing, the bytes-field gate, the step loop in `examples/vexprparse.verbose` | gen0 refuses in the safe direction until then |

---

## 9. Filter check — five pillars and the axiom

| Priority | How these slices pay |
|---|---|
| **1. Verifiability** | Slice 0: the operand gate is widened to a *named* shape (BoundText-registered bytes), never to `Type::Bytes` at large, so the criterion "a length the emitter can load" stays checkable. Slice 1: `max_steps`, `read_timeout`, state bounds and every `after:` source are mechanically checked; the loop's exit is three declared facts (EOF, `max_steps`, `read_timeout`) plus a fail-closed abort. |
| **2. Exploitability** | State bounds size buffers **and** drive the compile-time overflow proof (zero runtime check bytes when proved). `max_steps × read_timeout` is a computable per-connection ceiling. `read_timeout` needs no new branch because `-EAGAIN` reuses the shipped `jle`. Per-connection reset costs **zero instructions** — it is the fork (§5.3). |
| **3. Safety** | `byte_at` over a runtime length does real work for the first time (§4.1-3). Fail-closed is per **connection**, so one client's bad frame cannot stop the service. The claim is bounded honestly: work and read-time are declared; the write axis is a named residual (§5.4). |
| **4. Traceability** | `max_steps`, `read_timeout`, `concurrency: forked` are one keyword each. State fields, bounds and literal inits stay visible to `strings` (the init copies literals via jmp-over-data). Every state read is in the handler's `reads:` proof. |
| **5. Readability** | One lifetime rule — *`after:` runs at the bottom of the innermost loop* — replaces a lifetime table. No second state spelling until a program can observe two lifetimes at once (§6.1). |
| **Axiom (verify + apply, never guess)** | Frame widths from a `Vec` of declarations; `max_steps` / `read_timeout` literals; framing handler-managed and declared framing **deferred rather than inferred**; an unprovable `after:` source refused with the bound named. Slice 0 admits a runtime length only where the emitter *owns the slot* — it never infers one. |
