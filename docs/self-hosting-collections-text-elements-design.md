# Collections: TEXT element fields (the last collections gap)

## Goal
`collection(Concept)` where the element concept has `text` fields, in reductions and
JSON output. The motivating shape is roster.verbose (verbosec `--native` verified):
`Employee{name:text, salary:number}`, `fold(w.employees, "roster: ", acc, e =>
concat(acc, e.name, "=", e.salary, "; "))` → `roster: alice=100; bob=200; `. And
record JSON output with a text field → `{"name":"alice","salary":100}`. Today slices
5/6 gate on `field_list_all_number` — text element fields int3. This lifts that.

Oracle: roster.verbose (text fold) + a JSON map/filter over a text-field concept,
both verbosec `--native`.

## The one hard piece — per-element text via the MAP_FIXED region
Entry-input text fields already work: the trampoline mmaps a FIXED region at
0x20000000 and copies the argv string there, packing a text value as
`pack((0x20000000 - src_base) + bump, len)` — a COMPILE-TIME-CONSTANT start (region
offset) with a runtime len. Reading it computes `src_base + start = 0x20000000 +
offset` → the copied bytes.

For a COLLECTION element, only the CURRENT element is live during a streaming
reduction, so per element:
- for each TEXT field k of the element concept, copy that element's argv string to a
  FIXED region offset `off_k` (distinct per text field, e.g. field 0 → 0, field 1 →
  a slot past field 0's max), strlen → len, and store the packed span
  `((0x20000000 - src_base + off_k) << 32) | len` into the element's arena node slot.
  The start is compile-time-constant; only `len` is runtime — build it as
  `mov rax, imm(start<<32) ; or rax, rlen`.
- for each NUMBER field, atoi as today.
The region is OVERWRITTEN each iteration (only the current element is read before the
next), so no cumulative bump — fixed per-field offsets, region size bounded by the
element concept's text field count × a per-field cap. Reading `e.name` in the fold
body / JSON resolves the packed span → the region → the current element's bytes. ✓
(the pack convention MUST match the entry-text-field marshal exactly — mirror it.)

## Changes
1. **`x86_elem_field_loads`** (slice 6, currently atois `ef.rem` fields): thread the
   element concept's FIELD LIST so it dispatches per field on type — text → the
   region copy+pack (mirror the entry-text-field marshal's copy+strlen+pack bytes),
   number → atoi. Each pushes an i64 (packed span or value) into the node slot.
   `code_size` mirror per field type.
2. **Trampoline MAP_FIXED setup**: the region is mmap'd today only when
   `field_list_has_text(ecfields)` where ecfields = the ENTRY concept's fields. For a
   `collection(Concept-with-text)` entry, the entry field is the collection (not
   text), so the region isn't set up. Extend the trigger: set up the MAP_FIXED region
   also when the entry's collection ELEMENT concept has a text field. (Find the
   trampoline's has-text check; OR the element-has-text into it. Keep the size
   accounting in lockstep — blob_end_off.)
3. **`x86_json_record`** (the record-JSON slice): a TEXT field → `"` + the text bytes
   (write the span from `src_base + start`, the AstStr path — same as the field NAME
   write) + `"` (a QUOTED string value), instead of itoa. Number field → itoa (today).
   `code_size` mirror per field type.
4. **Gates**: replace the `field_list_all_number` int3 gate (slices 5/6 fold + the
   JSON slice) with text-aware handling; a text field now compiles instead of
   trapping. Keep numbers-only programs BYTE-IDENTICAL (the text path is a new branch;
   an all-number element takes the unchanged path — SHA-gate it).

## Scope
- Text fields readable in fold-body concat (`e.name`) and JSON output (quoted).
- NO json_escape of the text value in JSON (verbosec's Phase-3 record JSON does not
  escape — verify: it emits the raw bytes between quotes; match whatever verbosec
  does exactly, escape or not).
- Region size: bounded per element (fixed per-field offsets). A very long argv text
  beyond the region cap is out of scope (same posture as the entry text field's
  region cap).
- filter over a text-field concept (identity JSON with text) falls out of #1+#3.

## Gate (clean disk)
1. proofs check out; suite green; all-number collection binaries BYTE-IDENTICAL
   (SHA — the text path must not touch them).
2. two_generation gen1==gen2 (self-source uses no collections).
3. MILESTONE: gen1 compiles roster.verbose's text fold → byte-identical to verbosec
   `--native` (`roster: alice=100; bob=200; ` + a trailing newline the entrytx
   trampoline adds — confirm the exact bytes); and a JSON filter/map over
   `Employee{name:text,salary:number}` → `{"name":"alice","salary":100}\n...`
   byte-identical to verbosec. Empty collection edges. A Rust test mirroring the
   slice-5/6/json tests.
