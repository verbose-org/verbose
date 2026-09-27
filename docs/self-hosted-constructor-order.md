# Self-hosted named constructor fields

Design fixed before implementation, 2026-09-27. Parent: `300565b` (#260).

Records and variants carry named constructor fields. Evaluate every initializer
once, in written order; place its value in the slot belonging to its declared
field name. Field access and positional match binders then use declaration
order. This applies to existing supported payload representations; it adds no
new field type or ABI.

The self-hosted evaluator currently builds a positional value list in source
order, and its emitter pops source-order values into positional slots. Both
therefore confuse reversed fields. Share declaration lookup between validation,
evaluation and emission. Keep the evaluator's initial source-order evaluation,
then permute already evaluated values into declaration order. An identity
permutation reuses the existing value list. Reordered values may need new list
nodes in the existing evaluator arena; this is not a zero-allocation claim.

Native emission keeps the existing source-order pushes. Walk the constructor
fields backward when emitting pops and resolve each destination offset by name.
Each pop/store remains eight bytes. The target needs no new instructions,
storage, allocator or GC; declaration-order constructors retain their previous
machine code. Name resolution happens in the compiler.

Check constructor identity and a complete, unambiguous field mapping before
emission: unknown concepts/variants, missing/unknown/duplicate fields and
ambiguous field declarations refuse. Walk all expressions, including initializers,
calls, branches, lets, match arms and collection bodies. Integrate with the
existing rule type-error category; the raw emitter also needs the mapping gate
before any bytes. This is field-layout checking, not full payload type-checking
parity with Rust. The legacy unchecked evaluator keeps its defensive behavior.

The original-AST Rust interpreter is the semantic reference. Compare the
self-hosted evaluator and emitted binaries on field permutations, mixed scalar
and text fields, nested records/variants, calls, aliases, branches and positional
binders. Check source-order failures separately: the Rust native variant path
currently evaluates fields in declaration order, so it cannot serve as that
order oracle. Existing evaluator error/effect stubs remain outside this slice.

Verify the matrix again after self-compilation; run the serialized normal suite,
full bootstrap and CIDX checks. Compare the example corpus against the saved
parent emitter with repeated emissions, accounting separately for changed
self-source and deliberate refusals/corrections. Deliver as a separate PR after
the prerequisite #260 (merged as `fe2fc4c`).

## Implementation and boundaries

`constructor_fields` borrows the record fields or the named variant payload from
the parsed declarations. `eval_vfields` still evaluates in source order;
`eval_payload_order` reuses an identity list or constructs declaration-order
list nodes referring to those evaluated values. It does not copy text or repeat
an initializer. Those nodes follow the evaluator's existing arena lifetimes;
this slice does not add a general interpreter memory budget.

`x86_vfields` still emits one source-order evaluation per initializer.
`x86_vpops` now descends to the last initializer and emits each existing
pop/store with its declared destination offset. `code_size_node` retains the
same constructor size formula. Name lookup is compile-time work. For
source-declared constructors, target layouts, tags, stack pushes, arena node
widths and allocation counts remain unchanged.

The layout checker requires equal arity, unique written names, and a declared
slot for every name. Together these establish a bijection and reject duplicate
field declarations as well. The complete AST walk joins the existing type-error
category (4), including constructors inside unused lets, calls, unselected arms
and collection bodies. Compiler checking temporaries are reclaimed by scalar
arena scopes. The raw emitter gates this layout check before its first byte;
it remains a lower-level entry rather than a complete source verifier.

Synthetic HTTP concepts have anonymous field metadata. A closed built-in lookup
resolves `status/body` and `method/path/body` instead of treating those anonymous
slots as wildcards. Raw emission now receives the same HTTP concept metadata as
ELF emission, correcting its previously missing built-in tags and node stride.
The service backend retains its existing positional response-shape
restrictions; this does not add arbitrary constructor reordering to HTTP handler
syntax or turn the raw function blob into a runnable service.

| Path | Behavior in this slice |
| --- | --- |
| Original-AST Rust interpreter | Source-order semantic reference |
| Verbose evaluator, including after self-compilation | Named values in declared payload order; existing unchecked error/effect behavior |
| Self-hosted ELF emission | Verified field mapping; unchanged instruction/storage count for source-declared constructors |
| Self-hosted raw emission | Same placement, with layout refusal before any bytes |
| Rust native and WASM | Existing acceptance/lowering unchanged; no new support claim |

The regression matrix includes all six permutations of three record and variant
fields, mixed text/number/bool values, nested records and variants, aliases,
calls and record JSON output. Distinct bounds/division failures and an unused
failing field exercise native source order and eagerness. The unchecked
evaluator is compared on valid values, not used as an error/effect oracle.

The [example](../examples/constructor_order.verbose) writes `second` first,
then `first`, and returns `first * 100 + second`. Input 7 produces 708. The
minimal constant pair witness returned 21 on parent `300565b` instead of 12;
both the corrected evaluator and emitted binary return 12. Its binary is
635 bytes before and after the correction; only the two destination-offset bytes
change (16 to 8 and 8 to 16).

The self-source also contains two real reversed constructions: `type_check` and
`check_program` pass `TCheckListState` with `prog` before `src`, although its
declaration lists `src` before `prog`. They retain their written order. Bootstrap
probes emit and run both entry points to verify their valid/invalid source
diagnostics, alongside the raw emitter's constructor refusal matrix.

## Recorded validation

The serialized normal suite passes 878 tests (28 explicitly ignored); the seven
Python harness suites pass 97 tests. CIDX configuration, environment and security
checks pass, with the existing Python dependency findings still reported.

All 27 two-generation bootstrap checks pass. The fixed point is byte-identical,
the whole-source comparison succeeds, and both gen0 and gen1 run the constructor
matrix through emitted evaluators and native binaries. Their emitted raw,
`type_check` and `check_program` drivers also pass the valid/refusal probes.

The parent and corrected emitters were compared on identical input for all 193
current example sources, at rule index 0. Both accept 94 sources, including the
new example; acceptance and diagnostics agree throughout. Two emissions per
compiler/source confirm reproducibility. This is first-rule acceptance, not a
claim that every subject entry in these files is supported.

Of those sources, 191 retain byte-identical output. The new `constructor_order`
example retains its 680-byte binary size, changing only two payload offset bytes
(16 to 8 and 8 to 16). Compiling the updated self-source changes only four bytes:
the two `src`/`prog` destination offsets in each of `type_check` and
`check_program` (24 to 16 and 16 to 24). Both emitters produce 3,167,266 bytes for
that identical self-source; the corrected ELF has SHA-256
`de48f7e88a9653196cd0c2bcb3685da48a0d895d645abb8f0e6ec10f12da9f8e`.
This comparison isolates the emission correction; it does not claim the grown
self-source or its compiler binary is identical to the parent's version.
