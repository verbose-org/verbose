#!/bin/bash
# report.sh — Compile a full payroll report into ONE native binary.
#
# Four rules compiled into a single ELF (~2 KB, zero dependencies).
# Each rule re-parses the same argv and writes its output to stdout.
# The shell script just wraps the output with headers.
#
# Usage:
#   ./report.sh                      # uses built-in sample data
#   ./report.sh 3 Alice 60000 Bob 45000 Carol 90000  # custom data

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$SCRIPT_DIR/payroll_report.verbose"
BIN="${TMPDIR:-/tmp}/verbose-payroll-report-multi"

# Sample data if no args given
if [ $# -eq 0 ]; then
  set -- 3 Alice 60000 Bob 45000 Carol 90000
fi

N=$1

# Compile (multi-rule: all four rules in one binary)
VERBOSEC="${VERBOSEC:-cargo run --quiet --}"
RULES="bonus_report,total_salaries,high_earner_count,roster_line"
$VERBOSEC "$SRC" --native "$BIN" --run "$RULES" 2>/dev/null

# Run and capture each rule's output (they appear in sequence on stdout)
OUTPUT=$("$BIN" "$@")

# Parse: bonus_report = lines until first number-only line
BONUS=$(echo "$OUTPUT" | sed -n '1,/^[0-9]/{ /^[0-9]/!p }')
TOTAL=$(echo "$OUTPUT" | sed -n '/^[0-9]/{p;q}')
REST=$(echo "$OUTPUT" | sed -n '/^[0-9]/,${/^[0-9]/!{p;q}}' | tail -1)
COUNT=$(echo "$OUTPUT" | sed -n '2{/^[0-9]/p}' | head -1)
# Simpler: just read lines
LINES=()
while IFS= read -r line; do LINES+=("$line"); done <<< "$OUTPUT"

# Last 3 lines are: total, count, roster
NLINES=${#LINES[@]}
ROSTER="${LINES[$((NLINES-1))]}"
COUNT="${LINES[$((NLINES-2))]}"
TOTAL="${LINES[$((NLINES-3))]}"

SIZE=$(stat -c%s "$BIN")

echo "╔═══════════════════════════════════════╗"
echo "║         PAYROLL REPORT                ║"
echo "╚═══════════════════════════════════════╝"
echo ""
echo "  Employees: $N"
echo ""
echo "  Bonuses (10%):"
# Print all lines except the last 3
for ((i=0; i<NLINES-3; i++)); do
  echo "${LINES[$i]}"
done
echo ""
echo "  Summary:"
echo "    Total salaries:      $TOTAL"
echo "    High earners (>50k): $COUNT"
echo "    Roster: $ROSTER"
echo ""
echo "  Single binary: $SIZE B (zero dependencies, no libc, no heap)"
echo "  Reproduce: $0 $@"
