# Verbose examples

Each `<name>.verbose` file is a self-contained program that compiles to native x86-64. The `<name>.intent` companion is the human-readable specification (one numbered point per intention) — `.verbose` evolves freely under the AI's transformation; `.intent` formalizes human intent.

Every example here either:
- pins a regression test (the test reads the `.verbose` file and asserts on the emitted binary), or
- anchors a feature in [`CLAUDE.md`](../CLAUDE.md), or
- documents a real demo path (EU AI Act, SIEM, etc.)

If you want to know what a feature *looks like* in source, find it below and read the file. The header comment of each file names the slice / phase it demonstrates.

---

## Where to start

| File | What it shows |
|------|---------------|
| [`audit_gateway.verbose`](audit_gateway.verbose) | **SYNTHESIS DEMO** — production-shaped HTTP policy gate combining 9 features in one file. The example to read first if you want to know what Verbose actually does. ~2.9 KB native binary. |
| [`invoices.verbose`](invoices.verbose) | Smallest possible rule: one concept, one bool. The hello-world. |
| [`business.verbose`](business.verbose) | Arithmetic + composition (4 rules building on each other). |
| [`showcase.verbose`](showcase.verbose) | Almost every feature in one business scenario (6 rules). |

---

## Foundations (Phase 0 — scalar arithmetic, if/else, let)

| File | What it shows |
|------|---------------|
| `invoices.verbose` | `bool` rule, scalar input. |
| `business.verbose` | Arithmetic, rule composition (`important_invoice` calls `total_with_tax`), `parallel:` hint. |
| `pricing.verbose` | Nested if/else + let bindings, CSE. |
| `clients.verbose` | Text fields + text equality (`field == "literal"`). |
| `deadcode.verbose` | Dead-branch elimination from declared field ranges (`number [0, 50]`). |
| `collections.verbose` | `all` / `any` quantifiers on a nested input collection. |

## Result types (Phase 2A / 2B / 2D / 2F)

| File | What it shows |
|------|---------------|
| `purchase.verbose` | `Result(number, text)` — `validate_purchase` Ok→stdout, Err→stderr. Plus `discounted_purchase` exercising slice 2D match_result inlining. |
| `tier.verbose` | `Result(text, text)` — Ok-text writes to stdout via the shared text-write helper. |
| `gate_result.verbose` | Slice 2I-R: text `let` bindings reused across Ok and Err arms of a Result rule. |
| `enrich.verbose` | Slice 2F: outer Err arm transforms the captured err_var via concat. |
| `loan_decision.verbose` | EU AI Act demo: `Result(decision, text)` for a credit gate (Annex III point 5(b)). |

## Record output (Phase 2C / 2E / 2H-b)

| File | What it shows |
|------|---------------|
| `classify.verbose` | `output: Named(record)` — JSONL one-line-per-record output, if/else over two record arms. |
| `greeting.verbose` | Phase 2E: text-typed input field flows into a record's text field. |
| `fullname.verbose` | Phase 2C with text-via-concat: `concat(...)` value for a record text field. |
| `bonus.verbose` | Phase 3 + 2C: `map` produces a `collection(BonusReport)`. |
| `compose.verbose` | Phase 2H-b: helper-rule call appears inside a `concat(...)` argument. |
| `log_via_helper.verbose` | Phase 2H-a: reaction `append_file` content is itself a helper-rule call. |

## Text bindings (Phase 2I family)

| File | What it shows |
|------|---------------|
| `ledger_line.verbose` | Phase 2I: non-literal text `let` bindings in an `output: text` rule. |
| `gate_result.verbose` | Phase 2I-R extension to Result rules. |
| `greeting_service.verbose` | Phase 2I-H: text `let` reused twice in an HTTP handler's response body. |

## Collections — map / filter (Phase 3)

| File | What it shows |
|------|---------------|
| `payroll.verbose` | Multiple shapes: map → record collection, filter, map → number, map → text. |
| `retirement.verbose` | `map` + `filter` chained on the same collection. |
| `bonus.verbose` | `map` producing a record collection (JSONL streaming). |

## Number folds (Phase 4 — sum / count / min / max)

| File | What it shows |
|------|---------------|
| `payroll.verbose` | `total_salaries` (sum), `high_earner_count` (count). |
| `report.verbose` | `risk_score` — combined fold + quantifiers. |

## Text output (Phase 5)

| File | What it shows |
|------|---------------|
| `greeting_line.verbose` | Phase 5a: per-record text via `concat(...)`, one `write` per record. |
| `roster.verbose` | Phase 5b: text fold (`fold(...)` to text), append-only body, two-pass sizing. |

## Quantifiers (Phase 6 — `all` / `any` desugared to multi-fold)

| File | What it shows |
|------|---------------|
| `report.verbose` | Embedded `all` and `any` in scalar output, single-pass multi-accumulator. |
| `logs.verbose` | Same shape on a different domain (event severity / duration). |
| `priv_failure.verbose` | SIEM single-event predicate — inspired by Sigma's failed-Windows-logon rule. Streaming-mode candidate. |

## HTTP services (Phase 7 — `service` declarations)

| File | What it shows |
|------|---------------|
| `hello_http.verbose` | Slice 3b: constant `HttpResponse { status: 200, body: "..." }`. The smallest HTTP server. |
| `hello_router.verbose` | Slice 3c: if/else routing on `req.method` + `req.path`. |
| `echo_path.verbose` | Slice 3d: response body assembled via `concat(req.method, req.path, ...)`. |
| `method_guard.verbose` | Slice 3e: `status` is a computed expression (`if cond then 200 else 405`). |
| `prefix_router.verbose` | `starts_with(req.path, "/api/v1/")` for path-prefix routing without regex. |
| `uri_size_gate.verbose` | `length(req.path) > parse_int(read(max))` — runtime-tunable input gate. |
| `raw_tcp_echo.verbose` | `Protocol::RawTcp` — bytes-in, bytes-out, no parsing. |

## HTTP audit logs (Phase 8 — per-request `log:` block)

| File | What it shows |
|------|---------------|
| `hello_router_logged.verbose` | Slice 8a: simple `log: append_file` on a service. |
| `audit_complete.verbose` | Slices 8b + 8c: rich JSONL with `req.method`/`req.path`/`req.timestamp` + `resp.status`/`resp.body`. |
| `audit_strict.verbose` | Slice 8d: `on_error: abort` — fail-closed audit. |
| `dual_log.verbose` | Slice 8e: two `log:` blocks per service (strict audit + best-effort metrics). |
| `access_log_json.verbose` | Slice 8 + Phase 12 `json_escape` for safe JSON in audit lines. |
| `access_audited.verbose` | EU AI Act high-risk gate — user-facing reason ≡ audit-log reason (Pattern 2 in `docs/ai-act-usage.md`). |

## Reactions (CLI rules with declared side effects)

| File | What it shows |
|------|---------------|
| `reactions.verbose` | Basic reaction: print on trigger. |
| `alerts.verbose` | Dynamic alert content via interpolated values. |
| `audit_simple.verbose` | `append_file` reaction with a static content literal. |
| `audit_log.verbose` | `append_file` with dynamic `concat(...)` content (Phase 1B). |
| `audit_user.verbose` | `append_file` whose content concats a text-typed input field. |

## File I/O — `read(<resource>)` (Phase 9)

The resource-aware emitter sweep: every native emitter accepts `read()` with the same prologue pattern.

| File | Slice | What it shows |
|------|-------|---------------|
| `read_config.verbose` | 9.1 | `read()` in a text-output rule prologue. |
| `static_file_server.verbose` | 9.2 + 9.4 + 10 | `read()` in HTTP handler + `cache: true` + `concurrency: forked`. |
| `banner_roster.verbose` | 9.5 | `read()` as the INIT of a Phase 5b text fold. |
| `sep_roster.verbose` | 9.5b | `read()` in the BODY of a Phase 5b text fold. |
| `tagged_bonuses.verbose` | 9.5c | `read()` in a Phase 3 map's record field. |
| `sum_by_tag.verbose` | 9.5d | `read()` in a Phase 4 number fold body. |
| `access_check.verbose` | 9.5e | `read()` in a Phase 6 multi-fold (extracted quantifier). |
| `parallel_threshold.verbose` | 9.5f | `read()` in a parallel rule (sweep complete). |

## Outbound network — `fetch(<connection>)` (Phase 11)

| File | What it shows |
|------|---------------|
| `health_check.verbose` | Slice 11.1: outbound TCP fetch in a CLI rule. First Verbose binary that opens an outbound connection. |
| `api_gateway.verbose` | Slice 11.2: `fetch()` inside an HTTP handler — server AND client in one binary. |
| `reverse_proxy.verbose` | Slice 11.3: real reverse proxy — request bytes built from `req.method`/`req.path` per request. |
| `enriched_page.verbose` | Coverage example: `read(resource) + fetch(connection)` in the same handler body via `concat`. Surfaced + pinned a real concat-sizing bug (see commit `deb0047`). |

## Runtime primitives (2026-04-28 / 2026-04-29)

| File | Primitive | What it shows |
|------|-----------|---------------|
| `threshold_sum.verbose` | `parse_int(read(...))` | Numeric threshold loaded from disk; fail-closed twice. |
| `recent_event.verbose` | `now_unix()` | System clock as declared read; `reads: [now]` in proof. |
| `sliding_count.verbose` | `now_unix()` in fold body | Sliding-window count over a batch judged against ONE captured `now`. |
| `allowlist.verbose` | `field == read(<r>)` | Text equality with bound RHS — runtime allowlist. |
| `prefix_router.verbose` | `starts_with(h, n)` | HTTP path-prefix routing without regex. |
| `uri_size_gate.verbose` | `length(<text>)` | Byte count for input validation; composes with `parse_int(read(...))`. |
| `keyword_filter.verbose` | `contains(h, n)` | Substring search; "deploy once, retarget by editing". |
| `parallel_threshold.verbose` | `parse_int(read(...))` in parallel | Closes the read-everywhere matrix. |

## Modules

| File | What it shows |
|------|---------------|
| `app.verbose` | `use "stdlib/finance.verbose"` — module import and rule composition across files. |

## Streaming mode (`--stream`)

| File | What it shows |
|------|---------------|
| `alert.verbose` | Long-running event filter; reads stdin line by line. First long-running Verbose binary. |
| `priv_failure.verbose` | SIEM-style single-event predicate; designed to be piped from a log feed. |

## Multi-rule binaries (one binary, several declared rules)

| File | What it shows |
|------|---------------|
| `logs.verbose` | 5 rules sharing one input concept; multi-rule stdin binary at 2233 B. |
| `config.verbose` | 5 rules; 2929 B multi-rule stdin binary. |

## Architectural stratification

| File | What it shows |
|------|---------------|
| `layers.verbose` | `@layer: domain | application | interface` — sealed-subgraph discipline verified at compile time. |

## EU AI Act high-risk demos (`docs/ai-act-usage.md`)

| File | Annex III anchor | Pattern |
|------|------------------|---------|
| `loan_decision.verbose` | 5(b) credit scoring | `Result(decision, text)` — accept/reject with declared reason. |
| `cv_screening.verbose` | 4(a) recruitment | Per-candidate scalar verdict + declared reason. |
| `access_audited.verbose` | (general high-risk gate) | HTTP-fronted: user-facing reason bit-for-bit identical to audit-log reason. |

## Generated artifacts

| File | What it shows |
|------|---------------|
| `generated.verbose` | Header notes "GENERATED BY AI from generated.intent". Demonstrates the future `.intent → .verbose` pipeline (separate project per CLAUDE.md). |

## Composition with the verifier

| File | What it shows |
|------|---------------|
| `policy.verbose` | First multi-input rule: a context (policy thresholds) read once, requests checked per-record. |
