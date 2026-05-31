import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

# Generic HKDF-Extract(salt, ikm) = HMAC(salt, ikm), both 32-byte runtime inputs.
# Needed for the PSK-DHE Handshake Secret: HKDF-Extract(derived, ECDHE) where
# both operands are computed at runtime (the baked-key rules can't express this).

lets = []
key64 = [f"g.salt{i}" for i in range(32)] + ["0"]*32   # salt is the HMAC key (padded to 64)
msg   = [f"g.ikm{i}" for i in range(32)]               # ikm is the HMAC message
mac = emit_hmac(lets, "ex", key64, msg)
disp = []
for i in range(32):
    if i == 0: disp.append(f"    out = if g.which == 0 then {mac[0]}")
    elif i < 31: disp.append(f"      else if g.which == {i} then {mac[i]}")
    else: disp.append(f"      else {mac[31]}")

lines = ["@verbose 0.1.0","","concept Extract",
         '  @intention: "HKDF-Extract inputs: 32-byte salt + 32-byte IKM + which output byte"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    salt{i} : number [0, 255]")
for i in range(32): lines.append(f"    ikm{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["","","rule hkdf_extract",
          '  @intention: "HKDF-Extract(salt, IKM) = HMAC-SHA256(salt, IKM); byte which (RFC 5869 2.2)"',
          "  @source: invoices.intent:1","  input:","    g : Extract","  output:","    out : number","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.extend(disp)
reads = ", ".join([f"g.salt{i}" for i in range(32)] + [f"g.ikm{i}" for i in range(32)] + ["g.which"])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
          "    termination:","      bound : 400000",""]
open("examples/hkdf_extract.verbose","w").write("\n".join(lines))
print("wrote examples/hkdf_extract.verbose")
