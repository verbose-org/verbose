# Effects tier for the self-hosted compiler — arc framing + slice 1: `read`

REVISED after fresh-context strategic review (2026-07-23). The review found the
original layout FATAL (resource slots at 0x100000 sit inside the stdin source
area — the region is 4 MiB source-occupied on that channel, not 1 MiB) and five
MUST-FIX omissions (mmap constants, missing-mmap case, blob forcing, sizing
one-truth, code_size mirror). All corrections are folded in below.

## Why this arc

Self-hosting is complete for the computational core: vexprparse parses, verifies
(4 proof surfaces), refuses unverified source, and reproduces its entire self
byte-for-byte (gen1==gen2). The subset boundary is now the EFFECTS tier — every
construct vexprparse cannot compile is a declared external effect (read, reactions,
fetch, services) or an effect-adjacent wrapper (streaming, modules). This is also
where every deployable example lives (read_config, audit_log, health_check,
static_file_server). Widening here turns vexprparse from "compiles itself" into
"compiles the example corpus".

Arc order (each slice its own design+review+PR):

1. **`resource` + `read(<name>)`** — THIS SLICE.
2. Reaction `append_file`.
3. `connection` + `fetch`.
4. `service` (HTTP) — the capstone; composes 1-3.

Order justification (corrected by review): `read` and `append_file` are the SAME
syscall triple — the real asymmetry is that `read` needs zero new expression
grammar (`read(cfg)` already parses as `AstCall`) and its value drops into the
proven packed-span model, while a reaction is a new top-level construct with
trigger-rule linkage and conditional-effect control flow. Grammar surface, not
syscall surface, picks the opener.

Not in the arc (deliberate): modules `use` (multi-file I/O model — separate
design), `@layer` (tokenizer skips @-lines; verify-only, low payoff), streaming
mode (wrapper; revisit after 2). parallel/vectorize hints stay verbosec-only.

## Slice 1 — `read(<resource>)` in the self-hosted compiler

### Goal
A `.verbose` program declaring `resource <name>` with `path: "<literal>"`,
`max: N`, `on_read_error: abort`, using `read(<name>)` as a text value in rule
logic, compiles under gen1 to an ELF whose runtime behavior matches verbosec
`--native`: same stdout bytes on the same file fixture, exit 1 on missing file.

Oracle: `examples/read_config.verbose` shape (verbosec --native) — run-output
equality on a fixture file, exit-code equality on the missing-file path. (Not ELF
byte-equality across compilers — the gate is gen1(P)==gen0(P) self-consistency +
run-correctness vs verbosec, the established corpus discipline.)

### Region map (REDESIGNED — the review's fix (b))

Current reality (verified in review): argv channel mmaps the MAP_FIXED region at
0x20000000 with length **0x100000** (`x86_argv_marshal`, the `mov esi, 0x100000`
byte embedded mid-literal); per-element text slots start at offset **0**, stride
0x10000. stdin channel mmaps **0x400000** (`x86_stdin_marshal`) and the read loop
fills `[0, 0x400000)` with the source. So nothing below 0x400000 is safe on both
channels.

New layout: **resource base = 0x400000, uniform in both channels.** Resource i
owns the 64 KiB slot at region offset `0x400000 + i*0x10000`.

- argv channel: mmap length becomes `0x400000 + n_res*0x10000` when
  `program_uses_resources`, else the literal stays byte-for-byte (SHA gate). The
  hardcoded length byte is split out of the fixed literal into
  `..., le32(region_len), ...` — size-stable (le32 = 4 bytes = the immediate it
  replaces), so `argv_marshal_size` (the mirrored 88) holds.
- stdin channel: mmap length becomes `0x400000 + n_res*0x10000` under the same
  gate. The stdin trampoline has THREE 0x400000 constants — mmap length, per-read
  cap, overflow guard (`cmp r13, 0x400000`). **Only the mmap length grows**; the
  cap and guard bound the SOURCE area and must stay — named here so they don't
  drift together.
- Input-less entry (`rule main` with no params): NO marshal is emitted at all, so
  no region mmap exists. Slice-1 refusal: `program_uses_resources AND has_input
  == 0` → refuse with a breadcrumb naming this case. (A standalone gated mmap is
  the later lift.)
- Element-text boundary made explicit: an element concept with > 16 text fields
  previously faulted on the unmapped page (accidental fail-closed); with the
  grown mapping it would silently land in `[0x100000, 0x400000)`. Add the
  explicit refusal (> 16 text fields per element concept) — it is the documented
  #124 boundary, now enforced instead of accidental.
- p_filesz/p_memsz: unaffected (single PT_LOAD; the region is an anonymous
  runtime mmap) — verified in review, stated here so nobody re-checks.

### Slot + value model

- Slot layout: `[slot+0 .. slot+8)` = the resource's PACKED VALUE cell;
  `[slot+8 ...)` = content bytes. Refusals: `max > 65520`, `path_len + 1 >
  65520` (path+NUL must fit the slot), > 16 resources.
- **Trampoline resource block** (once per invocation, after the marshal, before
  `call entry`): for each declared+referenced resource: copy path bytes to
  `slot+8` via compile-time immediates (src address in the blob, dest, count —
  path LENGTH changes immediates, never code size), append NUL →
  `open(slot+8, O_RDONLY)` → `test rax; js abort` → fd → `read(fd, slot+8, max)`
  (content overwrites the path — dead after open) → `js abort` → len →
  `close(fd)` → pack `((0x20000000 + 0x400000 + i*0x10000 + 8 - src_base) << 32)
  | len` (start compile-time constant, len runtime) → store at `[slot+0]`.
  Single-shot read, no EINTR retry, short-read-becomes-len, close result
  ignored, oversize file silently truncated — verbosec's exact semantics
  (verified against `emit_resource_read_sequence`).
- **Register discipline** (review item 7): verbosec parks the fd in r15, but in
  vexprparse-emitted binaries r15 = arena base, r14 = node count, r12 = argv
  base, r13 = bump. The resource block keeps the fd in **rbx** (ephemeral at
  trampoline time). No r12-r15 touched.
- **`read(name)` emit** (any proc, any position text flows): `movabs rax,
  slot_addr ; mov rax, [rax] ; push rax` (14 B, constant). The packed value then
  composes through length / concat / byte_at / substring / streaming writes via
  the UNCHANGED span paths.
- Abort = the existing sys_exit(1) tail (fail-closed, verbosec parity).

### Sizing one-truth (review item 5)

New `resource_marshal_size` helper used by BOTH the emit pass and the byte pass
(the argv_marshal_size discipline: "one helper, so the trampoline size can never
drift"). Gated `ONLY when program_uses_resources`, mirroring the "ONLY when the
entry rule has an input" clause. Per-resource block is constant-size by
construction (immediates absorb path variance), so
`resource_marshal_size = n_referenced_resources * K`. It joins `blob_end_off`'s
constant chain — every packed span, AstStr immediate, and byte_at
`movabs src_base` depends on this being exact.

### code_size mirror (review item 6)

`span_is_read` gets an arm in **both** `x86_node` and `code_size_node`, at the
same position relative to the real-call fallthrough — the documented two-pass
drift edge. The 14 B constant is asserted by the existing script check.

### Blob forcing (review item 4)

Path bytes are spans into the embedded source, but blob inclusion is gated on
`uses_text`, which does not see `read`. Force it by construction:
`program_uses_resources` ORs into the blob-inclusion gate (the
`program_uses_result` injection precedent). "Most read programs are texty
anyway" is luck, not construction — rejected.

### Parse
- New top-level item: `resource <name>` + indented block `path:` (string
  literal — escaped chars (`\\`, `\"`) in the path REFUSED in slice 1, spans are
  raw source bytes and would diverge from verbosec's lexed path), `max:` (number
  literal), `on_read_error:` (`abort` only — anything else refused with a
  breadcrumb). Mirror of the concept capture: span_is_resource +
  parse_resources → a ResourceList.
- Threading (honest estimate, review item 9): the ResourceList must reach the
  ByteGenState family (name→slot resolution at AstCall sites), ProgGenState, the
  diag walk, and the purity family — realistic **150-300 mechanical edits**, the
  ConceptList scale, not the 40-120 originally claimed. Still slice-shaped.
- `read(cfg)` needs NO expression grammar: it already parses as
  `AstCall("read", [AstVar(cfg)])`; dispatch by name in the AstCall arms
  (byte_at/length/substring precedent). Known shadowing class: a corpus rule
  named `read` is shadowed by the primitive (same as length/max/min today) —
  documented, not new.

### Verify (the self-verify gate must stay clean on the fixed point)
- `read` joins the primitive exemption list (badcall).
- `read(<name>)` where `<name>` is not a declared resource → diagnostic.
- Purity: a `read(<name>)` call site requires `name` in the rule's declared
  `reads:` (R6d walk family; verbosec parity — the auditor greps `reads:` and
  finds every file the program touches).
- Fixed point untouched **by construction of the walk**: the review verified the
  self-source contains zero `read(` call sites (one comment occurrence only), so
  every new walk returns 0 on it because there are zero sites, not by luck.

### Eval (oracle split — pinned)
The self-hosted interpreter cannot open files and must not grow a host-file
primitive. `read` in eval → **empty-span text value** (`VText {start: 0, len:
0}`) — the typed neutral element (review item 10: VNum 0 was the wrong-typed
sentinel; an empty span flows correctly through length/concat/substring).
Pinned as the compile-only split with the documented divergence: eval probes see
empty text, the compiled binary sees file content. Ground truth for behavior =
running the emitted ELF vs verbosec's, same fixture. (Note: "compile-only
primitive" today exists in two mechanisms — max/min fall through to eval_call,
streaming shapes refuse — this slice PINS the read behavior explicitly rather
than inheriting either accident.)

### Trap checklist (review item 15)
- Every new recursive helper (resource_index_of, slot walks,
  resource_marshal_size) mentions its recursion AT MOST ONCE — the
  eager-double-mention 2^N scar.
- Countdown walks use `== 0` base cases, never `< 0`, on unbounded fields — the
  optimizer default-range scar.

### Refusals (breadcrumbed, slice 1)
- `on_read_error:` other than `abort`.
- Dynamic (non-literal) path — verbosec refuses this too.
- Escaped chars in the path literal.
- `max > 65520`; `path_len + 1 > 65520`; > 16 resources.
- `program_uses_resources AND has_input == 0` (no region mmap exists).
- > 16 text fields per element concept (pre-existing #124 boundary, now
  explicit).

### Gate (clean disk)
1. Proofs check out; suite green; ALL existing binaries byte-identical
   (resource-free programs never enter the new code — SHA pins hold; the
   marshal-literal le32 split must reproduce the identical bytes when
   region_len is the old constant).
2. two_generation gen1==gen2 (self-source declares no resources).
3. Corpus: a read program, gen1(P)==gen0(P).
4. MILESTONE: read_config-shaped program compiled by gen1 → run with fixture →
   stdout bytes == verbosec --native's binary; missing file → both exit 1.
   Composition probes: read in concat, length(read(r)). A stdin-channel read
   program (text entry + resource) proving the 0x400000 base is source-safe.
5. Verifier: undeclared resource → diagnostic; read without `reads:` → purity
   violation; both pinned.
