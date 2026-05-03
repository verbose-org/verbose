#!/usr/bin/env python3
"""Run the generator across a sample of repo intents and report a verify-rate.

Usage:
    export ANTHROPIC_API_KEY=sk-ant-...
    python3 tools/eval.py                                    # uses the default curated sample
    python3 tools/eval.py invoices business collections      # explicit list (basenames)
    python3 tools/eval.py --all                              # every examples/*.intent (expensive!)
    python3 tools/eval.py --output-dir /tmp/eval-out         # where to put generated .verbose
    python3 tools/eval.py --max-corrections 3 --model claude-sonnet-4-6

Output:
    A per-intent line ("OK <name> in N attempt(s)" or "FAIL <name> ...")
    plus a final summary table:

        first_try=X/N    after_corrections=Y/N    failed=Z/N

The generated `.verbose` files land in --output-dir (defaults to a fresh
tmpdir). The repo's canonical `examples/*.verbose` files are NEVER touched
— eval.py forces the generator's `--output` so the curated source-of-truth
stays intact.

This script is the metric the project lacked: how authorable is Verbose
by an AI that has never seen the source? Each run is one data point; it
improves as the prompt, the grammar doc, and the verifier diagnostics
all sharpen.
"""

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
EXAMPLES = REPO / "examples"

# Curated sample chosen to span the language surface without paying for
# all 70+ intents. Add/remove as the eval surface evolves; the goal is
# representative coverage, not exhaustive coverage.
DEFAULT_SAMPLE = [
    "invoices",       # 2 lines — minimal scalar rule
    "business",       # 5 lines — arithmetic + composition
    "collections",    # 5 lines — collection + all/any quantifiers
    "report",         # sum / count / fold over a collection
    "retirement",     # map + filter
    "audit_simple",   # reaction with append_file (literal content)
    "tier",           # Result(text, text)
    "purchase",       # Result(number, text) + match_result
]

# Parses the outcome line generate.py prints on its stdout.
OUTCOME_RE = re.compile(r"^(OK|FAIL)\s+(\S+)\s+(?:verified|did NOT verify)\s+after\s+(\d+)\s+attempt")


def run_one(
    intent_path: Path,
    output_path: Path,
    *,
    max_corrections: int,
    model: str,
    quiet: bool,
) -> tuple[str, int]:
    """Invoke generate.py for one intent. Return (status, attempts).

    status ∈ {"first_try", "corrected", "failed"}.
    """
    cmd = [
        sys.executable,
        str(REPO / "tools" / "generate.py"),
        str(intent_path),
        "--output", str(output_path),
        "--max-corrections", str(max_corrections),
        "--model", model,
    ]
    if quiet:
        cmd.append("--quiet")

    proc = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True)
    # generate.py's outcome line is the last informative line of stdout.
    outcome_line = ""
    for line in proc.stdout.splitlines():
        m = OUTCOME_RE.match(line)
        if m:
            outcome_line = line
            break

    # Echo generator's own logs so the operator sees attempt-by-attempt
    # progress in the eval transcript.
    if proc.stderr:
        sys.stderr.write(proc.stderr)
    if proc.stdout:
        sys.stdout.write(proc.stdout)

    if not outcome_line:
        # The generator failed before reaching the outcome line —
        # treat as a failure with 0 attempts (setup error, API
        # error, etc.). The captured stderr above explains why.
        return "failed", 0

    m = OUTCOME_RE.match(outcome_line)
    assert m  # we just matched it
    status_word = m.group(1)
    attempts = int(m.group(3))
    if status_word == "OK":
        return ("first_try" if attempts == 1 else "corrected"), attempts
    return "failed", attempts


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument(
        "stems",
        nargs="*",
        help="intent basenames (without .intent extension); default: a curated sample",
    )
    parser.add_argument("--all", action="store_true", help="run on every examples/*.intent")
    parser.add_argument("--output-dir", type=Path, help="where to put generated .verbose (default: tmpdir)")
    parser.add_argument("--max-corrections", type=int, default=3)
    parser.add_argument("--model", default="claude-sonnet-4-6")
    parser.add_argument("--quiet", action="store_true", help="suppress per-attempt logs from generator")
    args = parser.parse_args()

    if not os.environ.get("ANTHROPIC_API_KEY"):
        sys.exit("ANTHROPIC_API_KEY is not set")

    if args.all:
        intent_paths = sorted(EXAMPLES.glob("*.intent"))
    elif args.stems:
        intent_paths = []
        for stem in args.stems:
            p = EXAMPLES / f"{stem}.intent"
            if not p.exists():
                sys.exit(f"intent not found: {p}")
            intent_paths.append(p)
    else:
        intent_paths = []
        for stem in DEFAULT_SAMPLE:
            p = EXAMPLES / f"{stem}.intent"
            if not p.exists():
                print(f"  WARN  default-sample entry missing: {p}", file=sys.stderr)
                continue
            intent_paths.append(p)

    if not intent_paths:
        sys.exit("no intents to process")

    out_dir = args.output_dir or Path(tempfile.mkdtemp(prefix="verbose_eval_"))
    out_dir.mkdir(parents=True, exist_ok=True)
    print(f"writing generated .verbose files into: {out_dir}")
    print(f"running generator on {len(intent_paths)} intent(s); model={args.model}\n")

    # Per-intent results: (name, status, attempts)
    results: list[tuple[str, str, int]] = []
    for intent_path in intent_paths:
        out_path = out_dir / f"{intent_path.stem}.verbose"
        print(f"--- {intent_path.name} ---")
        status, attempts = run_one(
            intent_path,
            out_path,
            max_corrections=args.max_corrections,
            model=args.model,
            quiet=args.quiet,
        )
        results.append((intent_path.name, status, attempts))
        print()

    # Summary --------------------------------------------------------
    n = len(results)
    first_try = sum(1 for _, s, _ in results if s == "first_try")
    corrected = sum(1 for _, s, _ in results if s == "corrected")
    failed = sum(1 for _, s, _ in results if s == "failed")

    print("=" * 60)
    print(f"  results across {n} intent(s) (model={args.model}):")
    print()
    print(f"  first_try        = {first_try}/{n}")
    print(f"  after_corrections = {corrected}/{n}")
    print(f"  failed           = {failed}/{n}")
    print()
    # Per-intent breakdown for the operator's grep.
    for name, status, attempts in results:
        marker = {"first_try": "✓", "corrected": "~", "failed": "✗"}[status]
        print(f"  {marker} {name:<32}  status={status:<12}  attempts={attempts}")
    print()
    print(f"  output dir: {out_dir}")
    sys.exit(0 if failed == 0 else 1)


if __name__ == "__main__":
    main()
