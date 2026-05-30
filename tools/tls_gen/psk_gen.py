import sys, os, hashlib; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

# PSK key-schedule entry point (RFC 8446 7.1, external-PSK path):
#   Early Secret = HKDF-Extract(0^32, PSK) = HMAC(0^32, PSK)
#   binder_key   = Derive-Secret(Early, "ext binder", "")
#                = HKDF-Expand-Label(Early, "ext binder", SHA256(""), 32)
# Two rules; both reuse the validated inlined SHA-256/HMAC.

def dispatch(mac, n):
    d=[]
    for i in range(n):
        if i==0: d.append(f"    out = if g.which == 0 then {mac[0]}")
        elif i<n-1: d.append(f"      else if g.which == {i} then {mac[i]}")
        else: d.append(f"      else {mac[n-1]}")
    return d

def hkdf_prefix(label, length, ctxlen):
    full=b"tls13 "+label
    return [(length>>8)&0xff, length&0xff, len(full)] + list(full) + [ctxlen]

# Rule 1: psk_early_secret(psk) = HMAC(key=0^64-padded, msg=psk)
# HMAC key is 0^32 (padded to 64 inside the HMAC), message is the 32-byte PSK.
lets1=[]
key64=["0"]*64
msg=[f"g.p{i}" for i in range(32)]
mac1=emit_hmac(lets1,"pe",key64,msg)
r1=["rule psk_early_secret",
    '  @intention: "Early Secret = HKDF-Extract(0^32, PSK) = HMAC(0^32, PSK) (RFC 8446 7.1 external-PSK)"',
    "  @source: invoices.intent:1","  input:","    g : Psk","  output:","    out : number","  logic:"]
for n,e in lets1: r1.append(f"    let {n} = {e}")
r1 += dispatch(mac1,32)
reads1=", ".join([f"g.p{i}" for i in range(32)]+["g.which"])
r1 += ["  proofs:","    purity:",f"      reads : [{reads1}]","      calls : []","    termination:","      bound : 400000"]

# Rule 2: psk_ext_binder_key(early) = HKDF-Expand-Label(early, "ext binder", SHA256(""), 32)
empty=[str(b) for b in hashlib.sha256(b"").digest()]
lets2=[]
key64b=[f"g.s{i}" for i in range(32)]+["0"]*32
msg2=[str(b) for b in hkdf_prefix(b"ext binder",32,32)] + empty + ["1"]
mac2=emit_hmac(lets2,"bk",key64b,msg2)
r2=["rule psk_ext_binder_key",
    '  @intention: "binder_key = Derive-Secret(Early, ext binder, empty) = HKDF-Expand-Label(Early, ext binder, SHA256(empty), 32)"',
    "  @source: invoices.intent:1","  input:","    g : Early","  output:","    out : number","  logic:"]
for n,e in lets2: r2.append(f"    let {n} = {e}")
r2 += dispatch(mac2,32)
reads2=", ".join([f"g.s{i}" for i in range(32)]+["g.which"])
r2 += ["  proofs:","    purity:",f"      reads : [{reads2}]","      calls : []","    termination:","      bound : 400000"]

lines=["@verbose 0.1.0","","concept Psk",
       '  @intention: "32-byte external PSK + which output byte"',"  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    p{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["","concept Early",
          '  @intention: "32-byte Early Secret + which output byte"',"  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["",""]
lines.append("\n".join(r1))
lines += ["",""]
lines.append("\n".join(r2))
lines.append("")
open("examples/psk_schedule.verbose","w").write("\n".join(lines))
print("wrote examples/psk_schedule.verbose (2 rules)")
