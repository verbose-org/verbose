# Roadmap: TLS 1.3 HTTPS server in pure Verbose

Status: **map, post-review** (2026-05-30). Adversarially reviewed by a
fresh-context subagent; review findings folded into §3 (middlebox-compat
requirements), §5 (signature confirmed unavoidable; goal split), §6 (Gap C
de-escalated with a concrete deferral path), and §8 (RFC 8448 promoted to the
first milestone). Goal restated: a hello-world HTTPS server, written
in Verbose, that a real browser (or `curl`) can connect to over TLS 1.3 with
cipher suite `TLS_AES_128_GCM_SHA256`, X25519 key share, and a dedicated
self-signed certificate — the handshake decrypted/encrypted entirely by
Verbose-emitted machine code.

This document maps what remains, in what order, what is feasible with current
primitives vs needs new infrastructure, and the hard decisions. Read against
[[project_verbose_compiler_no_guessing]]: every new construct must keep
termination mechanically provable and add no heuristic to the compiler.

## 1. What "done" means

A browser navigates to `https://<host>:<port>/`, completes a TLS 1.3 handshake
(showing a self-signed-cert warning is acceptable), and sees "hello world". The
server's TLS state machine, record protection, key schedule, signature, and
X25519 are all Verbose-emitted. The cert may be generated out-of-band (openssl)
but the server uses its private key to sign the live transcript.

## 2. Inventory — what is already built and RFC-validated

| Primitive | Rule(s) | Validated against |
|---|---|---|
| AES-128-GCM (1 block, empty AAD) | `aes_gcm` | NIST GCM TC-2 (C+T) |
| AES-128-CTR | `aes_ctr` | NIST SP 800-38A + openssl |
| GHASH / GF(2^128) | `ghash`, `ghash_mul` | NIST SP 800-38D |
| SHA-256 | `sha256_*` | sha256sum |
| HMAC-SHA256 | `hmac_sha256` | RFC 4231 |
| HKDF Extract/Expand | `hkdf` | RFC 5869 |
| X25519 | `ladder_recursive` + `x25519_finish` | RFC 7748 §5.2 |

Plus the language infra they exercised: recursive callables threading ~51
number + 1 text field through 255 frames with a `decreasing` proof; 10-limb
multi-precision field arithmetic that fits in i64; runtime-indexed `byte_at`;
runtime shift amounts; the HTTP/1.0 service emitter (raw TCP accept loop,
`fetch` outbound, `read` resources, forked concurrency).

## 3. The TLS 1.3 handshake (server side), mapped to primitives

Flow (one round trip, server perspective), with what each step needs:

1. **Receive ClientHello** — parse TLS record (type=handshake), extract: client
   random, cipher suites, `key_share` (X25519 client public, 32 bytes),
   supported_versions, signature_algorithms. → needs **record/handshake byte
   parsing** (new: structured parse of a TLS record + handshake message + TLS
   extensions; doable with byte_at + length fields, but fiddly).
2. **Send ServerHello** — server random, chosen suite, `key_share` (server
   X25519 public = X25519(server_eph_priv, 9)). → needs **X25519 base-point
   scalar mult** (have: ladder with u=9) + **message serialization** (new).
3. **Derive handshake secrets** — ECDHE shared = X25519(server_eph_priv,
   client_pub) (have). Then the **TLS 1.3 key schedule**: Early Secret =
   HKDF-Extract(0, 0); Handshake Secret = HKDF-Extract(Derive-Secret(Early,
   "derived"), ECDHE); traffic secrets via **HKDF-Expand-Label** /
   **Derive-Secret**. → needs **HKDF-Expand-Label** (thin wrapper over `hkdf`:
   builds the `HkdfLabel` struct then Expand) + **transcript hash** (running
   SHA-256 over all handshake messages so far). Both feasible with current
   primitives; Expand-Label is mostly serialization.
4. **Send {EncryptedExtensions, Certificate, CertificateVerify, Finished}** —
   encrypted under the server handshake traffic key.
   - EncryptedExtensions: trivial (empty-ish). Serialization.
   - Certificate: the DER cert bytes (static, from the self-signed cert). Just
     embed and frame.
   - **CertificateVerify**: sign(transcript-hash context string) with the
     cert's private key. → **the signature arc** (§5, the biggest gap).
   - Finished: HMAC over the transcript with the finished key (have HMAC).
   - All four wrapped in **record-layer AEAD** (§4).
5. **Receive client Finished** — decrypt + verify its HMAC. → **AEAD decrypt**
   (§4) + HMAC (have).
6. **Derive application traffic secrets**, then **respond to the HTTP GET** with
   "hello world" encrypted as an application-data record. → record AEAD + the
   existing HTTP response logic.

**Middlebox compatibility mode (REQUIRED for browsers — §3 review fix).** A real
browser will not interoperate without TLS 1.3's compatibility quirks:
- **legacy_record_version = 0x0303** in record headers (the real version lives
  in the supported_versions extension = 0x0304).
- **Echo the client's legacy_session_id** verbatim in ServerHello (it is
  non-empty from browsers).
- Emit a **dummy ChangeCipherSpec record** (`14 03 03 00 01 01`) after
  ServerHello, and tolerate one from the client. It is ignored cryptographically
  but its absence breaks middlebox-tolerant clients.
- **HelloRetryRequest**: avoidable ONLY if the ClientHello's first `key_share`
  already offers X25519. Browsers commonly do (X25519 or P-256 first), but it is
  NOT guaranteed — at minimum the server must DETECT a missing X25519 share and
  either HRR or fail cleanly. A hello-world MAY fail-closed on no-X25519, but
  must not misparse.
- **SNI**: may be ignored (single host). **ALPN**: optional (skip → no ALPN
  extension; browser falls back to HTTP/1.1 over the TLS channel).

## 4. Gap A — the record layer (AEAD over real records)

Our `aes_gcm` is **single 16-byte block, empty AAD**. TLS records need:
- **Multi-block GCM** — records are up to 16 KB; GCM encrypts N counter blocks
  (CTR) and GHASHes N+AAD blocks. Our CTR is per-block (caller increments the
  counter); GHASH is 2-block. Both must extend to **N blocks**, which means a
  loop → the **recursion infra** (same pattern as the ladder: fold the block
  index with a `decreasing` counter). GHASH-over-N-blocks and CTR-over-N-blocks
  become recursive rules. Feasible (recursion proven), sizeable.
- **AAD** — the GCM additional authenticated data is the 5-byte record header;
  GHASH must fold AAD blocks before ciphertext blocks. Small extension.
- **GCM decrypt + tag verify** — CTR is its own inverse (have); the tag is
  recomputed and compared. Need constant-time tag compare.
- **Nonce** — per-record nonce = static IV XOR sequence number. Trivial.
- **Record framing** — 5-byte header (type, version, length) + the inner
  content type byte (TLS 1.3 puts real type at the end of plaintext). Parsing
  + serialization.

This is its own arc: "AES-128-GCM AEAD over arbitrary-length records, both
directions, with AAD". The recursion-over-blocks is the new piece.

## 5. Gap B — the server signature (the pivotal decision)

TLS 1.3 mandates the server proves possession of the cert private key by
signing the handshake transcript (`CertificateVerify`). A browser — even
ignoring trust — verifies this signature, so it is **unavoidable**. Options:

- **Ed25519** (recommended). REUSES the X25519 field arithmetic (same prime
  2^255-19), but adds: **SHA-512** (new hash: 64-bit words, 80 rounds — exact
  on i64, structurally like our SHA-256 but wider) and **twisted-Edwards
  scalar multiplication** (point add/double formulas — different from the
  Montgomery ladder, but on the same field; ~the same shape of recursive
  scalar-mult over a `decreasing` bit counter) and **mod-L reduction** (L = the
  group order, a 253-bit prime ≠ the field prime — a second multi-precision
  modulus). A real arc, but the most aligned with what exists.
- **ECDSA P-256** — entirely new field (mod p256) and curve; no reuse. More work.
- **RSA-2048** — big-int modexp with a 2048-bit modulus = arbitrary-precision
  far beyond 255-bit; our 10-limb scheme doesn't scale there cheaply. Avoid.

Recommendation: **Ed25519**, as its own arc, sequenced after the record layer
(so we can test the handshake framing before the signature lands). Sub-bricks:
SHA-512 → Edwards point ops → scalar-mult → mod-L → ed25519_sign, each
validated against RFC 8032 test vectors.

**Confirmed by review — the signature is genuinely unavoidable for the browser
goal.** `CertificateVerify` is checked by every compliant client, including
`curl --insecure` and `openssl s_client` (the `--insecure`/`-verify 0` flags
disable chain/hostname TRUST, not the transcript-signature check that proves key
possession). **PSK-only mode** (RFC 8446 §2.2) skips certificates and signatures
entirely, but no browser will PSK with an unknown server — so PSK only helps a
client we also control, not the stated browser demo. **Therefore split the
goal**: (i) an offline/`s_client` milestone reachable WITHOUT the Ed25519 arc
only if we use PSK or a controlled client; (ii) the real browser demo REQUIRES
Ed25519. Do not pretend the browser goal is reachable before the Ed25519 arc.

## 6. Gap C — the I/O and orchestration model

The existing HTTP service emitter is a raw TCP accept loop with a fixed
parse→handler→serialize shape. TLS interposes a **stateful, multi-message,
encrypted** exchange before any HTTP. Open questions:
- **State across reads**: the handshake spans several record reads/writes with
  evolving keys. The current service model is one-shot (read request → write
  response). TLS needs a multi-step state machine within one connection.
- **Composition vs recursion ABI**: our crypto rules return one Number/byte per
  invocation (the `which` pattern), and recursion can't return a record. A
  handshake that pipes a 32-byte secret from X25519 into HKDF into GCM cannot
  re-run each stage per output byte (the ladder alone is ~5 s). So the TLS
  driver needs a way to compute a stage **once** and feed all its bytes
  forward. This is the same wall hit in X25519 (resolved there by external
  orchestration of ladder→finish). For TLS, the orchestration is the protocol
  itself — which we want in Verbose. **This is the deepest unknown**: either
  (a) a new "rule returns a byte buffer" capability (record/array return from a
  rule, currently refused), or (b) a single mega-rule that recomputes
  everything per output byte (untenable performance), or (c) a staged model
  where each stage writes its output to a resource/buffer the next stage reads.
  Resolving this likely needs a **language-infra slice** (mutable byte buffers
  / multi-byte return), not just more crypto.

**Review de-escalation of Gap C.** The blocker is real but smaller than first
stated. Two existing mechanisms already let a stage compute ONCE and feed all
its bytes forward, deferring any new return-shape primitive:
- **Stage-via-resource**: a stage writes its output bytes to a file
  (`append_file`) and the next stage `read`s them. The crypto rules already take
  text/byte inputs and `byte_at` them; the handshake driver can be a sequence of
  rules glued by the OS/filesystem (or by the orchestrating service), exactly
  the ladder→finish external-composition pattern. Per-connection file I/O is
  ugly but works and needs NO language change.
- **Recompute-per-byte is tolerable for a demo**: the ladder is ~5 s; a
  handshake does ~2 scalar mults + a few AEAD records ≈ tens of seconds per
  connection. Unacceptable for load, fine to prove correctness.
So Gap C does NOT block the offline RFC 8448 milestone (§8); the clean
in-Verbose, in-memory data-flow (a real "rule returns a byte buffer" or mutable
state) is a LATER infra slice, justified on its own merits once the protocol
logic is proven. It is the right long-term fix, not a prerequisite.

## 7. Proposed ordering (critical path)

1. **Record layer arc** (Gap A): N-block GCM (CTR + GHASH via recursion) + AAD
   + decrypt + framing. Validate against openssl AES-128-GCM on multi-block
   AEAD vectors. Independent of signatures — unblocks testing the rest.
2. **Key schedule**: HKDF-Expand-Label + Derive-Secret + transcript hash.
   Validate against RFC 8448 (the TLS 1.3 worked-example trace) intermediate
   secrets — RFC 8448 gives every secret byte-for-byte, an excellent anchor.
3. **Resolve Gap C** (the buffer/return-shape infra) — likely a design doc of
   its own. This is the gate for an in-Verbose handshake driver.
4. **Ed25519 arc** (Gap B): SHA-512 → Edwards → sign, vs RFC 8032.
5. **Handshake state machine + record I/O**: assemble against RFC 8448, then a
   live `curl`/browser.

Steps 1, 2, 4 are independently validatable offline (RFC 8448 / 8032 / openssl)
before any live socket — that is the disciplined path. Step 3 is the riskiest
and may reorder things.

## 8. Honest scope assessment

This is **larger than the entire crypto arc so far**. AES-GCM+SHA+HKDF+X25519
was ~5 PRs of self-contained primitives with clean RFC anchors. TLS adds: a
second hash (SHA-512), a second curve (Edwards) + second modulus (mod L),
N-block AEAD, a stateful protocol, and — the real unknown — a
multi-byte/stateful data-flow model the current rule ABI doesn't have (Gap C).
RFC 8448 makes the handshake offline-testable, which de-risks correctness; the
infra (Gap C) is where the genuine language design work is.

**FIRST MILESTONE (review-promoted): reproduce RFC 8448 offline.** RFC 8448
("Example Handshake Traces for TLS 1.3") publishes a complete handshake with
EVERY secret, key, IV, and message byte spelled out — including the server's
ephemeral private key and randoms — so the entire ServerHello..Finished output
is **deterministically reproducible**. The milestone: feed the recorded
ClientHello + the example server private values, and emit the exact bytes RFC
8448 lists, checked byte-for-byte. This exercises key schedule, transcript hash,
record AEAD, and (in the full trace) the signature, against an authoritative
anchor — with **no live socket and no new I/O model** (Gap C deferred). Note:
RFC 8448's main trace uses RSA/ECDSA for the cert; for an Ed25519 path, anchor
the signature sub-arc on RFC 8032 separately and the rest on 8448. This is the
smallest milestone that proves the protocol stack end-to-end; live I/O and the
in-Verbose driver come after.

## 9. Risks

- **Gap C (data-flow ABI)** is the pivotal unknown — it may require a real
  language-infra slice (byte-buffer return / mutable state) before a pure-Verbose
  handshake driver is possible. Everything else has RFC anchors and known shapes.
- **Ed25519** is a full arc (SHA-512 + Edwards + mod L), not a brick.
- **Performance**: the ladder is ~5 s; a handshake doing several scalar mults +
  N-block AEAD per connection may be seconds-to-minutes. Fine for a demo, not
  for load. State it honestly.
- **Constant-time**: more secret-dependent operations enter (tag compare, sig).
  Claim "branch-free where it matters", not "constant-time", until audited.
