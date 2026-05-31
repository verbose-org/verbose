import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from field_emit import emit_fmul, emit_fadd, emit_fsub, to_limbs, P
from ed25519_ref import d

two_d = [str(v) for v in to_limbs((2*d) % P)]

def emit_edadd(lets, pfx, Pt, Qt):
    # Pt, Qt: each a 4-tuple of 10-limb name lists (X,Y,Z,T). Returns (X3,Y3,Z3,T3).
    X1,Y1,Z1,T1 = Pt; X2,Y2,Z2,T2 = Qt
    a=emit_fmul(lets,pfx+"A", emit_fsub(lets,pfx+"yx1",Y1,X1), emit_fsub(lets,pfx+"yx2",Y2,X2))
    b=emit_fmul(lets,pfx+"B", emit_fadd(lets,pfx+"yp1",Y1,X1), emit_fadd(lets,pfx+"yp2",Y2,X2))
    c=emit_fmul(lets,pfx+"C", emit_fmul(lets,pfx+"td",T1,two_d), T2)
    zz=emit_fmul(lets,pfx+"zz",Z1,Z2); dd=emit_fadd(lets,pfx+"D",zz,zz)
    e=emit_fsub(lets,pfx+"E",b,a); f=emit_fsub(lets,pfx+"F",dd,c)
    g=emit_fadd(lets,pfx+"G",dd,c); h=emit_fadd(lets,pfx+"H",b,a)
    return (emit_fmul(lets,pfx+"X3",e,f), emit_fmul(lets,pfx+"Y3",g,h),
            emit_fmul(lets,pfx+"Z3",f,g), emit_fmul(lets,pfx+"T3",e,h))

def Q(n): return ([f"s.q{c}{i}" for i in range(10)] for c in n) if False else None
def grp(letter): return ([f"s.{letter}x{i}" for i in range(10)],
                         [f"s.{letter}y{i}" for i in range(10)],
                         [f"s.{letter}z{i}" for i in range(10)],
                         [f"s.{letter}t{i}" for i in range(10)])
Qt = grp("q")   # accumulator
Pt = grp("p")   # current point

lets=[]
# bit index = 256 - j (only used when j>0); at j==0 we finalize.
lets.append(("bi", "256 - s.j"))
lets.append(("bidx", "shr(bi, 3)"))
lets.append(("bpos", "band(bi, 7)"))
lets.append(("hpos", "shl(bidx, 1)"))
lets.append(("hc", "byte_at(s.scalar, hpos)"))
lets.append(("lc", "byte_at(s.scalar, hpos + 1)"))
lets.append(("hv", "if hc <= 57 then hc - 48 else bor(hc, 32) - 87"))
lets.append(("lv", "if lc <= 57 then lc - 48 else bor(lc, 32) - 87"))
lets.append(("bytev", "16 * hv + lv"))
lets.append(("bit", "band(shr(bytev, bpos), 1)"))
# select via if/then/else (mask 0-bit proved unreliable at runtime)

# Qadd = ed_add(Q, P)
Qadd = emit_edadd(lets, "add_", Qt, Pt)
# Qnew[limb] = bit ? Qadd : Q  (branch-free select via mask)
Qnew=[]
flatQ = Qt[0]+Qt[1]+Qt[2]+Qt[3]
flatQadd = Qadd[0]+Qadd[1]+Qadd[2]+Qadd[3]
for i in range(40):
    nm=f"qn{i}"; lets.append((nm, f"if bit == 1 then {flatQadd[i]} else {flatQ[i]}")); Qnew.append(nm)
# Pnew = ed_add(P,P) (double)
Pdbl = emit_edadd(lets, "dbl_", Pt, Pt)
flatPnew = Pdbl[0]+Pdbl[1]+Pdbl[2]+Pdbl[3]

# finalize: which limb of Q
fparts=[]
for i in range(40):
    e=flatQ[i]
    if i==0: fparts.append(f"if s.which == 0 then {e}")
    elif i<39: fparts.append(f"if s.which == {i} then {e}")
    else: fparts.append(e)
def nest(p):
    x=p[-1]
    for y in reversed(p[:-1]): x=f"{y} else {x}"
    return x
finalize=nest(fparts)

# recursive record
rf=[]
qnames=[f"q{c}{i}" for c in "xyzt" for i in range(10)]
pnames=[f"p{c}{i}" for c in "xyzt" for i in range(10)]
for nm,val in zip(qnames, Qnew): rf.append(f"{nm}: {val}")
for nm,val in zip(pnames, flatPnew): rf.append(f"{nm}: {val}")
rf+=["j: s.j - 1","which: s.which","scalar: s.scalar"]
rec="ed_scalarmult(EdScalarState { "+", ".join(rf)+" })"
body=f"if s.j == 0 then {finalize} else {rec}"

L=["@verbose 0.1.0","","concept EdScalarState",
   '  @intention: "Edwards scalar mult state: accumulator Q + point P (extended coords, 40 limbs each) + bit counter j + which + 64-hex-char scalar"',
   "  @source: invoices.intent:1","  fields:"]
for c in "xyzt":
    for i in range(10): L.append(f"    q{c}{i} : number [0, 67108863]")
for c in "xyzt":
    for i in range(10): L.append(f"    p{c}{i} : number [0, 67108863]")
L.append("    j : number [0, 256]")
L.append("    which : number [0, 39]")
L.append("    scalar : text")
L+=["","","rule ed_scalarmult",
    '  @intention: "Ed25519 scalar mult [scalar]P by double-and-add over 256 bits (LSB-first); returns limb which of accumulator Q (X|Y|Z|T)"',
    "  @source: invoices.intent:1","  input:","    s : EdScalarState","  output:","    out : number","  logic:"]
for n,e in lets: L.append(f"    let {n} = {e}")
L.append(f"    out = {body}")
reads=", ".join([f"s.q{c}{i}" for c in "xyzt" for i in range(10)]+
                [f"s.p{c}{i}" for c in "xyzt" for i in range(10)]+["s.j","s.which","s.scalar"])
L+=["  proofs:","    purity:",f"      reads : [{reads}]","      calls : [ed_scalarmult]",
    "    termination:","      bound : 2000000","      decreasing : j"]
open("examples/ed_scalarmult.verbose","w").write("\n".join(L))
print("wrote examples/ed_scalarmult.verbose ; lets", len(lets))
