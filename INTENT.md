# Writing `.intent` Files

`.intent` is the plain-language layer of Verbose. It describes what the program should do, in numbered sentences a non-programmer can read and audit. The AI translates `.intent` into `.verbose`, and the compiler verifies `.verbose` independently.

This document catalogs the prose patterns the generation tool maps reliably to specific Verbose constructs. Nothing here is enforced by the compiler — `.intent` is free-form prose. But using the patterns below reduces the chance of the AI making a judgment call, which reduces the chance of a rejected or surprising `.verbose` file.

## How to read this document

- **Prose pattern**: the shape of the sentence in an `.intent` file.
- **Verbose construct**: what the AI should produce in `.verbose`.
- The patterns are not exclusive — the AI will recognize equivalent phrasings. They are the *canonical* forms. If you want predictability, match them. If you want elegance, write freely and check the output.

## Quantifiers and collection operations

| Prose pattern | Verbose construct | Returns |
|---|---|---|
| *"For each X, check that Y"* | `all(xs, x => y(x))` | `bool` |
| *"At least one X is Y"* / *"There exists an X that is Y"* | `any(xs, x => y(x))` | `bool` |
| *"No X is Y"* | `not any(xs, x => y(x))` | `bool` |
| *"For each X, compute Y"* | `map(xs, x => y(x))` | `collection(T)` |
| *"Keep the X where Y"* / *"The subset of X that are Y"* | `filter(xs, x => y(x))` | `collection` of the same element type |
| *"The total of Y across X"* | `sum(xs, x => y(x))` | `number` |
| *"The number of X that are Y"* / *"Count X where Y"* | `count(xs, x => y(x))` | `number` |
| *"The smallest Y among X"* | `min(xs, x => y(x))` | `number` |
| *"The largest Y among X"* | `max(xs, x => y(x))` | `number` |
| *"Accumulate Y over X, starting from Z"* | `fold(xs, z, acc, x => body)` | any type |

### Examples

```
.intent:  3. A client is blocked when all their invoices are overdue.
.verbose: blocked = all(c.invoices, inv => invoice_overdue(inv))

.intent:  4. For each employee, check whether they are of retirement age.
.verbose: status = map(w.employees, e => e.age >= 65)

.intent:  5. The retirees are the employees who are of retirement age.
.verbose: subset = filter(w.employees, e => e.age >= 65)

.intent:  6. The total revenue is the sum of each order's amount.
.verbose: total = sum(orders, o => o.amount)
```

## Conditions and classification

| Prose pattern | Verbose construct |
|---|---|
| *"X is Y when Z"* | a `rule` whose output is `Z` |
| *"If A, then X, otherwise Y"* | `if A then X else Y` |
| *"X is A when P, otherwise B when Q, otherwise C"* | nested `if/else` |
| *"X is greater than / less than / at least / at most N"* | `>`, `<`, `>=`, `<=` |
| *"X equals Y"* / *"X is not Y"* | `==`, `!=` |
| *"Both A and B"* / *"Either A or B"* / *"Not A"* | `and`, `or`, `not` |

### Examples

```
.intent:  3. An invoice is overdue when it has more than 30 days overdue.
.verbose: overdue = inv.days_overdue > 30

.intent:  4. A client is a premium client if their balance exceeds 100000,
              otherwise a standard client.
.verbose: tier = if c.balance > 100000 then "premium" else "standard"
```

## Composition

| Prose pattern | Verbose construct |
|---|---|
| *"X is Y when [condition involving rule R]"* | a rule whose logic calls `R(...)` |
| *"Given intermediate values A and B, compute C"* | `let` bindings inside `logic:` |
| *"See rule N"* / *"as defined above"* | rule composition (translates to a `calls:` entry) |
| *"X imports definitions from module M"* | `use "stdlib/M.verbose"` at the top |

### Example

```
.intent:
  3. An invoice is overdue when it has more than 30 days overdue.
  4. A client is blocked when all their invoices are overdue.

.verbose:
  rule invoice_overdue
    logic:
      overdue = inv.days_overdue > 30
    proofs:
      purity:
        calls: []
  rule client_blocked
    logic:
      blocked = all(c.invoices, inv => invoice_overdue(inv))
    proofs:
      purity:
        calls: [invoice_overdue]   ← declared because we call invoice_overdue
```

## Side effects

| Prose pattern | Verbose construct |
|---|---|
| *"When X happens, do Y"* | `reaction` with a `trigger:` naming the rule that fires it |
| *"Log the critical invoices"* | reaction with a `print` effect and a trigger |
| *"Notify the admin when ..."* | reaction whose `trigger` is a bool rule and whose effect prints an interpolated message |

### Example

```
.intent:  5. When an invoice is flagged as critical, print an alert
              mentioning its amount.

.verbose: reaction critical_alert
            @intention: "print an alert when an invoice is flagged as critical"
            trigger: is_critical
            effects:
              print "critical invoice amount: {inv.amount}"
```

## Structure conventions

- **Number every line** of prose. The `@source` declaration in `.verbose` points back to a line number, so numbers are the anchor of traceability.
- **Introduce a concept before rules that use it.** Rules can only reference concepts declared earlier (or imported via `use`).
- **Keep one claim per line** when possible. Compound sentences become compound rules, which is harder to audit.
- **Cross-reference** with "(see §3)" or "according to rule 2 above". The AI reads these as signals to compose rules rather than duplicate logic.

## Ambiguity and defaults

When the prose does not fully specify a detail, the AI fills it in:

- **Field ranges.** A sentence mentioning "amount" produces `amount : number` with a default range. If you care about the range, state it explicitly: *"amount, between 0 and 1 000 000"*.
- **Termination.** The AI picks `constant_bound` with a guessed bound, or `variable_bound` for loops. State *"bounded by at most N steps"* if the bound matters for your audit.
- **Determinism.** Assumed `total` unless the prose mentions nondeterminism (randomness, real time, external IO).
- **Overflow.** The AI may add `overflow: [min, max]` when arithmetic justifies it. Read the generated `.verbose` if runtime overflow behavior matters to you.
- **Hints.** The AI adds `vectorizable`, `parallel`, or `cache_result` when the logic shape supports them; each comes with a justification string the verifier cross-checks.

If an ambiguity would change the program's meaning, name it in prose. If it would not, let the AI choose and audit the `.verbose` output.

## What NOT to do

- **Do not encode Verbose syntax in `.intent`.** Writing `all(xs, x => y(x))` in an `.intent` file defeats the purpose of the two-layer split. `.intent` is the human language.
- **Do not invent proofs in prose.** Claims like *"this reads `c.invoices`"* or *"pure with no calls"* belong in `.verbose`, not `.intent`. The AI derives proofs from the logic; stating them in prose pollutes the pipeline.
- **Do not assume unpublished conventions.** If a pattern you want is not in this document, either write the `.verbose` directly, or propose the pattern (see [CONTRIBUTING.md](CONTRIBUTING.md)). An undocumented pattern is a gamble on the AI's judgment.
- **Do not mix languages in one file.** If you write part of your intent in French and part in English, the AI will translate, but the result is harder to audit. Pick one.

## Why this document exists

`.intent` is prose, and prose is ambiguous. The evolution rule for Verbose says `.intent` can evolve freely, because the compiler never reads it — only the AI does. But that freedom has a cost: if nothing is documented, every `.intent` file depends on the AI's improvisation.

This document is the shared grammar of what we have agreed to recognize. It bounds the improvisation without turning `.intent` into code. Future patterns can be added here as we identify them. Nothing here is frozen; everything here is written down.
