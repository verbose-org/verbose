#!/bin/bash
# report.sh — Compile and run a full payroll report from native binaries.
#
# Each rule compiles to its own binary (400-700 B, zero dependencies).
# The shell script composes them into a human-readable report.
# This is the UNIX philosophy applied to Verbose.
#
# Usage:
#   ./report.sh                      # uses built-in sample data
#   ./report.sh 3 Alice 60000 Bob 45000 Carol 90000  # custom data

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$SCRIPT_DIR/payroll_report.verbose"
BUILD_DIR="${TMPDIR:-/tmp}/verbose-payroll-report"

# Sample data if no args given
if [ $# -eq 0 ]; then
  set -- 3 Alice 60000 Bob 45000 Carol 90000
fi

# Extract employee count and data
N=$1
shift
ARGS="$@"

# Build directory
mkdir -p "$BUILD_DIR"

# Compile each rule (only if source is newer than binary)
VERBOSEC="${VERBOSEC:-cargo run --quiet --}"

compile_rule() {
  local rule=$1
  local out="$BUILD_DIR/$rule"
  $VERBOSEC "$SRC" --native "$out" --run "$rule" 2>/dev/null
}

compile_rule bonus_line
compile_rule high_earner_count
compile_rule total_salaries
compile_rule roster_line

# Generate the report
echo "╔═══════════════════════════════════════╗"
echo "║         PAYROLL REPORT                ║"
echo "╚═══════════════════════════════════════╝"
echo ""

echo "  Employees: $N"
echo ""

echo "  Bonuses (10%):"
# bonus_line takes individual Employee records (name, salary)
i=0
while [ $i -lt $((N * 2)) ]; do
  name=$(echo "$ARGS" | cut -d' ' -f$((i + 1)))
  salary=$(echo "$ARGS" | cut -d' ' -f$((i + 2)))
  "$BUILD_DIR/bonus_line" "$name" "$salary"
  i=$((i + 2))
done
echo ""

echo "  Summary:"
total=$("$BUILD_DIR/total_salaries" "$N" $ARGS)
high=$("$BUILD_DIR/high_earner_count" "$N" $ARGS)
roster=$("$BUILD_DIR/roster_line" "$N" $ARGS)
echo "    Total salaries:     $total"
echo "    High earners (>50k): $high"
echo "    Roster: $roster"
echo ""

echo "  Binary sizes:"
for rule in bonus_line high_earner_count total_salaries roster_line; do
  size=$(stat -c%s "$BUILD_DIR/$rule")
  printf "    %-20s %4d B\n" "$rule" "$size"
done
echo ""
echo "  All binaries: zero dependencies, no libc, no heap."
echo "  Reproduce: $0"
