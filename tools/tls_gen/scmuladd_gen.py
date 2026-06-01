"""Emit sc_muladd ((a*b + c) mod L) as a Verbose rule, mirroring
sc_proto.sc_muladd op-for-op. Same conventions as screduce_gen:
- products parenthesized (no Verbose precedence)
- carries via bias-ashr: ashr21(x) = shr(x + 2^52, 21) - 2^31
- fold_block base = top - 12

Input: a0..a31, b0..b31, c0..c31 (96 byte fields [0,255]) + which [0,31].
Output: byte which of (a*b+c) mod L, little-endian.
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from modl_ref import L

BIAS=4503599627370496  # 2^52
OFF=2147483648         # 2^31

def build():
    lets=[]; t=[0]
    def mk(base, vb):
        t[0]+=1; nm=f"{base}{t[0]}"; lets.append((nm, vb)); return nm
    def L3(p,i): return f"bor(bor({p}{i}, shl({p}{i+1}, 8)), shl({p}{i+2}, 16))"
    def L4(p,i): return f"bor(bor(bor({p}{i}, shl({p}{i+1}, 8)), shl({p}{i+2}, 16)), shl({p}{i+3}, 24))"
    def loadlimbs(prefix):
        # returns 12 limb expr-names for the 32-byte LE value with field prefix g.<prefix>
        p=f"g.{prefix}"
        e=[None]*12
        e[0]=f"band({L3(p,0)}, 2097151)"
        e[1]=f"band(shr({L4(p,2)}, 5), 2097151)"
        e[2]=f"band(shr({L3(p,5)}, 2), 2097151)"
        e[3]=f"band(shr({L4(p,7)}, 7), 2097151)"
        e[4]=f"band(shr({L4(p,10)}, 4), 2097151)"
        e[5]=f"band(shr({L3(p,13)}, 1), 2097151)"
        e[6]=f"band(shr({L4(p,15)}, 6), 2097151)"
        e[7]=f"band(shr({L3(p,18)}, 3), 2097151)"
        e[8]=f"band({L3(p,21)}, 2097151)"
        e[9]=f"band(shr({L4(p,23)}, 5), 2097151)"
        e[10]=f"band(shr({L3(p,26)}, 2), 2097151)"
        e[11]=f"shr({L4(p,28)}, 7)"
        return [mk(f"{prefix}l", x) for x in e]
    a=loadlimbs("a"); b=loadlimbs("b"); c=loadlimbs("c")
    # products s0..s22 (mirror sc_proto)
    def term(ai,bj): return f"({a[ai]} * {b[bj]})"
    def summ(parts):
        e=parts[0]
        for p in parts[1:]: e=f"{e} + {p}"
        return e
    cur={}
    def setlimb(k, vb): cur[k]=mk(f"s{k}_", vb)
    setlimb(0, f"{c[0]} + {term(0,0)}")
    setlimb(1, f"{c[1]} + {term(0,1)} + {term(1,0)}")
    setlimb(2, f"{c[2]} + {term(0,2)} + {term(1,1)} + {term(2,0)}")
    setlimb(3, f"{c[3]} + {term(0,3)} + {term(1,2)} + {term(2,1)} + {term(3,0)}")
    setlimb(4, f"{c[4]} + {term(0,4)} + {term(1,3)} + {term(2,2)} + {term(3,1)} + {term(4,0)}")
    setlimb(5, f"{c[5]} + {term(0,5)} + {term(1,4)} + {term(2,3)} + {term(3,2)} + {term(4,1)} + {term(5,0)}")
    setlimb(6, f"{c[6]} + {term(0,6)} + {term(1,5)} + {term(2,4)} + {term(3,3)} + {term(4,2)} + {term(5,1)} + {term(6,0)}")
    setlimb(7, f"{c[7]} + {term(0,7)} + {term(1,6)} + {term(2,5)} + {term(3,4)} + {term(4,3)} + {term(5,2)} + {term(6,1)} + {term(7,0)}")
    setlimb(8, f"{c[8]} + {term(0,8)} + {term(1,7)} + {term(2,6)} + {term(3,5)} + {term(4,4)} + {term(5,3)} + {term(6,2)} + {term(7,1)} + {term(8,0)}")
    setlimb(9, f"{c[9]} + {term(0,9)} + {term(1,8)} + {term(2,7)} + {term(3,6)} + {term(4,5)} + {term(5,4)} + {term(6,3)} + {term(7,2)} + {term(8,1)} + {term(9,0)}")
    setlimb(10, f"{c[10]} + {term(0,10)} + {term(1,9)} + {term(2,8)} + {term(3,7)} + {term(4,6)} + {term(5,5)} + {term(6,4)} + {term(7,3)} + {term(8,2)} + {term(9,1)} + {term(10,0)}")
    setlimb(11, f"{c[11]} + {term(0,11)} + {term(1,10)} + {term(2,9)} + {term(3,8)} + {term(4,7)} + {term(5,6)} + {term(6,5)} + {term(7,4)} + {term(8,3)} + {term(9,2)} + {term(10,1)} + {term(11,0)}")
    setlimb(12, summ([term(1,11),term(2,10),term(3,9),term(4,8),term(5,7),term(6,6),term(7,5),term(8,4),term(9,3),term(10,2),term(11,1)]))
    setlimb(13, summ([term(2,11),term(3,10),term(4,9),term(5,8),term(6,7),term(7,6),term(8,5),term(9,4),term(10,3),term(11,2)]))
    setlimb(14, summ([term(3,11),term(4,10),term(5,9),term(6,8),term(7,7),term(8,6),term(9,5),term(10,4),term(11,3)]))
    setlimb(15, summ([term(4,11),term(5,10),term(6,9),term(7,8),term(8,7),term(9,6),term(10,5),term(11,4)]))
    setlimb(16, summ([term(5,11),term(6,10),term(7,9),term(8,8),term(9,7),term(10,6),term(11,5)]))
    setlimb(17, summ([term(6,11),term(7,10),term(8,9),term(9,8),term(10,7),term(11,6)]))
    setlimb(18, summ([term(7,11),term(8,10),term(9,9),term(10,8),term(11,7)]))
    setlimb(19, summ([term(8,11),term(9,10),term(10,9),term(11,8)]))
    setlimb(20, summ([term(9,11),term(10,10),term(11,9)]))
    setlimb(21, summ([term(10,11),term(11,10)]))
    setlimb(22, term(11,11))
    setlimb(23, "0")
    def cget(k): return cur[k]
    def rcarry(k):
        tmp=mk("t", f"{cget(k)} + 1048576")
        c_=mk("c", f"shr({tmp} + {BIAS}, 21) - {OFF}")
        cur[k+1]=mk(f"s{k+1}_", f"{cget(k+1)} + {c_}")
        cur[k]=mk(f"s{k}_", f"{cget(k)} - shl({c_}, 21)")
    def pcarry(k):
        c_=mk("c", f"shr({cget(k)} + {BIAS}, 21) - {OFF}")
        cur[k+1]=mk(f"s{k+1}_", f"{cget(k+1)} + {c_}")
        cur[k]=mk(f"s{k}_", f"{cget(k)} - shl({c_}, 21)")
    def fold(dst, src, const):
        if const>=0: cur[dst]=mk(f"s{dst}_", f"{cget(dst)} + ({const} * {cget(src)})")
        else: cur[dst]=mk(f"s{dst}_", f"{cget(dst)} - ({-const} * {cget(src)})")
    def fold_block(top):
        consts=[666643,470296,654183,-997805,136657,-683901]
        base=top-12
        for j,cst in enumerate(consts): fold(base+j, top, cst)
    def setzero(k): cur[k]=mk(f"s{k}_", "0")
    # carry/fold sequence extracted PROGRAMMATICALLY from validated
    # sc_proto.sc_muladd (scmuladd_seq.SEQ) so it cannot drift.
    from scmuladd_seq import SEQ
    for op, kk in SEQ:
        if op == 'r': rcarry(kk)
        elif op == 'p': pcarry(kk)
        elif op == 'f': fold_block(kk)
        elif op == 'z': setzero(kk)
    s=[cget(k) for k in range(12)]
    raw=[None]*32
    raw[0]=s[0]; raw[1]=f"shr({s[0]}, 8)"; raw[2]=f"bor(shr({s[0]}, 16), shl({s[1]}, 5))"
    raw[3]=f"shr({s[1]}, 3)"; raw[4]=f"shr({s[1]}, 11)"; raw[5]=f"bor(shr({s[1]}, 19), shl({s[2]}, 2))"
    raw[6]=f"shr({s[2]}, 6)"; raw[7]=f"bor(shr({s[2]}, 14), shl({s[3]}, 7))"; raw[8]=f"shr({s[3]}, 1)"
    raw[9]=f"shr({s[3]}, 9)"; raw[10]=f"bor(shr({s[3]}, 17), shl({s[4]}, 4))"
    raw[11]=f"shr({s[4]}, 4)"; raw[12]=f"shr({s[4]}, 12)"; raw[13]=f"bor(shr({s[4]}, 20), shl({s[5]}, 1))"
    raw[14]=f"shr({s[5]}, 7)"; raw[15]=f"bor(shr({s[5]}, 15), shl({s[6]}, 6))"
    raw[16]=f"shr({s[6]}, 2)"; raw[17]=f"shr({s[6]}, 10)"; raw[18]=f"bor(shr({s[6]}, 18), shl({s[7]}, 3))"
    raw[19]=f"shr({s[7]}, 5)"; raw[20]=f"shr({s[7]}, 13)"
    raw[21]=s[8]; raw[22]=f"shr({s[8]}, 8)"; raw[23]=f"bor(shr({s[8]}, 16), shl({s[9]}, 5))"
    raw[24]=f"shr({s[9]}, 3)"; raw[25]=f"shr({s[9]}, 11)"; raw[26]=f"bor(shr({s[9]}, 19), shl({s[10]}, 2))"
    raw[27]=f"shr({s[10]}, 6)"; raw[28]=f"bor(shr({s[10]}, 14), shl({s[11]}, 7))"; raw[29]=f"shr({s[11]}, 1)"
    raw[30]=f"shr({s[11]}, 9)"; raw[31]=f"shr({s[11]}, 17)"
    outs=[mk(f"o{i}_", f"band({raw[i]}, 255)") for i in range(32)]
    return lets, outs

def emit_file():
    lets, outs = build()
    Lc=["@verbose 0.1.0","","concept ScMul",
        '  @intention: "a,b,c as 32 LE bytes each for (a*b+c) mod L (Ed25519 group order) + which output byte (0..31)"',
        "  @source: invoices.intent:1","  fields:"]
    for p in "abc":
        for i in range(32): Lc.append(f"    {p}{i} : number [0, 255]")
    Lc.append("    which : number [0, 31]")
    Lc+=["","","rule sc_muladd",
         '  @intention: "(a*b + c) mod L (Ed25519 group order, ref10); 32-byte LE operands; returns byte which (0..31)"',
         "  @source: invoices.intent:1","  input:","    g : ScMul","  output:","    out : number","  logic:"]
    for n,e in lets: Lc.append(f"    let {n} = {e}")
    for i in range(32):
        if i==0: Lc.append(f"    out = if g.which == 0 then {outs[0]}")
        elif i<31: Lc.append(f"      else if g.which == {i} then {outs[i]}")
        else: Lc.append(f"      else {outs[31]}")
    reads=", ".join([f"g.{p}{i}" for p in "abc" for i in range(32)]+["g.which"])
    Lc+=["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
         "    termination:","      bound : 20000"]
    open("examples/sc_muladd.verbose","w").write("\n".join(Lc))
    return len(lets)

if __name__=="__main__":
    print("wrote examples/sc_muladd.verbose,", emit_file(), "lets")
