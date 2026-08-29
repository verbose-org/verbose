import subprocess, sys, random, hmac, hashlib, json

def expand_label(secret, label, ctx, length):
    info = length.to_bytes(2,'big')+bytes([len(b"tls13 "+label)])+b"tls13 "+label+bytes([len(ctx)])+ctx
    return hmac.new(secret, info+b'\x01', hashlib.sha256).digest()[:length]
def extract(salt, ikm): return hmac.new(salt, ikm, hashlib.sha256).digest()

def ref(ecdhe, thash):
    early = extract(b'\x00'*32, b'\x00'*32)
    derived = expand_label(early, b"derived", hashlib.sha256(b"").digest(), 32)
    hs = extract(derived, ecdhe)
    s_hs = expand_label(hs, b"s hs traffic", thash, 32)
    # the six tls_schedule rules, chained the way tls_server chains them
    dd2 = expand_label(hs, b"derived", hashlib.sha256(b"").digest(), 32)
    master = extract(dd2, b'\x00'*32)
    return (hs, s_hs,
            expand_label(s_hs, b"key", b"", 16), expand_label(s_hs, b"iv", b"", 12),
            dd2, master,
            expand_label(hs, b"c hs traffic", thash, 32),
            expand_label(master, b"s ap traffic", thash, 32),
            expand_label(master, b"c ap traffic", thash, 32),
            expand_label(s_hs, b"finished", b"", 32))

def run(binp, args, n):
    # All ten rules return RECORDS since 2026-08-29 (aggregate-return arc):
    # ONE spawn yields every output byte as {"b0":...,...,"b(n-1)":...} JSON,
    # where the old `which` interface took n spawns of one byte each.
    r=subprocess.run([binp]+args,capture_output=True,text=True,timeout=600)
    s=r.stdout.strip()
    if s=="": sys.exit(2)
    rec=json.loads(s)
    if len(rec)!=n: sys.exit(2)
    return bytes(rec[f"b{i}"] for i in range(n))

def vrun(ecdhe, thash):
    zero=[str(0)]*32
    hs   = run("/tmp/ks_hs", [str(b) for b in ecdhe], 32)
    s_hs = run("/tmp/ks_ds", [str(b) for b in hs]+[str(b) for b in thash], 32)
    ek   = run("/tmp/ks_ek", [str(b) for b in s_hs], 16)
    ei   = run("/tmp/ks_ei", [str(b) for b in s_hs], 12)
    # the six tls_schedule rules (record spawns since 2026-08-29, tranche 3);
    # every upstream value below is the VERBOSE-computed one, so the chain is honest
    dd2    = run("/tmp/ks_dd", [str(b) for b in hs]+zero, 32)
    master = run("/tmp/ks_ms", [str(b) for b in dd2]+zero, 32)
    c_hs   = run("/tmp/ks_ch", [str(b) for b in hs]+[str(b) for b in thash], 32)
    s_ap   = run("/tmp/ks_sa", [str(b) for b in master]+[str(b) for b in thash], 32)
    c_ap   = run("/tmp/ks_ca", [str(b) for b in master]+[str(b) for b in thash], 32)
    fk     = run("/tmp/ks_fk", [str(b) for b in s_hs]+zero, 32)
    return hs, s_hs, ek, ei, dd2, master, c_hs, s_ap, c_ap, fk

random.seed(91)
for _ in range(3):
    ecdhe = bytes(random.randrange(256) for _ in range(32))
    thash = bytes(random.randrange(256) for _ in range(32))
    if ref(ecdhe, thash) != vrun(ecdhe, thash):
        sys.exit(1)
print("KEYSCHED_ASSEMBLY_OK")
sys.exit(0)
