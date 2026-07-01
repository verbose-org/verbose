# R7 — self-hosted codegen for variant/match (the arena, in Verbose)

## Context & honest scale
`x86_node` (the self-hosted emitter) is a STACK MACHINE: each node emits bytes that
leave its result pushed (`\x50`). `x86_proc` wraps a rule as `push rbp; mov rbp,rsp;
[sub rsp for lets]; <let stores>; <body>; pop rax; mov rsp,rbp; pop rbp; ret`. The
b8 ELF `_start` calls the entry proc and itoa-prints `rax`. It emits the CLOSED
scalar grammar (arith/if/let/calls/recursion) → a runnable ELF. `AstVariant`/
`AstMatch`/`AstField`/`AstStr`/`AstBool`/`AstNot` all stub to `\xcc` (int3).

R7 = emit real machine code for variant/match — an ARENA + tag dispatch, hand-written
IN Verbose (x86_node building bytes). This is greenfield: the stack machine has no
heap, and this is NOT a copy of verbosec's native.rs (different register model). It
is the largest remaining sub-arc — plausibly ≈ R1–R6 combined, multi-session. Sliced
below; R7a first. It is what turns "reads+checks+runs its language" into "COMPILES it".

## Arena-in-the-stack-machine convention (the core design)
- A **variant VALUE = its arena INDEX** (a Number). Indices flow through the stack
  machine unchanged (the machine already moves i64s on the stack) — no value-model
  change, unlike the interpreter's R6c.
- **Arena**: mmap'd once in the ELF `_start` trampoline (before calling the entry
  proc). Base kept in a CALLEE-SAVED register the procs never touch — **r15** (the
  stack machine uses only rax/rcx + the stack + rbp; procs `push rbp/pop rbp`, never
  r15, so r15 survives every `call`/`ret` by the SysV callee-saved guarantee). Node
  count in **r14** (same reasoning), or a fixed slot; r14 is cleaner. Both set in
  `_start` before the entry `call`.
- **entry_size** = fixed per program = `1 tag byte + 8*max_payload_fields`, padded
  to 8 (compute from the ConceptList — the max over all variants). Thread the
  ConceptList into x86_node (ByteGenState gains `concepts`).
- **tag** = `concept_index*256 + variant_index` (same scheme as R6c — self-computed
  from the ConceptList at emit time via variant_tag).

## Slices

### R7a — arena setup + VariantConstruct emit (FIRST)
- `_start` (in elf_program_src): before calling the entry proc, emit
  `mmap(NULL, max_nodes*entry_size, RW, PRIVATE|ANON, -1, 0)` → r15; `xor r14,r14`
  (count = 0). (Reuse the b8 trampoline; insert the mmap prologue.)
- `AstVariant(cstart,clen,vstart,vlen,fields)` in x86_node: the field values are
  already pushed (stack machine, left-to-right). Emit: `node_addr = r15 + r14*entry`;
  write the tag byte `mov byte [node_addr], tag`; pop each field value off the stack
  into the node's payload slots `[node_addr + 1 + 8*i]` (in reverse push order); the
  index (r14) → push it (the variant value); `inc r14`. Needs `code_size` for
  AstVariant too (the emitter threads offsets). entry_size + tag from the ConceptList.
- **Gate**: `out = Token::Num { value: 42 }` (or `Token::Eof`) compiled via
  elf_program_src → run the ELF → prints the node INDEX (**0** for the first
  construct). Proves arena setup + one construct emit. (Field readback is R7b.)

### R7b — MatchVariant emit (dispatch + payload bind)
- `AstMatch(scrut, arms)`: emit scrut (pushes its index); pop index → compute
  node_addr; read tag `movzx eax, byte [node_addr]`; for each arm, `cmp eax, arm_tag;
  je arm_label` (chained, tags from the ConceptList); at each arm, load the payload
  slots `[node_addr + 1 + 8*i]` into the arm binders' rbp LET slots (binders become
  locals in the stack-machine frame — extend the let-slot allocation), emit the arm
  body, `jmp end`. Needs code_size for arms/binders (jump offsets, like AstIf).
- **Gate**: `match build(): Cons(h,t) => h  Nil => 0` where `build()=Cons(5,Nil)` →
  ELF prints **5** (constructs, dispatches, binds h, returns it). Matches R6c oracle.

### R7c — calls/recursion carrying variant indices
- Indices are Numbers → they already pass through the existing AstCall/proc calling
  convention (args pushed, params in rbp slots). Verify recursion works: recursive
  list build + recursive match sum. **Gate**: list-sum ELF prints **6** (build(3)) /
  **15** (build(5)) — the R6c oracle, now COMPILED not interpreted. This is the
  self-hosting-codegen milestone.

Then: records if distinct from variants; text-in-arena (deferred, R6c left strings
as VNum 0 — codegen mirrors).

## Gate discipline (all slices)
The emitted ELF must RUN and print the number the interpreter (R6c) computes for the
same program — the interpreter is the oracle. Plus: scalar programs' ELFs unchanged
(R7 only adds arena setup when the program uses variants; guard so variant-free
programs emit byte-identically). Wrong bytes → the ELF crashes or prints wrong →
caught. Verify from CLEAN disk (run the ELF, compare to --run).

## Risks
- **r15/r14 across calls**: procs must never clobber them (they don't today — audit
  x86_node/x86_proc for any r14/r15 use; there is none). If a future proc used them,
  break. Guard: reserve them program-wide; document.
- **entry_size / tag consistency** between construct and match — one ConceptList,
  one variant_tag/entry_size helper (like R6b/R6c).
- **Arena-free byte-identity**: variant-free programs must not get the mmap prologue
  (guard on "program declares a concept_group / uses AstVariant").
- **code_size for AstVariant/AstMatch** must exactly match the emitted length across
  the two-pass offset threading (the same discipline as AstIf/AstCall today) — a
  mismatch mis-computes every downstream jump/call. This is the sharp edge.

## Honest scope
R7 is the giant + greenfield (arena machine code for a stack machine, in Verbose),
multi-session. R7a (arena + construct) first; R7b (match); R7c (recursion) = the
self-hosted-codegen milestone. The runtime gate (emitted ELF == interpreter) is the
safety net for wrong bytes. Termination verification (R6 leftover) is independent.
