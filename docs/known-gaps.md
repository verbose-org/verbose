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
(`emit_http_demo`, ~498 B) and `--echo-server` (`compile_echo_server`,
~358 B) are here. **No `.verbose` source is involved at all.** The entire
binary is hand-emitted by Rust code in `native.rs` that writes x86-64 bytes
directly. These prove the native backend *can* produce tiny network binaries;
they do **not** prove that the language can describe them yet.

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

## Text-valued let bindings

**Symptom**: `let sep = " | "` followed by `concat(... sep ...)` fails:
```
native codegen error: text literals not supported in native backend
```

**Root cause**: `emit_eval_expr` produces a scalar i64 in rax. Text values
are (ptr, len) pairs — they don't fit the "everything is rax" model. A
let binding evaluates its expression via emit_eval_expr and stores rax at
a rbp slot. Text literals can't go through that path.

**Workaround**: inline the text literal at each usage site instead of
binding it to a let. `concat(acc, " | ", e.name)` works; `let sep = " | "`
then `concat(acc, sep, e.name)` doesn't.

**Fix path**: Either (a) extend emit_eval_expr to handle text values by
storing (ptr, len) in TWO consecutive rbp slots (similar to Phase 2F's
err_ptr_slot/err_len_slot), or (b) detect text-typed let bindings at
compilation time and inline them at each reference site (constant
propagation). Option (b) is simpler for literals; option (a) is more
general (handles computed text values).

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
