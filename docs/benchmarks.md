# Benchmarks: verbosec's native emitter vs gcc / rustc / go

This is an honest, reproducible measurement of what a `verbosec`-compiled
native binary actually buys you, per axis, **including where it loses**.
Run it yourself with `./tools/benchmark.sh`. The numbers below were measured
on 2026-06-14 (Ubuntu 24.04, x86-64; gcc 13.3.0, rustc 1.90.0, go 1.25.4,
hyperfine 1.18.0, strace 6.8). Your absolute numbers will differ; the
*ratios* and the *shape* of the result are the point.

## What is and isn't being compared

This benchmarks **verbosec's own native backend** — the hand-written x86-64
emitter in `src/native.rs`, invoked as
`cargo run -- <file>.verbose --native <out> --run <rule>` — against the
mainstream optimizing toolchains: gcc (`-O3 -s -static`), rustc (`-O`), and
go (`go build`).

It is **NOT** "Verbose the language vs C the language." It is
"a no-runtime direct emitter vs LLVM / Go's gc backend." Verbose emits a
freestanding ELF with no libc, no allocator, no runtime, no GC; the others
link a runtime that pays a fixed cost before (and around) user code. Some of
what we measure is a property of *that architectural choice*, not of the
source language. Read every row with that caveat.

The two programs are written in all four languages with the **same logic and
the same output**, and the harness runs a **correctness gate** (every binary
must print the identical value) before any timing — a benchmark of
non-equivalent programs is meaningless.

One honest asymmetry: the Verbose native rule takes its input `N` from `argv`
(one record per argument), while the C/Rust/Go programs compile `N` in as a
literal. The computation is identical (`fib(40)` / the constant 42); only the
*delivery* of `N` differs. The harness feeds the Verbose binary the same `N`
the others hardcode.

## The two programs

### A. `trivial` — the no-runtime axis
Ignore input, compute a constant, print one number, exit. This isolates the
costs a runtime pays before any user code runs: process startup, binary size,
resident memory, and syscall count.

- Verbose: `examples/bench_trivial.verbose` — rule `out = 42`.
- C: `printf("%d\n", 42)` — `gcc -O3 -s -static`.
- Rust: `println!("42")` — `rustc -O`.
- Go: `fmt.Println(42)` — `go build`.

### B. `compute` — the throughput axis
Naive recursive Fibonacci `fib(40)` (`= 102334155`), an exponential call tree
with no memoization. This is a real compute load that stresses code-generation
quality: call/ret overhead, arithmetic, comparison, branch.

- Verbose: `examples/bench_fib.verbose` — `fib(n) = if n.v < 2 then n.v else fib(n-1) + fib(n-2)`, `n : number [0, 45]`.
- C: recursive `long fib(long)` — `gcc -O3 -static`.
- Rust: recursive `fn fib(i64) -> i64` — `rustc -O`.
- Go: recursive `func fib(int64) int64` — `go build`.

`N = 40` is chosen so the *slowest* binary (Verbose) lands around 0.7 s, well
inside a measurable window, and the same `N` is used everywhere.

## Results

### A. `trivial` (constant print + exit)

| lang    | size (B) | wall mean ± sd      | max RSS (kB) | syscalls (total / distinct) |
|---------|---------:|---------------------|-------------:|-----------------------------|
| verbose |      512 | 0.197 ± 0.205 ms    |          360 | 2 / 2                       |
| gcc     |  706 584 | 0.394 ± 0.151 ms    |          664 | 17 / 13                     |
| rustc   | 3 871 984 | 0.607 ± 0.150 ms   |        2 108 | 62 / 22                     |
| go      | 2 254 335 | 1.275 ± 0.277 ms   |        2 080 | 216 / 20                    |

- **Binary size**: verbose 512 B vs gcc-static 706 KB (≈1380×), rustc 3.87 MB
  (≈7560×), go 2.25 MB (≈4400×). gcc is `-static` here for an apples-to-apples
  comparison against Verbose's freestanding ELF; a dynamic `gcc -O3 -s` binary
  is ~14 KB but then depends on `libc.so` at runtime. Verbose depends on
  nothing.
- **Wall-clock (startup-dominated)**: verbose fastest at ~0.2 ms — there is no
  loader work, no runtime init, no `libstd`/GC bring-up to amortize. The error
  bars are wide at this scale (sub-millisecond), so treat this as "Verbose is
  in the lowest startup tier," not a precise multiple.
- **Max RSS**: verbose ~360 kB vs go/rustc ~2 MB. The no-runtime ELF maps only
  its own pages; Go reserves heap + GC structures, Rust pulls in `libstd`.
- **Syscalls**: verbose issues **2** (a `write` and an `exit`); go issues
  hundreds (loader, `mmap`, `rt_sigprocmask`, futexes, scheduler setup).

### B. `compute` (naive recursive `fib(40)` = 102334155)

| lang    | size (B) | wall mean ± sd      | max RSS (kB) | syscalls (total / distinct) |
|---------|---------:|---------------------|-------------:|-----------------------------|
| verbose |      635 | 731.951 ± 21.960 ms |          356 | 2 / 2                       |
| gcc     |  706 584 | 137.716 ± 5.772 ms  |          668 | 17 / 13                     |
| rustc   | 3 872 832 | 226.001 ± 11.365 ms |        2 104 | 62 / 22                     |
| go      | 2 254 624 | 398.208 ± 25.661 ms |        2 160 | 420 / 23                    |

- **Wall-clock (compute-dominated)**: verbose **loses**. It is **5.3× slower
  than gcc -O3**, **3.2× slower than rustc -O**, and **1.8× slower than go**.
  This is the expected result: verbosec has no register allocator and no
  instruction scheduling yet — every recursive call spills its operands
  through the stack rather than keeping them in registers. gcc/LLVM additionally
  apply tail-call and strength-reduction passes a naive emitter does not. This
  gap is the **register-allocation roadmap baseline**, not a defeat to hide.
- **Binary size / RSS / syscalls**: unchanged from the `trivial` story —
  verbose stays ~635 B, ~356 kB RSS, 2 syscalls regardless of how much it
  computes, because none of those costs scale with the workload for a
  no-runtime binary.

## Honest conclusion

**Where the no-runtime architecture wins (by construction):**
- **Binary size** — three to four orders of magnitude smaller than a static
  C/Rust/Go binary, because there is no runtime to link.
- **Startup latency** — lowest tier; no loader/runtime/GC init.
- **Resident memory** — ~5–6× smaller than Go/Rust; the ELF maps only its own
  pages.
- **Syscall count** — 2, versus dozens (gcc) to hundreds (go). A small, easily
  audited kernel-interaction surface.

**Where it loses today (code-generation maturity, not architecture):**
- **Raw compute throughput** — ~5× behind gcc -O3 on naive recursion, ~3×
  behind rustc, ~2× behind go. The fix is a register allocator + instruction
  scheduling; that is on the roadmap. Until then, compute-bound workloads pay a
  ~2–5× tax for the no-runtime properties above.

**What is *not* measured here:** anything that exercises Verbose's actual
differentiators — proof verification, overflow-bound exploitation, the
declared-effect audit surface. This harness deliberately measures only the
backend's raw output (size / startup / memory / syscalls / throughput) on logic
that is identical across languages, so the comparison is fair. The verification
and traceability value lives elsewhere and is not a runtime-performance claim.

## Reproduce

```sh
./tools/benchmark.sh
```

Requires `cargo` + `gcc`. `rustc`, `go`, `hyperfine`, `strace`, and
`/usr/bin/time` are optional — the harness degrades gracefully (skips an axis
or a language with a note) if any is absent.
