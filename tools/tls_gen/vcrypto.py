"""vcrypto: TLS 1.3 cryptographic layer driven by the pure-Verbose binaries.

Every cryptographic transformation here is computed by Verbose-emitted machine
code (compiled from examples/*.verbose). The host (this file) only spawns those
binaries and shuttles bytes.

X25519 (ladder + finish) returns RECORDS since the aggregate-return arc
(slices agg-1/agg-2a/agg-2c, 2026-08): one spawn yields all 20 ladder limbs
as a LadderLimbs JSON object and one spawn yields all 32 output bytes as a
Digest JSON object — 2 spawns per X25519 instead of 52. The HKDF-expand
family followed (2026-08-29): handshake_secret, derive_s_hs_traffic,
expand_key and expand_iv each return ALL their output bytes as one record
(Digest / KeyBytes / IvBytes JSON), so a full server key-schedule leg
(hs -> s_hs -> key + iv) is 4 spawns instead of 92. The six
tls_schedule.verbose rules followed the same day (tranche 3): _sched is ONE
record spawn, so the derived/master/traffic/finished leg is 6 spawns
instead of 192 pooled ones. SHA-256 followed (tranche 4): sha256_fold is a
RECURSIVE record-output rule (agg-2a tail recursion + agg-2c record entry),
so one spawn folds every block and returns all 32 digest bytes as one Digest
JSON object instead of 32 parallel `which` spawns. AES/GCM/GHASH followed
(tranche 5, 2026-08-30): `encrypt` returns one 16-field CipherBlock record
per spawn, `ghash_fold` (recursive, like sha256_fold) returns the whole
16-byte GHASH accumulator in one spawn, and `gctr` returns one PER-BLOCK
CipherBlock record — the host loop spawns once per 16-byte block instead of
once per data byte (framing glue: pad the tail block's hex, truncate after
unpacking). HKDF-Extract and the PSK schedule closed the arc (tranche 6,
2026-08-30): `hkdf_extract`, `psk_early_secret` and `psk_ext_binder_key`
each return all 32 output bytes as one Digest record. EVERY primitive is a
single record spawn of a verified binary — nothing spawns per-`which`, no
caller loops over output bytes, and the 64-thread pool that existed to beat
the one-byte-per-process-run cost is gone.

The AEAD FRAMING followed on 2026-09-02 (examples/gcm_frame.verbose): the
per-record nonce, J0, the record-header AAD, GHASH's length block, the tag
XOR and the constant-time tag compare -- every FIXED-WIDTH step §7 MAJOR-1
listed as "crypto in Python" -- are each one record spawn of a verified
binary. The host keeps only the variable-length glue (zero-padding a tail to
a block boundary, concatenating GHASH's input, the per-block CTR loop),
byte shuttling between binaries, and TLS framing that is not cryptography
(inner content type, record slicing). See the AEAD section below for the
per-step ledger.

Honest scope (per docs/tls-io-statemachine-design.md §4/§7): the cryptographic
PRIMITIVES (X25519, key schedule, SHA-256, AES/GCM/GHASH) AND the fixed-width
AEAD framing are pure Verbose. Byte repacking (bytes<->limbs), variable-length
padding/concatenation, sequencing, and randomness are host glue, clearly
separated below.
"""
import subprocess, os, sys, json

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = {}

def _compile(rule, src):
    out = f"/tmp/v_{rule}"
    if rule in BIN: return BIN[rule]
    r = subprocess.run(["cargo","run","--release","--","--native",out,"--run",rule, os.path.join("examples",src)],
                       cwd=ROOT, capture_output=True, text=True)
    # arg order: verbosec <file> --native <out> --run <rule>; but our CLI is file first.
    if not os.path.exists(out):
        r = subprocess.run(["cargo","run","--release","--", os.path.join("examples",src),
                            "--native",out,"--run",rule], cwd=ROOT, capture_output=True, text=True)
    if not os.path.exists(out):
        raise RuntimeError(f"compile {rule} from {src} failed: {r.stderr[-400:]}")
    BIN[rule] = out
    return out

def ensure(rules):
    for rule, src in rules: _compile(rule, src)

# ---- byte<->limb repacking (host glue: deterministic format conversion) ----
OFF = [0,26,51,77,102,128,153,179,204,230]; W=[26,25,26,25,26,25,26,25,26,25]
def to_limbs(x):
    x &= (1<<255)-1
    return [(x>>OFF[i]) & ((1<<W[i])-1) for i in range(10)]

# ---- X25519 (pure Verbose: ladder + finish; record output, 2 spawns) ----
def _run_record(rule, args):
    """One spawn of a record-output binary; parse its JSON object from stdout."""
    r = subprocess.run([BIN[rule]]+args, capture_output=True, text=True, timeout=600)
    s = r.stdout.strip()
    if s == "": raise RuntimeError(f"{BIN[rule]} empty (rc={r.returncode}) {r.stderr[-200:]}")
    return json.loads(s)

def _record_bytes(rule, args, n):
    """One spawn of a record-output rule whose fields are b0..b(n-1); return bytes."""
    rec = _run_record(rule, args)
    return bytes(rec[f"b{i}"] for i in range(n))

def x25519(scalar32: bytes, u32: bytes) -> bytes:
    u_int = int.from_bytes(u32,'little') & ((1<<255)-1)
    ul = to_limbs(u_int)
    init = to_limbs(1)+to_limbs(0)+list(ul)+to_limbs(1)+list(ul)
    sc_hex = scalar32.hex()
    # ONE spawn: the ladder returns all 20 limbs as a LadderLimbs record.
    rec = _run_record("ladder", [str(v) for v in init] + ["0","255",sc_hex])  # swap=0, i=255, scalar
    x2 = [rec[f"x2_{i}"] for i in range(10)]; z2 = [rec[f"z2_{i}"] for i in range(10)]
    # Recursive finish (x25519_rec.verbose): state seeded by the host —
    # t = z = z2, the 8 saved-intermediate slots = 0, x2 = ladder numerator,
    # j = 265. Same 266 field-muls as the unrolled finish, 31x smaller binary.
    zero = [0]*10
    # State groups in order: t, z2, z9, z11, z2_5_0, z2_10_0, z2_20_0,
    # z2_50_0, z2_100_0, z, x2, then j.
    fin_state = (
        [str(v) for v in z2]          # t
        + [str(v) for v in z2]        # z2
        + [str(v) for v in zero]      # z9
        + [str(v) for v in zero]      # z11
        + [str(v) for v in zero]      # z2_5_0
        + [str(v) for v in zero]      # z2_10_0
        + [str(v) for v in zero]      # z2_20_0
        + [str(v) for v in zero]      # z2_50_0
        + [str(v) for v in zero]      # z2_100_0
        + [str(v) for v in z2]        # z (== z2 input)
        + [str(v) for v in x2]        # x2
        + ["265"]                     # j
    )
    # ONE spawn: the finish returns all 32 output bytes as a Digest record.
    dig = _run_record("x25519_finish", fin_state)
    return bytes(dig[f"b{i}"] for i in range(32))

# ---- SHA-256 (pure Verbose) of arbitrary bytes ----
# sha256_fold returns a Digest RECORD (one spawn, all 32 bytes as one
# {"b0":...,...,"b31":...} JSON object) since 2026-08-29 — it is a RECURSIVE
# record-output rule (agg-2a tail recursion folds every block; agg-2c serves
# the record entry), so the old `which`-per-byte interface's 32 parallel spawns
# collapse to one sequential spawn.
H0=[0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19]
def sha256(msg: bytes) -> bytes:
    padded=bytearray(msg); L=len(msg); padded.append(0x80)
    while len(padded)%64!=56: padded.append(0)
    padded += (L*8).to_bytes(8,'big')
    nb=len(padded)//64
    # ONE spawn: h0..h7, nblocks, i, data(hex) — no `which`; returns 32 bytes.
    return _record_bytes("sha256_fold",[str(w) for w in H0]+[str(nb),str(nb),bytes(padded).hex()],32)

# ---- key schedule (pure Verbose) ----
# EVERY key-schedule rule returns a RECORD (one spawn, all output bytes as one
# {"b0":...,...} JSON object) since 2026-08-29 — the four HKDF-expand-family
# rules in tranche 2, the six tls_schedule.verbose rules (all via _sched) in
# tranche 3.
def _sched(rule, secret32, thash32):
    return _record_bytes(rule, [str(b) for b in secret32]+[str(b) for b in thash32], 32)
def handshake_secret(ecdhe32): return _record_bytes("handshake_secret",[str(b) for b in ecdhe32],32)
def derive_derived(secret32): return _sched("derive_derived", secret32, bytes(32))
def master_secret(derived32): return _sched("master_secret", derived32, bytes(32))
def derive_s_hs(secret32, thash32):
    return _record_bytes("derive_s_hs_traffic",[str(b) for b in secret32]+[str(b) for b in thash32],32)
def derive_c_hs(secret32, thash32): return _sched("derive_c_hs_traffic", secret32, thash32)
def derive_s_ap(secret32, thash32): return _sched("derive_s_ap_traffic", secret32, thash32)
def derive_c_ap(secret32, thash32): return _sched("derive_c_ap_traffic", secret32, thash32)
def finished_key(secret32): return _sched("finished_key", secret32, bytes(32))
def expand_key(secret32): return _record_bytes("expand_key",[str(b) for b in secret32],16)
def expand_iv(secret32):  return _record_bytes("expand_iv",[str(b) for b in secret32],12)

# ---- HKDF-Extract + PSK schedule (pure Verbose; record spawns since tranche 6,
# 2026-08-30 — the LAST which-form rules; their conversion is what killed the pool) ----
def hkdf_extract(salt32, ikm32):
    """PRK = HMAC-SHA256(salt, IKM), both 32 bytes (RFC 5869 2.2); one Digest spawn."""
    return _record_bytes("hkdf_extract", [str(b) for b in salt32]+[str(b) for b in ikm32], 32)
def psk_early_secret(psk32):
    """Early Secret = HKDF-Extract(0^32, PSK) (RFC 8446 7.1); one Digest spawn."""
    return _record_bytes("psk_early_secret", [str(b) for b in psk32], 32)
def psk_ext_binder_key(early32):
    """binder_key = Derive-Secret(Early, "ext binder", "") (RFC 8446 7.1); one Digest spawn."""
    return _record_bytes("psk_ext_binder_key", [str(b) for b in early32], 32)

# ---- AES-GCM AEAD record protection (primitives AND framing pure Verbose) ----
# All three primitives are RECORD spawns since tranche 5 (2026-08-30):
#   encrypt    — ONE spawn, all 16 ciphertext bytes as a CipherBlock record
#   gctr       — one spawn PER 16-BYTE BLOCK (nb spawns, not len(data)); the
#                host pads the tail block's hex to a full 32 chars and
#                truncates after unpacking — framing glue by design.
#                byte_at's fail-closed bounds make the padding load-bearing:
#                an unpadded short tail would abort the binary.
#   ghash_fold — ONE spawn; the recursive fold walks every block in-process
#                and returns the 16-byte accumulator as a GhashOut record.
def _aes_block(key16, block16):
    return _record_bytes("encrypt", [str(b) for b in block16]+[str(b) for b in key16], 16)
def _gctr(key16, nonce12, data):
    nb=(len(data)+15)//16
    if nb==0: return b""
    padded=bytes(data)+bytes((-len(data))%16)
    args=[str(b) for b in key16]+[str(b) for b in nonce12]+[str(nb)]
    hexd=padded.hex()
    out=b"".join(_record_bytes("gctr", args+[str(w), hexd], 16) for w in range(nb))
    return out[:len(data)]
def _ghash(h16, data):
    nb=len(data)//16
    args=[str(b) for b in [0]*16]+[str(b) for b in h16]+[str(nb),str(nb),bytes(data).hex()]
    return _record_bytes("ghash_fold", args, 16)

# ---- AEAD framing (pure Verbose since 2026-09-02: examples/gcm_frame.verbose) ----
# Every FIXED-WIDTH step of TLS 1.3 record protection that used to be Python
# here is now one record spawn of a verified binary. Per step:
#   gcm_nonce    RFC 8446 5.3   nonce = IV XOR [seq]64 (right-aligned)   -> 12 bytes
#   gcm_j0       SP 800-38D 7.1 J0 = nonce || 0^31 || 1 (binds gcm_nonce) -> 16 bytes
#   gcm_aad      RFC 8446 5.2   23 || 0303 || [len(inner)+16]16            -> 5 bytes
#   gcm_lenblock SP 800-38D 7.1 [len(A)*8]64 || [len(C)*8]64              -> 16 bytes
#   gcm_tag      SP 800-38D 7.1 T = S XOR E_K(J0)                          -> 16 bytes
#   gcm_tag_eq   constant-time compare (bxor/bor fold, cmov decision)      -> 0 / 1
# What the host STILL does around them, each variable-length or not crypto:
#   * zero-pad a variable-length AAD / ciphertext tail to a 16-byte boundary
#     and concatenate pad(A) || pad(C) || lenblock into GHASH's hex input
#     (_pad16 below; byte_at's fail-closed bounds are what make the padding
#     load-bearing for the gctr tail as well, see _gctr);
#   * the per-16-byte-block CTR loop (_gctr spawns gctr once per block);
#   * shuttle bytes between binaries: gcm_j0's record into `encrypt`,
#     GHASH's record and E_K(J0)'s record into gcm_tag, the computed and the
#     received tag into gcm_tag_eq;
#   * TLS framing that is not cryptography: append the inner content type
#     on protect, strip zero padding + content type on unprotect, slice the
#     received record into header / ciphertext / tag;
#   * branch on gcm_tag_eq's VERDICT (accept / reject is public by nature --
#     the peer observes it as an alert or a continued handshake).
# Nothing here XORs, pads a nonce, builds J0 or a length block, or compares
# tag bytes in Python any more -- grep this file for `^` to confirm: the
# only XOR left is the self-test's single-bit-flip negative control.
def gcm_nonce(iv12, seq):    return _record_bytes("gcm_nonce", [str(b) for b in iv12]+[str(seq)], 12)
def gcm_j0(iv12, seq):       return _record_bytes("gcm_j0", [str(b) for b in iv12]+[str(seq)], 16)
def gcm_aad(inner_len):      return _record_bytes("gcm_aad", [str(inner_len)], 5)
def gcm_lenblock(aad_len, ct_len): return _record_bytes("gcm_lenblock", [str(aad_len), str(ct_len)], 16)
def gcm_tag(S16, EJ0_16):    return _record_bytes("gcm_tag", [str(b) for b in S16]+[str(b) for b in EJ0_16], 16)
def gcm_tag_eq(a16, b16):
    """1 iff the two 16-byte tags are equal, computed by the branch-free rule; 0 otherwise."""
    r = subprocess.run([BIN["gcm_tag_eq"]]+[str(b) for b in a16]+[str(b) for b in b16], capture_output=True, text=True, timeout=600)
    if r.returncode != 0 or r.stdout.strip() not in ("0", "1"):
        raise RuntimeError(f"gcm_tag_eq: rc={r.returncode} out={r.stdout!r} {r.stderr[-200:]}")
    return int(r.stdout)

def _pad16(b):
    """Host glue: zero-pad a VARIABLE-length byte string to a 16-byte boundary."""
    return bytes(b)+bytes((-len(b))%16)

def _gcm_tag_over(key16, iv12, seq, aad, C):
    """The 16-byte tag T for (aad, C) under (key, iv, seq). Every fixed-width
    step is a Verbose spawn; the host only pads and concatenates GHASH's input."""
    H   = _aes_block(key16, bytes(16))                                   # H = E_K(0^128)
    S   = _ghash(H, _pad16(aad)+_pad16(C)+gcm_lenblock(len(aad), len(C)))
    EJ0 = _aes_block(key16, gcm_j0(iv12, seq))                           # E_K(J0), J0 from Verbose
    return gcm_tag(S, EJ0)

def aead_encrypt(key16, iv12, seq, inner_plaintext, content_type=0x17):
    """TLS 1.3 record protect: returns the record (5-byte header + ct + tag)."""
    inner = bytes(inner_plaintext)+bytes([content_type])   # TLS framing: append the inner content type
    aad   = gcm_aad(len(inner))                            # the record header IS the additional data
    C     = _gctr(key16, gcm_nonce(iv12, seq), inner)
    return aad + C + _gcm_tag_over(key16, iv12, seq, aad, C)

def aead_decrypt(key16, iv12, seq, record):
    """Verify+decrypt a TLS 1.3 record; returns (inner_content_type, plaintext) or None."""
    aad=record[:5]; ct=record[5:-16]; tag=record[-16:]      # TLS framing: slice header / ct / tag
    if gcm_tag_eq(_gcm_tag_over(key16, iv12, seq, aad, ct), tag) != 1:
        return None                                        # the host branches on the VERDICT only
    plain=_gctr(key16, gcm_nonce(iv12, seq), ct)            # CTR is its own inverse
    # TLS framing: strip inner content type (last non-zero byte; TLS 1.3 may zero-pad)
    i=len(plain)-1
    while i>=0 and plain[i]==0: i-=1
    return (plain[i], bytes(plain[:i]))

ALL_RULES = [
    ("ladder","ladder_recursive.verbose"), ("x25519_finish","x25519_rec.verbose"),
    ("sha256_fold","sha256_fold.verbose"),
    ("handshake_secret","handshake_secret.verbose"),
    ("derive_s_hs_traffic","derive_secret.verbose"),
    ("derive_derived","tls_schedule.verbose"), ("master_secret","tls_schedule.verbose"),
    ("derive_c_hs_traffic","tls_schedule.verbose"), ("derive_s_ap_traffic","tls_schedule.verbose"),
    ("derive_c_ap_traffic","tls_schedule.verbose"), ("finished_key","tls_schedule.verbose"),
    ("expand_key","hkdf_expand_label.verbose"), ("expand_iv","hkdf_expand_label.verbose"),
    ("encrypt","aes_encrypt.verbose"), ("gctr","aes_gctr.verbose"), ("ghash_fold","ghash_nblocks.verbose"),
    ("hkdf_extract","hkdf_extract.verbose"),
    ("psk_early_secret","psk_schedule.verbose"), ("psk_ext_binder_key","psk_schedule.verbose"),
    ("gcm_nonce","gcm_frame.verbose"), ("gcm_j0","gcm_frame.verbose"), ("gcm_aad","gcm_frame.verbose"),
    ("gcm_lenblock","gcm_frame.verbose"), ("gcm_tag","gcm_frame.verbose"), ("gcm_tag_eq","gcm_frame.verbose"),
]

if __name__ == "__main__":
    import time, hashlib
    ensure(ALL_RULES)
    # 1) X25519 vs RFC 7748 vector 1
    t=time.time()
    out = x25519(bytes.fromhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"),
                 bytes.fromhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c"))
    assert out.hex()=="c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552", out.hex()
    t_x = time.time()-t
    # 2) SHA-256 vs hashlib — sha256_fold is ONE record spawn since 2026-08-29
    # (tranche 4): a recursive record-output rule folds every block and returns
    # all 32 digest bytes as a Digest JSON object. Vectors: empty (1 block),
    # "abc" (1 block, FIPS-180), and a 4-block message (exercises the recursion).
    t=time.time()
    assert sha256(b"")==hashlib.sha256(b"").digest()
    assert sha256(b"abc")==hashlib.sha256(b"abc").digest()
    assert sha256(bytes(range(200)))==hashlib.sha256(bytes(range(200))).digest()
    t_sha = time.time()-t
    # 3) full key schedule chain sanity (handshake_secret -> s_hs -> key/iv)
    import hmac as H
    def el(s,l,c,n): return H.new(s,n.to_bytes(2,'big')+bytes([len(b"tls13 "+l)])+b"tls13 "+l+bytes([len(c)])+c+b'\x01',hashlib.sha256).digest()[:n]
    ecdhe=bytes(range(32)); thash=bytes(range(32,64))
    t=time.time()
    hs=handshake_secret(ecdhe)
    shs=derive_s_hs(hs,thash)
    k=expand_key(shs); iv=expand_iv(shs)
    t_ks = time.time()-t
    early=H.new(b'\x00'*32,b'\x00'*32,hashlib.sha256).digest()
    der=el(early,b"derived",hashlib.sha256(b"").digest(),32)
    assert hs==H.new(der,ecdhe,hashlib.sha256).digest()
    assert shs==el(hs,b"s hs traffic",thash,32)
    assert k==el(shs,b"key",b"",16)
    assert iv==el(shs,b"iv",b"",12)
    # 3b) the six tls_schedule rules (record spawns since 2026-08-29, tranche 3):
    # derived -> master -> traffic secrets -> finished_key, all vs the oracle
    t=time.time()
    dd=derive_derived(hs)
    ms=master_secret(dd)
    chs=derive_c_hs(hs,thash)
    sap=derive_s_ap(ms,thash)
    cap_=derive_c_ap(ms,thash)
    fk=finished_key(shs)
    t_sc = time.time()-t
    assert dd==el(hs,b"derived",hashlib.sha256(b"").digest(),32)
    assert ms==H.new(dd,b'\x00'*32,hashlib.sha256).digest()
    assert chs==el(hs,b"c hs traffic",thash,32)
    assert sap==el(ms,b"s ap traffic",thash,32)
    assert cap_==el(ms,b"c ap traffic",thash,32)
    assert fk==el(shs,b"finished",b"",32)
    # 3c) HKDF-Extract + PSK schedule (record spawns since tranche 6, 2026-08-30 —
    # the LAST which-form rules; converting them is what deleted the thread pool).
    # hkdf_extract is the exact PSK-DHE Handshake Secret shape the servers use
    # (HMAC(derived, ECDHE)); the two psk rules are verify_binder.py's chain.
    PSK=bytes(range(32))
    t=time.time()
    ex=hkdf_extract(der, ecdhe)
    pe=psk_early_secret(PSK)
    bk=psk_ext_binder_key(pe)
    t_px = time.time()-t
    assert ex==hs                                                  # == HMAC(der, ecdhe), pinned above
    assert pe==H.new(b'\x00'*32, PSK, hashlib.sha256).digest()
    assert bk==el(pe, b"ext binder", hashlib.sha256(b"").digest(), 32)
    # 4) AES/GCM/GHASH vs the published NIST vectors — record spawns since
    # tranche 5 (2026-08-30). FIPS-197 Appendix C.1 through `encrypt` (one
    # CipherBlock spawn), NIST GCM Test Case 2 through `gctr` (one per-block
    # spawn) and through `ghash_fold` (one recursive-fold spawn).
    assert _aes_block(bytes.fromhex("000102030405060708090a0b0c0d0e0f"),
                      bytes.fromhex("00112233445566778899aabbccddeeff")) \
        == bytes.fromhex("69c4e0d86a7b0430d8cdb78070b4c55a")          # FIPS-197 C.1
    assert _gctr(bytes(16), bytes(12), bytes(16)) \
        == bytes.fromhex("0388dace60b6a392f328c2b971b2fe78")          # GCM TC2 C
    _h_tc2=_aes_block(bytes(16), bytes(16))
    assert _h_tc2==bytes.fromhex("66e94bd4ef8a2c3b884cfa59ca342b2e")  # GCM TC2 H
    assert _ghash(_h_tc2, bytes.fromhex("0388dace60b6a392f328c2b971b2fe78")
                          +(0).to_bytes(8,'big')+(128).to_bytes(8,'big')) \
        == bytes.fromhex("f38cbb1ad69223dcc3457ae5b6b0f885")          # GCM TC2 GHASH
    # 4b) the AEAD FRAMING rules (examples/gcm_frame.verbose, 2026-09-02) vs the
    # PUBLISHED NIST SP 800-38D intermediates and RFC 8446: J0 must be TC2's Y0,
    # the tag must be S XOR E(K,Y0) on TC2 AND TC4, the length block must be
    # TC4's ([len(A)]64 || [len(C)]64 for 20-byte AAD / 60-byte C), the nonce
    # must be IV XOR seq right-aligned, and the AAD the TLS record header.
    t=time.time()
    assert gcm_nonce(bytes.fromhex("cafebabefacedbaddecaf888"), 0x0102030405060708) \
        == bytes.fromhex("cafebabefbccd8a9dbccff80")                  # RFC 8446 5.3
    assert gcm_j0(bytes(12), 0) == bytes(15)+b"\x01"                  # GCM TC2 Y0
    assert gcm_aad(12) == bytes.fromhex("170303001c")                 # RFC 8446 5.2, len(inner)=12 -> 28
    assert gcm_lenblock(20, 60) == bytes.fromhex("00000000000000a000000000000001e0")   # GCM TC4
    assert gcm_tag(bytes.fromhex("f38cbb1ad69223dcc3457ae5b6b0f885"),
                   bytes.fromhex("58e2fccefa7e3061367f1d57a4e7455a")) \
        == bytes.fromhex("ab6e47d42cec13bdf53a67b21257bddf")          # GCM TC2 T = S xor E(K,Y0)
    _t4=gcm_tag(bytes.fromhex("698e57f70e6ecc7fd9463b7260a9ae5f"),
                bytes.fromhex("3247184b3c4f69a44dbcd22887bbb418"))
    assert _t4 == bytes.fromhex("5bc94fbc3221a5db94fae95ae7121a47")   # GCM TC4 T
    assert gcm_tag_eq(_t4, _t4) == 1
    assert gcm_tag_eq(_t4, bytes([_t4[0]^0x80])+_t4[1:]) == 0         # single-bit flip -> 0 (negative control)
    assert gcm_tag_eq(_t4, _t4[:15]+bytes([_t4[15]^0x01])) == 0
    t_fr = time.time()-t
    # 5) AEAD record round-trip (encrypt then decrypt), timed, plus tamper rejection
    rk=bytes(range(1,17)); riv=bytes(range(17,29))
    t=time.time()
    rec=aead_encrypt(rk,riv,0,b"hello world",0x17)
    ct,pt=aead_decrypt(rk,riv,0,rec)
    t_ae = time.time()-t
    assert ct==0x17 and pt==b"hello world", (ct,pt)
    _bad=bytearray(rec); _bad[-1]^=1                                    # the ONLY xor left in this file
    assert aead_decrypt(rk,riv,0,bytes(_bad)) is None
    assert aead_decrypt(rk,riv,1,rec) is None                          # wrong seq -> wrong nonce -> reject
    print(f"VCRYPTO_OK  x25519={t_x:.3f}s  keysched={t_ks:.3f}s  sched={t_sc:.3f}s  sha256={t_sha:.3f}s  extract_psk={t_px:.3f}s  framing={t_fr:.3f}s  aead={t_ae:.3f}s  aead_roundtrip=ok  (every TLS primitive AND the AEAD framing: ONE record spawn of a verified binary; the which-era thread pool is DELETED)")
