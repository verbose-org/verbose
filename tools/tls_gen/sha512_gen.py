import os
# SHA-512 over N pre-padded 128-byte blocks, recursive fold (decreasing:i).
# 64-bit words held as i64 (negative for >=2^63). No masking needed: native
# arithmetic is mod 2^64 and band/shl/shr operate on full 64 bits.
# rotr/shr on 64-bit; sigma functions per FIPS 180-4.

K = [
0x428a2f98d728ae22,0x7137449123ef65cd,0xb5c0fbcfec4d3b2f,0xe9b5dba58189dbbc,
0x3956c25bf348b538,0x59f111f1b605d019,0x923f82a4af194f9b,0xab1c5ed5da6d8118,
0xd807aa98a3030242,0x12835b0145706fbe,0x243185be4ee4b28c,0x550c7dc3d5ffb4e2,
0x72be5d74f27b896f,0x80deb1fe3b1696b1,0x9bdc06a725c71235,0xc19bf174cf692694,
0xe49b69c19ef14ad2,0xefbe4786384f25e3,0x0fc19dc68b8cd5b5,0x240ca1cc77ac9c65,
0x2de92c6f592b0275,0x4a7484aa6ea6e483,0x5cb0a9dcbd41fbd4,0x76f988da831153b5,
0x983e5152ee66dfab,0xa831c66d2db43210,0xb00327c898fb213f,0xbf597fc7beef0ee4,
0xc6e00bf33da88fc2,0xd5a79147930aa725,0x06ca6351e003826f,0x142929670a0e6e70,
0x27b70a8546d22ffc,0x2e1b21385c26c926,0x4d2c6dfc5ac42aed,0x53380d139d95b3df,
0x650a73548baf63de,0x766a0abb3c77b2a8,0x81c2c92e47edaee6,0x92722c851482353b,
0xa2bfe8a14cf10364,0xa81a664bbc423001,0xc24b8b70d0f89791,0xc76c51a30654be30,
0xd192e819d6ef5218,0xd69906245565a910,0xf40e35855771202a,0x106aa07032bbd1b8,
0x19a4c116b8d2d0c8,0x1e376c085141ab53,0x2748774cdf8eeb99,0x34b0bcb5e19b48a8,
0x391c0cb3c5c95a63,0x4ed8aa4ae3418acb,0x5b9cca4f7763e373,0x682e6ff3d6b2b8a3,
0x748f82ee5defb2fc,0x78a5636f43172f60,0x84c87814a1f0ab72,0x8cc702081a6439ec,
0x90befffa23631e28,0xa4506cebde82bde9,0xbef9a3f7b2c67915,0xc67178f2e372532b,
0xca273eceea26619c,0xd186b8c721c0c207,0xeada7dd6cde0eb1e,0xf57d4f7fee6ed178,
0x06f067aa72176fba,0x0a637dc5a2c898a6,0x113f9804bef90dae,0x1b710b35131c471b,
0x28db77f523047d84,0x32caab7b40c72493,0x3c9ebe0a15c9bebc,0x431d67c49c100d4c,
0x4cc5d4becb3e42b6,0x597f299cfc657e2a,0x5fcb6fab3ad6faec,0x6c44198c4a475817,
]
H0 = [0x6a09e667f3bcc908,0xbb67ae8584caa73b,0x3c6ef372fe94f82b,0xa54ff53a5f1d36f1,
      0x510e527fade682d1,0x9b05688c2b3e6c1f,0x1f83d9abfb41bd6b,0x5be0cd19137e2179]

def sgn(v): return v if v < (1<<63) else v - (1<<64)
def lit(v): return str(sgn(v))

# 64-bit rotr/shr on an i64 expression name; use band masks built from negative
# literals where needed. rotr(x,n) = (x>>n)|(x<<(64-n)); shr is logical.
def rotr(x,n): return f"bor(shr({x}, {n}), shl({x}, {64-n}))"
def shr_(x,n): return f"shr({x}, {n})"
def s0(x): return f"bxor(bxor({rotr(x,1)}, {rotr(x,8)}), {shr_(x,7)})"
def s1(x): return f"bxor(bxor({rotr(x,19)}, {rotr(x,61)}), {shr_(x,6)})"
def S0(x): return f"bxor(bxor({rotr(x,28)}, {rotr(x,34)}), {rotr(x,39)})"
def S1(x): return f"bxor(bxor({rotr(x,14)}, {rotr(x,18)}), {rotr(x,41)})"
def ch(x,y,z): return f"bxor(band({x}, {y}), band(bnot({x}), {z}))"
def maj(x,y,z): return f"bxor(bxor(band({x}, {y}), band({x}, {z})), band({y}, {z}))"

lets=[]
state=[f"s.h{k}" for k in range(8)]
lets.append(("bidx","if s.i == 0 then 0 else s.nblocks - s.i"))
lets.append(("boff","bidx * 256"))   # hex offset of this 128-byte block (2 hex/byte)
# read 16 big-endian 64-bit words from hex
W=[]
for t in range(16):
    woff=t*16  # hex chars per 8-byte word
    bys=[]
    for j in range(8):
        hp=f"w{t}_h{j}"; lets.append((hp, f"byte_at(s.data, boff + {woff+2*j})"))
        lp=f"w{t}_l{j}"; lets.append((lp, f"byte_at(s.data, boff + {woff+2*j+1})"))
        hv=f"w{t}_hv{j}"; lets.append((hv, f"if {hp} <= 57 then {hp} - 48 else bor({hp}, 32) - 87"))
        lv=f"w{t}_lv{j}"; lets.append((lv, f"if {lp} <= 57 then {lp} - 48 else bor({lp}, 32) - 87"))
        bn=f"w{t}_b{j}"; lets.append((bn, f"16 * {hv} + {lv}")); bys.append(bn)
    wn=f"w{t}"
    expr=bys[7]
    for j in range(7): expr=f"bor({expr}, shl({bys[j]}, {8*(7-j)}))"
    lets.append((wn, expr)); W.append(wn)
for t in range(16,80):
    wn=f"w{t}"
    lets.append((wn, f"{s1(W[t-2])} + {W[t-7]} + {s0(W[t-15])} + {W[t-16]}"))
    W.append(wn)
a,b,c,d,e,f,g,h=state
for t in range(80):
    t1=f"t1_{t}"; lets.append((t1, f"{h} + {S1(e)} + {ch(e,f,g)} + {lit(K[t])} + {W[t]}"))
    t2=f"t2_{t}"; lets.append((t2, f"{S0(a)} + {maj(a,b,c)}"))
    ne=f"e_{t}"; lets.append((ne, f"{d} + {t1}"))
    na=f"a_{t}"; lets.append((na, f"{t1} + {t2}"))
    h,g,f,e,d,c,b,a=g,f,e,ne,c,b,a,na
ns=[f"nh{k}" for k in range(8)]
for k,(o,n) in enumerate(zip(state,[a,b,c,d,e,f,g,h])): lets.append((ns[k], f"{o} + {n}"))

# finalize: digest byte `which` (0..63), big-endian from 8 words
fparts=[]
for wd in range(8):
    for j in range(8):
        idx=wd*8+j
        e=f"band(shr(s.h{wd}, {8*(7-j)}), 255)"
        if idx==0: fparts.append(f"if s.which == 0 then {e}")
        elif idx<63: fparts.append(f"if s.which == {idx} then {e}")
        else: fparts.append(e)
def nest(p):
    x=p[-1]
    for y in reversed(p[:-1]): x=f"{y} else {x}"
    return x
finalize=nest(fparts)
rf=[f"h{k}: {ns[k]}" for k in range(8)]+["nblocks: s.nblocks","i: s.i - 1","which: s.which","data: s.data"]
rec="sha512_fold(Sha512State { "+", ".join(rf)+" })"
body=f"if s.i == 0 then {finalize} else {rec}"

L=["@verbose 0.1.0","","concept Sha512State",
   '  @intention: "SHA-512 fold state: 8 64-bit hash words + block count, counter, which, hex pre-padded data"',
   "  @source: invoices.intent:1","  fields:"]
for k in range(8): L.append(f"    h{k} : number")
L += ["    nblocks : number [0, 32]","    i : number [0, 32]","    which : number [0, 63]","    data : text","","",
      "rule sha512_fold",
      '  @intention: "SHA-512 over N pre-padded 128-byte blocks (hex): state=IV; per block compress; returns digest byte which (FIPS 180-4)"',
      "  @source: invoices.intent:1","  input:","    s : Sha512State","  output:","    out : number","  logic:"]
for n,ex in lets: L.append(f"    let {n} = {ex}")
L.append(f"    out = {body}")
reads=", ".join([f"s.h{k}" for k in range(8)]+["s.nblocks","s.i","s.which","s.data"])
L += ["  proofs:","    purity:",f"      reads : [{reads}]","      calls : [sha512_fold]",
      "    termination:","      bound : 3000000","      decreasing : i",""]
open("examples/sha512_fold.verbose","w").write("\n".join(L))
print("wrote examples/sha512_fold.verbose ; H0[0]_signed", sgn(H0[0]))
