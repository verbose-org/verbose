"""Generate examples/p256_ninv.verbose — BRICK 4 deliverable: modular inverse
k^-1 mod n (P-256 group order) in pure Verbose, output big-endian byte `which`.

Input convention: k arrives as 10 PLAIN base-2^26 little-endian limbs (k0..k9),
each in [0, 2^26). The rule lifts k into the Montgomery domain (emit_to_mont,
one CIOS mul by R^2 mod n), runs emit_ninv (z^(n-2) via square-and-multiply,
256-bit straight-line let-chain — NOT recursive), then emit_encode brings the
result out of Montgomery and serializes big-endian. `out` selects byte `which`.

Mirrors edaffine_gen.py's shape (the Ed25519 finv-producing binary) but in the
scalar field mod n. Straight-line ninv => one large flat binary, no deep
runtime stack (no recursion).
"""
import sys, os
sys.path.insert(0, os.path.dirname(__file__))
from p256_scalar import emit_to_mont, emit_ninv, emit_encode, N_LIMBS, MASK

K = [f"g.k{i}" for i in range(N_LIMBS)]

lets = []
k_mont = emit_to_mont(lets, "lift", K)          # plain limbs -> Montgomery
inv = emit_ninv(lets, "inv", k_mont)            # k^-1 mod n (Montgomery)
obytes = emit_encode(lets, "enc", inv)          # 32 big-endian bytes

# finalize: select byte `which` (0..31)
disp = []
for i in range(32):
    if i == 0:
        disp.append(f"    out = if g.which == 0 then {obytes[0]}")
    elif i < 31:
        disp.append(f"      else if g.which == {i} then {obytes[i]}")
    else:
        disp.append(f"      else {obytes[31]}")

fields = [f"    k{i} : number [0, {MASK}]" for i in range(N_LIMBS)]
reads = ", ".join([f"g.k{i}" for i in range(N_LIMBS)] + ["g.which"])

L = ["@verbose 0.1.0", "", "concept P256NinvInput",
     '  @intention: "a 256-bit scalar k as 10 plain base-2^26 little-endian limbs + which output byte"',
     "  @source: invoices.intent:1", "  fields:"]
L += fields
L.append("    which : number [0, 31]")
L += ["", "", "rule p256_ninv",
      '  @intention: "modular inverse k^-1 mod n (P-256 group order) via Fermat z^(n-2), Montgomery CIOS; returns big-endian byte which of the 32-byte result"',
      "  @source: invoices.intent:1", "  input:", "    g : P256NinvInput",
      "  output:", "    out : number", "  logic:"]
for n_, e in lets:
    L.append(f"    let {n_} = {e}")
L += disp
L += ["  proofs:", "    purity:", f"      reads : [{reads}]", "      calls : []",
      "    termination:", "      bound : 200000"]

out_path = os.path.join(os.path.dirname(__file__), "..", "..", "examples", "p256_ninv.verbose")
out_path = os.path.normpath(out_path)
open(out_path, "w").write("\n".join(L) + "\n")
print(f"wrote {out_path} ; lets {len(lets)}")
