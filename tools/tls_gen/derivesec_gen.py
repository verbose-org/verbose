import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

LABEL = b"s hs traffic"
full = b"tls13 " + LABEL
fixed_prefix = [0, 32, len(full)] + list(full) + [32]

lets = []
key64 = [f"s.s{i}" for i in range(32)] + ["0"]*32
msg = [str(b) for b in fixed_prefix] + [f"s.t{i}" for i in range(32)] + ["1"]
mac = emit_hmac(lets, "ds", key64, msg)
# All 32 output bytes are returned TOGETHER as one Digest record (aggregate-
# return arc, slices agg-1/agg-2c 2026-08): one invocation yields the whole
# derived secret as {"b0":...,...,"b31":...} JSON instead of one byte per
# `which` spawn.
finalize = "Digest { " + ", ".join(f"b{i}: {mac[i]}" for i in range(32)) + " }"
lines = ["@verbose 0.1.0","","concept DeriveInput",
         '  @intention: "32-byte secret + 32-byte transcript hash"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
for i in range(32): lines.append(f"    t{i} : number [0, 255]")
lines += ["","","concept Digest",
          '  @intention: "the 32 output bytes of Derive-Secret(secret, s-hs-traffic, transcript), returned together as one record"',
          "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    b{i} : number [0, 255]")
lines += ["","","rule derive_s_hs_traffic",
          '  @intention: "Derive-Secret(secret, s-hs-traffic, transcript) = HKDF-Expand-Label(secret, label, Transcript-Hash, 32); all 32 bytes as one Digest record (RFC 8446 7.1)"',
          "  @source: invoices.intent:1","  input:","    s : DeriveInput","  output:","    out : Digest","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.append(f"    out = {finalize}")
reads = ", ".join([f"s.s{i}" for i in range(32)] + [f"s.t{i}" for i in range(32)])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
          "    termination:","      bound : 400000",""]
open("examples/derive_secret.verbose","w").write("\n".join(lines))
print("wrote examples/derive_secret.verbose")
