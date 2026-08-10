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
- One fixture, `bad_arity.verbose`, is an **inverse** case: `verbosec` accepts
  it and gen0 refuses it. It is kept (and asserted) rather than deleted,
  because a fixture that measures a gap in the other direction is still a
  measurement.

## Adding a fixture

1. Make it as small as possible and isolate exactly one defect.
2. Point `@source` at `negative.intent`.
3. Confirm `verbosec` refuses it, and for the reason you intended:
   `cargo run --release -- examples/negative/<name>.verbose`
4. Run the sweep. It will fail, naming your fixture.
5. Triage it: if gen0 genuinely cannot perform the check today, add the name to
   `KNOWN_GAPS` **and** record the structural cause in the test's doc comment.
   If gen0 *should* catch it, you have found a bug — fix that instead.

## What this corpus does NOT cover

Stated plainly, because "the negative corpus is green" must not be read as
"gen0's verifier is complete":

- **Only 21 fixtures.** They were chosen from CLAUDE.md's known-gaps table plus
  what a first pass over `src/verifier.rs` and `src/parser.rs` suggested. They
  are not an enumeration of everything `verbosec` refuses — the verifier has
  many more refusal paths (resources, connections, services, reactions,
  `concept_group` bounds, `Result` arm typing, layer/effect composition) with
  no fixture here at all.
- **One defect per fixture, in isolation.** Nothing measures how gen0 behaves
  when two violations co-occur, or when a violation sits inside a construct
  gen0 parses differently (a service handler, a reaction, a `concept_group`).
- **No refusal-MESSAGE comparison.** The sweep compares exit status only. gen0
  refusing for an unrelated reason would score as a PASS.
- **Effects and services are absent.** Those refusal surfaces are exercised by
  the effect-position matrix in CLAUDE.md, over real TCP and real files, not
  here.
