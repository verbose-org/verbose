"""Generate examples/p256_scalarmult.verbose — BRICK 3 of the ECDSA-over-P-256
Verbose arc: scalar multiplication k*P in pure Verbose (recursive
double-and-add over 256 bits), consuming brick 1 (GF(p256) field) and brick 2
(point add / double) emit helpers UNCHANGED.

This mirrors edscalar_gen.py (the Ed25519 arc) but for short-Weierstrass P-256
in Jacobian (X:Y:Z) coordinates. The key structural difference from Edwards is
that the Weierstrass add-2007-bl formula is INCOMPLETE (spurious infinity on
P==Q, and broken on an infinity operand). We handle this with an INFINITY FLAG
carried through the recursion + a constant-shape field-wise select — exactly the
algorithm validated in p256_point.point_mul_inf_ref (65/65 vs the cryptography
oracle, invariant 65/65 distinct-point adds).

LADDER (LSB-first double-and-add, running point P = 2^pos * base):
  state: accumulator Q (30 limbs X|Y|Z), running point P (30 limbs), infinity
         flag inf (1 while Q == O), bit counter j (256..1), which, scalar.
  step (bit = bit `pos = 256-j` of the big-endian scalar):
     Qadd = point_add_core(Q, P)            # raw add-2007-bl, ALWAYS computed
     Q'[limb]  = if bit==1 then (if inf==1 then P[limb] else Qadd[limb])
                            else Q[limb]     # field-wise constant-shape select
     inf'      = if bit==1 then 0 else inf
     P'        = point_double(P)
     recurse j-1
  base (j==0): return limb `which` of Q.

The raw Qadd is CONSUMED only when bit==1 and inf==0, i.e. Q = m*base (a
non-zero multiple) and P = 2^pos*base — two DISTINCT non-infinity points whose
sum add-2007-bl computes correctly. The first set bit (inf==1) routes Q := P
(O + P) around the broken formula. P==Q never occurs for k < n (verified
empirically in the Python mirror over 65 k incl. k=1,2,n-1).

BIT ORDER: P-256 scalars are BIG-ENDIAN (SEC1). The scalar is a 64-hex-char
string, char 0 = most significant nibble. We process LSB-first, so integer bit
position pos = 256 - j maps to big-endian byte byte_be = 31 - (pos>>3), hex char
indices hpos = 2*byte_be and hpos+1, bit (bytev >> (pos&7)) & 1. (Verified
51200/51200 in /tmp/check_bitorder.py.)

LET-CHAIN SIZE: P-256 fmul is ~780 lets. One step is point_add_core (11M+5S)
plus point_double (3M+5S) ≈ 24 field muls ≈ ~19k lets, emitted ONCE; the
recursion loops via `decreasing : j`, so the native binary is one step's worth
of code, not 256×. Expect a large binary (hundreds of KB) and a slow compile.
"""

import sys, os
sys.path.insert(0, os.path.dirname(__file__))
from p256_point import emit_point_add, emit_point_double

N_LIMBS = 10
LIMB_MAX = 67108863  # 2^26 - 1

# field-name groups for the accumulator Q and running point P (coords X,Y,Z)
def grp(letter):
    return ([f"s.{letter}x{i}" for i in range(N_LIMBS)],
            [f"s.{letter}y{i}" for i in range(N_LIMBS)],
            [f"s.{letter}z{i}" for i in range(N_LIMBS)])

QX, QY, QZ = grp("q")   # accumulator Q
PX, PY, PZ = grp("p")   # running point P

lets = []

# --- bit extraction (big-endian scalar, LSB-first processing) ---
# pos = 256 - j  (only used when j>0; at j==0 we finalize, guard byte_at OOB)
lets.append(("pos", "if s.j == 0 then 0 else 256 - s.j"))
lets.append(("byte_be", "31 - shr(pos, 3)"))      # big-endian byte index
lets.append(("bpos", "band(pos, 7)"))              # bit within the byte
lets.append(("hpos", "shl(byte_be, 1)"))           # hex char index (high nibble)
lets.append(("hc", "byte_at(s.scalar, hpos)"))
lets.append(("lc", "byte_at(s.scalar, hpos + 1)"))
lets.append(("hv", "if hc <= 57 then hc - 48 else bor(hc, 32) - 87"))
lets.append(("lv", "if lc <= 57 then lc - 48 else bor(lc, 32) - 87"))
lets.append(("bytev", "16 * hv + lv"))
lets.append(("bit", "band(shr(bytev, bpos), 1)"))

# --- Qadd = point_add_core(Q, P)  (raw add-2007-bl let-chain) ---
QaddX, QaddY, QaddZ = emit_point_add(lets, "add", QX, QY, QZ, PX, PY, PZ)

# --- field-wise nested select for the new accumulator Q ---
# Q'[i] = if bit==1 then (if inf==1 then P[i] else Qadd[i]) else Q[i]
flatQ    = QX + QY + QZ
flatQadd = QaddX + QaddY + QaddZ
flatP    = PX + PY + PZ
Qnew = []
for i in range(30):
    nm = f"qn{i}"
    chosen = f"if s.inf == 1 then {flatP[i]} else {flatQadd[i]}"
    lets.append((nm, f"if bit == 1 then {chosen} else {flatQ[i]}"))
    Qnew.append(nm)

# --- new infinity flag: cleared on the first set bit, sticky thereafter ---
lets.append(("infn", "if bit == 1 then 0 else s.inf"))

# --- Pdbl = point_double(P)  (running point doubles every step) ---
PdblX, PdblY, PdblZ = emit_point_double(lets, "dbl", PX, PY, PZ)
flatPnew = PdblX + PdblY + PdblZ

# --- finalize: select limb `which` of Q (X|Y|Z, 30 limbs, index 0..29) ---
fparts = []
for i in range(30):
    e = flatQ[i]
    if i == 0:
        fparts.append(f"if s.which == 0 then {e}")
    elif i < 29:
        fparts.append(f"if s.which == {i} then {e}")
    else:
        fparts.append(e)
def nest(parts):
    x = parts[-1]
    for y in reversed(parts[:-1]):
        x = f"{y} else {x}"
    return x
finalize = nest(fparts)

# --- recursive record ---
qnames = [f"q{c}{i}" for c in "xyz" for i in range(N_LIMBS)]
pnames = [f"p{c}{i}" for c in "xyz" for i in range(N_LIMBS)]
rf = []
for nm, val in zip(qnames, Qnew):
    rf.append(f"{nm}: {val}")
for nm, val in zip(pnames, flatPnew):
    rf.append(f"{nm}: {val}")
rf += ["inf: infn", "j: s.j - 1", "which: s.which", "scalar: s.scalar"]
rec = "p256_scalarmult(P256ScalarState { " + ", ".join(rf) + " })"
body = f"if s.j == 0 then {finalize} else {rec}"

# --- assemble the .verbose text ---
L = ["@verbose 0.1.0", "", "concept P256ScalarState",
     '  @intention: "P-256 scalar mult state: Jacobian accumulator Q + running point P (X|Y|Z, 30 limbs each) + infinity flag + bit counter j + which + 64-hex-char big-endian scalar"',
     "  @source: invoices.intent:1", "  fields:"]
for c in "xyz":
    for i in range(N_LIMBS):
        L.append(f"    q{c}{i} : number [0, {LIMB_MAX}]")
for c in "xyz":
    for i in range(N_LIMBS):
        L.append(f"    p{c}{i} : number [0, {LIMB_MAX}]")
L.append("    inf : number [0, 1]")
L.append("    j : number [0, 256]")
L.append("    which : number [0, 29]")
L.append("    scalar : text")
L += ["", "", "rule p256_scalarmult",
      '  @intention: "P-256 scalar mult [scalar]P by double-and-add over 256 bits (LSB-first, big-endian scalar); incomplete-add safe via infinity flag; returns limb which of Jacobian accumulator Q (X|Y|Z)"',
      "  @source: invoices.intent:1", "  input:", "    s : P256ScalarState",
      "  output:", "    out : number", "  logic:"]
for n, e in lets:
    L.append(f"    let {n} = {e}")
L.append(f"    out = {body}")
reads = ", ".join([f"s.q{c}{i}" for c in "xyz" for i in range(N_LIMBS)] +
                  [f"s.p{c}{i}" for c in "xyz" for i in range(N_LIMBS)] +
                  ["s.inf", "s.j", "s.which", "s.scalar"])
L += ["  proofs:", "    purity:", f"      reads : [{reads}]",
      "      calls : [p256_scalarmult]",
      "    termination:", "      bound : 4000000", "      decreasing : j"]

out_path = os.path.join(os.path.dirname(__file__), "..", "..", "examples", "p256_scalarmult.verbose")
out_path = os.path.normpath(out_path)
open(out_path, "w").write("\n".join(L) + "\n")
print(f"wrote {out_path} ; lets {len(lets)}")
