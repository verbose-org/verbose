# The negative corpus

Small, deliberately-**invalid** `.verbose` programs. Each isolates ONE thing
`verbosec` refuses. They exist to measure a surface nothing else in this repo
measures: **whether the self-hosted compiler (gen0, `examples/vexprparse.verbose`)
refuses what `verbosec` refuses.**

Run the sweep:

```sh
cargo test --release -- --ignored --test-threads=1 two_generation_negative
```

It lives in `src/native.rs` as `two_generation_negative_corpus_sweep`, and the
`two_generation` prefix puts it in the existing `self-hosting-bootstrap` CI job.

## Why this directory has to exist

Every other gen0 measurement in the repo feeds **valid** programs:

| measurement | what it can see |
|---|---|
| `two_generation_corpus_acceptance_sweep` | how many valid programs gen0 accepts |
| the differential harness | whether both compilers agree on OUTPUT, for programs both accept |
| `two_generation_bootstrap_fixed_point` | that gen0 reproduces itself byte-for-byte |

**None of them can see a verifier that is too permissive.** A check gen0 simply
does not perform is invisible to a suite made entirely of programs that pass it.
Acceptance going *up* even reads as progress. The only way to measure the
refusal surface is to feed programs `verbosec` rejects and assert gen0 rejects
them too — which requires programs that are not in `examples/`, because
`examples/` is by construction the set of things that work.

## Layout and conventions

- Every fixture `@source`s into **`negative.intent`** in this directory.
  Without it `verbosec`'s `@source` existence check fires first and every
  fixture "fails" for a reason unrelated to the defect it isolates — the
  measurement would be worthless. This bit the first draft; keep it.
- Fixtures are **not** picked up by the corpus-acceptance sweep: that sweep
  reads `examples/` non-recursively and filters on the `.verbose` extension, so
  a subdirectory is excluded. `EXPECTED_TOTAL` stays 151.
- The sweep's `INVERSE` set — fixtures `verbosec` ACCEPTS and gen0 refuses — is
  currently **empty**, and the constant is kept as a tripwire rather than
  deleted. Its one occupant, `bad_arity.verbose`, was an inverse case from
  2026-08-10 until 2026-08-13: `verbosec` had no rule-call arity check at all,
  so `helper(i, i)` on a one-input rule verified clean while gen0 refused it.
  Keeping it asserted — instead of filing it away in some bucket — is what kept
  it visible until `check_call_arity` landed in `src/verifier.rs`; the fixture
  is now a PASS (both compilers refuse). **A fixture that measures a gap in the
  other direction is still a measurement**, and the empty set is what will
  catch the next one.

## One defect can have several SHAPES, and a fixture only holds the one it tests

The sharpest thing this corpus has produced so far is a lesson about itself.

`purity_reads_missing` scored **PASS** in the first sweep, so the "missing"
direction of the purity check read as covered. It was not. That fixture declares
`reads: []`, and the empty declaration was the *only* shape gen0 caught. A
**partial** under-declaration went through clean:

```
declares reads: []     , logic reads i.amount      -> gen0 rc=1, 0 bytes  (refused)
declares reads: [t.a]  , logic reads t.a + t.b     -> gen0 rc=0, 720 bytes (ACCEPTED)
```

`verbosec` refuses both. gen0's reads list stored each entry's **root ident**
only, so `[t.a]` put `t` in the list and every `t.<field>` access matched it —
one declared entry blessed every field of the input. Declaring most reads and
omitting one is exactly what a concealed read looks like, and the declared
`reads:` set *is* the audit surface, so the check was close to cosmetic while
reading as green. (Fixed 2026-08-11; the mechanism is in the `NameList` banner in
`examples/vexprparse.verbose`.)

This is the second time in this arc that a check looked green because the probe
landed on the working half of the input space — the first was text `==`, where a
stuck-`false` equality is right on every non-matching pair. So:

- **A fixture that passes for the wrong reason is worse than a missing one.** A
  missing fixture leaves a visible hole; a fixture passing vacuously retires the
  question.
- **When a check has more than one failure shape, fixture EACH one.** Do not let
  the cheapest shape stand in for the family. `purity_reads_missing` and
  `purity_reads_missing_partial` are both kept, and both `calls` shapes too —
  even though calls was measured correct all along, because "calls was fine" is
  a measurement, not a property.

## And a fixture set can SAMPLE a matrix while reading as if it enumerated one

The lesson above is about a fixture that passes for the wrong reason. There is a
third failure mode, found 2026-08-12, and it is about fixtures nobody wrote.

`@intention` / `@source` presence had **three** fixtures — `rule` × 2 and
`concept` × 1 — out of a possible **fourteen**. Verbose has seven declaration
kinds (`rule`, `concept`, `concept_group`, `reaction`, `service`, `resource`,
`connection`), each requiring both attributes. So "gen0 does not check presence"
had been *measured* on two kinds and *assumed* for the other five, and "verbosec
requires both attributes here" had never been checked per-kind at all.

Nothing in the sweep's output hints at this. Three names sitting in `KNOWN_GAPS`
look exactly like a closed question — the missing cells are invisible precisely
because a fixture that does not exist produces no row.

- **When a check ranges over a finite set of contexts, enumerate the set and
  fixture every cell.** Do not generalise from the cell you happened to write
  first. This is the same discipline the text-position matrix in `CLAUDE.md`
  applies to output positions.
- **Derive the reference behaviour per cell from the source**, rather than
  assuming the kind you tested speaks for the rest. Here it came from the seven
  `.ok_or_else(|| self.error("... missing @intention" / "@source"))` pairs in
  `src/parser.rs`, then a probe per kind to confirm the read.

Enumerating turned up **no surprise** — all seven kinds require both attributes,
unconditionally, with no exemption and no conditional. That is still a result:
it was not known before it was measured, and it is what licensed making the
check exact rather than conservative.

The matrix now has all 14 cells plus a 15th, `attr_missing_source_nested_concept`,
for the shape where a `concept` inside a `concept_group` lacks an attribute while
the enclosing group carries both. That one is not a duplicate of the plain
`concept` cell: it is what makes `concept_group`'s nesting load-bearing, and a
walk that segmented on top-level declarations only would score it clean.

## Adding a fixture

1. Make it as small as possible and isolate exactly one defect.
2. Point `@source` at `negative.intent`.
3. Confirm `verbosec` refuses it, and for the reason you intended:
   `cargo run --release -- examples/negative/<name>.verbose`
4. **Ask what OTHER shapes the same defect has** (empty vs partial, matching vs
   non-matching, present vs absent) and add a fixture per shape. See the section
   above for what skipping this costs.
5. Run the sweep. It will fail, naming your fixture.
6. Triage it: if gen0 genuinely cannot perform the check today, add the name to
   `KNOWN_GAPS` **and** record the structural cause in the test's doc comment.
   If gen0 *should* catch it, you have found a bug — fix that instead.

## What this corpus does NOT cover

Stated plainly, because "the negative corpus is green" must not be read as
"gen0's verifier is complete":

- **Only 35 fixtures.** They were chosen from CLAUDE.md's known-gaps table plus
  what a first pass over `src/verifier.rs` and `src/parser.rs` suggested. They
  are not an enumeration of everything `verbosec` refuses — the verifier has
  many more refusal paths (resources, connections, services, reactions,
  `concept_group` bounds, `Result` arm typing, layer/effect composition) with
  no fixture here at all.
- **One defect per fixture, in isolation.** Nothing measures how gen0 behaves
  when two violations co-occur, or when a violation sits inside a construct
  gen0 parses differently (a service handler, a reaction, a `concept_group`).
  Note this is about co-OCCURRENCE; a single defect's several SHAPES do now get
  a fixture each, for the reason the section above gives.
- **No refusal-MESSAGE comparison, and there is nothing to compare against.**
  The sweep compares exit status only, so gen0 refusing for an unrelated reason
  scores as a PASS. This is not a shortcut that could be tightened cheaply:
  **gen0 emits no diagnostic at all** — a refusal is a bare `exit 1` with zero
  bytes written, and nothing on stderr either. Attributing a refusal therefore
  needs a different instrument, and the one that works is a **corrected twin**:
  compile the minimally-fixed program too and require ACCEPT, so the only thing
  that differs between the two verdicts is the violation under test.
  `two_generation_gen0_detects_partial_purity_underdeclaration` does that for
  every purity shape it pins, and
  `wrong_arity_rule_call_rejected_at_verify_time` (`src/verifier.rs`) does it
  for the arity check. It is per-fixture work, which is why the sweep itself
  does not do it for all 35.
- **Effects and services are absent.** Those refusal surfaces are exercised by
  the effect-position matrix in CLAUDE.md, over real TCP and real files, not
  here.
