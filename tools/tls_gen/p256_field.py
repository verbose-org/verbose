"""GF(p256) field arithmetic emit helpers (Montgomery CIOS, 10-limb radix-2^26),
the foundation of the ECDSA-over-P-256 Verbose arc (brick 1). Mirrors the
structure and call shape of field_emit.py (the GF(2^255-19) brick) so brick 2
(point add) can consume these emit helpers unchanged.

P-256 coordinate field:
    p = 2^256 - 2^224 + 2^192 + 2^96 - 1
      = 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffff

WHY MONTGOMERY (CIOS), NOT LAZY-REDUCED SOLINAS:
  The 25519 brick uses a pseudo-Mersenne prime (2^255-19) whose fold is a single
  "*19" — one clean coefficient, no sign changes. P-256's prime is a Solinas
  prime with 2^256 = 2^224 - 2^192 - 2^96 + 1 (mod p): folding a schoolbook
  product would scatter the high columns to the 224/192/96 bit positions, which
  are NOT limb-aligned for a 26-bit radix, and the fold coefficients carry SIGN
  CHANGES (-2^192, -2^96). Hand-propagating those through a redundant-limb carry
  chain is exactly the off-by-one trap the Ed25519 arc hit twice. NIST/Solinas
  fast reduction is even worse for Verbose: it needs branchy conditional
  subtractions, and Verbose has no cheap conditionals and must stay fail-closed.
  Montgomery CIOS is the cleanest fit: every limb step is the SAME
  multiply-accumulate-reduce shape, there are no sign changes, and the only
  conditional is the final "subtract p if t >= p", done BRANCH-FREE with a mask.

LIMB REPRESENTATION (Montgomery domain):
  A field element x is stored as its Montgomery form  x_mont = x * R mod p,
  where R = B^n = 2^260, B = 2^26, n = 10. The Montgomery value (a residue in
  [0, p)) is split into 10 unsigned limbs of 26 bits each, little-endian by
  limb index:   x_mont = sum_{i=0}^{9} limb[i] * 2^(26*i),   limb[i] in [0, 2^26).
  (Limb 9 only needs 22 bits since 256 - 9*26 = 22, but the representation
  carries a full 26-bit slot; values stay < p so the top limb never exceeds
  N[9] range in a reduced element.)

  Modulus limbs N[i] = (p >> 26i) & (2^26-1):
      N = [67108863, 67108863, 67108863, 262143, 0, 0, 0, 1024, 67043328, 4194303]
  Montgomery inverse word constant: n0inv = (-N[0]^{-1}) mod 2^26 = 1
      (N[0] = 2^26-1 == -1 mod 2^26, so its inverse is -1, and -(-1) = 1).

i64 OVERFLOW SAFETY (the load-bearing argument):
  In CIOS the hottest accumulator value at any limb position is
      v = t[j] + a[i]*b[j] + C        (and symmetrically t[j] + m*N[j] + C)
  with every operand a 26-bit unsigned limb and the running carry C also a
  26-bit value (proven below). Worst case:
      vmax = (2^26-1)^2 + 2*(2^26-1) = 4503599560261632  (52 bits)
  which is < 2^63 with 11 bits of headroom. The carry out is
      C = vmax >> 26 = 2^26-1, i.e. it stays a 26-bit value (carry < B), so the
  bound is self-consistent across all 10 inner iterations. No accumulation of
  multiple partial products at one column (unlike schoolbook fmul) — CIOS folds
  column-by-column, so the 52-bit bound is the global max. i64 is safe with
  large margin; r=26 is the comfortable choice (r=22/12-limb also fits but
  r=26 mirrors the 25519 brick's limb width for auditor familiarity).

BIG-ENDIAN BYTE ORDER:
  P-256 field elements / point coordinates are serialized BIG-ENDIAN (SEC1,
  RFC 5480), unlike 25519's little-endian. emit_decode reads byte 0 as the MOST
  significant byte; emit_encode writes the MOST significant byte first. The
  decode also converts the integer INTO the Montgomery domain (multiply by R^2,
  one fmul) and encode converts OUT (multiply by 1, one fmul) so the limb arrays
  consumed/produced by the arithmetic helpers are always Montgomery residues.

emit_* helpers append (name, expr) lets to a list and return the 10 result
names, exactly like field_emit.py. PURE-PYTHON references (freduce/fadd/fsub/
fmul/finv) operate per-limb (NO big-int in the core) and are validated against
big-int %p in __main__.
"""

R_BITS = 26
N_LIMBS = 10
B = 1 << R_BITS          # 67108864
MASK = B - 1             # 67108863
P = (1 << 256) - (1 << 224) + (1 << 192) + (1 << 96) - 1
R = B ** N_LIMBS         # 2^260, the Montgomery radix
RR = (R * R) % P         # R^2 mod p, the to-Montgomery constant
# Modulus limbs (little-endian by 26-bit limb)
NMOD = [(P >> (R_BITS * i)) & MASK for i in range(N_LIMBS)]
# n0inv = -N[0]^{-1} mod B
N0INV = (-pow(NMOD[0], -1, B)) % B   # == 1


# ---------------------------------------------------------------------------
# Domain conversion (big-int only here; the arithmetic CORE below is big-int free)
# ---------------------------------------------------------------------------
def _int_to_limbs(x):
    """Plain base-2^26 little-endian limb split of a residue in [0, p)."""
    x %= P
    return [(x >> (R_BITS * i)) & MASK for i in range(N_LIMBS)]

def _limbs_to_int(l):
    return sum(int(l[i]) << (R_BITS * i) for i in range(N_LIMBS)) % P

def to_limbs(x):
    """Field int -> Montgomery-domain limbs (x*R mod p)."""
    return _int_to_limbs((x % P) * R % P)

def from_limbs(l):
    """Montgomery-domain limbs -> field int (value*R^{-1} mod p)."""
    return _limbs_to_int(l) * pow(R, -1, P) % P


# ---------------------------------------------------------------------------
# PURE-PYTHON references (per-limb int ops only — NO big-int in the core).
# These are the oracle shapes the Verbose let-chains mirror exactly.
# ---------------------------------------------------------------------------
def cond_sub_p(t):
    """Branch-free conditional subtract of p from an 11-limb value t (limbs in
    [0,B), t < 2p). Returns 10 reduced limbs. Mirrors the masked subtract the
    Verbose emitter produces: compute t-p with borrow, then select t-p if the
    final borrow is 0 (t >= p) else t, all via a mask. Big-int free."""
    # t has N_LIMBS+1 limbs (index 10 is the top carry). Subtract NMOD (10 limbs).
    diff = [0] * (N_LIMBS + 1)
    borrow = 0
    for i in range(N_LIMBS):
        d = t[i] - NMOD[i] - borrow
        borrow = (d >> 63) & 1          # logical: 1 if d went negative (d is python int; emulate)
        # emulate two's-complement borrow detection without big-int compare:
        # in Verbose we use the sign via the bias trick; here d may be negative
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
    # borrow == 1 means t < p (subtraction underflowed) -> keep t; else keep diff.
    mask_keep_t = -borrow            # all-ones if borrow==1 else 0 (two's comp)
    out = [0] * N_LIMBS
    for i in range(N_LIMBS):
        out[i] = (t[i] & mask_keep_t) | (diff[i] & (~mask_keep_t & MASK))
    return out

def mont_mul(a, b):
    """CIOS Montgomery multiply: returns limbs of (a*b*R^{-1}) mod p.
    Operates entirely on per-limb int ops (no big-int). a,b are 10-limb
    Montgomery residues; result is a 10-limb Montgomery residue."""
    t = [0] * (N_LIMBS + 2)          # accumulator: n+2 limbs
    for i in range(N_LIMBS):
        # t += a[i] * b
        C = 0
        for j in range(N_LIMBS):
            v = t[j] + a[i] * b[j] + C
            t[j] = v & MASK
            C = v >> R_BITS
        v = t[N_LIMBS] + C
        t[N_LIMBS] = v & MASK
        t[N_LIMBS + 1] = v >> R_BITS
        # m = t[0] * n0inv mod B ; t += m * N ; then shift t down by one limb
        m = (t[0] * N0INV) & MASK
        C = (t[0] + m * NMOD[0]) >> R_BITS      # low limb t[0] becomes 0, carry only
        for j in range(1, N_LIMBS):
            v = t[j] + m * NMOD[j] + C
            t[j - 1] = v & MASK
            C = v >> R_BITS
        v = t[N_LIMBS] + C
        t[N_LIMBS - 1] = v & MASK
        t[N_LIMBS] = t[N_LIMBS + 1] + (v >> R_BITS)
    # t now holds the reduced product in limbs 0..N_LIMBS (t[N_LIMBS] is final carry)
    return cond_sub_p(t[:N_LIMBS + 1])

def freduce(acc):
    """Carry-normalize an 11-limb non-negative accumulator then conditional-sub p.
    Used by fadd/fsub whose inputs are already Montgomery residues < p, so the
    sum/biased-difference is < 2p and one cond_sub suffices after carry propagation."""
    t = list(acc) + [0] * (N_LIMBS + 1 - len(acc))
    C = 0
    for i in range(N_LIMBS):
        v = t[i] + C
        t[i] = v & MASK
        C = v >> R_BITS
    t[N_LIMBS] = t[N_LIMBS] + C if len(t) > N_LIMBS else C
    return cond_sub_p(t[:N_LIMBS + 1])

def fadd(a, b):
    """(a + b) mod p, both Montgomery residues in [0,p). Montgomery form is
    additive-linear: (aR + bR) = (a+b)R, so plain limb add + reduce works."""
    return freduce([a[i] + b[i] for i in range(N_LIMBS)] + [0])

def fsub(a, b):
    """(a - b) mod p. Borrow-subtract limb-wise, then BRANCH-FREE conditionally
    add p back when a < b (final borrow set). A bias-to-multiple-of-p approach
    does NOT work here: a 26-bit/10-limb non-negative bias that keeps every limb
    >= MASK is necessarily ~16p (since MASK*(2^260-1)/(2^26-1) ~ 2^260 ~ 16p),
    and this Montgomery freduce/cond_sub only collapses values < 2p. So fsub uses
    the standard borrow-then-add-p shape (same masked-select discipline as
    cond_sub_p). Big-int free."""
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
    add_p = NMOD if borrow else [0] * N_LIMBS   # branch-free in emit; clear here
    out = [0] * N_LIMBS
    carry = 0
    for i in range(N_LIMBS):
        x = d[i] + add_p[i] + carry
        out[i] = x & MASK
        carry = x >> R_BITS
    return out

def fmul(a, b):
    return mont_mul(a, b)

def fsqr(a):
    return mont_mul(a, a)

def fone():
    """1 in Montgomery domain = R mod p."""
    return to_limbs(1)


# ---------------------------------------------------------------------------
# Fermat inverse z^(p-2) mod p via square-and-multiply over the fixed exponent.
# Montgomery-domain throughout (mont_mul preserves the domain). Correctness
# first; an optimized addition chain is a later micro-opt (brick 1 just needs it
# to be provably right).
# ---------------------------------------------------------------------------
_EXP = P - 2  # exponent for the multiplicative inverse

def finv(z):
    """z^(p-2) mod p, square-and-multiply MSB-first, all in Montgomery domain.
    Returns Montgomery-domain limbs of z^{-1}."""
    result = fone()
    base = list(z)
    e = _EXP
    bits = e.bit_length()
    for i in range(bits - 1, -1, -1):
        result = fsqr(result)
        if (e >> i) & 1:
            result = fmul(result, base)
    return result


# ===========================================================================
# Verbose emit helpers (let-chain text generators). Same call shape as
# field_emit.py: each appends (name, expr) lets and returns the result limb
# names. A,B are lists of 10 limb-name strings.
# ===========================================================================
def _let(lets, name, expr):
    lets.append((name, expr))
    return name


def emit_freduce(lets, prefix, acc_exprs):
    """Carry-normalize acc_exprs (10 expr strings, each a non-negative sum of
    Montgomery limbs) into 10 limbs, then branch-free conditional-subtract p.
    Returns the 10 reduced limb names."""
    # 1. materialize the raw accumulator limbs
    a = [_let(lets, f"{prefix}_t{i}", acc_exprs[i]) for i in range(N_LIMBS)]
    # 2. carry propagate low->high (one pass; inputs are < 2p so this suffices)
    carry = None
    lows = []
    for i in range(N_LIMBS):
        src = a[i] if carry is None else _let(lets, f"{prefix}_v{i}", f"{a[i]} + {carry}")
        lows.append(_let(lets, f"{prefix}_lo{i}", f"band({src}, {MASK})"))
        carry = _let(lets, f"{prefix}_c{i}", f"shr({src}, {R_BITS})")
    top = carry  # 11th limb (carry out of limb 9)
    return _emit_cond_sub_p(lets, prefix, lows, top)


def _emit_cond_sub_p(lets, prefix, t, top):
    """Branch-free t - p selection. t = 10 reduced limb names (each < B), top =
    name of the 11th (carry) limb. Computes diff = t - p with borrow, then
    selects diff when t >= p (borrow out == 0) else t, via an all-ones/zero mask.

    Borrow is detected WITHOUT a signed compare (Verbose `/` is logical): for
    each limb compute  d = t[i] - N[i] - borrow + B  (always in [0, 2B)), then
    the new borrow is the COMPLEMENT of bit 26 of d:  if d >= B no underflow
    happened (bit26 set -> borrow 0); if d < B it underflowed (bit26 clear ->
    borrow 1). diff limb = band(d, MASK)."""
    diff = []
    borrow = "0"
    for i in range(N_LIMBS):
        # d = t[i] + B - N[i] - borrow   (B-N[i] folded into one non-negative const where possible)
        d = _let(lets, f"{prefix}_d{i}", f"{t[i]} + {B} - {NMOD[i]} - {borrow}")
        diff.append(_let(lets, f"{prefix}_dl{i}", f"band({d}, {MASK})"))
        # bit26 of d: 1 == no borrow. new borrow = 1 - bit26.
        bit = _let(lets, f"{prefix}_bit{i}", f"band(shr({d}, {R_BITS}), 1)")
        borrow = _let(lets, f"{prefix}_br{i}", f"1 - {bit}")
    # account for the top carry limb: real value t may be >= p purely via top.
    # final borrow including top: if top - borrow underflows we keep t.
    dtop = _let(lets, f"{prefix}_dtop", f"{top} + {B} - {borrow}")
    bittop = _let(lets, f"{prefix}_bittop", f"band(shr({dtop}, {R_BITS}), 1)")
    final_borrow = _let(lets, f"{prefix}_fbr", f"1 - {bittop}")
    # mask_keep_t = 0 - final_borrow  (all-ones if borrow==1 else 0).
    # We build select per limb: out = band(diff, ~m & MASK) | band(t, m).
    # Avoid bitwise-not: m is 0 or all-ones; (MASK - (m & MASK)) flips low 26 bits.
    m = _let(lets, f"{prefix}_keep", f"0 - {final_borrow}")
    out = []
    for i in range(N_LIMBS):
        mt = _let(lets, f"{prefix}_mt{i}", f"band({t[i]}, {m})")
        notm = _let(lets, f"{prefix}_nm{i}", f"{MASK} - band({m}, {MASK})")
        md = _let(lets, f"{prefix}_md{i}", f"band({diff[i]}, {notm})")
        out.append(_let(lets, f"{prefix}_o{i}", f"bor({mt}, {md})"))
    return out


def emit_fadd(lets, prefix, A, B_):
    return emit_freduce(lets, prefix, [f"{A[i]} + {B_[i]}" for i in range(N_LIMBS)])


def emit_fsub(lets, prefix, A, B_):
    """Borrow-subtract A-B limb-wise (borrow detected via bit 26 of
    d = A[i] + B - B[i] - borrow, no signed compare), then branch-free add
    (p & mask) where mask = 0 - final_borrow. Returns 10 limb names."""
    d = []
    borrow = "0"
    for i in range(N_LIMBS):
        v = _let(lets, f"{prefix}_sd{i}", f"{A[i]} + {B} - {B_[i]} - {borrow}")
        d.append(_let(lets, f"{prefix}_sl{i}", f"band({v}, {MASK})"))
        bit = _let(lets, f"{prefix}_sbit{i}", f"band(shr({v}, {R_BITS}), 1)")
        borrow = _let(lets, f"{prefix}_sbr{i}", f"1 - {bit}")
    # mask = all-ones if a < b (borrow set) else 0
    m = _let(lets, f"{prefix}_smask", f"0 - {borrow}")
    out = []
    carry = "0"
    for i in range(N_LIMBS):
        # add (N[i] & m) + carry ; N[i] & m is N[i] when m all-ones else 0
        addp = _let(lets, f"{prefix}_sap{i}", f"band({NMOD[i]}, {m})")
        v = _let(lets, f"{prefix}_sv{i}", f"{d[i]} + {addp} + {carry}")
        out.append(_let(lets, f"{prefix}_so{i}", f"band({v}, {MASK})"))
        carry = _let(lets, f"{prefix}_sc{i}", f"shr({v}, {R_BITS})")
    return out


def emit_fmul(lets, prefix, A, B_):
    """CIOS Montgomery multiply as a let-chain. Emits the same per-limb
    accumulate/reduce shape as mont_mul. t is 12 named running limbs; each i
    iteration rebinds them via fresh let names (SSA-style, no mutation)."""
    # t[0..N_LIMBS+1] start at 0 (literal "0")
    t = ["0"] * (N_LIMBS + 2)
    for i in range(N_LIMBS):
        # --- t += a[i]*b ---
        carry = "0"
        for j in range(N_LIMBS):
            # v = t[j] + (a[i]*b[j]) + carry
            v = _let(lets, f"{prefix}_v_{i}_{j}", f"{t[j]} + ({A[i]} * {B_[j]}) + {carry}")
            t[j] = _let(lets, f"{prefix}_t_{i}_{j}", f"band({v}, {MASK})")
            carry = _let(lets, f"{prefix}_cc_{i}_{j}", f"shr({v}, {R_BITS})")
        vN = _let(lets, f"{prefix}_vN_{i}", f"{t[N_LIMBS]} + {carry}")
        t[N_LIMBS] = _let(lets, f"{prefix}_tN_{i}", f"band({vN}, {MASK})")
        t[N_LIMBS + 1] = _let(lets, f"{prefix}_tN1_{i}", f"shr({vN}, {R_BITS})")
        # --- m = t[0]*n0inv mod B ; t += m*N ; shift down one limb ---
        m = _let(lets, f"{prefix}_m_{i}", f"band(({t[0]} * {N0INV}), {MASK})")
        # j=0: low limb is discarded (becomes carry only)
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
    # final cond-sub p over t[0..N_LIMBS] (t[N_LIMBS] is the carry/top limb)
    return _emit_cond_sub_p(lets, prefix, t[:N_LIMBS], t[N_LIMBS])


def emit_fsqr(lets, prefix, A):
    return emit_fmul(lets, prefix, A, A)


def emit_finv(lets, prefix, z):
    """z^(p-2) mod p via square-and-multiply MSB-first. Emits one fsqr per bit
    and one fmul per set bit. result starts at 1 (Montgomery R). Returns the 10
    inverse limb names. Correctness-first; addition-chain optimization is a
    later micro-opt and does not change the audited result."""
    # result = 1 in Montgomery domain (R mod p): materialize the constant limbs
    one = to_limbs(1)
    result = [_let(lets, f"{prefix}_one{i}", str(one[i])) for i in range(N_LIMBS)]
    e = _EXP
    bits = e.bit_length()
    step = 0
    for i in range(bits - 1, -1, -1):
        result = emit_fsqr(lets, f"{prefix}_sq{step}", result)
        if (e >> i) & 1:
            result = emit_fmul(lets, f"{prefix}_mu{step}", result, z)
        step += 1
    return result


# ---------------------------------------------------------------------------
# BIG-ENDIAN 32-byte <-> Montgomery-domain limbs.
# decode: 32 big-endian bytes -> integer -> Montgomery (x*R^2*R^{-1} = x*R).
# encode: Montgomery limbs -> integer (mont_mul by 1) -> 32 big-endian bytes.
# The emit helpers below mirror this: decode emits the byte gather + a fmul by
# the R^2 constant; encode emits a fmul by 1 then the byte scatter.
# ---------------------------------------------------------------------------
def _be_byte_index_to_limb_pos(byte_be):
    """Big-endian byte index (0=MSB) -> the little-endian bit offset of its
    low bit within the 256-bit integer. byte_be=0 is the top byte (bits 248..255)."""
    le_byte = 31 - byte_be
    return le_byte * 8


def emit_decode_raw(lets, prefix, byte_names):
    """byte_names: 32 names, byte_names[0] = MOST significant byte (big-endian).
    Emit the plain base-2^26 limb gather of the 256-bit integer (NOT yet in
    Montgomery domain). Returns 10 limb names. Caller multiplies by R^2 to lift
    into Montgomery form (emit_to_mont)."""
    # Build each limb i by OR-ing the byte contributions overlapping [26i, 26i+26).
    out = []
    for i in range(N_LIMBS):
        lo, hi = R_BITS * i, R_BITS * i + R_BITS
        parts = []
        for byte_be in range(32):
            bit = _be_byte_index_to_limb_pos(byte_be)   # low bit of this byte in the integer
            if bit < hi and bit + 8 > lo:
                local = bit - lo                          # shift of this byte within the limb
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
    """Lift plain limbs into Montgomery domain by multiplying by R^2 mod p
    (one CIOS mul: (x)(R^2)R^{-1} = xR)."""
    rr = _int_to_limbs(RR)
    rr_names = [_let(lets, f"{prefix}_rr{i}", str(rr[i])) for i in range(N_LIMBS)]
    return emit_fmul(lets, f"{prefix}_lift", raw_limbs, rr_names)


def emit_decode(lets, prefix, byte_names):
    """Full big-endian decode: 32 byte names -> Montgomery limbs."""
    raw = emit_decode_raw(lets, prefix, byte_names)
    return emit_to_mont(lets, prefix, raw)


def emit_from_mont(lets, prefix, fe):
    """Bring a Montgomery residue back to plain form by multiplying by 1
    (CIOS mul: (xR)(1)R^{-1} = x)."""
    one = [_let(lets, f"{prefix}_one{i}", "1" if i == 0 else "0") for i in range(N_LIMBS)]
    return emit_fmul(lets, f"{prefix}_unlift", fe, one)


def emit_encode(lets, prefix, fe):
    """Full big-endian encode: Montgomery limbs -> 32 byte names (big-endian,
    byte 0 = MSB). Returns 32 byte-name lets."""
    plain = emit_from_mont(lets, prefix, fe)
    out = []
    for byte_be in range(32):
        bit = _be_byte_index_to_limb_pos(byte_be)   # low bit of this byte in the integer
        parts = []
        for i in range(N_LIMBS):
            lo, hi = R_BITS * i, R_BITS * i + R_BITS
            if lo < bit + 8 and hi > bit:
                local = lo - bit                       # shift of this limb relative to the byte
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
def decode_field(byts):
    """32 big-endian bytes -> Montgomery limbs."""
    x = int.from_bytes(bytes(byts), "big") % P
    return to_limbs(x)

def encode_field(fe):
    """Montgomery limbs -> 32 big-endian bytes."""
    return list((from_limbs(fe) % P).to_bytes(32, "big"))


if __name__ == "__main__":
    import random, sys

    random.seed(20260601)
    EDGE = [0, 1, 2, P - 1, P - 2, (1 << 256) - 1 - 0, (1 << 255), (1 << 96), (1 << 224)]
    EDGE = [e % P for e in EDGE]

    results = {}

    def check(name, n_random, fn):
        bad = 0
        total = 0
        # edge x edge
        for x in EDGE:
            for y in EDGE:
                total += 1
                if not fn(x, y):
                    bad += 1
        # random
        for _ in range(n_random):
            x = random.randrange(P)
            y = random.randrange(P)
            total += 1
            if not fn(x, y):
                bad += 1
        results[name] = (total - bad, total)

    # round-trip of the Montgomery encoding itself
    rt_bad = 0
    rt_total = 0
    for x in EDGE + [random.randrange(P) for _ in range(2000)]:
        rt_total += 1
        if from_limbs(to_limbs(x)) != x % P:
            rt_bad += 1
    results["mont_roundtrip"] = (rt_total - rt_bad, rt_total)

    check("fmul", 5000, lambda x, y: from_limbs(fmul(to_limbs(x), to_limbs(y))) == (x * y) % P)
    check("fadd", 5000, lambda x, y: from_limbs(fadd(to_limbs(x), to_limbs(y))) == (x + y) % P)
    check("fsub", 5000, lambda x, y: from_limbs(fsub(to_limbs(x), to_limbs(y))) == (x - y) % P)
    check("fsqr", 5000, lambda x, y: from_limbs(fsqr(to_limbs(x))) == (x * x) % P)

    # finv: only meaningful for x != 0
    inv_bad = 0
    inv_total = 0
    for x in [e for e in EDGE if e != 0] + [random.randrange(1, P) for _ in range(200)]:
        inv_total += 1
        got = from_limbs(finv(to_limbs(x)))
        exp = pow(x, P - 2, P)
        if got != exp:
            inv_bad += 1
        else:
            # also confirm x * x^{-1} == 1
            if (x * got) % P != 1:
                inv_bad += 1
    results["finv"] = (inv_total - inv_bad, inv_total)

    # encode/decode big-endian round-trip vs big-int
    ed_bad = 0
    ed_total = 0
    for x in EDGE + [random.randrange(P) for _ in range(2000)]:
        ed_total += 1
        be = list((x % P).to_bytes(32, "big"))
        limbs = decode_field(be)
        back = encode_field(limbs)
        if back != be:
            ed_bad += 1
        if from_limbs(limbs) != x % P:
            ed_bad += 1
    results["encode_decode_be"] = (ed_total - ed_bad, ed_total)

    all_ok = all(p == t for (p, t) in results.values())
    for k in ["mont_roundtrip", "fmul", "fadd", "fsub", "fsqr", "finv", "encode_decode_be"]:
        p, t = results[k]
        print(f"{k}: {p}/{t} {'OK' if p == t else 'FAIL'}")
    print("P256_FIELD_OK" if all_ok else "P256_FIELD_FAIL")
    sys.exit(0 if all_ok else 1)
