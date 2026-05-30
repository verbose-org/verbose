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
- **HelloRetryRequest (REQUIRED for browsers — review B4 correction)**: HRR is
  NOT avoidable for modern browsers. Current Chrome/Firefox lead with
  **X25519MLKEM768** (post-quantum hybrid, group 0x11ec) as their first and
  often only initial `key_share`, offering plain X25519 (0x001d) in
  `supported_groups` but WITHOUT a key_share for it. An X25519-only server then
  MUST send HelloRetryRequest to ask the client to resend an X25519 share. So:
  CLI clients (`curl --curves x25519`, `openssl s_client`) can be pinned to send
  X25519 first → HRR-free; **browsers cannot** → HRR is mandatory for the browser
  milestone. HRR is a special ServerHello (magic random) naming the group, plus a
  synthetic `message_hash` wrapper of ClientHello1 in the transcript.
- **SNI**: may be ignored (single host, ack optional). **ALPN**: skip → no ALPN
  extension → client uses **HTTP/1.1** (matches our handler). Getting this wrong
  shows up as "TLS succeeds, then the browser sends an HTTP/2 preface our
  HTTP/1.0 handler can't parse" — so omitting ALPN is a correctness choice.
- **ServerHello MUST also carry `supported_versions` (0x002b → 0x0304)** beside
  `key_share` — without it the client falls back to TLS 1.2 and the 1.3 path
  collapses. `key_share` entry = NamedGroup(x25519=0x001d) + len(0x0020) + 32-byte
  public.
- **ClientHello parsing must skip unknown extensions by length** — browsers send
  10–15 extensions including GREASE values (0x?a?a); reject-on-unknown breaks.

**Transcript-hash rules (review M3 — silent-failure footguns):**
- The transcript hashes **handshake-message bytes only** (`HandshakeType ||
  length || body`) — the 5-byte record headers, the inner content-type byte, and
  the dummy CCS are **excluded**.
- Cut points that matter: Hash(CH..SH) for handshake secrets; Hash(..Certificate)
  for CertificateVerify; Hash(..CertificateVerify) for the server Finished;
  Hash(..server Finished) for the client Finished and app secrets.
- `CertificateVerify` signs `64×0x20 || "TLS 1.3, server CertificateVerify" ||
  0x00 || Transcript-Hash(..Certificate)` — pin these context bytes exactly.
- `finished_key = HKDF-Expand-Label(server_hs_traffic_secret, "finished", "", 32)`;
  Finished = `HMAC(finished_key, Transcript-Hash(..CertificateVerify))`.
- If HRR fires, ClientHello1 is replaced in the transcript by a synthetic
  `message_hash` wrapper — handle per RFC 8446 §4.4.1.

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

**GCM correctness checklist (review M2 — known footguns our single-block TC-2
path does not exercise):**
- `J0 = nonce(12 bytes) || 0x00000001`; the **tag uses `E(J0)`**, the first data
  counter block is **`inc32(J0)`** (counter = 2 for block 1).
- GHASH order: **AAD blocks, then ciphertext blocks, then the length block.**
- **Zero-pad** a partial AAD block and the final partial ciphertext block to 16
  bytes before GHASH; the length block counts the **true unpadded** lengths.
- Final GHASH block = **`len(AAD)_bits || len(C)_bits`**, each a 64-bit
  **big-endian** bit-length (the classic GCM bug: bytes vs bits, or wrong
  endianness).
- `tag = GHASH(...) XOR E(J0)`.
- **Decrypt order**: compute the tag over the *received* ciphertext+AAD, compare
  it **constant-time**, and release plaintext ONLY on match (verify-then-return).
- Note: NIST GCM TC-2 exercises neither AAD nor multi-block, so there is
  currently **zero regression coverage** for any of the above — validate the new
  path against `openssl enc -aes-128-gcm` / NIST GCM vectors with AAD.

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

**Milestone 1 — live PSK + (EC)DHE handshake, no certificate/signature** (the
review's recommended target; Ed25519 OFF the critical path):
1. **Record layer arc** (Gap A): N-block GCM (CTR + GHASH via recursion) + AAD
   + decrypt + constant-time tag compare + framing. Validate against
   `openssl enc -aes-128-gcm` / NIST GCM AEAD vectors (with AAD + multi-block).
2. **Key schedule**: HKDF-Expand-Label + Derive-Secret + transcript hash.
   Validate against RFC 8448's published secrets (byte-for-byte).
3. **Gap C I/O staging** (M1 scope, NOT a deep ABI change): a multi-step
   read/write handler on one connection + persistent per-connection byte buffers
   (resources); the two X25519 ladders are computed-once by construction.
4. **PSK handshake state machine** (ClientHello → ServerHello → EE → Finished →
   client Finished → app data), `psk_dhe_ke`, X25519 pinned (HRR-free).
   Validate LIVE vs `openssl s_client -psk`.

**Milestone 2 — browser HTTPS (adds the certificate path):**
5. **Ed25519 arc** (Gap B): SHA-512 → Edwards point ops → scalar-mult → mod-L →
   sign, each vs RFC 8032.
6. **Certificate + CertificateVerify + HelloRetryRequest + middlebox-compat**,
   anchored offline against RFC 8448 (everything except the Ed25519 signature,
   which 8448 signs with RSA — so the sig is anchored by RFC 8032 separately),
   then live against a real browser.

Steps 1, 2, 5 are independently validatable offline (openssl / RFC 8448 / RFC
8032) before any live socket. Milestone 1 reaches a real TLS 1.3 peer
signature-free and early; Milestone 2 is the long browser arc.

## 8. Honest scope assessment

This is **larger than the entire crypto arc so far**. AES-GCM+SHA+HKDF+X25519
was ~5 PRs of self-contained primitives with clean RFC anchors. TLS adds: a
second hash (SHA-512), a second curve (Edwards) + second modulus (mod L),
N-block AEAD, a stateful protocol, and — the real unknown — a
multi-byte/stateful data-flow model the current rule ABI doesn't have (Gap C).
RFC 8448 makes the handshake offline-testable, which de-risks correctness; the
infra (Gap C) is where the genuine language design work is.

**FIRST MILESTONE (review's #1 de-risking recommendation): a live PSK + (EC)DHE
handshake, no certificate, no signature.** Use `psk_dhe_ke` (RFC 8446 §2.2):
authentication is by a pre-shared key + the Finished MAC (have HMAC), so there
is NO Certificate and NO CertificateVerify — the entire Ed25519 arc (SHA-512 +
Edwards + mod-L, the largest/riskiest sub-arc) is OFF the critical path. This
still forces building everything else end-to-end: the record layer, the full key
schedule (HKDF-Expand-Label / Derive-Secret), the transcript hash, the Finished
MAC, the multi-message state machine, and Gap C's I/O staging. Validate LIVE
against `openssl s_client -psk <key> -psk_identity <id> -tls1_3` (and Python
`ssl`), which speaks `psk_dhe_ke` with X25519 pinned (HRR-free). This reaches a
real TLS 1.3 peer talking to the Verbose server EARLY and signature-free.
Browsers won't do external PSK, so this is a CLI milestone — but it proves the
whole stack minus the signature.

**SECOND MILESTONE: reproduce RFC 8448 offline** (correctness anchor for the
certificate path). RFC 8448
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
