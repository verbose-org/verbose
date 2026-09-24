# Predicted execution workloads

Design fixed before implementation, 2026-09-24; now implemented. This first slice describes
expected use in the source and derives a reproducible experiment plan. It does
not select an organization, run a benchmark or rewrite source automatically.

The longer-term loop is: propose an organization from product/workload
expectations, verify its contracts, measure admissible variants, propose a source
revision, then reverify and validate it on separate data. A prediction is not a
proof. Frequency estimates never justify dropping guards, weakening bounds or
changing rare-input behavior. Algorithm rewrites require their own equivalence
argument; test agreement alone does not prove general equivalence.

## Source and meaning

An execution may contain one optional block, as in the complete
[reading example](../examples/workload_profile.verbose):

```verbose
  workload:
    objective: elapsed
    case interactive:
      weight: 99
      records: 1
      target_us: 2000
    case bulk:
      weight: 1
      records: 4096
      target_us: 100000
```

The block always describes a **prediction**. `objective` is `elapsed` or `cpu`:
minimize the weighted arithmetic mean of measured elapsed or aggregate CPU time
per complete invocation. CPU time includes every admitted worker. Resource
constraints remain the execution's existing `native_stack` or `native_memory`
contract, with their distinct scopes. There is no whole-process memory promise.

There are 1..16 named cases, in source order. Names are unique within the
profile. Each has a positive relative invocation `weight` and a `records`
count, both integers in 1..1,000,000. Weights need not add to 100. Optional
`target_us` is a positive integer in 1..1,000,000,000: a desired elapsed time
for the **whole invocation**, even when the objective is CPU. It is not a
runtime timeout, percentile or verified deadline. Every key is closed and
unique; missing required fields, duplicate cases and unsupported values refuse,
including in unselected executions and programmatically constructed ASTs.

A case is a complete input batch, not a restriction on input values or an
assertion that all records succeed. Actual executions may receive other batch
sizes. Scenarios do not supply test data, model arrivals or describe HTTP
traffic. Existing pure execution phases all see the original input batch;
per-rule hot/cold paths and conditional invocation frequencies remain later
work. HTTP/TLS is one motivating product, not a new supported execution scope.

## Report and use

`--workload-report [--json] --run <execution>` checks the original program and
every execution before returning a report. It defaults to the last execution,
requires an explicitly declared workload, and cannot combine with artifact,
runtime or other report modes. It produces no program output or artifact.

```sh
target/release/verbosec examples/workload_profile.verbose --workload-report
target/release/verbosec examples/workload_profile.verbose --workload-report --json
target/release/verbosec examples/workload_profile.verbose --native /tmp/readings
/tmp/readings a 1 b 2 c 3
```

The last command processes three records, although neither predicted case has
that size. Prediction does not narrow the supported runtime input domain.

Schema 1 exposes ordered `input_fields` (name and type) for native argv tooling,
the predicted objective, measurement metric, targets, original
case order and exact rational shares. For weights w and record counts r:

- Invocation share is w / sum(w).
- Record-volume share is w*r / sum(w*r).
- Expected records per invocation are sum(w*r) / sum(w).
- Full-success phase evaluations are r * number_of_phases, counting repeated
  phase positions separately. This is not an instruction/cycle estimate.
- Concurrent native result batches on full success are
  number_of_phases * ceil(r / result_batch). Sequential mode reports null for
  this quantity. It is not a futex/write-syscall count or measured throughput.

All quantities condition on the declared batch shape and, where stated, full
success. Failure can stop publication and concurrency can compute unpublished
work. Temporary values, parsing, calls within phases and actual branch costs
are not priced by these counts. The report never converts operation/record
counts into elapsed time or assumes equal cost per record.

The same report embeds the existing native layout report, including checked
resource ceilings and exclusions. It also states whether native emission has
its required budget (concurrent interpretation can omit `native_memory`). It
does not fabricate a budget. Exact integer arithmetic and declaration order
make repeated reports deterministic. Under the bounded schema, even weighted
phase counts fit u64.

For the example, interactive work represents 99/100 invocations but 99/4195
of record volume; bulk represents 1/100 invocations and 4096/4195 of records.
This helps decide what to measure without claiming the larger case consumes
the same fraction of CPU. Targets identify cases whose latency needs separate
measurement even if their weight is small.

## Compatibility and validation

The profile affects compiler analysis only. It adds no counters, allocations,
GC, runtime dispatch or machine instructions. Native output and interpreter
behavior must remain identical with the block omitted, changed or present,
including inputs outside its predicted cases and failure prefixes. Existing
stack/memory report formats remain unchanged. WASM and the self-hosted compiler
retain their explicit source-execution refusal before output.

| Path | Support |
|---|---|
| Rust parser/verifier | Closed profiles checked in every execution |
| Workload report, text/JSON | Original-source predictions, exact rational counts, existing native layout |
| Native argv / interpreter | Profile accepted, execution behavior unchanged |
| WASM / self-hosted diagnostics, raw x86, ELF | Existing execution refusal before output |
| Measurement and organization proposals | Separate [offline experiment tool](workload-experiments.md); never automatic profile-driven runtime changes |

Test closed syntax at every nesting level, range limits, duplicate names,
programmatic AST checks, unselected invalid profiles, rational weighting,
partial batches, repeated phases, maximum arithmetic and resource scopes.
CLI checks cover imports, selection, incompatible modes, JSON validity,
no-artifact failures, byte-identical compilation and input counts outside the
prediction. Run serialized normal tests, CLI tests, CIDX checks, the reference
example corpus and the existing self-hosted bootstrap before delivery.
The dedicated CLI suite is `python3 tools/test_workload_profile.py -v` after
`cargo build`; `VERBOSEC` can select another compiler build. CI runs it too.

The [experiment tool](workload-experiments.md) now binds constrained variants and
measurements to sources, compiler/binary identities and this profile, separates
selection from validation and emits a source diff only after measured checks.
Hardware/host load, refusals and target failures remain explicit. No winner is
inferred from these static counts alone.
