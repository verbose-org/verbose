import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

# Generic HKDF-Extract(salt, ikm) = HMAC(salt, ikm), both 32-byte runtime inputs.
# Needed for the PSK-DHE Handshake Secret: HKDF-Extract(derived, ECDHE) where
# both operands are computed at runtime (the baked-key rules can't express this).
#
# The rule returns ALL 32 PRK bytes together as one Digest record (aggregate-
# return arc, slices agg-1/agg-2c 2026-08) — one spawn instead of 32 `which`
# spawns. Field names are b0..b31 so the host unpacks the record with the same
# `rec[f"b{i}"]` loop as every other Digest-returning rule.

lets = []
key64 = [f"g.salt{i}" for i in range(32)] + ["0"]*32   # salt is the HMAC key (padded to 64)
msg   = [f"g.ikm{i}" for i in range(32)]               # ikm is the HMAC message
mac = emit_hmac(lets, "ex", key64, msg)
finalize = "Digest { " + ", ".join(f"b{i}: {mac[i]}" for i in range(32)) + " }"

lines = ["@verbose 0.1.0","","concept Extract",
         '  @intention: "HKDF-Extract inputs: 32-byte salt + 32-byte IKM"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    salt{i} : number [0, 255]")
for i in range(32): lines.append(f"    ikm{i} : number [0, 255]")
lines += ["","","concept Digest",
          '  @intention: "the 32 bytes of HKDF-Extract(salt, IKM), returned together as one record"',
          "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    b{i} : number [0, 255]")
lines += ["","","rule hkdf_extract",
          '  @intention: "HKDF-Extract(salt, IKM) = HMAC-SHA256(salt, IKM) (RFC 5869 2.2); all 32 PRK bytes as one Digest record"',
          "  @source: invoices.intent:1","  input:","    g : Extract","  output:","    out : Digest","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.append(f"    out = {finalize}")
reads = ", ".join([f"g.salt{i}" for i in range(32)] + [f"g.ikm{i}" for i in range(32)])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
          "    termination:","      bound : 400000",""]
open("examples/hkdf_extract.verbose","w").write("\n".join(lines))
print("wrote examples/hkdf_extract.verbose")
