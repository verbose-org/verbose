"""Generate examples/x25519_rec.verbose — RECURSIVE X25519 finish (Fermat inverse
+ encode), SIZE-optimized sibling of examples/x25519.verbose.

WHY: x25519.verbose unrolls the curve25519 inversion addition chain inline (265
field multiplies in finv + 1 final x2*zinv = 266 fmul, ~18.7k lets -> ~1.3 MB
native binary). This generator expresses the SAME addition chain as a single
self-recursive rule whose body emits EXACTLY ONE field multiply per frame
(operand muxed by the step counter) — 265 step-frames + 1 base-case final
multiply = 266 fmul total, i.e. ZERO extra field multiplies vs the unrolled form.

OPERAND-MUX DESIGN (the no-CPU-overhead constraint):
field_emit.emit_finv is a straight sequence of 265 single-fmul steps. Each step
`t = fmul(t, op)` where op is EITHER `t` itself (a SQUARE) OR one of {z, z2, z9,
z11, z2_5_0, z2_10_0, z2_20_0, z2_50_0, z2_100_0} (a JUNCTION). Never both.
Some steps additionally SAVE the result into a named intermediate that a later
junction reuses. We instrument emit_finv (see /tmp/x25519_instrument.py
methodology, reproduced inline below) to PROGRAMMATICALLY extract two
compile-time control tables — OP_TABLE (pc -> junction operand slot) and
SAVE_AT (pc -> saved slot) — then assert the reconstructed machine equals
pow(z, P-2, P).

Per recursive frame (pc = 265 - s.j, frames for s.j = 265..1):
  1) operand mux per limb: op_i = if pc==K1 then s.<slot1>_i else ... else s.t_i
     (default s.t_i => SQUARE; only junction pcs deviate)   -- cheap per-limb selects
  2) ONE field multiply: nt = fmul(t, op)                   -- the single fmul/frame
  3) save mux per saved slot: ns_<slot>_i = if pc==save_pc then nt_i else s.<slot>_i
                                                            -- 8 limb-group muxes, NO fmul
  4) recurse with j-1, t:=nt, saved slots updated, inputs unchanged.
Base case (s.j == 0): t holds z^(p-2). Compute x2*t (the final fmul, matching
the unrolled m266 = x2*zinv), little-endian encode, return byte `which`.

CPU OVERHEAD vs unrolled: identical 266 fmul. The ONLY extra work is the muxes:
1 operand mux (10 per-limb selects) + 8 save muxes (80 per-limb selects) = ~90
per-limb selects/frame. One fmul expands to ~100 multiply-terms across 10 limbs
+ a 2-pass carry reduce, so the mux overhead is well under one fmul's worth of
arithmetic and adds NO second multiply. This is the negligible overhead approved.

SHAPE — whole-chain single recursive rule (NOT per-block calls): Verbose/native
rules return a SINGLE scalar (selected by `which` at the base case); a rule
cannot return 10 field limbs to a caller, so per-block "call fsq_pow2" is not
expressible. The state record carries the ENTIRE machine state.

HOST GLUE: the recursive state is seeded by the host (vcrypto.x25519_rec_finish):
  t := z2  (the value to invert)
  z := z2  (input copy; junctions at pc=3 read it)
  all 8 saved slots := 0  (filled at their save steps before first read)
  x2 := x2 ladder numerator
  j := 265 ; which := 0..31
Arg order = field declaration order in the concept.
"""
import sys, os, random
sys.path.insert(0, os.path.dirname(__file__))
import field_emit as fe

P = fe.P
NSTEPS = 265   # field multiplies inside finv (the inversion addition chain)

# ---------------------------------------------------------------------------
# 1) PROGRAMMATIC table extraction by instrumenting emit_finv.
#    No hand transcription of 265 steps.
# ---------------------------------------------------------------------------
SAVED = ['z2','z9','z11','z2_5_0','z2_10_0','z2_20_0','z2_50_0','z2_100_0']

def extract_tables():
    trace = []
    real = fe.emit_fmul
    def instrumented(lets, prefix, A, B):
        res = real(lets, prefix, A, B)
        trace.append({'prefix': prefix, 'A': tuple(A), 'B': tuple(B),
                      'is_square': tuple(A) == tuple(B), 'res': tuple(res)})
        return res
    fe.emit_fmul = instrumented
    PFX = "f"
    zlimbs = [f"s.z_{i}" for i in range(10)]
    fe.emit_finv([], PFX, zlimbs)
    fe.emit_fmul = real

    # logical name -> produced limb tuple
    produced = {}
    for t in trace:
        produced[t['prefix'][len(PFX)+1:]] = t['res']
    group_limbs = {'z': tuple(zlimbs)}
    for slot in SAVED:
        group_limbs[slot] = produced[slot]

    def ident_op(Bnames):
        for slot, limbs in group_limbs.items():
            if tuple(Bnames) == limbs:
                return slot
        return None

    op_table = {}    # pc -> junction operand slot
    save_at = {}     # pc -> saved slot
    for pc, t in enumerate(trace):
        if not t['is_square']:
            op = ident_op(t['B'])
            if op is None:
                raise SystemExit(f"pc {pc}: operand not identified B={t['B']}")
            op_table[pc] = op
        n = t['prefix'][len(PFX)+1:]
        if n in SAVED:
            save_at[pc] = n
    assert len(trace) == NSTEPS, f"expected {NSTEPS} fmul in finv, got {len(trace)}"
    return op_table, save_at

OP_TABLE, SAVE_AT = extract_tables()

# ---------------------------------------------------------------------------
# 2) Reconstruct the machine numerically and assert == pow(z, P-2, P).
# ---------------------------------------------------------------------------
def run_machine(z):
    t = z
    slots = {s: None for s in SAVED}
    slots['z'] = z
    for pc in range(NSTEPS):
        if pc in OP_TABLE:
            op = slots[OP_TABLE[pc]]
            if op is None:
                raise RuntimeError(f"pc{pc} reads unsaved slot {OP_TABLE[pc]}")
        else:
            op = t
        t = (t * op) % P
        if pc in SAVE_AT:
            slots[SAVE_AT[pc]] = t
    return t

def _self_check():
    random.seed(7); bad = 0
    for _ in range(300):
        z = random.randrange(1, P)
        if run_machine(z) != pow(z, P-2, P): bad += 1
    for z in (1, 2, P-1, P-2):
        if run_machine(z) != pow(z, P-2, P): bad += 1
    if bad:
        raise SystemExit(f"machine reconstruction FAILED: {bad} mismatches")
_self_check()

# ---------------------------------------------------------------------------
# 3) Emit the recursive .verbose program — ONE fmul per frame.
# ---------------------------------------------------------------------------
INPUTS = ['z', 'x2']
ALL_SLOTS = ['t'] + SAVED + INPUTS    # field groups, 10 limbs each
LIMB_MAX = [(1 << w) - 1 for w in fe.W]

# pc = NSTEPS - s.j  ; frames run for s.j = NSTEPS..1 (pc = 0..NSTEPS-1).
PC = f"({NSTEPS} - s.j)"

lets = []
T = [f"s.t_{i}" for i in range(10)]

# (a) operand mux per limb: op_i = if pc==K then s.<slot>_i else ... else s.t_i
def operand_for_limb(i):
    # junction pcs in stable order
    parts = sorted(OP_TABLE.items())   # [(pc, slot), ...]
    expr = f"s.t_{i}"                  # default => square step
    for pc, slot in reversed(parts):
        expr = f"if {PC} == {pc} then s.{slot}_{i} else {expr}"
    return expr

OP = []
for i in range(10):
    nm = f"op_{i}"
    lets.append((nm, operand_for_limb(i)))
    OP.append(nm)

# (b) THE single field multiply: nt = fmul(t, op)
NT = fe.emit_fmul(lets, "nt", T, OP)

# (c) save mux per saved slot (each slot saved at exactly one pc)
slot_save_pc = {slot: pc for pc, slot in SAVE_AT.items()}
SAVE_NEW = {}
for slot in SAVED:
    spc = slot_save_pc[slot]
    grp = []
    for i in range(10):
        nm = f"ns_{slot}_{i}"
        lets.append((nm, f"if {PC} == {spc} then {NT[i]} else s.{slot}_{i}"))
        grp.append(nm)
    SAVE_NEW[slot] = grp

# (d) base case: final fmul x2 * t (= x2 * zinv), then little-endian encode.
X2 = [f"s.x2_{i}" for i in range(10)]
FINAL = fe.emit_fmul(lets, "fin", X2, T)     # the 266th fmul, only live at base case
obytes = fe.emit_encode(lets, "enc", FINAL)
def nest_which(names):
    expr = names[-1]
    for i in range(len(names) - 2, -1, -1):
        expr = f"if s.which == {i} then {names[i]} else {expr}"
    return expr
finalize = nest_which(obytes)

# (e) recursive record
rf = []
for i in range(10): rf.append(f"t_{i}: {NT[i]}")
for slot in SAVED:
    for i in range(10): rf.append(f"{slot}_{i}: {SAVE_NEW[slot][i]}")
for inp in INPUTS:
    for i in range(10): rf.append(f"{inp}_{i}: s.{inp}_{i}")
rf += ["j: s.j - 1", "which: s.which"]
rec = "x25519_finish_rec(X25519RecState { " + ", ".join(rf) + " })"
body = f"if s.j == 0 then {finalize} else {rec}"

# ---- assemble .verbose text ----
L = ["@verbose 0.1.0", "", "concept X25519RecState",
     '  @intention: "X25519 inverse-chain state: running accumulator t (10 limbs) + 8 saved intermediates (z2, z9, z11, z2_5_0, z2_10_0, z2_20_0, z2_50_0, z2_100_0; 10 limbs each) + inputs z and x2 (10 limbs each) + step counter j (265..0) + which output byte (0..31)"',
     "  @source: invoices.intent:1", "  fields:"]
def decl_group(name):
    for i in range(10):
        L.append(f"    {name}_{i} : number [0, {LIMB_MAX[i]}]")
for slot in ALL_SLOTS:
    decl_group(slot)
L.append(f"    j : number [0, {NSTEPS}]")
L.append("    which : number [0, 31]")

L += ["", "", "rule x25519_finish_rec",
      '  @intention: "X25519 finish via the curve25519 Fermat inverse addition chain, recursive: EXACTLY ONE conditional field multiply per frame (operand muxed by step counter — square or junction, never both), 265 step-frames (decreasing j); base case multiplies x2 by z^(p-2), little-endian encodes, returns byte which. Same algorithm and same 266-fmul count as the unrolled emit_finv; bit-for-bit identical result, zero extra field multiplies."',
      "  @source: invoices.intent:1", "  input:", "    s : X25519RecState",
      "  output:", "    out : number", "  logic:"]
for nm, e in lets:
    L.append(f"    let {nm} = {e}")
L.append(f"    out = {body}")
reads = []
for slot in ALL_SLOTS:
    reads += [f"s.{slot}_{i}" for i in range(10)]
reads += ["s.j", "s.which"]
L += ["  proofs:", "    purity:", f"      reads : [{', '.join(reads)}]",
      "      calls : [x25519_finish_rec]",
      "    termination:", "      bound : 2000000", "      decreasing : j"]

out_path = os.path.normpath(os.path.join(os.path.dirname(__file__), "..", "..",
                                         "examples", "x25519_rec.verbose"))
open(out_path, "w").write("\n".join(L) + "\n")

# count the fmul calls in the EMITTED recursive form for the report:
#   1 operand-mux fmul (nt) + 1 final fmul (fin) emitted in the BODY;
#   the body's single fmul runs once per frame -> 265 frames + 1 final = 266 at runtime.
n_body_fmul_emit = 2   # 'nt' (per-frame) + 'fin' (base-case)
print(f"wrote {out_path}")
print(f"finv_fmul_steps={NSTEPS} (extracted) ; junctions={len(OP_TABLE)} ; saves={len(SAVE_AT)}")
print(f"OP_TABLE={dict(sorted(OP_TABLE.items()))}")
print(f"SAVE_AT={dict(sorted(SAVE_AT.items()))}")
print(f"emit_fmul_in_body={n_body_fmul_emit} (nt=per-frame square-or-junction, fin=base x2*zinv)")
print(f"runtime_fmul = {NSTEPS} frames * 1 + 1 final = {NSTEPS+1}  (== unrolled 266)")
print(f"lets {len(lets)} ; fields {len(ALL_SLOTS)*10+2}")
