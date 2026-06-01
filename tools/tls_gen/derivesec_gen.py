import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

LABEL = b"s hs traffic"
full = b"tls13 " + LABEL
fixed_prefix = [0, 32, len(full)] + list(full) + [32]

lets = []
key64 = [f"s.s{i}" for i in range(32)] + ["0"]*32
msg = [str(b) for b in fixed_prefix] + [f"s.t{i}" for i in range(32)] + ["1"]
mac = emit_hmac(lets, "ds", key64, msg)
disp = []
for i in range(32):
    if i == 0: disp.append(f"    out = if s.which == 0 then {mac[0]}")
    elif i < 31: disp.append(f"      else if s.which == {i} then {mac[i]}")
    else: disp.append(f"      else {mac[31]}")
lines = ["@verbose 0.1.0","","concept DeriveInput",
         '  @intention: "32-byte secret + 32-byte transcript hash + which output byte"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
for i in range(32): lines.append(f"    t{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["","","rule derive_s_hs_traffic",
          '  @intention: "Derive-Secret(secret, s-hs-traffic, transcript) = HKDF-Expand-Label(secret, label, Transcript-Hash, 32); byte which (RFC 8446 7.1)"',
          "  @source: invoices.intent:1","  input:","    s : DeriveInput","  output:","    out : number","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.extend(disp)
reads = ", ".join([f"s.s{i}" for i in range(32)] + [f"s.t{i}" for i in range(32)] + ["s.which"])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
          "    termination:","      bound : 400000",""]
open("examples/derive_secret.verbose","w").write("\n".join(lines))
print("wrote examples/derive_secret.verbose")
