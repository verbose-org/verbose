#!/usr/bin/env python3
"""Generate `.verbose` from a `.intent` via Claude API, with verify-and-correct loop.

Usage:
    export ANTHROPIC_API_KEY=sk-ant-...
    python3 tools/generate.py examples/foo.intent
    python3 tools/generate.py examples/foo.intent --output /tmp/foo.verbose
    python3 tools/generate.py examples/foo.intent --max-corrections 3 --model claude-sonnet-4-6
    python3 tools/generate.py examples/foo.intent --dry-run         # print prompt, don't call API

Exit codes:
    0  the produced `.verbose` verifies (possibly after corrections)
    1  the produced `.verbose` did NOT verify within --max-corrections
    2  setup error (missing API key, can't read input, etc.)

Why this matters
----------------
This script closes the loop the project's thesis depends on:
    .intent  --(generator)-->  .verbose  --(verbosec verifier)-->  binary

Without the generator, the AI-as-source half of the thesis is played by
hand. With it, the experience matches the design intent: write prose,
get a verified binary. The verifier remains the durable artifact —
this script is the one that may be replaced as model quality changes.

Stdlib-only (no `requests`, no `openai`, etc.) on purpose: matches
the rest of the project's zero-external-deps discipline.
"""

import argparse
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_MODEL = "claude-sonnet-4-6"

# ---------------------------------------------------------------------------
# Prompt assets
# ---------------------------------------------------------------------------
#
# The system prompt is large (~30 KB) and identical across attempts on
# the same intent — perfect for prompt caching. The user message is
# small (the intent itself + per-correction diagnostic), so the only
# variable cost across attempts is the small delta plus output.
#
# Few-shot pairs are inlined verbatim from the canonical examples in
# the repo. Picking these THREE rather than scanning all 70+ pairs is
# a deliberate choice: too much few-shot dilutes the signal and
# blows the context budget; these three span the basic surface
# (single-rule, composition, collections+quantifiers) that the
# majority of test intents fall under.

GRAMMAR = """## Verbose grammar (compact reference)

A `.verbose` file declares concepts and rules that operate on them.
Every rule carries proofs the compiler verifies — they are NOT
optional.

### Skeleton

    @verbose 0.1.0

    concept ConceptName
      @intention: "what this concept represents"
      @source: <intent_filename>:<line>
      fields:
        field_name : type            -- types: number, bool, text, collection(Other)
        field_name : number [a, b]   -- optional integer range

    rule rule_name
      @intention: "what this rule expresses"
      @source: <intent_filename>:<line>
      input:
        var : ConceptName
      output:
        result_name : type
      logic:
        result_name = <expression>
      proofs:
        purity:
          reads : [var.field, ...]   -- exactly the fields touched
          calls : [other_rule, ...]  -- exactly the rules called
        termination:
          bound : N                  -- >= count of Binary/Call/If/Not/Neg ops

### Expressions

- arithmetic: +, -, *, /, %
- comparisons: >, <, >=, <=, ==, !=
- boolean: and, or, not
- if cond then expr else expr  (nestable)
- let name = expr  (then refer to `name` in subsequent expressions)
- rule_call(var)  (composes another rule; must be in `calls`)
- text composition: concat(arg1, arg2, ...)  (variadic, scalar args only)
- text->number: parse_int(text)
- number ops: abs(x), min(a, b), max(a, b)
- text predicates: starts_with(t, n), ends_with(t, n), contains(t, n)
- text length: length(text)
- absolute clock: now_unix()  (must appear in `reads:` as `now`)
- Result: Ok(v), Err(e), match_result(r, v => ok_body, e => err_body)
- Record: ConceptName { field: expr, ... }
- Collections: all(xs, x => p), any(xs, x => p), filter(xs, x => p),
               map(xs, x => e), sum(xs, x => e), count(xs, x => p),
               min(xs, x => e), max(xs, x => e),
               fold(xs, init, acc, x => body)

### Reactions (declared side effects)

    reaction name
      @intention: "..."
      @source: ...:..
      trigger: a_bool_rule
      effects:
        print "literal {var.field}"
        append_file "/path/literal" "literal {var.field}"

The path of `append_file` is a string literal; the content is a
text expression (use `concat(...)` for dynamic content).

### Hints (optional, each MUST carry a string justification)

      hints:
        vectorizable : "scalar arithmetic, no calls or cross-element deps"
        parallel     : "each iteration independent of the others"
        cache_result : "expensive pure rule reused multiple times"
        overflow     : [min, max]                    -- output bounds

### Rules the verifier will reject

- `reads:` includes a field never touched by `logic:`, OR omits one that is touched.
- `calls:` includes a rule never invoked, OR omits one that is.
- `bound:` is less than the actual operation count in `logic:`.
- A hint lacks a string justification (bare keyword).
- `@source` line number falls outside the `.intent` file's range.
- An expression construct unknown to the grammar above (made-up keyword).
"""

FEW_SHOT = """## Reference examples

Three paired (.intent, .verbose) examples ranging from minimal to
collection-using, drawn from the canonical examples shipped with
the compiler.

### Example 1 — minimal (single rule, scalar output)

intent:
    1. An invoice has an amount.
    2. An invoice is important when its amount exceeds 10000.

verbose:
    @verbose 0.1.0

    concept Invoice
      @intention: "An invoice has an amount"
      @source: invoices.intent:1

      fields:
        amount : number


    rule important_invoice
      @intention: "An invoice is important when its amount exceeds 10000"
      @source: invoices.intent:2

      input:
        i : Invoice

      output:
        important : bool

      logic:
        important = i.amount > 10000

      proofs:
        purity:
          reads   : [i.amount]
          calls   : []

        termination:
          bound : 1


### Example 2 — arithmetic + composition (multiple rules calling each other)

intent:
    1. An invoice has an amount, a tax rate, and a number of days overdue.
    2. The total with tax is amount plus amount times the tax rate divided by 100.
    3. An invoice is important when its total exceeds 10000.
    4. An invoice is overdue when it has more than 30 days overdue.
    5. An invoice is critical when it is both important and overdue.

verbose:
    @verbose 0.1.0

    concept Invoice
      @intention: "An invoice has an amount, a tax rate, and a number of days overdue"
      @source: business.intent:1

      fields:
        amount       : number
        tax_rate     : number
        days_overdue : number


    rule total_with_tax
      @intention: "The total with tax is amount plus amount times the tax rate divided by 100"
      @source: business.intent:2

      input:
        i : Invoice

      output:
        total : number

      logic:
        total = i.amount + i.amount * i.tax_rate / 100

      proofs:
        purity:
          reads   : [i.amount, i.tax_rate]
          calls   : []
        termination:
          bound : 3


    rule important_invoice
      @intention: "An invoice is important when its total exceeds 10000"
      @source: business.intent:3

      input:
        i : Invoice

      output:
        important : bool

      logic:
        important = total_with_tax(i) > 10000

      proofs:
        purity:
          reads   : [i]
          calls   : [total_with_tax]
        termination:
          bound : 2


    rule overdue_invoice
      @intention: "An invoice is overdue when it has more than 30 days overdue"
      @source: business.intent:4

      input:
        i : Invoice

      output:
        overdue : bool

      logic:
        overdue = i.days_overdue > 30

      proofs:
        purity:
          reads   : [i.days_overdue]
          calls   : []
        termination:
          bound : 1


    rule critical_invoice
      @intention: "An invoice is critical when it is both important and overdue"
      @source: business.intent:5

      input:
        i : Invoice

      output:
        critical : bool

      logic:
        critical = important_invoice(i) and overdue_invoice(i)

      proofs:
        purity:
          reads   : [i]
          calls   : [important_invoice, overdue_invoice]
        termination:
          bound : 3


### Example 3 — collections + quantifiers

intent:
    1. An invoice has an amount and a number of days overdue.
    2. A client has a name and a list of invoices.
    3. An invoice is overdue when it has more than 30 days overdue.
    4. A client is blocked when all their invoices are overdue.
    5. A client is at risk when any of their invoices are overdue.

verbose:
    @verbose 0.1.0

    concept Invoice
      @intention: "An invoice has an amount and a number of days overdue"
      @source: collections.intent:1

      fields:
        amount       : number
        days_overdue : number


    concept Client
      @intention: "A client has a name and a list of invoices"
      @source: collections.intent:2

      fields:
        name     : text
        invoices : collection(Invoice)


    rule invoice_overdue
      @intention: "An invoice is overdue when it has more than 30 days overdue"
      @source: collections.intent:3

      input:
        inv : Invoice

      output:
        overdue : bool

      logic:
        overdue = inv.days_overdue > 30

      proofs:
        purity:
          reads   : [inv.days_overdue]
          calls   : []
        termination:
          bound : 1


    rule client_blocked
      @intention: "A client is blocked when all their invoices are overdue"
      @source: collections.intent:4

      input:
        c : Client

      output:
        blocked : bool

      logic:
        blocked = all(c.invoices, inv => invoice_overdue(inv))

      proofs:
        purity:
          reads   : [c.invoices]
          calls   : [invoice_overdue]
        termination:
          bound : 2


    rule client_at_risk
      @intention: "A client is at risk when any of their invoices are overdue"
      @source: collections.intent:5

      input:
        c : Client

      output:
        at_risk : bool

      logic:
        at_risk = any(c.invoices, inv => invoice_overdue(inv))

      proofs:
        purity:
          reads   : [c.invoices]
          calls   : [invoice_overdue]
        termination:
          bound : 2
"""


def load_intent_md() -> str:
    return (REPO / "INTENT.md").read_text()


def build_system_prompt() -> str:
    return f"""You translate Verbose `.intent` files into `.verbose` source files.

The `.verbose` you produce is checked by an independent compiler
(`verbosec`). The compiler trusts NO claim — every `reads`, `calls`,
`bound`, type assertion, and `@source` reference is mechanically
verified. Output the compiler rejects fails the task.

Output ONLY the `.verbose` source. No prose, no markdown fences, no
explanation. Start with `@verbose 0.1.0` and stop after the last
declaration.

Match the indentation, spacing, and ordering of the reference
examples below.

{GRAMMAR}

{FEW_SHOT}

## Recognized prose patterns

{load_intent_md()}
"""


def indent(s: str, prefix: str) -> str:
    return "\n".join(prefix + line for line in s.splitlines())


def build_initial_user_prompt(intent_path: Path, intent_content: str) -> str:
    return f"""## Task

Translate the following intent into a `.verbose` file. Use
`{intent_path.name}:LINE` for every `@source` reference, where LINE is
the actual line number in the intent below.

intent:
{indent(intent_content.rstrip(), "    ")}

Output the complete .verbose file now.
"""


# Matches `@source:` lines whose value contains a path component (any `/`
# or `\` before the `:line` suffix). The Claude Agent SDK injects the
# CWD via system-reminder messages on every turn, which biases the
# model toward emitting absolute paths even when the prompt instructs
# otherwise. Rather than fight the prompt, we normalise post-generation:
# any path-like @source value gets its basename extracted.
import re as _re
_SOURCE_PATH_RE = _re.compile(
    r'(@source\s*:\s*)([^\s:]*[\\/][^\s:]*)(:\d+)',
    _re.MULTILINE,
)


def normalize_source_paths(verbose_text: str) -> str:
    """Collapse any path in `@source: <path>:<line>` to its basename.

    Idempotent and safe on already-correct input (the regex only fires
    when a slash is present in the value).
    """
    def _basename(match):
        prefix, path, suffix = match.group(1), match.group(2), match.group(3)
        # Use the part after the last separator. Both / and \ handled —
        # the model occasionally emits Windows-style paths even on Linux.
        bare = path.replace("\\", "/").rsplit("/", 1)[-1]
        return f"{prefix}{bare}{suffix}"
    return _SOURCE_PATH_RE.sub(_basename, verbose_text)


def build_correction_user_prompt(diagnostic: str) -> str:
    # Trim the diagnostic — the verifier sometimes prints multi-page
    # output. The first ~2000 chars usually contain the actionable
    # error message; beyond that we waste cache+output budget.
    snippet = diagnostic[:2000]
    if len(diagnostic) > 2000:
        snippet += "\n... (diagnostic truncated)"
    return f"""The compiler rejected the previous output with this diagnostic:

```
{snippet.strip()}
```

Produce a corrected `.verbose` file. Address every error above
without introducing new ones. Output ONLY the `.verbose` code.
"""


def call_claude(model: str, system: str, messages: list, *, timeout: int = 180) -> str:
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    if not api_key:
        raise SystemExit("ANTHROPIC_API_KEY is not set")
    body = json.dumps(
        {
            "model": model,
            "max_tokens": 8192,
            "system": [
                # Mark the system block as cacheable. On the second
                # attempt for the same intent the cache hit drops the
                # input cost ~10x — important when the diagnostic loop
                # is in play.
                {"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}
            ],
            "messages": messages,
        }
    ).encode("utf-8")
    req = urllib.request.Request(
        "https://api.anthropic.com/v1/messages",
        data=body,
        headers={
            "x-api-key": api_key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            data = json.loads(resp.read().decode("utf-8"))
    except urllib.error.HTTPError as e:
        body_text = e.read().decode("utf-8", errors="replace")
        raise SystemExit(f"API error {e.code}: {body_text[:500]}")
    except urllib.error.URLError as e:
        raise SystemExit(f"network error: {e}")
    return "".join(
        b.get("text", "") for b in data.get("content", []) if b.get("type") == "text"
    )


def strip_code_fence(text: str) -> str:
    """If the model wrapped output in ```...``` despite our instruction, unwrap.

    Tolerated because models sometimes refuse to drop fences. We
    accept either ``` or ```verbose as the opening fence.
    """
    text = text.strip()
    if text.startswith("```"):
        nl = text.find("\n")
        if nl != -1:
            text = text[nl + 1 :]
        if text.endswith("```"):
            text = text[:-3]
    return text.strip() + "\n"


def verify(verbose_path: Path) -> tuple[bool, str]:
    """Run `verbosec` on the produced `.verbose`. Return (ok, diagnostic)."""
    proc = subprocess.run(
        ["cargo", "run", "--quiet", "--release", "--", str(verbose_path)],
        cwd=REPO,
        capture_output=True,
        text=True,
        timeout=180,
    )
    ok = proc.returncode == 0 and "verified:" in proc.stdout
    diagnostic = (proc.stdout + "\n" + proc.stderr).strip()
    return ok, diagnostic


def run(
    intent_path: Path,
    output_path: Path,
    *,
    max_corrections: int,
    model: str,
    quiet: bool,
) -> tuple[bool, int, str]:
    """Generate, verify, optionally correct, return (ok, attempts_used, last_diag)."""
    intent_content = intent_path.read_text()
    system = build_system_prompt()
    initial_user = build_initial_user_prompt(intent_path, intent_content)
    messages: list = [{"role": "user", "content": initial_user}]
    diag = ""

    for attempt in range(max_corrections + 1):
        if not quiet:
            print(f"  [attempt {attempt + 1}] calling {model}...", file=sys.stderr)
        text = call_claude(model, system, messages)
        verbose = normalize_source_paths(strip_code_fence(text))
        output_path.write_text(verbose)

        ok, diag = verify(output_path)
        if ok:
            return True, attempt + 1, ""
        if attempt == max_corrections:
            return False, attempt + 1, diag
        if not quiet:
            print(f"  [attempt {attempt + 1}] rejected; retrying with diagnostic", file=sys.stderr)
        messages.append({"role": "assistant", "content": verbose})
        messages.append({"role": "user", "content": build_correction_user_prompt(diag)})

    # Unreachable, but keeps the type checker quiet.
    return False, max_corrections + 1, diag


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("intent_path", type=Path)
    parser.add_argument("--output", type=Path, help="output .verbose path (default: same dir as input)")
    parser.add_argument("--max-corrections", type=int, default=3)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--dry-run", action="store_true", help="build the prompt and print it; don't call the API")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args()

    if not args.intent_path.exists():
        sys.exit(f"intent file not found: {args.intent_path}")

    output_path = args.output or args.intent_path.with_suffix(".verbose")

    if args.dry_run:
        system = build_system_prompt()
        user = build_initial_user_prompt(args.intent_path, args.intent_path.read_text())
        print(f"=== system prompt ({len(system):,} chars) ===")
        print(system[:800] + ("\n... (truncated)" if len(system) > 800 else ""))
        print(f"\n=== user prompt ({len(user):,} chars) ===")
        print(user)
        return

    ok, attempts, diag = run(
        args.intent_path,
        output_path,
        max_corrections=args.max_corrections,
        model=args.model,
        quiet=args.quiet,
    )
    if ok:
        print(f"OK  {args.intent_path.name} verified after {attempts} attempt(s); output: {output_path}")
        sys.exit(0)
    print(f"FAIL  {args.intent_path.name} did NOT verify after {attempts} attempt(s); output: {output_path}")
    if not args.quiet:
        print("  last diagnostic:", file=sys.stderr)
        print(indent(diag[:2000], "    "), file=sys.stderr)
    sys.exit(1)


if __name__ == "__main__":
    main()
