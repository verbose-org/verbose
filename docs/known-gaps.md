# Known Gaps in Native Backend

Gaps discovered through project-driven testing. Each is a real user-facing
limitation with a documented workaround. Ordered by impact.

## Three tiers of native output (important clarification)

Not every native binary the repo produces is "a program described in Verbose".
Three tiers exist, and conflating them has been a source of confusion in
discussions — hence this section.

**Tier 1 — Fully described in Verbose.** Every `.verbose` rule compiled with
`--native --run RULE [--stream | --stdin]` lives here. The rule logic AND the
ELF layout both flow from the source through the verifier and the regular
native codegen path. Verification applies end to end. Examples: every file in
`examples/*.verbose` compiled to native, including `priv_failure.verbose` and
the streaming `alert.verbose`.

**Tier 2 — Hybrid rule + hardcoded network shell.** The `--http-server` mode
(`native::compile_http_server`) sits here. The *rule* comes from a `.verbose`
file and is verified; the *network plumbing* around it (socket / bind /
listen / accept loop / HTTP parse / response formatting) is emitted by
hand-written Rust in `src/native.rs`, not described in any `.verbose` source.
The rule is trustworthy under the usual proofs; the shell around it is a
non-Verbose artifact bolted on by the compiler driver.

**Tier 3 — Native emitter feasibility probes.** `--demo-http`
(`emit_http_demo`, ~498 B) is here. **No `.verbose` source is involved at
all.** The entire binary is hand-emitted by Rust code in `native.rs` that
writes x86-64 bytes directly. These prove the native backend *can*
produce tiny network binaries; they do **not** prove that the language
can describe them yet.

*Status update (Phase 7 slice 2b, 2026-04-20):* the TCP echo probe
(`compile_echo_server`, 358 B) has been **collapsed into tier 1** — it
is now also emittable from a `.verbose` file via the `service`
construct with `Protocol::RawTcp` and an identity handler (see
`examples/raw_tcp_echo.verbose`). Both paths share the same
emission body (`emit_raw_tcp_echo_bytes`), so the tier-1 and tier-3
binaries are bit-for-bit identical (asserted by a regression test).
`--echo-server` remains available as a tier-3 shortcut but no longer
represents a capability the language itself lacks. The HTTP demo
(`--demo-http`) is still tier 3; it collapses under Phase 7 slice 3
when HTTP/1.0 protocol support lands.

The long-term target is to collapse tiers 3 and 2 into tier 1, one syscall
family at a time, under a future Phase 7+ that introduces declarable network
primitives (see the *Network syscalls not describable in Verbose* gap below).
Until that phase lands, all three tiers coexist and must be labeled as such.

## Network syscalls not describable in Verbose (Phase 7+ target)

**Symptom**: there is no `.verbose` syntax today for `socket`, `bind`, `listen`,
`accept`, `read` from a socket, `write` to a socket, nor for the structured
parsing of HTTP requests or the formatting of HTTP responses. Binaries that
do these things (tier 2 and tier 3 above) rely on hand-emitted Rust code in
`native.rs`.

**Why it matters**: the project's long-term vision is that *everything the
program does is declared in `.verbose` and mechanically verified*. Network
syscalls are the biggest missing slice. Until they are declarable, any
network-facing artifact the repo produces carries a non-Verbose layer whose
security audit is manual (reading Rust source) rather than mechanical
(reading proofs).

**Fix path** (sketch, not a commitment): extend `Effect` with new declared
reactions — `listen_tcp port`, `accept_connection`, `read_until sentinel`,
`write_bytes`. Each carries its own proof obligations (bounded buffer sizes,
no unbounded loops without declared termination, no reads that outlive their
fd lifetime). The verifier checks those obligations; the native emitter
produces the same socket-syscall bytes it already produces, but driven by a
`.verbose` source rather than hardcoded. Tier 3 binaries collapse into tier 1
automatically.

**Scope warning**: this is a substantial phase (new AST constructs, new
verifier rules, new codegen paths, new test coverage). The project stays on
SIEM-style demos (tier 1) while network primitives are designed.

**Design sketch available**: see `docs/phase-7-design.md` for the proposed
shape — a `service` top-level construct declaring protocol / port /
max_request / handler, with a closed set of built-in protocols (HTTP/1.0
and raw TCP) emitted by the compiler around a normal rule. That sketch is
not an implementation commitment; it fixes the target shape so that when
Phase 7 is built, the design is already decided.

## Phase 9 file I/O — slice 1 landed (2026-04-25)

Phase 9 introduces declarable file-system access. Slice 1 covers
**read-only file reads** declared at the top level:

```verbose
resource config
  @intention: "Static application config"
  @source: read_config.intent:1
  path: "/etc/myapp.conf"
  max:  4096
  on_read_error: abort
```

Any rule whose output type is `text` may then call `read(config)` to
load the file contents at runtime. Constraints enforced by the
verifier:

- **Path is a string literal** baked into the binary inline. The
  auditor reads the source — or `strings` the binary — and sees every
  file the program can attempt to open. Dynamic paths
  (`concat("/var/", req.path)`) are rejected; slice 1 trades
  convenience for the no-traversal-by-construction guarantee.
- **`max:` ≤ 64 MiB**, declared per resource. Allocates a stack
  buffer of that size; reads larger than the bound truncate (no
  streaming in slice 1).
- **`on_read_error: abort`** is currently the only accepted policy
  (the parser rejects `drop`). Open or read failure exits the process
  with status 1 — same shape as Phase 8 slice 8d.
- **Rule purity proof** must list every resource name in `reads:`
  (e.g., `reads: [config]`), same discipline as field reads. Reading
  a resource you didn't declare to read is a verifier error.

Native cost: ~80 bytes for the open/read/close sequence + abort patch
sites + a buffer of `max` bytes on the stack per unique resource. The
sequence runs once per rule invocation (above `loop_top`), so a
streaming rule reading the same resource for every record reads the
file once, not N times. Worked example: `examples/read_config.verbose`
(541-byte binary).

**Phase 10 (2026-04-25):** opt-in `concurrency: forked` on services.
The Apache mpm_prefork shape via `fork()` after each `accept`. Parent
closes the client fd and loops; child runs the handler / log /
response then `sys_exit(0)`. `rt_sigaction(SIGCHLD, SIG_IGN)` once at
startup makes the kernel auto-reap children — no `wait`/`waitpid`,
no zombies. Default stays `Sequential` so existing services compile
byte-for-byte identical (additive slice). Restricted to HTTP/1.0
today (raw_tcp + fork combination is a later slice). Worked example:
`static_file_server.verbose` with `concurrency: forked` ships at
1730 bytes — +158 over the sequential variant.

**Slice 2 (2026-04-25):** `read(<name>)` now works inside HTTP
service handlers. The handler body can be `body: read(page)` directly
or via `concat(...)` containing a read. The open/read/close sequence
is emitted per-accept iteration (above the HTTP parse step), so the
operator can update the file on disk and the next request sees the
new content — no recompile, no signal. `static_file_server.verbose`
ships the worked example (1572-byte binary).

**Still deferred for later slices:**

- **`on_read_error: drop`** (silent-ignore + empty text). Slice 1's
  strict-only default makes failure obvious; relaxing is an explicit
  opt-in if needed.
- **File resources in collection / fold / parallel programs.**
  `emit_collection_program` and friends still reject `read(...)` —
  same shape as slice 1's prologue work but for fold-context frames.
- **Multiple resources composing in a single concat in service
  handlers.** Single-resource handler bodies tested; multi-resource
  in one expression should work via text_bindings but isn't covered
  by a regression test yet.
- **Request-derived file paths in services** (e.g.,
  `GET /static/file.html` → open `/var/www/file.html`). Requires path
  traversal proofs that slice 1/2 deliberately refuse; separate
  design pass with normalised path slots and `O_NOFOLLOW`.
- **Streaming reads larger than `max:`.** Single-shot only.
- **`cache: true`** declaration to read once at server startup
  instead of per-accept. Performance-sensitive deployments will
  want this; trade-off is staleness vs. ~3 µs syscall overhead per
  request.

## Phase 8 audit-log gaps still open (2026-04-23)

Phase 8 lets a `service` declare a per-request `log:` block that fires
between the handler and the wire response. As of 2026-04-23, slices 8a,
8b, 8c, and 8d have landed. The log scope sees a closed grammar:
text/number literals, `concat(...)`, plus four field accesses backed
by rbp slots:

  - `req.method`, `req.path`     — slice 8a, parsed from the request
  - `resp.status`, `resp.body`   — slice 8b, populated by the handler
  - `req.timestamp`              — slice 8c, captured by `clock_gettime`
                                    once per accept loop

The handler itself never sees `req.timestamp`; the rewrite is local to
the log scope so the response stays reproducible from `(method, path)`
alone. `audit_complete.verbose` exercises all four in one JSONL line.

Slice 8d adds an opt-in `on_error: drop | abort` line to the log block.
Drop is the default and matches slice 8a behaviour (silently ignore log
syscall failures). Abort exits the process with status 1 on any open()
or write() failure — the fail-closed posture an Article 12 audit chain
needs. `audit_strict.verbose` shows the syntax; the abort path costs
~16 bytes plus 8 bytes per checked syscall, zero when the policy is
Drop.

Still deferred:

- **Slice 8e — multiple log effects per service.** One `append_file`
  per service today. Two separate audit sinks (e.g. JSONL + a binary
  ring buffer) need either a list under `log:` or a parallel `audit:`
  block.
- **Slice 8f — JSON escaping primitive.** `concat` does not escape
  special characters in user-controlled fields. A path containing `"`
  produces broken JSON. Workaround until 8f: trust the upstream parser
  to reject the request before it reaches the log line, or expose the
  raw line through a JSON-tolerant pipeline. Real fix: a `json_escape`
  text primitive.
- **`req.timestamp` resolution.** The captured value is whole seconds
  (`tv_sec`); sub-second precision is in `tv_nsec` but not yet wired.
  Adds another slot and another itoa.
- **`resp.body` length is byte length.** The Phase 7 HTTP serializer
  treats the body as opaque bytes. Multibyte-aware length (codepoints,
  graphemes) is not in scope and is unlikely to ever be — Verbose
  stays at the byte level on purpose.

## Text-valued let bindings (partial — text-output rules only)

**What works (since 2026-04-23)**:

- **Text literal lets**, everywhere. The optimiser inlines every
  reference to the let-name with the literal at AST level (runs once
  before any backend sees the rule; respects lambda / fold /
  match-result scope shadowing; chains like `let a = "x"; let b = a`
  resolve in source order).
- **Non-literal text lets in `output: text` rules**. `let tagged =
  concat("[", e.user, "#", e.id, "]")` followed by `concat(tagged,
  " amount=", e.amount)` now compiles natively. The prologue's
  let-eval loop classifies each binding's RHS (text vs number),
  allocates two consecutive rbp slots for text ones, emits through
  `emit_text_produce_ptrlen`, and registers the name in
  `ctx.text_bindings`. The record-loop epilogue frees the concat
  buffer via `mov rsp, rbp` once per iteration — same mechanism as a
  bare Phase 5a text-output rule. See `examples/ledger_line.verbose`.
- **Non-literal text lets in Result rules** (`Result(number, text)`
  and `Result(text, text)`). `ctx.text_bindings` is threaded through
  `emit_eval_result_expr` → `emit_match_result_inlined` →
  `emit_redirect_callee_leaves`, so Ident(let-name) resolves in Ok,
  Err, and match_result Err capture arms. The Phase 2F err_var local
  binding is merged with the caller's text_bindings (one clone + one
  insert) so the outer Err body can reference both prior text lets
  AND the captured err_var in a single concat. See
  `examples/gate_result.verbose`.

**What still fails**:

- **Non-literal text lets in service handlers.** Phase 7 slice 3+ has
  its own handler emission path (`emit_handler_to_slots`) which
  doesn't go through `emit_record_loop_prologue`. Handler-body lets
  are still rejected at the `analyze_http10_handler_shape` gate.
  Fix path: run the same classify-and-emit loop inside
  `emit_http10_dynamic_bytes` before dispatching to the handler
  emitter, building a handler-local offsets + text_bindings pair.
- **Non-literal text lets in collection / fold programs** (map, filter,
  sum, count, the Phase 3/4/5b families). Each has its own
  `rule.logic.bindings` walk that still calls `emit_eval_expr`
  directly. Same fix shape as the record-loop prologue.

**Workaround for the rejected contexts**: inline the text expression
at each usage site, or factor it through a helper rule whose output
is text and call that rule (Phase 2G / 2H-a / 2H-b cover rule calls
in every text sink).

## Nested concat with Call args at 2+ levels

**Symptom**: `concat("a", outer_rule(p), "b")` where `outer_rule` body
is `concat("x", inner_rule(p), "y")` fails:
```
Phase 2H-b: nested concat cannot have its own Call args
```

**Root cause**: the `is_nested` flag in emit_concat_to_buffer_impl
prevents inner concats from having their own CallText pre-eval. The
outer's r11 slot base would be clobbered.

**Workaround**: flatten the composition by using an intermediate helper
rule that doesn't involve concat-of-Call, or restructure so Call appears
only at the top concat level.

**Fix path**: use rbp-relative slots for r11 saves instead of register
preservation. Requires prologue extension.
