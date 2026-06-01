"""Certificate-based TLS 1.3 server with HelloRetryRequest (HRR) + ClientHello
reassembly — the missing pieces real browsers force.

Everything cryptographic runs on Verbose-emitted binaries (vcrypto + ed25519);
the host does socket I/O, TLS framing, and server randomness (os.urandom) only.
Cipher suite TLS_AES_128_GCM_SHA256, X25519 ECDHE, server auth via Certificate +
CertificateVerify (Verbose Ed25519). NO PSK.

This is tls_cert_server.py extended in two ways (tls_cert_server.py is left
untouched as the known-good baseline):

  1. ClientHello reassembly — read + reassemble a handshake message that may
     span multiple TLS records (tlswire.read_handshake_message). Browsers
     fragment; openssl usually does not, but reassembly is implemented anyway.

  2. HelloRetryRequest — if the client did NOT send an X25519 key_share but
     lists X25519 (0x001d) in supported_groups, send an HRR selecting X25519,
     then read ClientHello2 (which now carries the X25519 key_share) and finish.

Transcript with HRR (RFC 8446 §4.4.1): CH1 is REPLACED by a synthetic
message_hash message, NOT seeded raw:

    message_hash = 0xfe || 0x00 0x00 0x20 || SHA-256(ClientHello1 hs-message bytes)

then running transcript = message_hash(CH1) || HRR || CH2 || SH || EE || Cert
|| CertVerify || Finished. Getting that wrong => bad record mac at the client.

Run recipe
----------
Server (SLOW — Verbose crypto is process-per-byte, allow minutes):

    python3 tools/tls_gen/tls_browser_server.py 14555

HRR path (openssl offers X448 key_share but lists X25519 too; we don't do X448
so we MUST retry, selecting X25519):

    printf 'GET / HTTP/1.0\r\n\r\n' | openssl s_client -connect 127.0.0.1:14555 \
      -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -groups X448:X25519 \
      -CAfile tools/tls_gen/cert.pem -servername localhost -ign_eof

No-HRR regression (openssl leads with X25519 key_share, no retry needed):

    printf 'GET / HTTP/1.0\r\n\r\n' | openssl s_client -connect 127.0.0.1:14555 \
      -tls1_3 -ciphersuites TLS_AES_128_GCM_SHA256 -groups X25519 \
      -CAfile tools/tls_gen/cert.pem -servername localhost -ign_eof

Success (both): decrypted app data contains "hello world" AND s_client reports
"Verify return code: 0 (ok)", no bad-record-mac / decryption / alert. For the
HRR run the server log must show CH1 had no x25519 key_share, "sending HRR",
and CH2 received with an x25519 key_share.
"""
import sys, os, socket, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vcrypto as V
import ed25519 as E
from tlswire import ClientHello, read_handshake_message
from tls_cert_messages import build_certificate, build_certificate_verify

HERE = os.path.dirname(os.path.abspath(__file__))
CERT_DER = os.path.join(HERE, "cert.der")
SEED = bytes(range(32))            # the cert's Ed25519 signing seed
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


def serve(conn):
    log = lambda *a: print(*a, flush=True)

    # 1. read + reassemble ClientHello1 (handles multi-record browser CHs).
    ch1_hs, leftover = read_handshake_message(conn)
    ch1 = ClientHello.from_handshake(ch1_hs)
    log(f"CH1: x25519_keyshare={'yes' if ch1.x25519_pub else 'no'} "
        f"key_share_groups={sorted(hex(g) for g in ch1.key_share_groups)} "
        f"supported_groups={[hex(g) for g in ch1.supported_groups]} "
        f"session_id_len={len(ch1.legacy_session_id)}")

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
    ee = hs_msg(0x08, b16(0))                      # EncryptedExtensions (empty)
    transcript += ee

    cert_msg = build_certificate(cert_der_bytes)   # Certificate (type 11)
    transcript += cert_msg

    th_cv = V.sha256(transcript)                   # H(.. EE .. Cert)
    cv_msg = build_certificate_verify(SEED, th_cv) # CertificateVerify (type 15)
    transcript += cv_msg
    log("Certificate + CertificateVerify built (CertVerify signed via Verbose Ed25519)")

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

    # 9. application data: master secret -> server app traffic secret, respond "hello world".
    derived2 = V.derive_derived(handshake)
    master = V.master_secret(derived2)
    th_full = V.sha256(transcript)                  # H(.. server Finished)
    s_ap = V.derive_s_ap(master, th_full)
    s_ak = V.expand_key(s_ap); s_aiv = V.expand_iv(s_ap)
    http = (b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n"
            b"Content-Type: text/plain\r\n\r\nhello world")
    conn.sendall(V.aead_encrypt(s_ak, s_aiv, 0, http, 0x17))
    log(f"sent application data (hello world) [HRR fired this handshake: {hrr_fired}]")
    time.sleep(2)   # let TCP deliver before close


cert_der_bytes = b""

def main():
    global cert_der_bytes
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 14555
    with open(CERT_DER, "rb") as f:
        cert_der_bytes = f.read()
    V.ensure(V.ALL_RULES + [("hkdf_extract", "hkdf_extract.verbose")])
    E.ensure()  # Ed25519 binaries for CertificateVerify signing
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(1)
    print(f"TLS-Verbose-browser listening on {port}", flush=True)
    while True:
        conn, _ = s.accept()
        try:
            serve(conn)
        except (ConnectionError, AssertionError) as e:
            print(f"connection error: {e}", flush=True)
        finally:
            conn.close()


if __name__ == "__main__":
    main()
