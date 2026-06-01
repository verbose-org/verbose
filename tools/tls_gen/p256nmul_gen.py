"""Generate examples/p256_nmul.verbose — optional BRICK 4 extra-confidence binary:
a*b mod n (P-256 group order), one big-endian output byte per invocation.

Input: a, b each as 10 Montgomery-domain 26-bit limbs (a0..a9, b0..b9) + which.
Mirrors examples/p256_fmul.verbose (brick 1's field multiply) but mod n. The
rule runs emit_nmul (CIOS Montgomery multiply mod n) then emit_encode to bring
the product out of Montgomery and serialize big-endian; `out` selects byte
`which`.
"""
import sys, os
sys.path.insert(0, os.path.dirname(__file__))
from p256_scalar import emit_nmul, emit_encode, N_LIMBS, MASK

A = [f"g.a{i}" for i in range(N_LIMBS)]
Bn = [f"g.b{i}" for i in range(N_LIMBS)]

lets = []
prod = emit_nmul(lets, "m", A, Bn)
obytes = emit_encode(lets, "enc", prod)

disp = []
for i in range(32):
    if i == 0:
        disp.append(f"    out = if g.which == 0 then {obytes[0]}")
    elif i < 31:
        disp.append(f"      else if g.which == {i} then {obytes[i]}")
    else:
        disp.append(f"      else {obytes[31]}")

fields = ([f"    a{i} : number [0, {MASK}]" for i in range(N_LIMBS)]
          + [f"    b{i} : number [0, {MASK}]" for i in range(N_LIMBS)])
reads = ", ".join([f"g.a{i}" for i in range(N_LIMBS)]
                  + [f"g.b{i}" for i in range(N_LIMBS)] + ["g.which"])

L = ["@verbose 0.1.0", "", "concept P256NmulInput",
     '  @intention: "two Z_n scalars as 10 Montgomery 26-bit limbs each, + which output byte"',
     "  @source: invoices.intent:1", "  fields:"]
L += fields
L.append("    which : number [0, 31]")
L += ["", "", "rule p256_nmul",
      '  @intention: "Montgomery CIOS scalar multiply mod n (P-256 group order), output big-endian byte which"',
      "  @source: invoices.intent:1", "  input:", "    g : P256NmulInput",
      "  output:", "    out : number", "  logic:"]
for n_, e in lets:
    L.append(f"    let {n_} = {e}")
L += disp
L += ["  proofs:", "    purity:", f"      reads : [{reads}]", "      calls : []",
      "    termination:", "      bound : 200000"]

out_path = os.path.join(os.path.dirname(__file__), "..", "..", "examples", "p256_nmul.verbose")
out_path = os.path.normpath(out_path)
open(out_path, "w").write("\n".join(L) + "\n")
print(f"wrote {out_path} ; lets {len(lets)}")
