"""Generate examples/p256_ninv_rec.verbose — RECURSIVE k^-1 mod n (P-256 group
order) in pure Verbose. Same correctness as the UNROLLED examples/p256_ninv.verbose
(brick 4), but the 256-bit Fermat square-and-multiply is expressed as a RECURSIVE
rule (one step's worth of code, looped via `decreasing : j`) instead of a flat
256-step let-chain. The result: the native binary shrinks from ~11.2 MB (the
unrolled emit_ninv unrolls all 256 exponent bits in line) to a few hundred KB.

This applies the lesson proven by examples/p256_scalarmult.verbose (recursive
double-and-add → 728 KB / 1.26 MB .verbose) to ninv. We MIRROR p256scalar_gen.py's
shape: a state concept carrying the accumulator + the (invariant) base + a bit
counter j + which + a hex string the exponent bits are read from; a recursive rule
that does ONE square-and-(conditional)-multiply step and recurses with j-1; a base
case at j==0; `termination: decreasing : j`; a complete `reads:` list; the byte_at
OOB guard for the exponent-bit read.

ALGORITHM (MSB-first square-and-multiply, matching the unrolled emit_ninv exactly):
  The unrolled emit_ninv does, for the exponent e = n-2 (bit_length 256):
      result = 1 (Montgomery R mod n)
      for i in range(255, -1, -1):          # MSB-first
          result = nsqr(result)
          if (e >> i) & 1: result = nmul(result, z)
  The recursive form uses a counter j running 256 -> 1. At counter j (j>0) we
  process exponent bit index i = j - 1 (so j=256 → bit 255 the MSB, …, j=1 →
  bit 0 the LSB). Per step:
      sq   = nsqr(result)                   # ALWAYS computed
      mul  = nmul(sq, z)                    # ALWAYS computed (constant shape)
      result' = if exp_bit(j-1) == 1 then mul else sq    # select by the bit
      recurse j-1, same z, same exp, same which
  Base case (j==0): result IS k^-1 in the Montgomery domain → bring it out of
  Montgomery (emit_from_mont) and serialize big-endian (emit_encode), return
  byte `which`.

BIT ORDER (how it matches the unrolled result): the exponent n-2 is passed as a
64-hex-char BIG-ENDIAN string `exp`. Integer bit index i maps to big-endian byte
byte_be = 31 - (i>>3), hex char indices hpos = 2*byte_be (high nibble) and hpos+1,
bit = (bytev >> (i&7)) & 1. With i = j-1 this reproduces the MSB-first loop above
EXACTLY (verified in Python: recursive square-and-multiply == pow(k, n-2, n) for
k in {1, 2, n-1, n-2, random}). Note: p256_scalarmult reads pos = 256 - j (LSB-
first over the scalar); here we read pos = j - 1 (MSB-first over the FIXED
exponent) — the standard square-and-multiply order, chosen to mirror emit_ninv's
`for i in range(bits-1, -1, -1)` so results are bit-identical.

SEEDING THE INITIAL MONTGOMERY STATE (host driver, mirroring p256_scalarmult):
  The recursive rule is PURE — it never lifts to Montgomery itself. The host driver
  computes and passes the initial state:
      result = none_mont()  = R mod n              (= "1" in Montgomery)
      z      = to_limbs_n(k) = k*R mod n           (k lifted to Montgomery, INVARIANT)
      j      = 256                                  (exponent bit_length)
      which  = 0..31                                (output byte selector)
      exp    = format(n-2, '064x')                 (64-hex-char big-endian exponent)
  This is exactly how p256_scalarmult's host passes Q=O and P=G as Montgomery limbs
  and seeds the bit counter — the .verbose stays a pure recursive walk.

LET-CHAIN SIZE: one step is emit_nsqr (one CIOS mul) + emit_nmul (one CIOS mul) +
the bit-read lets + a field-wise 10-limb select, emitted ONCE; the recursion loops
via `decreasing : j`. The base case adds emit_from_mont + emit_encode (one more
CIOS mul + the 32 big-endian byte assembles) but those too are emitted once. So
the native binary is ~3 CIOS muls' worth of code, not 256× — a few hundred KB.
"""

import sys, os
sys.path.insert(0, os.path.dirname(__file__))
from p256_scalar import (emit_nsqr, emit_nmul, emit_from_mont, emit_encode,
                         N_LIMBS, MASK, N)

LIMB_MAX = MASK  # 67108863 = 2^26 - 1

# accumulator (result) and base (z) limb-name groups read off the state record
R = [f"s.r{i}" for i in range(N_LIMBS)]
Z = [f"s.z{i}" for i in range(N_LIMBS)]

lets = []

# --- exponent bit extraction (big-endian exp string, MSB-first processing) ---
# At counter j (j>0) we process exponent bit index i = j - 1.
# Guard byte_at OOB at j==0 (we finalize there; pos value is unused but must be
# in-range for the byte_at scan that still gets emitted in the let-chain).
lets.append(("pos", "if s.j == 0 then 0 else s.j - 1"))   # exponent bit index
lets.append(("byte_be", "31 - shr(pos, 3)"))               # big-endian byte index
lets.append(("bpos", "band(pos, 7)"))                      # bit within the byte
lets.append(("hpos", "shl(byte_be, 1)"))                   # hex char index (high nibble)
lets.append(("hc", "byte_at(s.exp, hpos)"))
lets.append(("lc", "byte_at(s.exp, hpos + 1)"))
lets.append(("hv", "if hc <= 57 then hc - 48 else bor(hc, 32) - 87"))
lets.append(("lv", "if lc <= 57 then lc - 48 else bor(lc, 32) - 87"))
lets.append(("bytev", "16 * hv + lv"))
lets.append(("bit", "band(shr(bytev, bpos), 1)"))

# --- sq = nsqr(result)  (always computed) ---
SQ = emit_nsqr(lets, "sq", R)
# --- mul = nmul(sq, z)  (always computed, constant shape) ---
MUL = emit_nmul(lets, "mu", SQ, Z)

# --- field-wise select: result'[i] = if bit==1 then mul[i] else sq[i] ---
Rnew = []
for i in range(N_LIMBS):
    nm = f"rn{i}"
    lets.append((nm, f"if bit == 1 then {MUL[i]} else {SQ[i]}"))
    Rnew.append(nm)

# --- base case (j==0): out of Montgomery, big-endian encode, select byte which ---
plain = emit_from_mont(lets, "unl", R)        # result*R^-1 mod n -> plain limbs
obytes = emit_encode_from_plain = emit_encode  # alias for clarity below
# emit_encode itself calls emit_from_mont; we already un-lifted, so build the 32
# big-endian bytes directly from `plain` using emit_encode's byte-assembly. To
# avoid double-unlift, replicate emit_encode's byte loop over `plain`.
from p256_scalar import _be_byte_index_to_limb_pos, R_BITS
obytes = []
for byte_be in range(32):
    bit_pos = _be_byte_index_to_limb_pos(byte_be)
    parts = []
    for i in range(N_LIMBS):
        lo, hi = R_BITS * i, R_BITS * i + R_BITS
        if lo < bit_pos + 8 and hi > bit_pos:
            local = lo - bit_pos
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
    obytes.append((f"ob{byte_be}", f"band({expr}, 255)"))
for nm, e in obytes:
    lets.append((nm, e))
obnames = [nm for nm, _ in obytes]

# finalize: select byte `which` (0..31)
fparts = []
for i in range(32):
    e = obnames[i]
    if i == 0:
        fparts.append(f"if s.which == 0 then {e}")
    elif i < 31:
        fparts.append(f"if s.which == {i} then {e}")
    else:
        fparts.append(e)
def nest(parts):
    x = parts[-1]
    for y in reversed(parts[:-1]):
        x = f"{y} else {x}"
    return x
finalize = nest(fparts)

# --- recursive record: step result, pass z/which/exp through unchanged, j-1 ---
rnames = [f"r{i}" for i in range(N_LIMBS)]
znames = [f"z{i}" for i in range(N_LIMBS)]
rf = []
for nm, val in zip(rnames, Rnew):
    rf.append(f"{nm}: {val}")
for nm in znames:
    rf.append(f"{nm}: s.{nm}")
rf += ["j: s.j - 1", "which: s.which", "exp: s.exp"]
rec = "p256_ninv_rec(P256NinvState { " + ", ".join(rf) + " })"
body = f"if s.j == 0 then {finalize} else {rec}"

# --- assemble the .verbose text ---
L = ["@verbose 0.1.0", "", "concept P256NinvState",
     '  @intention: "P-256 k^-1 mod n state: Montgomery accumulator result (10 limbs r0..r9) + invariant Montgomery base z=k (10 limbs z0..z9) + bit counter j + which output byte + 64-hex-char big-endian exponent n-2"',
     "  @source: invoices.intent:1", "  fields:"]
for i in range(N_LIMBS):
    L.append(f"    r{i} : number [0, {LIMB_MAX}]")
for i in range(N_LIMBS):
    L.append(f"    z{i} : number [0, {LIMB_MAX}]")
L.append("    j : number [0, 256]")
L.append("    which : number [0, 31]")
L.append("    exp : text")
L += ["", "", "rule p256_ninv_rec",
      '  @intention: "modular inverse k^-1 mod n (P-256 group order) via Fermat z^(n-2), recursive MSB-first square-and-multiply over 256 exponent bits (decreasing j); accumulator + base in Montgomery (CIOS); base case un-lifts and serializes big-endian, returns byte which"',
      "  @source: invoices.intent:1", "  input:", "    s : P256NinvState",
      "  output:", "    out : number", "  logic:"]
for n_, e in lets:
    L.append(f"    let {n_} = {e}")
L.append(f"    out = {body}")
reads = ", ".join([f"s.r{i}" for i in range(N_LIMBS)] +
                  [f"s.z{i}" for i in range(N_LIMBS)] +
                  ["s.j", "s.which", "s.exp"])
L += ["  proofs:", "    purity:", f"      reads : [{reads}]",
      "      calls : [p256_ninv_rec]",
      "    termination:", "      bound : 4000000", "      decreasing : j"]

out_path = os.path.join(os.path.dirname(__file__), "..", "..", "examples", "p256_ninv_rec.verbose")
out_path = os.path.normpath(out_path)
open(out_path, "w").write("\n".join(L) + "\n")
print(f"wrote {out_path} ; lets {len(lets)}")
