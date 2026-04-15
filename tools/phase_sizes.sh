#!/bin/bash
# phase_sizes.sh — Compile every phase-representative example natively and
# report binary sizes in a table. Lets you see at a glance what each feature
# costs in emitted bytes.
#
# Each row names the phase, the example rule, and the byte count of the
# resulting ELF. No external deps: pure cargo run.

set -e

cargo build --release --quiet 2>/dev/null
VERBOSEC="target/release/verbosec"

TMP=$(mktemp -d)
trap "rm -rf $TMP" EXIT

# entries: "phase|source|rule|shape description"
ENTRIES=(
  "0       |examples/invoices.verbose   |important_invoice   |bool output, scalar compare"
  "1A      |examples/audit_simple.verbose|audit_suspicious   |reaction append_file literal"
  "1B      |examples/audit_log.verbose   |audit_suspicious   |reaction append_file concat (numbers)"
  "1B-text |examples/audit_user.verbose  |audit_suspicious   |reaction append_file concat (text field)"
  "2A      |examples/purchase.verbose    |validate_purchase  |Result(number, text)"
  "2B      |examples/tier.verbose        |classify_tier      |Result(text, text)"
  "2C      |examples/classify.verbose    |classify_invoice   |Record output with branches"
  "2D      |examples/purchase.verbose    |discounted_purchase|match_result pass-through"
  "2E      |examples/greeting.verbose    |make_report        |text input field → Record JSON"
  "2F      |examples/enrich.verbose      |enriched           |match_result enriched Err arm"
  "2G      |examples/compose.verbose     |name_line          |text-returning rule call inlined"
  "2H-a    |examples/log_via_helper.verbose|log_alert        |reaction append_file content = Call"
  "2H-b    |examples/compose.verbose     |greeting           |Call as concat arg"
  "3-map   |examples/payroll.verbose     |compute_bonuses    |collection(Record) via map"
  "3-filter|examples/payroll.verbose     |high_earners       |collection(Record) via filter"
  "3.2-n   |examples/payroll.verbose     |salaries           |collection(number) via map"
  "3.2-t   |examples/payroll.verbose     |names              |collection(text) via map"
  "4-sum   |examples/payroll.verbose     |total_salaries     |fold to number (sum)"
  "4-count |examples/payroll.verbose     |high_earner_count  |fold to number (count)"
  "5a      |examples/greeting_line.verbose|greeting_line     |output:text per-record (concat)"
  "5b      |examples/roster.verbose      |roster_line        |output:text via fold (append-only)"
  "Record+concat|examples/fullname.verbose|compose_greeting  |Record text field from concat"
)

printf "╔════════════════════════════════════════════════════════════════════════════╗\n"
printf "║  Phase-coverage binary sizes                                               ║\n"
printf "╚════════════════════════════════════════════════════════════════════════════╝\n"
printf "\n"
printf "  %-10s %-26s %8s  %s\n" "Phase" "Rule" "Size" "Shape"
printf "  %-10s %-26s %8s  %s\n" "-----" "----" "----" "-----"

for entry in "${ENTRIES[@]}"; do
  IFS='|' read -r phase src rule desc <<< "$entry"
  phase=$(echo "$phase" | xargs)
  src=$(echo "$src" | xargs)
  rule=$(echo "$rule" | xargs)
  desc=$(echo "$desc" | xargs)
  out="$TMP/bin_${rule}"
  if "$VERBOSEC" "$src" --native "$out" --run "$rule" >/dev/null 2>&1; then
    size=$(stat -c%s "$out")
    printf "  %-10s %-26s %7dB  %s\n" "$phase" "$rule" "$size" "$desc"
  else
    printf "  %-10s %-26s %8s  %s\n" "$phase" "$rule" "ERROR" "$desc"
  fi
done

printf "\n"
printf "  All binaries are statically linked ELF64, no libc, no heap.\n"
printf "  Reproduce: ./tools/phase_sizes.sh\n"
