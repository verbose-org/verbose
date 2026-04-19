#!/bin/bash
# generate.sh — Generate Verbose IR from a .intent file using Claude API
#
# Usage:
#   export ANTHROPIC_API_KEY=sk-ant-...
#   ./tools/generate.sh examples/invoices.intent > output.verbose
#
# This is SEPARATE from the compiler. The compiler verifies, this generates.
# If the AI makes a mistake, the compiler will catch it.

set -e

if [ -z "$1" ]; then
    echo "Usage: $0 <file.intent> [> output.verbose]" >&2
    echo "" >&2
    echo "Requires ANTHROPIC_API_KEY environment variable." >&2
    exit 1
fi

if [ -z "$ANTHROPIC_API_KEY" ]; then
    echo "Error: ANTHROPIC_API_KEY not set." >&2
    echo "Get one at https://console.anthropic.com/" >&2
    exit 1
fi

INTENT_FILE="$1"
INTENT_CONTENT=$(cat "$INTENT_FILE")
INTENT_BASENAME=$(basename "$INTENT_FILE")

# The prompt teaches the AI the Verbose syntax
PROMPT="You are a Verbose IR generator. Given a .intent file (numbered human intentions), generate a complete .verbose file.

VERBOSE SYNTAX REFERENCE:

\`\`\`
@verbose 0.1.0

concept ConceptName
  @intention: \"description\"
  @source: ${INTENT_BASENAME}:LINE_NUMBER
  fields:
    field_name : type              -- types: number, bool, text, collection(OtherConcept)
    field_name : number [min, max] -- optional range for optimization

rule rule_name
  @intention: \"description\"
  @source: ${INTENT_BASENAME}:LINE_NUMBER
  input:
    var : ConceptName
  output:
    result_name : type
  logic:
    result_name = expression
  proofs:
    purity:
      reads   : [var.field, ...]   -- every field accessed in logic
      calls   : [other_rule, ...]  -- every rule called in logic
      verdict : pure               -- the only currently-supported verdict
    termination:
      form  : constant_bound
      bound : N                    -- count of operations in logic
    determinism:
      form : total                 -- the only currently-supported determinism form
  hints:                           -- optional
    vectorizable : \"reason\"      -- if pure and no calls; the string explains WHY it is safe
    parallel     : \"reason\"      -- if each iteration is independent; the string explains why
    cache_result : \"reason\"      -- if memoization helps; the string explains why
    overflow     : [min, max]      -- output value bounds (mechanically verified)

reaction reaction_name             -- optional, for side effects
  @intention: \"description\"
  @source: ${INTENT_BASENAME}:LINE_NUMBER
  trigger: rule_name
  effects:
    print \"message\"
\`\`\`

EXPRESSIONS: arithmetic (+, -, *, /, %), comparisons (>, <, >=, <=, ==, !=), boolean (and, or, not), if COND then EXPR else EXPR, rule_call(var), let name = expr. Collection operations: all(coll, x => pred), any(coll, x => pred), map(coll, x => expr) returning collection, filter(coll, x => pred) returning collection, sum(coll, x => expr), count(coll, x => pred), min(coll, x => expr), max(coll, x => expr), fold(coll, initial, acc, x => body).

PROSE PATTERNS: the recognized mappings from natural-language intent phrasings to Verbose constructs are listed in INTENT.md. Common ones: \"for each X, check Y\" → all, \"for each X, compute Y\" → map, \"keep X where Y\" → filter, \"total of Y over X\" → sum, \"count X where Y\" → count, \"when X happens, do Y\" → reaction.

RULES:
- Every @source must reference an actual line number from the intent file
- reads must list EXACTLY the fields accessed (not more, not less)
- calls must list EXACTLY the rules called
- bound must be >= the number of Binary/Call/If/Not/Neg operations
- Use field ranges [min, max] when reasonable bounds are known
- Every hint (vectorizable / parallel / cache_result) MUST carry a string justification; bare keywords are rejected by the parser

INTENT FILE (${INTENT_BASENAME}):
${INTENT_CONTENT}

Generate the complete .verbose file. Output ONLY the verbose code, no explanation."

# Call Claude API
RESPONSE=$(curl -s https://api.anthropic.com/v1/messages \
    -H "x-api-key: $ANTHROPIC_API_KEY" \
    -H "content-type: application/json" \
    -H "anthropic-version: 2023-06-01" \
    -d "$(jq -n \
        --arg prompt "$PROMPT" \
        '{
            model: "claude-sonnet-4-20250514",
            max_tokens: 8192,
            messages: [{role: "user", content: $prompt}]
        }')")

# Extract the text content
echo "$RESPONSE" | jq -r '.content[0].text'
