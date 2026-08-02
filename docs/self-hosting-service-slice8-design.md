# Self-hosted HTTP service — slice 8: `concurrency: forked`

Turns the emitted server from sequential into genuinely concurrent: one child per
accepted connection, kernel auto-reaped. Evidence: NR=native.rs,
VP=vexprparse.verbose, main `5ffcb38`. Every byte figure below was objdump-verified
AND validated by hand-patching real emitted servers and running them.

## Why this brick (and why not S5d)
A sequential HTTP server head-of-line-blocks on a single silent client — a demo,
not something you put behind a port. S8 also **retroactively justifies S5b.5**:
measured, 32 processes × 20 lines × 7 pieces to one `O_APPEND` file —
`writev` **0 torn**, the pre-5b.5 N-write shape **387/640 torn**. And S8 is the
CHEAPER slice: flat `+160` with **zero jump-constant edits**, where S5d needs a
`LogBlockList` (a list, not a code) and its `jlog` stops being a single term.

## Emit — three gated splices, ZERO jump-constant edits
Layout (objdump-verified, every shape): tramp base 186; `accept_top` = tramp+137;
accept block 21 B; `mov %rax,%r13` ends at **tramp+158** — the same insertion
point S5a's static log uses.
1. **`rt_sigaction(SIGCHLD, SIG_IGN, NULL, 8)` — 71 B**, in the STARTUP between
   `bind` and `listen` (verbosec NR:18899-18928). Kernel auto-reaps: no
   wait/waitpid, no zombies. Pre-`accept_top`, so both endpoints of every
   back-jump shift equally → no constant changes. **Load-bearing, proven**: a
   fork-without-sigaction variant gave `children=12 zombies=12`.
2. **Fork dispatch — 82 B at tramp+158** (3 B less than verbosec's 83: `mov
   rdi,r13` vs `mov rdi,[rbp-48]`):
```
mov $0x39,%rax ; syscall ; test %rax,%rax
je  +68   → child        ; ⚠ the jz ENDS at fork-block offset +14, not +16,
                         ;   so the rel8 is 68, NOT 66 (a 66 resets every conn)
js  +17   → fork_error
mov $0x3,%rax ; mov %r13,%rdi ; syscall     ; parent closes client fd
e9 <-54>  → accept_top   ; HARD CONSTANT (see placement, below)
fork_error: eb 0c / "fork failed\n" / write(2,…) / e9 <-66> → parent_close
child: <falls through into the existing read>
```
3. **Tail swap**: the 5 B `jmp accept_top` becomes 12 B `mov edi,0 ; mov eax,60 ;
   syscall` (exit(0)) — the mirror of S5c's abort-stub house style. **+7, and it
   SIMPLIFIES**: in forked mode `jump6`, `jump6_4`, `-(87+block+logsz)` and
   `-(470+cap+logsz)` — the four hardest closed-form negatives in the file — are
   never computed.

**Jump arithmetic — verified by enumerating every transfer in all four shapes:
exactly ONE crosses tramp+158 (the tail back-jump to accept_top), and zero
forward jumps do.** So `jump1/2/6`, `jump1_4/2_4/6_4`, `jump1_2/2_2`, `jump3/4/5`
absorb NO `concsz` term, and the one that would is replaced by `exit(0)`. **Zero
de-baking, zero constant edits.** Also verified: no absolute 64-bit references
into the code/blob in any emitted server, so inserting bytes needs only
p_filesz/p_memsz.
Sizing: `concsz = if conc == 2 then 160 else 0`, a sibling of `logsz` in
`service_tramp_size` (VP:29108). Every patched binary measured exactly +160.
Emit: three separate rules (a concat-valued let is refused in this recursive SCC,
VP:29342) — `x86_svc_conc_pre` (71 B), `x86_svc_conc_fork` (82 B), and a tail
selector — spliced identically into all four branches.

## PLACEMENT: fork FIRST, before the static log block
Matches verbosec (fork at NR:19001, log at NR:19394 — it always logs in the
child). It is the only **uniform** rule: field logs are unavoidably post-fork, so
splitting on `hasf` would give inconsistent abort semantics. The parent then does
only accept+fork. And the parent's `jmp accept_top` becomes a hard **−54** instead
of `−(54+logsz)` — the alternative was measured and SIGSEGV'd (rc −11) when the
hardcoded displacement ignored `logsz`.
**Log-atomicity worry evaporates**: the log `open(path, 0x441 =
O_WRONLY|O_CREAT|O_APPEND, 0644)` is INSIDE the loop, so each child opens its own
`struct file` with its own offset — no shared-offset hazard at all. (If a future
slice hoists the open to startup, the inherited-fd case is safe ONLY because of
`O_APPEND` — without it children would overwrite each other. Leave a comment.)

## Parse — `concurrency:` LAST, a 4-valued code, and PACK to keep the arena free
**`concurrency` has no surface in VP today** (two prose comments only, :26641 and
:29065).
**ARENA: MkService is at 12 fields, TIED with MkRule for the group max** →
`entry_size = (1+96+7)&~7 = 104` (gen0's own mmap immediate is 624,000,000 =
6,000,000 × 104). A 13th field pushes it to **112**: +48 MB reservation and ~+1.2
GiB on the ~16 GiB self-compile — S5a's "arena cost is ZERO" note (VP:992) stops
being true at S8.
**MITIGATION (do this, don't widen): merge `log_present` (0/1) and
`log_on_error` (0..3) into `log_code = log_present + 4 * log_on_error`**, freeing
the 12th slot for `concurrency`. `svc_log_present` (VP:11221) becomes
`log_code % 4`, `svc_log_on_error` (VP:11293) becomes `log_code / 4` — **accessor
SIGNATURES UNCHANGED, so zero call sites move.** Both are already codes
(`proto_code`, `oe_code`), so this is house style. (Rejected: packing into
`protocol` — conflates orthogonal axes and forces `% 10` at every
`svc_protocol(h) == 1` consumer.) **This is the riskiest edit in the slice: it
touches shipped S5a/S5c gating. Pin it by re-running every existing log test
before adding S8's.**
`concurrency` itself is a 4-valued code mirroring `oe_code` — 0 absent (→
sequential), 1 `sequential`, 2 `forked`, 3 unrecognized (→ refuse). **No
`_present` flag needed**: it carries no span, so one code covers all states (the
S5a `log_present` flag existed only because a `(start,len)` pair's single −1
can't carry three states).
**POSITION: LAST in the service block** (after `log:`/`on_error:`). The oracles
disagree (static_file_server/body_content_gate/audit_gateway put it last;
policy_proxy puts it early) — last is the only position that perturbs no existing
cursor: one new conditional `cp = if is_log == 0 then gp else
skip_seps_dedent(<after the log block>)`. ⚠ **THE SUBTLETY THAT WILL BITE**:
`on_error` uses `skip_seps` because it is INSIDE the `log:` sub-block;
`concurrency:` is a SIBLING of `handler:`/`log:`, one indent out, so it needs
**`skip_seps_dedent`** (the distinction the `hp` step documents at VP:11364). Get
it wrong and the walk runs into the next top-level declaration — the documented
S5a hazard (VP:11408-11412).
New: `span_is_concurrency` (11 B), `span_is_forked` (6 B), `span_is_sequential`
(10 B) — clones of `span_is_on_error`/`span_is_abort_word`. ~200 lines, mechanical.

## REFUSE `forked` + `on_error: abort` (a security call, not a documentation note)
Measured, log path `/tmp` (→ `-EISDIR`):
| | sequential | forked |
|---|---|---|
| req 1 | empty reply, **server exit 1** | empty reply, **server alive** |
| req 2/3 | ECONNREFUSED (dead) | empty reply, still alive |
The per-request half survives (no log ⇒ no response bytes), but the **systemic
escalation is lost**: the port stays open, so a "is it listening" health check
passes while 100% of requests silently fail — where sequential gives systemd/k8s a
loud `exit 1`. S5c's contract is written in terms of the sequential loop
(VP:29065). Silently weakening a *declared* safety property is exactly what the
compiler axiom forbids, and no byte/size/SHA gate can see it. **REFUSE the
combination** — two lines in `service_errors` (VP:13998), zero emit bytes — and
leave `forked` + `drop` (a best-effort access log on a concurrent server) fully
sound. Lift later with a real escalation mechanism (child signals parent); that is
its own slice. The self-hosted emitter refusing MORE than verbosec is the
established pattern, so this is not a divergence problem.

## Byte-identity
`concsz`/the splices gate on `conc == 2`, so **absent (0) AND explicit
`sequential` (1) both emit byte-identically to today** — that is why the code is
4-valued rather than boolean. Pin as S5c did: for each of the 4 body shapes ×
{no-log, static-log, field-log}, `sha256(sequential) == sha256(pre-S8)` and
`len(forked) == len(sequential) + 160`; `p_filesz == len(bytes)` on a forked row.

## Milestone / tests (all four ran green on hand-patched binaries)
1. **OVERLAP — the assertion sequential CANNOT pass**: open connection A and send
   **nothing** (the child blocks in `read`), then send a full request on B.
   Sequential: TIMEOUT. Forked: `HTTP/1.0 200 OK…`. This is the milestone.
2. **300 parallel** (64-way): 300/300 `200`, and the log has exactly 300 lines
   matching a strict regex (catches torn writes).
3. **Zombies**: `ps --ppid <server>` after 12 requests → 0 children, 0 zombies.
   (Negative control: drop the sigaction → `children=12 zombies=12`.)
4. **Fd leak**: `/proc/<pid>/fd` after 300 conns → 4 (0,1,2 + listen).
5. Standing rules: ≥2 requests/server; `p_filesz == bytes`; `sequential`
   SHA-identical to pre-S8.

## Honest notes for the release
- **Forked is a head-of-line-blocking feature, not a throughput feature.**
  Measured: ~10% slower on a trivial handler (2381→2101 req/s single-worker,
  1581→1460 at 32). Say so in the example prose.
- `writev` atomicity remains the unproven Linux inode-lock property (parity with
  verbosec, not POSIX; not NFS/FUSE). S8 exercises it; it does not upgrade it.
- Pre-existing, not fixed here: the emitted server does not ignore `SIGPIPE` — a
  client that closes mid-response kills the process. Fork IMPROVES this (kills one
  child, not the server). Worth a note, not a slice.
