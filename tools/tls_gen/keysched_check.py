import subprocess, sys, random, hmac, hashlib

def expand_label(secret, label, ctx, length):
    info = length.to_bytes(2,'big')+bytes([len(b"tls13 "+label)])+b"tls13 "+label+bytes([len(ctx)])+ctx
    return hmac.new(secret, info+b'\x01', hashlib.sha256).digest()[:length]
def extract(salt, ikm): return hmac.new(salt, ikm, hashlib.sha256).digest()

def ref(ecdhe, thash):
    early = extract(b'\x00'*32, b'\x00'*32)
    derived = expand_label(early, b"derived", hashlib.sha256(b"").digest(), 32)
    hs = extract(derived, ecdhe)
    s_hs = expand_label(hs, b"s hs traffic", thash, 32)
    return hs, s_hs, expand_label(s_hs, b"key", b"", 16), expand_label(s_hs, b"iv", b"", 12)

def run(binp, args, n):
    out=[]
    for w in range(n):
        r=subprocess.run([binp]+args+[str(w)],capture_output=True,text=True,timeout=600)
        s=r.stdout.strip()
        if s=="": sys.exit(2)
        out.append(int(s))
    return bytes(out)

def vrun(ecdhe, thash):
    hs   = run("/tmp/ks_hs", [str(b) for b in ecdhe], 32)
    s_hs = run("/tmp/ks_ds", [str(b) for b in hs]+[str(b) for b in thash], 32)
    return hs, s_hs, run("/tmp/ks_ek", [str(b) for b in s_hs], 16), run("/tmp/ks_ei", [str(b) for b in s_hs], 12)

random.seed(91)
for _ in range(3):
    ecdhe = bytes(random.randrange(256) for _ in range(32))
    thash = bytes(random.randrange(256) for _ in range(32))
    if ref(ecdhe, thash) != vrun(ecdhe, thash):
        sys.exit(1)
print("KEYSCHED_ASSEMBLY_OK")
sys.exit(0)
