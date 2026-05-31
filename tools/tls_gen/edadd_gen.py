import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from field_emit import emit_fmul, emit_fadd, emit_fsub, to_limbs, P
from ed25519_ref import d

# Edwards point addition, extended twisted coords (a=-1), unified formula.
# Input: P1=(X1,Y1,Z1,T1), P2=(X2,Y2,Z2,T2), each 10-limb -> 80 number fields.
# Output: P3=(X3,Y3,Z3,T3) = 40 limbs; `which` (0..39) selects the output limb.
two_d = [str(v) for v in to_limbs((2*d) % P)]

def F(name): return [f"g.{name}{i}" for i in range(10)]
X1,Y1,Z1,T1 = F("x1_"),F("y1_"),F("z1_"),F("t1_")
X2,Y2,Z2,T2 = F("x2_"),F("y2_"),F("z2_"),F("t2_")

lets=[]
ymx1 = emit_fsub(lets,"ymx1",Y1,X1)
ymx2 = emit_fsub(lets,"ymx2",Y2,X2)
A    = emit_fmul(lets,"A", ymx1, ymx2)
ypx1 = emit_fadd(lets,"ypx1",Y1,X1)
ypx2 = emit_fadd(lets,"ypx2",Y2,X2)
B    = emit_fmul(lets,"B", ypx1, ypx2)
t1d  = emit_fmul(lets,"t1d", T1, two_d)
C    = emit_fmul(lets,"C", t1d, T2)
zz   = emit_fmul(lets,"zz", Z1, Z2)
Dd   = emit_fadd(lets,"D", zz, zz)          # 2*Z1*Z2
E    = emit_fsub(lets,"E", B, A)
Fp   = emit_fsub(lets,"F", Dd, C)
G    = emit_fadd(lets,"G", Dd, C)
Hh   = emit_fadd(lets,"H", B, A)
X3   = emit_fmul(lets,"X3", E, Fp)
Y3   = emit_fmul(lets,"Y3", G, Hh)
Z3   = emit_fmul(lets,"Z3", Fp, G)
T3   = emit_fmul(lets,"T3", E, Hh)
out = X3+Y3+Z3+T3   # 40 names

disp=[]
for i in range(40):
    if i==0: disp.append(f"    out = if g.which == 0 then {out[0]}")
    elif i<39: disp.append(f"      else if g.which == {i} then {out[i]}")
    else: disp.append(f"      else {out[39]}")

fields=[]
for grp in ["x1_","y1_","z1_","t1_","x2_","y2_","z2_","t2_"]:
    for i in range(10): fields.append(f"    {grp}{i} : number [0, 67108863]")
reads=", ".join([f"g.{grp}{i}" for grp in ["x1_","y1_","z1_","t1_","x2_","y2_","z2_","t2_"] for i in range(10)]+["g.which"])

L=["@verbose 0.1.0","","concept EdAddInput",
   '  @intention: "two Edwards points in extended coords (X,Y,Z,T), 10 limbs each, + which output limb (0..39)"',
   "  @source: invoices.intent:1","  fields:"]
L+=fields
L.append("    which : number [0, 39]")
L+=["","","rule ed_add",
    '  @intention: "twisted-Edwards extended-coord unified point addition P1+P2 mod 2^255-19; returns limb which of X3|Y3|Z3|T3"',
    "  @source: invoices.intent:1","  input:","    g : EdAddInput","  output:","    out : number","  logic:"]
for n,e in lets: L.append(f"    let {n} = {e}")
L+=disp
L+=["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
    "    termination:","      bound : 200000"]
open("examples/ed_add.verbose","w").write("\n".join(L))
print("wrote examples/ed_add.verbose ; lets", len(lets))
