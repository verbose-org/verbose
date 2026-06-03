# INDENT/DEDENT tokenization in pure Verbose — design note

**Status:** design note, NO implementation. Written 2026-06-03. Companion to
[group-field-abi-design.md](group-field-abi-design.md) (the single-group-field
recursive ABI this brick depends on and extends) and
[composition-abi-design.md](composition-abi-design.md) (the "an arena index is
just a number" insight). Decision aid for the author, not a commitment.

**Scope:** ONE self-hosting brick — turning Python-style significant
indentation into structural `INDENT` / `DEDENT` tokens, written in `.verbose`,
so the self-hosting tokenizer goes from *flat* (single-line expression) token
streams to *block-structured* whole-program token streams. This is the gate to
parsing real multi-line `.verbose` files.

---

## 0. The need, reproduced

`.verbose` source is indentation-significant: `concept` / `rule` bodies, and
the `logic:` / `proofs:` / `purity:` sub-blocks, are delimited by leading-space
width, exactly like Python. `src/parser.rs` consumes `Indent` / `Dedent` /
`Newline` tokens (`src/lexer.rs:34-36`) to recover block structure.

The self-hosting tokenizer so far (`examples/vexprparse.verbose`, bricks 1–7)
emits only `Ident` / `Keyword` / `Num` / `Op` / `Str` / `Eof` — it has **no
notion of lines or columns**. The self-hosting parser bricks built on top of it
(`parse_or` and the precedence ladder) consume only *flat* streams. The brick's
own header says it (`examples/vexprparse.verbose:24-25`):

> there is no INDENT/DEDENT in this tokenizer, so the multi-line block is a
> later brick.

That later brick is this one.

### A correction to the premise (verified against code)

The task framing says "skip_spaces eats newlines as whitespace." **That is not
what the current code does.** `skip_spaces` (`examples/vexprparse.verbose:230`)
advances only on byte `32` (space):

```
out = if s.pos >= len then s.pos
      else if byte_at(s.source, s.pos) == 32 then skip_spaces(... pos+1)
      else s.pos
```

A newline (byte `10`) is NOT consumed by `skip_spaces`. It would instead reach
`op_code` (`examples/vexprparse.verbose:493`), which returns `0` for byte `10`,
so `next_token` (`:540`) classifies it as `Eof` and `tokenize` (`:587`) stops at
the first newline. **The current flat tokenizer therefore terminates at the
first line break** — it cannot even *traverse* a multi-line file, let alone
emit indentation tokens. This matters for the design: line handling is not just
"add INDENT/DEDENT," it is "teach the scanner that a newline is a token boundary
to step over, not a stop signal."

---

## 1. The reference algorithm (verified, cited)

`src/lexer.rs::tokenize` + `handle_line_start`. Verified line by line.

### State (lexer.rs:98-107, 116-119)

- `indent_stack: Vec<usize>` initialised `vec![0]` (`:103`, `:116`). The
  sentinel `0` is the file's outermost (column-0) level and is **never popped**.
- `at_line_start: bool` initialised `true` (`:118`).
- `paren_depth: i32` initialised `0` (`:119`).

### Per-line-start measurement (lexer.rs:125-130, 185-240)

The main loop calls `handle_line_start` only when
`at_line_start && paren_depth == 0` (`:125`). `handle_line_start`:

1. Measures leading-space width by counting `b' '` (`:186-202`); a **tab (`b'\t'`)
   is rejected** with "tabs not allowed for indentation; use spaces"
   (`:193-199`).
2. **Blank-line / comment exceptions** — if, after the spaces, the position is at
   end-of-source (`:204-207`), at a newline (`:208-212`), or at a `--` comment
   (`:213-216`), it sets `at_line_start = false` and returns WITHOUT touching the
   stack. So blank lines and comment-only lines emit no INDENT/DEDENT.
3. Compares `width` to `current = *indent_stack.last()` (`:218`):
   - **`width > current`** (`:219-221`): push `width`, emit ONE `Indent`. (A single
     INDENT regardless of how many columns deeper.)
   - **`width < current`** (`:222-237`): `while *last() > width { pop; emit Dedent }`
     (`:223-226`) — this is the **multi-DEDENT** case, one DEDENT per popped level.
     Then if `*last() != width`, error "inconsistent indentation: width N does not
     match any enclosing block" (`:227-236`).
   - **`width == current`**: nothing.

### Newline handling inside the main loop (lexer.rs:141-158)

On byte `\n` with `paren_depth == 0`: emit a `Newline` token (unless the last
token was already a `Newline`/`Indent`/`Dedent`, `:148-154`), then set
`at_line_start = true` (`:155`). On `\n` with `paren_depth > 0`, the newline is
swallowed silently — **no Newline, no line-start trigger** (the `if paren_depth
== 0` guard at `:147`).

### Paren-depth tracking (lexer.rs:377-417)

`(`, `{`, `[` increment `paren_depth`; `)`, `}`, `]` decrement it. While
`paren_depth > 0`, the `at_line_start` branch (`:125`) is skipped and newlines
don't trigger line-start — i.e. **a bracketed construct spanning lines is one
logical line for indentation purposes.** (Concretely: a multi-line `reads : [...]`
list does not get spurious INDENTs.)

### EOF flush (lexer.rs:168-181)

At end of source: emit a trailing `Newline` if the last token wasn't one
(`:168-174`); then **`while indent_stack.len() > 1 { pop; emit Dedent }`**
(`:176-179`) — one DEDENT per still-open block; finally emit `Eof` (`:181`).
The sentinel `0` keeps `len() == 1` at the end, so the loop is well-founded.

### Summary of the five behaviours to reproduce

1. INDENT on deeper width (exactly one, push).
2. nothing on equal width.
3. multi-DEDENT on shallower width (pop until `<= width`), then inconsistency
   error if no exact match.
4. tab rejection; blank-line / comment-line / in-paren exceptions.
5. EOF flush: a DEDENT per remaining stack level above the sentinel.

---

## 2. The core question — how to express the column stack in Verbose

Verbose has **no mutable state, no `Vec`, no stack data structure**. What it has:
recursion, the arena (`concept_group` cons-lists, indexed, growable to
`max_nodes`), single-value returns, group-concept *fields* in recursive rules
(the 9529277 ABI), eager `let`, lazy `if`, fail-closed `byte_at`. A `Vec<usize>`
stack becomes a **cons-list in the arena, threaded through recursion** — the same
move bricks 2–7 used for the token list and the AST.

Three candidate designs.

### (a) Column stack as an arena cons-list, threaded with the output token list

A new group concept:

```
concept ColStack
  variants:
    Push of (width : number, rest : ColStack)
    Empty
```

A line-driver rule carries the recursion state as one record with **three
fields**: the current byte position (`number`), the column stack (`ColStack`,
group), and the output token list built so far (`TokenList`, group). It recurses
**line by line**:

- find the next line start, measure its leading-space `width`;
- compare `width` to `top_width(stack)`:
  - deeper → cons `Indent` onto output, `Push { width, rest: stack }`;
  - shallower → a *helper* recursion pops the stack and conses one `Dedent` per
    pop until `top <= width` (a sub-recursion over `ColStack`, returning both the
    popped stack and the emitted dedents — packed into a small result concept);
  - equal → no structural token;
- then tokenize the line's real lexemes (reusing brick 2's `next_token` /
  `token_end` machinery over the line's byte range), conS them onto output, cons
  a `Newline`;
- recurse on the next line with the new position, new stack, new output.

At EOF, a final flush rule pops the remaining stack (a recursion over `ColStack`
mirroring the shallower-width pop) emitting a `Dedent` per level, then conses
`Eof`.

This **mirrors the reference directly**: `indent_stack` ⇒ `ColStack`, the
`while top > width { pop; emit }` loop ⇒ a tail recursion, the EOF flush ⇒ the
same recursion with `width = 0` (so it pops down to the sentinel).

**Cost & shape.** The driver's input concept has TWO group fields (`ColStack`
and `TokenList`). The crux question was whether that compiles natively. **It
does — verified by probe (§2-crux below).** The output token list and the column
stack are both arena cons-lists; the per-line recursion threads both plus a
position number. No new ABI.

### (b) Two-pass: tokenize flat (existing), then a post-pass injects INDENT/DEDENT

Tokenize the whole file into a flat `TokenList` (after teaching the scanner to
*step over* newlines — see §3), then a second rule walks the flat list while
tracking line/column to splice in `Indent` / `Dedent` / `Newline`.

**Problem (verified):** the existing `Token` variants carry only `start`/`len`
**byte offsets** into the source, never a line or column (`examples/vexprparse.verbose:108-123`).
A post-pass can recover a token's column from its `start` byte offset *only by
re-scanning the source* — for each token, walk backwards from `start` to the
preceding newline and count spaces, or maintain a parallel "newline positions"
structure. Both reintroduce a per-token source scan, and the post-pass still
needs the column stack of design (a) to decide INDENT vs DEDENT. So (b) is (a)
plus an extra materialised flat list plus a column-recovery scan — **strictly
more work and more arena, no simpler.** The only thing it buys is keeping brick
2 byte-for-byte unchanged, which (a) does NOT require either (the brick-2 helpers
are reused as-is by (a), just driven over line ranges). Rejected.

### (c) Tokens carry their source column from the start

Add a `col : number` field to each `Token` variant, computed during tokenize,
then a separate rule derives INDENT/DEDENT from column transitions at line
starts.

**Problems.** (1) It bloats every token variant (and every arena entry) with a
column field that only the line-start tokens actually need — the four `Op` /
`Ident` / `Num` / `Str` mid-line tokens never consult their column. That is
false explicitation: a declaration carried but not exploited at most call sites.
(2) Computing the column still requires knowing where line starts are — i.e. the
same newline-aware scan as (a)/(b). (3) The derivation rule STILL needs the
column stack to turn "this line's column" into the right number of
INDENT/DEDENT. So (c) = (a)'s stack work + a wider token. Rejected.

### The crux feasibility result (probe evidence)

Design (a) hinges on: **can one recursive rule's input concept hold MORE THAN
ONE group-concept field?** The single-group-field ABI shipped in 9529277
(documented in group-field-abi-design.md) covers `Nth { lst: TokenList, n }` —
one group field. Design (a) needs `{ stack: ColStack, toks: TokenList, n }` —
*two*.

**Code reading.** The per-field shape check
(`src/native.rs:432-452`) loops over **every** field of the input concept and
accepts `Type::Named(n) if group_concept_names.contains(n)` for **each field
independently** (`:442`) — there is no "at most one group field" restriction.
The struct-layout builder (`src/native.rs:3575-3582`) gives every non-text field
(group fields included — a group field is `Type::Named`, not `Type::Text`) one
8-byte slot, again with no per-concept cap. The multi-field Record call-site
marshalling (`src/native.rs:11341-11398`) evaluates each constructor field
expression and stores rax to its struct slot; a group field's expression
(`s.stack` pass-through, or a `ColStack::Push { ... }` constructor) lowers to an
i64 arena index in rax, exactly like a number.

**Probe (built, run, deleted).** A throwaway `.verbose` with
`concept Work { stack: ColStack, toks: TokList, n: number }` and a recursive
`run(Work)` that pushes onto `stack` and conses onto `toks` each step:

```
verified: 4 concept(s), 4 rule(s); all proofs check out
native: ... -> /tmp/probe_go (1169 bytes, rule 'go', input: argv)
```

Native and interpreter agree exactly:

| n | native | interpreter |
|---|--------|-------------|
| 0 | 0 | 0 |
| 1 | 1 | 1 |
| 3 | 3 | 3 |
| 5 | 5 | 5 |

(The entry rule `go` takes a plain `Seed { n: number }` from argv and builds the
empty `ColStack::Empty` / `TokList::Nil` internally — the two-group-field record
is reached only through internal recursion, never as `_start`'s input. This is
exactly vexprparse's pattern: the entry takes text/number, the group-field rules
are internal — see `examples/vexprparse.verbose:2215-2216`.)

**Verdict on the crux: multiple group-concept fields in one recursive rule's
input compile natively AND run correctly, with NO compiler change.** Design (a)
is feasible as-is.

---

## 3. Line handling — the helpers needed

The scanner must gain a notion of *lines*. Concretely:

- **`line_width(source, pos) -> number`** — from a line start `pos`, count
  consecutive spaces (byte 32). Reject a tab (byte 9): the reference errors on
  tabs (`src/lexer.rs:193-199`); in Verbose the fail-closed posture is to make
  the whole tokenize abort — but Verbose rules cannot abort mid-evaluation, so
  the honest mapping is a **sentinel width** (e.g. return a flag the driver turns
  into an `Eof`-terminated truncation) OR a dedicated `Err` token kind. Decision:
  reproduce the reference's *rejection* by emitting a distinguished error token
  (a new `Token::IndentErr of (pos)`), so the downstream parser sees a structural
  error rather than silently mis-indenting. (`byte_at` is fail-closed OOB, so
  `line_width` at end-of-source returns the count so far — safe.)
- **`line_content_start(source, pos) -> number`** — `pos + line_width(...)`: the
  first non-space byte of the line. Used to apply the blank-line / comment
  exceptions (peek `byte_at(source, content_start)` for `10` newline, end-of-
  source, or `45 45` for `--`).
- **`next_line_start(source, pos) -> number`** — from anywhere on a line, scan
  forward to the byte after the next newline (byte 10), or to `length(source)`
  at EOF. This is how the driver steps from one line to the next; it replaces the
  "newline is a stop" behaviour the current scanner has.
- **`top_width(ColStack) -> number`** — head width, `0` for `Empty` (the
  sentinel). Trivial `match`.
- **`pop_to(ColStack, width) -> ColStack`** + **`dedents_to(ColStack, width) ->
  number`** (or a combined result concept) — the multi-DEDENT recursion: pop
  while `top > width`; the count of pops is the number of `Dedent` tokens; a
  final `top != width` (and `top` not the sentinel) is the inconsistency error.

The existing brick-2 helpers (`next_token`, `token_end`, `skip_spaces`,
`ident_run`, etc.) are **reused unchanged**, driven over each line's byte range:
the driver tokenizes the line's content with the same machinery, just bounded so
it does not run past the line's newline (it stops at `next_line_start` for that
line). No brick-2 edits.

---

## 4. Feasibility verdict

**Pure Verbose, no compiler change.** Every primitive design (a) needs already
exists and is verified:

- multiple group-concept fields in a recursive input — **confirmed by probe**
  (§2-crux);
- `byte_at` byte reads, fail-closed OOB — used throughout brick 2;
- arena cons-lists for the column stack and the output token list — same shape as
  `TokenList` / `ArgList` (`examples/vexprparse.verbose:125-204`);
- the eager-`let` / lazy-`if` discipline for guarded recursion — the parser
  bricks already live by it (`examples/vexprparse.verbose:1198-1210`).

No language extension and no native extension is required. (Contrast: the
single-group-field ABI itself DID need a native change — 9529277 — but that
landed already, and the multi-field generalisation rides on it for free because
the per-field loop never assumed a single group field.)

One honest caveat that is **not** a compiler change but a language *shape*
constraint: a Verbose rule cannot "abort mid-evaluation" the way the reference
lexer returns `Err`. The tab-rejection and inconsistent-indentation errors must
therefore be modelled as **error tokens in the stream** (a new `Token::IndentErr`
variant), which the downstream self-hosting parser treats as a hard stop. This is
a faithful translation of the reference's fail-closed intent into Verbose's
total-function world, not a weakening of it — and it keeps the tokenizer a pure
rule (no effect, no syscall).

---

## 5. Recommendation

**Adopt design (a): the column stack as an arena cons-list, threaded with the
output token list through a line-by-line driver recursion.** It mirrors the
reference one-to-one, needs no compiler change, and reuses brick 2 verbatim.

### New concepts

- `ColStack { Push(width, rest) | Empty }` — added to the program's single
  `concept_group` (the one-group-per-program native cap, `src/native.rs` slice
  4a.1, means it shares VExpr's group alongside `Token` / `TokenList` / `Ast`).
- `Token::Indent` / `Token::Dedent` / `Token::Newline` / `Token::IndentErr` —
  four new variants on the existing `Token` group concept. (`Newline` is needed
  too: the parser consumes it as a statement separator, `src/lexer.rs:36`.)
- A line-driver state concept with two group fields + a position:
  `LineState { stack: ColStack, out: TokenList, pos: number }`.
- A small multi-DEDENT result concept if `pop_to` / `dedents_to` are combined:
  `DedentResult { popped: ColStack, count: number }` (so one recursion returns
  both the new stack and the emitted-dedent count).

### Helper rules

`line_width`, `line_content_start`, `next_line_start`, `top_width`, `pop_to` /
`dedents_to` (or `dedent_step` over `DedentResult`), a `cons_dedents` rule that
prepends `count` `Dedent` tokens to the output, the line-driver `tokenize_lines`,
and an EOF-flush rule `flush_dedents` (the same pop recursion with `width = 0`).

### Validation strategy (the inputs that prove it)

Mirror `src/lexer.rs` behaviour on hand-checkable inputs; cross-check native vs
`--run` (the discipline that caught the swapped-opcode bug in WASM and the
arena-sizing bug in `enriched_page`):

1. **Single nested block** — `rule f\n  logic:\n    x` → `... Indent ... Indent
   ... Dedent Dedent Eof` (one INDENT per deeper level, two DEDENTs flushed at
   EOF).
2. **Multi-level dedent in one step** — a line returning from depth 2 directly to
   depth 0 → two `Dedent` tokens between it and the shallower line.
3. **Equal-width lines** — sibling statements at the same indent → `Newline`
   between them, no INDENT/DEDENT.
4. **EOF flush** — file ending inside a block → the right count of trailing
   `Dedent` before `Eof`.
5. **Inconsistent indentation** — a line whose width matches no enclosing level →
   `Token::IndentErr` in the stream.
6. **Tab** — a tab in leading whitespace → `Token::IndentErr`.
7. **Blank line / comment-only line** — neither emits INDENT/DEDENT/Newline-pair
   spuriously (the reference's exception, `src/lexer.rs:204-216`).
8. **In-paren newline** — a bracketed list spanning lines (`reads : [a,\n b]`)
   emits no INDENT for the continuation. (Requires the driver to track
   `paren_depth` as a fourth state number — see Risks.)

The observable is the structural signature of the produced `TokenList` (a
kind-count fingerprint, exactly the `shape_ast` technique brick 7 uses,
`examples/vexprparse.verbose:2151-2184`), so each test certifies the exact token
shape by a single number predictable by hand.

### One brick or split?

**Split into two bricks.**

- **Brick 8a — line traversal + Newline.** Teach the scanner to step over
  newlines (`next_line_start`, the line helpers) and emit `Newline` at line
  boundaries, tokenizing each line's lexemes with the brick-2 machinery. NO
  column stack yet — this alone lifts the "stops at first newline" limit and
  produces a multi-line flat-plus-Newline stream. Smallest increment that is
  independently observable.
- **Brick 8b — INDENT/DEDENT + the column stack.** Add `ColStack`, the
  two-group-field driver, the multi-DEDENT recursion, the EOF flush, the
  exceptions (blank/comment/in-paren), and the error tokens. This is the part
  that exercises the multi-group-field ABI and the threaded-stack recursion.

Splitting keeps each brick's arena sizing, termination posture, and validation
table small, and isolates the genuinely new hard part (the threaded column
stack) from the mechanical part (line stepping).

### Pillars / axiom / no-regression check

- **Verifiability** — the driver is a pure rule with `purity` + `termination`
  proofs; the column stack and EOF flush recurse over a strictly-shrinking value
  or an increasing position (the same bound-only posture brick 2's `tokenize`
  declares and the same mandatory recursion breadcrumb, since a precedence/line
  scanner has no `structural`/`decreasing` field that fits — the runtime
  backstops are the finite source length and the bounded `max_nodes`).
- **Exploitability** — every new variant (`Indent` / `Dedent` / `Newline` /
  `IndentErr`) is consumed by the self-hosting parser; no decoration. `ColStack`
  is load-bearing (the indentation decision); the `col`-per-token of design (c)
  was rejected precisely because it would have been carried-but-unused at most
  sites.
- **Compiler axiom (verify + emit, never guess)** — no new primitive forces the
  compiler to infer anything; the change is entirely in `.verbose` (new concepts
  + rules), which the existing verifier checks and the existing native emitter
  lowers. The compiler is untouched.
- **No CPU overhead / no regression** — this is a `.verbose` program, not a
  compiler change; existing examples are byte-for-byte unaffected (the probe
  confirmed the multi-group-field path is already in the emitter, exercised by no
  shipped example yet, so nothing else moves).

---

## 6. Honest risks

1. **The threaded column stack is the new hard part.** Brick 2 threaded one group
   field (the output list grows); the parser threaded one group field (`Parsed`).
   This driver threads **two** group fields plus a position, and the
   multi-DEDENT sub-recursion must return BOTH a new stack and a dedent count —
   packed into one result concept because a rule returns one value. The probe
   proves the ABI compiles, but the *logic* (getting the pop-count and the
   stack-after-pop consistent, and consing the right number of Dedents in the
   right order) is fiddly and is where a bug will hide. The shape-signature
   validation (test 2, multi-level dedent) is the guard.
2. **Eager-`let` / lazy-`if` discipline.** The line-driver must NOT compute the
   next line's tokenization in a `let` — that would fire eagerly and recurse past
   EOF (the exact trap documented at `examples/vexprparse.verbose:1198-1210`).
   The next-line recursion belongs inside the `then`/`else` branch of the
   "are we at EOF?" guard, like the parser's join rules.
3. **Arena sizing for a whole file.** Flat-expression bricks sized the arena for
   one line (`max_nodes: 4000`). A whole `.verbose` file's token stream is far
   larger: ~`2N+3` arena nodes for N tokens (Token value + Cons cell each) PLUS
   the column stack (one `Push` per open block, bounded by nesting depth — small)
   PLUS the INDENT/DEDENT/Newline tokens. For a realistic `.verbose` file this is
   thousands of nodes; `max_nodes` must be raised accordingly and the bounds-
   check (a VariantConstruct over the cap exits 1, fail-closed —
   `examples/vexprparse.verbose:101-102`) is the only backstop. Sizing must be
   chosen against the largest target file, with headroom.
4. **The in-paren exception adds a fourth state number.** Tracking `paren_depth`
   so multi-line bracketed lists don't get spurious INDENTs means the driver
   carries `{ stack, out, pos, paren_depth }`. That is still all numbers + two
   group fields (the probe shape), so no new ABI — but it widens the state record
   and the per-line logic. If the first target files have no multi-line bracketed
   constructs, brick 8b could ship without it and add it when a file needs it
   (the `reads : [...]` lists in proofs are the realistic trigger — they DO span
   lines in some examples, so this is likely needed early, not deferrable).
5. **Error-token modelling is a faithful but not identical translation.** The
   reference returns a `LexError` (aborts the lex). Verbose models it as a
   `Token::IndentErr` in the stream. The downstream parser must be written to
   treat `IndentErr` as a hard stop; until that parser brick exists, the error
   case is only observable as "the signature contains the IndentErr band." This
   is a known, accepted asymmetry (Verbose rules are total functions), not a
   silent divergence.
