"""vcrypto: TLS 1.3 cryptographic layer driven by the pure-Verbose binaries.

Every cryptographic transformation here is computed by Verbose-emitted machine
code (compiled from examples/*.verbose). The host (this file) only spawns those
binaries and shuttles bytes. To beat the one-byte-per-process-run cost, every
`which`-loop is spawned IN PARALLEL (all output bytes concurrently), which turns
the ladder's per-limb cost from sum into max — making a handshake tractable
without any change to the native backend.

Honest scope (per docs/tls-io-statemachine-design.md §7): the cryptographic
PRIMITIVES (X25519, key schedule, SHA-256, AES/GCM/GHASH) are pure Verbose.
Byte repacking (bytes<->limbs), AEAD framing (nonce/AAD/J0/tag-XOR), and
randomness are host glue, clearly separated below.
"""
import subprocess, os, sys, concurrent.futures as cf

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = {}
_POOL = cf.ThreadPoolExecutor(max_workers=64)

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

def _one(binp, args, w):
    r = subprocess.run([binp]+args+[str(w)], capture_output=True, text=True, timeout=600)
    s = r.stdout.strip()
    if s == "": raise RuntimeError(f"{binp} which={w} empty (rc={r.returncode}) {r.stderr[-200:]}")
    return int(s)

def run_bytes(rule, args, n):
    """Spawn all n `which` values in parallel; return bytes."""
    binp = BIN[rule]
    futs = {w: _POOL.submit(_one, binp, args, w) for w in range(n)}
    return bytes(futs[w].result() for w in range(n))

# ---- byte<->limb repacking (host glue: deterministic format conversion) ----
OFF = [0,26,51,77,102,128,153,179,204,230]; W=[26,25,26,25,26,25,26,25,26,25]
def to_limbs(x):
    x &= (1<<255)-1
    return [(x>>OFF[i]) & ((1<<W[i])-1) for i in range(10)]

# ---- X25519 (pure Verbose: ladder + finish) ----
def x25519(scalar32: bytes, u32: bytes) -> bytes:
    u_int = int.from_bytes(u32,'little') & ((1<<255)-1)
    ul = to_limbs(u_int)
    init = to_limbs(1)+to_limbs(0)+list(ul)+to_limbs(1)+list(ul)
    sc_hex = scalar32.hex()
    ladder_args = [str(v) for v in init] + ["0","255","__W__", sc_hex]
    # ladder returns 20 limbs (x2|z2); run which 0..19 in parallel
    binp = BIN["ladder"]
    def lad(w):
        a = [str(v) for v in init] + ["0","255",str(w),sc_hex]
        return _one(binp, a, w) if False else int(subprocess.run([binp]+a,capture_output=True,text=True,timeout=600).stdout.strip())
    futs = {w:_POOL.submit(lad,w) for w in range(20)}
    limbs = [futs[w].result() for w in range(20)]
    x2 = limbs[0:10]; z2 = limbs[10:20]
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
    return run_bytes("x25519_finish", fin_state, 32)

# ---- SHA-256 (pure Verbose) of arbitrary bytes ----
H0=[0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19]
def sha256(msg: bytes) -> bytes:
    padded=bytearray(msg); L=len(msg); padded.append(0x80)
    while len(padded)%64!=56: padded.append(0)
    padded += (L*8).to_bytes(8,'big')
    nb=len(padded)//64
    args=[str(w) for w in H0]+[str(nb),str(nb)]
    binp=BIN["sha256_fold"]; hexd=bytes(padded).hex()
    def one(w): return int(subprocess.run([binp]+args+[str(w),hexd],capture_output=True,text=True,timeout=600).stdout.strip())
    futs={w:_POOL.submit(one,w) for w in range(32)}
    return bytes(futs[w].result() for w in range(32))

# ---- key schedule (pure Verbose) ----
def _sched(rule, secret32, thash32):
    args=[str(b) for b in secret32]+[str(b) for b in thash32]
    return run_bytes(rule, args, 32)
def handshake_secret(ecdhe32): return run_bytes("handshake_secret",[str(b) for b in ecdhe32],32)
def derive_derived(secret32): return _sched("derive_derived", secret32, bytes(32))
def master_secret(derived32): return _sched("master_secret", derived32, bytes(32))
def derive_s_hs(secret32, thash32): return _sched("derive_s_hs_traffic", secret32, thash32)
def derive_c_hs(secret32, thash32): return _sched("derive_c_hs_traffic", secret32, thash32)
def derive_s_ap(secret32, thash32): return _sched("derive_s_ap_traffic", secret32, thash32)
def derive_c_ap(secret32, thash32): return _sched("derive_c_ap_traffic", secret32, thash32)
def finished_key(secret32): return _sched("finished_key", secret32, bytes(32))
def expand_key(secret32): return run_bytes("expand_key",[str(b) for b in secret32],16)
def expand_iv(secret32):  return run_bytes("expand_iv",[str(b) for b in secret32],12)

# ---- AES-GCM AEAD record protection (primitives pure Verbose; framing host) ----
def _aes_block(key16, block16):
    return run_bytes("encrypt", [str(b) for b in block16]+[str(b) for b in key16], 16)
def _gctr(key16, nonce12, data):
    nb=(len(data)+15)//16
    if nb==0: return b""
    args=[str(b) for b in key16]+[str(b) for b in nonce12]+[str(nb)]
    binp=BIN["gctr"]; hexd=bytes(data).hex()
    def one(w): return int(subprocess.run([binp]+args+[str(w),hexd],capture_output=True,text=True,timeout=600).stdout.strip())
    futs={w:_POOL.submit(one,w) for w in range(len(data))}
    return bytes(futs[w].result() for w in range(len(data)))
def _ghash(h16, data):
    nb=len(data)//16
    args=[str(b) for b in [0]*16]+[str(b) for b in h16]+[str(nb),str(nb)]
    binp=BIN["ghash_fold"]; hexd=bytes(data).hex()
    def one(w): return int(subprocess.run([binp]+args+[str(w),hexd],capture_output=True,text=True,timeout=600).stdout.strip())
    futs={w:_POOL.submit(one,w) for w in range(16)}
    return bytes(futs[w].result() for w in range(16))

def _gcm(key16, nonce12, pt, aad):
    H=_aes_block(key16, [0]*16)
    C=_gctr(key16, nonce12, pt)
    def pad(b): return bytes(b)+bytes((-len(b))%16)
    lenb=(len(aad)*8).to_bytes(8,'big')+(len(C)*8).to_bytes(8,'big')
    S=_ghash(H, pad(aad)+pad(C)+lenb)
    EJ0=_aes_block(key16, list(nonce12)+[0,0,0,1])
    tag=bytes(S[i]^EJ0[i] for i in range(16))
    return bytes(C), tag

def _nonce(iv12, seq):
    n=bytearray(iv12); sb=seq.to_bytes(8,'big')
    for j in range(8): n[4+j]^=sb[j]
    return bytes(n)

def aead_encrypt(key16, iv12, seq, inner_plaintext, content_type=0x17):
    """TLS 1.3 record protect: returns the record (5-byte header + ct + tag)."""
    inner=bytes(inner_plaintext)+bytes([content_type])
    length=len(inner)+16
    aad=bytes([0x17,0x03,0x03,(length>>8)&0xff,length&0xff])
    C,tag=_gcm(key16, _nonce(iv12,seq), inner, aad)
    return aad+C+tag

def aead_decrypt(key16, iv12, seq, record):
    """Verify+decrypt a TLS 1.3 record; returns (inner_content_type, plaintext) or None."""
    aad=record[:5]; ct=record[5:-16]; tag=record[-16:]
    C,exp=_gcm(key16, _nonce(iv12,seq), ct, aad)  # note: decrypt keystream == encrypt keystream
    # recompute tag over received ct
    H=_aes_block(key16,[0]*16)
    def pad(b): return bytes(b)+bytes((-len(b))%16)
    lenb=(len(aad)*8).to_bytes(8,'big')+(len(ct)*8).to_bytes(8,'big')
    S=_ghash(H, pad(aad)+pad(ct)+lenb)
    EJ0=_aes_block(key16, list(_nonce(iv12,seq))+[0,0,0,1])
    calc=bytes(S[i]^EJ0[i] for i in range(16))
    if calc!=tag: return None
    plain=_gctr(key16, _nonce(iv12,seq), ct)  # CTR is its own inverse
    # strip inner content type (last non-zero byte; TLS1.3 may zero-pad)
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
    # 2) SHA-256 vs hashlib
    assert sha256(b"abc")==hashlib.sha256(b"abc").digest()
    # 3) full key schedule chain sanity (handshake_secret -> s_hs -> key/iv)
    import hmac as H
    def el(s,l,c,n): return H.new(s,n.to_bytes(2,'big')+bytes([len(b"tls13 "+l)])+b"tls13 "+l+bytes([len(c)])+c+b'\x01',hashlib.sha256).digest()[:n]
    ecdhe=bytes(range(32)); thash=bytes(range(32,64))
    hs=handshake_secret(ecdhe)
    early=H.new(b'\x00'*32,b'\x00'*32,hashlib.sha256).digest()
    der=el(early,b"derived",hashlib.sha256(b"").digest(),32)
    assert hs==H.new(der,ecdhe,hashlib.sha256).digest()
    shs=derive_s_hs(hs,thash); assert shs==el(hs,b"s hs traffic",thash,32)
    assert expand_key(shs)==el(shs,b"key",b"",16)
    assert expand_iv(shs)==el(shs,b"iv",b"",12)
    # 4) AEAD record round-trip (encrypt then decrypt)
    rk=bytes(range(1,17)); riv=bytes(range(17,29))
    rec=aead_encrypt(rk,riv,0,b"hello world",0x17)
    ct,pt=aead_decrypt(rk,riv,0,rec)
    assert ct==0x17 and pt==b"hello world", (ct,pt)
    print(f"VCRYPTO_OK  x25519={t_x:.1f}s  aead_roundtrip=ok  (parallel which-spawn)")
