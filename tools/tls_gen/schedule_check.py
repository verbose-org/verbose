import subprocess, sys, random, hmac, hashlib

def expand_label(secret, label, ctx, length):
    info = length.to_bytes(2,'big')+bytes([len(b"tls13 "+label)])+b"tls13 "+label+bytes([len(ctx)])+ctx
    return hmac.new(secret, info+b'\x01', hashlib.sha256).digest()[:length]
def extract(salt, ikm): return hmac.new(salt, ikm, hashlib.sha256).digest()

def run(rule, secret, thash, n=32):
    out=[]
    for w in range(n):
        args=[str(b) for b in secret]+[str(b) for b in thash]+[str(w)]
        r=subprocess.run(["/tmp/sched"]+[rule]+args, capture_output=True, text=True, timeout=600)
        # placeholder; replaced below by per-rule binaries
        s=r.stdout.strip()
        if s=="": sys.exit(3)
        out.append(int(s))
    return bytes(out)

# Each rule compiled to its own binary path /tmp/sc_<rule>
def runb(rule, secret, thash, n=32):
    out=[]
    for w in range(n):
        args=[str(b) for b in secret]+[str(b) for b in thash]+[str(w)]
        r=subprocess.run(["/tmp/sc_"+rule]+args, capture_output=True, text=True, timeout=600)
        s=r.stdout.strip()
        if s=="": sys.exit(3)
        out.append(int(s))
    return bytes(out)

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
