# Design: X25519 in pure Verbose (multi-precision field arithmetic + the ladder loop)

Status: **design, post-review** (2026-05-30). Adversarially reviewed by a
fresh-context subagent; this revision folds in its findings (one BLOCKER, three
MAJOR, several minor). To be re-validated by a half-day spike (§6) before any
brick is coded. This is language-infra-shaped work — read it against the
compiler axiom: the compiler applies canonical transforms and never guesses, so
every construct here must keep termination mechanically provable.

## 1. The problem and why the unroll approach hits a wall

The symmetric + KDF half of `TLS_AES_128_GCM_SHA256` is built (AES-128-GCM,
HMAC-SHA256, HKDF), each as a straight-line `let`-chain unroll. X25519 — the
key-exchange half — cannot follow that pattern:

- X25519 is a **Montgomery ladder**: it processes scalar bits 254 → 0 (255
  iterations), each doing a fixed amount of field arithmetic mod p = 2^255 − 19.
- One field multiply, unrolled, is ~300–500 `let`s. One ladder step is ~5 field
  muls + several add/sub ≈ ~2500 `let`s.
- 255 steps fully unrolled ≈ **640k `let`s** → a multi-MB binary the
  emitter/optimizer would choke on (AES-GCM at ~11k `let`s is already 625 KB).
  The wall is real and quantified.

So the ladder **must loop**. Verbose has no loop construct; the only bounded
repetition with a mechanical termination guarantee is **recursion with a
`decreasing` proof** (Phase C). That is the spine of this design.

## 2. What already works (code-verified by the infra map)

- **Recursive callable, multi-Number-field input** via the pointer-in-rdi ABI
  (`emit_self_recursive_program` `native.rs:3479`, `SelfCallCtx` `:10647`). No
  hard field cap; each field is one i64 slot, marshalled `sub rsp,8N; stores;
  mov rdi,rsp; call`.
- **Threading a multi-field record across the recursive call**:
  `step(State{a: a', …, i: s.i - 1})` — each field expr evaluated, stored at the
  slot resolved by field NAME (`native.rs:11300`+). Worked at 2 fields
  (`gcd.verbose`); wider is in-shape but **untested** (see O1/§6-S2).
- **`decreasing : i`** (verifier `check_decreasing_recursion` `verifier.rs:3557`):
  accepted iff the recursive call passes a `Record` whose field `i` is exactly
  `input.i - <positive integer literal>`, `i` is Number-typed with a declared
  range. It checks ONLY that field; the other 50 state fields may be arbitrary
  exprs. Discharges the slice-5.0 "not a termination proof" breadcrumb. The
  runtime bounds-check enforces the range at field load (sys_exit on
  out-of-range).
- **Bitwise + logical shift on i64** — `band/bor/bxor/bnot/shl/shr` all wired
  (`native.rs:12497`+); shifts are **logical**. Battle-tested for masked-i64
  limbs in `sha256_round.verbose` (`band(x, 4294967295)` 32-bit emulation). This
  is the proven idiom for multi-precision-on-i64 with manual carries.
- **`byte_at(text, index)`** — runtime-indexed byte load from immutable text,
  unsigned-bounds-checked, returns 0..255.

## 3. The two capability gaps — designed around, not closed with new primitives

The infra map confirmed exactly two gaps. Both are avoided by the
representation choice, so the design needs **no new native feature — CONTINGENT
on §6-S2 passing** (the one untested ABI width). If S2 fails, the first PR is a
scoped ABI lift (in-axiom: an ABI extension, not a heuristic), not abandonment.

### Gap A — no high-half multiply (`imul rax,rcx` keeps low 64 bits; `native.rs:11194`)

A radix-2^51 (5-limb) representation needs 102-bit products — impossible
without `mul rdx:rax`. So we use the **10-limb 25.5-bit representation** (the
`curve25519-donna` / ref10 **s64 / 32-bit** path — which uses int64
accumulators, **not** the int128 of the 5-limb variant). A field element is 10
signed-i64 limbs, alternating 26,25,26,25,…,26,25 bits:
`value = Σ limb[i]·2^ceil(25.5·i)`.

**Verified bound (the make-or-break fact).** Schoolbook 10×10 with the 2^255−19
reduction folds high terms back with a ×19 factor; odd×odd cross terms carry an
extra ×2. Worst-case accumulator per output limb, by input-limb bound:

| fmul input limbs | max accumulator | bits | fits signed i64? |
|---|---|---|---|
| `< 2^26 / 2^25` (fully reduced) | 5.6e17 | 59 | yes, 16× headroom |
| `< 2^27 / 2^26` (donna's fmul precondition) | — | 61 | yes, 4× headroom |
| `< 2^28 / 2^27` | — | 63 | barely (1.03×) |
| `< 2^29 / 2^28` | — | 65 | **NO** |

So pure-i64 holds **with comfortable headroom — no high-half multiply needed** —
**provided fmul/fsqr inputs stay `< 2^27 / 2^26`**. That precondition is a hard
invariant, not an aside (see §4.2).

### Gap B — no runtime-indexed Number array

1. **The limbs**: avoided entirely — limbs are **named fields** (`x2_0…x2_9`),
   field arithmetic is **unrolled** across the fixed 10 names. No loop over
   limbs ⇒ no indexing.
2. **The scalar bit at step i**: index is runtime, but handled by
   **`byte_at(scalar_text, b/8)` + shift `b%8` + mask** — existing primitive, no
   new feature, provided the scalar is a `text` value.

**Split: unroll the field arithmetic (fixed limb count); loop only the ladder
(255 steps) via recursion.**

## 4. Proposed architecture

### 4.1 Representation & invariants

- Field element = 10 Number fields (`*_0…*_9`), 26/25-bit signed-i64 limbs.
- **Invariant I1 (non-negative limbs).** Every limb is `≥ 0` at all times. This
  makes logical `shr` coincide with arithmetic shift everywhere (a negative limb
  logical-shifted right is silently garbage — the exact "looks random" bug class
  in §8). I1 is enforced by the donna **`fsub` bias**: `out[i] = 2p[i] + a[i] −
  b[i]` with `2p[0]=2^27−38`, odd limbs `2^26−2` — these exceed any subtrahend
  limb, so results stay `≥ 0`. **There is therefore no need for an arithmetic-
  shift primitive.** (Resolves the old S1 "ashr vs non-negative" false choice: I1
  is mandatory anyway because §3 Gap-A's bound assumes it.)
- **Invariant I2 (fmul input magnitude).** Inputs to `fmul`/`fsqr` are
  `< 2^27 / 2^26`. Enforced by: `fmul`/`fsqr` always end with a full carry pass
  (`freduce`) so their **outputs** are `< 2^26 / 2^25 + ε`; `fadd`/`fsub`
  outputs (up to ~2^27) are only ever consumed **directly** by an `fmul`, never
  chained into another `fmul` without an intervening reduce. I2 is the spine of
  correctness — violate it and accumulators pass 2^63.
- Scalar = 32-byte `text`, **clamped inside `bit_of` (see §4.3)** — it cannot be
  pre-clamped because text is immutable.
- Base-point u = 10 limbs, decoded from 32 bytes (Brick 0).

### 4.2 Field-arithmetic helper rules (all UNROLLED, non-recursive)

- `fadd`, `fsub` (the biased form, I1), `fmul` (10×10 + 19-fold reduction; the
  crux), `fsqr` (≤ fmul bound, safe), `fmul121665` (×a24), `freduce`/`fcarry`
  (carry pass enforcing I2), `cswap(bit,a,b)` (branch-free, per limb:
  `mask = 0 − bit; t = mask & (a^b); a ^= t; b ^= t`).
- These are **inlined** at the ladder-step call sites so each produces 10 named
  output lets.
- **Brick-1 validation asserts both equality AND I1** (no limb negative at any
  shift site) — an output-only check could hide a logical-shr-on-negative bug
  behind inputs that happen to stay positive.

### 4.3 The ladder as a recursive callable

State threaded through recursion (single multi-field record):

```
LadderState {
  x2_0..x2_9, z2_0..z2_9,     // 20 limbs
  x3_0..x3_9, z3_0..z3_9,     // 20 limbs
  x1_0..x1_9,                 // 10 limbs: base-point u (constant across steps)
  swap,                       // accumulated swap bit (0/1)
  i : number [0, 255]         // counter — the decreasing field
}                             // ≈ 51 Number fields + the scalar (text)
```

**Loop bound (BLOCKER fix — process bits 254..0 inclusive, 255 steps).** The
counter starts at **255**; each step processes scalar bit **`i − 1`** and
finalize happens at `i == 0`. So step `i=255`→bit 254, … step `i=1`→bit 0, then
`i=0`→done. (The earlier draft finalized at `i==0` while only processing bits
254..1, silently dropping bit 0 — every output would have been wrong.)

```
ladder(s, scalar) =
  if s.i == 0 then
    s                                   // ladder done; inversion+encode done OUTSIDE (§4.4)
  else
    let bit = bit_of(scalar, s.i - 1)   // clamped bit extraction
    let sw  = bxor(s.swap, bit)
    ... cswap(sw, x2,x3); cswap(sw, z2,z3) ...
    ... Montgomery differential add-and-double (fmul/fsqr/fadd/fsub/fmul121665) ...
    ladder(LadderState{ x2_0: …, …, x1_k: s.x1_k, swap: bit, i: s.i - 1 }, scalar)
```

`decreasing : i` (range `[0,255]`, update `s.i - 1`) discharges termination —
it checks only field `i` among the 51, which the verifier supports.

**Scalar clamping folded into `bit_of` (MAJOR fix).** RFC 7748 clamp mutates
bytes (`s[0]&=248; s[31]&=127; s[31]|=64`); we can't mutate immutable text, so
`bit_of(scalar, b)` returns the *clamped* bit directly:
- `b ∈ {0,1,2}` → 0 (clear low 3 bits)
- `b == 254` → 1 (set bit 254)
- `b == 255` → 0 (cleared — and never queried anyway, since we process 254..0)
- else → `band(shr(byte_at(scalar, b/8), b mod 8), 1)`

`b/8` and `b mod 8` derive from the public loop counter, so `byte_at`'s
bounds-check is not on a secret index (constant-time-relevant; see §8).

**O1 — the pivotal unknown:** the callable takes ~51 Number fields **plus** one
`text` field (scalar). Multi-Number works; text-input works; **mixed
many-Number + text in one recursive callable is UNTESTED**, as is the
pointer-in-rdi struct at ~51 fields and the call-site marshalling that rebuilds
a ~408-byte struct (each field expr = a full inlined fmul-chain result) every
step. Resolved by §6-S2 **before** any brick. Fallback (pack scalar into Number
words) reintroduces Gap B (runtime word select) ⇒ would need a scoped ABI lift
first; the doc does not claim "no new primitive" unconditionally — it is
contingent on S2.

### 4.4 Final inversion & encode — a SEPARATE top-level rule, not the base case

z2^(p−2) mod p (Fermat inverse) is a fixed ~265-`fmul`/`fsqr` addition chain
(~26k lets unrolled), then `x2 · z2^(p−2)`, then encode 10 limbs → 32 bytes.
**Run this as a top-level rule invoked AFTER the ladder returns its final
state**, not embedded in the recursive base case: a 26k-let unroll belongs on
the proven large-top-level-unroll path (the one AES-GCM uses), not the untested
large-callable-body path. Validate against Python `pow(z, p-2, p)`.

## 5. Incremental brick sequence (one validated demonstrator at a time)

0. **`codec`** — byte↔limb pack/unpack across the 26/25 boundary (bits straddle
   byte edges — classic bug site). Validate against Python. *Must come first:
   fmul's random-vector test needs correct field elements as input.*
1. **`field_mul`** — `fmul`/`fadd`/`fsub` on the 10-limb representation,
   UNROLLED. Validate against Python `(a op b) mod (2^255−19)` over many random
   vectors + edge cases (0, p−1, max-carry), **and assert I1** (no negative limb
   at a shift). *Make-or-break gate.*
2. **`ladder_step`** — one Montgomery differential add-and-double, UNROLLED,
   vs a Python single-step reference.
3. **`ladder_recursive`** — full 255-step ladder via recursion + `decreasing`.
   First a TINY counter (≈4 steps) vs Python, then full 255. **Exercises O1.**
4. **`x25519`** — clamp (in `bit_of`) + ladder + Fermat inverse + encode,
   end-to-end vs **RFC 7748 §5.2 vectors** + the Alice/Bob shared-secret vector.

Stop after each brick; do not build N+1 until N matches its reference.

## 6. Spike to run BEFORE any brick (hard gate)

- **S1 — confirm I1 makes logical shr sufficient.** One-limb test: with the
  2p-biased `fsub`, verify every carry-extraction site sees a non-negative value,
  so logical `shr` is correct. (Expected: yes; this is a verification, not a
  build-vs-workaround decision.)
- **S2 — wide mixed recursive input (O1), the pivotal gate.** Compile a
  throwaway recursive rule with ~51 Number fields **+ 1 text field** that threads
  them and decrements a counter. Check, concretely: (a) it compiles and runs;
  (b) measure the emitted frame size and call-site byte count at that width
  (not just "compiles"); (c) `decreasing` verifies with the counter buried among
  50 other fields; (d) limb fields are declared **without** ranges (or wide
  `[0,2^27]`) so the runtime bounds-check doesn't reject legitimate limb
  magnitudes. **If S2 fails, the first PR is the scoped ABI lift its refusal
  names — before any crypto brick.**

If S1 + S2 are clean, the rest is a large but mechanical mirror of
curve25519-donna with validated bricks.

## 7. What this design explicitly does NOT do

- No general-purpose loop construct — bounded recursion + `decreasing` is the
  only iteration, termination stays mechanically proven.
- No runtime-indexed Number arrays — limbs are named fields; the only runtime
  index is `byte_at` into the immutable scalar text.
- No high-half multiply / no add-with-carry — the 26/25-bit width + I2 keep
  every intermediate in i64.
- No arithmetic-shift primitive — I1 (non-negative limbs) makes logical shr
  sufficient.
- No `%`-based field reduction — the signed-`idiv` trap is avoided; reduction is
  the donna carry chain folding 2^255 ≡ 19.

## 8. Risks (honest)

- **Limb-arithmetic correctness is unforgiving.** One wrong shift/mask in `fmul`
  → output matches nothing. Mitigated: `field_mul` is brick #1 with random +
  edge-case validation and the I1 assertion.
- **I2 discipline is easy to violate by accident** (chaining `fmul(fmul(...))`
  without a reduce, or `fadd`→`fadd`→`fmul`). Every ladder-step expression must
  be written so each `fmul` input is a freshly-reduced element or a single
  `fadd`/`fsub` of reduced elements.
- **O1 (S2) is the pivotal unknown.** The "no new native primitive" property is
  contingent on it. The spike resolves it before sunk cost.
- **Call-site marshalling pressure at ~51 fields.** Each step rebuilds a
  ~408-byte struct whose field exprs are full inlined fmul-chains; the per-step
  body (~2500 lets) is emitted once in the callable but the marshalling
  re-references all 51 results. Verify emit handles this (part of bricks 2→3).
- **Constant-timeness is a claim not to overstate.** `cswap` is branch-free and
  `bit_of`'s index is public, but a full constant-time audit is its own task.
  Call it a "branch-free ladder", not "constant-time", until audited.
