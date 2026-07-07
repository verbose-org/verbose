# Codegen dedup — precompute proc sizes once (O(N²) → O(N))

## The waste (grounded, quadratic)
`proc_offset(rules, target)` walks the rules list summing `proc_size(head)` for each
rule before `target`. `proc_size` walks that proc's ENTIRE body (allocating arena
nodes). `proc_offset` is called at every call-emit site (AstCall rel32) — O(sites) ≈
O(N). So total offset work = O(N × N × body) = **O(N² × body)**, and every proc_size
call re-walks a body it already walked. Measured: ~0.78M redundant arena nodes for
the 174-rule checker; scales ~quadratically → ~7M at 515 rules — plausibly THE wall
between "front end reads its full source" and "compiler emits its full source".

Nothing about the emitted bytes depends on the redundancy — `proc_offset` returns the
same constant however many times it runs (confirmed: 105936, 125×). So this is pure
waste, and removing it is byte-for-byte invisible in the output.

## The fix — one precomputed size list, threaded
1. **`proc_sizes(rules, src, concepts) -> ProcSizeList`** (new): ONE left-to-right
   pass building a cons-list of each rule's `proc_size`, in rule order. Called ONCE.
   (New concept `ProcSizeList` = `PSCons(size : number, rest) | PSNil`.)
2. **`proc_offset` uses the list**: sum precomputed sizes up to the target instead of
   calling `proc_size` per rule. `blob_end_off` (6 sites) sums ALL of them. Same
   numbers; the proc_size body-walk now happens N times total, not O(N²).
3. **Thread `ProcSizeList` through the codegen state** — ByteGenState / ProcGenState /
   the drivers, exactly as `concepts` was threaded (~the same construction sites). The
   list is computed at the top (x86_program / elf_program_src) and passed down; every
   proc_offset/blob_end_off site reads it.

ponytail: the list is still traversed O(N) per offset lookup (cons-list, no index) —
the eliminated cost is the redundant proc_size BODY-WALK (the arena-allocating part),
which drops O(N²)→O(N). A prefix-sum/indexed structure would also cut the traversal,
but the body-walk is the expensive/arena-heavy part; do the high-value half, note the
rest. (`# ponytail: O(N) list traversal per lookup remains; the body re-walk — the
arena cost — is what's removed.`)

## Gate (CLEAN disk)
1. vexprparse verifies; suite green (currently 440 + 1 ignored) + a new test.
2. **BYTE-IDENTICAL** emitted ELFs for a spread of examples (scalar, records, variant
   list-sum, the scanner, the count_rules/checker fragments) — `cmp` vs an
   origin/feat/self-hosting-10 build. This is the core correctness gate: the dedup
   must change nothing in the output.
3. **The win, measured**: emit the 174-rule checker fragment (via the Rust `--native
   --run elf_program_src` path, argv-fed since it's <128KB) under `/usr/bin/time -v`;
   peak RSS drops materially vs pre-dedup (the ~0.78M redundant nodes gone). Report
   the before/after peak — the number that says whether full-source emission now fits.
4. All existing self-compile milestones (front end 515, checker 0/1, scanner) still
   pass unchanged.
5. Regression test (src/native.rs): byte-identity for 2-3 emitted ELFs pre/post
   (compile the same fragment two ways is impossible in one tree — instead assert the
   emitted ELF for a fixed fragment matches a committed golden size/hash, and that
   proc_sizes-based offsets equal a spot-checked proc_offset value).

## Honest scope
Removes the codegen walk's quadratic redundant allocation — the self-hosted compiler
optimizing itself. Byte-identical output (gate). Likely the last structural piece
before vexprparse can EMIT (not just parse) its whole self within the arena; the
follow-on measurement (self-emit a large chunk) tells if the ceiling now clears. Does
NOT touch parse, the emitted binaries' runtime, or any feature surface.
