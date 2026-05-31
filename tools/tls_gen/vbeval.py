"""Evaluate an emitted Verbose let-chain in Python with the SAME semantics the
native backend uses, so a generator's algorithm can be validated in
milliseconds (no native compile). This is the key debugging tool: if vbeval of
the emitted exprs matches the native binary but not the truth, the bug is in the
generator's transcription, not in native codegen.

Semantics that matter (learned the hard way):
- numbers are i64, arithmetic wraps mod 2^64
- NO operator precedence: + - * evaluate STRICTLY LEFT-TO-RIGHT
- shr is LOGICAL (unsigned on the 2^64 representation); shl wraps mod 2^64
- band/bor/bxor/bnot are bitwise on the 2^64 rep; / is signed? NO — treat like
  the backend: we only ever emit '/' we've verified, so vbeval mirrors logical
  shr-based code. (We avoid '/' in generators now; bias-ashr instead.)
"""
M64 = 1 << 64
def _wrap(x): return x  # Python big-int; callers mask where the rule masks

def ev(e, env):
    e = e.strip()
    head = e.split("(")[0] if "(" in e else ""
    if head in ("band","bor","bxor","shl","shr","bnot") and e.endswith(")"):
        # parse balanced args
        depth=0
        for i,c in enumerate(e):
            if c=="(":
                depth+=1
                if depth==1: start=i+1
            elif c==")":
                depth-=1
                if depth==0: inner=e[start:i]; break
        args=[]; d=0; cur=""
        for c in inner:
            if c=="(": d+=1; cur+=c
            elif c==")": d-=1; cur+=c
            elif c=="," and d==0: args.append(cur); cur=""
            else: cur+=c
        args.append(cur)
        v=[ev(a,env) for a in args]
        if head=="band": return v[0] & v[1]
        if head=="bor":  return v[0] | v[1]
        if head=="bxor": return v[0] ^ v[1]
        if head=="bnot": return (~v[0]) % M64
        if head=="shl":  return (v[0] << v[1]) % M64
        if head=="shr":  return (v[0] % M64) >> v[1]
    # top-level + - * left-to-right
    toks=[]; d=0; cur=""
    for c in e:
        if c=="(": d+=1; cur+=c
        elif c==")": d-=1; cur+=c
        elif c in "+-*" and d==0: toks.append(cur.strip()); toks.append(c); cur=""
        else: cur+=c
    toks.append(cur.strip())
    def atom(t):
        t=t.strip()
        if t in env: return env[t]
        if "(" in t: return ev(t,env)
        return int(t)
    acc=atom(toks[0]); i=1
    while i<len(toks):
        op=toks[i]; r=atom(toks[i+1])
        acc = acc+r if op=="+" else acc-r if op=="-" else acc*r
        i+=2
    return acc

def run_lets(lets, env):
    for n,e in lets: env[n]=ev(e,env)
    return env
