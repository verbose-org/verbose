import sys, os, hmac, hashlib; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

def expand_label(secret, label, context, length):
    info = length.to_bytes(2,'big')+bytes([len(b"tls13 "+label)])+b"tls13 "+label+bytes([len(context)])+context
    return hmac.new(secret, info+b'\x01', hashlib.sha256).digest()[:length]

early = hmac.new(b'\x00'*32, b'\x00'*32, hashlib.sha256).digest()
derived = expand_label(early, b"derived", hashlib.sha256(b"").digest(), 32)

lets = []
key64 = [str(b) for b in derived] + ["0"]*32
msg = [f"s.e{i}" for i in range(32)]
mac = emit_hmac(lets, "hs", key64, msg)
disp = []
for i in range(32):
    if i == 0: disp.append(f"    out = if s.which == 0 then {mac[0]}")
    elif i < 31: disp.append(f"      else if s.which == {i} then {mac[i]}")
    else: disp.append(f"      else {mac[31]}")
lines = ["@verbose 0.1.0","","concept Ecdhe",
         '  @intention: "32-byte ECDHE shared secret (X25519 output) + which output byte"',
         "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    e{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["","","rule handshake_secret",
          '  @intention: "TLS 1.3 Handshake Secret = HKDF-Extract(Derive-Secret(Early,derived,empty), ECDHE); byte which (RFC 8446 7.1)"',
          "  @source: invoices.intent:1","  input:","    s : Ecdhe","  output:","    out : number","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.extend(disp)
reads = ", ".join([f"s.e{i}" for i in range(32)] + ["s.which"])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
          "    termination:","      bound : 400000",""]
open("examples/handshake_secret.verbose","w").write("\n".join(lines))
print("wrote examples/handshake_secret.verbose; derived=" + derived.hex()[:12])
