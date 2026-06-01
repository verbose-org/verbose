"""Build a self-signed X.509 certificate whose Ed25519 signature is produced
by the project's pure-Verbose Ed25519 implementation (tools/tls_gen/ed25519.py,
which runs on Verbose-emitted machine-code binaries).

The keypair, public key, and signature are ALL produced by ed25519.py — no
openssl/library is used to produce any cryptographic material. openssl is used
only as a read-only validation oracle (see the bottom of this file / the
companion validation run).

DER helpers are small pure functions. We hand-build the tbsCertificate, sign its
exact DER bytes, and wrap into a Certificate SEQUENCE.

Outputs (next to this file):
  cert.der  — DER-encoded certificate
  cert.pem  — PEM-encoded certificate
  tbs.der   — the exact tbsCertificate DER bytes that were signed
  sig.bin   — the 64-byte raw Ed25519 signature
"""
import os, sys, base64

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import ed25519 as E

# ---------------------------------------------------------------------------
# DER primitives. Every value is (tag_byte, content_bytes) -> bytes.
# ---------------------------------------------------------------------------

def _der_len(n: int) -> bytes:
    """DER definite length encoding."""
    if n < 0x80:
        return bytes([n])
    body = n.to_bytes((n.bit_length() + 7) // 8, "big")
    return bytes([0x80 | len(body)]) + body

def _tlv(tag: int, content: bytes) -> bytes:
    return bytes([tag]) + _der_len(len(content)) + content

def der_seq(*parts: bytes) -> bytes:
    return _tlv(0x30, b"".join(parts))

def der_set(*parts: bytes) -> bytes:
    return _tlv(0x31, b"".join(parts))

def der_int(value: int) -> bytes:
    """INTEGER (handles positive values; emits a leading 0x00 if the high bit
    of the top byte is set, so the value stays positive)."""
    if value == 0:
        return _tlv(0x02, b"\x00")
    body = value.to_bytes((value.bit_length() + 7) // 8, "big")
    if body[0] & 0x80:
        body = b"\x00" + body
    return _tlv(0x02, body)

def der_oid(dotted: str) -> bytes:
    parts = [int(p) for p in dotted.split(".")]
    first = 40 * parts[0] + parts[1]
    body = bytearray([first])
    for p in parts[2:]:
        if p == 0:
            body.append(0)
            continue
        stack = []
        while p > 0:
            stack.append(p & 0x7F)
            p >>= 7
        # most-significant group first, all but the last have bit 0x80 set
        for i in range(len(stack) - 1, -1, -1):
            b = stack[i]
            if i != 0:
                b |= 0x80
            body.append(b)
    return _tlv(0x06, bytes(body))

def der_bitstring(data: bytes, unused: int = 0) -> bytes:
    return _tlv(0x03, bytes([unused]) + data)

def der_octetstring(data: bytes) -> bytes:
    return _tlv(0x04, data)

def der_bool(val: bool) -> bytes:
    return _tlv(0x01, b"\xff" if val else b"\x00")

def der_utctime(s: str) -> bytes:
    # s like "260101000000Z"
    return _tlv(0x17, s.encode("ascii"))

def der_printable(s: str) -> bytes:
    return _tlv(0x13, s.encode("ascii"))

def der_ia5(s: str) -> bytes:
    return _tlv(0x16, s.encode("ascii"))

def der_explicit(tagno: int, content: bytes) -> bytes:
    """Context-specific [tagno] EXPLICIT (constructed)."""
    return _tlv(0xA0 | tagno, content)

def der_context_primitive(tagno: int, content: bytes) -> bytes:
    """Context-specific [tagno] primitive (e.g. SAN dNSName [2])."""
    return _tlv(0x80 | tagno, content)


# ---------------------------------------------------------------------------
# X.509 structure
# ---------------------------------------------------------------------------

OID_ED25519 = "1.3.101.112"          # RFC 8410, id-Ed25519
OID_CN = "2.5.4.3"                   # commonName
OID_BASIC_CONSTRAINTS = "2.5.29.19"
OID_SAN = "2.5.29.17"

def alg_id_ed25519() -> bytes:
    # AlgorithmIdentifier { algorithm = id-Ed25519, parameters ABSENT }
    return der_seq(der_oid(OID_ED25519))

def name_cn(cn: str) -> bytes:
    # Name ::= SEQUENCE OF RelativeDistinguishedName
    # one RDN: SET OF { SEQUENCE { CN-oid, PrintableString } }
    rdn = der_set(der_seq(der_oid(OID_CN), der_printable(cn)))
    return der_seq(rdn)

def subject_public_key_info(pubkey: bytes) -> bytes:
    return der_seq(alg_id_ed25519(), der_bitstring(pubkey))

def extensions(dns_name: str) -> bytes:
    # basicConstraints CA:FALSE (not marked critical here — minimal-but-valid)
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

def build_tbs(serial: int, pubkey: bytes, cn: str, dns: str,
              not_before: str, not_after: str) -> bytes:
    version = der_explicit(0, der_int(2))   # [0] EXPLICIT INTEGER 2 (v3)
    serial_f = der_int(serial)
    sig_alg = alg_id_ed25519()
    issuer = name_cn(cn)
    validity = der_seq(der_utctime(not_before), der_utctime(not_after))
    subject = name_cn(cn)                   # self-signed: issuer == subject
    spki = subject_public_key_info(pubkey)
    exts = extensions(dns)
    return der_seq(version, serial_f, sig_alg, issuer, validity, subject, spki, exts)

def build_certificate(tbs: bytes, signature: bytes) -> bytes:
    return der_seq(tbs, alg_id_ed25519(), der_bitstring(signature))

def to_pem(der: bytes, label: str = "CERTIFICATE") -> bytes:
    b64 = base64.encodebytes(der).decode("ascii").replace("\n", "")
    lines = [b64[i:i + 64] for i in range(0, len(b64), 64)]
    body = "\n".join(lines)
    return f"-----BEGIN {label}-----\n{body}\n-----END {label}-----\n".encode("ascii")


def main():
    seed = bytes(range(32))   # FIXED deterministic seed: 000102...1f

    print("compiling Verbose binaries (ensure)...", flush=True)
    E.ensure()

    print("deriving public key via Verbose ed25519.public_key...", flush=True)
    pub = E.public_key(seed)
    assert len(pub) == 32, f"pubkey len {len(pub)}"
    print("PUBKEY", pub.hex(), flush=True)

    tbs = build_tbs(
        serial=1,
        pubkey=pub,
        cn="verbose-tls",
        dns="localhost",
        not_before="260101000000Z",
        not_after="360101000000Z",
    )
    print("tbs len", len(tbs), flush=True)

    print("signing tbsCertificate via Verbose ed25519.sign...", flush=True)
    sig = E.sign(seed, tbs)
    assert len(sig) == 64, f"sig len {len(sig)}"
    print("SIG", sig.hex(), flush=True)

    cert = build_certificate(tbs, sig)

    der_path = os.path.join(HERE, "cert.der")
    pem_path = os.path.join(HERE, "cert.pem")
    tbs_path = os.path.join(HERE, "tbs.der")
    sig_path = os.path.join(HERE, "sig.bin")
    with open(der_path, "wb") as f: f.write(cert)
    with open(pem_path, "wb") as f: f.write(to_pem(cert))
    with open(tbs_path, "wb") as f: f.write(tbs)
    with open(sig_path, "wb") as f: f.write(sig)

    print("wrote", der_path, pem_path, tbs_path, sig_path, flush=True)


if __name__ == "__main__":
    main()
