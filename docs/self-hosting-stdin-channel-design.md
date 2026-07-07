# stdin input channel — the self-hosted front end reads its own full source

## Why (profiled, not assumed)
The runtime-input trampoline (PR #91) marshals argv. But `MAX_ARG_STRLEN` = 128 KiB,
and vexprparse's own source is **846 KB** — argv cannot carry it. Measured. So the
self-hosted front end cannot receive its own complete source through argv; a
file/stdin channel is the confirmed structural need for real-scale self-compile.

Second measured wall: parsing all 509 rules needs ~4.9M arena nodes (extrapolated
from the 174-rule checker's ~1.67M parse peak), over the 4M verifier ceiling. Both
walls must fall together to reach the milestone; each is small.

## Design — two changes

### 1. stdin channel in the emitted ELF (mirror the argv trampoline)
When the entry rule has exactly ONE text input field: read fd 0 to EOF into the
MAP_FIXED region at 0x20000000 (the region already exists for argv text), pack it as
that field's (start,len). Number fields keep coming from argv (declaration order,
argv[k] skipping the text field). So `cat src | ./elf 0` runs the entry with
source=stdin, pos=argv. 846 KB < the 1 MiB region (note the ceiling; bump the region
if a bigger self-source ever needs it).
- Read loop: `read(0, region+bump, region_size - bump)` until it returns 0; bump
  accumulates; the total is the field len. Guard: if bump hits region_size, exit(1)
  (fail-closed — source bigger than the region).
- Choice of channel: if the entry has a text field AND stdin is not a tty... simplest
  deterministic rule: text field ALWAYS from stdin when present; number fields from
  argv. (The self-fragment drivers are `ScanState{source:text, pos:number}` — source
  stdin, pos argv[1].) Document the convention.
- Uses input-field PASS-THROUGH downstream (the entry's text field flows into
  count_rules(ScanState{source: <field>, pos}) — the SUPPORTED shape), sidestepping
  the read()-into-record-arg codegen gap.
- Entry without a text input field: byte-identical to the argv/closed trampolines.

### 2. arena verifier ceiling 4M -> 8M
`src/verifier.rs:1419` PHASE_B1_MAX_NODES 4_000_000 -> 8_000_000. The arena index is
64-bit (mmap-backed) so this is safe; 8M nodes x 104 B = ~832 MB mmap (overcommit,
runtime only). Then bump the self-source's own VExpr `max_nodes` to fit the full
parse (~5M) with headroom. ponytail: 8M is the new ceiling — the real fix for
arbitrary self-source is the codegen-walk dedup (the O(sites*program) redundant
allocation) + eventually a growable/reclaiming arena; 8M buys the 509-rule milestone,
not infinity.

## Gate (CLEAN disk; the interpreter/real count is the oracle)
1. vexprparse verifies; suite green (currently 439 + 1 ignored) + a new test.
2. **THE MILESTONE**: a count_rules-closure ELF (entry = count_rules), fed the FULL
   examples/vexprparse.verbose (846 KB) via stdin: `cat examples/vexprparse.verbose |
   ./elf 0` -> **509** (== the real count_rules run through the interpreter/real
   binary on the same file). The self-hosted front end, compiled, processing its
   ENTIRE own source at runtime.
3. Smaller stdin inputs still correct: a 2-rule program via stdin -> 2.
4. argv/closed-main ELFs BYTE-IDENTICAL (stdin path only when entry has a text field
   and only changes that trampoline variant).
5. region-overflow (source > 1 MiB) -> exit 1.
6. Regression test (src/native.rs): full-source-via-stdin -> 509; small stdin -> 2;
   byte-identity.

## Honest scope
Unblocks the front end on its FULL source (the two measured walls). NOT the whole
self-compile: EMITTING (not just parsing) the full source adds the codegen walk's
~0.78M+ redundant nodes on top of parse — that needs the dedup chantier, a separate
slice. After this: vexprparse's front end reads and processes all 846 KB of itself.
