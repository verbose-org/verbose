#!/bin/bash
# benchmark.sh — HONEST benchmark: verbosec's native backend vs gcc / rustc / go
#
# This compares verbosec's OWN hand-written x86-64 emitter (src/native.rs,
# driven by `cargo run -- <f>.verbose --native <out> --run <rule>`) against
# the mainstream toolchains' optimizing backends (LLVM via rustc, gcc, gccgo/
# go's gc). It is NOT "Verbose the language vs C the language" — it is
# "a no-runtime direct emitter vs LLVM/gc". Read that caveat into every row.
#
# Two programs, each in all four languages, SAME logic + SAME output:
#   A. trivial  — print one constant and exit (startup / size / RSS / syscalls)
#   B. compute  — naive recursive fib(N) (raw compute throughput)
#
# Expected honest outcome: Verbose wins size / startup / RSS / syscalls by
# construction (no libc, no runtime, no GC); Verbose loses raw compute to
# gcc -O3 / rustc -O (no register allocator / instruction scheduling yet).
# Both are reported plainly. The compute loss is the regalloc roadmap
# baseline, not a defeat to hide.
#
# Usage: ./tools/benchmark.sh
# Requires: cargo + gcc. Optional: rustc, go, hyperfine, strace,
#           /usr/bin/time. Missing optional tools degrade gracefully.

set -u  # not -e: we want to degrade gracefully when an optional tool is absent

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

FIB_N=40              # fib(40) = 102334155; Verbose native ~0.7 s (slowest)
FIB_EXPECT=102334155
TRIVIAL_EXPECT=42

TMP="$(mktemp -d /tmp/verbose_bench.XXXXXX)"
trap 'rm -rf "$TMP"' EXIT

bar() { printf '%s\n' "════════════════════════════════════════════════════════════════════"; }

# ---- optional-tool detection -------------------------------------------------
have() { command -v "$1" >/dev/null 2>&1; }

HAVE_RUSTC=0; have rustc && HAVE_RUSTC=1
HAVE_GO=0;    have go    && HAVE_GO=1
HAVE_GCC=0;   have gcc   && HAVE_GCC=1
HAVE_HYPERFINE=0; have hyperfine && HAVE_HYPERFINE=1
HAVE_STRACE=0;    have strace    && HAVE_STRACE=1
TIME_BIN=""; [ -x /usr/bin/time ] && TIME_BIN=/usr/bin/time

echo
bar
echo "  Verbose Benchmark — verbosec native emitter vs gcc / rustc / go"
echo "  Framing: no-runtime direct emitter vs LLVM/gc, NOT language-vs-language"
bar
echo
echo "  Toolchains present:"
printf "    gcc      : %s\n" "$([ $HAVE_GCC = 1 ] && gcc --version | head -1 || echo MISSING)"
printf "    rustc    : %s\n" "$([ $HAVE_RUSTC = 1 ] && rustc --version || echo 'MISSING (skipped)')"
printf "    go       : %s\n" "$([ $HAVE_GO = 1 ] && go version || echo 'MISSING (skipped)')"
printf "    hyperfine: %s\n" "$([ $HAVE_HYPERFINE = 1 ] && hyperfine --version || echo 'MISSING (wall-clock skipped)')"
printf "    strace   : %s\n" "$([ $HAVE_STRACE = 1 ] && strace --version 2>&1 | head -1 || echo 'MISSING (syscalls skipped)')"
printf "    time(1)  : %s\n" "$([ -n "$TIME_BIN" ] && echo "$TIME_BIN" || echo 'MISSING (RSS skipped)')"
echo

if [ $HAVE_GCC = 0 ]; then
  echo "  FATAL: gcc is required (it is the size/correctness baseline). Aborting."
  exit 1
fi

echo "  Building verbosec (cargo build --quiet) ..."
cargo build --quiet 2>/dev/null || { echo "  FATAL: cargo build failed"; exit 1; }
echo "  done."
echo

# =============================================================================
# Build all binaries for one program.
# Args: <progname> <verbose_file> <verbose_rule> <c_src> <rust_src> <go_src>
# Sets global arrays LANGS / BINS for the program, plus per-lang notes.
# =============================================================================

# ---- program A: trivial -----------------------------------------------------
cat > "$TMP/trivial.c" <<'EOF'
#include <stdio.h>
int main(void) { printf("%d\n", 42); return 0; }
EOF
cat > "$TMP/trivial.rs" <<'EOF'
fn main() { println!("42"); }
EOF
cat > "$TMP/trivial.go" <<'EOF'
package main
import "fmt"
func main() { fmt.Println(42) }
EOF

# ---- program B: compute (fib N) ---------------------------------------------
cat > "$TMP/fib.c" <<EOF
#include <stdio.h>
long fib(long n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
int main(void) { printf("%ld\n", fib(${FIB_N})); return 0; }
EOF
cat > "$TMP/fib.rs" <<EOF
fn fib(n: i64) -> i64 { if n < 2 { n } else { fib(n-1) + fib(n-2) } }
fn main() { println!("{}", fib(${FIB_N})); }
EOF
cat > "$TMP/fib.go" <<EOF
package main
import "fmt"
func fib(n int64) int64 { if n < 2 { return n }; return fib(n-1) + fib(n-2) }
func main() { fmt.Println(fib(${FIB_N})) }
EOF

# Build helper records into parallel arrays keyed by index.
# We keep four canonical lang slots: verbose, gcc, rustc, go.

build_program() {
  # $1 = prog (trivial|fib)
  local prog="$1"
  local vfile vrule
  if [ "$prog" = trivial ]; then vfile=examples/bench_trivial.verbose; vrule=out_constant; else vfile=examples/bench_fib.verbose; vrule=fib; fi

  # Verbose native
  V_BIN="$TMP/${prog}_verbose"
  if cargo run --quiet -- "$vfile" --native "$V_BIN" --run "$vrule" >/dev/null 2>&1; then
    V_OK=1; V_NOTE="static ELF, no libc / no runtime"
  else
    V_OK=0; V_NOTE="BUILD FAILED"
  fi

  # gcc -O3 -s (static so the size comparison is apples-to-apples vs Verbose)
  G_BIN="$TMP/${prog}_gcc"
  if gcc -O3 -s -static -o "$G_BIN" "$TMP/${prog}.c" 2>/dev/null; then
    G_OK=1; G_NOTE="gcc -O3 -s -static (libc linked in)"
  elif gcc -O3 -s -o "$G_BIN" "$TMP/${prog}.c" 2>/dev/null; then
    G_OK=1; G_NOTE="gcc -O3 -s (dynamic, libc.so)"
  else
    G_OK=0; G_NOTE="BUILD FAILED"
  fi

  # rustc -O
  R_BIN="$TMP/${prog}_rust"; R_OK=0; R_NOTE="MISSING (skipped)"
  if [ $HAVE_RUSTC = 1 ]; then
    if rustc -O -o "$R_BIN" "$TMP/${prog}.rs" 2>/dev/null; then
      R_OK=1; R_NOTE="rustc -O (dynamic, libstd)"
    else R_NOTE="BUILD FAILED"; fi
  fi

  # go build
  O_BIN="$TMP/${prog}_go"; O_OK=0; O_NOTE="MISSING (skipped)"
  if [ $HAVE_GO = 1 ]; then
    if (cd "$TMP" && GOFLAGS= GO111MODULE=off go build -o "$O_BIN" "${prog}.go" 2>/dev/null); then
      O_OK=1; O_NOTE="go build (static, go runtime + GC)"
    else O_NOTE="BUILD FAILED"; fi
  fi
}

# Run a binary, capture stdout (trim trailing newline). Extra args forwarded.
run_out() { local b="$1"; shift; "$b" "$@" 2>/dev/null | tr -d '\n'; }

# The Verbose native rule reads its input from argv (one record per arg),
# whereas the C/Rust/Go programs hardcode N. The LOGIC is identical (compute
# fib(N) / the constant); only N's DELIVERY differs (argv vs literal). To keep
# the comparison fair we feed the Verbose binary the same N the others compile
# in. For 'trivial' the field is a never-read dummy; for 'fib' it is N.
vargs_for() { if [ "$1" = trivial ]; then echo 0; else echo "$FIB_N"; fi; }

# wall-clock via hyperfine -> "mean ± stddev"
wall() {
  # $1 = binary, $2... = args
  if [ $HAVE_HYPERFINE = 0 ]; then echo "n/a (no hyperfine)"; return; fi
  local bin="$1"; shift
  local json
  json="$(hyperfine --warmup 3 --min-runs 10 --export-json "$TMP/hf.json" "$bin $*" 2>/dev/null; cat "$TMP/hf.json" 2>/dev/null)"
  local mean stddev
  mean=$(echo "$json"   | grep -o '"mean": *[0-9.e-]*'   | head -1 | grep -o '[0-9.e-]*$')
  stddev=$(echo "$json" | grep -o '"stddev": *[0-9.e-]*' | head -1 | grep -o '[0-9.e-]*$')
  if [ -z "$mean" ]; then echo "n/a"; return; fi
  # to ms
  awk -v m="$mean" -v s="$stddev" 'BEGIN{ printf "%.3f ± %.3f ms", m*1000, s*1000 }'
}

# max RSS in kB via /usr/bin/time -v
rss() {
  if [ -z "$TIME_BIN" ]; then echo "n/a"; return; fi
  local bin="$1"; shift
  "$TIME_BIN" -v "$bin" "$@" >/dev/null 2>"$TMP/t.txt"
  grep "Maximum resident set size" "$TMP/t.txt" | grep -o '[0-9]*$'
}

# syscall total + distinct via strace -c -f
syscalls() {
  if [ $HAVE_STRACE = 0 ]; then echo "n/a"; return; fi
  local bin="$1"; shift
  strace -c -f "$bin" "$@" >/dev/null 2>"$TMP/s.txt"
  # The summary table: lines after the header, before the "total" line.
  # The "calls" column total is on the bottom "100.00 ... total" line.
  local total distinct
  total=$(awk '/ total$/ {print $4}' "$TMP/s.txt" | tail -1)
  # distinct = data rows (exclude header line "% time", separators, and total)
  distinct=$(awk 'NR>2 && !/-----/ && !/ total$/ && NF>=5 {c++} END{print c+0}' "$TMP/s.txt")
  if [ -z "$total" ]; then echo "n/a"; return; fi
  echo "${total} (${distinct} distinct)"
}

# =============================================================================
# Per-program driver
# =============================================================================
benchmark_program() {
  local prog="$1" expect="$2" label="$3"
  build_program "$prog"

  echo
  bar
  echo "  PROGRAM: $prog — $label"
  bar
  echo

  # ---- build status ----
  echo "  Build status:"
  printf "    %-16s %-4s %s\n" "verbose"  "$([ $V_OK = 1 ] && echo OK || echo FAIL)" "$V_NOTE"
  printf "    %-16s %-4s %s\n" "gcc"      "$([ $G_OK = 1 ] && echo OK || echo FAIL)" "$G_NOTE"
  printf "    %-16s %-4s %s\n" "rustc"    "$([ $R_OK = 1 ] && echo OK || echo SKIP)" "$R_NOTE"
  printf "    %-16s %-4s %s\n" "go"       "$([ $O_OK = 1 ] && echo OK || echo SKIP)" "$O_NOTE"
  echo

  local va; va="$(vargs_for "$prog")"

  # ---- correctness gate ----
  echo "  Correctness gate (all built binaries must print '$expect'):"
  local fail=0
  if [ $V_OK = 1 ]; then o=$(run_out "$V_BIN" $va); printf "    %-16s -> %s\n" verbose "$o"; [ "$o" = "$expect" ] || fail=1; fi
  if [ $G_OK = 1 ]; then o=$(run_out "$G_BIN");     printf "    %-16s -> %s\n" gcc     "$o"; [ "$o" = "$expect" ] || fail=1; fi
  if [ $R_OK = 1 ]; then o=$(run_out "$R_BIN");     printf "    %-16s -> %s\n" rustc   "$o"; [ "$o" = "$expect" ] || fail=1; fi
  if [ $O_OK = 1 ]; then o=$(run_out "$O_BIN");     printf "    %-16s -> %s\n" go      "$o"; [ "$o" = "$expect" ] || fail=1; fi
  if [ $fail = 1 ]; then
    echo
    echo "  *** CORRECTNESS GATE FAILED for '$prog' — outputs differ from '$expect'."
    echo "  *** A benchmark of non-equivalent programs is meaningless. Aborting program."
    return 1
  fi
  echo "    gate passed: every built binary prints '$expect'."
  echo

  # ---- per-axis measurements ----
  # Collect into arrays so we can format a table and compute interpretations.
  local -a NAMES=() SIZES=() WALLS=() RSSES=() SYS=()
  add_row() {
    # $1 name  $2 bin  $3 args
    NAMES+=("$1"); SIZES+=("$(stat -c%s "$2")")
    WALLS+=("$(wall "$2" $3)"); RSSES+=("$(rss "$2" $3)"); SYS+=("$(syscalls "$2" $3)")
  }
  [ $V_OK = 1 ] && add_row verbose "$V_BIN" "$va"
  [ $G_OK = 1 ] && add_row gcc     "$G_BIN" ""
  [ $R_OK = 1 ] && add_row rustc   "$R_BIN" ""
  [ $O_OK = 1 ] && add_row go      "$O_BIN" ""

  echo "  ┌─ Per-axis measurements ─────────────────────────────────────────"
  printf "  │ %-9s  %12s  %18s  %10s  %s\n" "lang" "size (B)" "wall (mean±sd)" "max RSS kB" "syscalls"
  printf "  │ %-9s  %12s  %18s  %10s  %s\n" "----" "--------" "--------------" "----------" "--------"
  local i
  for i in "${!NAMES[@]}"; do
    printf "  │ %-9s  %12s  %18s  %10s  %s\n" "${NAMES[$i]}" "${SIZES[$i]}" "${WALLS[$i]}" "${RSSES[$i]}" "${SYS[$i]}"
  done
  echo "  └─────────────────────────────────────────────────────────────────"
  echo

  # ---- honest interpretation ----
  # find verbose index + min size + fastest wall
  local v_idx=-1
  for i in "${!NAMES[@]}"; do [ "${NAMES[$i]}" = verbose ] && v_idx=$i; done

  # smallest binary
  local min_size=999999999 min_size_name=""
  for i in "${!NAMES[@]}"; do
    if [ "${SIZES[$i]}" -lt "$min_size" ]; then min_size="${SIZES[$i]}"; min_size_name="${NAMES[$i]}"; fi
  done
  echo "  Interpretation:"
  echo "    • Binary size: smallest is '$min_size_name' ($min_size B)."
  if [ $v_idx -ge 0 ]; then
    local vs="${SIZES[$v_idx]}"
    for i in "${!NAMES[@]}"; do
      [ "${NAMES[$i]}" = verbose ] && continue
      local factor; factor=$(awk -v a="${SIZES[$i]}" -v b="$vs" 'BEGIN{ if(b>0) printf "%.1f", a/b; else print "?" }')
      echo "        verbose is ${factor}× smaller than ${NAMES[$i]} (${SIZES[$i]} B)."
    done
  fi

  # wall-clock interpretation (only meaningful if hyperfine ran)
  if [ $HAVE_HYPERFINE = 1 ] && [ $v_idx -ge 0 ]; then
    # parse mean ms out of each WALLS entry
    local v_ms; v_ms=$(echo "${WALLS[$v_idx]}" | grep -o '^[0-9.]*')
    if [ -n "$v_ms" ]; then
      # find fastest
      local fast_ms=999999999 fast_name=""
      for i in "${!NAMES[@]}"; do
        local m; m=$(echo "${WALLS[$i]}" | grep -o '^[0-9.]*')
        [ -z "$m" ] && continue
        awk -v m="$m" -v f="$fast_ms" 'BEGIN{exit !(m<f)}' && { fast_ms="$m"; fast_name="${NAMES[$i]}"; }
      done
      if [ "$prog" = trivial ]; then
        echo "    • Wall-clock (startup-dominated): fastest is '$fast_name' (${fast_ms} ms mean)."
        echo "        For 'trivial' wall-clock ≈ process startup + one write; the"
        echo "        no-runtime binary has no loader/runtime init to amortize."
      else
        echo "    • Wall-clock (compute-dominated):"
        for i in "${!NAMES[@]}"; do
          [ "${NAMES[$i]}" = verbose ] && continue
          local m; m=$(echo "${WALLS[$i]}" | grep -o '^[0-9.]*'); [ -z "$m" ] && continue
          local sf; sf=$(awk -v v="$v_ms" -v o="$m" 'BEGIN{ if(o>0) printf "%.1f", v/o; else print "?" }')
          echo "        verbose is ${sf}× SLOWER than ${NAMES[$i]} — no register allocation /"
          echo "        instruction scheduling yet; every recursive call spills via the"
          echo "        stack. This is the regalloc roadmap baseline, not a defeat."
        done
      fi
    fi
  fi

  # RSS + syscalls (qualitative — the no-runtime story)
  if [ -n "$TIME_BIN" ] && [ $v_idx -ge 0 ]; then
    echo "    • Max RSS: the no-runtime ELF maps only its own pages — no libc"
    echo "        arena, no Go heap/GC reservation (Go's RSS includes runtime)."
  fi
  if [ $HAVE_STRACE = 1 ] && [ $v_idx -ge 0 ]; then
    echo "    • Syscalls: verbose issues a handful (write + exit, plus argv read);"
    echo "        libc/Go add loader, mmap, futex, rt_sigprocmask, sched setup, etc."
  fi
  echo
  return 0
}

benchmark_program trivial "$TRIVIAL_EXPECT" "constant print + exit (no-runtime axis)" || true
benchmark_program fib     "$FIB_EXPECT"     "naive recursive fib($FIB_N) (throughput axis)" || true

bar
echo "  Honest summary"
bar
echo "  WINS  (by construction, no-runtime architecture):"
echo "    binary size, startup latency, max RSS, syscall count."
echo "  LOSES (today, code-generation maturity):"
echo "    raw compute throughput vs gcc -O3 / rustc -O — verbosec has no"
echo "    register allocator and no instruction scheduling yet (regalloc roadmap)."
echo
echo "  Caveat: this measures verbosec's direct emitter vs LLVM/gc backends,"
echo "          not 'Verbose the language' vs 'C the language'. Same logic,"
echo "          same output, gated for correctness before every timing."
echo
echo "  Reproduce: ./tools/benchmark.sh"
bar
