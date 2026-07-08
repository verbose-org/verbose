# Raw-stdin channel (Rust backend) — clean 855 KB feed for the fixed-point attempt

## Why
Reaching the fixed point (elf_program_src emitting its own 855 KB source) needs the
compiled emitter to RECEIVE 855 KB raw. Every current channel is confounded:
- `--native --run --stdin` = the TOKEN reader (whitespace-splits, builds argv on the
  stack) → ~88 KB source = thousands of tokens → stack overflow (rc 139). Wrong tool.
- argv caps at 128 KB (MAX_ARG_STRLEN).
- `emit_self` (read-let → nested elf_program_src call) faults on its composition
  even at 35 bytes — a separate runtime bug, and an unnecessary wrapper.

None feeds a raw multi-KB text blob into the entry's text field. The clean tool: a
RAW-stdin mode in the Rust `--native` backend — read fd 0 to EOF into the entry
rule's single text field, verbatim, no tokenizing, no cap. This is exactly the
self-hosted stdin channel (x86_stdin_marshal in vexprparse.verbose, PR #93) but
emitted by the Rust backend; use it as the reference.

## Design
New flag `--stdin-raw` (distinct from the token `--stdin`). When set and the entry
rule's record input has exactly ONE text field:
- Emit a prologue (before the rule body) that mmaps a large RW buffer (e.g. anon
  MAP, 4 MiB — note the ceiling), reads fd 0 to EOF into it (read loop, accumulate,
  fail-closed on read<0 or buffer-full), and sets the entry's text field to
  `(buffer_ptr, total_bytes_read)`.
- Number fields (if any) come from argv, declaration order (as the argv trampoline
  does). ScanState = {source:text, pos:number} → source=stdin-raw, pos=argv[1].
- Reuse the existing field-slot/record setup the argv/token prologues use; only the
  text field's source changes (mmap+read-loop vs token/argv).

Mirror the self-hosted x86_stdin_marshal for the exact syscall sequence + packing.

## Gate
1. suite green (441/442 baseline) + a new test. Non-`--stdin-raw` compiles unchanged.
2. **Correctness**: `cat prog | ./bin` where bin = `--native --run count_rules
   --stdin-raw`: a 2-rule program → 2; the FULL vexprparse.verbose (855 KB) → 518
   (== grep). count_rules already proved it can parse 855 KB (PR #93 via the
   self-hosted channel) — this reproduces it via the Rust channel, cleanly.
3. **THE FIXED-POINT ATTEMPT**: `cat examples/vexprparse.verbose | ./emit` where
   emit = `--native --run elf_program_src --stdin-raw`. Outcome, whichever happens,
   is the result:
   - EMITS a non-empty ELF → THE FIXED POINT (vexprparse emits an ELF for its own
     source). Report the ELF size; check `file` says ELF; if feasible, run it.
   - Faults/walls → report rc + peak RSS (≈ nodes) + the fault address. This is now
     a CLEAN measurement (no token-reader / emit_self confound) — the real arena/
     addressing number for the emit-at-scale path.

## Honest scope
A clean input channel for the compiled emitter — the tool that lets the fixed point
be attempted or measured without confounds. If it emits: the north star. If it
walls: the first UNCONFOUNDED measurement of emit-at-full-scale, scoping the last
bug precisely. Either way, decisive.
