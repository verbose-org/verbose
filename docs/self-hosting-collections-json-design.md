# Collections: record map/filter → JSON collection output (self-hosted)

## Goal
Extend the self-hosted map/filter (slice 4b did `collection(number)` → decimal-per-line)
to RECORD element output: `filter(w.orders, o => pred)` over `collection(Order)` →
`collection(Order)`, and `map(w.orders, o => Out{...})` → `collection(Out)`, each
serialized as verbosec does (Phase 3 record output): one JSON object per element,
fields in declaration order, `{"name":val,...}\n`, empty collection → nothing, exit 0.
Numbers-only fields (text element fields stay deferred).

Oracle (verified, verbosec `--native`):
- filter: `filter(w.orders, o => o.amount > 100)` on `3 200,1 50,2 300,3` →
  `{"amount":200,"priority":1}\n{"amount":300,"priority":3}\n`
- map:    `map(w.orders, o => Tagged{v: o.amount*2, p: o.priority})` on `2 100,1 50,3` →
  `{"v":200,"p":1}\n{"v":100,"p":3}\n`

## The unifying insight — one JSON-serialize helper
Both cases reduce to **serialize an arena node of concept C to JSON**:
- filter: C = the INPUT element concept (identity pass-through — verbosec's filter emits
  the input element); the node is the slice-6-loaded element (its arena index sits in the
  item binder slot).
- map: C = the map body's CONSTRUCTOR concept; the node is the body evaluated
  (`x86_node(body)` on the `Out{...}` AstVariant leaves the arena index — the same
  construct the reductions already emit).

So add ONE emit helper `x86_json_record(concept C, node-index-on-stack)` and call it from
both arms.

### `x86_json_record` shape (per field i of C, decl order)
- separator: `{` before field 0, `,` before each later field (literal bytes).
- key: `"` + the field NAME bytes (write from `src_base + name_start`, len `name_len` —
  the AstStr src-blob write path, since field names live in the embedded source) + `":`.
- value: read the field from the arena node — `mov rax,[node_addr + 8 + 8*i]` (the slice-6
  / AstField slot layout: node_addr = r15 + index*entry_size; payload at +8+8*i) — then
  itoa it (the slice-4b/5 `push r9 ; call itoa_proc ; pop r9` pattern, r9 = the loop
  cursor, itoa clobbers it). Numbers only this slice.
- after the last field: `}` then `\n` (literal).
Node-index handling: pop the index once, compute node_addr = r15 + index*entry_size
(`imul` + `add r15`, the AstField/AstMatch prologue), keep node_addr in a callee-safe reg
across the per-field itoa calls (itoa clobbers rax/rcx/r9/r11 — pick a reg it preserves,
e.g. save node_addr on the stack or in a reg outside itoa's clobber set; VERIFY against
itoa_proc's body which regs survive). `code_size` mirror is per-field-count (a compile-time
constant from C's field count + the field-name byte lengths — all statically known).

## Wiring into the map/filter arms (x86_stream_node)
The rule is collection-output (entry_rule_collection, out_ty==3). Today slice 4b's arms
assume the element type is `number` (itoa). Add: resolve the OUTPUT element concept:
- filter → the input collection's element concept (`static_concept_of(coll)` — slice 6's
  resolver; identity output).
- map → the body's constructor concept (the `Out{...}` AstVariant's concept span).
If that element type is a CONCEPT (not "number"), route to `x86_json_record`; else keep the
slice-4b scalar decimal path (byte-identical — collection(number) map/filter must not
change; SHA-gate it). The loop scaffold (r8 count / r9 cursor / slice-6 element load for
the input record) is unchanged; only the per-element EMIT (serialize vs itoa) differs.
- filter record: slice-6 load the input element (arena node) → eval pred (o.field access
  works via the slice-6 binder) → jz skip → x86_json_record(input concept, element node) →
  skip.
- map record: slice-6 load the input element → `x86_node(body)` (the `Out{...}` constructor
  → arena node index) → x86_json_record(output concept, that index).

## Eval (eval_ast_env)
Already: AstMap/AstFilter → VData(COLLECTION_TAG, mapped/filtered ValueList). The elements
are now VData records (not VNum) — eval already carries them (record VData flows through
VECons binding + the constructor eval). No change expected; confirm the arms are
element-type-generic and NOTE if not (compiled path is the tested one, as prior slices).

## Guards / scope
- Numbers-only record fields (text field → int3, the slice-6 `field_list_all_number` gate).
- map body must be a record constructor of the declared output element concept (else the
  int3 declared/body-shape posture).
- filter output element == input element (verbosec's rule); a mismatch → int3.

## Gate (clean disk)
1. proofs check out; suite green; collection(number) map/filter binaries BYTE-IDENTICAL
   (slice 4b unchanged — the scalar path is the else branch).
2. two_generation gen1==gen2 (self-source uses no collections — unaffected).
3. MILESTONE: gen1's filter/map record output BYTE-IDENTICAL to verbosec `--native` on the
   oracles above, incl. empty → nothing exit 0. A Rust test mirroring the slice-4b/6 ones.

## Honest scope
Numbers-only fields; map body = a direct record constructor; filter = identity. Text
element fields, nested/computed constructor shapes beyond `Concept{field: numexpr}`, and
collection-returning composition stay deferred.
