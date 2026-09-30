# Self-hosted collection composition boundary

## Design fixed before implementation

The self-hosted emitter streams a rule's final `map` or `filter` directly to
stdout. It has no intermediate collection representation. Its value emitter,
`x86_node`, emits `int3` for either producer. Consequently a well-typed
`sum(map(i.items, x => x + 1), x => x)` can compile and then trap.
The equivalent `filter` composition has the same defect. The parent compiler
at `2652e46` reproduces both traps; the direct `fold` control prints `16`.

This slice adds a compiler capability check, independently of language typing:

- A `map` or `filter` is supported only as the final, top-level expression of
  a rule declaring a collection output. Its children are value positions.
- Every producer in a let, argument, constructor, condition, branch, match arm,
  Result or another collection operation is refused, including unused lets and
  inactive branches. Aliasing cannot hide the original unsupported producer.
- Calls to rules declaring collection outputs are refused: their procedures
  stream output rather than return a collection value.
- A rule declaring a collection output must have a top-level producer. A
  conditional, alias, input-field identity or forwarding call is not lowered
  by the current procedure emitter.
- Every parsed rule is checked, including uncalled rules and rules preceding
  a selected ELF entry. Raw x86 and ELF refuse before writing any bytes.

A complete AST walk counts unsupported producers and collection-returning
calls. At each rule boundary, exactly one producer is exempted when its result
is a top-level producer and its declared output is a collection. A separate
`collection_lowering_check` driver exposes the capability-error count; the
existing type checker remains independent. Refusal uses the existing compiler
policy: exit 1, empty stdout and stderr.

This does not add a collection value ABI, loop fusion, a heap or a GC. Fusion
would need a separate design preserving eager producer evaluation, failure and
effect order. Existing supported reduction/output emission is unchanged. Other
legacy collection input/element layout and general typing gaps remain outside
this guard; a zero count is not a claim that every backend capability is proven.

## Validation planned

Check all producer placements, both producers, every reduction/quantifier,
aliasing, shadowing, inactive branches, uncalled rules and nonzero entry
selection. Assert type checking still accepts well-typed compositions, while
both output drivers refuse with no artifact. Compare accepted direct reductions
and streamed outputs against the original-AST interpreter and the parent emitter
on empty and populated inputs, including observable failure. Run the matrix on
both bootstrap generations, the serialized normal suite, Python harnesses,
CIDX controls and the existing corpus/fixed-point checks.

The design commit is based on the parent PR's green normal suite (888 tests),
27 bootstrap tests, 97 Python tests and all six remote checks on `26f1abd`.
