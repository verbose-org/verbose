import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from field_emit import emit_fmul, emit_finv

# ed_affine: extended point (X,Y,Z,T) -> affine (x,y) = (X/Z, Y/Z) as 20 limbs.
# The field inversion (Fermat) + two multiplies are the crypto (Verbose); the
# host then canonicalizes (mod p) and packs to the 32-byte encodepoint form.
def F(c): return [f"g.{c}{i}" for i in range(10)]
X,Y,Z = F("x"), F("y"), F("z")
lets=[]
zinv = emit_finv(lets, "inv", Z)
x = emit_fmul(lets, "ax", X, zinv)
y = emit_fmul(lets, "ay", Y, zinv)
out = x + y  # 20 limbs

disp=[]
for i in range(20):
    if i==0: disp.append(f"    out = if g.which == 0 then {out[0]}")
    elif i<19: disp.append(f"      else if g.which == {i} then {out[i]}")
    else: disp.append(f"      else {out[19]}")

fields=[]
for c in "xyz":
    for i in range(10): fields.append(f"    {c}{i} : number [0, 67108863]")
reads=", ".join([f"g.{c}{i}" for c in "xyz" for i in range(10)]+["g.which"])

L=["@verbose 0.1.0","","concept EdPoint",
   '  @intention: "Edwards point X,Y,Z (extended coords, T unused for affine) as 30 limbs + which output limb (0..19)"',
   "  @source: invoices.intent:1","  fields:"]
L+=fields
L.append("    which : number [0, 19]")
L+=["","","rule ed_affine",
    '  @intention: "affine (x,y) = (X*Z^-1, Y*Z^-1) mod 2^255-19 via Fermat inverse; returns limb which of x|y"',
    "  @source: invoices.intent:1","  input:","    g : EdPoint","  output:","    out : number","  logic:"]
for n,e in lets: L.append(f"    let {n} = {e}")
L+=disp
L+=["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
    "    termination:","      bound : 2000000"]
open("examples/ed_affine.verbose","w").write("\n".join(L))
print("wrote examples/ed_affine.verbose ; lets", len(lets))
