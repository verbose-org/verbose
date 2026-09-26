# Self-hosted lexical binding resolution

Design fixed before implementation, 2026-09-26.

## Contract

The compiler written in Verbose must resolve an identifier to the latest visible
binding. A let's RHS sees earlier bindings and parameters; its new value becomes
visible afterward. An alias captures that earlier value and representation.
Nested match/reduction binders shadow outer lets and parameters only within their
body. Forward references and references to an otherwise unbound self remain
verification errors before an ELF artifact is emitted.

The evaluator's front-consed environments already implement this order. Native
self-hosted emission instead searches the full source-order list for its first
match, prefers parameters over lets, and recognizes only literal text RHSes when
printing a binding. This can read the wrong frame slot or print a packed text
descriptor as a number. Both sizing and emission must use the same resolution.

## Representation and lowering

- Keep source-order frame slots and the existing one-word values: numbers,
  packed text spans and record/variant indices. No target allocator, GC, runtime
  environment or new calling convention is introduced.
- Resolve the last matching visible binding, falling back to a parameter only
  when no let or nested binder matches. A nested binder is already appended at
  the end of the binding list, so the same lookup gives it precedence.
- For each let RHS, mask the current and later entries' names and values in the
  compiler's binding view. Preserve list length and positions: nested temporary
  slots still begin after the full let frame, and existing displacements stay
  stable. The source AST and eagerly emitted stores are unchanged.
- Resolve an alias's representation through its RHS in the view preceding that
  definition. This strictly reduces the visible binding index; it cannot follow
  a same-name alias into itself or into a later declaration.
- Share the existing packed-span classifier between stream sizing, stream
  emission and equality classification. Follow literal/field/parameter/slice
  aliases only where the underlying value representation already exists.
  Resolve record aliases through the same preceding view for field layout.

Fresh text produced by a concat or streaming text-returning call still cannot be
stored as a one-word value. Existing capability refusals remain; general streamed
substring output, WASM text conditionals and a general text ownership model are
separate work. This correction concerns compiler-side resolution, not a new
public type system or backend-wide support claim.

## Verification

Compare explicit results with original-AST interpretation, the self-hosted
evaluator for scalar observations, Rust native where supported, and programs
emitted by gen0 and gen1. Cover literal/dynamic aliases, multiple redefinitions,
self-aliases and own-RHS reads, scalar/text changes, equality, records, nested
binders and parameter shadowing. Preserve eager failures in overwritten lets.
Negative cases include forward/self references, mistyped aliases and existing
fresh-text refusals; emission must fail without artifact bytes.

Compare the unchanged example corpus with a saved parent self-hosted emitter,
checking acceptance, diagnostics, deterministic bytes and explaining any changed
emission. Run the serialized normal Rust suite, Python/CIDX checks and the full
two-generation bootstrap with the new probes included after self-compilation.
No performance benchmark is part of this slice.
