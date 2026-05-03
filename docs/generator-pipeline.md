# Generator Pipeline

The generator translates `.intent` (numbered prose) into `.verbose` (the
DSL the compiler verifies). It exists because Verbose's founding thesis
is *"the AI is the source author, the verifier is the durable artifact
that makes that safe"* — and without a generator, the AI-as-source half
is played by hand.

This document is the operator-facing reference for the two scripts in
`tools/` that implement the pipeline. For the WHY (thesis, design
choices), see [vision-journal.md](vision-journal.md). For the WHAT
the verifier checks, see [spec-proofs.md](spec-proofs.md).

## TL;DR

```bash
# One-time setup, subscription auth (recommended for habitual use):
pip install claude-agent-sdk
claude setup-token

# Per-session:
export CLAUDE_CODE_OAUTH_TOKEN=<token>
unset ANTHROPIC_API_KEY    # critical, see "Auth precedence gotcha" below
python3 tools/eval.py --use-sdk

# One-shot, per-token billing (no setup beyond the API key):
export ANTHROPIC_API_KEY=sk-ant-...
python3 tools/eval.py
```

Either path runs the generator across a curated sample of repo
intents and prints `first_try=X/N, after_corrections=Y/N, failed=Z/N`.

## What the pipeline does

```
    .intent ──┐
              │
              ├──> [generator]    Claude API or Agent SDK
              │       │
              │       └──> .verbose
              │              │
              │              └──> [verbosec]  ── ok ──> done
              │                       │
              │                       └── error ──> diagnostic ───┐
              │                                                    │
              └──── (correction loop) ─── re-prompt with diag ◄────┘
                            (cap at --max-corrections, default 3)
```

The generator is **not** trusted. Every claim it makes about the code
(`reads`, `calls`, `bound`, type assertions, `@source` line numbers)
is mechanically re-checked by `verbosec`. If verification fails, the
script feeds the diagnostic back to the model and asks it to fix —
up to `--max-corrections` times. If it still doesn't verify, the
script exits non-zero and the operator inspects.

## Two scripts, two transports

| Script | Transport | Auth | Dependencies |
|---|---|---|---|
| `tools/generate.py` | Anthropic Messages API directly via `urllib` | `ANTHROPIC_API_KEY` (per-token billing) | Python stdlib only |
| `tools/generate_sdk.py` | Claude Agent SDK | `CLAUDE_CODE_OAUTH_TOKEN` (subscription) OR `ANTHROPIC_API_KEY` | `pip install claude-agent-sdk` |

The two scripts use the **same prompt-building helpers** (imported from
`generate.py`), so eval runs are comparable across auth modes. The only
thing that changes between them is the HTTP client and the auth header.

### When to pick which

- **Subscription user, frequent runs**: `generate_sdk.py`. Your Pro/Max
  plan covers the calls; no per-token invoice.
- **One-off run, no SDK installed**: `generate.py`. Stdlib-only, just
  set `ANTHROPIC_API_KEY` and go.
- **CI / automation**: either works. API key is more typical in CI
  because subscription tokens require interactive setup.

## Authentication

### Subscription (Claude Pro / Max)

```bash
pip install claude-agent-sdk
claude setup-token
```

`claude setup-token` opens an OAuth flow in your browser, then prints
a token valid for one year. Copy it and:

```bash
export CLAUDE_CODE_OAUTH_TOKEN=<the token from setup-token>
unset ANTHROPIC_API_KEY      # see precedence gotcha below
```

The SDK picks up `CLAUDE_CODE_OAUTH_TOKEN` automatically — no code
change needed.

### API key (per-token billing)

```bash
export ANTHROPIC_API_KEY=sk-ant-...
```

Works with both `generate.py` (which uses it directly) and
`generate_sdk.py` (which routes through the SDK but still bills
per-token in this mode).

### Auth precedence gotcha

When BOTH `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` are set,
**the SDK uses the API key and silently ignores the OAuth token**.
The operator who set the OAuth token expecting subscription billing
discovers per-token charges at invoice time.

`tools/generate_sdk.py` prints a warning when both are set:

```
warning: both ANTHROPIC_API_KEY and CLAUDE_CODE_OAUTH_TOKEN are set.
         The SDK uses the API key (per-token billing) and IGNORES the
         subscription token. To use your subscription, run:
             unset ANTHROPIC_API_KEY
```

If you see this warning and you wanted subscription auth, do what it says.

## Running a single generation

```bash
# Builds a prompt, prints it, doesn't call the API. Useful to inspect
# what the model receives before paying for a real call.
python3 tools/generate.py examples/invoices.intent --dry-run

# Real call (per-token):
ANTHROPIC_API_KEY=sk-ant-... python3 tools/generate.py examples/invoices.intent

# Real call (subscription):
CLAUDE_CODE_OAUTH_TOKEN=<token> python3 tools/generate_sdk.py examples/invoices.intent
```

Default output path is `examples/invoices.verbose` — which **would
overwrite the canonical example shipped in the repo**. Pass `--output`
to write somewhere safe:

```bash
python3 tools/generate.py examples/invoices.intent --output /tmp/inv.verbose
```

(`tools/eval.py` always writes to a tmpdir; this caveat is only for
direct generator invocations.)

## Running the eval

```bash
# Default sample (8 intents covering the language surface):
python3 tools/eval.py                                  # API key
python3 tools/eval.py --use-sdk                        # subscription

# Specific intents:
python3 tools/eval.py invoices business collections

# All 70+ examples (expensive — only do this when you're investigating):
python3 tools/eval.py --all
```

Output (example):

```
writing generated .verbose files into: /tmp/verbose_eval_AbCdEf
running generator on 8 intent(s); model=claude-sonnet-4-6

--- invoices.intent ---
  [attempt 1] calling claude-sonnet-4-6...
OK  invoices.intent verified after 1 attempt(s); output: /tmp/.../invoices.verbose

--- business.intent ---
  [attempt 1] calling claude-sonnet-4-6...
  [attempt 1] rejected; retrying with diagnostic
  [attempt 2] calling claude-sonnet-4-6...
OK  business.intent verified after 2 attempt(s); output: /tmp/.../business.verbose

...

============================================================
  results across 8 intent(s) (model=claude-sonnet-4-6):

  first_try         = 5/8
  after_corrections = 2/8
  failed            = 1/8

  ✓ invoices.intent             status=first_try     attempts=1
  ~ business.intent             status=corrected     attempts=2
  ✓ collections.intent          status=first_try     attempts=1
  ...
  ✗ purchase.intent             status=failed        attempts=4

  output dir: /tmp/verbose_eval_AbCdEf
```

`✓` = first try, `~` = corrected within budget, `✗` = exhausted budget.

## Interpreting the metric

Three regimes, three lessons:

- **High `first_try` (e.g. 7-8/8)**: the grammar is well-documented, the
  prompt works, the pipeline is viable. Next step: widen the sample,
  write a "try this in 5 minutes" README, ship the demo.

- **Most via `after_corrections`**: the model gets close but needs a
  hint. Inspect the diagnostics — they reveal which patterns are
  ambiguous in `INTENT.md` or which verifier messages are unclear.
  Sharpen the docs, re-run.

- **High `failed`**: the loop doesn't converge. The verifier
  diagnostics may not be giving the model enough to act on, OR the
  prompt's grammar reference is missing constructs the model is
  reaching for. Read failed cases carefully — they're the most
  informative signal.

There is no "wrong" result. Every regime tells you what to fix next.

## Cost discussion

System prompt is ~20 KB (~5K tokens). User message is ~500 chars
(~150 tokens). Output is ~1-3 KB depending on intent complexity.

- **API key path (`generate.py`)**: cached system prompt drops input
  cost to ~$0.30/MTok on attempts 2+. First attempt is uncached. Per
  intent with 1-2 attempts: roughly **$0.02-0.05**. Eval on 8
  intents: **~$0.20-0.40**.
- **Subscription path (`generate_sdk.py`)**: zero per-call cost,
  consumes your monthly Pro/Max quota.
- **`--all` over 70+ intents** with API key: ~$2-4 depending on
  correction count.

These are rough — real cost depends on the model and how chatty the
correction loop gets. Sonnet 4.6 (default) is the sweet spot of
quality vs price for this task; Opus 4.7 produces better Verbose
on tricky intents but costs ~5x more.

## Customizing the prompt

Three pieces feed the system prompt:

1. The grammar reference inlined in `tools/generate.py` (the
   `GRAMMAR` constant).
2. Three reference (intent, verbose) example pairs (the `FEW_SHOT`
   constant). Picked to span the surface: minimal scalar rule
   (`invoices`), arithmetic + composition (`business`), collections +
   quantifiers (`collections`).
3. The full `INTENT.md` (recognized prose patterns).

To extend coverage: add a pattern to `INTENT.md` (the document is
load-bearing — the prompt includes it verbatim), or add a new
example pair to `FEW_SHOT` if a class of intent isn't represented.
Re-run the eval to see if the metric moves.

Don't make the system prompt much larger than ~30 KB. Beyond that,
prompt caching helps less and the model starts to lose attention to
the bottom of the prompt (where the task description sits).

## Known limitations

- **One intent per call.** Multi-intent batching could share the
  system prompt cache more aggressively but also makes failure
  isolation harder. Not implemented.
- **No model-specific prompts.** Same prompt for Sonnet, Opus,
  Haiku. May want to give Haiku tighter examples for cost reasons
  in future iterations.
- **Correction loop is single-threaded** within an intent. If the
  diagnostic could be parsed and the model nudged toward a specific
  fix (e.g. "add field X to your `reads:` line"), that would
  converge faster. Not implemented — current loop just dumps the
  full diagnostic.
- **`cargo run --release` per verify** rebuilds the binary on the
  first call (~30s warm-up), then incremental on subsequent calls.
  For automation, pre-build with `cargo build --release` or pin a
  built binary path.

## Replacing the generator entirely

The verifier is the durable artifact, not the generator. If a future
model exposes a better interface (function calling, native tool use,
structured output enforcement, etc.), swap the generator without
touching `verbosec`. The contract between them is just: generator
produces a `.verbose` file, verifier accepts or rejects. Anything
that respects that contract is interchangeable.
