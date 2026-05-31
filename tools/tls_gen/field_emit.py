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
