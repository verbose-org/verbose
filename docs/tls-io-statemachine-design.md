# Design: TLS 1.3 connection I/O state machine (Gap C)

Status: **design, post-review** (2026-05-30). Adversarially reviewed; the review
found two blockers and an integrity leak, folded into §7 below — the
streaming-emit unlock is now milestone 0 and a hard prerequisite. This is the gating arc between the validated TLS 1.3
crypto data-plane (X25519, key schedule, record AEAD — all RFC/NIST-validated in
pure Verbose) and a live HTTPS server. Read against
[[project_verbose_compiler_no_guessing]]: no heuristics in the compiler; every
construct stays mechanically verifiable.

## 1. The problem

The crypto is done and validated, but it is a set of **pure byte→byte rules**:
each returns one output byte per invocation (`which` pattern), and recursion
cannot return a buffer. A TLS server is the opposite shape: a **stateful,
multi-message, encrypted exchange** on one socket — read ClientHello, write
ServerHello + CCS + encrypted {EE, Cert, CertVerify, Finished}, read client
Finished, then exchange application records.

The existing native `service` emitter is **one-shot**: accept → parse one HTTP
request → run handler → write one response → close (or loop to next accept). It
has no way to (a) do several reads/writes on the same connection with evolving
state, or (b) hold per-connection state (the handshake transcript, the derived
keys, the sequence numbers) across those steps.

That gap — multi-step stateful I/O on one connection — is the ONLY thing between
the validated crypto and a live server. It is native-backend infrastructure,
not another `.verbose` crypto brick.

## 2. Two candidate strategies

### Strategy A — driver-in-Verbose (the in-language ideal)

Express the whole handshake as Verbose rules and have the service emitter run
them in sequence, persisting intermediate bytes between steps. Requires either:
- a new "rule returns a byte buffer" capability (deep recursion-ABI change), or
- staging every stage's output through a per-connection buffer/resource that the
  next stage reads (the option the roadmap §6 favors).

Pro: maximal in-language purity. Con: large native-backend change; the
recompute-per-byte cost of the `which` pattern multiplies across a multi-stage
pipeline; the buffer-staging plumbing is intricate. This is the long-term
target, not the first milestone.

### Strategy B — orchestrator harness drives validated Verbose rules (RECOMMENDED FIRST)

A thin **host orchestrator** (the same shape as our validators
`tools/tls_gen/*.py`, which already drive the Verbose binaries to do real TLS
crypto) owns the socket and the state machine, and calls the **Verbose-compiled
binaries** for every cryptographic operation. The crypto — key derivation,
record encrypt/decrypt, X25519 — is 100% Verbose-emitted machine code; the
socket bookkeeping and message framing are host glue.

This is exactly how `tls_record_check.py` and `keysched_check.py` already work:
they ARE partial TLS data-planes driven against the Verbose binaries. Extending
one of them into a full server harness gives a **live, browser/`s_client`-
reachable TLS 1.3 endpoint whose entire cryptographic core is pure Verbose** —
the demonstrable claim — without first solving the deep recursion-ABI change.

Honest framing of B: this is NOT "the TLS server is written in Verbose" in the
strongest sense (the state machine is host code). It IS "every byte of TLS
crypto on the wire is computed by Verbose-emitted machine code." That is a real,
defensible milestone and the right stepping stone; Strategy A (driver-in-Verbose)
remains the north star and is pursued after B proves the protocol logic.

The two are complementary: B nails down the exact byte-level protocol behavior
(message order, framing, transcript cut points, nonce sequencing) against a live
peer; A then re-homes that proven logic into Verbose rules + the new emitter,
with B as the oracle.

## 3. Strategy B milestone ladder (each independently testable)

1. **PSK-DHE handshake, offline vector.** Implement the host state machine for
   `psk_dhe_ke` (no certificate, no signature — Ed25519 stays off the critical
   path). Drive all crypto through the Verbose binaries. Validate the produced
   ServerHello..Finished bytes against a Python `tls` reference for a fixed
   (psk, ecdhe, randoms) tuple. No socket yet.
2. **PSK-DHE handshake, LIVE.** Put the state machine on a real TCP socket;
   complete a handshake with `openssl s_client -psk <key> -psk_identity <id>
   -tls1_3`. Exchange one application record ("hello world"). This is the first
   live TLS 1.3 connection whose crypto is pure Verbose.
3. **Certificate path (browser milestone).** Adds the Ed25519 arc (SHA-512 +
   Edwards + mod-L), Certificate/CertificateVerify, HelloRetryRequest (browsers
   lead with X25519MLKEM768 → HRR mandatory), and middlebox-compat. Then a real
   browser reaches `https://host/`.

Milestone 2 is the smallest "live TLS 1.3 server, crypto in Verbose" deliverable.
Milestone 3 is the full browser goal.

## 4. What stays pure Verbose in Strategy B (the integrity line)

Every cryptographic transformation on wire bytes is a Verbose-emitted binary:
- X25519 (server ephemeral pub = ladder(priv, 9); shared = ladder(priv, client_pub))
- handshake_secret / derive_secret / hkdf_expand_label (the full key schedule)
- sha256_fold (transcript hash, recomputed at each Derive-Secret cut point)
- aes_gctr + ghash_nblocks + aes_encrypt (record AEAD, both directions)

The host does ONLY: socket read/write, TLS message framing/parsing (lengths,
extension TLVs), sequencing, and gluing stage outputs to stage inputs. No
cryptography in the host. A reviewer can grep the harness and confirm every
keystream/tag/secret byte originates from a `verbosec`-compiled binary.

## 5. The path back to Strategy A (north star)

Once B pins the protocol byte-for-byte, the in-Verbose driver needs exactly one
language-infra slice: **per-connection persistent byte buffers + a multi-step
handler** in the service emitter (roadmap §6). With that, the host glue collapses
into Verbose rules reading/writing those buffers, and the state machine itself
becomes Verbose. This slice is designed on its own merits then (it also serves
self-hosting — buffers/mutable state are on that roadmap too,
[[project_verbose_ui_toolkit_vision]]), not rushed under the TLS deadline.

## 6. Risks / honesty

- **Strategy B's claim must be stated precisely** ("TLS crypto in pure Verbose,
  driven by a host state machine"), never overstated as "TLS server in Verbose".
  Overstating it would be the same integrity failure as a false test pass.
- **Performance**: each X25519 ladder is ~5s; a handshake does 2 → ~10s per
  connection, plus per-byte recompute for record crypto. Fine for a demo, not
  for load. Say so.
- **Constant-time**: not claimed; the host glue and the `which`-recompute are not
  constant-time. Demo-grade only.
- **Milestone 3 (browser) is a large arc** (Ed25519 ~ the size of the X25519 arc
  + HRR + cert encoding). B-milestone-2 (s_client, live) is the realistic near
  target.

## 7. Adversarial-review findings (folded in — supersede §3–§6 where they conflict)

**BLOCKER-1 — the one-byte-per-spawn ABI makes a live handshake un-completable.**
Each crypto binary returns ONE output byte per process run, re-executing the
whole unrolled rule. One TLS record AEAD ≈ 112 spawns of multi-hundred-KB
binaries; a full handshake is many minutes — and `openssl s_client`/browsers
enforce handshake timeouts, so the connection is torn down before server
Finished. **Milestone 0 (NEW, hard prerequisite): a "stream all N output bytes
in one process run to stdout" mode** for the crypto rules (a read-only precursor
to Strategy A's buffer-return). This is the keystone: it (1) makes the live demo
physically complete within timeouts, (2) makes repeated transcript hashing
affordable, (3) lets nonce-XOR / AAD / J0 / tag-XOR / constant-time-compare move
INTO Verbose (closing the §4 integrity leak). Build this FIRST.

**BLOCKER-2 — PSK-DHE requires binder verification, missing from §3/§4 scope.**
To accept a PSK ClientHello the server must verify the PSK binder: an HMAC over
the transcript hash of the truncated ClientHello (up to and including
`PreSharedKeyExtension.identities`, excluding the binders list, with length
fields set as if binders were present), keyed by `binder_key` from the
**PSK early-secret branch: Early Secret = HKDF-Extract(0, PSK)** — NOT the
`HKDF-Extract(0, 0)` the current key-schedule binaries validate (MAJOR-3). The
HMAC is Verbose; the truncated-transcript boundary is crypto-relevant framing.
Add binder verify + the PSK early-secret instances (re-validate vs a Python/RFC
reference) to milestone 1.

**MAJOR-1 — the §4 "no cryptography in the host" claim is false today and must
be narrowed or closed.** In the existing oracle (`tls_record_check.py`) the host
does: nonce = IV XOR seq, AAD/length-block construction, J0 = nonce‖00000001,
tag = S XOR E(J0), partial-block zero-padding, AND the constant-time compare, AND
generates the server ephemeral private key + server random (host RNG = a secret
input). Honest current claim: **"the AEAD/KDF/X25519 *primitives* are Verbose;
AEAD framing, comparisons, and all secret randomness are host glue."** Close most
of the leak via Milestone 0 (a Verbose `aes_gcm` rule taking (key, iv, seq, pt,
aad) and emitting the full record incl. tag, plus a branch-free compare rule
returning 0/1). The one irreducible host crypto-input is **randomness**: pure
Verbose needs a `getrandom` effect to generate the server scalar/random — name
it explicitly as the single host-side secret source until that effect exists.

**MAJOR-2 — PSK-first is partly throwaway; state the trade.** The user's real
goal is a browser, which won't external-PSK. PSK-DHE milestone defers Ed25519
(good) but builds ~30% disposable protocol surface (binder, psk_key_exchange_
modes / pre_shared_key parse) that does NOT carry to the browser. Cert-first
against `s_client` (pin `-curves x25519` → HRR-free) needs Ed25519 up front but
is a straight line to the browser. Milestone 1 is therefore a **de-risking
detour** — adopt it to validate record/key-schedule/state-machine live without
the big Ed25519 arc, eyes open that part of it is disposable.
Exact demo flags (the SHA-256 PSK requires forcing the suite):
`openssl s_client -psk <hex> -psk_identity <id> -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -curves x25519`.

**MINOR — transcript-hash cut points are host logic but DEFINE crypto inputs**
(handshake-message bytes only — no record headers, no inner content-type, no
CCS); getting boundaries wrong is silent. Milestone 0's streaming SHA makes the
four cut-point rehashes affordable. **Browser HRR also splits the ClientHello
across records** (X25519MLKEM768 key_share ≈ 1216 B), so the host parser must
reassemble a multi-record ClientHello — an I/O-state-machine task for milestone 3.

**Revised ordering:** Milestone 0 (streaming emit + fold framing into Verbose) →
Milestone 1 (PSK-DHE offline incl. binder, vs reference) → Milestone 2 (PSK-DHE
live vs `s_client`) → Milestone 3 (Ed25519 + cert + HRR + browser). Strategy A's
"exactly one slice" (§5) is really three: streaming/buffer emit, per-connection
buffers + multi-step handler, and a `getrandom` effect.
