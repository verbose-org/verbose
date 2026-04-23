# Using Verbose for EU AI Act High-Risk Compliance

This doc shows how to use Verbose to produce the audit artefacts required by Regulation (EU) 2024/1689 (the AI Act) for a high-risk automated decision. It is a reusable pattern, with `examples/loan_decision.verbose` as the worked case.

It is written for someone who needs to implement one high-risk decision under the Act and wants a structure they can audit, re-audit six months later, and hand to a regulator without re-writing anything.

## Why Verbose fits this problem specifically

The AI Act demands, for each high-risk automated decision, that the following be simultaneously true at runtime and available for inspection:

1. A plain-language statement of what the decision does (Article 13, transparency)
2. A formal specification whose relationship to the code is mechanically clear (Articles 15, 17)
3. An explicit, case-level reason when an individual is adversely affected (Article 86, right to explanation)
4. A log of each decision made, preserved over the system's lifecycle (Article 12, record-keeping)

In a conventional stack these sit in four different places: a Confluence page for (1), code comments and design docs for (2), an ad-hoc string concatenation returning an HTTP 403 body for (3), and an observability pipeline feeding Splunk for (4). Drift between any two of them is invisible until a regulator or a refused applicant asks a pointed question.

Verbose collapses (1), (2), and (3) into three mechanically-linked artefacts. Two patterns are documented for (4):

- **Pattern 1 — wrapper around stdout/stderr.** Works for any rule, including batch and queue-driven runners. The audit chain lives in a shell wrapper the operator controls. Documented below with `loan_decision`.
- **Pattern 2 — HTTP-fronted with an in-binary `log:` block.** Phase 7 introduced declarable network listeners; Phase 8 added a per-request audit-log effect on services. The wrapper disappears: log-append, timestamp capture, and fail-closed semantics are all part of the binary, verified alongside the rule. Documented below with `access_audited`.

The chain is *structural* in both patterns — nothing depends on anyone remembering to update the Confluence page or to keep the rejection message strings in sync with the code.

## The pattern, in five points

1. **Declare the input subject as a `concept`.** One concept per decided entity (applicant, patient file, transaction, candidate). Field ranges carry the domain constraints.
2. **Write the decision as a rule returning `Result(T, text)`.** `T` is whatever the approved decision produces (an amount, a classification code, a boolean, a record). The text channel carries the rejection reason.
3. **Each distinct failure mode is its own `Err("...")` branch.** The text is the explanation the provider must give under Article 86. Keep it plain-language, free of internal jargon, naming the specific criterion that failed.
4. **The `.intent` file (one numbered sentence per concept and rule) is the Article 13 disclosure.** It is what you show to a regulator or hand to a business stakeholder before they read any code. The verifier cross-checks `@source: intent_file:line` references for every concept and rule, so the `.intent` cannot silently drift out of date.
5. **Wrap the binary's stderr into an append-only log.** Each Err produced is one entry in the Article 12 trail. Because stdout carries approvals and stderr carries refusals+reasons, the two channels already separate the flows a logger needs to track.

## Worked example: `loan_decision.verbose`

The repository ships a complete example at `examples/loan_decision.verbose`. Regulatory anchor: Annex III point 5(b) explicitly lists AI systems that evaluate the creditworthiness of natural persons as high-risk.

- `examples/loan_decision.intent` — four numbered sentences describing what the rule does
- `examples/loan_decision.verbose` — the formal rule; inline comments at the top of the file cite each Article whose obligation is addressed
- Compiled output: 1554 bytes, zero dependencies, reads applicants from stdin as `income credit_score employment_months recent_defaults`, writes approved amounts to stdout and refusal reasons to stderr

To build and try it:

```bash
cargo run -- examples/loan_decision.verbose --native /tmp/loan --run loan_decision --stream
printf '30000 650 12 0\n15000 650 12 0\n' | /tmp/loan
# stdout: 9000       (applicant approved, amount = 30% of income)
# stderr: income below minimum required 25000
```

## Per-article mapping

| Article | Requirement | Pattern 1 (wrapper) | Pattern 2 (HTTP-fronted) |
|---|---|---|---|
| 12 | Automatic record-keeping of events over the system's lifecycle | Shell wrapper around stdout + stderr | `log:` block on the service, `on_error: abort` for fail-closed |
| 13 | Transparency: operation disclosed in a form users can understand | `.intent` file (same in both) | `.intent` file (same in both) |
| 15 | Accuracy, robustness, cybersecurity by design | Verifier-enforced: purity, termination bound, overflow bounds, zero external deps | Same verifier guarantees + closed-grammar log scope |
| 17 | Quality management system, documented over the lifecycle | Git history + `docs/vision-journal.md` + `cargo test` | Same |
| 86 | Right to explanation for adversely-affected individuals | Text of each `Err` branch | `resp.body` slot — same value to user and to audit log |

Note that Articles 15 and 17 are covered *structurally* (the way Verbose is built, you could not skip them if you tried); Articles 13, 86, and 12 require the author to actually write the `.intent`, the `Err` texts, and the log wrapper. The compiler cannot produce natural-language disclosure content for you.

## Article 12 — the logging wrapper

The binary separates approvals (stdout) from refusals-with-reason (stderr). An Article 12 trail needs both streams, timestamped, preserving the input that produced each verdict. One straightforward wrapper:

```bash
#!/usr/bin/env bash
# audit-log.sh — Article 12 wrapper around a Verbose decision binary.
# Append-only JSONL log: one record per decision, both approvals and refusals.
# High-volume deployments should replace this with a native logger (named
# pipes, journald, or — future — a Verbose reaction once Phase 7+ lands).

set -euo pipefail
BIN="${1:?usage: audit-log.sh <binary> <log_file>}"
LOG="${2:?usage: audit-log.sh <binary> <log_file>}"

while IFS= read -r input; do
  ts=$(date -u +%Y-%m-%dT%H:%M:%S.%3NZ)
  err_file=$(mktemp)
  if stdout=$(printf '%s\n' "$input" | "$BIN" 2>"$err_file"); then :; fi
  stderr=$(cat "$err_file"); rm -f "$err_file"
  if [[ -n "$stdout" ]]; then
    printf '{"ts":"%s","input":%s,"verdict":"approved","value":%s}\n' \
      "$ts" "$(printf '%s' "$input" | jq -Rs .)" "$stdout" >>"$LOG"
  else
    printf '{"ts":"%s","input":%s,"verdict":"refused","reason":%s}\n' \
      "$ts" "$(printf '%s' "$input" | jq -Rs .)" "$(printf '%s' "$stderr" | jq -Rs .)" >>"$LOG"
  fi
done
```

Usage:

```bash
printf '30000 650 12 0\n15000 650 12 0\n' \
  | ./audit-log.sh /tmp/loan /var/log/loan_decisions.jsonl
```

Each line of `/var/log/loan_decisions.jsonl` is one record. The log is append-only, timestamped, keyed by the exact input that produced each verdict. Rotation and retention are handled by whatever log infrastructure the operator already runs (logrotate, fluent-bit, etc.) — consistent with the project's "OS is the supervisor" posture.

This wrapper is deliberately simple and slow (one subprocess per decision). For throughput above a few hundred decisions per second, replace it with a wrapper written in a language that can read both channels concurrently, or wait for a future Verbose phase that introduces a declarable `append_to_log` reaction and removes the wrapper entirely.

## What this pattern does not cover

Honesty matters more than sales pitch. This pattern addresses the parts of AI Act compliance that concern the *decision* artefact itself. It does not cover, and cannot pretend to cover:

- **Article 10 (data governance).** If the rule's thresholds came from a training run, the provenance of that training data is not in the `.verbose` file. Rule-based decisions side-step Article 10 naturally; ML-derived thresholds do not, and require a separate data-governance artefact.
- **Article 14 (human oversight).** The pattern produces a verdict mechanically. Deciding when a human must review a verdict before it takes effect is an organizational choice the operator wires around the binary (e.g., the wrapper routes all refusals to a queue for human confirmation before sending the rejection email).
- **Article 22 GDPR (right to contest an automated decision).** Orthogonal to the AI Act trail. The refusal reason from the `Err` branch makes contesting possible; implementing the contest workflow is outside the binary.
- **Drift detection and post-market monitoring (Article 72).** The logs this pattern produces are the raw material; detecting that verdicts are trending in an unexpected direction is a separate analysis pipeline the operator runs against the JSONL stream.
- **Conformity assessment (Articles 43-49).** The binary is an input to a conformity assessment, not a substitute for it.

What this pattern *does* collapse: the drift problem between (disclosure) ↔ (specification) ↔ (code) ↔ (explanation). Those four are often four separate artefacts maintained by different people. Here they are three mechanically-linked artefacts plus a wrapper the operator controls. That is the scope.

## Pattern 2 (HTTP-fronted): the binary IS the audit chain

The shell wrapper above existed because, until Phase 7, Verbose could not describe a network listener nor a file-append effect. Phases 7 and 8 closed both gaps. A `service` declares the listener; a `log:` block on that service declares the per-request append-only audit effect. The wrapper disappears.

The repository ships the worked case at `examples/access_audited.verbose`. Same regulatory shape as `loan_decision`, applied to an access-control gate:

```bash
cargo run -- examples/access_audited.verbose --native /tmp/gate
/tmp/gate &
curl -s http://127.0.0.1:18897/public/index    # allowed: GET /public/index
curl -s http://127.0.0.1:18897/private/secret  # refused: path /private/secret is outside the public allowlist
curl -s -X POST http://127.0.0.1:18897/public/index  # refused: method POST is not GET
cat /tmp/verbose_access_audited.jsonl | jq -c .
```

The audit log:
```json
{"ts":1776934638,"method":"GET","path":"/public/index","status":200,"reason":"allowed: GET /public/index"}
{"ts":1776934638,"method":"GET","path":"/private/secret","status":403,"reason":"refused: path /private/secret is outside the public allowlist"}
{"ts":1776934638,"method":"POST","path":"/public/index","status":403,"reason":"refused: method POST is not GET"}
```

What changed structurally vs the wrapper pattern:

- **Article 86 anchor is mechanical, not aspirational.** The user-facing reason (`resp.body`) and the audit log reason (`resp.body` referenced in the log scope) are the same rbp slot. There is no place where a reviewer can render a verdict for the user and a different one for the trail; both flow from one assignment.
- **Article 12 is in-process, not subprocess.** No `mktemp`, no `jq`, no shell at all. One open + one write + one close per request, all visible in the binary's syscall trace.
- **Article 12 is fail-closed when declared as such.** `on_error: abort` (Phase 8 slice 8d) terminates the process with status 1 on any open/write failure. The next request fails to connect rather than silently succeeding without a logged trail. Default remains `drop` (silent ignore) for setups that prefer availability over guaranteed audit; the operator opts into the strict posture.
- **Timestamp comes from the kernel, not the wrapper.** `req.timestamp` (Phase 8 slice 8c) is one `clock_gettime(CLOCK_REALTIME)` per accept loop, captured before the parser runs. Visible only inside the log scope so the response stays reproducible from `(method, path)` alone — replaying a request produces the same verdict, only the timestamp differs.

When to keep using Pattern 1 (shell wrapper around stdout+stderr): non-HTTP workflows (batch scoring from a file, message-queue consumption), or rules whose input shape is too rich for path-based encoding. The two patterns coexist; neither replaces the other.

## For a second high-risk category

Duplicate either pattern with a new concept, a new rule, and domain-appropriate refusal text. Candidates whose Annex III classification is clear today: health or auto insurance scoring (point 5(a)), CV screening and employment decisions (point 4(a)), eligibility determination for public benefits (point 5(c)). The structure is the same; only the concept fields, the thresholds, and the rejection text change.
