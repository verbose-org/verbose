#!/bin/bash
# validate.sh — Invoice validation from a single native binary.
#
# Two rules compiled into ONE ELF (~1.6 KB, zero dependencies):
#   1. validated: Ok(amount) → stdout, Err("[client] reason") → stderr
#   2. classify:  "client: small/medium/large" → stdout
#
# Usage:
#   ./validate.sh                              # sample data
#   ./validate.sh Acme 5000 BadCo 0 BigCorp 200000 SmallShop 500

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$SCRIPT_DIR/invoice_validator.verbose"
BIN="${TMPDIR:-/tmp}/verbose-invoice-validator"

if [ $# -eq 0 ]; then
  set -- Acme 5000 BadCo 0 BigCorp 200000 SmallShop 500
fi

VERBOSEC="${VERBOSEC:-cargo run --quiet --}"
$VERBOSEC "$SRC" --native "$BIN" --run validated,classify 2>/dev/null

SIZE=$(stat -c%s "$BIN")

# Run: stdout gets Ok values + classification, stderr gets enriched errors.
STDOUT=$("$BIN" "$@" 2>/tmp/verbose-inv-err)
ERRORS=$(cat /tmp/verbose-inv-err)

# Split stdout: first N lines are validated Ok values, rest are classify labels.
# Count: each invoice is 2 argv tokens (client + amount).
N=$(( $# / 2 ))
VALID_COUNT=$(echo "$STDOUT" | head -n "$N" | grep -c '^[0-9]' || true)
CLASSIFY=$(echo "$STDOUT" | tail -n "$N")

echo "╔═══════════════════════════════════════╗"
echo "║      INVOICE VALIDATION REPORT        ║"
echo "╚═══════════════════════════════════════╝"
echo ""
echo "  Invoices: $N"
echo ""
echo "  Validation (Ok → stdout, Err → stderr):"
if [ -n "$(echo "$STDOUT" | head -n "$N" | grep '^[0-9]')" ]; then
  echo "    Valid amounts:"
  echo "$STDOUT" | head -n "$N" | grep '^[0-9]' | while read -r line; do echo "      $line"; done
fi
if [ -n "$ERRORS" ]; then
  echo "    Errors:"
  echo "$ERRORS" | while read -r line; do echo "      $line"; done
fi
echo ""
echo "  Classification:"
echo "$CLASSIFY" | while read -r line; do echo "    $line"; done
echo ""
echo "  Single binary: $SIZE B (zero dependencies, no libc, no heap)"
echo "  Reproduce: $0 $@"
