import subprocess, sys, random, hmac, hashlib, json

def expand_label(secret, label, ctx, length):
    info = length.to_bytes(2,'big')+bytes([len(b"tls13 "+label)])+b"tls13 "+label+bytes([len(ctx)])+ctx
    return hmac.new(secret, info+b'\x01', hashlib.sha256).digest()[:length]
def extract(salt, ikm): return hmac.new(salt, ikm, hashlib.sha256).digest()

# Each rule compiled to its own binary path /tmp/sc_<rule>.
# All six rules return RECORDS since 2026-08-29 (aggregate-return arc):
# ONE spawn yields all n output bytes as {"b0":...,...,"b(n-1)":...} JSON,
# where the old `which` interface took n spawns of one byte each.
def runb(rule, secret, thash, n=32):
    args=[str(b) for b in secret]+[str(b) for b in thash]
    r=subprocess.run(["/tmp/sc_"+rule]+args, capture_output=True, text=True, timeout=600)
    s=r.stdout.strip()
    if s=="": sys.exit(3)
    rec=json.loads(s)
    if len(rec)!=n: sys.exit(3)
    return bytes(rec[f"b{i}"] for i in range(n))

random.seed(131)
zero=bytes(32)
for _ in range(2):
    secret=bytes(random.randrange(256) for _ in range(32))
    thash=bytes(random.randrange(256) for _ in range(32))
    checks = [
        ("derive_derived",     runb("derive_derived", secret, thash),
                               expand_label(secret, b"derived", hashlib.sha256(b"").digest(), 32)),
        ("master_secret",      runb("master_secret", secret, thash),
                               extract(secret, zero)),
        ("derive_c_hs_traffic",runb("derive_c_hs_traffic", secret, thash),
                               expand_label(secret, b"c hs traffic", thash, 32)),
        ("derive_s_ap_traffic",runb("derive_s_ap_traffic", secret, thash),
                               expand_label(secret, b"s ap traffic", thash, 32)),
        ("derive_c_ap_traffic",runb("derive_c_ap_traffic", secret, thash),
                               expand_label(secret, b"c ap traffic", thash, 32)),
        ("finished_key",       runb("finished_key", secret, thash),
                               expand_label(secret, b"finished", b"", 32)),
    ]
    for name,got,exp in checks:
        if got != exp:
            print("FAIL", name); sys.exit(1)
print("TLS_SCHEDULE_OK")
sys.exit(0)
