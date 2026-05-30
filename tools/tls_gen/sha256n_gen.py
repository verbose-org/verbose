import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from sha2_emit import K, rotr, s0, s1, S0, S1, ch, maj
Mb=0xffffffff

def emit_compress(lets, prefix, state, blockwords):
    W=list(blockwords)
    for t in range(16,64):
        wn=f"{prefix}_w{t}"
        lets.append((wn, f"band({s1(W[t-2])} + {W[t-7]} + {s0(W[t-15])} + {W[t-16]}, {Mb})")); W.append(wn)
    a,b,c,d,e,f,g,h = state
    for t in range(64):
        t1=f"{prefix}_t1_{t}"; lets.append((t1, f"band({h} + {S1(e)} + {ch(e,f,g)} + {K[t]} + {W[t]}, {Mb})"))
        t2=f"{prefix}_t2_{t}"; lets.append((t2, f"band({S0(a)} + {maj(a,b,c)}, {Mb})"))
        ne=f"{prefix}_e{t}"; lets.append((ne, f"band({d} + {t1}, {Mb})"))
        na=f"{prefix}_a{t}"; lets.append((na, f"band({t1} + {t2}, {Mb})"))
        h,g,f,e,d,c,b,a = g,f,e,ne,c,b,a,na
    ns=[f"{prefix}_h{k}" for k in range(8)]
    for k,(o,n) in enumerate(zip(state,[a,b,c,d,e,f,g,h])): lets.append((ns[k], f"band({o} + {n}, {Mb})"))
    return ns

state = [f"s.h{k}" for k in range(8)]
lets = []
lets.append(("bidx", "if s.i == 0 then 0 else s.nblocks - s.i"))
lets.append(("boff", "bidx * 128"))
bw=[]
for t in range(16):
    woff=t*8; bytenames=[]
    for j in range(4):
        hp=f"w{t}_hi{j}"; lets.append((hp, f"byte_at(s.data, boff + {woff+2*j})"))
        lp=f"w{t}_lo{j}"; lets.append((lp, f"byte_at(s.data, boff + {woff+2*j+1})"))
        hv=f"w{t}_hv{j}"; lets.append((hv, f"if {hp} <= 57 then {hp} - 48 else bor({hp}, 32) - 87"))
        lv=f"w{t}_lv{j}"; lets.append((lv, f"if {lp} <= 57 then {lp} - 48 else bor({lp}, 32) - 87"))
        bn=f"w{t}_b{j}"; lets.append((bn, f"16 * {hv} + {lv}")); bytenames.append(bn)
    wn=f"w{t}"
    lets.append((wn, f"band(bor(bor(bor(shl({bytenames[0]}, 24), shl({bytenames[1]}, 16)), shl({bytenames[2]}, 8)), {bytenames[3]}), {Mb})"))
    bw.append(wn)
newstate = emit_compress(lets, "c", state, bw)
fparts=[]
for wd in range(8):
    for j in range(4):
        idx=wd*4+j; expr=f"band(shr(s.h{wd}, {24-8*j}), 255)"
        if idx==0: fparts.append(f"if s.which == 0 then {expr}")
        elif idx<31: fparts.append(f"if s.which == {idx} then {expr}")
        else: fparts.append(expr)
def nest(p):
    e=p[-1]
    for x in reversed(p[:-1]): e=f"{x} else {e}"
    return e
finalize=nest(fparts)
rf=[f"h{k}: {newstate[k]}" for k in range(8)] + ["nblocks: s.nblocks","i: s.i - 1","which: s.which","data: s.data"]
rec="sha256_fold(ShaState { " + ", ".join(rf) + " })"
body=f"if s.i == 0 then {finalize} else {rec}"
lines=["@verbose 0.1.0","","concept ShaState",
       '  @intention: "SHA-256 fold state: 8 hash words + block count, counter, which, hex pre-padded data"',
       "  @source: invoices.intent:1","  fields:"]
for k in range(8): lines.append(f"    h{k} : number [0, 4294967295]")
lines += ["    nblocks : number [0, 64]","    i : number [0, 64]","    which : number [0, 31]","    data : text","","",
          "rule sha256_fold",
          '  @intention: "SHA-256 over N pre-padded 64-byte blocks (hex): state=IV; per block state=compress(state,block); returns digest byte which"',
          "  @source: invoices.intent:1","  input:","    s : ShaState","  output:","    out : number","  logic:"]
for n,e in lets: lines.append(f"    let {n} = {e}")
lines.append(f"    out = {body}")
reads=", ".join([f"s.h{k}" for k in range(8)]+["s.nblocks","s.i","s.which","s.data"])
lines += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : [sha256_fold]",
          "    termination:","      bound : 400000","      decreasing : i",""]
open("examples/sha256_fold.verbose","w").write("\n".join(lines))
print("wrote examples/sha256_fold.verbose")
