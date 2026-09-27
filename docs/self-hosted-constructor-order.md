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
ambiguous declarations refuse. Walk all expressions, including initializers,
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
self-source and deliberate refusals/corrections. Deliver as a separate PR based
on #260 while that prerequisite remains open.
