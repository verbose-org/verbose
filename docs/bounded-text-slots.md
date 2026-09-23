# Reusing bounded text word slots

The native bounded text emitter now reuses eight-byte scalar, pointer and length
slots after their last emitted use. This extends the existing compile-time reuse
of [text buffers](bounded-text-storage.md). Source contracts, eager evaluation,
alias semantics and buffer ownership remain unchanged. Values retain their full
representation: an i64 still occupies eight bytes, with no compression or decode
step, allocator, reference counting or garbage collection in the executable.

## Placement and lifetime

Every word has a symbolic identity during emission. Each load and store records
the operand to relocate; placement never searches machine-code bytes for matching
patterns. After emission, the compiler assigns nonoverlapping lifetimes the same
physical word and patches only these recorded displacements. Frame reservation
and separately placed buffer addresses use the resulting smaller prefix.

A lifetime starts when its symbolic slot is reserved and ends after its last
emitted load **or store**. Repeated writes at a branch join are included. Branch
arms stay in emitted order, conservatively spanning both alternatives; this is
not full control-flow liveness analysis. Aliases retain the same identity and
every later use extends its lifetime. Input fields start at fragment entry,
even if discovered lazily in the second branch: the wrapper initializes them
together before the body. Every returned field survives until the wrapper has
consumed the result. Slot addresses never escape as language values.

Pointer-word lifetime and payload ownership are separate. Reusing the slot
holding a dead pointer does not release its buffer while another alias still
needs the payload. Existing buffer provenance, branch placement and call-retention
reports continue to use symbolic identities. HTTP status diagnostics also retain
the returned value's facts before relocation, so a dead literal sharing its
physical address cannot become a fact about that result.

Placement orders lifetimes deterministically and reuses the smallest free word.
With S symbolic slots and A accesses, it uses O(S log(S+2) + A) time and O(S+A)
compiler memory. No path enumeration or runtime metadata is introduced. If no
word is saved, original slot numbering is retained. The pre-reuse symbolic-slot
ceiling remains 2 MiB including the existing fixed scratch allowance; the final
frame, including placed buffers and that allowance, has its own 2 MiB check.
Unknown relocations or excessive layouts refuse before artifact emission.

## Measured storage

On 2026-09-23, against main at `dc4dc15dd08e3849cf2530f9bad44d37f4cba0f6`:

| Existing entry | Word slots before → after | Additional entry stack before → after |
|---|---:|---:|
| `text_stack::format_reading` | 72 → 72 | 208 → 208 |
| `text_stack::repeat_reading` | 152 → 104 | 384 → 336 |
| `sequential_stack::label` | 56 → 56 | 192 → 192 |
| `retained_stack::analyze` | 208 → 88 | 408 → 288 |

All sizes are bytes from `--stack-report`. The retained-record example saves
120 bytes (29.4%) of additional entry stack; its 96 bytes of placed buffers and
all call-retention capacities stay unchanged. The existing 408-byte source
budget still passes. An exact 288-byte budget passes and 287 refuses. The repeated
formatter similarly accepts 336 and refuses 335 under the same checked contract.
The report includes outer storage, saved registers and formatting scratch; the
invocation's word-slot prefix alone is not the complete entry bound.

The instruction tracer checks 18 cases across ordinary composition, constructed
inputs and conditional records, including both boolean operators and repeated
records. Invocation frames fall respectively from 280 to 192, 376 to 208 and
440 to 240 bytes. All cases preserve executed instruction counts, evaluation
order/counts, copied bytes/operations and syscalls. Outputs, stderr and exit
status match their oracle; actual word accesses and payload copies stay within
the invocation bounds. Negative controls ensure skipped eager work and an
incorrect branch cannot pass merely by printing the same output.

See the [recorded reports and traces](measurements/bounded-text-slots-2026-09-23.json).
These are stack-layout and instruction-count results, not CPU-time, RSS or cache
measurements. Equal instruction counts do not guarantee equal execution time.

All 184 existing top-level examples retain their native acceptance and diagnostics
(182 accepted, two refused), compiling deterministically twice per compiler.
Of the accepted examples, 173 binaries remain identical; nine bounded text
binaries change, all with unchanged lengths. A byte-diff classification finds
changes only inside frame reservation immediates and word/buffer displacements;
this classification complements the execution checks, not a machine-code proof.

## Scope and reproduction

| Path | Behavior |
|---|---|
| Native bounded text argv, supported stdin/raw/stream paths | Uses the shared fragment's word-slot placement |
| Supported bounded text HTTP handlers and persistent-state copies/logs | Uses the same placement; outer service/state storage remains separate |
| `native_stack` / `--stack-report` | Includes the reduced layout for supported argv entries; existing transport restrictions remain |
| Legacy native and strict numeric lowering | Unchanged; numeric slots already have their own placement |
| Interpreter | Unchanged value semantics; host allocations are outside native budgets |
| WASM and self-hosted compiler | Existing bounded-text/stack-contract refusals remain; no new lowering |

No syntax, arithmetic support, escaping references, recursive calls or nested
record support is added. Existing pure acyclic bounded text contracts remain
the acceptance boundary.

```sh
cargo test -- --test-threads=1
python3 tools/test_stack_budget.py
python3 tools/check_bounded_text_storage.py --check-slots --reference-compiler /path/to/baseline
python3 tools/check_bounded_text_storage.py --check-slots --check-inputs --reference-compiler /path/to/baseline
python3 tools/check_bounded_text_storage.py --check-slots --check-branches --reference-compiler /path/to/baseline
```

The tracer defaults to `target/debug/verbosec` and requires Linux x86-64 with
ptrace permitted. Unit tests execute encoded word operations before and after
relocation with an independent memory oracle, including late inputs, retained
results, dead words, simultaneous values, unregistered literal bytes and HTTP
status facts. Existing integration tests check native/interpreter agreement,
aliases, shadowing, records and branches across the shared emitter's entry modes.
