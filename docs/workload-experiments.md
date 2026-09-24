# Measured execution proposals

Design fixed before implementation, 2026-09-24; now implemented. This is the next slice after
[predicted workloads](workload-profiles.md): an offline, standard-library Python
tool compares existing native execution organizations and emits a reviewable
source proposal. The compiler remains the authority for syntax, proofs and
resource bounds. No runtime profiler, dispatcher, GC or new native code path is
introduced.

## Allowed changes

The original execution is always the baseline. Explicitly supplied candidates
may change only its mode, `max_in_flight` and `result_batch`. Rules, phase order,
input, failure policy, workload and proofs remain byte-for-byte unchanged. The
selected execution must be declared in the entry file. A conservative source
editor recognizes whole configuration lines outside strings/comments; layouts
it cannot identify refuse instead of receiving an approximate rewrite.

An existing resource ceiling cannot increase. A candidate in the other mode
requires an explicit ceiling supplied by the experiment author: sequential
`native_stack` and concurrent `native_memory` measure different scopes and must
never be compared or added as though they were total process memory. Every
candidate passes original-source compiler verification and native emission;
insufficient budgets are recorded as refusals, not automatically enlarged.

The experiment snapshots the entry, recursive imports and intention files.
Relative imports retain their directory structure. Absolute paths, parent
traversal and symlinks are outside this first tool's snapshot contract. Hashes
bind the compiler, tool/helpers, manifest, all source dependencies, input data,
profile and emitted binaries. Compilation is repeated to check reproducibility.
The compiler workload report supplies the ordered native input field schema;
the tool does not infer argument order from JSON object ordering.

## Data and functional checks

Each declared workload case requires separate selection and validation JSON
batches with exactly its predicted record count. Identical batches under
different filenames or JSON formatting refuse. Authors remain responsible for
representative independent data; different bytes alone cannot establish that.
Explicit additional JSON batches and raw argv checks cover rare and invalid
inputs without contributing to performance scores. An empty native argv check
is mandatory even when the manifest supplies no additional checks.

Before timing, run the original interpreter and every candidate on both sets
and additional checks. Compare exit status, stdout and stderr within each
backend. Native/interpreter stdout and status must also agree on successful
records and boolean-failure batches; input-error diagnostics remain backend
specific. Time only successful, stderr-free cases in this slice. Check the
native signatures again after timing. Test agreement supplements the narrow
structural change restriction; it is not a proof of arbitrary algorithm
equivalence.

An observed difference excludes that candidate before timing and keeps both
transcripts. In particular, today's sequential entry writes
`error: not enough arguments` on empty argv, while the concurrent entry exits
with empty stderr; both exit 1. The mandatory probe therefore excludes current
mode switches. This is an explicit backend compatibility limit, not a difference
the experiment may ignore for speed. Concurrent lane/batch changes remain useful
candidates. Byte-identical binaries are retained but not scored as improvements.

## Measurement and decision

Builds, correctness checks and measurements occur in separate stages. Timed
native invocations run serially, with stdout sent to `/dev/null`, fixed declared
CPU affinity and balanced rotating candidate order. Keep every raw sample,
aggregate child CPU accounting, wall time, environment and host-load note.
CPU includes workers; wall time includes process launch and wait. Clock probes
and impossible CPU/accounting results make the experiment inconclusive.

Score the source objective using the weighted arithmetic mean of complete
invocation times, never medians or record-volume weights. Select at most one
candidate using selection data only. Require a declared minimum relative gain
(default 5%) both overall and in each half of the sample series, and require
every latency target to hold on the measured arithmetic mean for its case.
This stability filter is not a confidence interval or a deadline guarantee.

Freeze that choice before measuring validation data; validate it against the
baseline with the same criteria. A failure does not trigger a search through
other candidates on validation data. No admitted improvement, target failure,
unstable gains, changed identities or accounting anomalies produce no proposal.
Retain the complete report and evidence in every outcome. This experiment
optimizes the declared weighted objective; untargeted individual cases may
regress, and their separate means must remain visible.

A successful run emits the candidate source and a unified diff, rechecks the
source dependencies/compiler against their initial identities, and reverifies
and rebuilds the exact proposed source. The original project is never changed.
Applying a proposal is a separate reviewed action against those same identities.
There is no automatic commit, deployment or general algorithm rewrite.

## Manifest and invocation

The JSON manifest has closed keys. Paths are relative to the manifest; the
source's directory is the root of the dependency snapshot. Candidate names are
unique identifiers (`baseline` and `proposal` are reserved), with at most eight candidates.
The optional `budgets` object supplies only a missing mode's ceiling; it cannot
override a ceiling in the original source. `checks` and `argv_checks` are
required arrays, possibly empty (empty arrays mean no additional coverage).

```json
{
  "schema_version": 1,
  "source": "workload_profile.verbose",
  "execution": "inspect_usage",
  "budgets": {"native_stack": 192},
  "variants": [
    {"name": "sequential", "mode": "sequential"},
    {"name": "batch64", "mode": "concurrent", "max_in_flight": 2, "result_batch": 64},
    {"name": "batch128", "mode": "concurrent", "max_in_flight": 2, "result_batch": 128}
  ],
  "cases": {
    "interactive": {"selection": "small-a.json", "validation": "small-b.json"},
    "bulk": {"selection": "bulk-a.json", "validation": "bulk-b.json"}
  },
  "checks": ["edges.json"],
  "argv_checks": [["a", "1", "b", "invalid", "c", "3"], []]
}
```

```sh
python3 tools/experiment_workload.py experiment.json \
  --compiler target/release/verbosec --cpus 2 4 6 8 \
  --host-note 'No significant external load' --output /tmp/workload-experiment
```

The output directory must not exist. It retains snapshot projects, inputs,
binaries, compiler transcripts, functional signatures and a JSON report.
`--repeats` defaults to 32, rounded up to complete balanced cycles in each half;
`--min-gain-pct` defaults to 5. No proposed change is a normal measured outcome
(exit 0), accounting/identity instability is inconclusive (exit 2), and invalid
configuration, baseline inconsistency or a post-measurement functional/build
inconsistency fails (exit 1). A candidate's pre-measurement compiler/functional
refusal alone does not fail the experiment; a rejected baseline does.

For a complete reproducible example, first run:

```sh
python3 tools/workload_experiment_example.py /tmp/reading-workload
python3 tools/experiment_workload.py /tmp/reading-workload/experiment.json \
  --compiler target/release/verbosec --cpus 2 4 6 8 \
  --host-note 'Describe current host load' --output /tmp/reading-experiment
```

Choose CPUs allowed on your machine. The generator makes an explicitly
**unbatched** version of the reading example, preserving its 20 KiB source
ceiling and 99:1 invocation profile. It supplies two separate batches per case,
empty/negative/i64-extreme/overlong/invalid argument checks, four result-batch
sizes and a sequential candidate. A batch of 128 exceeds this ceiling and is
recorded as refused; the sequential candidate fails the diagnostic comparison.
This does not claim an improvement over the already-batched repository example.

Supported measurement inputs are flat `number`/`text` records, with exact i64
JSON integers and NUL-free UTF-8 argv strings. Input bounds are still checked by
the compiler-generated entry and interpreter. Host argument-size limits can
refuse large cases; this slice does not substitute a different input channel.
The sequential stack and concurrent reservation reports are kept separately;
no RSS, cache residency, tail-latency, sustained-service or TLS claim follows.

| Path | Support |
|---|---|
| Rust compiler workload report | Additive ordered `input_fields` schema; native code unchanged |
| Offline native experiment | Linux x86-64, explicit candidates and data, existing strict pure executions |
| Interpreter | Untimed functional reference and candidate comparisons |
| Proposal | Reviewed source patch plus verified binary identity; original source unchanged |
| WASM / self-hosted compiler | Existing execution refusal; no experiment target |
| Algorithm rewriting / automatic application / service tuning | Not implemented |

## Validation

Tests must cover weighted arithmetic scoring, CPU versus elapsed objectives,
targets, noisy/reversed validation, no second-choice validation search, refusal
retention, closed manifests, unchanged original files, import identity changes,
source strings/comments/shadow declarations, explicit cross-mode ceilings,
reproducible emission, ordered numeric/text input and failure prefixes. Include
an end-to-end experiment whose decision is not asserted from noisy real timings;
test decision branches separately with deterministic synthetic samples. Run the
normal Rust suite serially, relevant Python suites and CIDX before delivery.
