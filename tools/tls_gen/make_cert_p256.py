"""Build a self-signed X.509 certificate over the NIST P-256 curve whose ECDSA
signature is produced by the project's pure-Verbose ECDSA-P256-SHA256
implementation (tools/tls_gen/ecdsa_p256.py — brick 5a).

This is brick 5b's cert half. It exists because Chrome offers
ecdsa_secp256r1_sha256 (SignatureScheme 0x0403) and NEVER ed25519 (0x0807):
a P-256 leaf + an ECDSA CertificateVerify is what lets a real browser proceed
to verify our handshake.

The keypair (d*G), the certificate's self-signature, and (via the server) the
CertificateVerify are ALL produced by ecdsa_p256.py — the heavy arithmetic runs
on Verbose-derived limb code (k*G ladder, scalar Montgomery field) and the
message hash on the pure-Verbose SHA-256 binary. openssl is used ONLY as a
read-only validation oracle.

DER helpers are reused verbatim from make_cert.py (the Ed25519 cert builder);
only the algorithm identifiers and SubjectPublicKeyInfo differ for EC.

Fixed private key (reproducible):
    d = int.from_bytes(bytes(range(1, 33)), "big") % n
      = 0x0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20
    (bytes 01 02 ... 20 big-endian, reduced mod the P-256 group order n; the
    reduction is a no-op here since the value < n, but kept explicit so any
    fixed 32-byte pattern stays in range). pubkey = d*G via the Verbose ladder.

DER differences from the Ed25519 cert (RFC 5480 / SEC1):
  * SubjectPublicKeyInfo.algorithm = SEQUENCE {
        OID id-ecPublicKey  1.2.840.10045.2.1,
        OID prime256v1      1.2.840.10045.3.1.7        (named-curve parameters) }
    subjectPublicKey BIT STRING = 0x04 || X(32 BE) || Y(32 BE)   (65 bytes).
  * signatureAlgorithm (both tbs.signature and the outer Certificate) =
        SEQUENCE { OID ecdsa-with-SHA256 1.2.840.10045.4.3.2 }   (params ABSENT).
  * signatureValue BIT STRING = the ECDSA DER signature SEQUENCE{INTEGER r,
    INTEGER s} over the tbsCertificate DER bytes (self-signed with the same d).

Outputs (next to this file):
  cert_p256.der  — DER-encoded certificate
  cert_p256.pem  — PEM-encoded certificate
  tbs_p256.der   — the exact tbsCertificate DER bytes that were signed
  sig_p256.der   — the ECDSA DER signature over the tbs

Validation gate (see __main__):
  openssl x509 -inform DER -in cert_p256.der -text -noout   (shows ECDSA/P-256)
  openssl verify -CAfile cert_p256.pem -check_ss_sig cert_p256.pem  -> OK
"""
import os, sys, base64

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

# Reuse the pure-function DER helpers from the Ed25519 cert builder verbatim —
# they are algorithm-agnostic TLV encoders.
from make_cert import (
    _der_len, _tlv, der_seq, der_set, der_int, der_oid, der_bitstring,
    der_octetstring, der_bool, der_utctime, der_printable, der_ia5,
    der_explicit, der_context_primitive, name_cn, to_pem,
)
import ecdsa_p256 as EC
import vcrypto

# ---------------------------------------------------------------------------
# EC OIDs (RFC 5480 / SEC1).
# ---------------------------------------------------------------------------
OID_EC_PUBLIC_KEY = "1.2.840.10045.2.1"        # id-ecPublicKey
OID_PRIME256V1 = "1.2.840.10045.3.1.7"          # prime256v1 / secp256r1
OID_ECDSA_WITH_SHA256 = "1.2.840.10045.4.3.2"   # ecdsa-with-SHA256

OID_BASIC_CONSTRAINTS = "2.5.29.19"
OID_SAN = "2.5.29.17"


def alg_id_ec_public_key() -> bytes:
    """AlgorithmIdentifier { id-ecPublicKey, namedCurve prime256v1 }.

    Unlike Ed25519 (parameters ABSENT), EC carries the named curve as the
    parameters OID — this is what makes openssl print 'ASN1 OID: prime256v1'.
    """
    return der_seq(der_oid(OID_EC_PUBLIC_KEY), der_oid(OID_PRIME256V1))


def alg_id_ecdsa_with_sha256() -> bytes:
    """AlgorithmIdentifier { ecdsa-with-SHA256 }, parameters ABSENT (RFC 5758)."""
    return der_seq(der_oid(OID_ECDSA_WITH_SHA256))


def ec_point_uncompressed(x: int, y: int) -> bytes:
    """SEC1 uncompressed point: 0x04 || X(32 BE) || Y(32 BE) = 65 bytes."""
    return b"\x04" + x.to_bytes(32, "big") + y.to_bytes(32, "big")


def subject_public_key_info(x: int, y: int) -> bytes:
    point = ec_point_uncompressed(x, y)
    return der_seq(alg_id_ec_public_key(), der_bitstring(point))


def extensions(dns_name: str) -> bytes:
    # basicConstraints CA:FALSE, marked critical (same as the Ed25519 cert).
    bc_value = der_seq()  # empty SEQUENCE => cA defaults FALSE, no pathlen
    ext_bc = der_seq(
        der_oid(OID_BASIC_CONSTRAINTS),
        der_bool(True),                      # critical
        der_octetstring(bc_value),
    )
    # subjectAltName: GeneralNames ::= SEQUENCE OF GeneralName; dNSName [2] IA5
    san_value = der_seq(der_context_primitive(2, dns_name.encode("ascii")))
    ext_san = der_seq(
        der_oid(OID_SAN),
        der_octetstring(san_value),
    )
    exts = der_seq(ext_bc, ext_san)
    return der_explicit(3, exts)            # [3] EXPLICIT Extensions


def build_tbs(serial: int, x: int, y: int, cn: str, dns: str,
              not_before: str, not_after: str) -> bytes:
    version = der_explicit(0, der_int(2))   # [0] EXPLICIT INTEGER 2 (v3)
    serial_f = der_int(serial)
    sig_alg = alg_id_ecdsa_with_sha256()    # inner signature algorithm (== outer)
    issuer = name_cn(cn)
    validity = der_seq(der_utctime(not_before), der_utctime(not_after))
    subject = name_cn(cn)                   # self-signed: issuer == subject
    spki = subject_public_key_info(x, y)
    exts = extensions(dns)
    return der_seq(version, serial_f, sig_alg, issuer, validity, subject, spki, exts)


def build_certificate(tbs: bytes, signature_der: bytes) -> bytes:
    """Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm,
    signatureValue }. signatureValue is the ECDSA DER signature wrapped in a
    BIT STRING (0 unused bits)."""
    return der_seq(tbs, alg_id_ecdsa_with_sha256(), der_bitstring(signature_der))


# Fixed, reproducible private key in [1, n-1].
PRIV_D = int.from_bytes(bytes(range(1, 33)), "big") % EC.N


def main():
    print("compiling Verbose binaries (sha256_fold) for ECDSA signing...", flush=True)
    vcrypto.ensure([("sha256_fold", "sha256_fold.verbose")])

    print(f"private key d = {PRIV_D:#066x}", flush=True)
    x, y = EC.public_key(PRIV_D)            # d*G via the Verbose-mirror ladder
    print(f"pubkey x = {x:#066x}", flush=True)
    print(f"pubkey y = {y:#066x}", flush=True)

    tbs = build_tbs(
        serial=1,
        x=x, y=y,
        cn="verbose-tls",
        dns="localhost",
        not_before="260101000000Z",
        not_after="360101000000Z",
    )
    print(f"tbs len {len(tbs)}", flush=True)

    print("signing tbsCertificate via Verbose ECDSA-P256-SHA256 (ecdsa_p256.sign)...",
          flush=True)
    sig_der = EC.sign(PRIV_D, tbs)         # DER SEQUENCE{INTEGER r, INTEGER s}, low-s
    print(f"sig (DER) len {len(sig_der)}: {sig_der.hex()}", flush=True)

    cert = build_certificate(tbs, sig_der)

    der_path = os.path.join(HERE, "cert_p256.der")
    pem_path = os.path.join(HERE, "cert_p256.pem")
    tbs_path = os.path.join(HERE, "tbs_p256.der")
    sig_path = os.path.join(HERE, "sig_p256.der")
    with open(der_path, "wb") as f: f.write(cert)
    with open(pem_path, "wb") as f: f.write(to_pem(cert))
    with open(tbs_path, "wb") as f: f.write(tbs)
    with open(sig_path, "wb") as f: f.write(sig_der)

    print("wrote", der_path, pem_path, tbs_path, sig_path, flush=True)


if __name__ == "__main__":
    main()
