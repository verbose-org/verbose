"""ECDSA-P256-SHA256 signing (deterministic, RFC 6979) — BRICK 5a of the
ECDSA-over-P-256 Verbose arc. Assembles the validated lower bricks into a
complete, openssl-verifiable signature in the format TLS 1.3 CertificateVerify
carries for ecdsa_secp256r1_sha256 (0x0403):

    DER SEQUENCE { INTEGER r, INTEGER s }   (low-s normalized)

This is the crypto core that lets Chrome accept our CertificateVerify (Chrome
offers ecdsa_secp256r1_sha256, not ed25519). Brick 5b (P-256 cert + server
integration + browser test) builds on this; it is NOT part of 5a.

----------------------------------------------------------------------------
WHAT RUNS ON VERBOSE vs WHAT IS HOST GLUE  (honest accounting)
----------------------------------------------------------------------------
The heavy arithmetic that defines a signature's validity runs on Verbose-derived
code, exactly mirroring ed25519.py's "host does byte plumbing, crypto on Verbose"
discipline:

  VERBOSE (the security-defining operations):
    * SHA-256 of the message  -> tools/tls_gen/vcrypto.sha256 (runs the pure
      Verbose sha256_fold binary). This produces z, the value being signed.
    * k*G (scalar mult of the base point) -> p256_point.point_mul_inf_ref, the
      Python mirror of the native p256_scalarmult ladder. It is the SAME
      algorithm the native binary computes (field ops via brick 1's GF(p256)
      Montgomery core, big-int free), validated 5/5 against the oracle in
      p256_point.py's self-test. Running the native binary per-signature is
      possible but slow (256-bit ladder, one process per output byte); the
      Python mirror is byte-for-byte the same computation and is the faithful
      stand-in used here. The field/point core never touches Python big-int
      mul/mod — it is the limb arithmetic the Verbose emits.
    * The signature equation s = k^-1 * (z + r*d) mod n -> p256_scalar's
      Montgomery scalar field (nmul / nadd / ninv / reduce_mod_n), the brick-4
      big-int-free CIOS core that the Verbose p256nmul / p256ninv binaries
      emit. No Python big-int arithmetic in the s computation: every multiply,
      add, and inverse goes through the limb core.

  HOST GLUE (deterministic byte plumbing — the acknowledged category):
    * int <-> 32-byte big-endian conversions, DER encoding, low-s comparison
      against n/2, public-key point selection (d*G via the same Verbose mirror).
    * RFC 6979 deterministic-nonce derivation HMAC-SHA256 (see below).

----------------------------------------------------------------------------
RFC 6979 HMAC — HOST GLUE FOR NOW (flagged)
----------------------------------------------------------------------------
RFC 6979 §3.2 derives k via HMAC-SHA256 keyed by a rolling K over byte strings
of length 1 (V) up to 1+32+1+32+32 = 97 bytes (V||tag||int2octets(x)||
bits2octets(h1)). The pure-Verbose HMAC binary (examples/hmac_sha256.verbose)
has a FIXED 8-byte message interface (`@intention: 64-byte block-padded key +
8-byte message`); it cannot carry the variable, up-to-97-byte RFC 6979 inputs
without rewriting that .verbose rule — which is brick-5b-shaped work, out of
scope for 5a.

Therefore the RFC 6979 nonce HMAC-SHA256 uses Python's hmac+hashlib HERE.
This is HOST GLUE FOR NOW, clearly flagged. It does NOT weaken the "crypto on
Verbose" thesis: the binder / HKDF path in vcrypto.py already proves pure-Verbose
HMAC-SHA256 works (RFC 4231 + the key schedule chain). The nonce derivation is
a deterministic input-selection step; the validity of the resulting signature
is fully checkable against openssl and the RFC 6979 published vectors regardless
of how k is derived. When a variable-length Verbose HMAC interface exists
(brick 5b can add one), this is a one-line swap of `_hmac_sha256`.

----------------------------------------------------------------------------
VALIDATION (see __main__): RFC 6979 Appendix A.2.5 (P-256/SHA-256) published
(k, r, s) vectors AND `openssl dgst -sha256 -verify`. Both are independent
oracles; the gate is matching A.2.5 AND openssl printing "Verified OK".
"""
import sys, os, hmac as _pyhmac, hashlib as _hashlib
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import vcrypto
from p256_point import point_mul_inf_ref, GX_INT, GY_INT, N_ORDER
from p256_scalar import (
    N, to_limbs_n, from_limbs_n, nmul, nadd, ninv, reduce_mod_n,
)

# P-256 group order (== N from p256_scalar == N_ORDER from p256_point; assert).
assert N == N_ORDER, "scalar n and point n disagree"
G_XY = (GX_INT, GY_INT)
HALF_N = N // 2  # low-s boundary (s must be <= n/2 for TLS/BoringSSL)


# ---------------------------------------------------------------------------
# HOST GLUE FOR NOW: RFC 6979 nonce HMAC-SHA256 (variable-length inputs).
# See module docstring. Swap to a Verbose variable-length HMAC in brick 5b.
# ---------------------------------------------------------------------------
def _hmac_sha256(key: bytes, msg: bytes) -> bytes:
    return _pyhmac.new(key, msg, _hashlib.sha256).digest()


# ---------------------------------------------------------------------------
# SHA-256 on Verbose. z = int(SHA256(m)); for P-256/SHA-256, bitlen(hash)==256
# ==bitlen(n), so bits2int is the plain integer (no shift / truncation).
# ---------------------------------------------------------------------------
def _sha256_int(msg: bytes) -> int:
    """int(SHA256(msg)) with SHA256 computed by the pure-Verbose binary."""
    return int.from_bytes(vcrypto.sha256(msg), "big")


def _int2octets(x: int) -> bytes:
    """RFC 6979 int2octets: rlen = ceil(qlen/8) = 32 bytes big-endian."""
    return (x % N).to_bytes(32, "big") if x >= N else x.to_bytes(32, "big")


def _bits2octets(h1_int: int) -> bytes:
    """RFC 6979 bits2octets: int2octets(bits2int(h1) mod n). With hlen==qlen,
    bits2int(h1)=h1_int (no shift), so this is the 32-byte BE of (h1_int mod n)."""
    return (h1_int % N).to_bytes(32, "big")


# ---------------------------------------------------------------------------
# RFC 6979 §3.2 deterministic k generator. Yields candidate k values; the
# caller accepts the first 1 <= k < n that also yields r != 0, s != 0 (the
# r==0/s==0 retry, astronomically rare, handled by the loop).
# h1 is SHA256(m) bytes; x is the private key int. HMAC is host glue (flagged).
# ---------------------------------------------------------------------------
def _rfc6979_k_generator(x: int, h1: bytes):
    qlen_octets = 32  # rlen for P-256
    h1_int = int.from_bytes(h1, "big")
    bx = _int2octets(x)             # int2octets(x), 32 bytes BE of the private key
    bh = _bits2octets(h1_int)       # bits2octets(h1), 32 bytes BE of (h1 mod n)

    V = b"\x01" * 32
    K = b"\x00" * 32
    # K = HMAC_K(V || 0x00 || int2octets(x) || bits2octets(h1)); V = HMAC_K(V)
    K = _hmac_sha256(K, V + b"\x00" + bx + bh)
    V = _hmac_sha256(K, V)
    # K = HMAC_K(V || 0x01 || int2octets(x) || bits2octets(h1)); V = HMAC_K(V)
    K = _hmac_sha256(K, V + b"\x01" + bx + bh)
    V = _hmac_sha256(K, V)

    while True:
        # T accumulates HMAC blocks until tlen >= qlen. hlen==qlen==256, so one
        # block (32 bytes) is exactly enough; T = V.
        T = b""
        while len(T) < qlen_octets:
            V = _hmac_sha256(K, V)
            T += V
        # bits2int(T): T is 256 bits == qlen, so plain integer (no shift).
        k = int.from_bytes(T[:qlen_octets], "big")
        if 1 <= k < N:
            yield k
        # If k out of range OR caller rejects (r==0/s==0), advance the generator.
        K = _hmac_sha256(K, V + b"\x00")
        V = _hmac_sha256(K, V)


# ---------------------------------------------------------------------------
# Public key d*G via the Verbose-mirror ladder (same algorithm as native).
# ---------------------------------------------------------------------------
def public_key(d: int):
    """Affine (x, y) of d*G. d in [1, n-1]."""
    if not (1 <= d < N):
        raise ValueError("private key out of range [1, n-1]")
    xy = point_mul_inf_ref(d, G_XY)
    if xy is None:
        raise ValueError("d*G is the point at infinity (d == 0 mod n)")
    return xy


# ---------------------------------------------------------------------------
# s = k^-1 * (z + r*d) mod n, computed in the Verbose scalar Montgomery field
# (brick 4's nmul / nadd / ninv). NO Python big-int arithmetic here — every op
# goes through the limb core. r, z, d are plain ints in [0, n); converted in.
# ---------------------------------------------------------------------------
def _compute_s(k: int, z: int, r: int, d: int) -> int:
    kM = to_limbs_n(k)
    zM = to_limbs_n(z % N)
    rM = to_limbs_n(r % N)
    dM = to_limbs_n(d % N)
    kinvM = ninv(kM)                    # k^-1 mod n   (Verbose scalar field)
    rdM = nmul(rM, dM)                  # r*d mod n
    zrdM = nadd(zM, rdM)               # z + r*d mod n
    sM = nmul(kinvM, zrdM)            # k^-1 * (z + r*d) mod n
    return from_limbs_n(sM)


def sign_raw(d: int, msg: bytes):
    """Deterministic ECDSA-P256-SHA256 (RFC 6979) -> low-s (r, s) integers.

    z      = int(SHA256(msg))  [Verbose SHA-256]
    k      = RFC 6979 deterministic nonce  [HMAC host glue, flagged]
    R=k*G  [Verbose-mirror ladder]; r = R.x mod n  [Verbose reduce_mod_n]
    s      = k^-1 * (z + r*d) mod n  [Verbose scalar field]; low-s normalized.
    """
    if not (1 <= d < N):
        raise ValueError("private key out of range [1, n-1]")
    h1 = vcrypto.sha256(msg)            # SHA-256 on Verbose
    z = int.from_bytes(h1, "big")      # bits2int(h1): hlen==qlen, plain int

    for k in _rfc6979_k_generator(d, h1):
        R = point_mul_inf_ref(k, G_XY)  # k*G via Verbose-mirror ladder
        if R is None:
            continue                    # k*G == O (only if k==0 mod n; can't happen here)
        r = reduce_mod_n(R[0])         # r = R.x mod n  (Verbose scalar reduce)
        if r == 0:
            continue                    # RFC 6979: bump to next k (astronomically rare)
        s = _compute_s(k, z, r, d)     # k^-1 * (z + r*d) mod n  (Verbose field)
        if s == 0:
            continue                    # also bump (astronomically rare)
        # Low-s normalization (REQUIRED for TLS/BoringSSL): if s > n/2, s = n - s.
        if s > HALF_N:
            s = N - s
        return (r, s)
    raise RuntimeError("RFC 6979 generator exhausted (impossible for valid d)")


# ---------------------------------------------------------------------------
# DER encoding: SEQUENCE { INTEGER r, INTEGER s }. INTEGER is minimal big-endian
# two's-complement-positive: strip leading zero bytes, then prepend a single
# 0x00 if the top bit of the first byte is set (so it stays a positive integer).
# ---------------------------------------------------------------------------
def _der_int(x: int) -> bytes:
    if x < 0:
        raise ValueError("DER INTEGER for ECDSA must be non-negative")
    b = x.to_bytes((x.bit_length() + 7) // 8 or 1, "big")
    b = b.lstrip(b"\x00") or b"\x00"   # minimal big-endian
    if b[0] & 0x80:                    # high bit set -> prepend 0x00 to keep positive
        b = b"\x00" + b
    return b"\x02" + _der_len(len(b)) + b


def _der_len(n: int) -> bytes:
    if n < 0x80:
        return bytes([n])
    enc = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(enc)]) + enc


def _der_sequence(*elements: bytes) -> bytes:
    body = b"".join(elements)
    return b"\x30" + _der_len(len(body)) + body


def sign(d: int, msg: bytes) -> bytes:
    """ECDSA-P256-SHA256 signature as DER SEQUENCE{INTEGER r, INTEGER s}
    (low-s, RFC 6979 deterministic). This is the format TLS CertificateVerify
    carries for ecdsa_secp256r1_sha256."""
    r, s = sign_raw(d, msg)
    return _der_sequence(_der_int(r), _der_int(s))


# ===========================================================================
# VALIDATION — the gate: RFC 6979 A.2.5 published vectors AND openssl verify.
# ===========================================================================
if __name__ == "__main__":
    import subprocess, tempfile, json

    # Only SHA-256 is needed from the Verbose binaries for 5a (k*G and the
    # scalar field run via the Python mirrors of the validated native code).
    vcrypto.ensure([("sha256_fold", "sha256_fold.verbose")])

    # ---- RFC 6979 Appendix A.2.5 (P-256, SHA-256) — the authoritative oracle ----
    # Private key x and the published (k, r, s) for SHA-256 with messages
    # "sample" and "test". https://www.rfc-editor.org/rfc/rfc6979#appendix-A.2.5
    A25_X = 0xC9AFA9D845BA75166B5C215767B1D6934E50C3DB36E89B127B8A622B120F6721
    A25 = {
        "sample": {
            "k": 0xA6E3C57DD01ABE90086538398355DD4C3B17AA873382B0F24D6129493D8AAD60,
            "r": 0xEFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716,
            "s": 0xF7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8,
        },
        "test": {
            "k": 0xD16B6AE827F17175E040871A1C7EC3500192C4C92677336EC2537ACAEE0008E0,
            "r": 0xF1ABB023518351CD71D881567B1EA663ED3EFCF6C5132B354F28D3B0B7D38367,
            "s": 0x019F4113742A2B14BD25926B49C649155F267E60D3814B4C0CC84250E46F0083,
        },
    }

    report = {"a25": {}, "openssl": [], "low_s": []}

    # Confirm our public key matches the A.2.5 published Ux/Uy (sanity on d*G).
    A25_UX = 0x60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6
    A25_UY = 0x7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299
    pub_xy = public_key(A25_X)
    report["pubkey_matches_a25"] = (pub_xy == (A25_UX, A25_UY))

    # For each A.2.5 message: recompute (r, s) and the deterministic k.
    # NOTE: A.2.5 publishes the RAW s (NOT low-s normalized). We compute both:
    #   - raw (r, s) to compare against the published vector EXACTLY,
    #   - then confirm our low-s output equals min(s, n-s).
    for label, vec in A25.items():
        msg = label.encode()           # the RFC 6979 test messages are ASCII
        h1 = vcrypto.sha256(msg)
        z = int.from_bytes(h1, "big")
        # deterministic k from the generator (first valid)
        kgen = _rfc6979_k_generator(A25_X, h1)
        k_used = None
        for k in kgen:
            R = point_mul_inf_ref(k, G_XY)
            r = reduce_mod_n(R[0])
            if r == 0:
                continue
            s_raw = _compute_s(k, z, r, A25_X)
            if s_raw == 0:
                continue
            k_used = k
            break
        s_low = (N - s_raw) if s_raw > HALF_N else s_raw
        report["a25"][label] = {
            "k_exp": f"{vec['k']:064x}", "k_got": f"{k_used:064x}",
            "r_exp": f"{vec['r']:064x}", "r_got": f"{r:064x}",
            "s_exp": f"{vec['s']:064x}", "s_got": f"{s_raw:064x}",
            "k_match": k_used == vec["k"],
            "r_match": r == vec["r"],
            "s_match": s_raw == vec["s"],
            "s_low": f"{s_low:064x}",
            "s_low_ok": s_low <= HALF_N,
        }

    # ---- openssl verify: sign several (d, m), build a PEM pubkey, verify ----
    def _pem_pubkey(d: int, path: str):
        """Write a PEM SubjectPublicKeyInfo for d*G using the `cryptography`
        lib's SERIALIZER ONLY (an independent encoder; the point d*G itself
        comes from our Verbose-mirror ladder, then is checked on-curve by the
        lib when constructing the public key)."""
        from cryptography.hazmat.primitives.asymmetric import ec
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.backends import default_backend
        x, y = public_key(d)
        pub = ec.EllipticCurvePublicNumbers(x, y, ec.SECP256R1()).public_key(default_backend())
        pem = pub.public_bytes(
            serialization.Encoding.PEM,
            serialization.PublicFormat.SubjectPublicKeyInfo,
        )
        with open(path, "wb") as f:
            f.write(pem)

    test_cases = [
        (A25_X, b"sample"),
        (A25_X, b"test"),
        (0x1, b"hello world"),
        (0x0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF, b"verbose ecdsa brick 5a"),
        (N - 1, b""),
    ]

    tmpd = tempfile.mkdtemp(prefix="ecdsa5a_")
    for idx, (d, m) in enumerate(test_cases):
        der = sign(d, m)
        r, s = sign_raw(d, m)
        report["low_s"].append({"case": idx, "s_le_half_n": s <= HALF_N})
        sigp = os.path.join(tmpd, f"sig{idx}.der")
        msgp = os.path.join(tmpd, f"msg{idx}.bin")
        pubp = os.path.join(tmpd, f"pub{idx}.pem")
        with open(sigp, "wb") as f:
            f.write(der)
        with open(msgp, "wb") as f:
            f.write(m)
        _pem_pubkey(d, pubp)
        proc = subprocess.run(
            ["openssl", "dgst", "-sha256", "-verify", pubp, "-signature", sigp, msgp],
            capture_output=True, text=True,
        )
        report["openssl"].append({
            "case": idx, "d_bits": d.bit_length(), "msg": m.decode("latin1"),
            "stdout": proc.stdout.strip(), "rc": proc.returncode,
        })

    # Write report to disk so results are read from disk (env can fabricate stdout).
    rp = os.path.join(tmpd, "report.json")
    with open(rp, "w") as f:
        json.dump(report, f, indent=2)
    print("REPORT_PATH", rp)

    a25_ok = all(
        v["k_match"] and v["r_match"] and v["s_match"] and v["s_low_ok"]
        for v in report["a25"].values()
    ) and report["pubkey_matches_a25"]
    ossl_ok = all(e["stdout"] == "Verified OK" and e["rc"] == 0 for e in report["openssl"])
    lows_ok = all(e["s_le_half_n"] for e in report["low_s"])

    print("A25_MATCH", "OK" if a25_ok else "FAIL")
    print("OPENSSL", "OK" if ossl_ok else "FAIL")
    print("LOW_S", "OK" if lows_ok else "FAIL")
    print("ECDSA_P256_5A_OK" if (a25_ok and ossl_ok and lows_ok) else "ECDSA_P256_5A_FAIL")
    sys.exit(0 if (a25_ok and ossl_ok and lows_ok) else 1)
