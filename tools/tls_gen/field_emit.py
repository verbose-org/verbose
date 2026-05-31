"""GF(2^255-19) field arithmetic emit helpers (10-limb 26/25-bit), reusable by
the X25519 and Ed25519 Verbose generators. Mirrors the committed
examples/field_mul.verbose algorithm exactly (validated vs big-int mod p).

A field element = 10 named i64 limbs, alternating 26/25-bit widths, non-negative.
emit_* helpers append (name, expr) lets to a list and return the 10 result names.
"""
P = (1 << 255) - 19
W = [26, 25, 26, 25, 26, 25, 26, 25, 26, 25]
OFF = [0, 26, 51, 77, 102, 128, 153, 179, 204, 230]
MASK = [(1 << w) - 1 for w in W]
TWO_P = [((1 << 27) - 38)] + [((1 << (W[i] + 1)) - 2) for i in range(1, 10)]

def to_limbs(x):
    x %= P
    return [(x >> OFF[i]) & MASK[i] for i in range(10)]

def from_limbs(l):
    return sum(int(l[i]) << OFF[i] for i in range(10)) % P

def emit_freduce(lets, prefix, acc_exprs):
    a = []
    for i in range(10):
        nm = f"{prefix}_a{i}"; lets.append((nm, acc_exprs[i])); a.append(nm)
    for p in range(2):
        for i in range(9):
            c = f"{prefix}_c{p}_{i}"; lets.append((c, f"shr({a[i]}, {W[i]})"))
            lo = f"{prefix}_lo{p}_{i}"; lets.append((lo, f"band({a[i]}, {MASK[i]})")); a[i] = lo
            nx = f"{prefix}_s{p}_{i}"; lets.append((nx, f"{a[i+1]} + {c}")); a[i+1] = nx
        c9 = f"{prefix}_c{p}_9"; lets.append((c9, f"shr({a[9]}, {W[9]})"))
        lo9 = f"{prefix}_lo{p}_9"; lets.append((lo9, f"band({a[9]}, {MASK[9]})")); a[9] = lo9
        w0 = f"{prefix}_w{p}"; lets.append((w0, f"{a[0]} + 19 * {c9}")); a[0] = w0
    return a

def emit_fadd(lets, prefix, A, B):
    return emit_freduce(lets, prefix, [f"{A[i]} + {B[i]}" for i in range(10)])

def emit_fsub(lets, prefix, A, B):
    return emit_freduce(lets, prefix, [f"{A[i]} + {TWO_P[i]} - {B[i]}" for i in range(10)])

def emit_fmul(lets, prefix, A, B):
    terms = [[] for _ in range(10)]
    for i in range(10):
        for j in range(10):
            both = (i % 2 == 1 and j % 2 == 1)
            k = i + j
            prod = f"({A[i]} * {B[j]})"
            if k < 10:
                coef, tgt = (2 if both else 1), k
            else:
                coef, tgt = (38 if both else 19), k - 10
            terms[tgt].append(prod if coef == 1 else f"{coef} * {prod}")
    return emit_freduce(lets, prefix, [" + ".join(t) for t in terms])

# Python field ops (oracle for the emitted rules)
def freduce(acc):
    a = list(acc)
    for _ in range(2):
        for i in range(9):
            c = a[i] >> W[i]; a[i] &= MASK[i]; a[i+1] += c
        c = a[9] >> W[9]; a[9] &= MASK[9]; a[0] += 19 * c
    return a
def fadd(a, b): return freduce([a[i]+b[i] for i in range(10)])
def fsub(a, b): return freduce([a[i]+TWO_P[i]-b[i] for i in range(10)])
def fmul(a, b):
    acc=[0]*10
    for i in range(10):
        for j in range(10):
            pr=a[i]*b[j]
            if i%2==1 and j%2==1: pr*=2
            k=i+j
            if k<10: acc[k]+=pr
            else: acc[k-10]+=19*pr
    return freduce(acc)

if __name__ == "__main__":
    import random, sys
    random.seed(1); bad=0
    for _ in range(5000):
        x=random.randrange(P); y=random.randrange(P)
        if from_limbs(fmul(to_limbs(x),to_limbs(y)))!=(x*y)%P: bad+=1
        if from_limbs(fadd(to_limbs(x),to_limbs(y)))!=(x+y)%P: bad+=1
        if from_limbs(fsub(to_limbs(x),to_limbs(y)))!=(x-y)%P: bad+=1
    print("FIELD_EMIT_OK" if bad==0 else f"{bad} FAIL")
    sys.exit(0 if bad==0 else 1)

# ---- Fermat inverse z^(p-2) and little-endian byte encode (Ed25519 brick 4) ----
def emit_finv(lets, pfx, z):
    """Emit z^(p-2) mod p via the curve25519 addition chain. Returns 10 limbs."""
    def sq(n, a): return emit_fmul(lets, f"{pfx}_{n}", a, a)
    def mul(n, a, b): return emit_fmul(lets, f"{pfx}_{n}", a, b)
    z2 = sq("z2", z)
    t = sq("t0", z2); t = sq("t0b", t)        # z^8
    z9 = mul("z9", t, z)
    z11 = mul("z11", z9, z2)
    t = sq("t1", z11)
    z2_5_0 = mul("z2_5_0", t, z9)
    t = sq("t2", z2_5_0)
    for k in range(1,5): t = sq(f"t2_{k}", t)
    z2_10_0 = mul("z2_10_0", t, z2_5_0)
    t = sq("t3", z2_10_0)
    for k in range(1,10): t = sq(f"t3_{k}", t)
    z2_20_0 = mul("z2_20_0", t, z2_10_0)
    t = sq("t4", z2_20_0)
    for k in range(1,20): t = sq(f"t4_{k}", t)
    t = mul("t4m", t, z2_20_0)
    t = sq("t5", t)
    for k in range(1,10): t = sq(f"t5_{k}", t)
    z2_50_0 = mul("z2_50_0", t, z2_10_0)
    t = sq("t6", z2_50_0)
    for k in range(1,50): t = sq(f"t6_{k}", t)
    z2_100_0 = mul("z2_100_0", t, z2_50_0)
    t = sq("t7", z2_100_0)
    for k in range(1,100): t = sq(f"t7_{k}", t)
    t = mul("t7m", t, z2_100_0)
    t = sq("t8", t)
    for k in range(1,50): t = sq(f"t8_{k}", t)
    t = mul("t8m", t, z2_50_0)
    t = sq("t9", t)
    for k in range(1,5): t = sq(f"t9_{k}", t)
    return mul("zinv", t, z11)

# little-endian byte b (0..31) gathers from limbs overlapping [8b,8b+8)
def _enc_terms(bi):
    terms=[]
    lo,hi=8*bi,8*bi+8
    for i in range(10):
        a,c=OFF[i],OFF[i]+W[i]
        if a<hi and c>lo:
            terms.append((i, OFF[i]-8*bi))
    return terms

def emit_encode(lets, pfx, fe):
    """fe: 10 reduced limb names. Returns 32 byte-name lets (little-endian)."""
    out=[]
    for bi in range(32):
        parts=[]
        for (i,local) in _enc_terms(bi):
            parts.append(f"shl({fe[i]}, {local})" if local>0 else (fe[i] if local==0 else f"shr({fe[i]}, {-local})"))
        e=parts[0]
        for p_ in parts[1:]: e=f"bor({e}, {p_})"
        nm=f"{pfx}_ob{bi}"; lets.append((nm, f"band({e}, 255)")); out.append(nm)
    return out

# Python oracles
def finv(z): return pow(z, P-2, P)
def encode_field(x):  # field int -> 32 LE bytes
    return list((x % P).to_bytes(32,'little'))
