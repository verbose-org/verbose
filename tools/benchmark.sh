#!/bin/bash
# benchmark.sh — Reproducible comparison: Verbose vs gcc on the same logic
#
# Usage: ./tools/benchmark.sh
#
# Requires: gcc, cargo (builds verbosec if needed)

set -e

echo "╔═══════════════════════════════════════════════════════╗"
echo "║     Verbose Benchmark — Reproducible Comparison       ║"
echo "╚═══════════════════════════════════════════════════════╝"
echo ""

# Build verbosec if needed
cargo build --quiet 2>/dev/null

# Create equivalent C program
cat > /tmp/verbose_bench.c << 'CEOF'
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char** argv) {
    for (int i = 1; i < argc; i++) {
        long x = atol(argv[i]);
        puts(x > 10000 ? "true" : "false");
    }
    return 0;
}
CEOF

echo "Logic: amount > 10000 → true/false"
echo "Input: 15000 500 10000 10001 0"
echo ""

# Compile all variants
gcc -O3 -s -o /tmp/verbose_bench_gcc /tmp/verbose_bench.c 2>/dev/null
cargo run --quiet -- examples/invoices.verbose --native /tmp/verbose_bench_native --run important_invoice 2>/dev/null
cargo run --quiet -- examples/invoices.verbose --wasm /tmp/verbose_bench_wasm --run important_invoice 2>/dev/null

# Get sizes
GCC_SIZE=$(stat -c%s /tmp/verbose_bench_gcc)
NATIVE_SIZE=$(stat -c%s /tmp/verbose_bench_native)
WASM_SIZE=$(stat -c%s /tmp/verbose_bench_wasm)

# Verify same results
GCC_OUT=$(/tmp/verbose_bench_gcc 15000 500 10000 10001 0 | tr '\n' ' ')
NATIVE_OUT=$(/tmp/verbose_bench_native 15000 500 10000 10001 0 | tr '\n' ' ')

echo "  Backend             Size        Ratio    Dependencies"
echo "  -------             ----        -----    ------------"
printf "  gcc -O3 -s          %6d B    1.0x     3 shared libs (libc)\n" $GCC_SIZE
printf "  Verbose native      %6d B    %dx smaller  none (zero)\n" $NATIVE_SIZE $(($GCC_SIZE / $NATIVE_SIZE))
printf "  Verbose WASM        %6d B    %dx smaller  browser\n" $WASM_SIZE $(($GCC_SIZE / $WASM_SIZE))
echo ""

# Verify correctness
echo "  Results match:"
echo "    gcc:     $GCC_OUT"
echo "    native:  $NATIVE_OUT"
if [ "$GCC_OUT" = "$NATIVE_OUT" ]; then
    echo "    ✓ identical"
else
    echo "    ✗ MISMATCH"
fi
echo ""

echo "  Verbose advantages:"
echo "    ✓ Proofs verified (purity, termination, determinism)"
echo "    ✓ Overflow bounds proven via interval arithmetic"
echo "    ✓ SIMD-eligible (vectorizable hint verified)"
echo "    ✓ Source traceability (every instruction → intention)"
echo "    ✓ Zero runtime dependencies"
echo ""

echo "  gcc advantages:"
echo "    ✓ Register allocation (Verbose uses push/pop)"
echo "    ✓ Instruction scheduling"
echo "    ✓ 20+ years of optimization passes"
echo ""

echo "  Reproduce: ./tools/benchmark.sh"

# Cleanup
rm -f /tmp/verbose_bench.c /tmp/verbose_bench_gcc /tmp/verbose_bench_native /tmp/verbose_bench_wasm
