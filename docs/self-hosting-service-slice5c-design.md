# Self-hosted HTTP service — slice 5c: `on_error: abort` (fail-closed audit)

Makes the service `log:` block fail-closed: on a log syscall failure the server
exits(1) instead of silently serving unlogged requests. This is the Article-12
posture — *no log persisted ⇒ no claim of having served the request*. Evidence:
NR=native.rs, VP=vexprparse.verbose, main `daaca62`. Everything below was
verified by the S5c scoping pass (objdump + a live verbosec oracle run); this doc
transcribes that verified spec.

## Why it is small now
S5b.5's `writev` collapsed the log block from N writes to one, so this slice needs
**2 checks** (open, writev) — matching verbosec exactly (NR:7789-7795 open check,
NR:7809-7814 write check, close deliberately unchecked at NR:7816-7824) — instead
of the 5 the N-write shape would have required, and a **flat** size term instead
of an arity-dependent one needing a new counting walk.

## The four edits
1. **`svc_log_ok` (VP:13910)**: `... else (if sk.on_error > 1 then 0 else 1)` →
   `> 2`. **Code 3 (unrecognized policy word) MUST stay refused.** `oe_code`
   (VP:11425) is 0 = absent, 1 = explicit `drop`, 2 = `abort`, 3 = unrecognized.
2. **`svc_log_block_size` (VP:28708)**: `+ (if svc_log_on_error(h) == 2 then 34
   else 0)`. Flat — 2 stubs × 17 B. Gate on `== 2`, NOT `> 0` (absent and `drop`
   must both contribute 0).
3. **`x86_svc_log_block` (VP:28909)**: splice a 17-byte INLINE stub after the
   `open` syscall and after the `writev` syscall.
4. Nothing else. No new concepts, no new walks, no touched jump constants.

## The stub — inline, not a shared tail
```
48 85 c0          test rax, rax
79 0c             jns  +12            ; ALWAYS exactly 12, every site, every branch
bf 01 00 00 00    mov  edi, 1
b8 3c 00 00 00    mov  eax, 60
0f 05             syscall             ; exit(1)
```
17 B. The 12-byte exit(1) is already the house form — byte-for-byte the
mmap-failure path in every emitted service. Because the `jns` displacement is the
**literal 12**, this adds **zero new jump constants and requires no de-baking**.
Placement detail: the open stub may sit before or after `mov r15,rax` (`mov`
preserves flags).
**REJECTED — a shared end-of-tramp tail.** Verified: there is NO dead space after
the back-jump (`e9 74 fd ff ff` at 0x3ca ends 0x3cf; the next rule proc's `push
%rbp` starts immediately at 0x3cf), so a tail means *extending* the tramp; and the
distance from each log block to the tramp end differs per branch AND per placement
(pre-read vs post-select), so it would need a distinct closed form for each of the
7 splice sites. The self-hosted emitter has no patch list — every displacement must
be closed-form — which is exactly why the inline stub wins.
**The S5b liveness constraint does NOT bind the stub.** That rule ("the log block
can never move later than immediately-after-the-select") exists because the log
block READS r10/r8/rbp/r9. The stub reads nothing — it is `exit(1)` — so it is
placement-free.

## Sizing / byte-identity
`logsz`/`jlog` (VP:29064/29087) remain the sole carriers of log size into the 10
branch constants, so the +34 flows automatically — **no jump constant changes**.
Both the size and emit terms sit inside the existing `svc_log_present` gate, so a
log-less service is byte-identical by construction, and `drop`/absent services are
byte-identical too (the `== 2` gate). The canonical no-log pin (931 B,
`ff8f9674…`) and every static/field-log SHA must be **unchanged** — this slice
only adds bytes when `abort` is explicitly declared.

## Verify
Only the one comparison change (edit 1). verbosec's verifier doesn't inspect
`on_error` either (it is purely a codegen concern; parsed as a closed set at
parser.rs:1843-1859). No content shape or branch needs a carve-out — the stub
travels with the log block, so all 7 splice sites work uniformly including S1's
pre-read placement.

## Two behavioral facts for the release note (both verbosec parity, not divergences)
- **Placement determines WHEN abort fires.** A static log sits pre-read, so abort
  fires after `accept` but before the request is read; a field log fires
  post-parse. Either way the client gets an empty reply.
- **`abort` converts a log failure into a full service outage.** The accept loop is
  sequential and un-forked, so `exit(1)` kills the server, not just the request.
  That IS the fail-closed contract (no log ⇒ no service) — and it is also a DoS
  surface if an attacker can exhaust the log filesystem. verbosec behaves
  identically (confirmed live). Say both halves out loud.

## Milestone / test
**Use `/tmp` (a DIRECTORY) as the log path** — `open(O_WRONLY|O_CREAT|O_APPEND)` on
a directory returns `-EISDIR` unconditionally, needs no privileges, and is stable
across environments (`/proc/1/mem` is not). Three rows:
1. `abort` + unwritable path → **server exit status 1**, client gets an empty
   reply, no log file;
2. the same service with `on_error: drop` → serves normally, **stays alive**, no
   log file;
3. `abort` + writable path → serves normally, stays alive, log line correct.
Plus the standing rules: **≥2 requests per spawned server** (the S5b back-jump
scar); `p_filesz == emitted byte count` on an abort row; no-log/static/field SHAs
unchanged.
Oracle (confirmed live during scoping): verbosec's abort service returns curl
rc=52 (empty reply) and the process exits 1 — the exact assertion pair.
**NEGATIVE CONTROL (required):** make the stub's `jns` skip the wrong distance (or
drop one of the two stubs) and confirm row 1 FAILS — a fail-closed feature that
never failed for the right reason isn't verified. Bonus, per S5b.5's standard: run
the new abort assertion against a **pre-change** ELF and confirm it reports the
wrong result.

## Gate (clean disk)
Proofs check out; suite green; two_generation gen1==gen2 + composite demo; the
three rows above; a hand-run (spawn an abort service on an unwritable path, curl
it, observe exit 1; then a writable one, curl ≥2×, cat the log).

## Deferred
S5d multi-block; S6/S7 read+fetch in handlers (service+resource/connection is
refused outright at VP:13884 — lifting it is structural, not a slice); S8 forked
(needs a new `concurrency` parse surface); S9 state.
