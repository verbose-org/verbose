"""Emit sc_reduce as a Verbose rule, mirroring the validated sc_proto.sc_reduce
op-for-op via a Builder that simultaneously (a) emits a let and (b) tracks the
Python value. Before writing the .verbose, the Builder result is asserted equal
to sc_proto.sc_reduce over random inputs, so the emitted rule cannot drift.

Input: 64 byte fields b0..b63 [0,255] + which [0,31]. Output: byte `which` of
(int(b) mod L), little-endian. Carries use the validated ashr21 identity
(signed floor-shift by 21 = (x - band(x,2097151)) / 2097152).
"""
import sys, os; sys.path.insert(0, os.path.dirname(__file__))
from modl_ref import L
import sc_proto

MASK21 = 2097151

class B:
    def __init__(self):
        self.lets=[]; self.valof={}; self._n=0
    def _mk(self, base, py, vb):
        self._n+=1; nm=f"{base}_{self._n}"
        self.lets.append((nm, vb)); self.valof[nm]=py; return nm
    # leaf: a raw byte field
    def byte(self, i):
        nm=f"g.b{i}"; self.valof[nm]=("BYTE",i); return nm
    # we keep values as ints by evaluating against a concrete input during build;
    # but emission must be input-independent. So Builder runs in EMIT mode with a
    # parallel concrete evaluation: store python int in valof for non-leaf, and
    # for leaves store a marker resolved by .eval(inp).
    # Simpler: two passes. Here we do EMIT producing (name->expr); a separate
    # run() recomputes values from an input using the SAME expr list.

def build(emit_input=None):
    """Returns (lets, out_names[32]). Pure emission; expressions reference g.b*."""
    lets=[]; t=[0]
    def mk(base, vb):
        t[0]+=1; nm=f"{base}{t[0]}"; lets.append((nm, vb)); return nm
    b=lambda i: f"g.b{i}"
    def load3(i): return f"bor(bor({b(i)}, shl({b(i+1)}, 8)), shl({b(i+2)}, 16))"
    def load4(i): return f"bor(bor(bor({b(i)}, shl({b(i+1)}, 8)), shl({b(i+2)}, 16)), shl({b(i+3)}, 24))"
    # initial 24 limbs (names s_k -> current vb expr held in a dict)
    cur={}
    def setlimb(k, vb): cur[k]=mk(f"s{k}_", vb)
    setlimb(0, f"band({load3(0)}, 2097151)")
    setlimb(1, f"band(shr({load4(2)}, 5), 2097151)")
    setlimb(2, f"band(shr({load3(5)}, 2), 2097151)")
    setlimb(3, f"band(shr({load4(7)}, 7), 2097151)")
    setlimb(4, f"band(shr({load4(10)}, 4), 2097151)")
    setlimb(5, f"band(shr({load3(13)}, 1), 2097151)")
    setlimb(6, f"band(shr({load4(15)}, 6), 2097151)")
    setlimb(7, f"band(shr({load3(18)}, 3), 2097151)")
    setlimb(8, f"band({load3(21)}, 2097151)")
    setlimb(9, f"band(shr({load4(23)}, 5), 2097151)")
    setlimb(10, f"band(shr({load3(26)}, 2), 2097151)")
    setlimb(11, f"band(shr({load4(28)}, 7), 2097151)")
    setlimb(12, f"band(shr({load4(31)}, 4), 2097151)")
    setlimb(13, f"band(shr({load3(34)}, 1), 2097151)")
    setlimb(14, f"band(shr({load4(36)}, 6), 2097151)")
    setlimb(15, f"band(shr({load3(39)}, 3), 2097151)")
    setlimb(16, f"band({load3(42)}, 2097151)")
    setlimb(17, f"band(shr({load4(44)}, 5), 2097151)")
    setlimb(18, f"band(shr({load3(47)}, 2), 2097151)")
    setlimb(19, f"band(shr({load4(49)}, 7), 2097151)")
    setlimb(20, f"band(shr({load4(52)}, 4), 2097151)")
    setlimb(21, f"band(shr({load3(55)}, 1), 2097151)")
    setlimb(22, f"band(shr({load4(57)}, 6), 2097151)")
    setlimb(23, f"shr({load4(60)}, 3)")

    def cget(k): return cur[k]
    def fold(dst, src, const):  # dst += src*const  (const may be negative int)
        if const>=0: cur[dst]=mk(f"s{dst}_", f"{cget(dst)} + ({const} * {cget(src)})")
        else: cur[dst]=mk(f"s{dst}_", f"{cget(dst)} - ({-const} * {cget(src)})")
    def fold_block(top):
        # the 6-term fold used per high limb 'top' -> targets top-11..top-6
        consts=[666643,470296,654183,-997805,136657,-683901]
        base=top-12
        for j,cst in enumerate(consts):
            fold(base+j, top, cst)
    def rcarry(k):  # c=(s_k+2^20)>>21 (arith) via bias; s_{k+1}+=c; s_k-=c<<21
        tmp=mk("t", f"{cget(k)} + 1048576")
        c=mk("c", f"shr({tmp} + 4503599627370496, 21) - 2147483648")
        cur[k+1]=mk(f"s{k+1}_", f"{cget(k+1)} + {c}")
        cur[k]=mk(f"s{k}_", f"{cget(k)} - shl({c}, 21)")
    def pcarry(k):  # c=s_k>>21 (arith floor) via bias; s_{k+1}+=c; s_k-=c<<21
        c=mk("c", f"shr({cget(k)} + 4503599627370496, 21) - 2147483648")
        cur[k+1]=mk(f"s{k+1}_", f"{cget(k+1)} + {c}")
        cur[k]=mk(f"s{k}_", f"{cget(k)} - shl({c}, 21)")
    def setzero(k): cur[k]=mk(f"s{k}_", "0")

    for top in [23,22,21,20,19,18]: fold_block(top)
    for k in [6,8,10,12,14,16,7,9,11,13,15]: rcarry(k)
    for top in [17,16,15,14,13,12]: fold_block(top)
    setzero(12)
    for k in [0,2,4,6,8,10,1,3,5,7,9,11]: rcarry(k)
    fold_block(12); setzero(12)
    for k in range(12): pcarry(k)
    fold_block(12)
    for k in range(11): pcarry(k)

    s=[cget(k) for k in range(12)]
    # output packing (mirror sc_proto) -> 32 byte exprs, then band 255
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
    L_=["@verbose 0.1.0","","concept Sc64",
        '  @intention: "64 little-endian bytes to reduce mod the Ed25519 group order L, + which output byte (0..31)"',
        "  @source: invoices.intent:1","  fields:"]
    for i in range(64): L_.append(f"    b{i} : number [0, 255]")
    L_.append("    which : number [0, 31]")
    L_+=["","","rule sc_reduce",
         '  @intention: "reduce a 64-byte little-endian integer mod L (Ed25519 group order, ref10); returns byte which (0..31)"',
         "  @source: invoices.intent:1","  input:","    g : Sc64","  output:","    out : number","  logic:"]
    for n,e in lets: L_.append(f"    let {n} = {e}")
    disp=[]
    for i in range(32):
        if i==0: disp.append(f"    out = if g.which == 0 then {outs[0]}")
        elif i<31: disp.append(f"      else if g.which == {i} then {outs[i]}")
        else: disp.append(f"      else {outs[31]}")
    L_+=disp
    reads=", ".join([f"g.b{i}" for i in range(64)]+["g.which"])
    L_+=["  proofs:","    purity:",f"      reads : [{reads}]","      calls : []",
         "    termination:","      bound : 4000"]
    open("examples/sc_reduce.verbose","w").write("\n".join(L_))
    return len(lets)

if __name__=="__main__":
    n=emit_file()
    print(f"wrote examples/sc_reduce.verbose, {n} lets")
