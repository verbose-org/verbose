"""Z_n scalar-field arithmetic for P-256 (Montgomery CIOS, 10-limb radix-2^26) —
BRICK 4 of the ECDSA-over-P-256 Verbose arc. This is the SAME machinery as
brick 1 (tools/tls_gen/p256_field.py), with the modulus swapped from the
coordinate field prime p to the GROUP ORDER n, and the Montgomery constants
(N limbs, n0inv, R^2 mod n) recomputed for n. ECDSA's signature equation
    s = k^-1 * (z + r*d)  mod n
lives entirely in this scalar field. r is reduced mod n from a coordinate
(in [0, p)); z is the truncated hash reduced mod n; d, k, s are in [1, n-1].

P-256 group order (a DIFFERENT 256-bit prime than the coordinate field p):
    n = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551
n is prime, so by Fermat  k^-1 mod n = k^(n-2) mod n  — exactly brick 1's finv
but with exponent n-2 and modulus n.

FACTORING DECISION (sibling module, not parameterization):
  p256_field.py bakes p's constants as MODULE-LEVEL globals (P, NMOD, N0INV, RR)
  and every helper reads them directly. Parameterizing those into every function
  signature would touch brick 1's audited, frozen code (consumed UNCHANGED by
  bricks 2/3). Instead this module is a faithful COPY of brick 1's CORE +
  emit helpers with the four modulus-dependent constants recomputed for n. The
  CIOS structure, the 52-bit i64 hot-accumulator bound, the radix (2^26), the
  shift discipline, the branch-free conditional subtract, the borrow-then-add
  fsub discipline, and the big-endian encode/decode are BIT-IDENTICAL to brick 1
  — only P -> N, NMOD, N0INV, RR change. The names are *n-suffixed (nmul/nadd/
  nsub/ninv) so a program can import both fields side by side without collision.

WHY THE SAME CIOS, NOT A SPECIAL n-REDUCTION:
  n is also a Solinas-shaped prime (0xFFFFFFFF00000000FFFFFFFF... high half),
  so a fast Solinas fold for n carries the same sign-change / non-limb-aligned
  hazards brick 1 documented for p. Montgomery CIOS is modulus-agnostic: the
  only thing that changes is the constant array N and the word inverse n0inv.
  Reuse the proven shape; re-derive only the constants. The constants are
  computed here with big-int at module-build time and ASSERTED, then the limb
  core is big-int free (validated against big-int %n in __main__).
"""

R_BITS = 26
N_LIMBS = 10
B = 1 << R_BITS          # 67108864
MASK = B - 1             # 67108863

# P-256 group order n (the scalar-field modulus). DIFFERENT prime than brick 1's p.
N = 0xFFFFFFFF00000000FFFFFFFFFFFFFFFFBCE6FAADA7179E84F3B9CAC2FC632551

R = B ** N_LIMBS         # 2^260, the Montgomery radix (same as brick 1)
RR = (R * R) % N         # R^2 mod n, the to-Montgomery constant (n-specific)
# Modulus limbs (little-endian by 26-bit limb)
NMOD = [(N >> (R_BITS * i)) & MASK for i in range(N_LIMBS)]
# n0inv = -N[0]^{-1} mod B (n-specific word inverse)
N0INV = (-pow(NMOD[0], -1, B)) % B

# --- ASSERT the recomputed n-specific Montgomery constants (build-time, big-int) ---
assert sum(NMOD[i] << (R_BITS * i) for i in range(N_LIMBS)) == N, "NMOD limbs != n"
assert N.bit_length() == 256, "n must be a 256-bit modulus"
# n0inv: N[0] * n0inv == -1 (mod B), i.e. (N[0]*n0inv + 1) % B == 0
assert (NMOD[0] * N0INV + 1) % B == 0, "n0inv is not -N[0]^{-1} mod 2^26"
# R^2 mod n: confirm RR really is R*R mod n and lies in [0, n)
assert RR == (R * R) % N and 0 <= RR < N, "RR != R^2 mod n"
# Sanity: n is odd (required for Montgomery; word inverse exists iff gcd(N[0],B)=1)
assert N & 1 == 1, "n must be odd for Montgomery"


# ---------------------------------------------------------------------------
# Domain conversion (big-int only here; the arithmetic CORE below is big-int free)
# ---------------------------------------------------------------------------
def _int_to_limbs(x):
    x %= N
    return [(x >> (R_BITS * i)) & MASK for i in range(N_LIMBS)]

def _limbs_to_int(l):
    return sum(int(l[i]) << (R_BITS * i) for i in range(N_LIMBS)) % N

def to_limbs_n(x):
    """Scalar int -> Montgomery-domain limbs (x*R mod n)."""
    return _int_to_limbs((x % N) * R % N)

def from_limbs_n(l):
    """Montgomery-domain limbs -> scalar int (value*R^{-1} mod n)."""
    return _limbs_to_int(l) * pow(R, -1, N) % N


# ---------------------------------------------------------------------------
# PURE-PYTHON references (per-limb int ops only — NO big-int in the core).
# Bit-identical structure to brick 1's core, NMOD/N0INV are the n versions.
# ---------------------------------------------------------------------------
def cond_sub_n(t):
    """Branch-free conditional subtract of n from an 11-limb value t (limbs in
    [0,B), t < 2n). Returns 10 reduced limbs. Big-int free."""
    diff = [0] * (N_LIMBS + 1)
    borrow = 0
    for i in range(N_LIMBS):
        d = t[i] - NMOD[i] - borrow
        if d < 0:
            d += B
            borrow = 1
        else:
            borrow = 0
        diff[i] = d & MASK
    d = t[N_LIMBS] - borrow
    if d < 0:
        d += B
        borrow = 1
    else:
        borrow = 0
    diff[N_LIMBS] = d & MASK
    mask_keep_t = -borrow            # all-ones if borrow==1 (t<n) else 0
    out = [0] * N_LIMBS
    for i in range(N_LIMBS):
        out[i] = (t[i] & mask_keep_t) | (diff[i] & (~mask_keep_t & MASK))
    return out

def mont_mul(a, b):
    """CIOS Montgomery multiply mod n: returns limbs of (a*b*R^{-1}) mod n.
    Per-limb int ops only (no big-int)."""
    t = [0] * (N_LIMBS + 2)
    for i in range(N_LIMBS):
        C = 0
        for j in range(N_LIMBS):
            v = t[j] + a[i] * b[j] + C
            t[j] = v & MASK
            C = v >> R_BITS
        v = t[N_LIMBS] + C
        t[N_LIMBS] = v & MASK
        t[N_LIMBS + 1] = v >> R_BITS
        m = (t[0] * N0INV) & MASK
        C = (t[0] + m * NMOD[0]) >> R_BITS
        for j in range(1, N_LIMBS):
            v = t[j] + m * NMOD[j] + C
            t[j - 1] = v & MASK
            C = v >> R_BITS
        v = t[N_LIMBS] + C
        t[N_LIMBS - 1] = v & MASK
        t[N_LIMBS] = t[N_LIMBS + 1] + (v >> R_BITS)
    return cond_sub_n(t[:N_LIMBS + 1])

def nreduce(acc):
    """Carry-normalize an 11-limb non-negative accumulator then conditional-sub n.
    Inputs are Montgomery residues < n, so sum/biased-diff < 2n and one
    cond_sub suffices after carry propagation."""
    t = list(acc) + [0] * (N_LIMBS + 1 - len(acc))
    C = 0
    for i in range(N_LIMBS):
        v = t[i] + C
        t[i] = v & MASK
        C = v >> R_BITS
    t[N_LIMBS] = t[N_LIMBS] + C if len(t) > N_LIMBS else C
    return cond_sub_n(t[:N_LIMBS + 1])

def nadd(a, b):
    """(a + b) mod n, both Montgomery residues in [0,n)."""
    return nreduce([a[i] + b[i] for i in range(N_LIMBS)] + [0])

def nsub(a, b):
    """(a - b) mod n. Borrow-subtract then branch-free conditional add n."""
    d = [0] * N_LIMBS
    borrow = 0
    for i in range(N_LIMBS):
        x = a[i] - b[i] - borrow
        if x < 0:
            x += B
            borrow = 1
        else:
            borrow = 0
        d[i] = x & MASK
    add_n = NMOD if borrow else [0] * N_LIMBS
    out = [0] * N_LIMBS
    carry = 0
    for i in range(N_LIMBS):
        x = d[i] + add_n[i] + carry
        out[i] = x & MASK
        carry = x >> R_BITS
    return out

def nmul(a, b):
    return mont_mul(a, b)

def nsqr(a):
    return mont_mul(a, a)

def none_mont():
    """1 in Montgomery domain = R mod n."""
    return to_limbs_n(1)


# ---------------------------------------------------------------------------
# reduce_mod_n: reduce a value < 2^256 (an x-coordinate in [0,p), or a 256-bit
# truncated hash) mod n. BRANCH-FREE, bounded number of conditional subtracts.
#
# BOUND ARGUMENT (how many conditional subtracts of n suffice):
#   The input v satisfies 0 <= v < 2^256. We want v mod n with v < 2^256 and
#   n in (2^255, 2^256). Concretely n > 2^255 (its top bit is set), so:
#       2^256 = 2 * 2^255 < 2 * n   ==>   v < 2^256 < 2n.
#   Therefore v - n is either negative (v < n, keep v) or in [0, n) (v >= n,
#   one subtract lands in range). EXACTLY ONE conditional subtract of n suffices
#   for any v < 2^256. (Two would be needed only if v could reach 3n, i.e.
#   v >= 3*2^255 > 2^256 — impossible for a 256-bit value.) We do the single
#   conditional subtract branch-free via the same masked-select as cond_sub_n,
#   but subtracting a 256-bit value (not an 11-limb 2n-residue): the input here
#   is a full 10-limb 256-bit number, no 11th carry limb.
# ---------------------------------------------------------------------------
def _cond_sub_n_256(limbs):
    """Branch-free single conditional subtract of n from a 10-limb value
    representing v < 2^256 (no carry limb). Returns 10 reduced limbs in [0, n).
    diff = v - n with borrow; if borrow set (v < n) keep v, else keep diff."""
    diff = [0] * N_LIMBS
    borrow = 0
    for i in range(N_LIMBS):
        d = limbs[i] - NMOD[i] - borrow
        if d < 0:
            d += B
            borrow = 1
        else:
            borrow = 0
        diff[i] = d & MASK
    mask_keep_v = -borrow   # all-ones if v < n (borrow set) else 0
    out = [0] * N_LIMBS
    for i in range(N_LIMBS):
        out[i] = (limbs[i] & mask_keep_v) | (diff[i] & (~mask_keep_v & MASK))
    return out

def reduce_mod_n(value):
    """Reduce a value mod n. Accepts an int OR 32 big-endian bytes. For a value
    < 2^256, one branch-free conditional subtract suffices (proof above).
    For a > 256-bit value (e.g. a 512-bit hash), falls back to splitting into
    256-bit halves reduced via Montgomery (correctness-first); ECDSA only needs
    the < 2^256 path. Returns a plain (NON-Montgomery) int in [0, n)."""
    if isinstance(value, (bytes, bytearray, list)):
        value = int.from_bytes(bytes(value), "big")
    if value < (1 << 256):
        limbs = [(value >> (R_BITS * i)) & MASK for i in range(N_LIMBS)]
        red = _cond_sub_n_256(limbs)
        return sum(int(red[i]) << (R_BITS * i) for i in range(N_LIMBS)) % N
    # >256-bit fallback: process 256-bit chunks via Horner with 2^256 mod n.
    # (Not needed by ECDSA's strict path; kept for the 512-bit hash convenience.)
    K = (1 << 256) % N
    acc = 0
    chunks = []
    v = value
    while v:
        chunks.append(v & ((1 << 256) - 1))
        v >>= 256
    for c in reversed(chunks):
        acc = (acc * K + (c % N)) % N
    return acc


# ---------------------------------------------------------------------------
# Fermat inverse z^(n-2) mod n via square-and-multiply over the fixed exponent.
# Montgomery-domain throughout. Straight-line (the emit form unrolls 256 bits).
# ---------------------------------------------------------------------------
_EXP = N - 2  # exponent for the multiplicative inverse mod n

def ninv(z):
    """z^(n-2) mod n, square-and-multiply MSB-first, all in Montgomery domain."""
    result = none_mont()
    base = list(z)
    e = _EXP
    bits = e.bit_length()
    for i in range(bits - 1, -1, -1):
        result = nsqr(result)
        if (e >> i) & 1:
            result = nmul(result, base)
    return result


# ===========================================================================
# Verbose emit helpers (let-chain text generators). Same call shape as
# p256_field.py's emit_* — only NMOD/N0INV/RR are the n versions.
# ===========================================================================
def _let(lets, name, expr):
    lets.append((name, expr))
    return name


def emit_nreduce(lets, prefix, acc_exprs):
    a = [_let(lets, f"{prefix}_t{i}", acc_exprs[i]) for i in range(N_LIMBS)]
    carry = None
    lows = []
    for i in range(N_LIMBS):
        src = a[i] if carry is None else _let(lets, f"{prefix}_v{i}", f"{a[i]} + {carry}")
        lows.append(_let(lets, f"{prefix}_lo{i}", f"band({src}, {MASK})"))
        carry = _let(lets, f"{prefix}_c{i}", f"shr({src}, {R_BITS})")
    top = carry
    return _emit_cond_sub_n(lets, prefix, lows, top)


def _emit_cond_sub_n(lets, prefix, t, top):
    """Branch-free t - n selection (11-limb t: 10 limb names + top carry name)."""
    diff = []
    borrow = "0"
    for i in range(N_LIMBS):
        d = _let(lets, f"{prefix}_d{i}", f"{t[i]} + {B} - {NMOD[i]} - {borrow}")
        diff.append(_let(lets, f"{prefix}_dl{i}", f"band({d}, {MASK})"))
        bit = _let(lets, f"{prefix}_bit{i}", f"band(shr({d}, {R_BITS}), 1)")
        borrow = _let(lets, f"{prefix}_br{i}", f"1 - {bit}")
    dtop = _let(lets, f"{prefix}_dtop", f"{top} + {B} - {borrow}")
    bittop = _let(lets, f"{prefix}_bittop", f"band(shr({dtop}, {R_BITS}), 1)")
    final_borrow = _let(lets, f"{prefix}_fbr", f"1 - {bittop}")
    m = _let(lets, f"{prefix}_keep", f"0 - {final_borrow}")
    out = []
    for i in range(N_LIMBS):
        mt = _let(lets, f"{prefix}_mt{i}", f"band({t[i]}, {m})")
        notm = _let(lets, f"{prefix}_nm{i}", f"{MASK} - band({m}, {MASK})")
        md = _let(lets, f"{prefix}_md{i}", f"band({diff[i]}, {notm})")
        out.append(_let(lets, f"{prefix}_o{i}", f"bor({mt}, {md})"))
    return out


def emit_nadd(lets, prefix, A, B_):
    return emit_nreduce(lets, prefix, [f"{A[i]} + {B_[i]}" for i in range(N_LIMBS)])


def emit_nsub(lets, prefix, A, B_):
    d = []
    borrow = "0"
    for i in range(N_LIMBS):
        v = _let(lets, f"{prefix}_sd{i}", f"{A[i]} + {B} - {B_[i]} - {borrow}")
        d.append(_let(lets, f"{prefix}_sl{i}", f"band({v}, {MASK})"))
        bit = _let(lets, f"{prefix}_sbit{i}", f"band(shr({v}, {R_BITS}), 1)")
        borrow = _let(lets, f"{prefix}_sbr{i}", f"1 - {bit}")
    m = _let(lets, f"{prefix}_smask", f"0 - {borrow}")
    out = []
    carry = "0"
    for i in range(N_LIMBS):
        addn = _let(lets, f"{prefix}_san{i}", f"band({NMOD[i]}, {m})")
        v = _let(lets, f"{prefix}_sv{i}", f"{d[i]} + {addn} + {carry}")
        out.append(_let(lets, f"{prefix}_so{i}", f"band({v}, {MASK})"))
        carry = _let(lets, f"{prefix}_sc{i}", f"shr({v}, {R_BITS})")
    return out


def emit_nmul(lets, prefix, A, B_):
    """CIOS Montgomery multiply mod n as a let-chain (mirrors mont_mul)."""
    t = ["0"] * (N_LIMBS + 2)
    for i in range(N_LIMBS):
        carry = "0"
        for j in range(N_LIMBS):
            v = _let(lets, f"{prefix}_v_{i}_{j}", f"{t[j]} + ({A[i]} * {B_[j]}) + {carry}")
            t[j] = _let(lets, f"{prefix}_t_{i}_{j}", f"band({v}, {MASK})")
            carry = _let(lets, f"{prefix}_cc_{i}_{j}", f"shr({v}, {R_BITS})")
        vN = _let(lets, f"{prefix}_vN_{i}", f"{t[N_LIMBS]} + {carry}")
        t[N_LIMBS] = _let(lets, f"{prefix}_tN_{i}", f"band({vN}, {MASK})")
        t[N_LIMBS + 1] = _let(lets, f"{prefix}_tN1_{i}", f"shr({vN}, {R_BITS})")
        m = _let(lets, f"{prefix}_m_{i}", f"band(({t[0]} * {N0INV}), {MASK})")
        carry = _let(lets, f"{prefix}_rc_{i}_0", f"shr({t[0]} + ({m} * {NMOD[0]}), {R_BITS})")
        new_t = [None] * (N_LIMBS + 2)
        for j in range(1, N_LIMBS):
            v = _let(lets, f"{prefix}_rv_{i}_{j}", f"{t[j]} + ({m} * {NMOD[j]}) + {carry}")
            new_t[j - 1] = _let(lets, f"{prefix}_rt_{i}_{j}", f"band({v}, {MASK})")
            carry = _let(lets, f"{prefix}_rcc_{i}_{j}", f"shr({v}, {R_BITS})")
        vN = _let(lets, f"{prefix}_rvN_{i}", f"{t[N_LIMBS]} + {carry}")
        new_t[N_LIMBS - 1] = _let(lets, f"{prefix}_rtN_{i}", f"band({vN}, {MASK})")
        new_t[N_LIMBS] = _let(lets, f"{prefix}_rtN1_{i}", f"{t[N_LIMBS + 1]} + shr({vN}, {R_BITS})")
        new_t[N_LIMBS + 1] = "0"
        t = new_t
    return _emit_cond_sub_n(lets, prefix, t[:N_LIMBS], t[N_LIMBS])


def emit_nsqr(lets, prefix, A):
    return emit_nmul(lets, prefix, A, A)


def emit_ninv(lets, prefix, z):
    """z^(n-2) mod n via square-and-multiply MSB-first. Straight-line: one nsqr
    per bit + one nmul per set bit. result starts at 1 (Montgomery R mod n).
    NOT recursive — a flat let-chain (large but no deep stack at runtime)."""
    one = to_limbs_n(1)
    result = [_let(lets, f"{prefix}_one{i}", str(one[i])) for i in range(N_LIMBS)]
    e = _EXP
    bits = e.bit_length()
    step = 0
    for i in range(bits - 1, -1, -1):
        result = emit_nsqr(lets, f"{prefix}_sq{step}", result)
        if (e >> i) & 1:
            result = emit_nmul(lets, f"{prefix}_mu{step}", result, z)
        step += 1
    return result


# ---------------------------------------------------------------------------
# BIG-ENDIAN 32-byte <-> Montgomery-domain limbs (mod n). Identical shape to
# brick 1, only emit_to_mont multiplies by R^2 mod n (the n version of RR).
# ---------------------------------------------------------------------------
def _be_byte_index_to_limb_pos(byte_be):
    le_byte = 31 - byte_be
    return le_byte * 8


def emit_decode_raw(lets, prefix, byte_names):
    out = []
    for i in range(N_LIMBS):
        lo, hi = R_BITS * i, R_BITS * i + R_BITS
        parts = []
        for byte_be in range(32):
            bit = _be_byte_index_to_limb_pos(byte_be)
            if bit < hi and bit + 8 > lo:
                local = bit - lo
                bn = byte_names[byte_be]
                if local > 0:
                    parts.append(f"shl({bn}, {local})")
                elif local == 0:
                    parts.append(bn)
                else:
                    parts.append(f"shr({bn}, {-local})")
        expr = parts[0]
        for p_ in parts[1:]:
            expr = f"bor({expr}, {p_})"
        out.append(_let(lets, f"{prefix}_raw{i}", f"band({expr}, {MASK})"))
    return out


def emit_to_mont(lets, prefix, raw_limbs):
    """Lift plain limbs into Montgomery domain mod n by multiplying by R^2 mod n."""
    rr = _int_to_limbs(RR)
    rr_names = [_let(lets, f"{prefix}_rr{i}", str(rr[i])) for i in range(N_LIMBS)]
    return emit_nmul(lets, f"{prefix}_lift", raw_limbs, rr_names)


def emit_decode(lets, prefix, byte_names):
    raw = emit_decode_raw(lets, prefix, byte_names)
    return emit_to_mont(lets, prefix, raw)


def emit_from_mont(lets, prefix, fe):
    one = [_let(lets, f"{prefix}_one{i}", "1" if i == 0 else "0") for i in range(N_LIMBS)]
    return emit_nmul(lets, f"{prefix}_unlift", fe, one)


def emit_encode(lets, prefix, fe):
    plain = emit_from_mont(lets, prefix, fe)
    out = []
    for byte_be in range(32):
        bit = _be_byte_index_to_limb_pos(byte_be)
        parts = []
        for i in range(N_LIMBS):
            lo, hi = R_BITS * i, R_BITS * i + R_BITS
            if lo < bit + 8 and hi > bit:
                local = lo - bit
                ln = plain[i]
                if local > 0:
                    parts.append(f"shl({ln}, {local})")
                elif local == 0:
                    parts.append(ln)
                else:
                    parts.append(f"shr({ln}, {-local})")
        expr = parts[0]
        for p_ in parts[1:]:
            expr = f"bor({expr}, {p_})"
        out.append(_let(lets, f"{prefix}_ob{byte_be}", f"band({expr}, 255)"))
    return out


# Python oracles for encode/decode (big-endian)
def decode_scalar(byts):
    """32 big-endian bytes -> Montgomery limbs (mod n)."""
    x = int.from_bytes(bytes(byts), "big") % N
    return to_limbs_n(x)

def encode_scalar(fe):
    """Montgomery limbs -> 32 big-endian bytes."""
    return list((from_limbs_n(fe) % N).to_bytes(32, "big"))


if __name__ == "__main__":
    import random, sys

    random.seed(20260601)
    # edge cases incl. 0, 1, n-1, n-2, values near 2^256, and a value > n
    EDGE = [0, 1, 2, N - 1, N - 2, (1 << 256) - 1, (1 << 255), N + 1, N + 12345,
            (1 << 256) - 5]
    EDGE_MODN = [e % N for e in EDGE]

    results = {}

    def check(name, n_random, fn):
        bad = 0
        total = 0
        for x in EDGE_MODN:
            for y in EDGE_MODN:
                total += 1
                if not fn(x, y):
                    bad += 1
        for _ in range(n_random):
            x = random.randrange(N)
            y = random.randrange(N)
            total += 1
            if not fn(x, y):
                bad += 1
        results[name] = (total - bad, total)

    # Montgomery round-trip
    rt_bad = 0
    rt_total = 0
    for x in EDGE_MODN + [random.randrange(N) for _ in range(2000)]:
        rt_total += 1
        if from_limbs_n(to_limbs_n(x)) != x % N:
            rt_bad += 1
    results["mont_roundtrip"] = (rt_total - rt_bad, rt_total)

    check("nmul", 5000, lambda x, y: from_limbs_n(nmul(to_limbs_n(x), to_limbs_n(y))) == (x * y) % N)
    check("nadd", 5000, lambda x, y: from_limbs_n(nadd(to_limbs_n(x), to_limbs_n(y))) == (x + y) % N)
    check("nsub", 5000, lambda x, y: from_limbs_n(nsub(to_limbs_n(x), to_limbs_n(y))) == (x - y) % N)
    check("nsqr", 5000, lambda x, y: from_limbs_n(nsqr(to_limbs_n(x))) == (x * x) % N)

    # ninv: only meaningful for x != 0; also confirm x * x^-1 == 1
    inv_bad = 0
    inv_total = 0
    for x in [e for e in EDGE_MODN if e != 0] + [random.randrange(1, N) for _ in range(200)]:
        inv_total += 1
        got = from_limbs_n(ninv(to_limbs_n(x)))
        exp = pow(x, N - 2, N)
        if got != exp:
            inv_bad += 1
        elif (x * got) % N != 1:
            inv_bad += 1
    results["ninv"] = (inv_total - inv_bad, inv_total)

    # (n-1)^-1 == n-1 sanity (since (n-1)^2 == 1 mod n)
    nm1 = N - 1
    inv_nm1 = from_limbs_n(ninv(to_limbs_n(nm1)))
    results["ninv_nm1_is_nm1"] = (1 if inv_nm1 == nm1 else 0, 1)

    # reduce_mod_n: values < 2^256 (incl. x-coords that can exceed n), and >256-bit
    red_bad = 0
    red_total = 0
    red_inputs = (
        [0, 1, N - 1, N, N + 1, (1 << 256) - 1, (1 << 255), N + 99999]
        + [random.randrange(1 << 256) for _ in range(5000)]
    )
    for v in red_inputs:
        red_total += 1
        if reduce_mod_n(v) != v % N:
            red_bad += 1
    # bytes form
    for _ in range(500):
        v = random.randrange(1 << 256)
        be = v.to_bytes(32, "big")
        red_total += 1
        if reduce_mod_n(be) != v % N:
            red_bad += 1
    # >256-bit (512-bit hash) fallback path
    for _ in range(500):
        v = random.randrange(1 << 512)
        red_total += 1
        if reduce_mod_n(v) != v % N:
            red_bad += 1
    results["reduce_mod_n"] = (red_total - red_bad, red_total)

    # encode/decode big-endian round-trip vs big-int
    ed_bad = 0
    ed_total = 0
    for x in EDGE_MODN + [random.randrange(N) for _ in range(2000)]:
        ed_total += 1
        be = list((x % N).to_bytes(32, "big"))
        limbs = decode_scalar(be)
        back = encode_scalar(limbs)
        if back != be:
            ed_bad += 1
        if from_limbs_n(limbs) != x % N:
            ed_bad += 1
    results["encode_decode_be"] = (ed_total - ed_bad, ed_total)

    all_ok = all(p == t for (p, t) in results.values())
    order = ["mont_roundtrip", "nmul", "nadd", "nsub", "nsqr", "ninv",
             "ninv_nm1_is_nm1", "reduce_mod_n", "encode_decode_be"]
    for k in order:
        p, t = results[k]
        print(f"{k}: {p}/{t} {'OK' if p == t else 'FAIL'}")
    print(f"n0inv={N0INV} RR_lo_limb={_int_to_limbs(RR)[0]}")
    print("P256_SCALAR_OK" if all_ok else "P256_SCALAR_FAIL")
    sys.exit(0 if all_ok else 1)
