# Self-hosted HTTP service — slice 5b.5: one `writev` per log line

A small, self-contained slice that replaces the log block's N per-arg `write()`
calls with ONE `writev()`. Independently valuable (it is the recorded fix for the
S8 fork-interleave problem) and it makes S5c (`on_error: abort`) nearly trivial.
Evidence: NR=native.rs, VP=vexprparse.verbose, main `ee3995e`. All byte claims
objdump-verified against gen0-emitted ELFs.

## Why this before S5c (the reorder, with numbers)
Today the log block emits **one `write()` per concat arg** — objdump of an S2
field-echo server with `concat(req.method," ",req.path,"\n")` shows syscalls at
0x202 (open), 0x217 / 0x23a / 0x24c / 0x26f (four writes), 0x279 (close).
verbosec emits **one** write because `emit_concat_to_buffer` (NR:7877-7896)
materializes the line first. Consequences of the N-write shape:

| | N-write | writev |
|---|---|---|
| S5c checks for the canonical line | **5** (open + 4 writes) | **2** (open + writev) — == verbosec |
| S5c size term | `17 * (1 + nblocks)` → needs a NEW arity walk family | flat **`+34`** — no new walk |
| partial-failure semantics | torn line, then exit — **diverges** from verbosec | all-or-nothing — **parity** |
| fork interleave (S8) | lines interleave across children | one syscall per line |

Building S5c first means writing an arity-dependent check-count walk that reopens
S5b's drift class (its MUST-FIX 10 collapsed three walks into one family for
exactly this reason) AND shipping a policy named `abort` whose partial-failure
behavior diverges from the oracle. `writev` deletes both problems.

## The emit — replace N writes with one writev
`writev(fd, iov, iovcnt)` = syscall **20**; rdi=fd (r15), rsi=iov ptr, rdx=count.
The iovec array is `16*N` bytes (per entry: 8-byte base ptr, 8-byte len),
allocated on the stack BELOW the request buffer and balanced immediately:
```
sub rsp, 16*N                 ; iovec scratch (below [rsp,rsp+max_req) — the
                              ;  request buffer is ABOVE and untouched; the
                              ;  captured (ptr,len) point INTO it and are
                              ;  unaffected. Same balanced pattern as the itoa's
                              ;  sub rsp,0x20.)
<fill entry i for each arg, in source order>
mov rsi, rsp ; mov rdi, r15 ; mov edx, N ; mov eax, 20 ; syscall
add rsp, 16*N
```
Per-arg fill (both forms are FIXED length — no arity dependence beyond N):
- **AstStr arg**: the decoded bytes stay inline (jmp-over-data, unchanged from
  today) — only the *write* is replaced by a fill: `lea rax,[rip-off]` →
  `mov [rsp+16*i], rax` ; `mov qword [rsp+16*i+8], declen`.
- **AstField arg**: `mov [rsp+16*i], r10|rbp` ; `mov [rsp+16*i+8], r8|r9`
  (the S5b capture registers, read directly — no strlen, no copy).
Open and close are unchanged (open keeps `mov r15,rax`; close stays unchecked —
verbosec parity, NR:7816-7824).

## Sizing
`svc_carg_size`'s VERBATIM reuse (S5b's headline win) is lost either way — the
iovec-fill block length differs from the body-write block length, so a
log-specific size walk is needed with or without writev (the scoping was honest
about this; writev avoids the *arity-dependent check count* and the accumulator
threaded through the emit recursion, not the walk itself). Add
`svc_log_iov_size` / `x86_svc_log_iov` as ONE hand-synced pair, mirroring
arm-for-arm (AstStr → fixed fill + inline data block; AstField → fixed fill;
every other arm → the refused-shape fallback, since eager lets still evaluate
them — VP:26276-26278).
`logsz` and `jlog` (VP:29064/29087) remain the SOLE carriers of log size into the
10 branch constants (jump1/2/6, jump1_4/2_4/6_4, jump1_2/2_2, and the two inline
back-jumps), so **zero jump constants change and nothing needs de-baking** — the
growth flows automatically, exactly as `cap` did in S5b.

## Byte-identity
A log-less service emits identically by construction (the whole term is gated on
`svc_log_present`). **Static-log and field-log binaries DO change** (the write
sequence is replaced) — that is the point of the slice. Re-pin the 8
`{S1..S4}×{no-log, static-log}` SHAs and the field-log SHAs; the canonical no-log
931 B / `ff8f9674…` pin (NR:24540) must be **unchanged**.

## Verify
No gate change. `on_error: abort` stays refused (S5c). Content grammar unchanged
(AstStr | req.method|req.path | concat of those). One thing to confirm: N is the
number of *emitted* iovec entries, which must equal the size walk's count on
every shape the gate accepts AND on the refused shapes eager lets evaluate.

## Milestone / test
The observable is unchanged (same bytes in the log file), so the milestone is
**behavioral equivalence + one syscall**:
1. The full S5b matrix re-run: every log row produces byte-identical log content
   and byte-identical wire responses to today.
2. **`strace -f -e trace=write,writev` (or /proc-based counting) shows ONE
   `writev` per request instead of N `write`s** — the actual property being
   bought. If strace isn't available in CI, assert via objdump that the log block
   contains exactly one `0f 05` after the open and before the close.
3. ≥2 requests per spawned server (the S5b back-jump scar — a wrong loop edge
   answers request 1 and SIGSEGVs on request 2).
4. `p_filesz == emitted byte count` on a field-log row (the 1-size→2-emit
   fan-out net).
5. no-log SHA pin unchanged; static/field-log SHAs re-pinned.

## Gate (clean disk)
Proofs check out; suite green; two_generation gen1==gen2 + composite demo; the
matrix above; a hand-run (spawn a logged router, curl it ≥2×, cat the log).

## Honest scope notes
- **Atomicity is NOT upgraded.** `writev` is atomic under the same unproven Linux
  inode-lock property as a single `write` (implementation, not POSIX contract;
  fails over NFS). This is **parity with verbosec**, not a guarantee — do not
  restate it as "fork-safe".
- `writev` is **separable from `concurrency: forked`**: the self-hosted parser has
  NO `concurrency` surface today (the only VP hit is the writev comment itself),
  so S8 proper still needs its own parse work. This slice is the prerequisite that
  makes forked logging *correct*, not the forked slice.
- Also fold in here: delete the stale paragraph in
  `docs/self-hosting-service-slice5b-design.md` (~L106-109) that says to record
  the rbp invariant in CLAUDE.md — it contradicts the corrected framing at ~L75-81
  in the same file. CLAUDE.md:821 is already correct.

## Then S5c becomes (per the scoping, verified)
`svc_log_ok` (VP:13910) `> 1` → `> 2`; `svc_log_block_size` `+ (if on_error == 2
then 34 else 0)`; two 17-byte INLINE stubs (`test rax,rax ; jns +12 ; mov edi,1 ;
mov eax,60 ; syscall`) after open and after writev — the `jns` displacement is the
literal 12 at every site in every branch, so **zero new jump constants**. Milestone:
point the log at `/tmp` (a directory → `open` returns -EISDIR unconditionally, no
privileges needed); assert server exit 1 + empty client reply under `abort`, and
normal service under `drop`. verbosec's behavior confirmed live: curl rc=52, server
exit 1.
