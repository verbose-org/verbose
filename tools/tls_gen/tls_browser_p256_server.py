"""ECDSA-P256 variant of tls_browser_server.py — a TLS 1.3 server hardened for
real browsers that presents a P-256 leaf certificate and signs CertificateVerify
with ECDSA-P256-SHA256 (SignatureScheme ecdsa_secp256r1_sha256 = 0x0403).

This is the brick that lets Chrome accept our handshake: Chrome offers
ecdsa_secp256r1_sha256 and NEVER ed25519 (0x0807), so the Ed25519 server
(tls_browser_server.py) is rejected at signature-algorithm selection. This server
is byte-for-byte the same handshake EXCEPT:

  * presents cert_p256.der (self-signed P-256 leaf, built by make_cert_p256.py),
  * signs CertificateVerify with ecdsa_p256.sign over the standard TLS 1.3 server
    CertificateVerify signed-content, SignatureScheme 0x0403, signature field =
    the ECDSA DER signature (2-byte length prefix). DER, not raw r||s — TLS 1.3
    uses the ANSI X9.62 / SEC1 DER form for ecdsa_* schemes.

Everything cryptographic runs on Verbose-emitted code:
  * X25519 ECDHE, AES-128-GCM, SHA-256, the key schedule  -> Verbose binaries
    (vcrypto), as in the Ed25519 server.
  * the CertificateVerify ECDSA signature -> ecdsa_p256.sign: SHA-256 on the pure
    Verbose binary, k*G + d*G + the scalar field on the Verbose-mirror limb code.
The host does socket I/O, TLS framing, and server randomness (os.urandom) only.

Cipher suite TLS_AES_128_GCM_SHA256, X25519 ECDHE, server auth via Certificate +
CertificateVerify (Verbose ECDSA-P256). NO PSK.

Kept verbatim from the Ed25519 server: HelloRetryRequest (HRR), multi-record
ClientHello reassembly, ALPN (http/1.1), fork-per-connection accept loop, and a
real HTTP/1.1 HTML response. The transcript handling (synthetic message_hash on
HRR) is identical.

Run recipe
----------
Server (SLOW — Verbose crypto is process-per-byte, allow minutes per handshake):

    python3 tools/tls_gen/tls_browser_p256_server.py 47123

Direct path (openssl leads with an X25519 key_share, no retry; forces the ECDSA
cert/scheme by trusting only cert_p256.pem):

    printf 'GET / HTTP/1.0\r\n\r\n' | openssl s_client -connect 127.0.0.1:47123 \
      -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -groups X25519 \
      -CAfile tools/tls_gen/cert_p256.pem -servername localhost -ign_eof

HRR path (openssl offers X448 key_share but lists X25519 too; we don't do X448
so we MUST retry, selecting X25519):

    printf 'GET / HTTP/1.0\r\n\r\n' | openssl s_client -connect 127.0.0.1:47123 \
      -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -groups X448:X25519 \
      -CAfile tools/tls_gen/cert_p256.pem -servername localhost -ign_eof

Success (both): "Verify return code: 0 (ok)", "Peer signature type: ECDSA"
(proving CertVerify was ECDSA), and the decrypted body "Hello from Verbose TLS".
"""
import sys, os, socket, time, signal
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vcrypto as V
import ecdsa_p256 as EC
from tlswire import ClientHello, read_handshake_message
from tls_cert_messages import build_certificate, build_certificate_verify_ecdsa_p256

HERE = os.path.dirname(os.path.abspath(__file__))
CERT_DER = os.path.join(HERE, "cert_p256.der")
# The P-256 signing key — MUST match make_cert_p256.py's PRIV_D (the cert is
# self-signed with it, and CertificateVerify signs with the same key so the
# CertVerify signature validates under the cert's public key).
PRIV_D = int.from_bytes(bytes(range(1, 33)), "big") % EC.N
BASEPOINT = bytes([9] + [0] * 31)

X25519_GROUP = 0x001d

# RFC 8446 §4.1.3: the special ServerHello.random that marks a HelloRetryRequest.
HRR_RANDOM = bytes.fromhex(
    "cf21ad74e59a6111be1d8c021e65b891c2a211167abb8c5e079e09e2c8a8339c")


def b16(x): return x.to_bytes(2, 'big')
def b24(x): return x.to_bytes(3, 'big')
def hs_msg(t, body): return bytes([t]) + b24(len(body)) + body
def record(ct, payload): return bytes([ct]) + b"\x03\x03" + b16(len(payload)) + payload

def hkdf_extract(salt, ikm):
    return V.run_bytes("hkdf_extract", [str(b) for b in salt] + [str(b) for b in ikm], 32)


def build_hello_retry_request(legacy_session_id, selected_group):
    """HRR = ServerHello (type 0x02) with the HRR magic random and a key_share
    extension carrying ONLY the selected group (no key exchange data).
    RFC 8446 §4.1.4 / §4.2.8."""
    sv_ext = b16(0x002b) + b16(2) + b16(0x0304)              # supported_versions
    ks_ext = b16(0x0033) + b16(2) + b16(selected_group)       # key_share: group only
    exts = sv_ext + ks_ext
    body = (b"\x03\x03" + HRR_RANDOM
            + bytes([len(legacy_session_id)]) + legacy_session_id
            + b"\x13\x01" + b"\x00"
            + b16(len(exts)) + exts)
    return hs_msg(0x02, body)


def build_server_hello(legacy_session_id, server_random, server_pub):
    """Real ServerHello with an X25519 KeyShareEntry (key exchange data)."""
    ks_ext = b16(0x0033) + b16(2 + 2 + 32) + b16(X25519_GROUP) + b16(32) + server_pub
    sv_ext = b16(0x002b) + b16(2) + b16(0x0304)
    exts = sv_ext + ks_ext
    body = (b"\x03\x03" + server_random
            + bytes([len(legacy_session_id)]) + legacy_session_id
            + b"\x13\x01" + b"\x00"
            + b16(len(exts)) + exts)
    return hs_msg(0x02, body)


def build_encrypted_extensions(alpn_selected):
    """EncryptedExtensions (type 8). Empty unless we selected an ALPN protocol,
    in which case echo it back (RFC 7301): ext 0x0010, body =
    ProtocolNameList = u16(list_len) || u8(name_len) || name."""
    exts = b""
    if alpn_selected is not None:
        name = alpn_selected
        pnl = bytes([len(name)]) + name           # one ProtocolName entry
        body = b16(len(pnl)) + pnl                 # ProtocolNameList
        exts += b16(0x0010) + b16(len(body)) + body
    return hs_msg(0x08, b16(len(exts)) + exts)


# The HTML page a real browser renders. Body is fixed; Content-Length is exact.
HTML_BODY = (b"<!doctype html><meta charset=utf-8>"
             b"<h1>Hello from Verbose TLS</h1>"
             b"<p>This page was served over TLS 1.3 whose entire cryptography "
             b"(X25519, AES-128-GCM, SHA-256, ECDSA-P256) runs on Verbose-emitted "
             b"machine code.</p>")

def http_response():
    return (b"HTTP/1.1 200 OK\r\n"
            b"Content-Type: text/html\r\n"
            b"Content-Length: " + str(len(HTML_BODY)).encode() + b"\r\n"
            b"Connection: close\r\n"
            b"\r\n" + HTML_BODY)


def serve(conn):
    log = lambda *a: print(*a, flush=True)

    # 1. read + reassemble ClientHello1 (handles multi-record browser CHs).
    ch1_hs, leftover = read_handshake_message(conn)
    ch1 = ClientHello.from_handshake(ch1_hs)
    alpn_offered = getattr(ch1, "alpn_protocols", [])
    # We only speak HTTP/1.1. If the client offered ALPN at all, select
    # http/1.1 (present it whether or not h2 was also offered). If the client
    # did not send ALPN, select nothing and omit the EE extension.
    alpn_selected = b"http/1.1" if alpn_offered else None
    log(f"CH1: x25519_keyshare={'yes' if ch1.x25519_pub else 'no'} "
        f"key_share_groups={sorted(hex(g) for g in ch1.key_share_groups)} "
        f"supported_groups={[hex(g) for g in ch1.supported_groups]} "
        f"alpn_offered={[p.decode('latin1') for p in alpn_offered]} "
        f"alpn_selected={alpn_selected.decode() if alpn_selected else None} "
        f"session_id_len={len(ch1.legacy_session_id)} "
        f"sig_algs={[hex(s) for s in getattr(ch1,'sig_algs',[])]} "
        f"offers_ecdsa_p256={0x0403 in getattr(ch1,'sig_algs',[])}")

    # Bytes the handshake transcript will be seeded with, and the CH we finish on.
    ch_for_keyexch = ch1
    hrr_fired = False
    transcript = b""

    if ch1.x25519_pub is not None:
        # 2a. No HRR needed — client already led with an X25519 key_share.
        log("CH1 has X25519 key_share -> NO HRR (direct path)")
        transcript = ch1_hs
    elif X25519_GROUP in ch1.supported_groups:
        # 2b. HRR path: client supports X25519 but didn't share one. Retry.
        hrr_fired = True
        log("CH1 has NO x25519 key_share but lists X25519 in supported_groups "
            "-> sending HRR (selected_group=x25519 0x001d)")
        hrr_hs = build_hello_retry_request(ch1.legacy_session_id, X25519_GROUP)
        # sanity: prove the HRR magic random is in the bytes we send.
        log(f"HRR random in message: {hrr_hs[6:6+32].hex()}")
        assert hrr_hs[6:6+32] == HRR_RANDOM, "HRR magic random not where expected"

        conn.sendall(record(0x16, hrr_hs))     # HRR as PLAINTEXT handshake record
        conn.sendall(record(0x14, b"\x01"))    # dummy CCS (middlebox compat)
        log("sent HelloRetryRequest + dummy CCS")

        # RFC 8446 §4.4.1: seed transcript with synthetic message_hash(CH1), NOT raw CH1.
        ch1_hash = V.sha256(ch1_hs)
        message_hash = bytes([0xfe]) + b24(len(ch1_hash)) + ch1_hash
        log(f"transcript seed = message_hash(CH1) = {message_hash[:4].hex()}.. "
            f"(0xfe 00 00 20 || SHA256(CH1)); CH1 sha256={ch1_hash.hex()[:16]}..")
        transcript = message_hash + hrr_hs

        # 4. read + reassemble ClientHello2 (now carries an X25519 key_share).
        ch2_hs, leftover = read_handshake_message(conn, leftover)
        ch2 = ClientHello.from_handshake(ch2_hs)
        log(f"CH2: x25519_keyshare={'yes' if ch2.x25519_pub else 'no'} "
            f"key_share_groups={sorted(hex(g) for g in ch2.key_share_groups)}")
        if ch2.x25519_pub is None:
            log("CH2 still has no X25519 key_share -> abort"); return
        transcript += ch2_hs
        ch_for_keyexch = ch2
        # ALPN selection is driven by the final ClientHello (RFC 8446 §4.1.2:
        # CH2 mirrors CH1's extensions; re-read so we never echo a protocol the
        # client did not actually offer in the message we finish on).
        alpn_offered = getattr(ch2, "alpn_protocols", [])
        alpn_selected = b"http/1.1" if alpn_offered else None
        log(f"CH2 alpn_offered={[p.decode('latin1') for p in alpn_offered]} "
            f"alpn_selected={alpn_selected.decode() if alpn_selected else None}")
    else:
        log("CH1 has no X25519 key_share and does not list X25519 -> "
            "cannot negotiate, aborting"); return

    # 2. server ephemeral X25519 (host RNG = the one acknowledged host secret).
    sk = bytearray(os.urandom(32)); sk[0] &= 248; sk[31] &= 127; sk[31] |= 64
    server_pub = V.x25519(bytes(sk), BASEPOINT)
    ecdhe = V.x25519(bytes(sk), ch_for_keyexch.x25519_pub)
    server_random = os.urandom(32)
    log("server X25519 ephemeral + ECDHE computed")

    # 3. ServerHello (real, with X25519 KeyShareEntry). Echo CH1's session id.
    sh_hs = build_server_hello(ch1.legacy_session_id, server_random, server_pub)
    transcript += sh_hs

    # 5. key schedule (cert mode, no PSK), keyed off the HRR-aware transcript.
    handshake = V.handshake_secret(ecdhe)
    th_chsh = V.sha256(transcript)
    s_hs = V.derive_s_hs(handshake, th_chsh)
    c_hs = V.derive_c_hs(handshake, th_chsh)
    s_key = V.expand_key(s_hs); s_iv = V.expand_iv(s_hs)
    c_key = V.expand_key(c_hs); c_iv = V.expand_iv(c_hs)
    log("handshake key schedule derived (s_hs / c_hs / keys / ivs)")

    # 6. send ServerHello (plaintext) + dummy CCS.
    conn.sendall(record(0x16, sh_hs))
    conn.sendall(record(0x14, b"\x01"))

    # 7. encrypted handshake flight under server handshake key (seq 0).
    ee = build_encrypted_extensions(alpn_selected) # EncryptedExtensions (ALPN echo if offered)
    transcript += ee

    cert_msg = build_certificate(cert_der_bytes)   # Certificate (type 11), P-256 leaf
    transcript += cert_msg

    th_cv = V.sha256(transcript)                   # H(.. EE .. Cert)
    cv_msg = build_certificate_verify_ecdsa_p256(PRIV_D, th_cv)  # CertVerify (type 15), 0x0403
    transcript += cv_msg
    log("Certificate + CertificateVerify built (CertVerify signed via Verbose ECDSA-P256, scheme 0x0403)")

    th_fin = V.sha256(transcript)
    sfk = V.finished_key(s_hs)
    verify_data = hkdf_extract(sfk, th_fin)        # HMAC(finished_key, transcript_hash)
    fin = hs_msg(0x14, verify_data)                # server Finished (type 20)
    transcript += fin

    flight = ee + cert_msg + cv_msg + fin
    conn.sendall(V.aead_encrypt(s_key, s_iv, 0, flight, 0x16))
    log("sent SH + CCS + {EE, Certificate, CertificateVerify, Finished} (1 encrypted record)")

    # 8. read client CCS (skip) + encrypted client Finished; decrypt under c_hs (seq 0).
    def read_record(buf):
        while len(buf) < 5 or len(buf) < 5 + int.from_bytes(buf[3:5], "big"):
            chunk = conn.recv(4096)
            if not chunk: return None, buf
            buf += chunk
        rlen = 5 + int.from_bytes(buf[3:5], "big")
        return buf[:rlen], buf[rlen:]
    buf = leftover
    rec, buf = read_record(buf)
    if rec and rec[0] == 0x14:                      # skip client ChangeCipherSpec
        rec, buf = read_record(buf)
    if rec:
        dec = V.aead_decrypt(c_key, c_iv, 0, rec)
        if dec is None:
            log("client Finished: AEAD decrypt/auth FAILED")
        else:
            inner_ct, body = dec
            ok = "ok" if (inner_ct == 0x16 and len(body) >= 4 and body[0] == 0x14) else "unexpected"
            log(f"client Finished decrypted (inner_ct=0x{inner_ct:02x}, hs_type=0x{body[0]:02x}) -> {ok}")
            if inner_ct == 0x15 and len(body) >= 2:     # TLS alert: [level, description]
                _ALERTS = {0:"close_notify",10:"unexpected_message",20:"bad_record_mac",
                           40:"handshake_failure",42:"bad_certificate",43:"unsupported_certificate",
                           44:"certificate_revoked",45:"certificate_expired",46:"certificate_unknown",
                           47:"illegal_parameter",48:"unknown_ca",49:"access_denied",
                           50:"decode_error",51:"decrypt_error",70:"protocol_version",
                           80:"internal_error",109:"missing_extension",110:"unsupported_extension",
                           112:"unrecognized_name",116:"certificate_required",120:"no_application_protocol"}
                lvl = "fatal" if body[0]==2 else ("warning" if body[0]==1 else f"lvl{body[0]}")
                desc = _ALERTS.get(body[1], f"alert_{body[1]}")
                log(f"  >>> client sent TLS ALERT: {lvl} {desc} (raw {body[0]},{body[1]})")

    # 9. application data: master secret -> traffic secrets (server AND client).
    derived2 = V.derive_derived(handshake)
    master = V.master_secret(derived2)
    th_full = V.sha256(transcript)                  # H(.. server Finished)
    s_ap = V.derive_s_ap(master, th_full)
    s_ak = V.expand_key(s_ap); s_aiv = V.expand_iv(s_ap)
    c_ap = V.derive_c_ap(master, th_full)
    c_ak = V.expand_key(c_ap); c_aiv = V.expand_iv(c_ap)

    # 9a. drain the client's HTTP request (one app-data record, c_ap seq 0).
    req_summary = "none"
    try:
        conn.settimeout(10.0)
        rec, buf = read_record(buf)
        while rec and rec[0] == 0x14:               # stray ChangeCipherSpec
            rec, buf = read_record(buf)
        if rec and rec[0] == 0x17:
            dec = V.aead_decrypt(c_ak, c_aiv, 0, rec)
            if dec is not None:
                _ict, reqbody = dec
                first_line = reqbody.split(b"\r\n", 1)[0]
                req_summary = first_line.decode("latin1", "replace")[:80]
        elif rec is not None:
            req_summary = f"non-appdata record 0x{rec[0]:02x}"
    except (socket.timeout, OSError) as e:
        req_summary = f"no request read ({e})"
    finally:
        try: conn.settimeout(None)
        except OSError: pass
    log(f"client request line: {req_summary!r}")

    # 9b. serve the HTML page (server app traffic key, seq 0).
    conn.sendall(V.aead_encrypt(s_ak, s_aiv, 0, http_response(), 0x17))
    log(f"sent HTTP/1.1 HTML response ({len(HTML_BODY)} body bytes) "
        f"[HRR fired this handshake: {hrr_fired}, ALPN selected: "
        f"{alpn_selected.decode() if alpn_selected else None}]")
    time.sleep(1)   # let TCP deliver before close


cert_der_bytes = b""

def main():
    global cert_der_bytes
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 47123
    with open(CERT_DER, "rb") as f:
        cert_der_bytes = f.read()
    V.ensure(V.ALL_RULES + [("hkdf_extract", "hkdf_extract.verbose")])
    # ecdsa_p256.sign needs the pure-Verbose SHA-256 binary (z = SHA256(content)).
    # k*G / d*G / scalar field run via the validated Verbose-mirror limb code.
    V.ensure([("sha256_fold", "sha256_fold.verbose")])

    # Reap children automatically so per-connection forks never become zombies.
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)

    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(64)   # backlog for parallel browser conns
    # "listening" is the readiness token the validation harness polls for.
    print(f"TLS-Verbose-P256-browser listening on {port}", flush=True)

    while True:
        try:
            conn, addr = s.accept()
        except InterruptedError:
            continue
        except OSError as e:
            print(f"accept error: {e}", flush=True)
            continue

        try:
            pid = os.fork()
        except OSError as e:
            print(f"fork failed: {e}; serving inline (degraded)", flush=True)
            try:
                serve(conn)
            except Exception as ex:
                print(f"inline connection error: {ex}", flush=True)
            finally:
                conn.close()
            continue

        if pid == 0:
            # --- child ---
            s.close()                       # child does not accept
            rc = 0
            try:
                serve(conn)
            except (ConnectionError, AssertionError, socket.timeout, OSError) as e:
                print(f"connection error (child {os.getpid()}): {e}", flush=True)
                rc = 1
            except Exception as e:
                print(f"unexpected child error: {e!r}", flush=True)
                rc = 1
            finally:
                try: conn.close()
                except OSError: pass
            os._exit(rc)
        else:
            # --- parent ---
            conn.close()


if __name__ == "__main__":
    main()
