# Memory management and garbage collection

Verbose's implemented native storage paths use compile-time placement and
regions, rather than a tracing garbage collector. Reuse is automatic within
their supported contracts; authors do not manually free individual numeric or
bounded text values. This is not a claim of universal ownership or memory safety
for every language/backend combination.

## Three concrete mechanisms

| Storage | How it is managed | Present limit |
|---|---|---|
| Strict native numeric values | The compiler assigns one-word stack slots from checked lexical lifetimes. Locals, expression scratch and expanded calls reuse dead slots. | Pure acyclic numeric argv subset; a separate 2 MiB frame ceiling. |
| Bounded native text | Each invocation owns a fixed region. Checked capacities, alias lifetimes and exclusive branches allow buffers to share space; output can be written directly into its destination. | Supported bounded text rules/handlers; transport, input and persistent state have separate storage. |
| Recursive structures and self-hosted compiler intermediates | Arenas allocate nodes within declared capacity. Supported `arena_scope` paths reclaim scoped allocations together after their result is consumed or a scalar survives. | Reclamation depends on declared boundaries and supported result/backend forms; long-lived data can retain substantial storage. |

See [numeric storage](numeric-local-lifetimes.md),
[bounded text storage](bounded-text-storage.md), and the shipped
[streaming](self-hosting-arena-scope-design.md) and
[scalar](self-hosting-scalar-arena-scope-design.md) arena slices. Arena documents
contain dated measurements and implementation history, not current global memory
budgets. Native concept-group arenas can use OS mappings when too large for the
small-arena stack path. Absence of GC does not mean absence of allocation.

## Difference from a tracing collector

A tracing GC determines during execution which heap objects are still reachable
and reclaims unreachable ones. Collectors differ: they may run incrementally or
concurrently, and some move objects to compact storage. Automatic memory
management also includes other approaches, such as reference counting.

For the bounded numeric/text paths above, the compiler determines storage
lifetimes and offsets before execution. The executable does not scan an object
graph or count references to decide which of these slots/buffers are reusable.
Arena scopes instead reclaim a whole region at a known boundary; that reset is
runtime work, but it does not discover individual unreachable objects.

The tradeoff is less runtime bookkeeping for these values, with a more restricted
program shape. Unknown capacities, unsafe escaping values or unsupported forms
cannot be accepted under a contract that requires a proof. A GC can support
lifetimes and shared object graphs that cannot be assigned these simple regions
in advance. Neither approach is intrinsically faster for every workload.

## What a smaller frame establishes

Reusing a slot does not compress its value: a numeric i64 still occupies eight
bytes, with no decode step. It means storing different values at the same address
at different times. The compiler-generated loads and stores use that address
directly; no runtime decision about lifetime is added.

Less reserved storage can help reduce the working set, but does not establish
lower RSS or better cache hit rates. Resetting an arena or finishing a request
makes storage reusable; it does not necessarily erase bytes or return pages to
the OS. The process also owns stacks, input data, transport buffers, mappings and
other state outside any one bounded region.

These descriptions concern emitted native programs. The Rust compiler and
interpreter have their own host-language allocations; the compiler written in
Verbose also has its own substantial parse-tree and arena storage. A small
output executable does not imply a small compiler process.
