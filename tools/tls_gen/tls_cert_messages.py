"""TLS 1.3 Certificate (type 11) + CertificateVerify (type 15) handshake messages.

The CertificateVerify signature is produced by the project's pure-Verbose Ed25519
implementation (tools/tls_gen/ed25519.py): all signing crypto runs on Verbose-emitted
binaries; the host does only RFC 8446 byte framing. Self-contained building block
toward a browser-reachable TLS 1.3 server — NOT wired into the live server yet.

Conventions match tools/tls_gen/tls_server.py:
  hs_msg(t, body) = bytes([t]) + len.to_bytes(3,'big') + body
  b16 / b24 length-prefix helpers.

References:
  RFC 8446 §4.4.2 Certificate
  RFC 8446 §4.4.3 CertificateVerify
  RFC 8446 §4.4.3 / §4.2.3 SignatureScheme ed25519 = 0x0807
"""
import sys, os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ed25519 as E
import ecdsa_p256 as EC

# --- length-prefix helpers (same as tls_server.py) ---
def b16(x): return x.to_bytes(2, 'big')
def b24(x): return x.to_bytes(3, 'big')

def hs_msg(t, body):
    """Wrap a handshake-message body with type + 3-byte length (RFC 8446 §4)."""
    return bytes([t]) + b24(len(body)) + body

# --- constants ---
HS_CERTIFICATE = 0x0b          # RFC 8446 §4.4.2
HS_CERTIFICATE_VERIFY = 0x0f   # RFC 8446 §4.4.3
SIG_SCHEME_ED25519 = 0x0807    # RFC 8446 §4.2.3 ("ed25519")
SIG_SCHEME_ECDSA_P256 = 0x0403 # RFC 8446 §4.2.3 ("ecdsa_secp256r1_sha256")

# RFC 8446 §4.4.3: the context string for a server-side CertificateVerify.
# ASCII, NO trailing NUL inside the string; a single 0x00 separator byte follows.
CERTVERIFY_CONTEXT = b"TLS 1.3, server CertificateVerify"


def build_certificate(cert_der: bytes, context: bytes = b"") -> bytes:
    """Build a TLS 1.3 Certificate handshake message (type 11) for one X.509 cert.

    Body (RFC 8446 §4.4.2):
      opaque certificate_request_context<0..2^8-1>   -- 1-byte length prefix
      CertificateEntry certificate_list<0..2^24-1>   -- 3-byte length prefix

    Each CertificateEntry (X.509):
      opaque cert_data<1..2^24-1>      -- 3-byte length prefix + DER bytes
      Extension extensions<0..2^16-1>  -- 2-byte length prefix (empty => 0x00 0x00)
    """
    if len(context) > 0xff:
        raise ValueError("certificate_request_context exceeds 1-byte length field")
    if not (1 <= len(cert_der) <= 0xffffff):
        raise ValueError("cert_data must be 1..2^24-1 bytes")

    # one CertificateEntry: cert_data (b24-prefixed) + empty extensions (b16=0)
    entry = b24(len(cert_der)) + cert_der + b16(0)
    cert_list = entry  # single entry; certificate_list is just the concatenation

    body = bytes([len(context)]) + context + b24(len(cert_list)) + cert_list
    return hs_msg(HS_CERTIFICATE, body)


def certverify_signed_content(transcript_hash: bytes) -> bytes:
    """Exact bytes signed for a server CertificateVerify (RFC 8446 §4.4.3).

      0x20 * 64
      || "TLS 1.3, server CertificateVerify"   (ASCII, no NUL terminator)
      || 0x00                                    (single separator byte)
      || Transcript-Hash(Handshake Context, Certificate)   (SHA-256 => 32 bytes)
    """
    if len(transcript_hash) != 32:
        raise ValueError("transcript_hash must be 32 bytes (SHA-256)")
    return b"\x20" * 64 + CERTVERIFY_CONTEXT + b"\x00" + transcript_hash


def build_certificate_verify(seed: bytes, transcript_hash: bytes) -> bytes:
    """Build a TLS 1.3 CertificateVerify handshake message (type 15).

    Signs certverify_signed_content(transcript_hash) with the Verbose Ed25519 impl.
    `seed` is the 32-byte Ed25519 private seed (= the cert's signing key).

    Body (RFC 8446 §4.4.3):
      SignatureScheme algorithm;     -- 2 bytes, ed25519 = 0x0807
      opaque signature<0..2^16-1>;   -- 2-byte length prefix + signature
    """
    if len(seed) != 32:
        raise ValueError("Ed25519 seed must be 32 bytes")
    signed = certverify_signed_content(transcript_hash)
    sig = E.sign(seed, signed)  # 64 bytes; SLOW (process-per-byte Verbose path)
    if len(sig) != 64:
        raise ValueError(f"expected 64-byte Ed25519 signature, got {len(sig)}")
    body = b16(SIG_SCHEME_ED25519) + b16(len(sig)) + sig
    return hs_msg(HS_CERTIFICATE_VERIFY, body)


def build_certificate_verify_ecdsa_p256(priv_d: int, transcript_hash: bytes) -> bytes:
    """Build a TLS 1.3 CertificateVerify handshake message (type 15) signed with
    ECDSA-P256-SHA256 (SignatureScheme ecdsa_secp256r1_sha256 = 0x0403).

    This is the scheme Chrome offers (it never offers ed25519). Signs
    certverify_signed_content(transcript_hash) with the Verbose ECDSA impl.
    `priv_d` is the P-256 private scalar (= the cert's signing key).

    Body (RFC 8446 §4.4.3):
      SignatureScheme algorithm;     -- 2 bytes, ecdsa_secp256r1_sha256 = 0x0403
      opaque signature<0..2^16-1>;   -- 2-byte length prefix + signature

    IMPORTANT: for ecdsa_* schemes the TLS signature field carries the ECDSA
    *DER* encoding SEQUENCE{INTEGER r, INTEGER s} (RFC 8446 §4.2.3 references the
    ANSI X9.62 / SEC1 DER form), NOT a raw r||s. ecdsa_p256.sign already returns
    that DER form (low-s normalized), so the field is used verbatim.
    """
    signed = certverify_signed_content(transcript_hash)
    sig = EC.sign(priv_d, signed)  # DER SEQUENCE{INTEGER r, INTEGER s}, low-s
    # DER ECDSA-P256 sigs are 70-72 bytes typically (r,s each ~32B + framing).
    if not (8 <= len(sig) <= 72):
        raise ValueError(f"unexpected ECDSA DER signature length {len(sig)}")
    body = b16(SIG_SCHEME_ECDSA_P256) + b16(len(sig)) + sig
    return hs_msg(HS_CERTIFICATE_VERIFY, body)


# ---------------------------------------------------------------------------
# Self-test / validation gate. openssl is the strong independent oracle.
# ---------------------------------------------------------------------------
if __name__ == "__main__":
    import subprocess, hashlib, tempfile

    SEED = bytes(range(32))  # 000102...1f — same seed make_cert.py used
    EXP_PUB = bytes.fromhex(
        "03a107bff3ce10be1d70dd18e74bc09967e4d6309ba50d5f1ddc8664125531b8")
    HERE = os.path.dirname(os.path.abspath(__file__))
    CERT_DER = os.path.join(HERE, "cert.der")
    CERT_PEM = os.path.join(HERE, "cert.pem")

    # FIXED synthetic 32-byte transcript hash (hardcoded, not the real handshake).
    TH = hashlib.sha256(b"verbose-test-transcript").digest()

    with open(CERT_DER, "rb") as f:
        cert_der = f.read()

    print("ensure() — compiling Verbose binaries if needed (~30-60s first run)...",
          flush=True)
    E.ensure()

    results = {}  # name -> bool

    # --- (a) message structure asserts ---
    cert_msg = build_certificate(cert_der)
    signed = certverify_signed_content(TH)
    print(f"signed-content length = {len(signed)} (expect 130)", flush=True)
    print(f"transcript hash = {TH.hex()}", flush=True)

    print("signing CertificateVerify via Verbose Ed25519 (SLOW)...", flush=True)
    cv_msg = build_certificate_verify(SEED, TH)

    struct_ok = True
    try:
        # Certificate message
        assert cert_msg[0] == HS_CERTIFICATE, "cert type != 0x0b"
        c_len = int.from_bytes(cert_msg[1:4], 'big')
        assert c_len == len(cert_msg) - 4, "cert 3-byte length != body length"
        assert cert_msg[4] == 0x00, "cert context byte != 0x00 (empty)"
        clist_len = int.from_bytes(cert_msg[5:8], 'big')
        assert clist_len == len(cert_msg) - 8, "certificate_list length mismatch"
        entry_len = int.from_bytes(cert_msg[8:11], 'big')
        assert entry_len == len(cert_der), "cert_data length != DER length"
        assert cert_msg[11:11 + entry_len] == cert_der, "DER bytes mismatch"
        ext_len = int.from_bytes(cert_msg[11 + entry_len:13 + entry_len], 'big')
        assert ext_len == 0, "cert extensions not empty"

        # CertificateVerify message
        assert cv_msg[0] == HS_CERTIFICATE_VERIFY, "CV type != 0x0f"
        cv_len = int.from_bytes(cv_msg[1:4], 'big')
        assert cv_len == len(cv_msg) - 4, "CV 3-byte length != body length"
        alg = int.from_bytes(cv_msg[4:6], 'big')
        assert alg == SIG_SCHEME_ED25519, f"algorithm 0x{alg:04x} != 0x0807"
        sig_len = int.from_bytes(cv_msg[6:8], 'big')
        assert sig_len == 64, f"embedded sig length {sig_len} != 64"
        embedded_sig = cv_msg[8:8 + sig_len]
        assert len(embedded_sig) == 64
    except AssertionError as e:
        struct_ok = False
        print(f"STRUCTURE ASSERT FAILED: {e}", flush=True)
    results["(a) structure asserts"] = struct_ok

    print(f"signature = {embedded_sig.hex()}", flush=True)

    # --- (b) openssl pkeyutl -verify (strong oracle) ---
    # Use a dedicated scratch dir so cleanup is unambiguous.
    scratch = tempfile.mkdtemp(prefix="vcv_")
    pub_pem = os.path.join(scratch, "certpub.pem")
    signed_file = os.path.join(scratch, "signed.bin")
    sig_file = os.path.join(scratch, "sig.bin")
    ossl_out = os.path.join(scratch, "verify.out")

    # extract the cert's Ed25519 public key
    with open(pub_pem, "wb") as f:
        ex = subprocess.run(
            ["openssl", "x509", "-in", CERT_PEM, "-noout", "-pubkey"],
            stdout=f, stderr=subprocess.PIPE)
    if ex.returncode != 0:
        print("pubkey extraction failed:", ex.stderr.decode(), flush=True)

    with open(signed_file, "wb") as f:
        f.write(signed)
    with open(sig_file, "wb") as f:
        f.write(embedded_sig)

    ver = subprocess.run(
        ["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", pub_pem,
         "-rawin", "-in", signed_file, "-sigfile", sig_file],
        capture_output=True, text=True)
    # Persist stdout+stderr+exit to disk, then read back (no trusting stream).
    with open(ossl_out, "w") as f:
        f.write(f"EXIT={ver.returncode}\n")
        f.write("STDOUT:\n" + ver.stdout)
        f.write("STDERR:\n" + ver.stderr)
    with open(ossl_out) as f:
        ossl_disk = f.read()

    openssl_ok = (ver.returncode == 0
                  and "Signature Verified Successfully" in ver.stdout)
    results["(b) openssl verifies CV signature"] = openssl_ok

    print("----- openssl pkeyutl -verify (read back from disk) -----", flush=True)
    print(ossl_disk, flush=True)
    print("--------------------------------------------------------", flush=True)

    # --- (c) sign determinism + embedded pubkey match ---
    sig_again = E.sign(SEED, signed)
    det_ok = (sig_again == embedded_sig)
    pub = E.public_key(SEED)
    pub_ok = (pub == EXP_PUB)
    # also confirm the pubkey is the one embedded in cert.der
    pub_in_der = (EXP_PUB in cert_der)
    results["(c) sign determinism + pubkey match"] = det_ok and pub_ok and pub_in_der
    print(f"sign deterministic: {det_ok}", flush=True)
    print(f"public_key(seed) == expected: {pub_ok} ({pub.hex()})", flush=True)
    print(f"pubkey present in cert.der: {pub_in_der}", flush=True)

    print("===== RESULTS =====", flush=True)
    all_ok = True
    for name, ok in results.items():
        print(f"  {'PASS' if ok else 'FAIL'}  {name}", flush=True)
        all_ok = all_ok and ok

    # cleanup scratch dir
    for p in (pub_pem, signed_file, sig_file, ossl_out):
        try: os.remove(p)
        except OSError: pass
    try: os.rmdir(scratch)
    except OSError: pass

    sys.exit(0 if all_ok else 1)
