# Runtime text input — self-compiled ELFs take argv like verbosec's do

## Why
Every self-fragment so far runs through a CLOSED wrapper main (input embedded as a
literal). The fail-closed newline rejection (PR #90) correctly killed multi-line
source-as-literal — so the front-end self-fragment (the 146-rule count_rules
closure) cannot be driven at all: its input IS a multi-line program. The unlock:
the EMITTED ELF reads its input from argv — the SAME CLI convention as verbosec's
native binaries (one argv per input field: text fields raw, number fields decimal).
`./elf "rule a\n..." 0` — self-compiled count_rules on real input.

## Design — two compile-time-constant moves (again)

### 1. The argv-text region: MAP_FIXED keeps the span model intact
A packed text span is (start << 32 | len) with bytes at `src_base + start`
(src_base = 0x400000 + blob_end_off, an imm64). Argv bytes live at a runtime
address — UNLESS the trampoline COPIES them to a FIXED address: mmap(0x20000000,
1 MiB, RW, MAP_FIXED|PRIVATE|ANON). Then an argv text value = pack((0x20000000 -
src_base) + bump, len) — a compile-time-constant offset. byte_at / length /
substring / streaming writes: ALL UNCHANGED (they compute src_base + start).
The region is ours (tiny non-PIE fixed-base ELF; nothing else lives there).

### 2. Trampoline argv marshalling (only when the entry rule HAS an input)
Entry rule with a non-empty rule_params_of (the input:-block bridge supplies it):
the trampoline, before `call entry`:
- Linux entry stack: [rsp]=argc, [rsp+8]=argv0, [rsp+16]=argv1, …
- mmap the fixed region (once; only when any text field exists).
- Per field, in declaration order (argv[k+1] for field k — verbosec's convention):
  * text field → strlen(argv[k+1]) (byte loop), rep movsb copy into the region at
    the current bump, push pack(region_off + bump, len), bump += len.
  * number field → inline atoi (val = val*10 + d loop; leading '-' supported),
    push.
- `call entry` — the args sit exactly like a call site's (x86_args order); the
  callee (via the bridge) pops them into its param slots. Entry rules WITHOUT
  input keep the closed-main trampoline BYTE-IDENTICAL.
- argc guard: argc < 1 + nfields → exit(1) (fail-closed, mirrors verbosec).

### 3. The oracle shifts: the REAL binary is ground truth
eval_main cannot host multi-line inputs (literals rejected — correctly). For
self-fragments the oracle is the REAL rule compiled by verbosec: fragment-ELF(X)
== real-binary(X) for the same argv. (eval remains the oracle for single-line
cases — cross-check where possible.)

## Gate (CLEAN disk; probes python-written)
1. vexprparse verifies; suite green (currently 437 + 1 ignored) + a new test.
2. **Scanner on argv**: the verbatim scan_word fragment with ENTRY = word_length
   (NO wrapper main): `./elf "hello world" 0` → 5; `"abc" 0` → 3; `"  x" 0` → 0 —
   each == the REAL examples/scan_word binary AND == eval (single-line).
3. **THE MILESTONE — the self-compiled FRONT END runs on real input**: the
   146-rule count_rules closure fragment, ENTRY = count_rules:
   `./elf "$(two-rule program)" 0` → **2** == the REAL count_rules driver;
   1-rule → 1; "1 + 2" → 0. vexprparse's front end, compiled by vexprparse,
   parsing real multi-line source at runtime.
4. Closed-main programs (all existing milestones incl. streaming/print_chain):
   BYTE-IDENTICAL ELFs (marshalling only emitted when the entry has an input).
5. argc-missing → exit 1 (no garbage).

## Honest scope
Codegen-only (trampoline + region; the bridge already handles params inside).
Deferred: number bounds-checks on argv (verbosec emits them; the toy trusts —
note it), inputs beyond text/number fields (records-as-argv: no), eval-side argv
(the real binary serves as oracle instead). After this: the front end self-runs;
the remaining self-compile tiers are Result/collections/bytes-concat emitters.
