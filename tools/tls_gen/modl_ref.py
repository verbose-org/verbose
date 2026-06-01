"""mod-L (Ed25519 group order) arithmetic, designed for a clean Verbose port.

L = 2^252 + d0,  d0 = 27742317777372353535851937790883648493  (~2^125).
So 2^252 ≡ -d0 (mod L). Reduction folds high bits down by multiplying by -d0.

We use a fold approach validated against Python's exact %L:
  reduce(v): repeatedly v = (v & (2^252-1)) - d0*(v >> 252) until v < 2^253,
             then conditionally add/sub L into [0,L).
  muladd(a,b,c) = reduce_full(a*b + c).

These are big-int here (the oracle). The Verbose port will represent the
operands in limbs; this file pins the algorithm + provides the oracle.
"""
L = 2**252 + 27742317777372353535851937790883648493
D0 = 27742317777372353535851937790883648493
MASK252 = (1 << 252) - 1

def reduce_full(v):
    # bring any non-negative v (up to ~2^512) into [0, L) using 2^252 ≡ -d0
    while v >> 252:
        v = (v & MASK252) - D0 * (v >> 252)
    # v can be negative now; normalize
    v %= L
    return v

def muladd(a, b, c):
    return reduce_full(a * b + c)

def reduce_bytes_le(b):
    return reduce_full(int.from_bytes(b, "little"))

if __name__ == "__main__":
    import random, sys
    random.seed(3); bad = 0
    for _ in range(20000):
        v = random.randrange(0, 1 << 512)
        if reduce_full(v) != v % L: bad += 1
    for _ in range(20000):
        a = random.randrange(0, L); b = random.randrange(0, 1 << 255); c = random.randrange(0, L)
        if muladd(a, b, c) != (a * b + c) % L: bad += 1
    # 64-byte SHA-style reductions
    for _ in range(5000):
        bb = random.randbytes(64)
        if reduce_bytes_le(bb) != int.from_bytes(bb, "little") % L: bad += 1
    print("MODL_OK" if bad == 0 else f"{bad} FAIL")
    sys.exit(0 if bad == 0 else 1)
