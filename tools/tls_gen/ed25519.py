"""Ed25519 sign + public key, with ALL cryptography on the Verbose-emitted
binaries (SHA-512, ed_scalarmult, ed_affine, sc_reduce, sc_muladd). The host
does only byte plumbing (clamp, concat, point packing) — the acknowledged
host-glue category, same as TLS record framing. Validated vs RFC 8032.
"""
import sys, os, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from field_emit import to_limbs, from_limbs, P
from ed25519_ref import B as B_REF
import concurrent.futures as cf

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = {}
RULES = {
    "sha512_fold": "sha512_fold.verbose",
    "ed_scalarmult": "ed_scalarmult.verbose",
    "ed_affine": "ed_affine.verbose",
    "sc_reduce": "sc_reduce.verbose",
    "sc_muladd": "sc_muladd.verbose",
}
def _compile(rule):
    out = f"/tmp/v_{rule}"
    if rule in BIN and os.path.exists(out): return out
    subprocess.run(["cargo","run","--release","--", os.path.join("examples",RULES[rule]),
                    "--native",out,"--run",rule], cwd=ROOT, capture_output=True, text=True)
    if not os.path.exists(out): raise RuntimeError(f"compile {rule} failed")
    BIN[rule]=out; return out
def ensure():
    for r in RULES: _compile(r)

def _parmap(binp, fixed_args, n, tail=()):
    def one(w):
        a=[binp]+[str(x) for x in fixed_args]+[str(w)]+[str(t) for t in tail]
        return int(subprocess.run(a, capture_output=True, text=True, timeout=900).stdout.strip())
    with cf.ThreadPoolExecutor(max_workers=64) as ex:
        return bytes(ex.map(one, range(n)))

# --- SHA-512 (Verbose) over arbitrary bytes ---
H0_512=[0x6a09e667f3bcc908,0xbb67ae8584caa73b,0x3c6ef372fe94f82b,0xa54ff53a5f1d36f1,
        0x510e527fade682d1,0x9b05688c2b3e6c1f,0x1f83d9abfb41bd6b,0x5be0cd19137e2179]
def _sgn(v): return v if v<(1<<63) else v-(1<<64)
def sha512(msg: bytes) -> bytes:
    p=bytearray(msg); Ln=len(msg); p.append(0x80)
    while len(p)%128!=112: p.append(0)
    p+=(Ln*8).to_bytes(16,'big')
    nb=len(p)//128; hexd=bytes(p).hex()
    fixed=[_sgn(w) for w in H0_512]+[nb,nb]
    return _parmap(BIN["sha512_fold"], fixed, 64, tail=(hexd,))

# --- [scalar]B then encode to 32 bytes ---
def _neutral(): return to_limbs(0)+to_limbs(1)+to_limbs(1)+to_limbs(0)
def _Blimbs(): return to_limbs(B_REF[0])+to_limbs(B_REF[1])+to_limbs(B_REF[2])+to_limbs(B_REF[3])
def scalarmult_base_encode(scalar32: bytes) -> bytes:
    sc=scalar32.hex()
    fixed=_neutral()+_Blimbs()+[256]
    limbs=[int(x) for x in _parmap_ints(BIN["ed_scalarmult"], fixed, 40, tail=(sc,))]
    xyz=limbs[0:30]
    aff=[int(x) for x in _parmap_ints(BIN["ed_affine"], xyz, 20)]
    x=from_limbs(aff[0:10]); y=from_limbs(aff[10:20])
    return (y | ((x & 1) << 255)).to_bytes(32,'little')

def _parmap_ints(binp, fixed_args, n, tail=()):
    def one(w):
        a=[binp]+[str(x) for x in fixed_args]+[str(w)]+[str(t) for t in tail]
        return int(subprocess.run(a, capture_output=True, text=True, timeout=900).stdout.strip())
    with cf.ThreadPoolExecutor(max_workers=64) as ex:
        return list(ex.map(one, range(n)))

def sc_reduce(b64: bytes) -> bytes:
    return _parmap(BIN["sc_reduce"], list(b64), 32)
def sc_muladd(a32: bytes, b32: bytes, c32: bytes) -> bytes:
    return _parmap(BIN["sc_muladd"], list(a32)+list(b32)+list(c32), 32)
def clamp(h32: bytes) -> bytes:
    a=bytearray(h32); a[0]&=248; a[31]&=127; a[31]|=64; return bytes(a)

def public_key(sk: bytes) -> bytes:
    h=sha512(sk); a=clamp(h[:32])
    return scalarmult_base_encode(a)
def sign(sk: bytes, msg: bytes) -> bytes:
    h=sha512(sk); a=clamp(h[:32]); prefix=h[32:]
    A=scalarmult_base_encode(a)
    r=sc_reduce(sha512(prefix+msg))
    R=scalarmult_base_encode(r)
    k=sc_reduce(sha512(R+A+msg))
    S=sc_muladd(k, a, r)
    return R+S

if __name__=="__main__":
    ensure()
    sk=bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
    exp_pub=bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    exp_sig=bytes.fromhex("e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b")
    pub=public_key(sk); sig=sign(sk, b"")
    print("pub", "OK" if pub==exp_pub else "FAIL", pub.hex()[:16])
    print("sig", "OK" if sig==exp_sig else "FAIL", sig.hex()[:16])
    import sys as _s; _s.exit(0 if pub==exp_pub and sig==exp_sig else 1)
