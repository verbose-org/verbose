# Text/bytes-typed `state:` fields on a service — design note (native slice `text-state-1`)

> **Status: DESIGN ONLY. Nothing in this note is implemented.** Written against `main = 7bf998b`,
> clean tree. Every `file:line` below was opened and read at that commit.

---

## 0. Executive summary, and the two things that changed my mind

> **Read §8.4's correction first if you are here for the security finding.** Its claim of a LIVE
> overflow was disproved by measurement; the declaration inconsistency it found was real and is
> fixed in PR #194 (`de9870e`).

A `service` may declare `state:` fields that persist across requests. They are **Number-only**,
refused independently by the parser (`src/parser.rs:1951`) and the verifier
(`src/verifier.rs:1124`). This note designs the text half.

**The recommendation in one paragraph.** A text state field is declared
`name : text [..N] = "<literal>"`, reusing the existing `[..N]` bound syntax verbatim
(`src/parser.rs:422-437`). It gets, inside the service's rbp frame, the same
`(ptr_slot, len_slot) + N-byte buffer` triple that a `resource` and a `connection` already get
(`compute_resource_extra_bytes`, `src/native.rs:2955-2960`). `set <field> = <expr>` in the
`after:` block **copies** the bytes into that buffer with `rep movsb`; it never stores the
source pointer. Reads register under the composite key `__state_<field>` — never the bare name —
and resolve through the existing BoundText machinery. Slice 1 is HTTP/1.0, sequential-only, and
**refuses every `set` whose source has no compile-time-provable upper bound ≤ N**.

**Two things I set out believing, which the code says are false.** Both are in §8 in full; they
are here because they change the design rather than decorating it.

1. **The brief asks where the *runtime* bound check lives. The primary gate should be a
   *compile-time* refusal, not a runtime check.** Every text source reachable in a service
   handler already carries a compile-time upper bound that is *already runtime-enforced at the
   point the bytes enter the process* — `req.method`/`req.path` at the HTTP parse
   (`src/native.rs:21233-21249`), `req.body` by the kernel's `read` count, `read()` by the
   resource's `max:`, `fetch()` by `max_response`. So the compiler can compare *declaration
   against declaration* and either prove the copy safe (emit no check) or refuse. That is
   "exploit the declaration" (design priority #2) rather than merely "enforce it", it removes a
   remote-DoS path a runtime `sys_exit(1)` would create inside a listener, and it makes the one
   genuinely unbounded shape — the append-accumulator — an honest refusal instead of a silent
   truncation. A 13-byte runtime backstop stays, as defence in depth, and §5.5 argues why.

2. **The `forked` story is worse than `CLAUDE.md` says, and it is already worse for Numbers.**
   `CLAUDE.md` records that under `concurrency: forked` "a counter under `forked` therefore
   counts per-connection, not globally". Under HTTP/1.0 one connection is one request is one
   child, and the child's tail is `sys_exit(0)` (`src/native.rs:21144-21160`), so the `after:`
   block's mutation is observed by **nobody, ever** — the next request's child inherits the
   parent's untouched copy. It is not degraded counting; it is no counting. §6.4.

---

## 1. The gap, and why it is a LANGUAGE capability

### 1.1 What exists

```
state:
  count : number = 0

after:
  set count = state.count + 1
```

`examples/counter_service.verbose:47-51`. The handler reads `state.count`
(`examples/counter_service.verbose:30`, declared in its `reads:` proof), the response body is
`concat("count:", state.count)`, and three sequential requests measure `count:0`, `count:1`,
`count:2` (`service_mutable_state_counter`, `src/native.rs:39859`, which spawns the binary and
holds three real TCP conversations).

Number-only is refused twice, deliberately: `src/parser.rs:1951-1956` at parse, and
`src/verifier.rs:1124-1131` at verify. Both messages say "in this slice". `src/ast.rs:254-255`
records the reason: *"Text state fields need (ptr, len, buffer) management and are a follow-up."*

### 1.2 Why this is a language capability, not a TLS feature

The project rule is that applications never drive language design. Three non-TLS consumers, none
of which is expressible today and each of which is a different shape:

- **A WAF / content filter that remembers.** `examples/body_content_gate.verbose` already gates
  requests on a banned substring loaded from disk. What it cannot do is remember anything about
  the *previous* request — so "reject if this path equals the last rejected path" (the cheapest
  useful form of a repeat-offender heuristic) is inexpressible. This is the **last-value** shape.
- **A rate limiter keyed by one client.** A gate that stores the last-seen client identifier and
  a `now_unix()` stamp, and answers 429 when the same identifier returns inside the window. The
  identifier is text; the stamp is a number the language already has. This is the
  **key + counter** shape, and it is the one that makes text state *compose* with what exists.
- **A protocol driver's per-connection buffer.** `docs/tls-io-statemachine-design.md` and the
  "next session priorities" memory both point at moving a TLS driver out of Python, which needs a
  handshake state machine holding bytes across reads. That is a *consumer*, and it is deliberately
  listed third: the design below is settled without it, and if it needed a different shape the
  answer would be to build the general capability anyway and let the driver adapt.

There is a fourth argument that has nothing to do with any consumer. **`state:` is currently the
only place in the language where a declared type is restricted to `number` for an implementation
reason rather than a semantic one.** Every other text position — input fields, record fields,
`let` bindings, `Result` payloads, fold accumulators, handler bodies, log content — takes text.
The restriction is a hole in the type system's uniformity, and closing holes in uniformity is
worth doing on its own.

---

## 2. What already exists that this reuses — measured

Nothing below is proposed. All of it is at `7bf998b`.

### 2.1 The slot discipline already fits, and the persistence property is already load-bearing

`src/native.rs:20480`:

```rust
let state_slots_bytes: i32 = (service.state_fields.len() as i32) * 8;
```

folded into `frame_base_fixed` at `:20487`, with offsets assigned at `:20491-20497`:

```rust
let state_block_start: i32 = -(body_pre_offset + body_extra_bytes + handler_let_slots_bytes);
for (i, sf) in service.state_fields.iter().enumerate() {
    let slot = state_block_start - 8 * (i as i32 + 1);
    state_offsets.insert(sf.name.as_str(), slot);
}
```

and initialised once, before the socket is even created, at `:20517-20527`. The comment at
`:20518-20521` states the property the whole feature rests on:

> *These slots persist across accept iterations because they're within the rbp-relative frame
> (`lea rsp, [rbp - frame_size]` restores rsp below them but never overwrites the slots
> themselves).*

That is verified in the emitted sequence: the iteration tail's `lea rsp, [rbp + neg_frame_size]`
is at `src/native.rs:21144-21152`, and `rbp` is untouched from the prologue (`:20511-20514`) to
the end of the accept loop. **A buffer placed in the same block inherits exactly this property.**
No new mechanism is required for persistence — that is the single largest reason this slice is
small.

### 2.2 The (ptr, len) + sized-buffer precedent is adjacent in the same frame

`compute_resource_extra_bytes` (`src/native.rs:2955-2960`):

```rust
referenced.iter().map(|r| 16 + (((r.max_bytes as i32) + 7) & !7)).sum()
```

— sixteen bytes for the `(ptr, len)` pair, plus a `max:`-sized buffer padded to 8. The connection
sibling is inline at `src/native.rs:20435-20438` with `max_response`. Both blocks live in the same
service frame, below the state block, added into `frame_base` at `src/native.rs:20499`. The slot
map is documented at `src/native.rs:20440-20450`.

**A text state field is byte-for-byte this shape.** `16 + ((N + 7) & !7)`.

### 2.3 Field access is composite-keyed already

`__state_<field>` is resolved in `emit_eval_expr`'s `Expr::Field` arm (`src/native.rs:14817-14821`)
and in its rcx sibling (`:13707-13712`), registered at `src/native.rs:20951-20959`. The aggregate
arc added `__agg_<let>_<field>` on the same principle, and `src/native.rs:5571-5578` names
`__state_` as *"the one existing"* precedent. §4.3 shows why this is not optional for text.

### 2.4 The value machinery from the aggregate arc

`docs/bytes-value-return-design.md` §3.1 settles that a text field crossing a boundary is
**16 B, `(ptr, len)`, in concept declaration order, from a `Vec` and never from a `HashMap`**
(`docs/bytes-value-return-design.md:613-625`; it cites the determinism hazard to
`src/native.rs:14318-14329`, which has since drifted — the DETERMINISM comment is now at
`src/native.rs:15400-15411`, restated at `:2185-2191`). §3.3 (`:646-672`) records the hazard that a pointer parked in a
register before a marshalling loop is destroyed by `emit_strlen`, and that the fix is to emit the
address computation **last**. Both apply here: §4.2's copy computes its destination with `lea`
immediately before use, and the destination is indexed from `service.state_fields`, a `Vec`.

### 2.5 The runtime bound-check machinery, and the scar it came from

`emit_text_bound_check` (`src/native.rs:3045`, doc `:3008-3044`) is a `max+1`-budget
`repne scasb` with an implicit compare — the bound is the `rcx` budget and the branch is `jne` on
the scan's termination. 23 bytes. `emit_bounds_check` (`src/native.rs:2984`) is the Number
sibling, 38 bytes. Both append to a shared `abort_patches` vec drained by
`emit_resource_abort_tail` (`src/native.rs:3078`), which early-returns when the vec is empty — so
programs that need no check pay zero bytes.

They exist because of the defect documented at `src/native.rs:38822-38856`
(`runtime_text_bound_check_rejects_overlong_input_on_every_channel`): a declared `[..N]` sized a
static buffer that nothing enforced, the fill copied the *actual* bytes, and the attacker's fill
byte became the process exit code. **Declaring the bound was what opted a program into the
unchecked path.** §5.5 is written against that scar, and §8.4 reports a live descendant of it.

### 2.6 The ordering, and what makes the copy possible at all

Measured section order inside one accept iteration of `emit_http10_dynamic_bytes`
(`src/native.rs:20361`):

| line | section |
|---|---|
| `:21011` | HANDLER BODY — populates `[rbp-24]` status, `[rbp-32]` body ptr, `[rbp-40]` body len |
| `:21023` | LOG EFFECT — the `for log_block in &service.logs` loop |
| `:21092-21093` | HTTP SERIALIZE — `emit_http_serialize(&mut code)`, **the response write** |
| `:21095-21120` | AFTER BLOCK — `for aset in &service.after_sets` |
| `:21122` | CLOSE |
| `:21144-21152` | ITERATION TAIL — `lea rsp, [rbp + neg_frame_size]` |

Two consequences, both load-bearing:

- The `after:` block runs after the response write and after every log block. `CLAUDE.md`'s claim
  holds. (Small correction: the log blocks run *before* the response write, not after it — the
  sentence "after the response write AND after every log block" is true but can be read as
  ordering the logs after the response. They are not.)
- **The `after:` block runs BEFORE the `lea rsp` that frees the iteration's transients.** Every
  concat buffer the handler allocated, every `read()` buffer, every `fetch()` response, and the
  HTTP read buffer itself are all still live at the copy point. That is what makes §4.2's copy
  expressible without moving anything. **It is an invariant, not an accident, and §7 records it as
  something a future slice must not break.**

### 2.7 What the interpreter and WASM do with services today

Zero. `grep -c Service src/interpreter.rs src/wasm.rs` returns **0 and 0**. `Item::Service` is
handled only in `src/main.rs:316-327`, which dispatches to `native::compile_service` and returns.
`--run` resolves against `Item::Rule` only. **Services are already native-only, structurally**;
text state adds no new asymmetry. §7.2 says what to do about the breadcrumb.

---

## 3. Options considered

### 3.1 Declaration surface

| # | Shape | Failure mode | Verdict |
|---|---|---|---|
| A | `buf : text [..256] = ""` | none found | **RECOMMENDED** |
| B | `buf : text = ""` (size inferred from use) | The compiler would have to *guess* a size from the `set` sites — the axiom violation named in `CLAUDE.md` ("the compiler verifies and applies, never guesses"). And the inference is not even well-defined: two `set` sites with different worst cases have no canonical answer. | **REJECTED** |
| C | `buf : text(256) = ""` (a new sizing syntax) | A second spelling for a bound the language already spells `[..N]`, parsed at `src/parser.rs:422-437` and carried as `Field.range: Option<(i64,i64)>` (`src/ast.rs:387-391`). Two spellings of one concept is exactly the "ergonomic sugar without optimization value" the Development Rules refuse. | **REJECTED** |
| D | `buf : text [..256]` with no `= …` (implicitly empty) | Defensible, and cheaper. Rejected only for uniformity: `count : number = 0` makes the initial value explicit, and a reader scanning a `state:` block should not have to know that one type defaults and the other does not. `= ""` costs three characters and zero bytes (§4.1). | **REJECTED, weakly** |

**On the bound being MANDATORY for text.** A concept field may declare `[..N]` or omit it; omitting
it routes the emitter to a runtime-`strlen` sizing path. There is no such fallback for a
*persistent* slot — the buffer is allocated once, in the prologue, before any request exists. So
`buf : text = ""` with no bound must be a **parse error**, not a silently-unbounded field. This is
the one place where the text surface is strictly stricter than a concept field's, and the reason is
the one the effect model already states: *"Every byte the binary touches is either a stack slot
whose offset is computable at compile time, or a region inside a syscall buffer whose size is a
declared u32 literal"* (`docs/effect-model.md`, "Memory bounds").

**On the initial value.** A **text literal only**. Not `read()`, not `concat`, not `now_unix()`.
The init runs once, above `accept_top`, at `src/native.rs:20517-20527` — where there is no request,
no handler scope, and (for a `read()`) an error policy question with no request to fail. A literal
keeps the init a `jmp`-over-data plus `rep movsb`, and keeps the whole state block visible to
`strings binary`, which is the audit contract in `docs/effect-model.md` ("Audit visibility").
`read()`-initialised state is a legitimate slice-2 idea and is listed in §7.

**The AST must widen, and the brief's framing understates this slightly.** `StateField`
(`src/ast.rs:267-271`) is `{ name, ty, initial_value: i64 }` — there is nowhere to put either the
bound or a text initial value. Recommended:

```rust
pub struct StateField {
    pub name: String,
    pub ty: Type,
    pub max_bytes: Option<i64>,   // Some(N) for text, None for number
    pub init: StateInit,
}
pub enum StateInit { Number(i64), Text(String) }
```

The blast radius is small and was counted: one construction site (`src/parser.rs:1960-1964`), one
emit read (`src/native.rs:20523`), one verify loop (`src/verifier.rs:1123-1140`), and four parser
test assertions (`src/parser.rs:3256`, `:3258`).

### 3.2 The mutation copy

| # | Shape | Failure mode | Verdict |
|---|---|---|---|
| A | **Copy** the bytes into the persistent buffer at the `set` site | none | **RECOMMENDED** |
| B | **Alias** — store the source `(ptr, len)` into the state slots | Silent wrong answer, and the *worst* kind: it works for a text literal (which lives in `.text` forever) and for the first request of anything else. A `set last = req.path` aliased to the HTTP read buffer returns, on the *next* request, whatever that request wrote into the same buffer — i.e. its own path. Plausible output, rc 0, no diagnostic. | **REJECTED** — and it is NC-1 in §6.6 |
| C | Copy **lazily**, at first read | Needs the source to still be live at read time, which it is not: the read is in the *next* iteration, after `lea rsp`. Strictly worse than B. | **REJECTED** |
| D | Copy at the `set` site, but into a **double buffer** so a read in the same request sees the old value | Solves a problem §5.3 shows does not exist: the `after:` block already runs after the response, so no read in the same request can see the new value. Pure cost. | **REJECTED** |

Option B deserves one more sentence because it is the shape a reasonable implementer reaches for
first: the state slots are *already* `(ptr, len)`, and BoundText registration is *already* by
slot, so aliasing is a two-instruction change and it type-checks. It is wrong for one reason —
lifetime — and the lifetime is invisible in the source.

### 3.3 The overflow policy — the substantive decision

| # | Policy | Failure mode | Verdict |
|---|---|---|---|
| A | Runtime `cmp` + `sys_exit(1)` at the `set` site, wired to the existing abort tail | Consistent with every other bound in the compiler — but every one of those is in a **one-shot rule binary**, where exiting is free and the caller re-runs. Here it kills the *listener*. A remote client who can make the source exceed `N` has a one-packet DoS. The compiler's own precedent contradicts it: the HTTP-parse bound guards deliberately chose per-request fail-closed, and say so — *"an over-long path must not be a way to take the listener down"* (`src/native.rs:21212-21216`). | **REJECTED as the primary gate; kept as a backstop** |
| B | Truncate to `N` | Silent data loss, and it makes the declaration a lie in the other direction: the program says "at most N bytes" and the runtime says "exactly the first N of whatever arrived". Refused on the same grounds as every other truncating bound in this language. | **REJECTED** |
| C | Skip the mutation and keep serving | Silent divergence between what the source says and what the process does — the silent-wrong-answer class this repo has spent eight gen0 slices closing. Worse than A. | **REJECTED** |
| D | Per-request drop (close the connection, no response) | **Not available at this point in the emit.** The response has already been written (`src/native.rs:21092-21093`) by the time the `after:` block runs. Making it available means moving the state mutation before the serialize, which destroys §5.3's ordering guarantee. | **REJECTED — structurally unavailable** |
| E | **Compile-time refusal**: prove `worst_case(source) ≤ N` from declarations, or refuse | Refuses the append-accumulator (`set log = concat(state.log, req.path)`), which has no bounded worst case. That is a real cost and §7 names the slice that lifts it. | **RECOMMENDED** |

**Why E works, in full, because the argument is the security argument.** Every text source
reachable in an HTTP/1.0 handler carries a compile-time upper bound, *and* that bound is already
enforced at runtime at the point the bytes enter the process:

| source | compile-time bound | where the runtime enforcement already lives |
|---|---|---|
| text literal | exact byte length | n/a — it is in `.text` |
| `req.method` | 8 | `emit_token_len_guard`, `src/native.rs:21233-21249`, invoked for method at `:21285` and for path at `:21335`; failure joins `fail_patches` → close-connection |
| `req.path` | 256 | same guard, path invocation; both bounds read from the concept itself (`src/native.rs:21217-21227`) so parser and sizer cannot disagree |
| `req.body` | **`max_request`**, *not* the declared 4096 — see §8.4 | the kernel's `read` count; `body_len = bytes_read - 4` (`src/native.rs:20855-20857`) |
| `read(<resource>)` | resource `max:` | the `read(fd, buf, max)` count argument |
| `fetch(<connection>, _)` | connection `max_response:` | same |
| a Phase-2I handler text `let` | recursive over this table | the same buffer sizing already relies on it |
| `substring(t, a, b)` | bound of `t` (a slice can never exceed its haystack) | `substring`'s own fail-closed bounds |
| `json_escape(x)` | `2 × bound(x)` | the 2× worst case is exact (each escaped byte becomes two) |
| `concat(a, b, …)` | sum of the arg bounds | this is *already* how the concat buffer is sized (`src/native.rs:7889-7970`) |
| another `state.<f>` | that field's declared `N` | this design |

So the compiler compares **a declaration against a declaration**, and the only runtime facts it
trusts are ones a *different, already-shipped* check enforces. **That is the exact inverse of the
2026-08-05 defect** (`src/native.rs:38822-38856`), which trusted a declaration about data whose
size came from outside the process with nothing checking it.

**This is a stated dependency, in the style of the `layer_list` → purity dependency in
`CLAUDE.md`: if any row of that enforcement column is ever weakened, this design breaks
silently.** §6.5 pins the two rows that are load-bearing and cheap to regress.

---

## 4. Frame layout and emit sequence

### 4.1 Layout

Only `state_slots_bytes` (`src/native.rs:20480`) and the offset loop (`:20491-20497`) change. The
state block stays exactly where it is — between the handler let slots and the resource block —
and grows per field:

```
  -8    method ptr                    ] unchanged
  -16   path ptr                      ]
  -24   status                        ]
  -32   body ptr                      ]
  -40   body len                      ]
  -48   client_fd                     ]
  -56   timestamp          (optional) ]
        handler let slots             ] unchanged  (handler_let_slots_bytes)
  ┌──── STATE BLOCK ───────────────────────────────────────────────┐
  │  number field:  8 B  — value                    (unchanged)    │
  │  text field:   16 B  — ptr slot, len slot                      │
  │              + N↑8 B — the persistent buffer                   │
  └────────────────────────────────────────────────────────────────┘
        resource (ptr, len) + buffers               ] unchanged
        connection (ptr, len) + buffers             ] unchanged
        HTTP read buffer (max_request)              ] unchanged
```

```rust
// replaces src/native.rs:20480
let state_slots_bytes: i32 = service.state_fields.iter()
    .map(|sf| match sf.ty {
        Type::Number => 8,
        Type::Text   => 16 + (((sf.max_bytes.unwrap() as i32) + 7) & !7),
        _ => unreachable!("verifier rejects other state types"),
    })
    .sum();
```

`sf.max_bytes.unwrap()` is sound only because the parser makes the bound mandatory for text
(§3.1); if that ever becomes optional, this is the site that breaks.

Offsets are assigned by a **descending cursor over `service.state_fields`, a `Vec`** — the same
discipline `docs/bytes-value-return-design.md:620-625` insists on, for the same reason (a
`HashMap` walk here would produce a different byte sequence on every compile, which is
`CLAUDE.md`'s "Reproducible emit" rule and the `scc_callable_order_is_deterministic_across_compiles`
scar). A text field's triple is `ptr_slot = cursor`, `len_slot = cursor - 8`,
`buf_off = cursor - 8 - (N↑8)`, then `cursor -= 16 + (N↑8)`.

**Number fields are byte-identical.** A service with only Number state produces the same cursor
positions it does today, because the Number arm contributes the same 8 and the cursor descends in
the same order. §6.5 proves this by measurement rather than by this sentence.

### 4.2 Emit — three sites

**(a) Init, once, at `src/native.rs:20517-20527`.** For a Number field, unchanged
(`emit_mov_rbp_slot_imm`, `src/native.rs:18336`). For a text field:

```asm
  lea  rax, [rbp + buf_off]
  mov  [rbp + ptr_slot], rax        ; the buffer address, constant for the process's life
  mov  qword [rbp + len_slot], L    ; L = initial literal's byte length
  ; when L > 0, copy the literal in:
  jmp  .over                        ; jmp-over-data, rel8 or rel32 per src/native.rs:8435-8442
  .data: <L bytes>
  .over:
  lea  rsi, [rip + .data]
  lea  rdi, [rbp + buf_off]
  mov  rcx, L
  rep  movsb
```

`L = 0` (the `= ""` case) emits the first three instructions only — about 21 bytes.

**(b) The `set` copy, in the after loop at `src/native.rs:21095-21120`.** Today that loop is
`emit_eval_expr` → `store_rax_at_rbp`. It becomes a two-arm dispatch on the field's type; the
Number arm is byte-identical to today's body, and the text arm is:

```asm
  <emit_text_produce_ptrlen(&aset.value, ...)>   ; rax = src ptr, rdx = len  (src/native.rs:8356)
  cmp  rdx, N                                    ; the BACKSTOP — §5.5
  ja   .abort                                    ;   → the existing abort_patches tail (:21163-21178)
  mov  rsi, rax
  lea  rdi, [rbp + buf_off]                      ; destination computed LAST, per §2.4
  mov  rcx, rdx
  cld
  rep  movsb
  mov  [rbp + len_slot], rdx
```

Six facts about this sequence, each checked:

1. **`emit_text_produce_ptrlen` is the right producer.** It is the same helper the handler let
   prologue already calls for a text `let` (`src/native.rs:20971-20980`), leaving `(rax, rdx)`.
   Its grammar is at `src/native.rs:8356-8460`+.
2. **The transients it may allocate are still live.** §2.6: the after block precedes the
   `lea rsp`. This is the invariant the whole slice sits on.
3. **`ptr_slot` is not rewritten.** The buffer address never changes, so only `len_slot` moves.
   Keeping the redundant `ptr_slot` is deliberate: it is what lets every BoundText reader
   (`src/native.rs:8684-8700`, `:16986-16995`) work unmodified. The alternative — store only the
   length and `lea` the pointer at each read — saves 8 bytes of frame and costs a new arm in
   every reader. Uniformity wins, and `docs/effect-model.md`'s "the bound travels alongside the
   data" argument applies.
4. **Register clobbers are safe.** `rax, rcx, rdx, rsi, rdi` are ephemeral per `CLAUDE.md`'s
   register table. `r12` (server fd) and `client_fd` at `[rbp-48]` — read by the `close` at
   `src/native.rs:21122` — are untouched. `r15` is not live here (the resource and connection
   sequences both close their fds before returning).
5. **`rep movsb` overlap is safe in every reachable case.** `rcx = 0` is a no-op. A `concat`
   source is below `rsp`, hence disjoint from the frame. `set a = state.a` has `src == dst`
   (byte *i* → byte *i*, a no-op). `set a = substring(state.a, k, m)` has `src ≥ dst` within one
   buffer, and forward `movsb` is safe when `dst ≤ src`. Two distinct state fields have disjoint
   buffers. There is no reachable shape with `dst > src` and overlap.
6. **`cld` is emitted.** `emit_text_bound_check` (`src/native.rs:3045-3068`) emits its own `cld`
   before `repne scasb` rather than assuming DF=0. Same discipline; one byte.

**(c) Read registration, at `src/native.rs:20951-20959`.** The existing loop builds
`__state_<field>` keys into `handler_offsets`. A text field additionally inserts into
`http_text_bindings` under **the same composite key**:

```rust
http_text_bindings.insert(&state_composite_keys[i], (ptr_slot, len_slot));
```

`state_composite_keys` is already a `Vec<String>` that outlives the map (`src/native.rs:20948-20953`
explains why), so the lifetime works unchanged.

### 4.3 The composite key is mandatory, and here is the collision

`src/native.rs:20878` does:

```rust
http_text_bindings.insert("body", (body_ptr_slot, body_len_slot));
```

— a **bare** name. And three sites extract a BoundText's key from a field expression *ignoring the
base*:

- `src/native.rs:8098` — the concat **sizing** pass: `Expr::Field(_, n) => Some(n.as_str())`
- `src/native.rs:8680` — the concat **fill** pass: same
- `src/native.rs:9561` — `emit_text_write_to_fd`: same

So a service declaring `state: body : text [..64] = ""` alongside any use of `req.body` would,
under bare-name registration, have `state.body` resolve to **`req.body`'s slots** — a plausible
value, rc 0, no diagnostic. That is the silent-wrong-answer class, and it is the same hazard
`docs/bytes-value-return-design.md` §6.2 refusal #9 records for `__agg_`.

Slice 1 therefore does two things: registers under `__state_<f>`, and makes those three
extractions **base-aware** —

```rust
Expr::Field(base, n) => Some(match base.as_ref() {
    Expr::Ident(b) if b == "state" => /* the composite key */,
    _ => n.as_str(),
}),
```

`classify_concat_arg`'s state arm (`src/native.rs:7697-7701`) currently returns
`ConcatArgKind::Number` unconditionally with the comment *"state.field — Number-only in this
slice"*; it grows a text branch that probes the composite key. **NC-3 in §6.6 is the control that
proves the keying is real**, and it must use a fixture whose state field is literally named
`body`, because a fixture named anything else passes under bare-name registration too.

---

## 5. Semantics

### 5.1 Read

`state.<f>` in the handler resolves as **BoundText**: a `(ptr, len)` pair, identical in shape to
`req.body`, `read()`, `fetch()` and a Phase-2I text let. It therefore composes with `concat` and
`length` for free once §4.3's three sites are base-aware. `length(state.f)` is a zero-scan slot
load (`src/native.rs:16986-16995`), because the length is already stored.

The handler must declare `reads: [state.<f>]`, unchanged — the verifier already accepts
`state.X` as a length-2 path (`src/verifier.rs:4033-4037`) and cross-checks it against the
declared fields (`src/verifier.rs:1155-1175`). This is what keeps a state-reading handler
greppable in exactly the way a resource-reading one is.

Predicates (`starts_with`, `contains`, `ends_with`), `substring`, `json_escape`, `parse_int` and
text `==` each dispatch through their own field arm which is gated on
`base == input_name` (`emit_starts_with_load_text`, `src/native.rs:17368`;
`emit_length`, `:16982`; `emit_text_produce_ptrlen`, `:8454`). **Slice 1 refuses all of them on
`state.<f>` with named breadcrumbs** (§6.2) rather than widening seven arms at once.

### 5.2 Write

`set <f> = <expr>` copies (§4.2b). The source may be any shape §3.3's table bounds, provided the
worst case is ≤ `N`.

### 5.3 Ordering — unchanged, and still right for bytes

The after block runs after the response write and after every log block (§2.6, measured). So:

> **A state field is never observable to the request that changed it.**

That still holds for text, and the argument for it is *stronger* for text than for a counter. A
session cache wants "commit after the response was actually served", so that a request whose
response failed does not leave a record claiming it was served — the same fail-closed reasoning
`docs/effect-model.md` gives for `on_error: abort`. And it keeps the response a function of
`(method, path, body, state-as-of-entry)`, which is the reproducibility argument the effect model
already makes for `req.timestamp` ("`req.timestamp` is intentionally restricted").

The honest cost, stated because a reader will hit it: **"echo back what I just stored" is
inexpressible.** The value a request sets is visible from the *next* request onward. For the
last-value, session-cache and rate-limiter shapes that is correct. For a "return the id I just
minted" shape it is not, and the answer there is a handler `let`, not state.

### 5.4 `forked` — unchanged, and worse than documented

Unchanged, and **it must stay unchanged**: making a text buffer shared across children means
shared memory, which `docs/effect-model.md` refuses by name in two adjacent bullets
("Shared-memory service state", "Pthreads / shared-memory concurrency"). This is a consequence of
a standing refusal, not a gap.

What a byte-state service under `forked` actually means, stated plainly because the current
documentation is optimistic: **nothing at all.** The fork is per `accept`
(`src/native.rs:20654-20655` + the fork dispatch), HTTP/1.0 is one request per connection, and the
child's iteration tail is `sys_exit(0)` (`src/native.rs:21144-21160`). So each child executes the
`after:` block exactly once and then dies without anyone reading the result; the next child
inherits the *parent's* copy, which is still the initial value. A forked text state field is a
**write-only constant**. So is a forked counter — this is not new with text, and §8.3 records it as
a correction to `CLAUDE.md`.

**Recommendation: the verifier refuses `state:` together with `concurrency: forked`**, for both
types, with a breadcrumb saying the mutation is unobservable. This refuses something currently
accepted, so it is a behaviour change and the user should make the call explicitly. It costs
nothing in the corpus: `examples/counter_service.verbose` is the only file with a `state:` block
(measured over `examples/*.verbose`) and it declares no `concurrency:` line. The softer
alternative — keep accepting it, document it harder — is defensible, and the argument against it
is that a declaration whose only effect is to be discarded is precisely the *false explicitation*
the Development Rules forbid.

### 5.5 Overflow — where the check lives

**Primary gate: compile time.** For each `set <f> = <expr>`, the verifier computes
`worst_case(expr)` from §3.3's table and refuses when it exceeds `N`, naming both numbers. A
program that passes carries a proof, not an assertion, and emits **zero** check bytes.

**Backstop: 13 bytes at the set site**, `cmp rdx, N` (7) + `ja rel32` (6), wired to the abort tail
that `src/native.rs:21163-21178` already emits. It is **unreachable by construction** given the
gate above — which is exactly why it is worth arguing about rather than assuming.

The case for keeping it: the compile-time gate is a piece of *reasoning*, and this repository's
scar is that the reasoning was subtly wrong. The 2026-08-05 defect was not a missing check; it was
a *correct-looking* static size defended by an argument that had a hole in it, and §8.4 below
reports a live descendant of the identical mistake, in this very emitter, that nobody has noticed
because every service example happens to use `max_request : 4096`. Thirteen bytes converts "a
future widening of the source table becomes a remote stack overflow" into "a future widening
becomes an `exit(1)`". Given security is pillar #1, that trade is worth making.

The case against, stated fairly: unreachable code that cannot be exercised from any source program
is close to *false explicitation*, and the Development Rules forbid decoration. The answer is that
it **is** exercised — by NC-4 in §6.6, which patches the compile-time gate out, feeds an over-long
value and asserts `exit(1)`. A backstop with a compiler-patching negative control is a mechanism;
a backstop without one is decoration. **If NC-4 is not written, drop the backstop.**

What the backstop does *not* do is create the DoS §3.3 rejected: it is unreachable, so no client
can drive the process to it.

---

## 6. Slice `text-state-1`

### 6.1 Scope

1. `state:` accepts `<name> : text [..N] = "<literal>"`, with `1 ≤ N ≤ 65536` and
   `len(literal) ≤ N`.
2. `Protocol::Http10` only (inherits `src/verifier.rs:1115-1120`) and
   `ConcurrencyMode::Sequential` only (§5.4).
3. Reads: `state.<f>` as a `concat` argument, as `length(...)`'s operand, and as an
   `HttpResponse.body` value directly.
4. Writes: `set <f> = <expr>` where `expr` is a literal, `req.method`/`req.path`/`req.body`,
   another `state.<g>`, `read(<r>)`, `fetch(<c>, _)`, a handler text `let`, or a `concat` of
   those — and `worst_case(expr) ≤ N`.
5. Native only.

The `N ≤ 65536` cap is a judgement call and should be read as one. The state block shares one
`sub rsp, imm32` frame with `max_request` and every handler let; 64 KiB per field is generous for
all three §1.2 consumers and keeps the block from dominating the frame. It is *not* the effect
model's 64 MiB resource ceiling, deliberately — a resource buffer is per-invocation, this is
per-process-lifetime. **Note a pre-existing exposure this does not fix:** the verifier bounds
`max_request` only below (`src/verifier.rs:967`, `> 0`), so the total service frame is already
unbounded above; §7.3.

### 6.2 Refusals, with breadcrumbs

House rule: each names the offender and the slice that lifts it.

| # | Shape | Breadcrumb |
|---|---|---|
| 1 | text state field with no `[..N]` | `state field 'f': a text state field must declare a maximum byte length, e.g. 'f : text [..256] = ""'; the buffer is allocated once in the service prologue, so its size must be a compile-time constant.` (parse error) |
| 2 | `[..N]` out of range | `state field 'f': declared bound N must be in 1..=65536; the state block shares one stack frame with max_request and every handler let.` |
| 3 | initial literal longer than `N` | `state field 'f': initial value is L bytes but the declared bound is N.` |
| 4 | non-literal initial value | `state field 'f': the initial value must be a text literal; state is initialised once before listen(), where there is no request scope and no error policy for a failed read. Resource-initialised state is slice text-state-3.` |
| 5 | a `set` whose source has no compile-time bound | `after: set 'f' = <expr>: source shape '<K>' has no compile-time byte bound, so the copy into a fixed N-byte buffer cannot be proved safe. Slice text-state-1 accepts literals, req.method / req.path / req.body, state fields, read(), fetch(), handler text lets, and concat of those.` |
| 6 | a `set` whose worst case exceeds `N` | `after: set 'f' = <expr>: worst case W bytes exceeds the declared bound N. Append-accumulation (concat(state.f, …)) can never satisfy this — it needs a declared overflow policy, which is slice text-state-2.` **This is the accumulator refusal, and it must say so explicitly** so a reader is not left thinking the bound is merely too small. |
| 7 | ~~`state:` with `concurrency: forked`~~ **ALREADY SHIPPED — PR #194 (`de9870e`)**, and keyed better than proposed here: on **`after_sets`, not `state_fields`**. A `state:` block with no `after:` is a per-process constant that reads identically under both modes, so refusing it would reject valid programs (verified: forked + state-without-after still accepts). Slice text-state-1 inherits this refusal; it does not add it. | |
| 8 | `state.<f>` in `starts_with` / `ends_with` / `contains` | `starts_with: 'state.f' is a text state field; its (ptr, len) load is not wired into emit_starts_with_load_text (native.rs:17368) in slice text-state-1. Bind it to a handler let first, or wait for slice text-state-2.` (one message per primitive, each naming its emitter site) |
| 9 | `state.<f>` in `substring` / `json_escape` / `parse_int` / text `==` | same shape, naming `src/native.rs:8454`, the JsonEscape sizing arm at `:7941-7967`, `emit_parse_int` at `:17828`, and the text-equality arms at `:14956-15030` |
| 10 | `state:` on a `raw_tcp` service | unchanged — `src/verifier.rs:1115-1120` |
| 11 | `bytes`-typed state field | `state field 'f': type 'bytes' is not supported; a service handler has no bytes-valued position today (HttpResponse.body is text), so a bytes state field would be write-only. Slice text-state-4.` **See §8.5 — the brief's title says "text/bytes" and the bytes half has no consumer inside a service.** |

WASM and interpreter: §7.2.

### 6.3 Worked example — `examples/last_path_service.verbose`

```
service memo
  listen:  protocol: http_1_0 ; port: <ephemeral> ; max_request: 4096
  handler: recall
  state:
    last : text [..256] = "none"
  after:
    set last = req.path

rule recall
  input:  req : HttpRequest
  output: resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: concat("prev:", state.last) }
  proofs:
    purity: reads : [state.last, req.path] ; calls : []
```

Three sequential requests must give:

| request | expected body | what it proves |
|---|---|---|
| `GET /alpha` | `prev:none` | the initial literal reached the buffer, and the mutation is **not** visible to its own request (§5.3) |
| `GET /beta` | `prev:/alpha` | the value **persisted across the `lea rsp`** at `src/native.rs:21144-21152`, and it was **copied**, not aliased |
| `GET /gamma` | `prev:/beta` | it is a live cell, not a one-shot |

**The source is `req.path`, not a literal, and that is the whole point of the fixture.** A literal
source lives in `.text` forever, so an aliasing implementation would pass all three rows. §6.6
NC-1 is why this matters.

`worst_case(req.path) = 256 ≤ 256`, so the compile-time gate passes and **no runtime check bytes
are emitted**. Expected size: `counter_service`'s ~975 B plus the state block's init (~21 B for a
4-byte literal), the copy (~20 B), and the concat's BoundText arm — call it ~1.1 KB, to be pinned
by measurement, not by this estimate.

A second fixture, `examples/negative/state_text_accumulator.verbose`, holds refusal #6:
`last : text [..64]` with `set last = concat(state.last, req.path)`, worst case `64 + 256 = 320`.

A third, `examples/negative/state_text_shadows_body.verbose`, holds §4.3: a state field named
`body` in a service that also reads `req.body`.

### 6.4 Acceptance tests

Native, `src/native.rs`, alongside `service_mutable_state_counter` (`:39859`), which is the
template — it spawns the binary, polls for bind, and holds one fresh TCP connection per request.

1. `service_text_state_persists_across_requests` — the §6.3 conversation, all three rows.
   **Use an ephemeral port** (`TcpListener::bind(("127.0.0.1", 0))`, the pattern at
   `src/native.rs:25624` and seven other sites) — *not* `service_mutable_state_counter`'s
   hardcoded `18950`, whose `src.replace("18950", &port.to_string())` at `:39865-39866` is a no-op
   substitution and is genuinely collision-prone.
2. `service_text_state_initial_literal_visible_on_first_request` — row 1 alone, asserted
   separately so a regression in init is distinguishable from a regression in copy.
3. `service_text_state_composite_key_does_not_shadow_req_body` — the `body`-named fixture:
   `state.body` and `req.body` must give different bytes in one response.
4. `service_text_state_refuses_unbounded_accumulator` — refusal #6, rc 1, zero bytes, message
   naming both numbers; plus a **corrected twin** (`[..512]`) that must verify and run, so the
   refusal is attributable to the bound and not to `concat` generally. (Corrected twins are the
   house answer to attributable refusals — `docs/bytes-value-return-design.md:1038-1060`.)
5. `service_text_state_refuses_forked` — refusal #7, plus a sequential twin that passes.
6. `service_text_state_refuses_unwired_predicates` — refusals #8/#9, one cell each, each with a
   twin that binds to a handler `let` first and passes.
7. Parser/verifier unit tests mirroring the existing four
   (`src/parser.rs:3216`, `:3264`, `:3303`, `:3440`) and four
   (`src/verifier.rs:8515`, `:8556`, `:8596`, `:8635`).

### 6.5 Byte-identity — how it will be proved, not argued

The house method (`CLAUDE.md`'s corpus sweeps; `docs/bytes-value-return-design.md:1102`+):

- Build a compiler from `7bf998b` and one from the branch. Compile **all 154 `examples/*.verbose`
  × every rule / service / reaction**. Compare by size **and** sha256.
- **Run baseline-vs-baseline FIRST and require it empty.** `CLAUDE.md` records that until the
  2026-08-10 determinism fix, a same-size-different-bytes delta was as likely to be the hasher as
  a real regression; post-fix the control is a cheap assertion, and a non-empty one voids the
  sweep.
- Expected: exactly the new example's rows change. Everything else identical.
- Specifically, these must not move: `service_mutable_state_counter` (`src/native.rs:39859`, still
  ≤ 2048 B, still `count:0/1/2`), and gen0's two service byte-pin tables —
  `self_hosted_service_log_field_content` (`:26447`, an 8-row size+sha table plus two field-log
  pins) and `self_hosted_service_concurrency_forked` (`:27249`, an 11-row table plus a
  `forked == sequential + 160` delta assertion). None of those pinned probes declares `state:`, so
  they are structurally out of reach — which is a reason to expect the sweep to be clean, not a
  substitute for running it.

The structural argument behind the expectation: every new emit is gated on `sf.ty == Type::Text`,
and the Number arm of `state_slots_bytes` contributes the same 8 in the same `Vec` order.

### 6.6 Negative controls — what to break to prove a test is not vacuous

The aggregate arc shipped three controls that passed vacuously on the flagship fixture. Each
control below names the fixture it must run on, and why that fixture and not another.

| # | Break | Fixture | Must | Why not the obvious fixture |
|---|---|---|---|---|
| **NC-1** | Replace the `rep movsb` copy with `mov [rbp+ptr_slot], rax` (i.e. **alias**) | §6.3's `req.path` fixture | FAIL — row 2 returns `prev:/beta` (its own path) instead of `prev:/alpha` | A **literal**-sourced fixture PASSES the broken build: a literal lives in `.text` and is never freed. This is the exact trap the aggregate arc fell into three times. |
| **NC-2** | Move the state buffer allocation below `rsp` (out of the rbp frame) | §6.3 fixture | FAIL — row 2 returns garbage | A **single-request** test passes the broken build. The control needs ≥ 2 requests. |
| **NC-3** | Register in `http_text_bindings` under the **bare** field name | the `body`-named fixture | FAIL | The §6.3 fixture (`last`) PASSES under bare-name registration — nothing collides with `last`. The control is only discriminating on a name that collides, and `body` is the only one registered bare (`src/native.rs:20878`). The `last` fixture must **still pass** in the same run, or the control is proving nothing about keying. |
| **NC-4** | Delete the compile-time gate (refusal #6) | the accumulator fixture, driven with a long path | exit 1 with no response | **This is the only way the §5.5 backstop is ever executed.** Without NC-4 the backstop is untestable and should be deleted instead. |
| **NC-5** | Move the after loop above `emit_http_serialize` (`src/native.rs:21092`) | §6.3 fixture | FAIL — row 1 returns `prev:/alpha` | A test asserting only rows 2 and 3 PASSES the broken build. The ordering guarantee lives entirely in row 1. |
| **NC-6** | Drop the `req.path` HTTP-parse length guard (`src/native.rs:21335`) | a `[..8]`-bounded state field set from `req.path`, long path | FAIL / overflow | §3.3's soundness rests on that guard. This control is what makes the dependency mechanical rather than a paragraph. It is the cheapest insurance against §8.4 repeating. |

---

## 7. What slice 1 does NOT do

### 7.1 Deferred, with the slice that lifts each

- **`text-state-2` — a declared overflow policy.** `on_overflow: reject | abort | truncate` on the
  state field, mirroring `on_error:` / `on_read_error:` / `on_connect_error:`. This is what unlocks
  the append-accumulator and the rate-limiter's growing key set. `reject` is the interesting one
  and it is not free: rejecting a request whose response has already been written requires either
  moving the state mutation before the serialize (destroying §5.3) or accepting that "reject" means
  "reject the *next* request". That question is the whole content of the slice.
- **`text-state-2` also — the remaining read positions.** `starts_with` / `ends_with` /
  `contains` / `substring` / `json_escape` / `parse_int` / text `==` on `state.<f>` (refusals
  #8/#9). Each is one base-aware arm in a named emitter; batched because they share the shape.
- **`text-state-3` — `read(<resource>)` as an initial value.** Load a seed from disk once at
  startup. Composes with `cache: true`'s existing above-`accept_top` hoist
  (`src/native.rs`, slice 9.4).
- **`text-state-4` — `bytes`-typed state.** Blocked on there being a bytes-valued position in a
  service at all; see §8.5.
- **Not scheduled: shared state across forked children.** Refused on principle
  (`docs/effect-model.md`). A proposal must argue past that refusal first.
- **Not scheduled: state on `raw_tcp`.** Unchanged from today.

### 7.2 Backends — a deliberate asymmetry, not an accidental one

`src/interpreter.rs` and `src/wasm.rs` contain **zero** occurrences of `Service` (measured by
grep). Services are already native-only, and `--run` resolves against `Item::Rule` only
(`src/main.rs:316-327`). Text state adds no asymmetry; it inherits one.

What *should* change, and it is one line rather than a slice: `--run <service-name>` and
`--wasm <service-name>` currently fail with an unknown-rule message, which is a true statement
about the lookup and a misleading one about the language. Both should say
`'S' is a service; services are emitted by the native backend only (--native). The interpreter and
the WASM backend have no accept-loop.` That makes the asymmetry legible, which is the standard
`docs/bytes-value-return-design.md:1060` sets for WASM refusals.

### 7.3 Two pre-existing issues this note found and does not fix

- **The service frame is unbounded above.** `max_request` is checked `> 0`
  (`src/verifier.rs:967`) and never capped, and `frame_size` is a `u32` fed to `sub rsp, imm32`.
  A large `max_request` produces a multi-gigabyte stack adjustment. Out of scope; worth its own
  look.
- **§8.4's `req.body` sizing hole.** Genuinely security-relevant and genuinely not this slice.

---

## 8. Where the brief was wrong, or where I was

The brief asked for the correction rather than compliance. Six items; #4 is the one that matters.

### 8.1 The ordering citations are off by ~1080 lines

The brief says *"native.rs log loop at ~19988, emit_http_serialize, then the after loop at
~20015"*. The **order is exactly right**; the line numbers are not. `src/native.rs:19983` is
`analyze_http10_handler_shape`. The real sites are `:21023` (log loop), `:21092-21093`
(serialize), `:21095` (after loop), inside `emit_http10_dynamic_bytes` (`:20361`). Worth
correcting because the brief's numbers land in an unrelated function, and a reader following them
would conclude the ordering claim was unverified.

### 8.2 `src/parser.rs:1953` is the message, not the check

The Number-only refusal is `if ty != Type::Number` at **`src/parser.rs:1951`**; `:1953` is the
format string inside it. The verifier's independent copy is `src/verifier.rs:1124`. The brief's
larger claim — that it is enforced independently in both — is correct, and worth keeping: it is
why both need to move together.

### 8.3 The `forked` caveat is worse than `CLAUDE.md` records, and already worse for Numbers

`CLAUDE.md` says *"A counter under `forked` therefore counts per-connection, not globally."* That
reads as degraded-but-meaningful counting. Traced through the emitter, it is not: fork is per
`accept`, HTTP/1.0 is one request per connection, and the child's tail is `sys_exit(0)`
(`src/native.rs:21144-21160`), so the `after:` mutation is written into a frame that is discarded
microseconds later and read by nobody. **A forked counter reads its initial value on every request,
forever.** The brief says the bytes story "should not change" from the Number story, and it does
not — but the shared story is worse than either document says. Hence refusal #7 (§6.2), which is a
behaviour change for Number and needs the user's explicit go-ahead.

### 8.4 A latent — NOT live — descendant of the 2026-08-05 defect (CORRECTED, and since FIXED)

> **CORRECTION, added after review (2026-08-30).** This section originally claimed the overflow
> below was reachable "the first time someone raises `max_request`". **That is wrong**, and it was
> disproved by measurement rather than by reading:
>
> - **The plain-concat path is runtime-sized, not static.** `ConcatArgKind::BoundText =>
>   { has_dynamic = true; }` (`src/native.rs:7927-7928`) puts `req.body` on the dynamic path.
>   Measured: a service with `max_request : 65536` and `body : concat("[", req.body, "]")`
>   round-trips a **60 000-byte** body correctly (60 044 bytes returned). No overflow.
> - **The `json_escape` path does not compile at all.** Both in a handler concat and in a `log:`
>   block, `concat(json_escape(req.body), …)` dies with
>   `native codegen error: json_escape inner field 'body' has no rbp slot`. The static arm this
>   section indicts is therefore unreachable with `body` as inner — the refusal blocks it, by
>   accident rather than by design, but it blocks it.
>
> **What the section got RIGHT, and what it bought.** The *declaration* really was false:
> `body : text [..4096]` contradicted its actual runtime bound (`max_request`, which the verifier
> only checks is non-zero), and the verifier's own doc comment said so. That inconsistency was a
> real landmine — the static arm resolves `Expr::Field` against the declared range without
> consulting `text_bindings`, so wiring body's rbp slot into the json_escape fill (a plausible
> future slice) would have resurrected the 2026-08-05 overflow, remotely.
>
> **Fixed in PR #194 (`de9870e`)**: `body`'s declared range now DERIVES from `max_request`
> (`src/native.rs:20384` per-service; the verifier takes the max over all Http10 services), so the
> declaration is true by construction and the landmine is defused. The corpus was byte-identical
> (1375/1375) because all 23 service examples declare `max_request : 4096`.
>
> **The transferable lesson is about this document.** A careful, well-cited design note produced a
> confident security finding that a five-minute probe refuted. Read the rest of this section as
> the *reasoning that led to a real fix*, not as a description of a live hole.


The brief warns: *"do not design a repeat"* of the unenforced-declared-bound overflow. While
verifying that `req.body` is a safe copy source, I found the same pattern still present:

**`req.body`'s declared bound `[..4096]` is exploited for static buffer sizing and is not
runtime-enforced.**

- The bound is declared twice, at `src/native.rs:20351-20355` (native's copy of the built-in
  concept) and `src/verifier.rs:897-899` (the verifier's).
- The verifier's own doc comment says what is actually true: body is *"bounded by the service's
  `max_request` at runtime"* (`src/verifier.rs:875`). **The comment and the declaration disagree
  whenever `max_request ≠ 4096`.**
- Nothing enforces 4096. The body parse (`src/native.rs:20831-20880`) scans for `\r\n\r\n`, then
  `add rbx,4 ; sub rax,4` and stores `rax` as the length. There is no length guard — unlike
  `req.method` and `req.path`, which do have one (`src/native.rs:21233-21249`, added by the
  2026-08-05 fix). The only bound is the kernel's `read` count, i.e. `max_request`.
- And the declared 4096 **is** used statically: `emit_concat_to_buffer_impl`'s `JsonEscapedText`
  arm (`src/native.rs:7941-7967`) looks the inner field's `range` up in the concept and does
  `static_total += 2 * max_len` at `:7964` — reserving 8192 bytes. The dynamic sizing path
  explicitly documents that it is *"only reached when the inner does NOT have a `[..N]` bound
  (otherwise the `static_total` path absorbed it)"* (`src/native.rs:8128-8131`). The fill then
  writes the **runtime** length (`emit_json_escape_fill_loop`, `src/native.rs:8739-8748`).
- `max_request` has no upper bound in the verifier (`src/verifier.rs:967`, `> 0` only).

So a service declaring `max_request : 65536` with `json_escape(req.body)` anywhere in a handler
concat or a `log:` block reserves 8192 bytes and fills with up to ~65500 attacker-chosen bytes —
remotely, unauthenticated. **It is latent, not live**: every one of the twenty service examples
declares `max_request : 4096` (measured over `examples/*.verbose`), which makes 4096 a correct
worst case by coincidence. It goes live the first time someone raises `max_request`, and nothing
in the corpus, the test suite, or the gen0 sweep would notice.

The fix is small and belongs in its own slice: either add a body-length guard next to the
method/path ones (routed to `fail_patches`, i.e. per-request fail-closed), or make the two
built-in concept copies declare body's bound as `max_request` so the static sizing is honest. The
second is more in keeping with the reason the method/path bounds are read from the concept in the
first place — *"so the parser and the sizer can never disagree"* (`src/native.rs:21217-21227`).

**Why this belongs in this note.** §3.3's compile-time gate is only sound because every source's
compile-time bound is backed by an already-enforced runtime bound. `req.body` is a row in that
table, and it is the row that is currently wrong. Slice `text-state-1` must therefore use
**`max_request`**, not 4096, as `req.body`'s bound — and NC-6 exists to keep that dependency
mechanical.

### 8.5 "text/bytes" — the bytes half has no consumer inside a service

The brief's title is "text/bytes-typed `state:` fields". Text is designed above. **Bytes is not,
and should not be in slice 1**, because there is no bytes-valued position in a service to read it
from or write it into: `HttpResponse.body` is `Type::Text` (`src/verifier.rs:925-927`),
`req.body` is `Type::Text` (`:897-898`), and `docs/effect-model.md` records that binary response
bodies "await bytes primitives". A `bytes` state field today would be write-only — the definition
of false explicitation. Refusal #11 says so. (The `raw_tcp` protocol *does* use a
`bytes [..max_request]` concept, `src/verifier.rs:988-992`, which is where bytes state will
eventually make sense — and `state:` is refused on `raw_tcp` today anyway.)

### 8.6 The brief locates the check at runtime; I locate it at compile time

Stated in §0 and argued in §3.3/§5.5. The brief's instinct — "every other bound in this compiler
is fail-closed, so this one should be too" — is right about the *posture* and, I think, wrong about
the *mechanism*, for one reason the brief could not have known without the emitter open: every one
of those other fail-closed bounds is in a **one-shot rule binary**, and the one bound that lives
inside a listener already chose per-request fail-closed over `sys_exit(1)`, in writing, for exactly
this reason (`src/native.rs:21212-21216`). Compile-time refusal keeps the posture (nothing unsafe
compiles) while removing the failure mode (nothing remote can stop the listener), and it exploits
the declaration instead of merely enforcing it.

### 8.7 One thing the brief got exactly right, and it is load-bearing

*"The slot discipline already fits."* It does, and it is why this is a slice rather than an arc.
The persistence property at `src/native.rs:20518-20521` was written for an 8-byte counter and is
stated in terms that make no reference to width — `lea rsp, [rbp - frame_size]` restores rsp
*below* the state block and never touches it. An N-byte buffer inherits it verbatim. The one thing
the brief's framing understates is `StateField`'s shape (§3.1): the AST does have to widen.

---

## 9. Filter check — five pillars and the axiom

| Priority | How this slice pays |
|---|---|
| **1. Verifiability** | `[..N]` is mechanically checked (bound range, initial-literal length, worst-case-vs-N per `set`). `reads: [state.f]` is already cross-checked (`src/verifier.rs:1155-1175`). No declaration is unchecked. |
| **2. Exploitability** | `N` is *used*: it sizes the buffer, and it is the compile-time gate that removes the runtime check entirely. The declaration buys both safety and zero emitted bytes — the two-for-one this priority exists to demand. |
| **3. Safety** | Nothing unbounded compiles. The one runtime path (§5.5) is a backstop with a control that reaches it, not decoration. |
| **4. Traceability** | The state block, its bounds and its initial literals are all in the source and all in `strings binary`. Every reader appears in the handler's `reads:` proof. |
| **5. Readability** | `last : text [..256] = "none"` reads the same as `count : number = 0` and the same as `name : text [..64]` anywhere else in the language. One spelling, one meaning. |
| **Axiom** | Nothing is inferred. The size comes from a declaration; the worst case comes from other declarations; an unprovable `set` is refused with both numbers named rather than guessed at. |
