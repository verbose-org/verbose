#!/usr/bin/env python3
"""Same as `tools/generate.py` but uses the Claude Agent SDK so the user can
authenticate via their **Claude Pro / Max subscription** instead of paying
per-token via an API key.

Why this exists
---------------
`tools/generate.py` calls the Anthropic Messages API directly through
`urllib`, which requires `ANTHROPIC_API_KEY` and bills per token. For
operators who already pay for a Claude subscription, `pip install
claude-agent-sdk` + `claude setup-token` lets the same generation work
load draw from the subscription quota — zero per-token billing, no
extra API key management.

The generation logic, prompt, verify-and-correct loop, and exit-code
contract are IDENTICAL to `generate.py`. Only the transport (HTTP +
auth) changes. Both scripts can coexist; pick whichever auth model
matches the operator's setup.

Auth precedence gotcha
----------------------
The SDK silently picks `ANTHROPIC_API_KEY` over
`CLAUDE_CODE_OAUTH_TOKEN` when BOTH are set. If you wanted
subscription auth and got per-token billing instead, that's why.
This script surfaces a warning when both are set so you don't
discover it on your invoice.

Usage
-----
First time only::

    pip install claude-agent-sdk
    claude setup-token         # mints a year-long OAuth token

Then::

    export CLAUDE_CODE_OAUTH_TOKEN=<token from setup-token>
    unset ANTHROPIC_API_KEY    # CRITICAL: API key would otherwise win
    python3 tools/generate_sdk.py examples/foo.intent

Same flags as generate.py: --output, --max-corrections, --model,
--dry-run, --quiet. Same exit codes.
"""

import argparse
import asyncio
import os
import sys
from pathlib import Path

# Reuse the prompt-building, code-fence stripping, and verifier helpers
# from generate.py so the two scripts produce IDENTICAL prompts. If the
# prompt drifts between the two paths we lose the ability to compare
# eval results across auth modes.
sys.path.insert(0, str(Path(__file__).parent))
from generate import (  # noqa: E402
    DEFAULT_MODEL,
    build_correction_user_prompt,
    build_initial_user_prompt,
    build_system_prompt,
    format_diagnostic_snippet,
    indent,
    load_dotenv,
    normalize_source_paths,
    strip_code_fence,
    verify,
)


def _import_sdk():
    """Lazy SDK import so --help and --dry-run work without it installed."""
    try:
        from claude_agent_sdk import (  # noqa: F401
            AssistantMessage,
            ClaudeAgentOptions,
            ClaudeSDKClient,
            TextBlock,
        )
    except ImportError:
        sys.exit(
            "claude-agent-sdk is not installed. Install it with:\n"
            "    pip install claude-agent-sdk\n"
            "Then mint a subscription token with:\n"
            "    claude setup-token\n"
            "and export it as CLAUDE_CODE_OAUTH_TOKEN."
        )
    return ClaudeAgentOptions, ClaudeSDKClient, AssistantMessage, TextBlock


async def run_async(
    intent_path: Path,
    output_path: Path,
    *,
    max_corrections: int,
    model: str,
    quiet: bool,
) -> tuple[bool, int, str]:
    """Same shape as generate.run() — returns (ok, attempts_used, last_diag)."""
    ClaudeAgentOptions, ClaudeSDKClient, AssistantMessage, TextBlock = _import_sdk()

    intent_content = intent_path.read_text()
    system = build_system_prompt()
    initial_user = build_initial_user_prompt(intent_path, intent_content)

    options = ClaudeAgentOptions(
        system_prompt=system,
        model=model,
        # Empty list disables ALL tools. This is a pure text-generation
        # task — the agent has no business running Bash, Read, Edit,
        # etc. We own the verify-correct loop ourselves so we keep the
        # metric (first_try / corrected / failed) precise.
        allowed_tools=[],
    )

    diag = ""
    # ClaudeSDKClient maintains the multi-turn session in-process;
    # subsequent .query() calls reuse the prior conversation, which is
    # how the correction loop carries the assistant's prior output +
    # the verifier diagnostic forward.
    async with ClaudeSDKClient(options=options) as client:
        for attempt in range(max_corrections + 1):
            if not quiet:
                print(f"  [attempt {attempt + 1}] calling {model} (SDK)...", file=sys.stderr)

            user_msg = initial_user if attempt == 0 else build_correction_user_prompt(diag)
            await client.query(user_msg)

            text = ""
            async for message in client.receive_response():
                if isinstance(message, AssistantMessage):
                    for block in message.content:
                        if isinstance(block, TextBlock):
                            text += block.text

            verbose = normalize_source_paths(strip_code_fence(text))
            output_path.write_text(verbose)

            ok, diag = verify(output_path)
            if ok:
                return True, attempt + 1, ""
            if attempt == max_corrections:
                return False, attempt + 1, diag
            if not quiet:
                print(f"  [attempt {attempt + 1}] rejected; retrying with diagnostic:", file=sys.stderr)
                print(format_diagnostic_snippet(diag), file=sys.stderr)

    # Unreachable; return for type checker.
    return False, max_corrections + 1, diag


def _check_auth(quiet: bool):
    """Surface the precedence gotcha + bail if no auth at all."""
    has_key = bool(os.environ.get("ANTHROPIC_API_KEY"))
    has_oauth = bool(os.environ.get("CLAUDE_CODE_OAUTH_TOKEN"))
    if has_key and has_oauth and not quiet:
        print(
            "warning: both ANTHROPIC_API_KEY and CLAUDE_CODE_OAUTH_TOKEN are set.\n"
            "         The SDK uses the API key (per-token billing) and IGNORES the\n"
            "         subscription token. To use your subscription, run:\n"
            "             unset ANTHROPIC_API_KEY",
            file=sys.stderr,
        )
    if not has_key and not has_oauth:
        sys.exit(
            "no auth configured. Put one of these in .env (copy .env.example\n"
            "as a starting point) or `export` it:\n"
            "  - subscription:  CLAUDE_CODE_OAUTH_TOKEN=<token from `claude setup-token`>\n"
            "  - per-token:     ANTHROPIC_API_KEY=sk-ant-..."
        )


def main():
    load_dotenv()

    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("intent_path", type=Path)
    parser.add_argument("--output", type=Path, help="output .verbose path (default: same dir as input)")
    parser.add_argument("--max-corrections", type=int, default=3)
    parser.add_argument("--model", default=DEFAULT_MODEL)
    parser.add_argument("--dry-run", action="store_true", help="build the prompt and print it; don't call the SDK")
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

    _check_auth(args.quiet)

    ok, attempts, diag = asyncio.run(
        run_async(
            args.intent_path,
            output_path,
            max_corrections=args.max_corrections,
            model=args.model,
            quiet=args.quiet,
        )
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
