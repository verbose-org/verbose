#!/bin/bash
# validate.sh — Compile and run invoice validation from a single native binary.
#
# Usage:
#   ./validate.sh                              # sample data
#   ./validate.sh Acme 5000 BadCo 0 BigCorp 200000 SmallShop 500

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC="$SCRIPT_DIR/invoice_validator.verbose"
BIN="${TMPDIR:-/tmp}/verbose-invoice-validator"

# Sample data if no args
if [ $# -eq 0 ]; then
  set -- Acme 5000 BadCo 0 BigCorp 200000 SmallShop 500
fi

# Compile
VERBOSEC="${VERBOSEC:-cargo run --quiet --}"

echo "=== Compiling ==="
# Individual binaries for the validation pipeline
$VERBOSEC "$SRC" --native "$BIN-validated" --run validated
$VERBOSEC "$SRC" --native "$BIN-classify" --run classify

SIZE_V=$(stat -c%s "$BIN-validated")
SIZE_C=$(stat -c%s "$BIN-classify")

echo ""
echo "╔═══════════════════════════════════════╗"
echo "║      INVOICE VALIDATION REPORT        ║"
echo "╚═══════════════════════════════════════╝"
echo ""

# Run validation: Ok → stdout, Err → stderr (captured separately)
echo "  Validation (Ok → stdout, Err → stderr):"
VALID=$("$BIN-validated" "$@" 2>/tmp/verbose-inv-err)
ERRORS=$(cat /tmp/verbose-inv-err)
if [ -n "$VALID" ]; then
  echo "    Valid amounts:"
  echo "$VALID" | while read -r line; do echo "      $line"; done
fi
if [ -n "$ERRORS" ]; then
  echo "    Errors:"
  echo "$ERRORS" | while read -r line; do echo "      $line"; done
fi
echo ""

echo "  Classification:"
"$BIN-classify" "$@" | while read -r line; do echo "    $line"; done
echo ""

echo "  Binaries: validated=$SIZE_V B, classify=$SIZE_C B"
echo "  Total: $((SIZE_V + SIZE_C)) B, zero dependencies"
echo "  Reproduce: $0 $@"
