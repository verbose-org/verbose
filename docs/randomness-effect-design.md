# Declared randomness — a `getrandom` effect for Verbose

**Status: DESIGN NOTE, nothing built.** Written against `main = 490cd50` (2026-09-02). No
source file was modified, no binary compiled, no test run — every byte count below is a
prediction from instruction encodings, not a measurement, and is labelled as such. Citations
are `file:line` at `490cd50` and were each read while writing this.

This is a candidate row for [`docs/effect-model.md`](effect-model.md), answered against that
document's eight-question checklist (`docs/effect-model.md:114-127`), and it is the effect
[`docs/tls-io-statemachine-design.md:154-166`](tls-io-statemachine-design.md) names as *"the
one irreducible host crypto-input"*.

---

## 0. Executive summary

- **Shape**: a top-level declared item, not a bare primitive. `entropy <name>` with a
  declared byte count; referenced as `random(<name>)`. The `resource` / `connection` shape
  (`docs/effect-model.md:30-38`), not the `now_unix()` shape — §3.1 says why.
- **Width and type**: `random(<name>)` is **`bytes`**, `N` bytes long, delivered exactly the way
  `read(<resource>)` delivers a file today — a `(ptr, len)` pair into an `N`-byte stack buffer.
  **`1 ≤ N ≤ 256`**, and the 256 is not taste: it is the byte count below which the
  `getrandom(2)` contract says a read *cannot* be short or interrupted (§3.2, §5.3).
- **Sampling unit**: one `getrandom` per declared name per **evaluation** — per input record in
  a rule binary, per request in a service. Two references to one name in one evaluation see the
  same bytes (you need that: the nonce in the body and in the computation must agree). Two
  independent values are two declarations. The brief's "two `random_bytes(32)` in one rule must
  not be equal" is satisfied because the aliasing form is not writable (§5.1, §8.1).
- **Error policy**: `abort` (`sys_exit(1)`) in every context, including a listener. §5.3 argues
  it: the failure modes are enumerated from the syscall contract and **none is client-
  triggerable**, which is the exact opposite of the condition
  `docs/text-state-fields-design.md:279-289` refused to abort on.
- **Secrecy**: the value has no path to a text sink without an explicit, visible extraction.
  `concat`, `HttpResponse.body`, the `log:` grammar and text `==` all refuse a `bytes` operand
  today, so the hazard the brief names — echoing a secret by accident — is closed by the
  existing `bytes`/`text` isolation, not by new machinery (§5.4).
- **Forked**: the draw sits after the fork dispatch, in the child; the one position that would
  make children share a parent's sample (the cached-resource hoist above `accept_top`) does not
  exist for this item, and the negative control that re-creates it is named (§5.2).
- **Reproducibility**: the binary is a function of the source; the output is not. Both are
  asserted, differently (§6.4).
- **Slice 1**: the item, the primitive, `byte_at` / `length` over it, `output: bytes` rules,
  and HTTP handlers. A 16-byte nonce service and a 32-byte nonce CLI as worked examples,
  with tests whose negative controls each exercise a specific hazard (§6).

---

## 1. The gap, and why it is a language capability

### 1.1 What exists: nothing

`grep -rni 'getrandom\|urandom\|random\|entropy'` over `src/ast.rs`, `src/parser.rs`,
`src/verifier.rs`, `src/native.rs`, `src/interpreter.rs`, `src/wasm.rs`, `src/optimizer.rs`
and `docs/effect-model.md` returns only comments about Rust's randomly-seeded `HashMap`
hasher (`src/native.rs:1756`, `src/wasm.rs:966`, …) and the two ASLR-disclosure post-mortems.
There is no AST node, no primitive name in `PRIMITIVE_CALL_NAMES` (`src/parser.rs:37-74`),
no verifier arm, no emitter, and no row in the effect catalogue
(`docs/effect-model.md:18-26`). The brief's claim is confirmed.

Today the only secret the TLS arc cannot compute in Verbose is drawn by Python:
`tools/tls_gen/tls_server.py:52-55` — `sk = bytearray(os.urandom(32))` clamped into an
X25519 scalar, and `server_random = os.urandom(32)`. The same four lines recur in
`tls_cert_server.py:73-76` and `tls_browser_p256_server.py:210-213`, and every one of those
files' docstrings names it as *"the acknowledged host secret input"* (`tls_server.py:4`).
`docs/tls-io-statemachine-design.md:186-189` prices the in-Verbose driver as three slices,
the third being *"a `getrandom` effect"*. This note is that slice's design.

### 1.2 Three consumers that are not TLS

The effect is a language capability with TLS as one user among several. Each consumer below
is expressible the day slice 1 lands, with the primitives that already exist:

| consumer | shape | what it needs from the effect |
|---|---|---|
| **Per-request nonce / session token** in an HTTP service | `entropy nonce` (16 B); handler body renders `byte_at(random(nonce), i)` per byte; a client gets a fresh unguessable token per request | fresh per request, fresh per forked child, never equal across two requests, never a fixed value |
| **Salt for a stored hash** | a rule producing `Digest`-style records already takes `salt0..salt31 : number [0, 255]` (`examples/hkdf_extract.verbose:7-35`); `Extract { salt0: byte_at(random(salt), 0), … }` feeds it | 32 independent bytes per record; the same bytes readable at 32 sites in one evaluation |
| **Random backoff / jitter** | `1000 + byte_at(random(jitter), 0) * 4` as a number — a retry delay a fleet will not synchronise on | one byte, no bias worth worrying about at this width; the value is NOT secret and may go to stdout |
| **Audit-visible non-determinism** | exactly like `now_unix()` (`src/ast.rs:660-671`): a reviewer greps `reads:` and finds every rule whose output is not a function of its input | the name in `reads:`, verified both ways |
| (TLS) X25519 ephemeral scalar + `server_random` | `band(byte_at(random(sk), 0), 248)`, `bor(band(byte_at(random(sk), 31), 127), 64)` — the clamp at `tls_server.py:52` is three bitwise primitives Verbose already has | 32 bytes; the raw bytes must ALSO stream into a transcript (`output: bytes`), which is slice 2 (§7) |

### 1.3 The wrong way to get it, stated so it is refused rather than forgotten

A userspace generator — an LCG, xorshift, ChaCha over a seed — is refused on three grounds,
each independent. It would need *state*, and Verbose has no place for cross-evaluation state
except service `state:` fields, whose whole design assumes the value is not secret. It would
need *seeding*, and the seed is either the clock (guessable) or the kernel (in which case the
generator adds nothing but a copy of the secret in userspace). And under `concurrency: forked`
every child would inherit the parent's generator state by copy-on-write and **emit the parent's
stream** — the classic fork-safety failure of userspace RNGs, and precisely the hazard §5.2 has
to defend against for the kernel interface too. The kernel CSPRNG has no userspace state,
needs no seed from us, and is not inherited across `fork()`. `random(7)`'s own recommendation
is the one this design follows: *"either read from the /dev/urandom device or employ
getrandom(2) without the GRND_RANDOM flag."*

---

## 2. What already exists that this reuses — cited, not assumed

| mechanism | where | what the entropy effect takes from it |
|---|---|---|
| **The synthetic-read audit shape** | `now_unix()` inserts the synthetic name `now` into the rule's read facts (`src/verifier.rs:4512-4518`), `validate_read_path` accepts it as a length-1 base (`:4686-4690`), `check_purity` diffs declared vs performed in both directions (`:4956-4998`), and `walk_for_match_result_callees` propagates `now` out of an inlined callee (`:4085-4093`) | the name of an entropy item enters `reads:` through exactly these four sites, as a resource name does (`:4488-4490`, `:4670-4673`) |
| **The declared-item shape** | `Item::Resource` (`src/ast.rs:16-55`), `parse_resource` (`src/parser.rs:2093-2210`), the top-level keyword dispatch (`src/parser.rs:110-113`), name collection + duplicate + cross-namespace collision checks (`src/verifier.rs:219-249`), the `read(name)`-resolves-to-a-declaration cross-check (`:293-311`) | `Item::Entropy` is the third member of that family; the collision check exists because resource and connection names *"flow through `reads:` purity facts as a single identifier path"* (`:232-234`) and an entropy name flows the same way |
| **The `(ptr, len) + sized buffer` triple** | `emit_resource_read_sequence` (`src/native.rs:3225-3370`): `ptr_slot`, `len_slot`, a `max_bytes` buffer padded to 8, a descending cursor, `js rel32` into a shared abort tail; the same accounting in the HTTP frame (`:20577`, `:20603-20606`) and for text state fields (`:20653-20663`) | the entropy buffer is this triple with the `open`/`close` removed and the `read` replaced by one `getrandom`; it costs `16 + ((N + 7) & !7)` frame bytes per referenced name |
| **The BoundText consumers** | `classify_concat_arg` (`:7620-7650`), `emit_starts_with_load_text`'s `Read \| Ident \| Fetch` arm (`:17473-17475`), `emit_length` (`:17001`), `emit_handler_to_slots`'s body arms (`:21711-21800`), all keyed through `TextBindings` (`:7618`) | `byte_at` and `length` over `random(k)` resolve through the same `text_bindings` lookup a `read(k)` does; the *text* sinks (concat, body, log) are deliberately NOT extended — §5.4 |
| **The shared abort tail** | `emit_resource_abort_tail` (`:3078-3090`), the service's end-of-binary abort sequence (`:21512-21520`), the verifier's `abort`-only policy doctrine (`docs/effect-model.md:46-57`) | one more `jne rel32` patch site per draw; the label is emitted once per binary whether one or twenty sites reference it |
| **The per-accept position after `fork`** | fork dispatch (`src/native.rs:20916-20975`) → per-accept resources (`:21018-21047`) → READ (`:21050-21058`) → HTTP parse → connections (`:21140-21178`) → handler lets → handler → logs → after → tail (`:21480-21510`) | the draw goes between the connection block and the handler lets; every instruction there runs in the child under `forked` |
| **`byte_at` over a compile-time-length byte value** | `check_byte_addressable_operand` (`src/verifier.rs:2756-2777`) admits a `b"..."` literal alongside text; the CLAUDE.md "Declared constant byte tables" bullet argues why a bytes-typed *field* stays out ("runtime byte values with no compile-time length") | `random(k)` has a compile-time length by declaration + contract (§3.2), so it is admitted on the same criterion that keeps `req.data : bytes [..N]` out |
| **`bytes` as a runtime value** | `Type::Bytes` (`src/ast.rs:452-460`), `Value::Bytes` (`src/interpreter.rs:16`), the raw-write `--run` path (`src/main.rs:626-630`), `emit_bytes_program` / `emit_streaming_bytes_body` (`src/native.rs:7169-7300`), the `bytes`/`text` concat isolation (`src/verifier.rs:3252-3264`, `:3284-3300`) | the result type is not new; only its *source* is |
| **Record return** (agg arc) | `docs/bytes-value-return-design.md` §6, `examples/aggregate_pair.verbose` | NOT used for the primitive's own result (§3.2 option W1 says why); used by the consumers that build a `Digest`-shaped record from `byte_at` extractions |

---

## 3. Options considered

### 3.1 Declaration surface

| # | option | how it reads | verifier checks | failure mode | verdict |
|---|---|---|---|---|---|
| **D1** | **Primitive with a synthetic read**: `random_bytes(N)` anywhere an expression goes; the rule's `reads:` must list the synthetic name `random` — the `now_unix()` shape (`src/ast.rs:660-671`) | `let n = random_bytes(32)` | `random` in `reads:`; `N` a literal in `1..=256` | (a) Audit surface is one bit — `reads: [random]` says *a draw happens*, not how many, how wide, or which value feeds what; a reviewer counting nonces has to read the logic. (b) Per-call freshness forces a per-CALL-SITE buffer and a pre-walk to count sites (the `count_match_result_max_depth` pattern), and the "same value twice" need (body + log) then needs a `bytes`-typed `let`, which does not exist (Phase 2I lets are text/number). (c) Nothing stops `random_bytes(32)` and `random_bytes(32)` from being *meant* as the same value and compiled as two draws — a silent semantic trap in the direction that produces a wrong answer at rc 0. | **REJECTED** |
| **D2** | **Declared item, referenced by name**: top-level `entropy <name>` with `bytes : N`; `random(<name>)` in logic; `<name>` in `reads:` — the `resource` / `connection` shape (`docs/effect-model.md:30-38`) | `entropy nonce` … `random(nonce)` | duplicate name; collision with resource/connection names; `N` in `1..=256`; every `random(name)` resolves; `name` in `reads:` both ways | The declaration is one line more than D1 per draw. That is the whole cost, and it buys: every draw enumerated in one place with its width; sampling-per-name-per-evaluation, so reuse within an evaluation is *the default* and independence is *a second declaration*; no new value-binding machinery. | **RECOMMENDED** |
| D3 | Service-level `entropy:` block only (like `state:`) | `service s … entropy: nonce : 16` | as D2, scoped to services | Rule binaries (the CLI nonce, the salt generator, the backoff) would have no access; and every other effect is a top-level item usable from both rules and handlers. A service-only effect would be the first context-locked one for no reason the effect model states. | REJECTED |
| D4 | A `bytes`-typed input field the OS fills (`n : bytes [..32] = os_random`) | declaration on the concept | as a field bound | Puts an effect inside a *type declaration*; a concept becomes impure by annotation, invisible to the rule's `reads:`. The opposite of the naming convention (`docs/effect-model.md:32-38`). | REJECTED |

**What the verifier checks under D2**, mirrored arm-for-arm on the resource precedent so the two
cannot drift:

1. `entropy` names are unique and collide with neither resource nor connection names
   (`src/verifier.rs:219-249` gains a third loop).
2. `bytes : N` with `1 ≤ N ≤ 256` (§3.2); anything else refused with the breadcrumb in §6.2.
3. Every `random(<name>)` in a rule's logic or `let` RHS resolves to a declared `entropy`
   (`:293-311` gains a sibling walk, `collect_random_names`).
4. The name appears in the rule's `reads:` — `collect_expr_facts` inserts `[name]`
   (`:4488-4490` sibling); `validate_read_path` accepts it as a base (`:4670-4673` sibling);
   `check_purity` (`:4956-4998`) reports `missing: [nonce]` / `extra: [nonce]` unchanged;
   `walk_for_match_result_callees` propagates it out of an inlined callee (`:4089-4092`
   sibling), else a `match_result` on a drawing callee would fail purity in the caller.
5. `infer_expr_type(Random(_)) = Some(Type::Bytes)` (`:3724` sibling); `count_operations`
   charges 1 (`:5473` sibling, same as `Read`); `describe_expr_kind` names it (`:2015` sibling);
   `text_source_worst_case` has no arm for it, and does not need one — a `set <text field> =
   random(k)` is refused by the type check that now runs first (`:1335-1347`).
6. `check_byte_addressable_operand` (`:2756-2777`) admits `Expr::Random(_)` next to
   `Expr::Bytes(_)`; everything else that takes a text operand keeps refusing a `bytes` one.
7. The primitive name joins `PRIMITIVE_CALL_NAMES` (`src/parser.rs:37-74`, 36 → 37) so a rule
   named `random` is refused at its declaration; `primitive_call_names_matches_the_parser_chain`
   (`src/parser.rs:3533`) pins it to the parser chain automatically.

### 3.2 Width and type — settled against the consumers, not in the abstract

What the consumers actually consume today:

| consumer | representation it takes | evidence |
|---|---|---|
| `hkdf_extract` (salt, IKM), the PSK schedule, `handshake_secret` | **per-byte numbers**: `salt0..salt31 : number [0, 255]` | `examples/hkdf_extract.verbose:7-35` |
| X25519 `ladder` (the scalar) | **hex TEXT**: `scalar : text`, read two hex chars at a time with `byte_at(s.scalar, b_pos)` / `byte_at(s.scalar, b_pos + 1)` | `examples/ladder_recursive.verbose:59`, `:99-100`; the host passes `scalar32.hex()` (`tools/tls_gen/vcrypto.py:83-87`) |
| `server_random` into the ServerHello / transcript | **raw bytes** concatenated into a byte stream | `tls_server.py:55`, `:62-63` |
| a nonce a client will see | **rendered** — decimal per byte via `concat` today; hex once a `hex(...)` primitive exists (§7) | `concat` itoa path, `src/native.rs:7620-7650` |

Three different shapes; no single result type *is* the consumers' representation. So the
primitive should deliver the one shape every consumer can be built from with the primitives
that exist, and the options are:

| # | result | how the consumers get their shape | failure mode | verdict |
|---|---|---|---|---|
| W1 | **a record** of `N` fields `b0..bN-1 : number [0, 255]`, either a compiler-synthesised concept per width (the `HttpRequest` precedent, `src/verifier.rs:944-975`) or a user-declared one | `hkdf_extract`: field-to-field copy. ladder: needs hex text — no. `server_random`: needs bytes — no. nonce: `concat` of fields — yes | (a) Record lets exist only on the callable path (agg-1's `__agg_<let>_<field>` keying, `src/native.rs:13762-13768`, `:2528`) — not in an HTTP handler, so the flagship consumer cannot bind the value. (b) A record of 32 numbers is 256 bytes of frame for 32 bytes of entropy and 32 `mov`s to extract them from `rax` (`emit_eval_expr` leaves every field in rax — `docs/bytes-value-return-design.md` §3.4). (c) Two of three consumers still need a conversion. | REJECTED |
| W2 | **`text`**, `(ptr, len)` BoundText — literally what `read(<resource>)` returns (`src/verifier.rs:3724`) | every text consumer accepts it unchanged | **The secrecy hazard in one line**: `body: random(nonce)` and `concat("token=", random(k))` would both compile and write the raw secret to the wire, and `log: append_file … concat(random(k))` would be one grammar arm away from logging it. Also a lie: the bytes are not UTF-8 (`Type::Bytes` doc, `src/ast.rs:452-460`: *"a honest type that does not pretend to be text"*). | REJECTED |
| **W3** | **`bytes`**, `(ptr, len)` into an `N`-byte frame buffer; `length(random(k)) = N` | `hkdf_extract`: `Extract { salt0: byte_at(random(s), 0), … }` (32 visible extractions). nonce: `byte_at` + `concat`. `server_random`: `output: bytes` concat with a BoundText arm — slice 2. ladder: a hex encoder or a regenerated ladder taking 32 numbers — the consumer's follow-up, §7 | Every text sink refuses it by the existing isolation (§5.4), so exposing the value is always an explicit act. The two crypto consumers need one adapter each; neither adapter belongs to this effect. `byte_at` extraction is ~40 B per site (CLAUDE.md `byte_at` bullet), so a 32-byte record build is ~1.3 KB of code — acceptable for a rule that already unrolls SHA-256. | **RECOMMENDED** |

**Why `N ≤ 256`.** `getrandom(2)`: *"If the urandom source has been initialized, reads of up to
256 bytes will always return as many bytes as requested and will not be interrupted by
signals. No such guarantees apply for larger buffer sizes."* Under that bound, with `flags = 0`,
on an initialized pool, a return value other than `N` is impossible — so the emitter may treat
`len == N` as a **fact it trusts, not an assertion it checks**, which is the same standard the
input bounds-check meets (CLAUDE.md, "Field ranges … the emitter is trusting a fact, not an
assertion"). A program that wants more draws two names, or waits for the slice that adds a
fill loop (§7). Every consumer in §1.2 needs 32 or fewer.

### 3.3 The reference name

`random(<name>)` reads beside `read(<name>)` and `fetch(<name>, …)`; it is the reader, the
item keyword `entropy` is the source. Both words are reserved: `random` as a primitive call
name (§3.1 point 7), `entropy` as a top-level keyword the item dispatch recognises
(`src/parser.rs:110-113` sibling). gen0's `span_is_primitive` does not know `random`, so gen0
**refuses** any program using it (rc 1, zero bytes) — the direction its own banner calls *"the
project's documented safe direction"* for `parse_int` and `now_unix`
(`examples/vexprparse.verbose:17916-17924`). It does not join the corpus figure
(`EXPECTED_ACCEPTED`, `src/native.rs:50828`) until gen0 learns it.

---

## 4. The effect-model row

Written in the format of `docs/effect-model.md:18-26` so it can be pasted in.

| Effect | Declaration site | Required proof | Syscalls emitted | Error policy | Memory bound | Audit visibility | Allowed contexts |
|---|---|---|---|---|---|---|---|
| **`random(<entropy>)`** — kernel CSPRNG bytes | Top-level `entropy <name> { bytes: N, on_draw_error: abort }` (`on_draw_error` optional, `abort` the only value) | Rule's `reads:` lists the entropy name, checked both ways like a resource name | `getrandom(&buf, N, 0)` — **once per declared name per evaluation**: per input record in a rule binary (inside `loop_top`), per request in a service (after `fork`, after the HTTP parse). Flags word is the constant 0: no `GRND_RANDOM`, no `GRND_NONBLOCK`, no `GRND_INSECURE`. `syscall 318` on x86-64. | `abort` only. A return value `≠ N` (a `-errno`, or a short read — unreachable for `N ≤ 256` per the syscall contract) → `sys_exit(1)` via the shared abort label. In a listener this kills the process (sequential) or the child (forked); §5.3 argues that this is correct because no client can trigger it. Blocking before the pool is initialized is not a failure — the syscall waits, and that is the desired semantics. | `N` is a literal, verifier-bounded `1..=256`. Stack buffer of `N` bytes (padded to 8) + `(ptr, len)` = `16 + ((N + 7) & !7)` bytes per referenced name, in the resource/connection block of the frame | **Source**: the `entropy` block enumerates every draw and its width; the using rule's `reads:` names it. **Binary**: implicit, like `clock_gettime` — `strings` shows nothing; a disassembler shows `mov rax, 318`. No path, no host, no port to show. | Rule logic and `let` RHS in every emitter built on `emit_record_loop_prologue` (scalar / bool / `Result` / record / `text` / `bytes` outputs); HTTP service handler logic and handler `let`s. Consumed by `byte_at`, `length`, and as the whole body of an `output: bytes` rule. **NOT** in `log:` content, `after:` sets, reactions, `raw_tcp` handlers, collection / fold / parallel / vectorized rules, recursive callables — each refused with a breadcrumb (§6.2). |

The **"What is NOT in the effect model"** list (`docs/effect-model.md:97-112`) needs no
change: randomness was never listed as refused, and its first bullet is the rule this note
obeys — *"Adding a new syscall is adding a new effect, with its own declaration shape and audit
story."* The **`req.timestamp` doctrine** (`:91-95`) needs an amendment, stated in §8.7.

---

## 5. Semantics

### 5.1 The sampling unit — per name, per evaluation, and why not per invocation

`now_unix()` samples once **per rule invocation**: `clock_gettime` above `loop_top`
(`src/native.rs:4265-4283`), so every record in one argv batch sees the same instant, and
`docs/effect-model.md:93` makes the same choice for `req.timestamp` per accept. That is the
right unit for a clock (one instant per batch is a feature) and the **wrong unit for
entropy**: a rule binary run under `--stream` (`emit_stream_prologue`, `src/native.rs:19520`)
processes one line per iteration for the life of the process, and an above-`loop_top` draw
would hand every line the *same* nonce. The salt consumer has the same problem with a
multi-record argv batch.

So the draw is emitted **after `loop_top`** (`:4415`) — after the per-record field loads and
their bound checks, and *before* the `let` loop (`:4548`), since a `let` RHS may reference
`random(k)`. The buffer's frame slots are reserved once at prologue (they sit in the same
descending cursor as resources); only the syscall is per-record. In a service the equivalent
position is per accept (§5.2).

Within one evaluation, every reference to one name reads the same buffer. That is not a
concession, it is the feature: the nonce in the response body and the nonce fed to a
computation must be the same value, and D1's anonymous form could not express that without a
`bytes` let. Two independent values are two `entropy` declarations, each with its own buffer
and its own syscall — and the reviewer sees exactly two.

### 5.2 `concurrency: forked` — where the call goes, and the one place it must not

In `emit_http10_dynamic_bytes` the fork dispatch is at `src/native.rs:20916-20975`: the
parent takes `close + jmp accept_top`, the child falls through into the per-accept body. The
entropy draw is emitted **immediately after the connection block** (`:21140-21178`, which is
already after the HTTP parse) and **before the handler `let`s**. Every instruction there
executes in the child, so each child performs its own `getrandom` and the parent's frame is
never the source of a child's bytes. A fork after the draw would be the userspace-RNG failure
of §1.3 re-created with kernel bytes; the emit order is what prevents it.

The position that WOULD reproduce the failure exists in this emitter for resources:
`cache: true` hoists a resource's read to between `LISTEN` and `accept_top` (`:20866-20903`),
and its own comment says why that is right for a static asset — *"children inherit the
populated slot via COW with no per-child read cost"*. For entropy that is the exact wrong
property, so **the `entropy` item has no `cache:` field**; the parser rejects one with the
breadcrumb in §6.2, and negative control NC-1 (§6.5) is a compiler deliberately patched to
hoist the draw, which the distinctness test must catch.

`after:` state mutation under `forked` is refused (`docs/effect-model.md:106`); an `entropy`
draw under `forked` is **not** — the draw is per-child by construction and observed by the
request that made it. The two combinations differ in exactly the property that mattered there
(who observes the write), and refusing this one would reject a valid program.

### 5.3 Error policy — abort, in a listener too, and the argument

`docs/text-state-fields-design.md:279-289` rejected a runtime `sys_exit(1)` as the *primary*
gate for a text-state overflow, and the reason is stated there in one sentence: *"A remote
client who can make the source exceed `N` has a one-packet DoS."* The condition was
**client-controlled**. Entropy failure is not, and that inverts the conclusion. The
`getrandom(2)` failure list, each against this emitter:

| errno / outcome | condition (man page) | reachable here? |
|---|---|---|
| `EAGAIN` | *"would have blocked … `GRND_NONBLOCK`"* | **No** — flags are the constant 0 |
| `EFAULT` | buffer outside the address space | **No** — the buffer is `rbp`-relative in the frame |
| `EINVAL` | invalid flag | **No** — flags are 0 |
| `EINTR` | *"interrupted by a signal handler while blocked waiting for entropy"* | **Effectively no** — only while blocking pre-initialization, and only by a *handled* signal. The binary installs no handlers; under `forked` it sets `SIGCHLD` to `SIG_IGN` (`docs/effect-model.md:25`), which discards the signal without interrupting the syscall |
| `ENOSYS` | kernel < 3.17 | **Yes**, and permanent for the process — a deployment error, surfaced on the first evaluation |
| short read | `N > 256`, or pre-initialization | **No** — `N ≤ 256` by verifier bound; pre-init the call blocks rather than shortens |
| blocking | pool not yet initialized (early boot) | **Yes, and correct** — the first request waits until the kernel is seeded; serving it earlier would be serving a nonce that is not one |

So the only reachable *failure* is process-level and permanent, triggered by no input a client
sends. For that condition the fail-closed doctrine (`docs/effect-model.md:53-55`) applies with
no exception: *"the binary terminates rather than serve a request whose contract it cannot
honour."* Serving with a zero or stale buffer would be the false declaration this project
forbids by name; per-request `drop` would be structurally available here (the draw runs
*before* the response, unlike the `after:` block that made option D of §3.3 unavailable) and
would loop on every request forever, hiding the deployment error behind an empty-response
symptom. `abort` it is, in both contexts, through the shared tail — the child under `forked`
exits 1 and the parent keeps accepting, which is per-request fail-closed for free.

**The backstop is `cmp rax, N ; jne abort`, not `test rax, rax ; js abort`.** One compare
catches both a `-errno` and a short read, and it is what stands between "a future kernel or a
future widening of `N` changes the contract" and "a nonce with trailing stale bytes". It is
unreachable by contract for `N ≤ 256`, exactly like the text-state copy's 13-byte backstop, and
§6.5 NC-3 shows it is a mechanism rather than decoration.

**Not in slice 1 — a startup probe.** A `getrandom(…, 1, GRND_NONBLOCK)` before `LISTEN`
would turn "first request blocks until seeded" into "service refuses to start". That is worse
under a supervisor (a restart loop until the pool initializes, versus one request that waits),
it draws a byte no rule reads, and it uses the one flag this design refuses. Blocking is the
better semantics; no probe.

### 5.4 Secrecy — why the value cannot be printed by accident

The brief asks whether the existing bool / number / record output paths risk echoing the
secret. They do not, because a `bytes` value reaches none of them without an explicit
`byte_at`:

| sink | what refuses a `bytes` operand today | line |
|---|---|---|
| `concat(...)` in a text position | *"concat mixes bytes and text: a bytes argument … cannot appear in a text concat"* | `src/verifier.rs:3252-3264` |
| `HttpResponse.body` | the built-in concept's `body : text` (`src/verifier.rs:977-1000`); `body: random(k)` is a type error before any emitter sees it | `:990-994` |
| `log:` content | the closed grammar — literals, `req.*`, `resp.*`, `concat`, `json_escape`, `parse_int`, `length` — and nothing else | `:1837-1990` |
| arithmetic, ordering, `==` | the 2026-08-20 operand check: Number for arithmetic/ordering, Number-or-Text for equality | CLAUDE.md, "SECURITY — a `text` operand in arithmetic" |
| `after: set <text> = random(k)` | the 2026-08-30 `set` type check runs first | `:1335-1347` |
| number / bool / record output | a `bytes` value is not a number, a bool, or a record field of either type; only `byte_at(random(k), i)` is, and it is visible at the site | — |
| `--run` (interpreter) | prints a `Value::Bytes` raw only when the rule's declared output IS the bytes (`src/main.rs:626-630`); `--json` renders `\xNN` (`:657`) — both are the rule's *declared* output, not a leak | — |

**The `log:` grammar is deliberately not extended.** An audit log must be able to see that a
draw *happened* — it does, from the source (`entropy` block + `reads:`) — and must not be one
grammar arm away from recording the *value* of an X25519 scalar. An author who wants a
non-secret nonce in the audit line puts it in `resp.body` (an explicit choice) and logs
`resp.body` (an existing arm); §8.5 corrects the brief on this point.

**Not in slice 1 — buffer hygiene.** The buffer is overwritten by the next draw (sequential)
or discarded with the child (forked); it is not zeroed at the iteration tail. The same frame
holds `req.body` and resource contents for as long, and the process has no other reader, so
this is hygiene rather than a hole; a `rep stosb` at the tail is a later slice (§7) and must
be argued on its own, not slipped in.

### 5.5 Reproducible binary, non-reproducible output — the distinction, stated

The effect model's reproducibility rule (CLAUDE.md, "The emitter must be reproducible") is
about the **binary**: the same `.verbose` compiles to the same bytes, every run. This effect
keeps that: entropy items are iterated from `program.items` (a `Vec`) in declaration order,
the referenced names are collected in source order like `collect_rule_read_names`
(`src/native.rs:1580-1588`), and no `HashMap` order reaches an offset. Two compiles are
byte-identical, and §6.4 asserts it.

The **output** is non-deterministic by design — that is the effect's entire purpose — and
`now_unix()` is its only sibling in the catalogue. The consequence for testing is that no
test may assert a *value*; every runtime assertion in §6.4 is about a *property*: length,
distinctness, distribution, exit code. The optimizer already treats the sibling correctly —
`Expr::NowUnix` is never folded (`src/optimizer.rs:776-778`) — and `Expr::Random` gets the same
pass-through arms as `Expr::Read` (`:397`, `:741`). `length(random(k))` could legitimately fold
to `N`; slice 1 does not, so the `len_slot` load stays the single source of that number.

### 5.6 The exact emit — one draw site

Mirrors `emit_resource_read_sequence` (`src/native.rs:3254-3370`) with the `open` / `close`
removed. Slot triple `(ptr_slot, len_slot, buf_slot)` from the same descending cursor; register
use `rax / rdi / rsi / rdx` only — all ephemeral per the CLAUDE.md register table.

```asm
  push r11                        ; the arena base — a syscall clobbers r11 and rcx by ABI, and
                                  ; this draw runs AFTER the prologue's `lea r11`; the 4-byte
                                  ; push/pop is the discipline emit_streamed_write_rsi_rdx already
                                  ; uses (src/native.rs:6660)
  mov  rax, 318                   ; sys_getrandom (x86-64)
  lea  rdi, [rbp + buf_slot]      ; buf
  mov  rsi, N                     ; size — the declared literal
  xor  edx, edx                   ; flags = 0 — no GRND_RANDOM, no GRND_NONBLOCK, no GRND_INSECURE
  syscall
  pop  r11
  cmp  rax, N                     ; the backstop: -errno OR a short read both fail this
  jne  .abort                     ; rel32 → the shared sys_exit(1) tail (resource_abort_patches /
                                  ;         the service's abort_patches)
  mov  qword [rbp + len_slot], N  ; len — a fact, not rax: the compare above just proved them equal
  lea  rax, [rbp + buf_slot]
  mov  [rbp + ptr_slot], rax      ; ptr — so every BoundText reader works unmodified
```

**Predicted, not measured**: 50–66 bytes per draw site depending on displacement widths, plus
`16 + ((N + 7) & !7)` frame bytes per referenced name, plus the 16-byte abort tail once if the
binary had none. A program that references no entropy item reserves nothing, emits nothing,
and patches nothing — §6.4 test T5 is how that is proved rather than said.

The name registers in `text_bindings` under the composite key **`__entropy_<name>`**, not the
bare name. `req.body` is registered under the bare `"body"` (`src/native.rs:20515-20527`, the
comment on `state_text_key`), and the text-state slice measured what a bare-keyed second
registration does: `state.body` silently resolved to `req.body`'s slots. Whether a *resource*
named `body` collides the same way today is not measured here and is out of scope; the new
item simply does not take the risk.

---

## 6. Slice 1 — `entropy-1: declared randomness, byte-addressed`

### 6.1 Scope

| layer | in |
|---|---|
| **AST** | `Item::Entropy(Entropy { name, intention, source, bytes: u32, on_draw_error: ErrorPolicy })`; `Expr::Random(String)` |
| **Parser** | top-level `entropy <name>` block with mandatory `@intention` / `@source` (every declaration kind requires both — the PR #162 matrix), `bytes : N`, optional `on_draw_error : abort`; `random(<name>)` in `parse_primary` beside `read` (`src/parser.rs:1225-1236` is the shape); `random` in `PRIMITIVE_CALL_NAMES` |
| **Verifier** | the seven checks of §3.1; `Expr::Random` added to every walk that has an `Expr::Read` arm — the grep at `490cd50` lists them: `src/verifier.rs:452, 623, 743, 1604, 1768, 2015, 3724, 4198, 4232, 4488, 4949, 5161, 5414, 5473` |
| **Optimizer** | pass-through arms beside `Expr::Read` (`src/optimizer.rs:56, 397, 741`); never folded |
| **Native — rules** | frame reservation + per-record draw in `emit_record_loop_prologue` (`src/native.rs:3922`), which serves every scalar / `Result` / record / `text` / `bytes` output emitter; `random(k)` accepted as the whole body of an `output: bytes` rule (`emit_bytes_program`'s top-level shape check at `:7169-7195` gains the arm; `emit_streaming_bytes_body` at `:7245` gains a `(ptr, len)` → `emit_streamed_write_rsi_rdx` arm) |
| **Native — services** | frame reservation (`entropy_extra_bytes` beside `:20603-20606`) + per-accept draw after the connection block (`:21140-21178`); `byte_at` / `length` over it in handler logic and handler `let`s through the existing BoundText lookups |
| **Native — consumers** | `byte_at(random(k), i)` and `length(random(k))` via the `Read \| Ident \| Fetch` arms (`:17473`, `:17090`), widened to `Random` |
| **Interpreter** | `Expr::Random(name)` → `Value::Bytes` of `N` bytes drawn from `/dev/urandom` via `std::fs` (§6.6) |
| **WASM** | refused with a named breadcrumb (§6.6) |
| **Docs** | the §4 row into `docs/effect-model.md`; the §8.7 amendment to the `req.timestamp` doctrine; a CLAUDE.md Language Features bullet and example entries |

### 6.2 Refusals, each naming the offender and the lifting slice

| # | shape | breadcrumb (verbatim intent) | lifted by |
|---|---|---|---|
| 1 | `bytes : 0`, `bytes : 257+`, missing `bytes:` | `entropy 'k': bytes must be a literal in 1..=256 (got 300); getrandom(2) guarantees a full, uninterrupted read only up to 256 bytes — declare a second entropy item, or wait for slice entropy-3 (fill loop)` | entropy-3 |
| 2 | `on_draw_error : drop` | `entropy 'k': on_draw_error 'drop' is not accepted; entropy failure is process-level and not client-triggerable (docs/randomness-effect-design.md §5.3), so drop would loop on every request — only 'abort'` | none planned; revisit only if a real transient failure is measured |
| 3 | `cache : true` on an entropy item | `entropy 'k': 'cache' is not a field of entropy — a cached draw would be inherited by every forked child (§5.2); each evaluation draws fresh by construction` | never |
| 4 | duplicate name / collision with a resource or connection | `duplicate entropy name 'k'` / `entropy name 'k' collides with a resource of the same name; reads: lists merge both namespaces` (the `:235-249` wording) | never |
| 5 | `random(k)` with no `entropy k` | `random('k') references unknown entropy — declare it at top level with `entropy k ...`` (mirror of `:303-309`) | never |
| 6 | `random(k)` without `k` in `reads:` | the existing `declared reads do not match logic; missing: [k]` (`:4995`); `extra: [k]` for the reverse | never |
| 7 | `random(k)` in a text position: `concat`, `HttpResponse.body`, text `==`, `starts_with` & co. | the existing bytes/text refusals (§5.4) — no new message | `hex(<bytes>)` primitive, §7 (renders explicitly) |
| 8 | `random(k)` in `log:` content | the existing closed-grammar refusal (`:1986`) | never (§5.4) |
| 9 | `random(k)` in a recursive / callable-path rule | `recursive rule 'f': random('k') is not supported on the callable path (slice entropy-4: a draw-let with a per-callable buffer, the shape read-lets already have at src/native.rs:5423)` | entropy-4 |
| 10 | `random(k)` in a collection / fold / multi-fold / parallel / vectorized rule | `rule 'f': random('k') is not supported in <emitter> (slice entropy-5: per-element draws)` — one message per emitter, naming it | entropy-5 |
| 11 | `random(k)` in a `raw_tcp` handler, a reaction, an `after:` set | `<context>: random('k') is not supported here (slice entropy-2 / entropy-6)` | entropy-2 (raw_tcp, with the bytes stream), entropy-6 (reactions) |
| 12 | `random(k)` as a `concat` arg in an `output: bytes` rule | `output: bytes concat: random('k') as an argument is slice entropy-2 (streaming BoundText); slice 1 accepts random(k) only as the whole body` | entropy-2 |
| 13 | WASM | `random() has no WASM lowering: WASM has no syscalls, and admitting one effect through a WASI import (random_get) is the WASI-vs-host-imports design call CLAUDE.md names — the same one read() and fetch() are waiting on` | a WASM effects design, not an entropy slice |

### 6.3 Worked examples

**`examples/nonce_service.verbose`** — the flagship. A fresh 16-byte token per request,
rendered as dotted decimal (the rendering primitives that exist today; hex is §7).

```
@verbose 0.1.0

-- Declared randomness (slice entropy-1): one kernel CSPRNG draw per request,
-- fresh in every forked child, never equal across two requests. The raw bytes
-- cannot reach the response, the log, or a text comparison without an explicit
-- byte_at — every exposure is visible at its site.

entropy nonce
  @intention: "16 bytes of kernel randomness per request, the client's session token"
  @source: nonce_service.intent:1
  bytes : 16

rule issue
  @intention: "Answer every request with a fresh token"
  @source: nonce_service.intent:2

  input:
    req : HttpRequest

  output:
    resp : HttpResponse

  logic:
    resp = HttpResponse {
      status: 200,
      body: concat("token:",
        byte_at(random(nonce), 0), ".", byte_at(random(nonce), 1), ".",
        byte_at(random(nonce), 2), ".", byte_at(random(nonce), 3), ".",
        byte_at(random(nonce), 4), ".", byte_at(random(nonce), 5), ".",
        byte_at(random(nonce), 6), ".", byte_at(random(nonce), 7), ".",
        byte_at(random(nonce), 8), ".", byte_at(random(nonce), 9), ".",
        byte_at(random(nonce), 10), ".", byte_at(random(nonce), 11), ".",
        byte_at(random(nonce), 12), ".", byte_at(random(nonce), 13), ".",
        byte_at(random(nonce), 14), ".", byte_at(random(nonce), 15))
    }

  proofs:
    purity:
      reads : [nonce]
      calls : []
    termination:
      bound : 50

service token_server
  @intention: "HTTP token issuer — one fresh token per request"
  @source: nonce_service.intent:3

  listen:
    protocol    : http_1_0
    port        : 18960
    max_request : 4096

  handler: issue
```

The sixteen references to `random(nonce)` in one evaluation read one buffer (§5.1) — sixteen
loads of the same `ptr_slot`, one `getrandom`. A second file, `examples/nonce_service.verbose`
with `concurrency: forked` added, is the same source and is driven by the same test.

**`examples/nonce_cli.verbose`** — the rule-binary half, and the one that shows the
per-record unit. `./nonce_cli 1 2 3 | xxd` prints 96 bytes: three distinct 32-byte nonces,
one per record, no newline (the `output: bytes` contract, `src/native.rs:7163-7168`);
`--stream` prints one per stdin line for the life of the process.

```
@verbose 0.1.0

entropy seed
  @intention: "32 bytes of kernel randomness, one draw per input record"
  @source: nonce_cli.intent:1
  bytes : 32

concept Tick
  @intention: "a record whose only role is to be one evaluation"
  @source: nonce_cli.intent:1
  fields:
    n : number [0, 1000000]

rule nonce
  @intention: "emit the 32 raw bytes of one draw"
  @source: nonce_cli.intent:2
  input:
    t : Tick
  output:
    out : bytes
  logic:
    out = random(seed)
  proofs:
    purity:
      reads : [seed]
      calls : []
    termination:
      bound : 1
```

### 6.4 Acceptance tests

All in `src/native.rs` beside `now_unix_runtime_capture_and_verifier_check` (`:33429`), which
is the template for the verifier half, and `text_state_drive` (`:40327-40390`), which is the
template for driving a service over real TCP.

| test | asserts |
|---|---|
| **T1** `entropy_draw_is_fresh_per_record_and_per_request` | (a) `nonce_cli` on argv `1 2 3` → exactly 96 bytes on stdout, rc 0, the three 32-byte chunks pairwise distinct; (b) two invocations → six pairwise-distinct chunks; (c) `--stream` with 8 lines → 256 bytes, 8 distinct chunks; (d) `token_server` sequential: 64 requests → 64 bodies each matching `^token:(\d{1,3}\.){15}\d{1,3}$` with every number ≤ 255, pairwise distinct; (e) the same source with `concurrency: forked`: the same 64-request assertion — this is the one that catches NC-1 |
| **T2** `entropy_bytes_pass_a_gross_breakage_screen` | over the 64 × 16 = 1024 service bytes and the 64 × 32 = 2048 CLI bytes, separately: no chunk is all-zero; ≥ 200 distinct byte values seen (expected ≈ 251 of 256 at 1024 draws; the shortfall probability is negligible); monobit count within `bits/2 ± 5.5·√(bits/4)` (for 8192 bits: `4096 ± 249`). **What this screen is and is not** (§8.9): it distinguishes a CSPRNG from *zeros, a constant, or an aliased buffer*; it cannot distinguish it from a good userspace PRNG, and does not claim to |
| **T3** `entropy_verifier_requires_declaration_reads_and_bounds` | each refusal of §6.2 rows 1–8 with a **corrected twin** that must verify clean (the PR #157 discipline: an attributable refusal); the `reads:` half is asserted in BOTH directions (`missing: [nonce]` when omitted, `extra: [nonce]` when declared on a rule that draws nothing) — the half-checked-purity lesson from the gen0 arc |
| **T4** `entropy_emit_is_byte_identical_across_compiles` | compile `nonce_cli` and both `token_server` variants 20 times each → one sha256 per source; the `scc_callable_order_is_deterministic_across_compiles` shape (`:37961`) |
| **T5** `entropy_is_additive_by_measurement` | the corpus sweep (all 155 examples × every rule / service / reaction, compiler from `490cd50` vs branch, **baseline-vs-baseline control empty first**): 0 status changes, 0 size changes, 0 same-size-different-bytes on every pre-existing target; the only new rows are the new examples'. This is the standing check every slice since agg-1 has passed and is how "purely additive" is proved rather than argued |
| **T6** `entropy_flags_word_is_zero_and_no_userspace_source` | scan the emitted bytes of each example for `48 C7 C0 3E 01 00 00` (`mov rax, 318`) and require, within the next 24 bytes, `31 D2` (`xor edx, edx`) then `0F 05`; the count of such sites equals the number of referenced entropy names (1 per rule binary, 1 per service); `strings` contains no `/dev/urandom` and no `/dev/random` — the runtime tests cannot see the flags on a seeded host, which is why this one reads the bytes |
| **T7** `entropy_refused_on_the_callable_path_and_in_wasm` | §6.2 rows 9–13 by breadcrumb text, each with the smallest program that reaches the arm |

### 6.5 Negative controls — what to break to prove a test is not vacuous

Each is a deliberate one-line patch to the compiler (never committed), and the named test must
FAIL against it. The text-state design's §6.6 is the precedent for listing them.

| # | patch | which test fails, and how |
|---|---|---|
| NC-1 | emit the service draw between `LISTEN` and `accept_top` (the `cache: true` position, `:20866-20903`) | T1(e): under `forked` every child inherits the parent's buffer — 64 identical bodies. T1(d) also fails (sequential: one draw for the process). **This is the control that exercises the §5.2 hazard**, and it is the reason T1 drives the forked variant separately |
| NC-2 | emit the rule draw above `loop_top` (the `now_unix` position, `:4265-4283`) | T1(a): three identical chunks in one invocation; T1(c): eight identical lines. Two invocations (T1(b)) still pass — which is why (a) and (c) exist |
| NC-3 | keep the slot stores, delete the `syscall` | T2: all-zero chunks, 1 distinct byte value, monobit 0. T1 fails too (all chunks equal) — but T2 is the one that says *why* |
| NC-4 | `mov edx, 1` (`GRND_NONBLOCK`) or `mov edx, 2` (`GRND_RANDOM`) | T6 only. On a seeded developer machine T1/T2 pass against either flag — this is the reason a byte-scan test exists at all, and the reason §5.3's "flags are the constant 0" is asserted on the bytes rather than trusted |
| NC-5 | delete `cmp rax, N ; jne abort`, and separately set the syscall number to 335 (an invalid one, `ENOSYS`) | with both: the binary runs on garbage and *prints a token* at rc 0 — T2 catches it on the zero-initialised frame, T1 may not. With the backstop restored and only the bad syscall number: rc 1, no output — which is the assertion that makes the backstop a mechanism (§5.3) |
| NC-6 | register the buffer under the bare name instead of `__entropy_<name>`, and name the item `body` | a handler reading `req.body` resolves to the entropy slots or vice versa — the `state.body` measurement re-created; T3 gains a fixture naming an entropy item `body` and asserting `req.body` still echoes the request |
| NC-7 | drop `push r11 / pop r11` and compile a program that declares a `concept_group` and draws | the arena base is clobbered before the first `VariantConstruct` — a probe with a one-node arena must still produce its node; without the control, "r11 is safe" is an assertion |

### 6.6 Backends

**Native** — everything above; x86-64 Linux only, as every effect in the catalogue is.

**Interpreter** — the precedent is split and the split matters. `now_unix()` performs a **real**
clock read (`src/interpreter.rs:853-863`, `SystemTime::now()`); `read(...)` and `fetch(...)` are
**stubs** that return the empty text (`:793-797`, *"interpreter returns empty placeholder for
now"*). The stub precedent is refused for this effect: a zero-filled `Value::Bytes` is a silent
wrong answer of the class this repo has spent eight slices closing, and here the wrong answer is
also **a secret equal to zero**. So `Expr::Random(name)` draws `N` real bytes. With zero
dependencies (`Cargo.toml` declares none) the only `std` path is `std::fs::File::open("/dev/urandom")`
+ `read_exact` — one honest divergence from native, stated: `/dev/urandom` never blocks and may
return pre-initialization output on an unseeded host (`random(7)`), where `getrandom(0)` waits.
The interpreter is the reference for **value semantics**, not the deployment artifact, and a
developer's host is seeded; the divergence is recorded in the CLAUDE.md bullet, not hidden.
A raw `syscall` via inline `asm!` would remove it at the cost of the first inline assembly in
the compiler's own source — refused for slice 1 on that ground alone. Failure (no `/dev`, a
short read) is a `RuntimeError`, which `--run` turns into `exit(1)` — the interpreter's
`sys_exit(1)`. The entropy widths reach `eval_expr` as one more table threaded from `main.rs`
beside `all_rules` / `all_concepts`; a process global is refused.

**WASM** — refused with the row-13 breadcrumb, **not** the generic catch-all
(`src/wasm.rs:1858-1860`). Not WASI `random_get`, deliberately: WASM today refuses *every*
effect, and admitting one through an import would silently make the "WASI vs host-imports"
decision CLAUDE.md names as open. Randomness must not be the effect that sneaks that decision
in; when it is made, `random_get` is the obvious first import.

---

## 7. Not in slice 1, and the slice that lifts each

| slice | lifts | why it is separate |
|---|---|---|
| **entropy-2** — streaming bytes | `random(k)` as a `concat` argument in `output: bytes` rules (`emit_streaming_bytes_body` gains a BoundText arm); `random(k)` in `raw_tcp` handlers | this is what puts `server_random` into a ServerHello transcript — the TLS consumer's actual need; it touches the streaming ABI, whose interaction with a per-record buffer is unanalysed |
| **entropy-3** — `N > 256` | a fill loop that re-issues `getrandom` for the remainder and checks the total | the >256 contract is weaker (short reads possible), so the loop and its abort logic need their own argument |
| **entropy-4** — callable path | `let s = random(k)` in a recursive rule, with the buffer in the callable's own frame — the shape read-lets already have (`src/native.rs:5423-5445`) | per-frame draw semantics (fresh per recursion level?) is a design question, not a wiring one |
| **entropy-5** — collection contexts | a draw per element in `map` / `filter` / fold bodies (a nonce per output record) | each emitter has its own prologue; the per-element unit must be argued per emitter |
| **entropy-6** — reactions | `random(k)` in a reaction's `append_file` content | needs the §5.4 argument re-made for a file sink |
| **`hex(<bytes>) : text`** | rendering a draw as hex text for a client or for the ladder's `scalar : text` input | a separate primitive with a separate justification (it is a pure transform, not an effect); it is what makes the nonce example read `token:9f3a…` instead of dotted decimal |
| **ladder regenerated to 32 number fields** | `ladder(LadderState { …, scalar_0..31 })` from `byte_at` extractions, closing the hex-text seam at `examples/ladder_recursive.verbose:59` | a consumer change; generator-driven like every crypto conversion since tranche 2 |
| **buffer zeroing at the iteration tail** | `rep stosb` over the entropy buffers after the response | hygiene, argued on its own (§5.4) |
| **gen0 mirror** | `entropy` item parsing + `random` in `span_is_primitive` in `examples/vexprparse.verbose`, so gen0 stops refusing and joins the agreement cell | the gaps-table row for `now_unix` is the template; until then gen0 refuses in the safe direction |
| **`log:` admission** | — | **never**, §5.4 |

---

## 8. Where the brief was wrong, or where reading the code moved the design

**8.1 "For randomness [once per invocation] is WRONG — two `random_bytes(32)` in one rule must
not be equal."** The sentence presupposes an anonymous primitive. Under a *named* draw the
question dissolves: two references to one name are one value (which the body-and-log use case
needs), two values are two declarations (which the reviewer can count). And the unit the brief
did not name is the load-bearing one: not per-*call* versus per-*invocation* but per-
**evaluation** — a `--stream` binary would reuse a nonce for its whole life under `now_unix`'s
above-`loop_top` shape (§5.1). The emit position, not the call-site count, is the difference.

**8.2 "`getrandom(2)` … can short-read or fail (`EAGAIN`/`EINTR` early at boot)."** With
`flags = 0`, `EAGAIN` is impossible by the man page (it is the `GRND_NONBLOCK` errno), and for
`N ≤ 256` on an initialized pool a short read and a signal interruption are both excluded by
the same page. Pre-initialization the call *blocks*; it does not shorten. That contract is what
lets the verifier bound `N` at 256 and lets the emitter store `N` as the length (§3.2, §5.6).

**8.3 "In a service that kills the listener (the text-state design §3.3 argued at length why
that is wrong for a per-request condition)."** The citation is right and the conclusion
inverts: §3.3's argument was about a **client-controlled** condition (a one-packet DoS).
Entropy failure is process-level and not reachable from the wire (§5.3), so fail-closed is
correct. Also, §3.3's option D was *"structurally unavailable"* because the `after:` block runs
after the response; the draw runs *before* it, so per-request `drop` IS available here — and is
still not chosen, for the reason §5.3 gives.

**8.4 "the interpreter … may use `std`."** True, and the brief's framing misses the actual
decision: the interpreter today **stubs** `read`/`fetch` to empty text
(`src/interpreter.rs:793-797`) while performing a real `now_unix()` (`:853-863`). Two
precedents, opposite directions; the choice (§6.6) is between them, not between `std` and
`libc`.

**8.5 "a `now_unix()`-style non-determinism the audit log must be able to see."** The log must
see that a draw *occurred* — and does, from the source. Admitting `random(k)` to the `log:`
grammar would make logging an X25519 scalar a one-line convenience. Corrected: audit-visible in
the **source**, refused in the **log grammar** (§5.4).

**8.6 "a record of `number [0,255]` fields (the crypto files' representation)."** Half of it.
`hkdf_extract` and the PSK schedule consume per-byte numbers; the X25519 `ladder` consumes
**hex text** (`examples/ladder_recursive.verbose:59`; `tools/tls_gen/vcrypto.py:83-87` passes
`scalar32.hex()`); `server_random` is raw bytes into a stream. No result type *is* the
consumers' representation, which is why §3.2 settles on the shape each of them can be built
from rather than on one of them.

**8.7 The `req.timestamp` doctrine forbids what the worked example does — the doctrine needs
one sentence, and this is a decision for the author.** `docs/effect-model.md:91-95` restricts
`req.timestamp` to `log:` scope so that *"the response stays a function of `(method, path,
body)` alone"*. A nonce service is non-reproducible by design. Two observations make the
amendment small: (a) the doctrine is not what the code enforces — the verifier already accepts
`now_unix()` with `reads: [now]` in a handler (nothing in `verify_service` refuses `now`; the
only two sites are `src/verifier.rs:4091` and `:4689`), and it is the HTTP *emitter* that
refuses, by having no `now` slot (`src/native.rs:16735-16741`; no capture in
`emit_http10_dynamic_bytes`) — a pre-existing verify-accept / emit-refuse cell, fail-closed;
(b) `req.timestamp` is a *synthetic* name a handler cannot declare, whereas an entropy name is
a *declared* read the handler must list. Proposed wording: *the response is a function of
`(method, path, body)` and of the non-deterministic reads the handler DECLARES in `reads:`;
`req.timestamp` is the one non-deterministic value that is deliberately not declarable there.*
The alternative — refuse randomness in handlers — keeps the doctrine verbatim and deletes the
flagship consumer; recommended against, but it is the author's call, not this note's.

> **DECIDED (2026-08-31, project author): amend the doctrine.** A service response is a
> function of `(method, path, body)` **and of the effects the handler declares in its
> `reads:` proof** (`now`, `random(<name>)`). Auditability is preserved in full — every source
> of non-determinism is greppable in the proof block, which is the whole point of the effect
> model. "Replaying a request gives the same response" now holds exactly for handlers that
> declare no such effect, which is already true of `now`. The alternative (refusing randomness
> in handlers) would have deleted the flagship consumer and blocked the in-Verbose TLS driver,
> whose `server_random` must be drawn inside the handler. `docs/effect-model.md`'s wording is
> amended in this same commit.

**8.8 "a `bytes` value (which is streaming-only today)."** Mostly right; `bytes` also exists as
a `raw_tcp` field value with a `(ptr, len)` (`check_raw_tcp_binding`, `src/verifier.rs:2079-2165`),
and `(ptr, len)`-into-a-frame-buffer is exactly how `read`/`fetch` deliver text. A bytes-typed
BoundText is a new *source* for an existing representation, not a new representation.

**8.9 "verified to have kernel-quality entropy by a cheap statistical check or by the
`getrandom` contract."** Not *or*. No statistical screen can tell a CSPRNG from a competent
userspace PRNG, so the security evidence is structural — syscall 318, flags word 0, no userspace
state, drawn after `fork` — and it is asserted on the emitted bytes (T6, NC-4). The screen (T2)
catches gross breakage only: zeros, constants, aliasing. Both are in §6.4 with that division of
labour stated.

**8.10 Confirmed as stated**: `main = 490cd50`; the grep-confirmed absence of any randomness
surface; `tls_server.py:52-55` as the single host-side secret source; `now_unix` sampled once
per invocation into a dedicated `rbp` slot (`src/native.rs:4265-4283`, `:4539-4541`).

---

## 9. Filter check — the five pillars and the axiom

- **Verifiability**: every draw is a declaration with a width, cross-checked against use in
  both directions; the width bound is derived from the syscall contract, not chosen.
- **Exploitability**: the declared `N` sizes the buffer and is the stored length; the bound
  `≤ 256` is what makes the `jne` backstop unreachable rather than a runtime check the program
  pays for.
- **Safety**: fail-closed on the one reachable failure; the value cannot reach a text sink
  without a visible extraction; forked children never share a sample.
- **Traceability**: `entropy <name>` + `reads: [<name>]` — grep-able exactly like `now` and a
  resource name.
- **Readability**: `random(nonce)` beside `read(config)` and `fetch(upstream, …)`; the effect
  model row reads like its neighbours.
- **The axiom** (verify + emit canonical, never guess): one syscall, one flag word, one
  position per context, one error policy — no options the emitter chooses among.
