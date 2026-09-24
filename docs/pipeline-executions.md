# Bounded record pipelines

Design recorded before implementation. A source execution can compose pure
rules into one per-record pipeline, instead of running independent phases over
the original batch. This is useful for preparing, normalizing and rendering data
in command-line tools, compiler passes and eventually service handlers. It does
not introduce a transport or a general concurrent dataflow runtime.

## Source contract

```verbose
execution prepare_readings
  @intention: "Prepare and render each reading within one checked frame"
  @source: pipeline.intent:1
  input: Reading
  mode: pipeline
  phases: [prepare, forward, render]
  on_failure: stop
  native_stack: 512
```

`pipeline` explicitly selects different semantics from `sequential` and
`concurrent`; it is not an optimization hint. There are 2..64 ordered phases,
including repeated rules. The first phase consumes the declared input concept.
Each following phase consumes the preceding phase's result: the nominal record
concept must agree, and every transferred text capacity and numeric range must
fit the receiving rule's public input contract. Input variable names can differ.
Intermediate results must be flat records of numbers and bounded text. Final
results can be numbers, booleans, bounded text or supported flat records.

Every phase and its transitive callees must fit the existing pure, acyclic
bounded-text composition subset: literals, fields, aliases, conditionals,
comparisons, length, concat, flat records and checked calls. This execution
declaration opts its composition into that strict analysis, including when no
phase returns text. It does not admit arithmetic merely because a legacy rule
previously accepted it. Unknown capacities, unsupported expressions, effects,
recursion, context inputs, hints, collections, nested records and Result values
refuse explicitly. Original source proof checks still apply to every rule.

The pipeline is sequential and processes one record through all stages before
starting the next. Intermediate values are private; only the final value is
published. A final boolean false is printed and remembered until all valid input
records have been processed, then status 1 is returned, as for an ordinary
boolean rule. Fatal input/evaluation errors stop immediately, preserving
the completed output prefix. Interpreter writer errors also stop further work;
native output retains the existing ordinary-rule syscall behavior.
`on_failure: stop` does not imply recovery or
rollback. Intermediate booleans refuse because they cannot feed a record input.

`native_stack` is required with the existing 1..2,097,152 byte limits. Concurrent
resource fields refuse. Predicted workload blocks and the experiment tool refuse
this mode in this slice: current workload accounting assumes independent phase
publication. Existing sequential/concurrent semantics remain unchanged.

## Reference and native lowering

The interpreter executes the original verified phase ASTs directly, moving each
record value to the next phase; it does not evaluate a synthetic wrapper. JSON
events name only the final phase, with its original one-based phase position and
zero-based input record index. The interpreter retains the decoded input batch
and uses host allocations; its memory is outside the native contract.

Native lowering builds an internal ordinary-call composition equivalent to:

```verbose
let stage0 = prepare(input)
let stage1 = forward(stage0)
out = render(stage1)
```

Each call argument is evaluated once. Original rules retain their source proofs;
the compiler-generated orchestration is checked by the same type/range/capacity
analysis as ordinary calls. It is not a claimed author-supplied proof. The native
emitter expands acyclic calls into one invocation frame and reuses its existing
word and buffer placement. No new calling convention, allocator, GC, runtime
lifetime table or queue is needed. Immutable aliases retain their existing
owners, and lifetimes extend through downstream uses before storage is reused.

Pseudocode: guard and decode an argv record; enter the fixed invocation frame;
evaluate each stage into placed values; publish the last result; release the
frame; advance to the next record. Registers, frame offsets, text destinations,
saved registers and transient formatting scratch are managed entirely by the
existing bounded-text emitter. Output references remain valid until publication
finishes. No stage emits a standalone entry or parses argv again.

The additional entry stack bound describes the **complete shared composition**,
including overlapping retained data; it is neither the sum nor the maximum of
standalone phase bounds. Every declaration, selected or not, is checked before
artifact creation. Phase-level `proofs.native_stack` checks retain their own
standalone meaning. The source AST used for the report is also used for native
pipeline lowering, so optimizer passes cannot silently change the reported
layout. Existing bounded expansion/frame limits still apply.

`--stack-report` provides a schema-1 pipeline report with execution, input,
ordered phase names, declared bytes, `composition: "pipeline"`, publication and
record-order semantics, and one nested invocation report. Its total comes from
the emitter; call-retention details describe capacities already included there,
not extra allocations. Initial argv/environment, OS/kernel memory and interpreter
storage remain excluded; this is not a total-process or cache-residency claim.

## Backend boundary and verification

| Path | Contract |
|---|---|
| Parser/verifier | Check every pipeline, including transfer and storage bounds |
| Interpreter | Original-AST phases over strict JSON records, final events only |
| Linux x86-64 native argv | One shared checked frame per record |
| Native stdin/raw/stream and HTTP wrapper | Refuse before artifact creation |
| Concurrent pipeline, workload experiments | Refuse in this slice |
| WASM and self-hosted compiler | Existing execution-declaration gates refuse |

Validation will compare direct phase interpretation, explicit ordinary-call
composition and native pipeline output/status. Cover UTF-8 byte boundaries,
empty strings, i64 extremes, aliases, shadowing, conditional records, repeated
stages, final scalar/record forms, final boolean failure and invalid later input.
Refusals cover mismatched concepts, unsafe field transfer, unknown/effectful/
recursive forms, resource fields, unselected insufficient budgets and unsupported
backends without overwriting artifacts. Check exact/one-byte-small budgets,
deterministic emission and unchanged pre-existing example binaries. Run the
serialized Rust suite, relevant CLI tests and CIDX controls before delivery.

Further clock calibration and performance comparisons are deferred as of
2026-09-24. The previous workload experiment remains inconclusive; this slice
will claim checked behavior and storage only, without a measured speedup.
