# SHA-256 over an explicit byte list, as a Verbose let-chain. Plus HMAC helper.
K=[0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2]
H0=[0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19]
M=0xffffffff
def rotr(x,n): return f'band(bor(shr({x}, {n}), shl({x}, {32-n})), {M})'
def s0(x): return f'band(bxor(bxor({rotr(x,7)}, {rotr(x,18)}), shr({x}, 3)), {M})'
def s1(x): return f'band(bxor(bxor({rotr(x,17)}, {rotr(x,19)}), shr({x}, 10)), {M})'
def S0(x): return f'band(bxor(bxor({rotr(x,2)}, {rotr(x,13)}), {rotr(x,22)}), {M})'
def S1(x): return f'band(bxor(bxor({rotr(x,6)}, {rotr(x,11)}), {rotr(x,25)}), {M})'
def ch(x,y,z): return f'band(bxor(band({x}, {y}), band(bnot({x}), {z})), {M})'
def maj(x,y,z): return f'band(bxor(bxor(band({x}, {y}), band({x}, {z})), band({y}, {z})), {M})'

def sha256_bytes(lets, prefix, byte_exprs):
    L=len(byte_exprs); padded=list(byte_exprs)+['128']
    while len(padded)%64!=56: padded.append('0')
    bl=L*8
    for s in range(56,-8,-8): padded.append(str((bl>>s)&0xff))
    nb=len(padded)//64
    st=[f'{prefix}_h{i}i' for i in range(8)]
    for i in range(8): lets.append((st[i], str(H0[i])))
    for blk in range(nb):
        base=blk*64; W=[]
        for t in range(16):
            b=[padded[base+4*t+j] for j in range(4)]
            wn=f'{prefix}_b{blk}w{t}'
            lets.append((wn, f'band(bor(bor(bor(shl({b[0]}, 24), shl({b[1]}, 16)), shl({b[2]}, 8)), {b[3]}), {M})')); W.append(wn)
        for t in range(16,64):
            wn=f'{prefix}_b{blk}w{t}'
            lets.append((wn, f'band({s1(W[t-2])} + {W[t-7]} + {s0(W[t-15])} + {W[t-16]}, {M})')); W.append(wn)
        a,b,c,d,e,f,g,h=st
        for t in range(64):
            t1=f'{prefix}_b{blk}t1_{t}'; lets.append((t1, f'band({h} + {S1(e)} + {ch(e,f,g)} + {K[t]} + {W[t]}, {M})'))
            t2=f'{prefix}_b{blk}t2_{t}'; lets.append((t2, f'band({S0(a)} + {maj(a,b,c)}, {M})'))
            ne=f'{prefix}_b{blk}e{t}'; lets.append((ne, f'band({d} + {t1}, {M})'))
            na=f'{prefix}_b{blk}a{t}'; lets.append((na, f'band({t1} + {t2}, {M})'))
            h,g,f,e,d,c,b,a=g,f,e,ne,c,b,a,na
        ns=[f'{prefix}_h{i}_{blk}' for i in range(8)]
        for i,(o,n) in enumerate(zip(st,[a,b,c,d,e,f,g,h])): lets.append((ns[i], f'band({o} + {n}, {M})'))
        st=ns
    return st

def words_to_bytes(lets, prefix, words):
    out=[]
    for i,w in enumerate(words):
        for j in range(4):
            bn=f'{prefix}_ob{i}_{j}'; lets.append((bn, f'band(shr({w}, {24-8*j}), 255)')); out.append(bn)
    return out

def hmac(lets, prefix, key64, msg):
    ipad=[]; opad=[]
    for i in range(64):
        ip=f'{prefix}_ip{i}'; lets.append((ip, f'bxor({key64[i]}, 54)')); ipad.append(ip)
        op=f'{prefix}_op{i}'; lets.append((op, f'bxor({key64[i]}, 92)')); opad.append(op)
    iw=sha256_bytes(lets, prefix+'_in', ipad+list(msg)); ib=words_to_bytes(lets, prefix+'_in', iw)
    ow=sha256_bytes(lets, prefix+'_ou', opad+ib); return words_to_bytes(lets, prefix+'_ou', ow)
