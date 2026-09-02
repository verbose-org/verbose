import subprocess, sys, random
sys.path.insert(0, __import__('os').path.dirname(__file__))
from sha2_emit import K  # noqa (ensure module path ok)

# --- Python AES + GCM reference (NIST-validated core) ---
SBOX=[0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16]
RCON=[0,1,2,4,8,16,32,64,128,0x1b,0x36]; SR=[0,5,10,15,4,9,14,3,8,13,2,7,12,1,6,11]
def xt(x): return ((x<<1)^0x1b)&0xff if x&0x80 else (x<<1)&0xff
def expand(k):
    w=list(k)
    for i in range(4,44):
        t=w[4*(i-1):4*i]
        if i%4==0: t=[SBOX[t[(j+1)%4]] for j in range(4)]; t[0]^=RCON[i//4]
        w+=[w[4*(i-4)+j]^t[j] for j in range(4)]
    return w
def aes(k,b):
    w=expand(k); s=[b[i]^w[i] for i in range(16)]
    for r in range(1,11):
        s=[SBOX[x] for x in s]; s=[s[SR[i]] for i in range(16)]
        if r<10:
            n=list(s)
            for c in range(4):
                a,bb,cc,d=s[c*4:c*4+4]
                n[c*4]=xt(a)^xt(bb)^bb^cc^d; n[c*4+1]=a^xt(bb)^xt(cc)^cc^d
                n[c*4+2]=a^bb^xt(cc)^xt(d)^d; n[c*4+3]=xt(a)^a^bb^cc^xt(d)
            s=[x&0xff for x in n]
        s=[s[i]^w[16*r+i] for i in range(16)]
    return s
def gf(X,Y):
    Z=[0]*16;V=list(X)
    for i in range(128):
        if (Y[i//8]>>(7-(i%8)))&1: Z=[Z[k]^V[k] for k in range(16)]
        lsb=V[15]&1;nv=[0]*16
        for k in range(16):
            hi=(V[k-1]&1) if k>0 else 0; nv[k]=(V[k]>>1)|(hi<<7)
        if lsb: nv[0]^=0xe1
        V=nv
    return Z
def ghash(H,blocks):
    Y=[0]*16
    for B in blocks:
        Y=[Y[k]^B[k] for k in range(16)]; Y=gf(Y,H)
    return Y
def gcm(key,nonce,pt,aad):
    H=aes(key,[0]*16); C=[]
    for j in range(0,len(pt),16):
        ctr=list(nonce)+list((2+j//16).to_bytes(4,'big'))
        ks=aes(key,ctr); blk=pt[j:j+16]; C+=[blk[i]^ks[i] for i in range(len(blk))]
    def pad(b): return list(b)+[0]*((-len(b))%16)
    lenb=list((len(aad)*8).to_bytes(8,'big'))+list((len(C)*8).to_bytes(8,'big'))
    S=ghash(H,[pad(aad)[i:i+16] for i in range(0,len(pad(aad)),16)]+[pad(C)[i:i+16] for i in range(0,len(pad(C)),16)]+[lenb])
    EJ0=aes(key,list(nonce)+[0,0,0,1])
    return bytes(C), bytes(S[i]^EJ0[i] for i in range(16))

def tls_record_ref(key, iv, seq, plaintext, ctype=0x17):
    nonce=bytearray(iv)
    seqb=seq.to_bytes(8,'big')
    for j in range(8): nonce[4+j]^=seqb[j]
    inner=list(plaintext)+[ctype]
    length=len(inner)+16
    aad=[0x17,0x03,0x03,(length>>8)&0xff,length&0xff]
    C,tag=gcm(list(key),bytes(nonce),inner,aad)
    return bytes(aad)+C+tag

# --- Verbose orchestration (existing committed bricks) ---
# Record interface since tranche 5 (2026-08-30): each spawn prints one JSON
# object {"b0":...,...}. encrypt/ghash_fold are ONE spawn; gctr is one spawn
# per 16-byte block (`which` = block index, tail block hex padded by the
# host and truncated after unpacking — same framing glue as vcrypto._gctr).
# Since 2026-09-02 the FRAMING is Verbose too (examples/gcm_frame.verbose):
# nonce, J0, AAD, length block, tag XOR and the tag compare are each one
# record spawn, so this driver no longer XORs or builds a block in Python —
# the Python reference above is the ONLY place framing is computed by hand,
# which is what makes it an oracle for the rules rather than a copy of them.
import json as _json, os as _os
_ROOT=_os.path.dirname(_os.path.dirname(_os.path.dirname(_os.path.abspath(__file__))))
_BIN={"tr_aes":("encrypt","aes_encrypt.verbose"), "tr_gctr":("gctr","aes_gctr.verbose"),
      "tr_ghash":("ghash_fold","ghash_nblocks.verbose"),
      "tr_gcm_nonce":("gcm_nonce","gcm_frame.verbose"), "tr_gcm_j0":("gcm_j0","gcm_frame.verbose"),
      "tr_gcm_aad":("gcm_aad","gcm_frame.verbose"), "tr_gcm_lenblock":("gcm_lenblock","gcm_frame.verbose"),
      "tr_gcm_tag":("gcm_tag","gcm_frame.verbose")}
for _name,(_rule,_src) in _BIN.items():   # compile on demand (the binaries used to be assumed present)
    if not _os.path.exists("/tmp/"+_name):
        subprocess.run(["cargo","run","--release","--",_os.path.join("examples",_src),"--native","/tmp/"+_name,"--run",_rule],
                       cwd=_ROOT, capture_output=True, text=True)
        assert _os.path.exists("/tmp/"+_name), f"could not compile {_rule} from {_src}"
def _rec(cmd, n):
    r=subprocess.run(cmd,capture_output=True,text=True)
    o=_json.loads(r.stdout.strip())
    assert len(o)==n, f"{cmd[0]}: field count {len(o)} != {n}"
    return bytes(o[f"b{i}"] for i in range(n))
def venc(key, block):
    args=[str(b) for b in block]+[str(b) for b in key]
    return _rec(["/tmp/tr_aes"]+args, 16)
def vgctr(key, nonce, pt):
    nb=(len(pt)+15)//16; padded=bytes(pt)+bytes((-len(pt))%16); hexpt=padded.hex()
    base=[str(b) for b in key]+[str(b) for b in nonce]+[str(nb)]
    out=b"".join(_rec(["/tmp/tr_gctr"]+base+[str(w),hexpt], 16) for w in range(nb))
    return out[:len(pt)]
def vghash(H, data):
    nb=len(data)//16; hexd=bytes(data).hex()
    args=[str(b) for b in [0]*16]+[str(b) for b in H]+[str(nb),str(nb),hexd]
    return _rec(["/tmp/tr_ghash"]+args, 16)
def vnonce(iv, seq):    return _rec(["/tmp/tr_gcm_nonce"]+[str(b) for b in iv]+[str(seq)], 12)
def vj0(iv, seq):       return _rec(["/tmp/tr_gcm_j0"]+[str(b) for b in iv]+[str(seq)], 16)
def vaad(inner_len):    return _rec(["/tmp/tr_gcm_aad", str(inner_len)], 5)
def vlenblock(la, lc):  return _rec(["/tmp/tr_gcm_lenblock", str(la), str(lc)], 16)
def vtag(S, EJ0):       return _rec(["/tmp/tr_gcm_tag"]+[str(b) for b in S]+[str(b) for b in EJ0], 16)
def v_tls_record(key, iv, seq, plaintext, ctype=0x17):
    inner=bytes(plaintext)+bytes([ctype])            # TLS framing (host): append the inner content type
    aad=vaad(len(inner))                              # Verbose: the record header / additional data
    nonce=vnonce(iv, seq)                             # Verbose: IV XOR seq
    H=venc(key,[0]*16); C=vgctr(key,nonce,inner)      # Verbose: H, then one gctr spawn per block
    def pad(b): return bytes(b)+bytes((-len(b))%16)   # host glue: variable-length zero padding
    S=vghash(list(H), pad(aad)+pad(C)+vlenblock(len(aad),len(C)))   # Verbose: length block + GHASH
    EJ0=venc(key,vj0(iv,seq))                         # Verbose: J0, then E_K(J0)
    return aad+C+vtag(S,EJ0)                          # Verbose: T = S XOR E_K(J0)

random.seed(123)
for _ in range(3):
    key=bytes(random.randrange(256) for _ in range(16))
    iv=bytes(random.randrange(256) for _ in range(12))
    seq=random.randrange(0,2**32)
    pt=bytes(random.randrange(256) for _ in range(random.choice([5,17,40])))
    if tls_record_ref(key,iv,seq,pt) != v_tls_record(key,iv,seq,pt):
        sys.exit(1)
print("TLS_RECORD_OK")
sys.exit(0)
