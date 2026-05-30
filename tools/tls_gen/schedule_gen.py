import sys, os, hashlib; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import hmac as emit_hmac

# Full TLS 1.3 key-schedule rules (RFC 8446 7.1) as baked-label rules, one file.
# reads: declares g.t* ONLY when the rule's message actually uses the transcript
# context (verifier rejects declared-but-unused reads).

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

def make_rule(name, intent, msg_tokens, uses_thash):
    lets=[]
    key64=[f"g.s{i}" for i in range(32)]+["0"]*32
    mac=emit_hmac(lets,name,key64,msg_tokens)
    out=[f"rule {name}",f'  @intention: "{intent}"',"  @source: invoices.intent:1",
         "  input:","    g : SecretCtx","  output:","    out : number","  logic:"]
    for n,e in lets: out.append(f"    let {n} = {e}")
    out += dispatch(mac,32)
    reads_list=[f"g.s{i}" for i in range(32)]
    if uses_thash: reads_list += [f"g.t{i}" for i in range(32)]
    reads_list += ["g.which"]
    out += ["  proofs:","    purity:",f"      reads : [{', '.join(reads_list)}]","      calls : []",
            "    termination:","      bound : 400000"]
    return "\n".join(out)

empty_hash=[str(b) for b in hashlib.sha256(b"").digest()]

rules = []
# Derive-Secret(secret,"derived","") — context = SHA256(""), a constant, so NOT g.t*
rules.append(make_rule("derive_derived",
    "Derive-Secret(secret, derived, empty) = HKDF-Expand-Label(secret, derived, SHA256(empty), 32)",
    [str(b) for b in hkdf_prefix(b"derived",32,32)] + empty_hash + ["1"], False))
# Master Secret = HKDF-Extract(derived, 0^32) = HMAC(derived, zero)
rules.append(make_rule("master_secret",
    "Master Secret = HKDF-Extract(derived, 0^32) = HMAC(derived, zero) (RFC 8446 7.1)",
    ["0"]*32, False))
# Derive-Secret with transcript context -> uses g.t*
for lab, fn, desc in [
    (b"c hs traffic","derive_c_hs_traffic","client handshake traffic secret"),
    (b"s ap traffic","derive_s_ap_traffic","server application traffic secret"),
    (b"c ap traffic","derive_c_ap_traffic","client application traffic secret"),
]:
    rules.append(make_rule(fn,
        f"Derive-Secret(secret, {lab.decode()}, transcript) [RFC 8446 7.1] -> {desc}",
        [str(b) for b in hkdf_prefix(lab,32,32)] + [f"g.t{i}" for i in range(32)] + ["1"], True))
# finished_key = HKDF-Expand-Label(secret,"finished","",32) — empty context, NOT g.t*
rules.append(make_rule("finished_key",
    "finished_key = HKDF-Expand-Label(secret, finished, empty, 32) (RFC 8446 4.4.4)",
    [str(b) for b in hkdf_prefix(b"finished",32,0)] + ["1"], False))

lines=["@verbose 0.1.0","","concept SecretCtx",
       '  @intention: "32-byte secret + 32-byte transcript context (used only by transcript rules) + which output byte"',
       "  @source: invoices.intent:1","  fields:"]
for i in range(32): lines.append(f"    s{i} : number [0, 255]")
for i in range(32): lines.append(f"    t{i} : number [0, 255]")
lines.append("    which : number [0, 31]")
lines += ["",""]
lines.append("\n\n\n".join(rules))
lines.append("")
open("examples/tls_schedule.verbose","w").write("\n".join(lines))
print("wrote examples/tls_schedule.verbose (6 rules)")
