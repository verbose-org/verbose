"""A certificate-based TLS 1.3 server whose ENTIRE cryptography runs on
Verbose-emitted binaries (via vcrypto + ed25519). The host does socket I/O,
TLS message framing, and server randomness (os.urandom) only — the acknowledged
host-glue category (docs/tls-io-statemachine-design.md §7).

Cipher suite TLS_AES_128_GCM_SHA256, X25519 key exchange, server authentication
via Certificate (Ed25519 self-signed cert) + CertificateVerify. NO PSK.

The CertificateVerify signature is produced by the pure-Verbose Ed25519 path
(ed25519.py); the X25519 ECDHE, the SHA-256 transcript hashes, the HKDF key
schedule, and the AES-128-GCM record protection all run on Verbose binaries.

Scope: cert-auth path proved end-to-end against openssl forced to X25519
(so NO HelloRetryRequest). HRR + ClientHello reassembly for real browsers is a
later step.

Run recipe
----------
Terminal 1 (server; SLOW — Verbose crypto is process-per-byte, allow minutes):

    python3 tools/tls_gen/tls_cert_server.py 14444

Terminal 2 (client; openssl forced to X25519 so no HRR, our cert as trust root):

    openssl s_client -connect 127.0.0.1:14444 -tls1_3 \
      -ciphersuites TLS_AES_128_GCM_SHA256 -groups X25519 \
      -CAfile tools/tls_gen/cert.pem -servername localhost -ign_eof

Success = decrypted app data contains "hello world" AND s_client reports
"Verify return code: 0 (ok)" (our CertificateVerify signature + cert chain
validated), with no bad-record-mac / decryption / handshake-failure alerts.
"""
import sys, os, socket, time, hmac, hashlib
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import vcrypto as V
import ed25519 as E
from tlswire import ClientHello
from tls_cert_messages import build_certificate, build_certificate_verify, \
    certverify_signed_content

HERE = os.path.dirname(os.path.abspath(__file__))
CERT_DER = os.path.join(HERE, "cert.der")
SEED = bytes(range(32))            # the cert's Ed25519 signing seed
BASEPOINT = bytes([9] + [0] * 31)

def b16(x): return x.to_bytes(2, 'big')
def b24(x): return x.to_bytes(3, 'big')
def hs_msg(t, body): return bytes([t]) + b24(len(body)) + body
def record(ct, payload): return bytes([ct]) + b"\x03\x03" + b16(len(payload)) + payload

def hkdf_extract(salt, ikm):
    return V.run_bytes("hkdf_extract", [str(b) for b in salt] + [str(b) for b in ikm], 32)


def serve(conn):
    log = lambda *a: print(*a, flush=True)

    # 1. read ClientHello record (one record is enough for s_client)
    data = b""
    while len(data) < 5 or len(data) < 5 + int.from_bytes(data[3:5], 'big'):
        chunk = conn.recv(4096)
        if not chunk:
            log("connection closed before full ClientHello"); return
        data += chunk
    ch = ClientHello(data)
    ch_hs = ch.handshake
    if ch.x25519_pub is None:
        log("no X25519 key_share in ClientHello"); return
    log(f"ClientHello: x25519_pub={ch.x25519_pub.hex()[:16]}... "
        f"session_id_len={len(ch.legacy_session_id)} (PSK binder present: {ch.psk_binder is not None})")

    # 2. server ephemeral X25519 (host RNG = the one acknowledged host secret input)
    sk = bytearray(os.urandom(32)); sk[0] &= 248; sk[31] &= 127; sk[31] |= 64
    server_pub = V.x25519(bytes(sk), BASEPOINT)
    ecdhe = V.x25519(bytes(sk), ch.x25519_pub)
    server_random = os.urandom(32)
    log("server X25519 ephemeral + ECDHE computed")

    # 3. build ServerHello — X25519 key_share + supported_versions only (NO psk ext)
    ks_ext = b16(0x0033) + b16(2 + 2 + 32) + b16(0x001d) + b16(32) + server_pub
    sv_ext = b16(0x002b) + b16(2) + b16(0x0304)
    exts = sv_ext + ks_ext
    sh_body = b"\x03\x03" + server_random + bytes([len(ch.legacy_session_id)]) + ch.legacy_session_id \
        + b"\x13\x01" + b"\x00" + b16(len(exts)) + exts
    sh_hs = hs_msg(0x02, sh_body)

    # 4. transcript = CH || SH (handshake-message bytes only)
    transcript = ch_hs + sh_hs

    # 5. key schedule (cert mode, no PSK):
    #    handshake_secret() folds early(0,0) -> derived -> Extract(salt=derived, ikm=ecdhe).
    handshake = V.handshake_secret(ecdhe)
    th_chsh = V.sha256(transcript)
    s_hs = V.derive_s_hs(handshake, th_chsh)
    c_hs = V.derive_c_hs(handshake, th_chsh)
    s_key = V.expand_key(s_hs); s_iv = V.expand_iv(s_hs)
    c_key = V.expand_key(c_hs); c_iv = V.expand_iv(c_hs)
    log("handshake key schedule derived (s_hs / c_hs / keys / ivs)")

    # 6. send ServerHello (plaintext) + dummy CCS
    conn.sendall(record(0x16, sh_hs))
    conn.sendall(record(0x14, b"\x01"))

    # 7. encrypted handshake flight under server handshake key (seq 0).
    #    Advance the transcript message-by-message: CertificateVerify signs the
    #    transcript THROUGH Certificate; Finished MACs THROUGH CertificateVerify.
    ee = hs_msg(0x08, b16(0))                      # EncryptedExtensions (empty)
    transcript += ee

    cert_msg = build_certificate(cert_der_bytes)   # Certificate (type 11)
    transcript += cert_msg

    th_cv = V.sha256(transcript)                   # H(CH..SH..EE..Cert)
    cv_msg = build_certificate_verify(SEED, th_cv) # CertificateVerify (type 15), Verbose Ed25519
    transcript += cv_msg
    log("Certificate + CertificateVerify built (CertVerify signed via Verbose Ed25519)")

    th_fin = V.sha256(transcript)                  # H(CH..SH..EE..Cert..CertVerify)
    sfk = V.finished_key(s_hs)
    verify_data = hkdf_extract(sfk, th_fin)        # HMAC(finished_key, transcript_hash)
    fin = hs_msg(0x14, verify_data)                # server Finished (type 20)
    transcript += fin

    flight = ee + cert_msg + cv_msg + fin
    conn.sendall(V.aead_encrypt(s_key, s_iv, 0, flight, 0x16))
    log("sent SH + CCS + {EE, Certificate, CertificateVerify, Finished} (1 encrypted record)")

    # 8. read client CCS (skip) + encrypted client Finished; decrypt under c_hs (seq 0)
    def read_record(buf):
        while len(buf) < 5 or len(buf) < 5 + int.from_bytes(buf[3:5], "big"):
            chunk = conn.recv(4096)
            if not chunk: return None, buf
            buf += chunk
        rlen = 5 + int.from_bytes(buf[3:5], "big")
        return buf[:rlen], buf[rlen:]
    buf = b""
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

    # 9. application data: master secret -> server app traffic secret, respond "hello world"
    derived2 = V.derive_derived(handshake)
    master = V.master_secret(derived2)
    th_full = V.sha256(transcript)                  # H(CH..server Finished)
    s_ap = V.derive_s_ap(master, th_full)
    s_ak = V.expand_key(s_ap); s_aiv = V.expand_iv(s_ap)
    http = (b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n"
            b"Content-Type: text/plain\r\n\r\nhello world")
    conn.sendall(V.aead_encrypt(s_ak, s_aiv, 0, http, 0x17))
    log("sent application data (hello world)")
    time.sleep(2)   # let TCP deliver before close


cert_der_bytes = b""

def main():
    global cert_der_bytes
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 14444
    with open(CERT_DER, "rb") as f:
        cert_der_bytes = f.read()
    # Compile/cache every Verbose binary the cert path needs.
    V.ensure(V.ALL_RULES + [("hkdf_extract", "hkdf_extract.verbose")])
    E.ensure()  # Ed25519 binaries for CertificateVerify signing
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", port)); s.listen(1)
    print(f"TLS-Verbose-cert listening on {port}", flush=True)
    conn, _ = s.accept()
    try:
        serve(conn)
    finally:
        conn.close(); s.close()


if __name__ == "__main__":
    main()
