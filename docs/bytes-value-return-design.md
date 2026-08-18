# Aggregate return — design note (native slice "agg-1")

**Status:** design note, **NO implementation**. Written 2026-08-18, revised the same
day after review, against `main = 47d7b8d`.

**Filename note.** The file is named for the question that was *asked* — "can a bounded
`bytes` value be returned by value in registers?" — because that is the search term a
future reader will use. The answer it reaches is **no, and the value should be a declared
record delivered through a caller-allocated destination**. §2.1 rejects the by-value
`bytes` shape on measured grounds; §8 lists everything in the two briefs that turned out
to be wrong, including four arithmetic claims.

**Revision note (read this before trusting a previous copy).** The first draft's *shape*
survived review; its *specification* did not. Three paragraphs an implementer would have
copied verbatim would have produced a **wrong binary rather than a refusal**: §3.3
clobbered `rsi`, §3.4 wrote through `rdx` where every emitter here leaves `rax`, and
§3.4's safety argument for the A2/A4 register-allocation fast path was **factually
inverted** — a record output makes `qualifies_rbx` *true*, not false, so the destination
pointer would have landed on the caller's saved `rbx`. Those are fixed below and each
carries a negative control in §6.4. §1.3's performance numbers were unrecoverable,
mutually inconsistent, and priced the wrong slice; they are replaced with a measurement
taken for this note, with its method and its limits recorded. Every citation in this
revision was opened and read.

**Companions:** [composition-abi-design.md](composition-abi-design.md) (the 2026-06-02
"don't build it yet" decision this note revisits), [native-call-convention-design.md](native-call-convention-design.md)
(the shipped call ABI), [effect-model.md](effect-model.md) (the eight-question checklist a
new capability signs against), [tls13-roadmap.md](tls13-roadmap.md) §6 (Gap C, the consumer).

---

## 1. The problem, and what is already settled

### 1.1 The wall, stated precisely

A Verbose rule cannot return an **aggregate** to another rule *in the native backend*.
Native's four return conventions are:

| Output type | Convention | Where |
|---|---|---|
| `number` / `bool` | value in `rax` | `src/native.rs:4567` (`emit_itoa_inline` at the driver) |
| `text` | `(rax = ptr, rdx = len)` | `src/native.rs:4553-4564` — the driver does `mov rsi, rax` and leaves `rdx` **unmoved** as `write`'s third argument |
| a `concept_group` (sum type) | one i64 **arena index** in `rax` | `src/native.rs:460-466` (`is_group_output`) |
| `bytes` | **nothing** — the bytes are streamed to fd 1 during the walk and the `(rax, rdx)` pair is explicitly discarded | `src/native.rs:4535-4542` |

A plain record and a `Result` are refused outright:

```
recursive rule 'step' output must be Number, Bool, Text, or Bytes (got Named("Pair"));
Result/Record returns are later slices.
```
— `src/native.rs:469-476`, reproduced live (§1.4).

**Read the guard on that refusal, because slice 1 has to lift more than it looks like.**
The check lives inside `for r in &scc_rules_owned` and is preceded by
`if !entry_is_recursive { continue; }` (`src/native.rs:365-368`). So it fires for **every
rule routed to the callable path whenever the ENTRY is recursive** — not only for the
recursive rule itself. A *non*-recursive record-returning callee sitting inside a
recursive program hits the same message, and slice 1 must lift that case too.

The consequence, in the words of the generator that hit it first:

> *"Verbose/native rules return a SINGLE scalar (selected by `which` at the base case); a
> rule cannot return 10 field limbs to a caller, so per-block `call fsq_pow2` is not
> expressible. The state record carries the ENTIRE machine state."*
> — `tools/tls_gen/x25519rec_gen.py:38-41`

### 1.2 The consumer: `which`-parameterised process spawning

Because a rule returns one scalar, every crypto primitive carries a `which : number`
selector field and is invoked **once per output unit**, as a separate process:

```python
def run_bytes(rule, args, n):
    """Spawn all n `which` values in parallel; return bytes."""
    binp = BIN[rule]
    futs = {w: _POOL.submit(_one, binp, args, w) for w in range(n)}
    return bytes(futs[w].result() for w in range(n))
```
— `tools/tls_gen/vcrypto.py:44-48`, over a 64-worker pool (`vcrypto.py:19`).

Measured spawn counts:

| Operation | Spawns | Cite |
|---|---|---|
| X25519 | **52** = 20 (`ladder`) + 32 (`x25519_finish`) | `vcrypto.py:68`, `vcrypto.py:91` |
| SHA-256 | 32 (independent of message length) | `vcrypto.py:103` |
| Each key-schedule secret | 32 | `vcrypto.py:107-119` |
| AES-GCM encrypt, L-byte plaintext | 48 + L | `vcrypto.py:140-148` |
| One TLS record AEAD | ≈112 | `docs/tls-io-statemachine-design.md:133` |

Every one of those spawns **recomputes the whole primitive** and throws away all but one
byte. `examples/p256_fmul.verbose` is the clearest specimen: its body computes 32 output
bytes into `let e_ob0 … e_ob31` and then discards 31 of them in a 32-arm dispatch chain
(`out = if g.which == 0 then e_ob0 else if …`). The `which` field is declared
`number [0, 31]` at `examples/p256_fmul.verbose:27`.

This is `docs/tls13-roadmap.md` **Gap C**. HTTPS is blocked on it: the TLS driver must
pipe a 32-byte X25519 shared secret into HKDF into AES-GCM *in one process*, and today it
cannot, so the driver lives in Python (`vcrypto.py:64-91`, `:107-119`, `:140-153`) while
`examples/policy_proxy.verbose` proves the *non*-crypto half of the same server compiles
to 2519 B of Verbose-emitted machine code.

### 1.3 What the measurement says — and what it does NOT say

**The previous version of this section quoted "2.38 cycles per 8-byte word per call",
"p256_fmul's body is 6 670 cycles", and "~26× less CPU, 59 ms → 4.5 ms". All of it is
struck.** Not deferred to a later section — struck. `grep -rn "2\.38\|6670" docs/ src/`
returns nothing, so the method was unrecoverable; "59 ms → 4.5 ms" is 13×, not the 26×
quoted beside it; the repo's own recorded X25519 figure is **0.1 s**, not 59 ms
(`docs/tls13-roadmap.md:227-229`); and "0.14 % at 32 bytes" divided by the byte count when
§2.1(a) establishes that a 32-byte value is a **32-field, 256-byte record** in this
codebase, which §2.2 prices ~8× higher on its own figure. *(Two of the derived numbers —
"~4.5 ms", "~26x less CPU" — **are** in the repo, at `docs/tls13-roadmap.md:234-236`, added
by PR #176. So "nothing is recorded" was itself too strong; what is unrecorded is the pair
of primitive measurements the derivation rests on, which is the half that matters.)*

`HEAD~3` is literally the commit that fixed the previous instance of this failure mode
(`628a820`, *"docs: replace the unmeasured '~5 s ladder' figure with a measured 0.1 s"*),
whose own note reads that the stale figure *"was wrong by ~2 orders of magnitude — which
mattered, because it was the number that made Gap C look like a performance emergency
rather than an expressiveness one"* (`docs/tls13-roadmap.md:230-236`). Repeating it four
commits later would be the same defect with a different number.

#### The measurement taken for this note

**What was measured: the 74-spawn HKDF chain that `hkdf_matches_rfc5869` already runs.**
That test's `run_bin` closure (`src/native.rs:42322-42333`) spawns one process per output
byte — 32 for `hkdf_extract` (`:42334`) and 42 for `hkdf_expand` (`:42354`). It needs no
compiler change to time, and it is exactly the chain §6.3 proposes as slice 1's second
milestone.

*Method.* Binaries built at `47d7b8d` with `cargo build --release`:
`examples/hkdf.verbose --native … --run hkdf_extract` (328 738 B) and `--run hkdf_expand`
(645 596 B); the process floor is `examples/invoices.verbose --native … --run
important_invoice` (683 B, a static no-libc Verbose ELF — `/bin/true` is the *wrong*
floor, and measured *slower* than both crypto binaries, because it pays glibc's dynamic
loader). A `/bin/sh` script issues the 74 invocations sequentially with the test's own
argv, stdout to `/dev/null`; before timing, a Python mirror of the same loop was checked
against RFC 5869 Test Case 1 and reproduced the published PRK and OKM byte-for-byte.
Timed with `hyperfine -N` (no shell wrapper). Host: WSL2 (kernel
`5.15.153.1-microsoft-standard-WSL2`), AMD Ryzen 7 5800X, 16 threads.

| | mean ± σ | runs |
|---|---|---|
| the 74-spawn Extract→Expand chain | **19.6 ms ± 1.0 ms** | 100 |
| one `hkdf_extract` spawn (one `which`) | 260.5 µs ± 36.2 µs | 1000 |
| one `hkdf_expand` spawn (one `which`) | 282.1 µs ± 27.4 µs | 1000 |
| process floor (683 B static Verbose ELF) | 208.5 µs ± 16.0 µs | 1000 |

Two readings, and the first is the honest headline:

- **74 × 208.5 µs = 15.4 ms, i.e. ~79 % of the chain's wall time is process creation, not
  crypto.** The `which` pattern's cost is overwhelmingly *spawning*, and eliminating it is
  a spawn-elimination win.
- A one-process chain pays the floor once and each body's non-floor cost once:
  208.5 + 52.0 + 73.6 ≈ **334 µs**, i.e. **≈ 59×** less wall time than 19.6 ms.

*What that projection is not.* It is arithmetic over measured single-spawn costs, **not a
measurement of a binary that does not exist**. The composed image would be ~975 KB, so its
own exec paging is not free and the real figure will be worse than 334 µs. A second
instrument disagrees about the split: `getrusage(RUSAGE_CHILDREN)` puts per-spawn child
CPU at 3.4 µs (extract) and 7.1 µs (expand) against a 3.9 µs floor — i.e. the arithmetic
itself is single-digit microseconds and nearly all of the 52/74 µs delta is setup and
demand paging, not compute. **The two instruments were not reconciled and this note does
not pretend they were.** Process creation on WSL2 is also slow; on a native kernel the
floor drops and the ratio changes. Re-measure on the deployment host before quoting.

#### The prize is assurance, and slice 1's CPU prize is zero

Two corrections to the previous framing, both load-bearing:

1. **The 52 → 1 X25519 prize belongs to agg-2, not slice 1.** Both X25519 rules are
   *self-recursive* — `examples/ladder_recursive.verbose:1470` declares `calls : [ladder]`
   and `examples/x25519_rec.verbose:395` declares `calls : [x25519_finish]` — and slice 1
   refuses a recursive callee (§6.1, refusal #1). **Slice 1 delivers no X25519 speedup at
   all.** Quoting X25519's numbers next to slice 1 prices the wrong work.
2. **Slice 1's case is assurance.** The TLS key schedule is Python
   (`tools/tls_gen/vcrypto.py:107-119`), so every proof, every declared read and every
   bound stops at the process boundary. Slice 1 moves a stage of that driver **inside the
   verifier's reach**. CPU is a second-order effect and belongs to agg-2. §8.6 already
   said this; §1.3 and §2.5 used to contradict it, and no longer do.

### 1.4 What already works — measured here, not assumed

The following program **verifies clean and runs correctly today**, on `47d7b8d`, with no
compiler change. It is printed in full, because the previous draft printed an abbreviated
form that would not have parsed (no `@intention`, no `@source`, no `proofs:` — all three
mandatory) and then claimed the printed text verified.

```verbose
@verbose 0.1.0

concept In
  @intention: "two bytes to be swapped"
  @source: aggregate_pair.intent:1
  fields:
    a : number [0, 255]
    b : number [0, 255]

concept Pair
  @intention: "the swapped pair, returned as an aggregate"
  @source: aggregate_pair.intent:2
  fields:
    x : number [0, 255]
    y : number [0, 255]

rule swap2
  @intention: "return both bytes swapped, as one record value"
  @source: aggregate_pair.intent:3
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { x: i.b, y: i.a }
  proofs:
    purity:
      reads : [i.a, i.b]
      calls : []
    termination:
      bound : 3

rule total
  @intention: "compose: call swap2 and read both fields of the returned aggregate"
  @source: aggregate_pair.intent:4
  input:
    i : In
  output:
    out : number
  logic:
    let p = swap2(i)
    out = p.x * 1000 + p.y
  proofs:
    purity:
      reads : [i]
      calls : [swap2]
    termination:
      bound : 10
```

**Note `reads : [i]`, not `[i.a, i.b]`.** Passing the whole record to a call records a read
of the *binding*, and `[i.a, i.b]` is rejected with `missing: [i], extra: [i.a, i.b]`.
That is not a quirk to work around — it is the correct declared read set for a rule that
hands its whole input to a callee, and §4.1's HKDF sketch has the same shape.

| Invocation | Result |
|---|---|
| `verbosec agg.verbose` | `verified: 2 concept(s), 2 rule(s); all proofs check out` |
| `--run total --input agg.json` (a=7, b=9) | `out = 9007` — **correct** |
| `--native --run swap2` | **821 B binary**, prints `{"x":9,"y":7}` |
| `--native --run total` | rc 1, **zero bytes**: `native codegen error: rich operations (collection/result/record/concat) not supported in native backend` — `src/native.rs:14706` |
| `--wasm --run total` | `wasm error: unsupported expression in WASM backend` |
| `--compile --run total` | `rustc` fails, `E0308` + `E0610` (`codegen.rs` folds `Record`/`Field` to the literal `false`) |

Three things follow, and they shape everything below.

1. **This is not a language gap and it is *mostly* not a verifier gap.** The verifier
   accepts a record-returning rule, a record-typed `let`, and `.field` on it — and since
   PR #178 it **typechecks** that `.field` (§4.2). The interpreter composes them:
   `Expr::Call` (`src/interpreter.rs:772-791`) returns whatever `eval_rule_with_value`
   (`:290`) produces, including a `Value::Record`; `:318-330` reads a field off one and
   `:586-596` builds one. This confirms `composition-abi-design.md:93-95` — *"make two
   backends do what the interpreter already does"*.
2. **The aggregate is already computed natively.** `--native --run swap2` emits 821 bytes
   that construct the record and serialise it. The record is *streamed to stdout as JSON*
   instead of *returned*. The missing piece is a destination, not an arithmetic.
3. **There are three different refusals for the same gap**, and none of them names it:
   `src/native.rs:14706` (rich ops), `src/native.rs:469-476` (record output on the callable
   path), and `src/native.rs:13769` — `unknown field 'x' in native codegen`, the bare-name
   lookup failure. Slice 1 must collapse these into one breadcrumb.

**One residual verifier gap, found while measuring this section and NOT closed by PR
#178.** A record-typed binding used as a *whole value* in a scalar position is still
accepted:

```verbose
let p = swap2(i)
out = p * 1000        -- verifies clean at rc 0
```

`--run` then fails at runtime with `cannot apply Mul to {y: 7, x: 9} and 1000`. PR #178
typechecked `.field` *on* a record binding; it deliberately did not add a refusal class for
a bare record `Ident` in arithmetic position (its commit body lists that omission as one of
the six deliberate silences). Slice 1's refusal #5 must therefore be enforced in
`src/native.rs` on its own and must not assume the verifier has already screened this shape.

---

## 2. Options considered

The constraint every option is judged against: **reading a declared aggregate with a
bounds-checked index is not C; writing at a computed offset IS C and is off the table.**
The project has already ruled on the adjacent question, and the ruling is load-bearing here
— of the shipped pointer-in-rdi *input* ABI, CLAUDE.md says:

> *"**No pointers in the language**: the rdi pointer is an ABI artifact between caller and
> callee, lifetime bounded by the call/ret pair (`sub rsp` allocates, `add rsp` frees). No
> syntax for `&x`, `*p`, casts, or arithmetic on it."*

So the C-line is drawn **in the language**, not in the emitter. An address that exists only
between a `call` and its `ret`, at compile-time-constant offsets, is already blessed.

### 2.1 Option A — by-value `bytes` in registers (the original proposal) — **REJECTED**

*Shape:* a bounded `bytes` value returned as N packed registers (32 bytes = 4 registers),
no address anywhere. Free caller-saved registers at a `ret`: `rax, rcx, rdx, rsi, rdi, r8`
(48 B), `r10` de-facto free (56 B). `r11` is the arena base and is TRUSTED across `call`/`ret`
(`src/native.rs:4870-4877`); `r12`/`r13`/`r14` are `_start`-loop state; `rbx`/`r15` are
callee-saved on the A2/A4 paths (`src/native.rs:4786`, `:4793`, `:5139-5143`).

**Register liveness is NOT the objection, and it is worth saying so.** A `call` already
clobbers `rax` and `rdx` (the text pair), and a callee's body freely clobbers
`rcx/rsi/rdi/r8-r10` (every syscall, `emit_strlen`, `emit_itoa_inline`). The A4 brick exists
*because* the authors already knew this: it spills a binop's LHS to `r15`, a callee-saved
register, specifically to survive a sibling recursive call (`src/native.rs:4777`, `:4793`). So the caller-saved set is genuinely free. Five objections stand anyway.

**(a) The consumers' aggregates are not byte strings.** Every crypto rule's input concept is
a record of **one-byte-per-field `number [0, 255]` fields** (field counts below are counted
from the declarations, not estimated):

| Concept | Shape | Fields | Cite |
|---|---|---|---|
| `GcmInput` | `k0..k15`, `iv0..iv11`, `p0..p15`, `which` | 45 | `examples/aes_gcm.verbose:9-57` |
| `ExtractInput` | `salt0..salt63`, `ikm0..ikm21`, `which [0,31]` | 87 | `examples/hkdf.verbose:3-93` |
| `ExpandInput` | `prk0..prk63`, `info0..info9`, `which [0,41]` | 75 | `examples/hkdf.verbose:95-173` |
| `Secret` | `s0..s31`, `which [0,15]` | 33 | `examples/hkdf_expand_label.verbose:3-39` |
| `HmacInput` | `k0..k63`, `m0..m7`, `which [0,31]` | 73 | `examples/hmac_sha256.verbose:3-79` |

So **a 32-byte value is a 256-byte struct in this codebase's representation**, not 32 bytes.
"≤ 32 bytes fits in four registers" is true of the abstract datum and false of every actual
consumer. Making it true would require re-emitting every crypto file under a packed-word
convention.

**(b) It forces the hardest open question to be answered.** With a packed `bytes` value, the
only way to feed `prk0..prk63` is `byte_at(v, i)`. At a *runtime* index over registers that
needs a jump table or a spill-to-stack-then-index — and the spill reintroduces exactly the
address the option was chosen to avoid. A record return makes the question **not arise**
(§5).

**(c) It does not remove the memory write; it adds one.** A `let`-bound aggregate needs a
frame home, because `.field` reads happen after the call returns. Count the traffic for an
N-word result:

| | callee | caller | total |
|---|---|---|---|
| destination-pointer return | N stores directly into the let's slots | 0 | **N stores** |
| register return | N `mov reg, val` | N stores into the let's slots | **N movs + N stores** |

Registers win only when the value is consumed exactly once inside the same expression — and
for that case a non-cyclic callee is **inlined** today, so no call happens at all.

**(d) It caps at 6 words and needs a second convention immediately.**
`composition-abi-design.md:139-147` was right that a field element is 10 limbs; the
rebuttal ("that sized the objection on the crypto INTERMEDIATE") is itself wrong about the
consumer — see §8.1. The `ladder → x25519_finish` boundary carries **20 limbs**, not 32
bytes (`examples/ladder_recursive.verbose:59` declares `which : number [0, 19]`;
`vcrypto.py:68-70` slices the 20 results as `limbs[0:10]` = x2, `[10:20]` = z2), and it is
**20 of X25519's 52 spawns**. Six registers cannot carry it.

**(e) `bytes` has no exact declared width.** `Type::Bytes` is a unit variant
(`src/ast.rs:421`); the width lives on the *field* as `range: Option<(i64, i64)>`
(`src/ast.rs:386-391`), where `[..N]` parses to a **maximum** (`src/parser.rs:425-431`) that
native enforces at runtime as a bound, not an equality (`emit_text_bound_check`,
`src/native.rs:2461`). A fixed register quad therefore needs a companion length register, at
which point the value is a variable-length buffer in registers, its `byte_at` bound is a
runtime length, and the natural next request is to store it. A concept's field list, by
contrast, is an **exact** width, declared, already parsed, already used for the input struct.

**Verdict: reject.** The shape is not wrong in spirit — a value with no address is the
strongest C-line story available — it is wrong for *these consumers*.

### 2.2 Option B — caller-allocated destination (sret) — **RECOMMENDED**

*Shape:* the caller reserves the destination in its own frame and passes its address; the
callee writes N words there at compile-time-constant offsets and returns.

- **Cost:** N stores in the callee and nothing in the caller (table in §2.1c), against a
  measured baseline where **79 % of the chain's wall time is process creation** (§1.3). The
  per-call memory traffic that `composition-abi-design.md:232-235` set the "a few %"
  threshold on has *not* been isolated here and is not claimed to have been; what has been
  measured is that the thing it would replace costs ~208 µs per unit of output. And the cost
  is only ever paid by rules on the callable path; a non-cyclic callee is inlined today, so a
  *small* callee never pays it.
- **Reuses shipped machinery, which is the decisive argument.** The layout loop
  (`struct_layouts`, `src/native.rs:4391-4410`), the per-field copy helper
  (`emit_copy_rbp_to_rsp`, used at `src/native.rs:14310-14314` and `:14335`), the callable
  prologue/epilogue (`emit_callable_into`, `src/native.rs:4641` / epilogue `:5124-5148`), and
  the frame discipline all exist. CLAUDE.md's own lesson: *"every one of the three was fixed
  by WIRING, not by new machinery … Check for an existing emitter before writing one."*
- **Symmetric with the input ABI**, so the audit sentence is one sentence for both
  directions: *aggregates cross a call boundary by a caller-allocated buffer at
  compile-time-constant offsets, whose lifetime is bounded by the call.*
- **The §4.3 "genuinely new machinery" already ships.** `examples/x25519_rec.verbose` nests
  265 frames × 112 fields ≈ 237 KB of caller-allocated struct buffers on the default 8 MB
  stack. A 32-slot destination per frame adds 265 × 256 B ≈ 68 KB — still under 4 % of the
  budget. *(112 is the field count of `X25519RecState`, counted from the declaration; a
  `grep -c " : number"` on that file returns 113 because it also matches the rule's
  `out : number`.)*
- **C-line:** passes, by the CLAUDE.md ruling quoted above. No `&x`, no `*p`, no cast, no
  arithmetic on the pointer, no indexed store — offsets come from the concept's declaration
  order. Non-aliasing of the destination slot groups is a **claim that must become a test**,
  not a sentence — see §9 and §6.4's negative control.
- **Failure mode:** a destination whose lifetime is mis-computed — a callee writing into a
  frame the caller has already popped. Contained in slice 1 by requiring the destination to
  be a `let` slot group in the *live* caller frame, and by refusing recursion (which is the
  only shape that needs a per-frame temporary).

### 2.3 Option C — arena handle (i64 index in `rax`) — **REJECTED for this consumer**

*Shape:* already ships. A `concept_group` output returns one arena index
(`src/native.rs:460-466`); `examples/label_tree.verbose` exercises it end-to-end.

- **Cost:** zero new ABI — genuinely tempting, and it must be argued down rather than ignored.
- **Why not:** (i) the crypto concepts are *plain records*, not sum types; converting them
  means every `g.k0` becomes a `match` arm, i.e. rewriting every generator. (ii) The arena is
  a single global mutable region with a reset discipline, and its dangling hazard is
  documented in the language itself — `Expr::ArenaScope` is *"Restricted to a bytes-returning
  (streaming) position — a stored / let-bound result would dangle after the reset"*
  (`src/ast.rs:734-738`). A let-bound crypto aggregate is exactly the refused shape.
  (iii) Arena budget is already a live operational hazard: gen0 runs at 8 M nodes and its
  exhaustion mode is a *partial ELF at exit 1* that looks like a codegen bug (CLAUDE.md,
  "gen0's node arena is a first-class budget").
- **C-line:** passes (an index is a number), but at the price of a global mutable store,
  which is the *weaker* safety story of the two.

### 2.4 Option D — status quo (reuse stays generator-side) — **REJECTED, with one concession**

`composition-abi-design.md:193-203` argues this may be the correct *permanent* answer: the
generators act as macro expanders, the emitter stays fully inlined and minimal, and
duplication in the binary is harmless.

**That argument is still correct for the crypto *hot path*, and this note does not overturn
it.** An inlined, SSA-folded `fmul` is the fast path; a shared callable `fmul` is a size win
the project does not need. What Option D cannot do is put the **driver** in Verbose — and
that, not dedup, is Gap C. `tls_gen/vcrypto.py:64-91`, `:107-119`, `:140-153` is a TLS key
schedule written in Python, i.e. the protocol logic sits outside everything the verifier
checks. That is the argument for building, and it is an *assurance* argument.

**Concession — a packing interim exists today with no compiler change, and it is smaller
than previously claimed.** A rule may already return several values packed into one i64,
using shipped primitives. Measured at `47d7b8d`, both packings, both directions:

```verbose
-- 8 bytes per i64
let w = bor(p.a, shl(p.b, 56))                 -- a = 1, b = 255
out = w                       -- native: -72057594037927935  (= 0xFF00000000000001)
out = band(w, 255)            -- native: 1
out = band(shr(w, 56), 255)   -- native: 255      (logical shr, round-trips)

-- 2 x 26-bit limbs per i64
let w = bor(p.a, shl(p.b, 26))                 -- a = b = 67108863 = 2^26 - 1
out = w                       -- native: 4503599627370495  (= 2^52 - 1)
out = band(w, 67108863)       -- native: 67108863
out = band(shr(w, 26), 67108863)  -- native: 67108863
```

**The previous draft said this takes X25519 from 52 spawns to 8. It does not — it takes it
to 14, and getting that wrong repeats the exact error §8.1 exists to correct.** Eight bytes
per i64 applies only to the 32-byte `x25519_finish` half (32 → 4 words). The `ladder` half is
**20 limbs of 26 bits** (`examples/ladder_recursive.verbose:7` declares the limb range
`[0, 67108863]`, `:59` declares `which : number [0, 19]`), and only 2 of those fit in an i64
— 52 bits used of 64 — so 20 → 10 words. **52 → 14.**

It is also not cost-free: it requires regenerating the `.verbose` files with a packed `which`
tail, work that slice 1 makes obsolete. Evaluate it on its own merits and do *not* use it as
an argument against building the return, **because it leaves the driver in Python** — which
is §8.6's point and the reason Option D loses.

### 2.5 Summary

| Option | Covers 20 limbs? | New machinery | C-line | Cost | Verdict |
|---|---|---|---|---|---|
| A — by-value registers | ✗ (6 words) | packing engine, runtime-index extraction | strongest, but reopened by extraction | no memory write, but adds N movs (§2.1c) | **reject** |
| B — caller-allocated destination | ✓ any width | destination slot group + 2 epilogue shapes | passes (blessed by slice 5.3) | N stores per call; per-call traffic **not isolated** (§1.3) | **recommend** |
| C — arena handle | ✓ | none | passes, but global mutable store | arena write+read, budget | reject here |
| D — status quo | n/a | none | n/a | driver stays outside the verifier | reject as the endpoint; keep for the hot path |

*(The previous version of this table put "26× CPU" in Option D's cost cell and "0.14–0.36 %"
in Option B's. Both were unrecoverable numbers, and the first priced X25519 — a workload
slice 1 does not touch. The cells now say what was measured and admit what was not.)*

---

## 3. The recommended shape and its exact ABI

> **An aggregate is a value of a declared plain-record concept. It crosses a call boundary
> through a destination the caller allocates in its own frame, whose address is passed in
> `rsi`. Its width and field order are the concept's declaration — never inferred.**

### 3.1 Layout

The destination layout is **byte-identical to the shipped input fields-struct layout**, and
must be produced by the same code path (the `struct_layouts` loop, `src/native.rs:4391-4410`
— a local `HashMap` built by a loop over `scc_rules`, not a function): number/bool field →
8 B, text field → 16 B (ptr, len), in **concept declaration order**. Field `i` of concept `C`
is at `dest + offset_C(i)`.

Declaration order comes from `concept.fields`, a `Vec` — never from a `HashMap`. This is not
a stylistic note: `src/native.rs:14318-14329` carries a comment recording that iterating
`offsets` (a `HashMap`) at a call site produced *"a different byte sequence on every
compile … precisely the same-size-different-bytes signature that makes a real codegen
regression unreadable."* The destination emitter inherits that hazard and must sort by, or
index, declaration order.

### 3.2 Registers

| Register | Role at the call | Note |
|---|---|---|
| `rdi` | input value (single scalar field) or pointer to the input fields-struct | **unchanged** — `src/native.rs:4489-4516`, `:4805-4838` |
| `rsi` | pointer to the caller-allocated destination | **new**, and only for record-returning callees |
| `rax` | on return, the destination pointer | mirrors SysV; a `mov rax, [rbp - disp8]` reload is 4 bytes; defined beats undefined |

**Deliberate divergence from System V, stated so nobody "fixes" it later.** Real SysV puts
the sret pointer in `rdi` and shifts the real arguments. We put it in `rsi` and leave `rdi`
alone, because moving `rdi` would change the shipped input ABI and break byte-identity for
every existing recursive binary. Verbose's ABI is private to Verbose (the same reason gen0's
bytes are only ever compared to gen1's, not to verbosec's), so the divergence costs nothing.

`rsi` is caller-saved and every syscall and `emit_strlen` clobbers it, so the callee **must
spill it at prologue** — exactly as `rdi` is spilled to `[rbp - 8]` today
(`src/native.rs:4805`).

### 3.3 Caller sequence — **the `lea` goes LAST, and that is not cosmetic**

```asm
  sub  rsp, in_frame_bytes       ; input fields-struct — unchanged from src/native.rs:4486-4493
  <store each input field at [rsp + s_off]>     ; src/native.rs:4494-4515
  mov  rdi, rsp
  lea  rsi, [rbp - dest_off]     ; destination LAST — see below. rbp is stable, so this is safe here
  call <callee_label>            ; rel32, two-pass patched (src/native.rs:4411-4435)
  add  rsp, in_frame_bytes
  ; the N result words are now in the let's slots; `p.<f_i>` is an ordinary slot read
```

**The previous draft emitted `lea rsi` FIRST, before building the input struct. That is a
bug, and it would have produced a wrong binary rather than a refusal.** The shipped
marshalling clobbers `rsi` while filling the struct: for every text field it does
`mov rsi, rax` and then `emit_strlen` (`src/native.rs:4502-4504`, and the same pair inside
`emit_copy_rbp_to_rsp`'s callers), and any number field whose expression reaches
`emit_itoa_inline` does `lea rsi, [rsp+22]` (`src/native.rs:17131`). A destination pointer
parked in `rsi` before that loop is gone by the time `call` executes, and the callee would
store the record through a string length or an itoa scratch pointer.

Emitting the `lea` immediately before the `call` is safe **because `rbp` is stable across
the marshalling** — nothing between the caller's prologue and the `call` touches `rbp`, which
is the same invariant the slice-3d iteration epilogue already relies on when it frees buffers
with `lea rsp, [rbp - frame_size]`. The destination address is therefore recomputable at any
point from a compile-time constant, and the last possible moment is the correct one.

### 3.4 Callee sequence

```asm
  push rbp
  mov  rbp, rsp
  sub  rsp, frame_bytes          ; frame_bytes grew by 8 — see below
  mov  [rbp - sret_slot], rsi    ; spill the destination
  <copy input fields from [rdi + k] to slots — unchanged, src/native.rs:4805-4838>
  <body>
  ; at every record leaf, per field, in DECLARATION order:
  <eval field expr>              ; emit_eval_expr leaves the value in RAX
  mov  rcx, [rbp - sret_slot]    ; reload the destination (rax is the value)
  mov  [rcx + offset_C(i)], rax
  jmp  .ret                      ; forward-patched; all leaves converge
.ret:
  mov  rax, [rbp - sret_slot]    ; return the destination pointer
  mov  rsp, rbp
  pop  rbp
  ret
```

**Two corrections to the previous draft, both of which would have shipped a wrong binary.**

**(1) The value lands in `rax`, not `rdx`.** The previous draft wrote
`<eval field expr -> rdx> ; mov [rax + offset], rdx`. No emitter in this file does that:
`emit_eval_expr` leaves its result in **`rax`**, universally — `emit_record_as_json`'s Number
arm is `emit_eval_expr(...)` immediately followed by `emit_itoa_to_stdout_no_newline`
(`src/native.rs:12932-12933`), and the `Expr::Field` arm ends in `load_rax_from_rbp`
(`src/native.rs:13771`). As written, the store would have written whatever the *previous*
field expression happened to leave in `rdx`. The sketch above reloads the destination into
`rcx` per field instead of parking it. Parking it in a callee-saved register is the obvious
alternative and is **refused here**: `rbx` and `r15` are exactly the two registers the A2/A4
fast path claims (`src/native.rs:4786`, `:4793`), and `r11` is the arena base
(`src/native.rs:4870-4877`). A 4-byte reload per field is the cheap, collision-free answer;
`rcx` is ephemeral by the register table in CLAUDE.md and there is a precedent helper
(`emit_eval_simple_into_rcx`, `src/native.rs:12677`) for using it this way.

**(2) The A2/A4 argument was inverted, and this is the dangerous one.** The previous draft
said a record output "fails `qualifies_rbx` because that predicate requires `scalar_output`".
Read the predicate:

```rust
let is_text  = rule.output_ty == Type::Text;    // src/native.rs:4651
let is_bytes = rule.output_ty == Type::Bytes;   // :4652
let scalar_output = !is_text && !is_bytes;      // :4767
```

For `Type::Named(C)` both are false, so **`scalar_output` is `true`** — a record output
*satisfies* that conjunct. And `body_is_pure_scalar_arith` has an explicit `Expr::Record` arm
(`src/native.rs:4631`) that returns true when every field expression is pure-scalar. So a
callable with a **single Number input field, no let bindings, no concept_group, and a
`Record { … }` body of arithmetic** satisfies every conjunct of `qualifies_rbx`
(`src/native.rs:4768-4772`), takes the A2 prologue at `src/native.rs:4782-4795` — which is
`push rbp ; mov rbp, rsp ; push rbx` and emits **no `sub rsp`** — and a sret slot at
`[rbp - 8]` would land **exactly on the pushed `rbx`**. The caller then reads its own A2
parameter out of a slot the callee overwrote with a pointer.

Slice 1's fixture (`swap2`, a 2-field input) does **not** hit this — `is_multi_field` is true
(`src/native.rs:4664`) so `single_scalar_field` fails. The shape that does hit it is
`N { v }`, the single-Number-field helper that is the most common shape in the crypto arc.

**The guard.** `qualifies_rbx` must gain `&& !is_record_output`, i.e. the A2/A4 fast path is
switched off for a record-returning callable, which puts it back on the ordinary slot
prologue where the sret slot has a frame to live in. That is the *conservative* direction (a
missed optimisation, never a wrong store), it costs only the shapes that would newly qualify,
and it must be pinned: see §6.4's negative control 2. Composing A2 with sret — keeping the
parameter in `rbx` *and* finding a home for the destination without a `sub rsp` — is a real
register-allocation question and belongs to agg-2, not to slice 1.

**Frame accounting, and where the slot goes.** `frame_bytes = total_slots * 8 +
resource_extra_bytes` (`src/native.rs:4741`) and resource buffers descend from
`-((total_slots + 1) * 8)` (`src/native.rs:4743`). Bumping `total_slots` by one is necessary
but **not sufficient**, because the frame is not a flat array:

```rust
let tmp_slot_base_local = -((nfields + n_let_slots + max_arm_binder_slots + 1) * 8);  // :4712
let total_slots         =    nfields + n_let_slots + max_arm_binder_slots + tmp_slots; // :4713
```

`tmp_slot_base_local` is computed **without** `tmp_slots`, so it anchors the VariantConstruct
tmp pool at a fixed depth. Inserting the sret slot anywhere *above* that anchor shifts the
arena tmp base and silently re-points every VariantConstruct spill. **The sret slot must go
strictly below the tmp pool**, i.e. `total_slots` grows by one and the slot sits at
`-((nfields + n_let_slots + max_arm_binder_slots + tmp_slots + 1) * 8)`, with the resource
descent starting one slot lower still. `f26fe3c` (`HEAD~3`, PR #175) was literally a
slot-aliasing bug in this exact frame — *"reserve the MatchVariant arm-binder pool instead of
aliasing live slots"* — so this is a known-live hazard, not a hypothetical. Cost: **+8 B of
frame, and only for record-returning callables**.

### 3.5 The width question, answered

At ≤ 8 / 16 / 24 / 32 bytes and above 32, **under this ABI there are no thresholds** — that
is the point, and it is the property Option A cannot have. One convention covers a 2-field
`Pair`, a 32-field digest, a 20-limb ladder result, and a 112-field machine state, and it
never needs a companion.

The width itself is `sum over concept.fields of (16 if Text else 8)`: a **declaration**, read
from the AST, satisfying *"if the width cannot be determined from declarations, the design is
wrong."* Nothing is inferred and nothing is guessed.

### 3.6 Effect-model checklist

Per `docs/effect-model.md`'s eight questions. Six are `n/a`: **this is not an effect.** No
syscall is added, no file or socket is touched, nothing appears in `strings`, there is no
error policy and no `on_*_error` knob. The two that do apply:

- **Memory bound:** the destination is `sum(field widths)` bytes of the caller's stack frame,
  a compile-time constant from the concept declaration. No heap, no growable buffer, and the
  *whole* frame is still `sub rsp, <constant>`, which is the invariant `effect-model.md`'s
  "Memory bounds" section states.
- **Allowed contexts:** exactly one in slice 1 — the RHS of a `let` in a rule compiled by
  `emit_callable_into` on the callable path. Every other position is refused (§6.2).

---

## 4. Declaration surface

### 4.1 What the author writes

**Nothing new.** This is the strongest property of the recommendation and it is measured, not
asserted: the program in §1.4 parses and verifies on `47d7b8d` exactly as printed.

For a crypto consumer the shape would be — **note that this sketch is aspirational, and §6.3
states the rewrite it would cost**:

```verbose
concept Prk
  fields:
    b0 : number [0, 255]
    ...
    b31 : number [0, 255]

rule hkdf_extract
  input:  g : ExtractInput
  output: p : Prk                      -- an ordinary concept, no new type
  logic:
    ...
    p = Prk { b0: e_out_ob0, ..., b31: e_out_ob31 }

rule derive_key
  input:  s : Seed
  output: out : number
  logic:
    let prk = hkdf_extract(ExtractInput { ... })      -- aggregate crosses here
    out = expand(ExpandInput { prk0: prk.b0, ..., prk63: prk.b63, ... })
  proofs:
    purity: { reads: [s], calls: [hkdf_extract, expand] }
```

Two things about this sketch, both of which the previous draft got wrong:

- `ExpandInput` is **`prk0..prk63`**, not `prk0..prk31` — the PRK is block-padded to 64 bytes
  before it becomes the HMAC key (`examples/hkdf.verbose:99-162`, `which` at `:173`).
- **`expand` here is still scalar-output, so this sketch does NOT eliminate expand's 42
  spawns.** §6.3 used to claim 74 → 1 while §4.1 kept `out : number`; the two contradicted
  each other. Getting to one process needs *both* rules to return records and a third rule to
  compose them, which is the rewrite §6.3 now prices.

The `which : number` field and the 32-arm dispatch chain **disappear from the source** of any
rule that adopts the record output. That is a readability and auditability win on top of any
CPU win: `examples/p256_fmul.verbose` loses 32 lines of pure selection noise, and the rule
stops lying about what it computes.

### 4.2 What the verifier checks — updated for PR #178

The previous draft said *"the verifier needs no new rule for slice 1."* **That was false when
written, and the missing rule landed in PR #178 (`47d7b8d`, this note's `main`)** — read that
commit before touching this section.

Already enforced today:

| Check | Where |
|---|---|
| the record constructor's field set matches the concept exactly | `src/verifier.rs`, `check_expr_against` / `Expr::Record` arm |
| each field expression typechecks against its declared field type | same |
| the rule's declared `output:` type matches the body's shape | `check_expr_against(&rule.logic.value, &rule.output_ty, …)`, `src/verifier.rs:1936-1944` |
| **`.field` on a record-typed `let` exists on that concept** | **new in PR #178** — `concept_field_error`, driven from `LogicFacts::local_reads` |
| **`.field` on a record-typed `let` has the declared type** | **new in PR #178** — `infer_expr_type`'s `Expr::Field` arm now consults a `bindings: &HashMap<String, &Concept>` map threaded through `check_expr_against` (42 sites) |
| the same two checks on the `context:` binding | **new in PR #178** |
| `purity.reads` / `purity.calls` equal the performed set, both directions | `check_purity` |
| exactly one argument per rule call | `check_call_arity`, added 2026-08-13 |

**The residual, and it is load-bearing for slice 1.** PR #178 removes "field of nothing" and
"field of the wrong concept". It does **not** remove the *legitimate collision*: if the
returned concept `Q` has a field `a` and the caller's input concept `P` also has a field `a`,
**both accesses are valid** and the verifier is right to accept both. Native field lookup is
still keyed by **bare field name**:

```rust
let lookup_key = match base.as_ref() {
    Expr::Ident(base_name) if base_name == "state" => format!("__state_{}", field_name),
    _ => field_name.clone(),
};                                                          // src/native.rs:13748-13753
let offset = *offsets.get(lookup_key.as_str()).ok_or_else(|| NativeError {
    message: format!("unknown field '{}' in native codegen", field_name),
})?;                                                        // :13768-13770
```
— and the same shape in the `rcx` helper at `src/native.rs:12691-12707`. The one composite-key
precedent is `__state_<field>` (`src/native.rs:13746-13747`), whose own comment says it exists
"to avoid collisions with input fields". **So the moment a record `let` registers its fields in
`offsets`, `p.a` resolves to the INPUT's `a` slot and prints a plausible number at rc 0** —
the silent-wrong-answer class this repo spent eight gen0 slices closing. PR #178's commit body
says exactly this, and names it as the reason the verifier fix could not wait for the emitter.

**Therefore composite keying `__<let>_<field>` (or an equivalent scoped map) is REQUIRED in
slice 1, and needs its own acceptance test** — a fixture where the returned concept and the
input concept deliberately share a field name, asserting the value comes from the returned
record. *(The previous draft cited `src/native.rs:12700-12708` for the field read; that region
emits `mov rcx`, not `mov rax` — the rax read is `:13734-13772`. That mis-cite is precisely
what made the collision look like a non-issue.)*

**One thing the verifier still does not catch, measured in §1.4:** a record-typed binding in a
bare arithmetic position (`out = p * 1000`) verifies clean and fails only at interpretation.
Slice 1's refusal #5 must stand on its own in `src/native.rs`.

One thing to *watch* rather than change: `check_call_arity` was added after the discovery that
*"the verifier blessed a program its own emitter refused"* (CLAUDE.md). This slice deliberately
moves in the opposite direction — it makes the emitter accept more of what the verifier already
blesses, shrinking that gap rather than widening it.

---

## 5. Extraction — the honest answer

### 5.1 What a caller can do with an aggregate, in slice 1

**Read a field: `p.<name>`.** That is all, and it is enough. `p.b0` compiles to a single
`mov rax, [rbp - k]` — the same instruction as reading an input field
(`src/native.rs:13734-13772`), because the destination slot group has the same layout as the
input slot group. There is no address in the language, no index, no bounds check needed.
**Subject to the collision fix in §4.2**: the lookup key must distinguish `p.a` from the
input's `a`.

One derived use falls out for free and is in slice 1:

- **Feed the whole aggregate to the next rule**, one field at a time inside a record
  constructor: `expand(ExpandInput { prk0: p.b0, … })`. Each `p.b0` is an ordinary `Field`
  expression that the existing call-site marshaller evaluates into a struct slot
  (`src/native.rs:14338-14392`). **This is the TLS composition**, and it needs no machinery
  beyond §3 plus the collision fix.

**The second derived use is NOT free, and the previous draft was wrong to list it as such.**
Serialising with `concat(le64(p.w0), le64(p.w1), …)` requires the *caller's* output to be
`Type::Bytes`, which routes the whole SCC through `decide_streaming_bytes_mode`
(`src/native.rs:542-548`, predicate at `:2011`) and `emit_streaming_bytes_body`
(`src/native.rs:6318`, `Expr::Concat` arm at `:6334-6344`). That is a **whole-SCC streaming
ABI** with `push r11` / `pop r11` around every write (`emit_streamed_write_rsi_rdx`,
`src/native.rs:5736-5742`; also `:6145`, `:6234`), and whether that emitter can host a record destination slot group is entirely
unaddressed by this note. Either give it its own acceptance test or **move it out of slice 1**
— the recommendation here is to move it out (§7), because §6.1's caller-output refusal already
excludes it.

The primitives themselves do exist: `Expr::Le32` / `Expr::Le64` (`src/ast.rs:692-700`) and
bytes `concat` (`src/verifier.rs:2349-2367`). Endianness is the producer's business: `le64`
emits low byte first, so a producer that packs byte 0 into bits 0–7 gets little-endian for free.
That is a generator convention, not a compiler feature.

### 5.2 `byte_at(<aggregate>, i)` with a runtime index — **does not arise, and should stay out**

This is the question Option A would have forced. Under the recommendation it is **moot for
slice 1**, because the consumers index at compile-time positions: the generators emit
`g.k0 … g.k15` literally against the declarations at `examples/aes_gcm.verbose:13-28`, and
`p256_fmul`'s 32 output bytes are 32 named lets. No crypto consumer needs a runtime byte index
across a rule boundary.

When it *is* asked for, here is the ruling this note recommends in advance:

- **Over a frame slot group — acceptable in principle.** The width is a compile-time constant
  from the concept declaration, so a bounds check is *against a fact, not an assertion* —
  which is the exact standard CLAUDE.md sets, and the same standard that makes
  `byte_at(b"...", i)` sound today (`check_byte_addressable_operand`,
  `src/verifier.rs:1977-1999`). It is `byte_at` generalised.
- **Over a register quad — refuse.** The only implementations are a jump table or a
  spill-to-stack-then-index. The spill manufactures an address whose width the *emitter*
  inferred, in order to bounds-check a read the *declaration* did not describe. That is the
  wrong side of the line, and it is the concrete reason Option A's extraction story is worse
  than Option B's even though Option A's *value* story is better.
- **Either way, still no indexed store.** Reading `p.b[i]` is a generalised `byte_at`; writing
  `p.b[i] = v` is C, and stays refused.

### 5.3 The rest of the interaction surface, settled

| Question | Answer for slice 1 |
|---|---|
| **`let` bindings** | The only allowed position. A record-typed `let` is N consecutive rbp slots — the same widening the prologue already does for text lets, which take 2 (Phase 2I; `callable_binding_is_text`, `src/native.rs:4692-4704`). Field `i` is at `let_base - 8*i`. |
| **`if`/`else` arms** | Both arms must produce the same concept — **already enforced by the verifier**, which typechecks each arm against the declared output. In the emitter both arms write the same offsets and converge on the common `.ret`, mirroring `emit_eval_record_expr`'s If arm (`src/native.rs:12772-12793`). Refused in slice 1 only at the *call site* (`let p = if c then f(x) else g(x)` — slice 2). |
| **`match` arms** | Refused in slice 1 (§6.2). A sum-type value already has a return convention — the arena index — and two conventions for one shape is exactly the drift this note is trying to avoid. |
| **The declared bound** | The width is **exact**, from `concept.fields`. `[..N]` on a `bytes` field is a *maximum* and is why `bytes` is the wrong carrier (§2.1e). No length register exists, because no variable-length aggregate exists. |
| **Variable-length results** | Not expressible, deliberately. A rule that wants to return "up to N bytes" returns a fixed-width record plus a `len : number` field, and the consumer reads both. The length is then a declared field the verifier can see, not an ABI side-channel. |

---

## 6. Slice 1 — "agg-1: record return on the callable path"

### 6.1 Scope

A rule **R** whose `output:` is `Type::Named(C)`, where `C` is a plain record concept
(`C.variants.is_empty()`) with ≥1 field and **all fields `Type::Number`**, may be **called** by
another rule **S**, provided:

1. **R is not recursive** — R is not in a cycle in the native call graph
   (`detect_native_recursion`, called at `src/native.rs:271`, defined at `:1579`).
2. **R's own body does not call a record-returning rule** — one aggregate hop per slice.
3. **The call is the entire RHS of a `let`** in S: `let p = R(<arg>)`.
4. **`p` is used only as `p.<field>`.**
5. **S's own output is `Type::Number` or `Type::Bool`.**

**`Type::Number` only, not "number or bool", and the previous draft over-scoped this.** The
serializer slice 1 rewrites, `emit_record_as_json` (`src/native.rs:12898-12968`), handles
`Type::Number` and `Type::Text` and nothing else — `Type::Bool` falls into a catch-all
`native record field '{}' has unsupported type` at `src/native.rs:12955-12962`. **Bool record
fields do not emit today, in any backend path.** Slice 1 can either add the arm (it is a
`test/setcc`-shaped few bytes) or keep the refusal; this note recommends keeping the refusal
and saying so, because widening the serializer is orthogonal work that would move existing
record-output binaries' bytes.

**Constraint 5 is new, and it is not optional.** The previous draft constrained the callee's
output, the call's position and the binding's use, and said nothing about the *caller's*
output. Two consequences follow:

- **Routing.** The non-recursive route into `needs_callable_path` is gated on `entry_scalar`
  (`src/native.rs:281-282`), which is
  `matches!(&rule.input_ty, Type::Named(_)) && matches!(&rule.output_ty, Type::Number | Type::Bool | Type::Text)`.
  A record-output caller therefore **does not route to the callable path at all**, so slice 1
  has to widen that predicate — which is also what makes the next point unavoidable.
- **Printing.** `emit_self_recursive_program`'s result printing (`src/native.rs:4531-4568`)
  branches on `is_bool` / `is_bytes` / `is_text` and ends in `else emit_itoa_inline`
  (`:4566-4567`). **A record-returning ENTRY would itoa the destination pointer** — the
  itoa-a-pointer family CLAUDE.md spends five gen0 slices closing, reproduced verbatim in
  verbosec. Slice 1 must therefore either add a record arm to that dispatch or refuse a
  record-output entry. **This note recommends the refusal** (§6.2, refusal #7): a record entry
  already has a correct emitter (`emit_record_program` → JSON), and giving `_start` a second
  way to render a record is how two conventions for one shape start.

Emit changes, all in `src/native.rs`:

- **Widen `needs_callable_path` (`:275-300`)**, not `scc_rules_owned`. The previous draft said
  "add non-recursive record-returning callees to `scc_rules_owned`"; that is already done —
  `collect_transitive_recursive_callees` (`:2054`) is misnamed and has been unconditional since
  PR #56 (CLAUDE.md, "transitive callable extension unconditional"), already pulling in
  let-bearing and cross-concept callees (`:325-351`). What blocks a record-returning callee is
  the `entry_scalar` gate above.
- Add the sret slot + spill to `emit_callable_into`'s prologue and the
  `mov rax, [rbp - sret_slot]` to its epilogue, both gated on record output, with the slot
  placed strictly below the tmp pool (§3.4).
- Add `&& !is_record_output` to `qualifies_rbx` (`:4768-4772`) — §3.4 correction (2).
- New `emit_record_to_sret`. **This is a bigger rewrite than "the JSON writes replaced", and
  the previous draft under-scoped it.** `emit_record_as_json` (`:12898-12968`) (a) interleaves
  `emit_write_static_to_fd` **syscalls** between per-field values, each clobbering
  `rax`/`rcx`/`r11`, so the write sequence is not merely removable — the ordering assumptions
  around it go with it; (b) supports Number and Text only, with the catch-all refusal above;
  and (c) hardcodes `emit_eval_expr(…, None, None)` at `:12932` — `self_call: None`,
  `arena_ctx: None` — so a record body containing a **call** or a **variant construct** cannot
  work until `SelfCallCtx` is threaded through it. CLAUDE.md records the identical
  `arena_ctx: None` defect for Phase B slice 4c ("Call inline path in `emit_eval_expr` was
  dropping `arena_ctx`"), so this is a known-shape bug waiting to be re-introduced.
- **Caller side is `emit_callable_into`, not `emit_record_loop_prologue`.** The previous draft
  named the latter and contradicted its own §3.6. For the only shape slice 1 permits, the
  caller is a callable: `emit_self_recursive_program` **explicitly clears the entry rule's let
  bindings** before running the `_start` prologue (`src/native.rs:4450-4456` — *"The entry
  rule's let bindings are handled by the callable's own prologue"*). The real work is
  `emit_callable_into`'s `n_let_slots` (`:4702-4704`), its let-eval loop (`:4930-4933` onward), and
  the `callable_offsets` map (`:4842-4866`) that `Field(Ident(let), f)` must resolve against —
  with the composite key from §4.2.

### 6.2 Refusals, with breadcrumb text

Every message names the offender and the slice that lifts it, per the house rule that a
breadcrumb must name the offending identifier (`native_cross_concept_match_rejects_missing_field`,
`src/native.rs:28673`).

| # | Shape | Breadcrumb |
|---|---|---|
| 1 | **any** record-returning rule on the callable path that is in a cycle, **or is any rule in `scc_rules_owned` while the entry is recursive** | `aggregate return: rule 'R' returns record 'C' and is on the callable path of a recursive program (cycle through 'X'); a recursive aggregate return needs a per-frame destination (slice agg-2). Use --run.` — replaces `src/native.rs:469-476` for the record case. **Note the scope**: `:365-368` makes the existing message fire for every `scc_rules_owned` member once the entry is recursive, so slice 1 must lift it for a *non*-recursive record-returning callee inside a recursive program too. |
| 2 | non-Number field in the returned concept | `aggregate return: concept 'C' field 'f' has type Text; slice agg-1 returns number fields only — text fields need the (ptr, len) pair convention (slice agg-3), and bool record fields do not emit in any path today (emit_record_as_json, native.rs:12955).` |
| 3 | sum-type concept as the return | `aggregate return: 'C' is a concept_group concept; a group value already returns as an arena index in rax — do not route it through the destination convention.` |
| 4 | aggregate call not directly bound to a `let` | `aggregate return: the call to record-returning rule 'R' must be the entire right-hand side of a let-binding in slice agg-1 (found it in an if-condition); nested aggregate calls are slice agg-2.` |
| 5 | record-typed binding used other than `p.<field>` | `aggregate return: binding 'p' of record type 'C' may only be read with '.field' in slice agg-1; passing the whole record as a call argument is slice agg-2.` **Must be enforced here regardless of the verifier** — `out = p * 1000` verifies clean today (§1.4). |
| 6 | `Result(Named(C), E)` output | unchanged — `Result` return stays refused (`src/native.rs:469-476`) |
| 7 | **the CALLER's output is not Number/Bool** | `aggregate return: rule 'S' calls record-returning rule 'R', but S's own output is 'Named(D)' / 'Text' / 'Bytes'; slice agg-1 requires a number or bool caller, because _start's result dispatch (native.rs:4566) would itoa the destination pointer. A record-output rule is already served by emit_record_program.` **New — closes the itoa-a-pointer hole.** |
| 8 | **the CALLEE's own body calls a record-returning rule** | `aggregate return: record-returning rule 'R' itself calls record-returning rule 'R2'; slice agg-1 allows one aggregate hop, because a second destination needs a nested slot group with expression-scoped lifetime (slice agg-2).` **New — refusal #4 only constrains the caller's position, not the callee's body.** |
| 9 | **field-name collision without composite keying** | Either implement `__<let>_<field>` keying (recommended, §4.2) **or** refuse: `aggregate return: field 'a' of returned concept 'C' collides with input concept 'P' field 'a'; native field lookup is keyed by bare name (native.rs:13748), so the read would silently resolve to the input slot. Rename, or wait for slice agg-1's composite keying.` **New — a silent-wrong-answer hole, not a missing feature.** |
| 10 | the legacy messages | `src/native.rs:14706`, `:13769`/`:12706` and `:469-476` must no longer be reachable for a record-returning callee; each is replaced by one of the above. |

WASM: refuse with `wasm: aggregate (record) return is not supported; slice agg-1 is native-only. Use --run or --native.` — replacing today's generic `unsupported expression in WASM backend`, so the asymmetry is deliberate and legible. Interpreter: **unchanged, already correct** (§1.4).

**Noted pre-existing defect, NOT slice-1 scope.** The Rust transpiler folds `Record` and
`Field` to the literal `false` and then fails inside `rustc` with `E0308`/`E0610` (§1.4).
That is the same pattern CLAUDE.md already records for `ByteAt`/`Length`. A compiler that
emits Rust that does not compile is worse than one that refuses; making `codegen.rs` refuse
cleanly is a separate ~10-line slice and should be taken, but not here.

### 6.3 Worked example, and the composition that is NOT in slice 1

**`examples/aggregate_pair.verbose`** (+ `.intent`, + `.json`) — **slice 1's proof, and its
only flagship.** The §1.4 program, verbatim: two number fields, one `let`, two `.field` reads.
Measured today at `47d7b8d`: `--run total` gives `9007`, `--native --run swap2` gives 821 B
printing `{"x":9,"y":7}`, `--native --run total` refuses with rc 1 and zero bytes. Small enough
to `objdump` by hand and confirm the `mov rdi, rsp` / `lea rsi` / `call` / slot-read sequence.
A second fixture must add the §4.2 field-name collision (returned concept and input concept
sharing a field name).

**HKDF Extract → Expand is a FOLLOW-UP, not slice 1's flagship. The previous draft claimed it
"needs no other change"; that is wrong, and here is the budget.** Both rules in
`examples/hkdf.verbose` declare **`output: out : number`** — `hkdf_extract` at `:181-182`,
`hkdf_expand` at `:1749-1750` — not a record, which slice 1 requires. Making the chain compile
as one process means:

1. deleting `hkdf_extract`'s 32-arm dispatch chain and `hkdf_expand`'s 42-arm chain;
2. adding two record concepts (`Prk` with 32 fields, `Okm` with 42);
3. removing `which` from `ExtractInput` (87 fields) and `ExpandInput` (75 fields);
4. fixing both `reads:` lists, which are single lines enumerating all 87 and all 75 paths;
5. adding a third composing rule.

…across a **4 839-line file**, and **with no generator to regenerate it**: nothing under
`tools/` references `hkdf.verbose`, and neither `ExtractInput` nor `ExpandInput` appears
anywhere in `tools/`. *(There is a `tools/tls_gen/extract_gen.py`, but it generates a
**different** file — `examples/hkdf_extract.verbose`, concept `Extract`, an unpadded 32-byte
salt — which is what `tls_browser_server.py:97-98` actually calls at 32 spawns. Do not mistake
one for the other.)* So the HKDF rewrite is hand work on a generated-looking file, and it
should be scheduled as its own slice with that stated.

When it lands, the oracle is authoritative — RFC 5869 Test Case 1 publishes both the PRK and
the OKM, and `hkdf_matches_rfc5869` (`src/native.rs:42286`) already checks both — and the
measured prize is §1.3's: **19.6 ms of 74 spawns → an estimated ~0.34 ms in one process**. It
is also the first brick of Gap C that is *load-bearing for TLS rather than illustrative of it*:
`docs/tls13-roadmap.md` §7 Milestone 1 step 2 is exactly "HKDF-Expand-Label + Derive-Secret +
transcript hash, validated against RFC 8448".

### 6.4 Acceptance tests

House style per `docs/self-hosting-service-slice5c-design.md:79-98`; helper names from
`src/native.rs`.

**Byte-identity (structural).** Slice 1 is additive **by construction, and this was verified
rather than assumed.** Sweeping all 151 files in `examples/` at `47d7b8d`: exactly **four**
rules declare a plain-record concept as their output — `classify.verbose::classify_invoice`,
`fullname.verbose::compose_greeting`, `greeting.verbose::make_report`,
`raw_tcp_echo.verbose::echo_handler` — and **none of the four is named in any `calls:` list**,
so no existing program has a record-returning callee at all. *(A naive sweep reports three
extra hits in `vexprparse.verbose`; `Block` there is a **sum type** inside `concept_group
VExpr` (`examples/vexprparse.verbose:558-562`), and collides by name with the plain-record
`Block` in `aes_transforms.verbose:9`. Scope the concept table per file.)* Four synthetic host
shapes were also probed and all four refuse today; `swap2(i).x` does not even parse
(`parse error: expected DEDENT, got '.'`). The pins that must confirm additivity:

1. `sha256(--native --run swap2)` **unchanged at 821 B** — a record-returning rule compiled as
   the *entry* still streams JSON and is untouched.
2. The `(size, sha256)` pin tables in `self_hosted_service_log_field_content`
   (`src/native.rs:25435`) and `self_hosted_service_concurrency_forked`
   (`src/native.rs:26296`) — **all rows unchanged**.
3. Baseline-vs-baseline corpus sweep comes back **empty** first (CLAUDE.md, "The emitter must
   be reproducible"); then patched-vs-baseline must show **0 changed**, and in particular **no
   binary changing bytes while keeping its size**. Use the denominator from `47d7b8d`'s own
   commit body — **1340 native rule-binaries (782 emit, 558 refused natively)** — and say so.
   *(CLAUDE.md carries two older figures: 749 at `:889` and 1290 / 768-emitting at `:962`. They
   are snapshots from earlier commits, not alternatives; quote the one you measured.)*
4. `scc_callable_order_is_deterministic_across_compiles` (`src/native.rs:36399`) still green —
   this slice adds a call site whose per-field emit order comes from a `Vec`, and that test is
   the standing guard.

**Milestone (the assertions the old code cannot pass).**

5. `aggregate_return_composes_in_one_process`: compile `examples/aggregate_pair.verbose`
   `--native --run total`, run with argv `7 9`, assert stdout `9007` — **and** assert it equals
   `--run total --input aggregate_pair.json`.
   *Verified to FAIL pre-change: rc 1, `native codegen error: rich operations …`, zero bytes.*
6. `aggregate_return_distinguishes_colliding_field_names`: the §4.2 fixture. Returned concept
   `Q { a, b }`, input concept `P { a, b }` with **different values**; assert `q.a` is the
   returned record's `a`, not the input's. *This one has no pre-change failure mode to show,
   because the shape does not compile today — which is exactly why it must exist before the
   feature does.*
7. `hkdf_chain_matches_rfc5869_tc1` — **follow-up slice, not slice 1** (§6.3). One binary, one
   invocation, OKM byte-exact against RFC 5869 TC-1. Big-stack thread
   (`stack_size(64 * 1024 * 1024)`, as `hkdf_matches_rfc5869` already does at
   `src/native.rs:42292-42293`) — the body is a ~5 000-line let-chain.

**Refusals, each with a corrected twin** so the refusal is attributable
(`examples/negative/README.md`, and the `gen0_accepts` pattern at `src/native.rs:49122`):

8. One `.expect_err` test per row of §6.2, asserting `msg.contains(<offender name>)` **and**
   `msg.contains(<slice name>)` as separate asserts, each paired with a minimally-corrected
   program that must **compile and run correctly**. Rows 7, 8 and 9 are new and have no
   existing coverage anywhere.
9. Refusal #10's twin must specifically prove `unknown field 'x' in native codegen`
   (`src/native.rs:13769`) is no longer reachable for this shape.

**gen0 — record the verdict AT THE DECLARED ENTRY, not at rule #0.**

10. Measured at `47d7b8d` with a gen0 built by
    `verbosec examples/vexprparse.verbose --native … --run elf_program_src --stdin-raw`
    (657 831 B), fed the §1.4 fixture:

    | entry | gen0 | verbosec |
    |---|---|---|
    | index 0 (`swap2`) | rc 0, 2 063 B, prints `{"x":9,"y":7}` — **agrees** | rc 0, 821 B, same output |
    | index 1 (`total`) | **rc 0, an 825-byte ELF that exits 133 (SIGTRAP) with no output** | rc 1, **zero bytes** |

    *(gen0's byte counts embed the source blob, so they move with the fixture's comments and
    `@intention` strings; the behaviour is the stable part.)*

    **This is NOT an `INVERSE_CAPABILITY`** — that bucket requires gen0 to *refuse* a valid
    program verbosec compiles correctly. Here gen0 **accepts** and emits a binary that traps:
    the accept-what-you-cannot-emit class of `iso_date` and `aes_sbox`. And because rule #0 is
    `swap2`, adding this fixture would take `EXPECTED_ACCEPTED` from **93 → 94 GREEN**
    (`src/native.rs:48197`) while the file's *subject* traps — the exact metric trap CLAUDE.md
    documents at length ("THE CORPUS FIGURE HAS BEEN MEASURING THE WRONG PROGRAM FOR 37 OF ITS
    100 FILES"). So: assert gen0's verdict at the **declared entry**, and if the count moves,
    argue the direction in the commit body per the scoreboard protocol at
    `src/native.rs:48245-48263`.

11. `two_generation` suite green: R0/R1/R2 (`src/native.rs:47777`) and the negative-corpus
    sweep (`:48494`) with `KNOWN_GAPS` / `INVERSE_REFERENCE_GAP` / `INVERSE_CAPABILITY`
    **sets** unchanged unless test 10's finding is filed — and if it is filed, it is a gap
    row, not an inverse row.

**Standing gates.**

12. `all_examples_with_json_run_without_panicking` (`src/verifier.rs:5988`) and
    `all_example_verbose_files_parse_and_verify` (`src/verifier.rs:6097`) green; adding
    `examples/aggregate_pair.*` bumps `EXPECTED_TOTAL` from 151 (`src/native.rs:48198`) —
    **in the same commit**, with the direction argued in the body.
13. Registration: an `examples/README.md` row, a CLAUDE.md native-emitter table row (slice /
    mechanism / size delta / worked example), and a `tools/phase_sizes.sh` entry.

**NEGATIVE CONTROLS (all three required).** A convention whose load-bearing parts were never
shown to be load-bearing is not verified.

- **NC-1 — the destination spill.** Drop `mov [rbp - sret_slot], rsi` and confirm test 5
  FAILS.
- **NC-2 — the A2/A4 guard (§3.4 correction 2).** Remove `&& !is_record_output` from
  `qualifies_rbx` and confirm a **single-Number-field, no-lets, `Record { … }`-of-arithmetic**
  callable now produces a wrong answer or a crash. This is the control that would have caught
  the previous draft's inverted claim, and it must use that exact shape — `swap2`'s two-field
  input does not qualify (`is_multi_field`), so a test written against the flagship fixture
  would pass against the broken build.
- **NC-3 — the collision key (§4.2).** Revert `__<let>_<field>` to the bare field name and
  confirm test 6 FAILS **with a plausible wrong number at rc 0**, not with an error. The whole
  point of that fixture is that the failure mode is silent.

Then run tests 5 and 6 against the pre-change binary and confirm both report the wrong result
or refuse.

---

## 7. What slice 1 deliberately does not do

| Not in slice 1 | Why | Slice |
|---|---|---|
| **Recursive aggregate return** | needs a per-frame destination for non-tail calls (`sub rsp, N*8 ; lea rsi, [rsp]` — the input-struct pattern mirrored). Unblocks `ladder → x25519_finish`, i.e. **X25519 in one process** and 52 → 1 spawns. The single highest-value follow-up, and the one that owns X25519's numbers (§1.3). | agg-2 |
| **A2/A4 composed with sret** | keeping the parameter in `rbx` while finding a destination home without a `sub rsp` is a register-allocation question, not an ABI one (§3.4) | agg-2 |
| **Aggregate in a nested position** (`f(g(x))`, an `if` arm, a call argument) | needs a temporary destination with expression-scoped lifetime | agg-2 |
| **A record-returning callee that itself calls a record-returning rule** | second destination, nested lifetime — refusal #8 | agg-2 |
| **Passing a whole record as a call argument** (`f(p)` rather than `f(C { a: p.a, … })`) | a struct-to-struct copy; mechanically easy, but a distinct capability deserving its own refusal-lift | agg-2 |
| **A record-output CALLER** | `_start`'s result dispatch would itoa the destination pointer (§6.1, refusal #7); needs a record arm in `emit_self_recursive_program`'s printing, or the entry to stay on `emit_record_program` | agg-2 |
| **`concat(le64(p.w0), …)` in a bytes-output caller** | routes the whole SCC through `emit_streaming_bytes_body`'s streaming ABI with its `push r11` discipline; interaction with a destination slot group is unanalysed (§5.1) | agg-2 |
| **`Type::Bool` and `Type::Text` record fields** | Bool does not emit in `emit_record_as_json` today at all; Text needs the `(ptr, len)` pair to point somewhere that outlives the callee's frame | agg-3 |
| **`Result(Named(C), E)` return** | tag + payload; composes with the shipped `match_result` slot machinery but is its own convention | agg-4 |
| **HKDF Extract → Expand in one process** | needs both rules rewritten to record output across a 4 839-line file with no generator (§6.3) | follow-up |
| **A genuine `bytes` VALUE** | still streaming-only, and still the wrong carrier for these consumers (§2.1). If ever wanted, the width must be compile-time exact from the expression, `[..N]` becomes a *checked bound* rather than the width, and §5.2's register-quad refusal applies | later, on evidence |
| **`byte_at` at a runtime index over an aggregate** | does not arise (§5.2); the ruling is pre-recorded there so it does not get relitigated | later, on evidence |
| **WASM parity** | refuse with a breadcrumb; the computational port is mechanical but unwritten | later |
| **Rust transpiler** | pre-existing `E0308` defect noted in §6.2; clean refusal is a separate small slice | separate |
| **Dedup / one shared `fmul`** | `composition-abi-design.md:193-203`'s Option C stands for the hot path. Slice 1 is about *composition*, not *reuse*, and conflating them is how the 2026-06-02 note ended up deferring both | not planned |

---

## 8. Where the briefs were wrong

Both briefs asked to be told plainly. Nine items; four are arithmetic.

### 8.1 "Every value that must cross the boundary is ≤ 32 bytes" — false

The `ladder → x25519_finish` boundary carries **20 limbs**, not 32 bytes.
`examples/ladder_recursive.verbose:59` declares `which : number [0, 19]` and its output is one
26-bit limb (`[0, 67108863]`, `:7`), not one byte; `tools/tls_gen/vcrypto.py:68-70` collects
all twenty and slices them `limbs[0:10]` = x2, `[10:20]` = z2. That boundary is **20 of
X25519's 52 spawns** — the second-largest single crossing in the whole TLS stack.

So `composition-abi-design.md:139-147`'s "10 limbs ≫ 6 registers" objection was **not** an
arithmetic error about the consumer. It was correctly sized on a boundary that genuinely
exists. The correct statement is: *the TLS **stage** boundaries are ≤ 32 bytes; the X25519
**internal** boundary is 20 words, and it is a real consumer.* Which is decisive against any
register-capped convention. **§2.4's "52 → 8" made the same mistake in the other direction**
and is corrected there to 52 → 14.

### 8.2 "32 bytes = 4 registers" presupposes a representation this codebase does not use

Every crypto concept in the repo is a record of **one-byte-per-field `number [0, 255]`**
(field counts in §2.1a, counted from the declarations). A 32-byte digest is a **256-byte
struct** here. Register packing would require re-emitting every crypto file.

### 8.3 By-value registers do not eliminate the memory write; they add one

A `let`-bound aggregate needs a frame home for its later `.field` reads, so the register path
costs N `mov`s **plus** the same N stores the destination path pays. The only case where
registers win is immediate single consumption, and that case is inlined today, so it never
reaches a `call`. Table in §2.1(c).

### 8.4 "A rule cannot return an aggregate" is a *native* statement, not a language one

Measured at `47d7b8d`: a record-returning rule with a record-typed `let` and `.field` reads
**verifies clean and runs correctly under `--run`** (§1.4). The interpreter needs no change;
`--native --run <the callee>` already emits 821 bytes that build the record. This is a codegen
gap in exactly one backend, which makes the work materially smaller than the original framing
implied — and it means **no new type, no new primitive, no new declaration** is needed, which
is why §4.1 adds nothing.

It also means framing `bytes` as the natural carrier is upside-down: the language *already
has* an aggregate with a declared, exact width and a compile-time-named reader. It is called a
concept.

### 8.5 The performance numbers were not recoverable, and priced the wrong slice

Struck and replaced in §1.3 rather than deferred. Four separate defects in one paragraph: the
primitive measurements (2.38 cyc/word/call, 6 670 cycles) appear nowhere in the repo; "59 ms →
4.5 ms" is 13× beside a quoted 26×; the repo's own X25519 figure is 0.1 s, not 59 ms; and
"0.14 % at 32 bytes" used the byte count where the same document establishes a 32-byte value
is a 256-byte record. Worst of all, **all of it priced X25519, which slice 1 does not touch** —
both X25519 rules are self-recursive and refused by refusal #1. §1.3 now carries a measurement
taken for this note, with its method, its host, and the two instruments that disagree about it.

### 8.6 A packing interim exists today with no compiler change, and it is not an argument against building

Measured, §2.4: a rule can already pack 8 bytes — or 2 26-bit limbs — into one i64 via
`bor`/`shl`, and `shr` (logical, 6-bit-masked) round-trips them exactly. That takes X25519 from
52 spawns to **14** (not 8, §8.1's error mirrored), purely generator-side but at the cost of
regenerating every `.verbose` file with a packed `which` tail.

This is worth knowing before committing engineering time, and worth *not* over-reading: it does
not move the TLS driver into Verbose, so it leaves every proof, every declared read and every
bound stopping at the Python boundary. **The case for slice 1 is assurance, not throughput** —
and stating it that way is what keeps §2.4's Option C argument honest rather than defeated.

### 8.7 One thing both briefs got exactly right, and it is the load-bearing one

The C-line as drawn — *reading a declared aggregate with a bounds-checked index is not C;
writing at a computed offset is* — is the right line, and it is what makes the destination
convention admissible. The project had already drawn it once, in the same place, for the input
direction: *"the pointer in rdi is invisible in the `.verbose` source … it exists only as an
ABI artifact between caller and callee, lifetime bounded by the `sub rsp` / `add rsp` pair
around the `call`"* (CLAUDE.md, slice 5.3). Slice 1 is that sentence, mirrored, with the same
lifetime argument and the same absence of syntax.

### 8.8 Two things the review brief itself got wrong

Recorded because the same discipline applies in both directions.

- **`x25519_rec`'s state concept has 112 fields, not 113.** The doc's original figure was
  right. `X25519RecState` declares 110 limb fields plus `j` and `which`; a
  `grep -c " : number"` returns 113 because it also matches the rule's own `out : number`
  line. §2.2's 237 KB arithmetic (265 × 112 × 8) is unchanged.
- **"~26× less CPU" and "~4.5 ms" ARE recorded in the repo**, at
  `docs/tls13-roadmap.md:234-236`, added by PR #176 — so "unrecoverable from the repo" was too
  strong as stated. What is genuinely unrecorded is the *pair of primitive measurements* the
  derivation rests on (2.38 cyc/word/call and p256_fmul's 6 670-cycle body), which is the half
  that matters and the reason §1.3 strikes the derived figures anyway: a ratio whose inputs
  nobody can reproduce is not a measurement.

Two smaller ones, for completeness: the brief's gen0 size for `swap2` (1 911 B) did not
reproduce — 2 063 B here — because gen0 embeds the source blob and the fixture text differs;
and `docs/self-hosting-service-slice5c-design.md`'s test section is `:79-98`, not `:53-98`
(`:53-61` is "Sizing / byte-identity").

---

## 9. Filter check (five pillars + the axiom)

- **Verifiability** — improved. A composed rule is verified once at its definition instead of
  N times inlined, and 32 lines of `which`-dispatch selection noise leave every crypto source
  that adopts the record output. No new proof obligation, because there is no new declaration.
  One caveat carried from §4.2: the verifier's `.field` check landed in PR #178, and the
  *collision* case it cannot catch is the emitter's problem, not the verifier's.
- **Exploitability** — the concept's field list, already a declaration, becomes the exact width
  and layout of a call boundary. Nothing decorative is added; §4.1 adds no syntax at all.
- **Safety** — one new ABI register (`rsi`), spilled at prologue; +8 B of frame on
  record-returning callables only; destination in the caller's live frame at
  compile-time-constant offsets. No syscall, no heap, no growable buffer, no new `strings`
  surface. Recursion — the only shape needing a per-frame temporary — is refused.
  **Non-aliasing of the per-`let` slot groups is a claim, and it must be a TEST, not a
  sentence.** The previous draft asserted "disjoint per-`let` slot groups, so no aliasing" and
  stopped there. That frame is not flat: `total_slots` (`src/native.rs:4713`) includes
  `tmp_slots` while `tmp_slot_base_local` (`:4712`) does not, so a sret slot placed anywhere
  but strictly below the tmp pool moves the arena tmp base under every VariantConstruct spill
  — and `f26fe3c`, three commits before this note's `main`, was exactly a slot-aliasing bug in
  this exact frame. §6.4's NC-1/NC-2/NC-3 are the standing controls.
- **Traceability** — the same one-hop concession the recursion slices already accepted and
  documented (`native-call-convention-design.md` §6). `objdump` shows
  `mov rdi, rsp ; lea rsi, [rbp-N] ; call f`, which an auditor reads at a glance.
- **Readability** — no new syntax; the `which` field and its dispatch chain *leave* the source.
- **Compiler axiom (controls and applies, never guesses)** — the width, the field order and
  the offsets all come from `concept.fields`. Nothing is inferred. The inline-vs-call decision
  stays mechanical (cyclic ⇒ call; record-returning ⇒ call), never a heuristic, and every shape
  outside §6.1 is refused with a breadcrumb rather than guessed at.
