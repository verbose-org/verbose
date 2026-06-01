import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac

def hkdf_label_msg(label, length, context=b""):
    info = length.to_bytes(2,'big') + bytes([len(b"tls13 "+label)]) + b"tls13 "+label + bytes([len(context)]) + context
    return list(info) + [1]

def emit_rule(name, intent, label, length):
    lets = []
    key64 = [f"s.s{i}" for i in range(32)] + ["0"]*32
    msg = [str(b) for b in hkdf_label_msg(label, length)]
    mac = hmac(lets, name, key64, msg)
    disp = []
    for i in range(length):
        if i == 0: disp.append(f"    out = if s.which == 0 then {mac[0]}")
        elif i < length-1: disp.append(f"      else if s.which == {i} then {mac[i]}")
        else: disp.append(f"      else {mac[length-1]}")
    out = [f"rule {name}", f'  @intention: "{intent}"', "  @source: invoices.intent:1",
           "  input:", "    s : Secret", "  output:", "    out : number", "  logic:"]
    for nm,e in lets: out.append(f"    let {nm} = {e}")
    out.extend(disp)
    reads = ", ".join([f"s.s{i}" for i in range(32)] + ["s.which"])
    out += ["  proofs:", "    purity:", f"      reads : [{reads}]", "      calls : []",
            "    termination:", "      bound : 400000"]
    return "\n".join(out)

lines = ["@verbose 0.1.0","","concept Secret",
         '  @intention: "32-byte HKDF secret + which output byte"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
lines.append("    which : number [0, 15]")
lines += ["",""]
lines.append(emit_rule("expand_key",
    "HKDF-Expand-Label(secret, key, empty, 16) per RFC 8446 7.1 (TLS record key)", b"key", 16))
lines += ["",""]
lines.append(emit_rule("expand_iv",
    "HKDF-Expand-Label(secret, iv, empty, 12) per RFC 8446 7.1 (TLS record IV)", b"iv", 12))
lines.append("")
open("examples/hkdf_expand_label.verbose","w").write("\n".join(lines))
print("wrote examples/hkdf_expand_label.verbose")
