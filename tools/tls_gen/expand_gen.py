import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac

def hkdf_label_msg(label, length, context=b""):
    info = length.to_bytes(2,'big') + bytes([len(b"tls13 "+label)]) + b"tls13 "+label + bytes([len(context)]) + context
    return list(info) + [1]

# Each rule returns ALL its output bytes together as one record (aggregate-
# return arc, slices agg-1/agg-2c 2026-08). The two rules have different
# output WIDTHS (16 vs 12), and a record's width is its concept declaration —
# so each rule gets its own output concept (KeyBytes / IvBytes), emitted from
# the same parameterised loop. Field names stay b0..bN-1 in both so the host
# unpacks either record with the same `rec[f"b{i}"]` loop.
def emit_rule(name, intent, label, length, out_concept):
    lets = []
    key64 = [f"s.s{i}" for i in range(32)] + ["0"]*32
    msg = [str(b) for b in hkdf_label_msg(label, length)]
    mac = hmac(lets, name, key64, msg)
    finalize = out_concept + " { " + ", ".join(f"b{i}: {mac[i]}" for i in range(length)) + " }"
    out = [f"rule {name}", f'  @intention: "{intent}"', "  @source: invoices.intent:1",
           "  input:", "    s : Secret", "  output:", f"    out : {out_concept}", "  logic:"]
    for nm,e in lets: out.append(f"    let {nm} = {e}")
    out.append(f"    out = {finalize}")
    reads = ", ".join([f"s.s{i}" for i in range(32)])
    out += ["  proofs:", "    purity:", f"      reads : [{reads}]", "      calls : []",
            "    termination:", "      bound : 400000"]
    return "\n".join(out)

def emit_concept(name, intent, length):
    out = [f"concept {name}", f'  @intention: "{intent}"',
           "  @source: invoices.intent:1", "  fields:"]
    for i in range(length): out.append(f"    b{i} : number [0, 255]")
    return "\n".join(out)

lines = ["@verbose 0.1.0","","concept Secret",
         '  @intention: "32-byte HKDF secret"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
lines += ["",""]
lines.append(emit_concept("KeyBytes",
    "the 16 output bytes of HKDF-Expand-Label(secret, key, empty, 16), returned together as one record", 16))
lines += ["",""]
lines.append(emit_concept("IvBytes",
    "the 12 output bytes of HKDF-Expand-Label(secret, iv, empty, 12), returned together as one record", 12))
lines += ["",""]
lines.append(emit_rule("expand_key",
    "HKDF-Expand-Label(secret, key, empty, 16) per RFC 8446 7.1 (TLS record key); all 16 bytes as one KeyBytes record", b"key", 16, "KeyBytes"))
lines += ["",""]
lines.append(emit_rule("expand_iv",
    "HKDF-Expand-Label(secret, iv, empty, 12) per RFC 8446 7.1 (TLS record IV); all 12 bytes as one IvBytes record", b"iv", 12, "IvBytes"))
lines.append("")
open("examples/hkdf_expand_label.verbose","w").write("\n".join(lines))
print("wrote examples/hkdf_expand_label.verbose")
