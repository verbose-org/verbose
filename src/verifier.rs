use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path as StdPath;

use crate::ast::*;
use crate::parser::PRIMITIVE_CALL_NAMES;

#[derive(Debug)]
pub struct VerifyError {
    pub context: String,
    pub message: String,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.context, self.message)
    }
}

pub fn verify_program(program: &Program, base_dir: &StdPath) -> Vec<VerifyError> {
    let mut errors = Vec::new();

    // Phase 7 slice 3a: if any service declares Protocol::Http10, the compiler
    // owns the names `HttpRequest` and `HttpResponse`. Two consequences:
    // (1) any user-declared concept with one of those names is rejected as a
    // reserved-name conflict; (2) synthesised built-in concepts (below) are
    // injected into the concepts map so handler rules can reference them.
    let any_http10 = program.items.iter().any(|it| {
        matches!(it, Item::Service(s) if s.protocol == Protocol::Http10)
    });

    if any_http10 {
        for it in &program.items {
            if let Item::Concept(c) = it {
                if c.name == "HttpRequest" || c.name == "HttpResponse" {
                    errors.push(VerifyError {
                        context: format!("concept '{}'", c.name),
                        message: format!(
                            "'{}' is a reserved built-in concept for Protocol::Http10; remove the user declaration",
                            c.name
                        ),
                    });
                }
            }
        }
    }

    let synth_concepts: Vec<Concept> = if any_http10 {
        // `body`'s declared bound tracks `max_request` so the declaration is
        // true rather than decorative (see builtin_http_request). The concept
        // is program-wide while max_request is per-service, so take the
        // maximum over every Http10 service — the tightest bound true for all
        // of them. `any_http10` guarantees at least one, so the fold below
        // cannot yield the 0 default.
        let body_max = program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Service(s) if s.protocol == Protocol::Http10 => Some(s.max_request as i64),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        vec![builtin_http_request(body_max), builtin_http_response()]
    } else {
        Vec::new()
    };

    let mut concepts: HashMap<String, &Concept> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Concept(c) => Some((c.name.clone(), c)),
            _ => None,
        })
        .collect();
    for c in &synth_concepts {
        // Built-ins override any user concept with the same name. A user
        // conflict on these names was already flagged above; downstream
        // verification should see the compiler's shape, not the user's.
        concepts.insert(c.name.clone(), c);
    }

    // Phase B slice 1: concepts declared inside a `concept_group` share
    // the program-wide concept namespace. Register them here so name
    // collisions with top-level concepts are caught and so downstream
    // references can resolve them. We also record each group concept's
    // owning group so the rule check below can refuse a rule that uses
    // a group concept as its input/output (lifts in slice B.3).
    let mut group_concept_owner: HashMap<String, String> = HashMap::new();
    for item in &program.items {
        if let Item::ConceptGroup(g) = item {
            for c in &g.concepts {
                if concepts.contains_key(&c.name) {
                    errors.push(VerifyError {
                        context: format!(
                            "concept_group '{}' / concept '{}'",
                            g.name, c.name
                        ),
                        message: format!(
                            "concept name '{}' collides with another concept (top-level or in a different group)",
                            c.name
                        ),
                    });
                }
                group_concept_owner.insert(c.name.clone(), g.name.clone());
                concepts.insert(c.name.clone(), c);
            }
        }
    }

    let all_rules: Vec<&Rule> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();

    // A rule may not be named after a built-in expression primitive.
    //
    // `parse_primary` special-cases every name in `PRIMITIVE_CALL_NAMES` when
    // it appears in call position, BEFORE the generic `Expr::Call` fallback. So
    // a rule named `band` can never be reached: `band(6, 9)` parses as bitwise
    // AND and evaluates to 0, not as a call to the user's rule. Nothing in the
    // pipeline noticed — the program compiled, ran, and returned a different
    // answer than the source reads. The arity-1 form is barely better: it
    // reports "band requires exactly two arguments", an error about a primitive
    // the author never mentioned.
    //
    // Both shapes are the same defect — a name that silently changes meaning —
    // and this is the axiom the compiler exists to uphold: it verifies, it does
    // not guess. So the collision is refused at declaration, where the name is,
    // rather than mis-resolved at every call site. Same discipline as the
    // reserved `HttpRequest` / `HttpResponse` concept names above.
    for item in &program.items {
        if let Item::Rule(r) = item {
            if PRIMITIVE_CALL_NAMES.contains(&r.name.as_str()) {
                errors.push(VerifyError {
                    context: format!("rule '{}'", r.name),
                    message: format!(
                        "rule name '{}' collides with the built-in primitive '{}(...)'; \
                         the parser resolves '{}(...)' to the primitive, so this rule \
                         could never be called — rename it",
                        r.name, r.name, r.name
                    ),
                });
            }
        }
    }

    // A named top-level item may not be declared twice within its kind.
    //
    // Without this check the two backends SILENTLY DISAGREE about which
    // definition wins. Rule resolution is the sharpest case: the interpreter
    // resolves a call with `all_rules.iter().find(|r| r.name == name)` — the
    // FIRST match — while the native emitter builds `HashMap<name, &Rule>`
    // (native.rs) whose `insert` OVERWRITES on collision, so the LAST match
    // wins. So `rule f` declared twice makes `--run` and `--native` compute
    // DIFFERENT answers from the same source, both exiting 0, with the
    // verifier reporting `all proofs check out`. Measured on a 3-rule probe
    // (`f` returns v+1, `caller` calls `f`, second `f` returns v+100):
    // `--run caller` on v=5 gives 6, `--native` gives 105.
    //
    // The verifier itself is not immune: its own `all_rules` is a Vec walked
    // with `.find` (first-match, like the interpreter) while its `concepts`
    // HashMap keeps the LAST — so a "verified" program can carry two
    // meanings. That is the project's central thesis (the verifier is the
    // durable artifact) failing outright, and it is the same family as the
    // arity check (PR #163), the record-`.field` type check (PR #178) and
    // the text-in-arithmetic operand check (PR #182): the verifier certifies
    // a program its executors mishandle. Refuse at the declaration, where
    // the name is — same discipline as the reserved-primitive-name and
    // reserved-`HttpRequest`/`HttpResponse` checks above.
    //
    // Resources and connections already have their own duplicate check
    // below (they share one namespace, and a connection may not collide with
    // a resource). This pass covers the remaining named top-level kinds:
    // rules, top-level concepts, reactions, services, and concept groups.
    // Concept names declared INSIDE a `concept_group` are checked against the
    // shared concept namespace separately (above), so two group concepts of
    // the same name, or a group concept colliding with a top-level concept,
    // are already refused — this pass adds the top-level-vs-top-level concept
    // case and the group-NAME case those checks do not reach.
    {
        let mut seen_rules: HashSet<&str> = HashSet::new();
        let mut seen_concepts: HashSet<&str> = HashSet::new();
        let mut seen_reactions: HashSet<&str> = HashSet::new();
        let mut seen_services: HashSet<&str> = HashSet::new();
        let mut seen_groups: HashSet<&str> = HashSet::new();
        for item in &program.items {
            let (set, kind, name): (&mut HashSet<&str>, &str, &str) = match item {
                Item::Rule(r) => (&mut seen_rules, "rule", r.name.as_str()),
                Item::Concept(c) => (&mut seen_concepts, "concept", c.name.as_str()),
                Item::Reaction(rx) => (&mut seen_reactions, "reaction", rx.name.as_str()),
                Item::Service(s) => (&mut seen_services, "service", s.name.as_str()),
                Item::ConceptGroup(g) => (&mut seen_groups, "concept_group", g.name.as_str()),
                _ => continue,
            };
            if !set.insert(name) {
                errors.push(VerifyError {
                    context: format!("{} '{}'", kind, name),
                    message: format!(
                        "duplicate {kind} name '{name}' (already declared earlier); {kind} names \
                         must be unique — the interpreter binds the first definition and the native \
                         emitter binds the last, so a duplicate makes the two backends disagree",
                        kind = kind, name = name
                    ),
                });
            }
        }
    }

    // Phase 9 slice 1: collect declared resource names for cross-checking
    // every `read(name)` reference. Duplicate resource names also rejected
    // here (resource namespace is global at the program level).
    let mut all_resources: HashSet<String> = HashSet::new();
    for item in &program.items {
        if let Item::Resource(r) = item {
            if !all_resources.insert(r.name.clone()) {
                errors.push(VerifyError {
                    context: format!("resource '{}'", r.name),
                    message: format!("duplicate resource name '{}'", r.name),
                });
            }
        }
    }

    // Phase 11 slice 1: collect declared connection names for cross-checking
    // every `fetch(name, ...)` reference. Same global namespace discipline as
    // resources; a connection name must not collide with a resource name
    // (both flow through `reads:` purity facts as a single identifier path).
    let mut all_connections: HashSet<String> = HashSet::new();
    for item in &program.items {
        if let Item::Connection(c) = item {
            if !all_connections.insert(c.name.clone()) {
                errors.push(VerifyError {
                    context: format!("connection '{}'", c.name),
                    message: format!("duplicate connection name '{}'", c.name),
                });
            }
            if all_resources.contains(&c.name) {
                errors.push(VerifyError {
                    context: format!("connection '{}'", c.name),
                    message: format!(
                        "connection name '{}' collides with a resource of the same name; reads: lists merge both namespaces",
                        c.name
                    ),
                });
            }
        }
    }

    // Slice entropy-1: collect declared entropy names for cross-checking
    // every `random(name)` reference. Third member of the resource /
    // connection family: same global namespace, and an entropy name must
    // collide with neither, because all three flow through `reads:` purity
    // facts as a single identifier path (refusal row 4).
    let mut all_entropies: HashSet<String> = HashSet::new();
    for item in &program.items {
        if let Item::Entropy(e) = item {
            if !all_entropies.insert(e.name.clone()) {
                errors.push(VerifyError {
                    context: format!("entropy '{}'", e.name),
                    message: format!("duplicate entropy name '{}'", e.name),
                });
            }
            if all_resources.contains(&e.name) {
                errors.push(VerifyError {
                    context: format!("entropy '{}'", e.name),
                    message: format!(
                        "entropy name '{}' collides with a resource of the same name; reads: lists merge both namespaces",
                        e.name
                    ),
                });
            }
            if all_connections.contains(&e.name) {
                errors.push(VerifyError {
                    context: format!("entropy '{}'", e.name),
                    message: format!(
                        "entropy name '{}' collides with a connection of the same name; reads: lists merge both namespaces",
                        e.name
                    ),
                });
            }
        }
    }

    // Slice `text-state-1`: the declared byte ceiling of every resource and
    // connection, keyed by name. The compile-time overflow gate for a text
    // `set` needs the NUMBER, not just the name — `all_resources` /
    // `all_connections` above carry names only. Built from the item list (a
    // Vec) so no HashMap iteration order can reach a decision.
    let mut resource_max_bytes: HashMap<&str, i64> = HashMap::new();
    let mut connection_max_response: HashMap<&str, i64> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Resource(r) => {
                resource_max_bytes.insert(r.name.as_str(), r.max_bytes as i64);
            }
            Item::Connection(c) => {
                connection_max_response.insert(c.name.as_str(), c.max_response as i64);
            }
            _ => {}
        }
    }

    for item in &program.items {
        match item {
            Item::Concept(c) => verify_concept(c, base_dir, &mut errors),
            Item::ConceptGroup(g) => {
                verify_concept_group(g, &group_concept_owner, base_dir, &mut errors);
            }
            Item::Rule(r) => {
                // Phase B slice 1: rules cannot yet reference a concept
                // declared inside a `concept_group` from their input or
                // output. Phase B slice 3 (2026-05-21) lifted the
                // interpreter refusal: rules can now build and traverse
                // recursive Variant values via `--run`. The native
                // refusal moves to `compile_native_code` (Phase B
                // slice 4+ ships arena allocation + tag dispatch).
                // The verifier no longer rejects rules that use group
                // types — type-checking against `Type::Named(C)` where
                // C is in a group works through the program-wide
                // namespace already shared by B.1.
                verify_rule(r, &concepts, &all_rules, &all_resources, &all_connections, &all_entropies, &group_concept_owner, base_dir, &mut errors);
                // Phase 9 slice 1: every read(name) in the rule's logic
                // must resolve to a declared resource. This is a separate
                // pass to keep check_expr_against's signature stable; the
                // walk is shallow and does not duplicate type checking.
                let mut referenced: Vec<String> = Vec::new();
                collect_read_names(&r.logic.value, &mut referenced);
                for (_, expr) in &r.logic.bindings {
                    collect_read_names(expr, &mut referenced);
                }
                for name in &referenced {
                    if !all_resources.contains(name) {
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", r.name),
                            message: format!(
                                "read('{}') references unknown resource — declare it at top level with `resource {} ...`",
                                name, name
                            ),
                        });
                    }
                }
                // Slice entropy-1: every random(name) in the rule's logic
                // or let RHS must resolve to a declared entropy item
                // (refusal row 5). Mirror of the resource cross-check
                // above — same shallow walk, separate namespace.
                let mut random_refs: Vec<String> = Vec::new();
                collect_random_names(&r.logic.value, &mut random_refs);
                for (_, expr) in &r.logic.bindings {
                    collect_random_names(expr, &mut random_refs);
                }
                for name in &random_refs {
                    if !all_entropies.contains(name) {
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", r.name),
                            message: format!(
                                "random('{}') references unknown entropy — declare it at top level with `entropy {} ...`",
                                name, name
                            ),
                        });
                    }
                }
                // Phase 11 slice 1: every fetch(name, ...) in the rule's
                // logic must resolve to a declared connection. Mirrors the
                // resource cross-check above — same shallow walk, separate
                // namespace.
                let mut fetch_refs: Vec<String> = Vec::new();
                collect_fetch_names(&r.logic.value, &mut fetch_refs);
                for (_, expr) in &r.logic.bindings {
                    collect_fetch_names(expr, &mut fetch_refs);
                }
                for name in &fetch_refs {
                    if !all_connections.contains(name) {
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", r.name),
                            message: format!(
                                "fetch('{}', ...) references unknown connection — declare it at top level with `connection {} ...`",
                                name, name
                            ),
                        });
                    }
                }
                // Slice-1 limit: at most one fetch per connection per rule
                // invocation. The native emitter allocates one (ptr, len,
                // buf) slot triple per connection above loop_top and would
                // need a runtime dispatch on the request bytes to fire
                // multiple distinct sequences. That dispatch lands in a
                // later slice; reject the shape here with a clear message.
                let mut seen: HashSet<&String> = HashSet::new();
                let mut dups: Vec<String> = Vec::new();
                for n in &fetch_refs {
                    if !seen.insert(n) {
                        if !dups.contains(n) {
                            dups.push(n.clone());
                        }
                    }
                }
                // collect_fetch_names dedupes already, so dups will be empty;
                // do an explicit count-walk over the AST to catch true
                // duplicates (the same connection used twice).
                let mut count_walk: Vec<String> = Vec::new();
                collect_fetch_names_with_dups(&r.logic.value, &mut count_walk);
                for (_, expr) in &r.logic.bindings {
                    collect_fetch_names_with_dups(expr, &mut count_walk);
                }
                let mut counts: HashMap<&str, usize> = HashMap::new();
                for n in &count_walk {
                    *counts.entry(n.as_str()).or_insert(0) += 1;
                }
                for (n, c) in &counts {
                    if *c > 1 {
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", r.name),
                            message: format!(
                                "slice 1: at most one fetch per connection per rule; '{}' is fetched {} times",
                                n, c
                            ),
                        });
                    }
                }
            }
            Item::Reaction(rx) => {
                // Verify source ref exists
                if let Err(msg) = verify_source_ref(&rx.source, base_dir) {
                    errors.push(VerifyError {
                        context: format!("reaction '{}' / @source", rx.name),
                        message: msg,
                    });
                }
                // Verify trigger rule exists + find it for context-typed
                // checks on effect expressions.
                let trigger_rule = all_rules.iter().find(|r| r.name == rx.trigger).copied();
                if trigger_rule.is_none() {
                    errors.push(VerifyError {
                        context: format!("reaction '{}' / trigger", rx.name),
                        message: format!("trigger references unknown rule '{}'", rx.trigger),
                    });
                }
                if let Some(rule) = trigger_rule {
                    // The concept in scope inside effects is the input concept
                    // of the triggering rule.
                    let input_concept = match &rule.input_ty {
                        Type::Named(n) => concepts.get(n).copied(),
                        _ => None,
                    };
                    for effect in &rx.effects {
                        if let Effect::AppendFile { content, .. } = effect {
                            // content must produce text at runtime — the
                            // interpreter writes bytes from a text value.
                            // Empty binding scope: a reaction's effect
                            // expression is written in the REACTION, not in
                            // the rule's `logic:` block, so the trigger rule's
                            // `let` bindings are not in scope for it. Only the
                            // input concept is, and that is passed above.
                            let no_bindings: HashMap<String, &Concept> = HashMap::new();
                            check_expr_against(
                                content,
                                &Type::Text,
                                rule,
                                &all_rules,
                                input_concept,
                                &concepts,
                                &no_bindings,
                                &mut errors,
                            );
                        }
                    }
                }
            }
            Item::Service(s) => verify_service(
                s,
                &concepts,
                &all_rules,
                &resource_max_bytes,
                &connection_max_response,
                base_dir,
                &mut errors,
            ),
            Item::Resource(r) => verify_resource_stub(r, base_dir, &mut errors),
            Item::Connection(c) => verify_connection_stub(c, base_dir, &mut errors),
            Item::Entropy(e) => verify_entropy_stub(e, base_dir, &mut errors),
        }
    }
    errors
}

/// Phase 9 slice 1: per-resource validation. Checks the @source ref
/// resolves and that max_bytes is within the slice-1 bound. Name
/// uniqueness across all top-level items is enforced separately by
/// verify_program (see the duplicate-name pre-pass).
///
/// Maximum read size capped at 64 MiB — well above any reasonable
/// "static config / template" payload, well below "we should be
/// streaming". Streaming larger files is a later slice.
const SLICE1_MAX_RESOURCE_BYTES: u32 = 64 * 1024 * 1024;

/// Phase 9 slice 1 — walk an expression tree collecting every `Read(name)`
/// reference (de-duplicated by caller). Used by verify_program to
/// cross-check that each `read(name)` resolves to a declared resource.
fn collect_read_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Read(name) => {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) | Expr::Random(_) => {}
        Expr::Field(base, _) => collect_read_names(base, out),
        Expr::Binary(_, l, r) => {
            collect_read_names(l, out);
            collect_read_names(r, out);
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => collect_read_names(i, out),
        Expr::If(c, t, e) => {
            collect_read_names(c, out);
            collect_read_names(t, out);
            collect_read_names(e, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                collect_read_names(a, out);
            }
        }
        Expr::Quantifier(_, c, _, body) => {
            collect_read_names(c, out);
            collect_read_names(body, out);
        }
        Expr::Fold(c, init, _, _, body) => {
            collect_read_names(c, out);
            collect_read_names(init, out);
            collect_read_names(body, out);
        }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
            collect_read_names(c, out);
            collect_read_names(body, out);
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            collect_read_names(t, out);
            collect_read_names(ok, out);
            collect_read_names(err, out);
        }
        Expr::Record(_, fields) => {
            for (_, e) in fields {
                collect_read_names(e, out);
            }
        }
        // Phase 11 slice 1: a fetch's connection name is collected by
        // collect_fetch_names, not here; we still recurse into the
        // request bytes expression so any nested read(...) inside a
        // request body (e.g. fetch(c, read(template))) shows up.
        Expr::Fetch(_, req) => collect_read_names(req, out),
        // Phase 12 (json_escape): pure pass-through — recurse into the
        // inner expression so any read(...) embedded in the source text
        // is still collected.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => collect_read_names(inner, out),
        // Phase 12 (parse_int): pure pass-through — recurse into the inner
        // text expression (which is typically `read(...)` itself).
        Expr::ParseInt(inner) => collect_read_names(inner, out),
        // `now_unix()` is not a resource read — its synthetic name `now`
        // is added by `collect_expr_facts` directly. No recursion needed.
        Expr::NowUnix => {}
        // `starts_with(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` (e.g. needle is loaded from
        // a resource).
        Expr::StartsWith(h, n) => {
            collect_read_names(h, out);
            collect_read_names(n, out);
        }
        // `contains(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` reference (e.g. needle is
        // loaded from a resource at runtime).
        Expr::Contains(h, n) => {
            collect_read_names(h, out);
            collect_read_names(n, out);
        }
        // `ends_with(haystack, needle)` — recurse into both children;
        // either side may carry a `read(...)` reference.
        Expr::EndsWith(h, n) => {
            collect_read_names(h, out);
            collect_read_names(n, out);
        }
        // `length(<text_expr>)` — pure pass-through; the inner may carry a
        // `read(...)` (e.g. `length(read(name))`).
        Expr::Length(inner) => collect_read_names(inner, out),
        // `abs(<number_expr>)` — pure pass-through; the inner may carry a
        // `read(...)` via `parse_int(read(name))` etc.
        Expr::Abs(inner) | Expr::BitNot(inner) | Expr::Le32(inner) | Expr::Le64(inner) | Expr::ArenaScope(inner) | Expr::AbortIf(inner) => collect_read_names(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children; either
        // side may carry a `read(...)` (e.g. `min(amount, parse_int(read(cap)))`).
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => {
            collect_read_names(l, out);
            collect_read_names(r, out);
        }
        // `substring(text, start, end)` — recurse into all three children;
        // any child may carry a `read(...)` (e.g. the source text might be
        // `read(buf)`).
        Expr::Substring(t, s, e) => {
            collect_read_names(t, out);
            collect_read_names(s, out);
            collect_read_names(e, out);
        }
        // `byte_at(text, index)` — recurse into both children; either side
        // may carry a `read(...)` (e.g. the source text might be `read(buf)`).
        Expr::ByteAt(t, i) => {
            collect_read_names(t, out);
            collect_read_names(i, out);
        }
        // `fold_bytes(text, init, acc, byte, idx => body)` — recurse into
        // text, init, and body. The three bound names (acc, byte, idx) are
        // lambda-bound so any field accesses prefixed with them are filtered
        // out by `collect_expr_facts`; here we collect every read regardless
        // and let the purity check filter (mirrors Fold's shape).
        Expr::FoldBytes(t, init, _, _, _, body) => {
            collect_read_names(t, out);
            collect_read_names(init, out);
            collect_read_names(body, out);
        }
        // Phase A slice 2: variant construction — recurse into each field
        // assignment's expression. Same shape as `Record`.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, e) in fields {
                collect_read_names(e, out);
            }
        }
        // Phase A slice 3: pattern match — recurse into scrutinee + each
        // arm's body. Same shape as MatchResult, generalized to N arms.
        Expr::MatchVariant(scrutinee, arms) => {
            collect_read_names(scrutinee, out);
            for a in arms {
                collect_read_names(&a.body, out);
            }
        }
    }
}

/// Slice entropy-1 — walk an expression tree collecting every `Random(name)`
/// reference (de-duplicated, source order). Used by verify_program to
/// cross-check that each `random(name)` resolves to a declared `entropy`
/// item. Built on `walk_expr_children` rather than as a third arm-for-arm
/// copy of `collect_read_names`: that helper is enumerated without a
/// catch-all, so a new `Expr` variant fails to compile there instead of
/// becoming a silently-unvisited subtree here.
fn collect_random_names(expr: &Expr, out: &mut Vec<String>) {
    if let Expr::Random(name) = expr {
        if !out.contains(name) {
            out.push(name.clone());
        }
    }
    walk_expr_children(expr, &mut |child| collect_random_names(child, out));
}

/// Slice entropy-1: per-item validation. The @source ref must resolve, and
/// `bytes` must sit in `1..=256` — the parser already refuses the floor at
/// the literal, this is the ceiling half of refusal row 1, worded by the
/// same shared function so the two cannot disagree about the bound. The
/// 256 is the `getrandom(2)` contract, not taste: below it a read on an
/// initialized pool cannot be short or interrupted, which is what lets the
/// emitter store the DECLARED width as the value's length.
fn verify_entropy_stub(e: &Entropy, base_dir: &StdPath, errors: &mut Vec<VerifyError>) {
    if let Err(msg) = verify_source_ref(&e.source, base_dir) {
        errors.push(VerifyError {
            context: format!("entropy '{}' / @source", e.name),
            message: msg,
        });
    }
    if e.bytes == 0 || e.bytes > 256 {
        errors.push(VerifyError {
            context: format!("entropy '{}' / bytes", e.name),
            message: entropy_bytes_range_message(&e.name, e.bytes as i64),
        });
    }
}

fn verify_resource_stub(r: &Resource, base_dir: &StdPath, errors: &mut Vec<VerifyError>) {
    if let Err(msg) = verify_source_ref(&r.source, base_dir) {
        errors.push(VerifyError {
            context: format!("resource '{}' / @source", r.name),
            message: msg,
        });
    }
    if r.max_bytes == 0 {
        errors.push(VerifyError {
            context: format!("resource '{}' / max", r.name),
            message: "max must be greater than zero".into(),
        });
    }
    if r.max_bytes > SLICE1_MAX_RESOURCE_BYTES {
        errors.push(VerifyError {
            context: format!("resource '{}' / max", r.name),
            message: format!(
                "max {} exceeds slice-1 ceiling of {} bytes (64 MiB) — larger files require streaming, a later slice",
                r.max_bytes, SLICE1_MAX_RESOURCE_BYTES
            ),
        });
    }
}

/// Phase 11 slice 1 — walk an expression tree collecting every
/// `Fetch(name, _)` connection name (de-duplicated by caller). Mirrors
/// `collect_read_names` exactly so the two stay in sync if Expr grows
/// new variants.
fn collect_fetch_names(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Fetch(name, req) => {
            if !out.contains(name) {
                out.push(name.clone());
            }
            collect_fetch_names(req, out);
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) => {}
        Expr::Read(_) | Expr::Random(_) => {}
        Expr::Field(base, _) => collect_fetch_names(base, out),
        Expr::Binary(_, l, r) => {
            collect_fetch_names(l, out);
            collect_fetch_names(r, out);
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => collect_fetch_names(i, out),
        Expr::If(c, t, e) => {
            collect_fetch_names(c, out);
            collect_fetch_names(t, out);
            collect_fetch_names(e, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                collect_fetch_names(a, out);
            }
        }
        Expr::Quantifier(_, c, _, body) => {
            collect_fetch_names(c, out);
            collect_fetch_names(body, out);
        }
        Expr::Fold(c, init, _, _, body) => {
            collect_fetch_names(c, out);
            collect_fetch_names(init, out);
            collect_fetch_names(body, out);
        }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
            collect_fetch_names(c, out);
            collect_fetch_names(body, out);
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            collect_fetch_names(t, out);
            collect_fetch_names(ok, out);
            collect_fetch_names(err, out);
        }
        Expr::Record(_, fields) => {
            for (_, e) in fields {
                collect_fetch_names(e, out);
            }
        }
        // Phase 12 (json_escape): pure pass-through — recurse into the
        // inner expression so any fetch(...) embedded in the source text
        // is still collected.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => collect_fetch_names(inner, out),
        // Phase 12 (parse_int): pure pass-through.
        Expr::ParseInt(inner) => collect_fetch_names(inner, out),
        // `now_unix()` is not a connection — leaf node, nothing to collect.
        Expr::NowUnix => {}
        // `starts_with(haystack, needle)` — recurse into both children.
        Expr::StartsWith(h, n) => {
            collect_fetch_names(h, out);
            collect_fetch_names(n, out);
        }
        // `contains(haystack, needle)` — recurse into both children.
        Expr::Contains(h, n) => {
            collect_fetch_names(h, out);
            collect_fetch_names(n, out);
        }
        // `ends_with(haystack, needle)` — recurse into both children.
        Expr::EndsWith(h, n) => {
            collect_fetch_names(h, out);
            collect_fetch_names(n, out);
        }
        // `length(<text_expr>)` — pure pass-through.
        Expr::Length(inner) => collect_fetch_names(inner, out),
        // `abs(<number_expr>)` — pure pass-through.
        Expr::Abs(inner) | Expr::BitNot(inner) | Expr::Le32(inner) | Expr::Le64(inner) | Expr::ArenaScope(inner) | Expr::AbortIf(inner) => collect_fetch_names(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => {
            collect_fetch_names(l, out);
            collect_fetch_names(r, out);
        }
        // `substring(text, start, end)` — recurse into all three children.
        Expr::Substring(t, s, e) => {
            collect_fetch_names(t, out);
            collect_fetch_names(s, out);
            collect_fetch_names(e, out);
        }
        // `byte_at(text, index)` — recurse into both children.
        Expr::ByteAt(t, i) => {
            collect_fetch_names(t, out);
            collect_fetch_names(i, out);
        }
        // `fold_bytes(text, init, acc, byte, idx => body)` — recurse into
        // text, init, and body. Same shape as Fold: no name bindings here,
        // just children.
        Expr::FoldBytes(t, init, _, _, _, body) => {
            collect_fetch_names(t, out);
            collect_fetch_names(init, out);
            collect_fetch_names(body, out);
        }
        // Phase A slice 2: variant construction — recurse into each field
        // assignment's expression. Same shape as `Record`.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, e) in fields {
                collect_fetch_names(e, out);
            }
        }
        // Phase A slice 3: pattern match — recurse into scrutinee + each
        // arm's body. Same shape as MatchResult, generalized to N arms.
        Expr::MatchVariant(scrutinee, arms) => {
            collect_fetch_names(scrutinee, out);
            for a in arms {
                collect_fetch_names(&a.body, out);
            }
        }
    }
}

/// Phase 11 slice 1 — same as `collect_fetch_names` but does NOT
/// deduplicate. Used to enforce the slice-1 "at most one fetch per
/// connection per rule invocation" rule: if any connection name appears
/// more than once in the resulting list, the rule is rejected.
fn collect_fetch_names_with_dups(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Fetch(name, req) => {
            out.push(name.clone());
            collect_fetch_names_with_dups(req, out);
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) => {}
        Expr::Read(_) | Expr::Random(_) => {}
        Expr::Field(base, _) => collect_fetch_names_with_dups(base, out),
        Expr::Binary(_, l, r) => {
            collect_fetch_names_with_dups(l, out);
            collect_fetch_names_with_dups(r, out);
        }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i) => {
            collect_fetch_names_with_dups(i, out)
        }
        Expr::If(c, t, e) => {
            collect_fetch_names_with_dups(c, out);
            collect_fetch_names_with_dups(t, out);
            collect_fetch_names_with_dups(e, out);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                collect_fetch_names_with_dups(a, out);
            }
        }
        Expr::Quantifier(_, c, _, body) => {
            collect_fetch_names_with_dups(c, out);
            collect_fetch_names_with_dups(body, out);
        }
        Expr::Fold(c, init, _, _, body) => {
            collect_fetch_names_with_dups(c, out);
            collect_fetch_names_with_dups(init, out);
            collect_fetch_names_with_dups(body, out);
        }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => {
            collect_fetch_names_with_dups(c, out);
            collect_fetch_names_with_dups(body, out);
        }
        Expr::MatchResult(t, _, ok, _, err) => {
            collect_fetch_names_with_dups(t, out);
            collect_fetch_names_with_dups(ok, out);
            collect_fetch_names_with_dups(err, out);
        }
        Expr::Record(_, fields) => {
            for (_, e) in fields {
                collect_fetch_names_with_dups(e, out);
            }
        }
        // Phase 12 (json_escape): pure pass-through.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => collect_fetch_names_with_dups(inner, out),
        // Phase 12 (parse_int): pure pass-through.
        Expr::ParseInt(inner) => collect_fetch_names_with_dups(inner, out),
        // `now_unix()` is not a fetch — leaf node, nothing to collect.
        Expr::NowUnix => {}
        // `starts_with(haystack, needle)` — recurse into both children.
        Expr::StartsWith(h, n) => {
            collect_fetch_names_with_dups(h, out);
            collect_fetch_names_with_dups(n, out);
        }
        // `contains(haystack, needle)` — recurse into both children.
        Expr::Contains(h, n) => {
            collect_fetch_names_with_dups(h, out);
            collect_fetch_names_with_dups(n, out);
        }
        // `ends_with(haystack, needle)` — recurse into both children.
        Expr::EndsWith(h, n) => {
            collect_fetch_names_with_dups(h, out);
            collect_fetch_names_with_dups(n, out);
        }
        // `length(<text_expr>)` — pure pass-through.
        Expr::Length(inner) => collect_fetch_names_with_dups(inner, out),
        // `abs(<number_expr>)` — pure pass-through.
        Expr::Abs(inner) | Expr::BitNot(inner) | Expr::Le32(inner) | Expr::Le64(inner) | Expr::ArenaScope(inner) | Expr::AbortIf(inner) => collect_fetch_names_with_dups(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => {
            collect_fetch_names_with_dups(l, out);
            collect_fetch_names_with_dups(r, out);
        }
        // `substring(text, start, end)` — recurse into all three children.
        Expr::Substring(t, s, e) => {
            collect_fetch_names_with_dups(t, out);
            collect_fetch_names_with_dups(s, out);
            collect_fetch_names_with_dups(e, out);
        }
        // `byte_at(text, index)` — recurse into both children.
        Expr::ByteAt(t, i) => {
            collect_fetch_names_with_dups(t, out);
            collect_fetch_names_with_dups(i, out);
        }
        // `fold_bytes(text, init, acc, byte, idx => body)` — recurse into
        // text, init, and body. Bound names contribute no fetches.
        Expr::FoldBytes(t, init, _, _, _, body) => {
            collect_fetch_names_with_dups(t, out);
            collect_fetch_names_with_dups(init, out);
            collect_fetch_names_with_dups(body, out);
        }
        // Phase A slice 2: variant construction — recurse into each field
        // assignment's expression. Same shape as `Record`.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, e) in fields {
                collect_fetch_names_with_dups(e, out);
            }
        }
        // Phase A slice 3: pattern match — recurse into scrutinee + each
        // arm's body. Same shape as MatchResult, generalized to N arms.
        Expr::MatchVariant(scrutinee, arms) => {
            collect_fetch_names_with_dups(scrutinee, out);
            for a in arms {
                collect_fetch_names_with_dups(&a.body, out);
            }
        }
    }
}

/// Phase 11 slice 1: max response buffer size. Same envelope as
/// SLICE1_MAX_RESOURCE_BYTES — well above any reasonable HTTP/1.0
/// response payload, well below "we should be streaming".
const SLICE1_MAX_RESPONSE_BYTES: u32 = 64 * 1024 * 1024;

fn verify_connection_stub(c: &Connection, base_dir: &StdPath, errors: &mut Vec<VerifyError>) {
    if let Err(msg) = verify_source_ref(&c.source, base_dir) {
        errors.push(VerifyError {
            context: format!("connection '{}' / @source", c.name),
            message: msg,
        });
    }
    // Host: parser already validates the dotted-quad shape, so reaching
    // here with a malformed host means a bug in the parser (or an AST
    // built bypassing the parser, e.g. a unit test). Re-validate here so
    // a programmatic AST cannot smuggle in a bad host.
    let octets: Vec<&str> = c.host.split('.').collect();
    let mut host_ok = octets.len() == 4;
    if host_ok {
        for o in &octets {
            match o.parse::<u32>() {
                Ok(n) if n <= 255 => {}
                _ => { host_ok = false; break; }
            }
        }
    }
    if !host_ok {
        errors.push(VerifyError {
            context: format!("connection '{}' / host", c.name),
            message: format!(
                "host '{}' is not an IPv4 dotted quad (slice 1 supports IPv4 literals only — no DNS, no IPv6)",
                c.host
            ),
        });
    }
    if c.port == 0 {
        errors.push(VerifyError {
            context: format!("connection '{}' / port", c.name),
            message: "port must be in 1..=65535".into(),
        });
    }
    if c.max_response == 0 {
        errors.push(VerifyError {
            context: format!("connection '{}' / max_response", c.name),
            message: "max_response must be greater than zero".into(),
        });
    }
    if c.max_response > SLICE1_MAX_RESPONSE_BYTES {
        errors.push(VerifyError {
            context: format!("connection '{}' / max_response", c.name),
            message: format!(
                "max_response {} exceeds slice-1 ceiling of {} bytes (64 MiB) — larger responses require streaming, a later slice",
                c.max_response, SLICE1_MAX_RESPONSE_BYTES
            ),
        });
    }
}

/// Phase 7 slice 3a: synthesised `HttpRequest` concept injected into the
/// program's concept scope when any Http10 service is present. The auditor
/// does not see this declaration in any .verbose file; it lives in the
/// compiler because the wire-format-to-concept bridge is a closed,
/// compiler-owned translation. Fields:
///   method : text [..8]    — GET / POST / DELETE / etc. (fits OPTIONS = 7)
///   path   : text [..256]  — URL path segment
///   body   : text [..body_max] — the bytes after the \r\n\r\n delimiter.
///                            Stored as (ptr, len) — body may contain
///                            arbitrary bytes so NUL-termination is unsafe.
///
/// `body_max` TRACKS the service's declared `max_request` and is not a
/// constant. It used to be a hardcoded 4096 while the doc comment on this
/// very function said body was "capped by the service's `max_request` at
/// runtime" — and `max_request` has no UPPER bound anywhere (verify_service
/// checks `!= 0`, plus `>= 64` for http_1_0; both are floors), so a service
/// declaring `max_request : 65536` carried a
/// declared `[..4096]` bound on a field that can hold ~65500 bytes. That is
/// a FALSE declaration, and a declared `[..N]` on a text field is EXPLOITED
/// by the native emitter for compile-time buffer sizing (see
/// `emit_concat_to_buffer_impl`'s `static_total += max_len` and its
/// `2 * max_len` json_escape sibling) — the exact shape of the 2026-08-05
/// argv-controlled overflow. Deriving the bound from `max_request` makes the
/// declaration true BY CONSTRUCTION: the body is a suffix of a buffer the
/// `read(client_fd, buf, max_request)` syscall already caps, so no runtime
/// check is needed to make it hold.
///
/// The concept is synthesised once per PROGRAM while `max_request` is
/// per-service, so the caller passes the MAXIMUM over every Http10 service —
/// the tightest program-wide bound that is true for all of them. Native
/// synthesises its own copy per COMPILED SERVICE and uses that service's
/// exact `max_request` (see `http_request_builtin_concept_native`), which is
/// ≤ this value; the divergence is only ever in the safe direction, and for
/// a single-service program (every example in the repo, and the only shape
/// `--run <service>` compiles) the two are identical.
fn builtin_http_request(body_max: i64) -> Concept {
    Concept {
        name: "HttpRequest".to_string(),
        intention:
            "A parsed HTTP/1.0 request: method, path, and body (compiler built-in for Protocol::Http10)"
                .to_string(),
        source: SourceRef { file: "<builtin>".to_string(), line: 0 },
        fields: vec![
            Field {
                name: "method".to_string(),
                ty: Type::Text,
                range: Some((0, 8)),
            },
            Field {
                name: "path".to_string(),
                ty: Type::Text,
                range: Some((0, 256)),
            },
            Field {
                name: "body".to_string(),
                ty: Type::Text,
                range: Some((0, body_max)),
            },
        ],
        variants: vec![],
    }
}

/// Phase 7 slice 3a: synthesised `HttpResponse` concept, counterpart of
/// `HttpRequest`. Fields:
///   status : number [100, 599] — valid HTTP status code range
///   body   : text [..4096]     — response body (text only in slice 3;
///                                binary bodies await bytes primitives)
fn builtin_http_response() -> Concept {
    Concept {
        name: "HttpResponse".to_string(),
        intention:
            "An HTTP/1.0 response: status and body (compiler built-in for Protocol::Http10)"
                .to_string(),
        source: SourceRef { file: "<builtin>".to_string(), line: 0 },
        fields: vec![
            Field {
                name: "status".to_string(),
                ty: Type::Number,
                range: Some((100, 599)),
            },
            Field {
                name: "body".to_string(),
                ty: Type::Text,
                range: Some((0, 4096)),
            },
        ],
        variants: vec![],
    }
}

/// Phase 7 service verification.
///
/// Checks:
///   - @source file:line exists (same discipline as concept / rule / reaction)
///   - port is in [1, 65535] — statically guaranteed by the parser storing
///     port as u16, but we keep the check explicit for audit readability
///   - max_request > 0 (zero-byte read makes no sense for a listener)
///   - the handler rule exists in the program
///   - for RawTcp: the handler's input and output are each a Named concept
///     with exactly one `bytes [..max_request]` field. The bound MUST match
///     the service's declared max_request exactly — a looser handler bound
///     would leak unread bytes, a tighter one would truncate.
/// Slice `text-state-1`: the largest `[..N]` a text state field may declare.
///
/// A judgement call, and it should be read as one. The state block shares one
/// `sub rsp, imm32` frame with `max_request` and every handler let, so 64 KiB
/// per field is generous for the consumers in
/// `docs/text-state-fields-design.md` §1.2 while keeping the block from
/// dominating the frame. It is deliberately NOT the effect model's 64 MiB
/// resource ceiling: a resource buffer is per-invocation, a state buffer is
/// per-process-lifetime.
const TEXT_STATE_MAX_BYTES: i64 = 65536;

fn verify_service(
    s: &Service,
    concepts: &HashMap<String, &Concept>,
    all_rules: &[&Rule],
    resource_max_bytes: &HashMap<&str, i64>,
    connection_max_response: &HashMap<&str, i64>,
    base_dir: &StdPath,
    errors: &mut Vec<VerifyError>,
) {
    if let Err(msg) = verify_source_ref(&s.source, base_dir) {
        errors.push(VerifyError {
            context: format!("service '{}' / @source", s.name),
            message: msg,
        });
    }

    if s.port == 0 {
        errors.push(VerifyError {
            context: format!("service '{}' / listen.port", s.name),
            message: "port must be in [1, 65535]; 0 is not bindable".into(),
        });
    }

    if s.max_request == 0 {
        errors.push(VerifyError {
            context: format!("service '{}' / listen.max_request", s.name),
            message: "max_request must be greater than zero".into(),
        });
    }

    let handler = match all_rules.iter().find(|r| r.name == s.handler) {
        Some(r) => *r,
        None => {
            errors.push(VerifyError {
                context: format!("service '{}' / handler", s.name),
                message: format!("handler references unknown rule '{}'", s.handler),
            });
            return;
        }
    };

    match s.protocol {
        Protocol::RawTcp => {
            // RawTcp handler shape: input and output must each be a Named
            // concept whose single field is `bytes [..max_request]`. Enforced
            // strictly so that what the service reads exactly matches what
            // the handler expects, and what the handler returns exactly
            // matches what the service writes.
            let expected_bound = s.max_request as i64;
            check_raw_tcp_binding(
                &handler.input_ty,
                handler.name.as_str(),
                "input",
                expected_bound,
                concepts,
                s,
                errors,
            );
            check_raw_tcp_binding(
                &handler.output_ty,
                handler.name.as_str(),
                "output",
                expected_bound,
                concepts,
                s,
                errors,
            );
        }
        Protocol::Http10 => {
            // Http10 handler shape: input is Named("HttpRequest"),
            // output is Named("HttpResponse"). No field-level check —
            // the built-in concepts have fixed shapes and are synthesised
            // by the verifier (see builtin_http_request / _response).
            // max_request must be >= 64 (HTTP/1.0 minimum well-formed
            // request: "GET / HTTP/1.0\r\n\r\n" = 18 bytes; 64 gives
            // room for the shortest realistic path + version).
            check_http10_binding(
                &handler.input_ty,
                handler.name.as_str(),
                "input",
                "HttpRequest",
                s,
                errors,
            );
            check_http10_binding(
                &handler.output_ty,
                handler.name.as_str(),
                "output",
                "HttpResponse",
                s,
                errors,
            );
            if s.max_request < 64 {
                errors.push(VerifyError {
                    context: format!("service '{}' / listen.max_request", s.name),
                    message: format!(
                        "http_1_0 requires max_request >= 64 (minimum well-formed HTTP/1.0 request); got {}",
                        s.max_request
                    ),
                });
            }
        }
    }

    // Phase 8 slices 8a/8b/8c: if a log effect is declared, validate its
    // content against the closed log-scope grammar (text literals, scalar
    // numbers, concat thereof, and field accesses on the synthetic `req`
    // and `resp` bindings). The subset is intentionally narrow — anything
    // outside of it is rejected here rather than silently miscompiled.
    // Phase 8 slice 8e: each log block is verified independently; multiple
    // blocks on the same service compose without restriction (closed
    // grammar applies block-by-block, on_error policy is per-block). The
    // index in the error context lets a misdeclared second block surface
    // its own message instead of being swallowed by a first-block fix.
    for (i, log_block) in s.logs.iter().enumerate() {
        let scope_ctx = if s.logs.len() == 1 {
            format!("service '{}' / log", s.name)
        } else {
            format!("service '{}' / log[{}]", s.name, i)
        };
        match &log_block.effect {
            Effect::AppendFile { content, .. } => {
                if s.protocol != Protocol::Http10 {
                    errors.push(VerifyError {
                        context: scope_ctx,
                        message: "Phase 8 slice 8a restricts log to http_1_0 services (raw_tcp log lands in a later slice)".into(),
                    });
                } else {
                    let req_concept = match &handler.input_ty {
                        Type::Named(n) => concepts.get(n).copied(),
                        _ => None,
                    };
                    let resp_concept = match &handler.output_ty {
                        Type::Named(n) => concepts.get(n).copied(),
                        _ => None,
                    };
                    if let Err(msg) =
                        verify_log_content(content, req_concept, resp_concept, &Type::Text)
                    {
                        errors.push(VerifyError { context: scope_ctx, message: msg });
                    }
                }
            }
            // Reactions today only define AppendFile and Print; parser
            // rejects Print in the log block, so this arm is defensive.
            Effect::Print(_) => {
                errors.push(VerifyError {
                    context: scope_ctx,
                    message: "Phase 8 slice 8a: log blocks accept only 'append_file', not 'print'".into(),
                });
            }
        }
    }

    // ── Slice `multistep-1` (docs/multistep-connection-design.md §5) ──────
    //
    // A `raw_tcp` service declaring `max_steps` + `read_timeout` is
    // MULTI-STEP: one forked child per connection runs a per-connection step
    // loop (read → handler → response → `after:`), so `state:` / `after:`
    // gain a second lifetime — per connection — and `concurrency: forked`
    // becomes mandatory (§5.4). The four declaration-shape refusals below
    // (#1–#4 of §5.5) come FIRST, each attributable to one missing or
    // misplaced key, so a reader is never sent to the emitter for a shape
    // the source already decides.
    let multistep = s.max_steps.is_some() || s.read_timeout.is_some();

    // Refusal #2 — both knobs or neither. `max_steps` alone bounds WORK, not
    // TIME: it says how many frames a client may send, not how long it may
    // hold a child. Shipping it alone would be false explicitation.
    if s.max_steps.is_some() != s.read_timeout.is_some() {
        errors.push(VerifyError {
            context: format!("service '{}' / max_steps + read_timeout", s.name),
            message: format!(
                "service '{}': a multi-step service must declare both 'max_steps' (a work bound) and \
                 'read_timeout' (a time bound). max_steps alone bounds how many frames a client may \
                 send, not how long it may hold a child; read_timeout alone bounds one read, not the \
                 conversation. Declared: max_steps {}, read_timeout {}",
                s.name,
                s.max_steps.map(|n| n.to_string()).unwrap_or_else(|| "absent".into()),
                s.read_timeout.map(|n| n.to_string()).unwrap_or_else(|| "absent".into()),
            ),
        });
    }

    // Refusal #4 — the step loop is a raw_tcp shape. http_1_0 is one request
    // per connection by protocol; HTTP/1.1 keep-alive is the slice that
    // brings the step loop under that protocol (and the one that makes the
    // `state:` / `session:` split mandatory — design §6.1).
    if multistep && s.protocol == Protocol::Http10 {
        errors.push(VerifyError {
            context: format!("service '{}' / max_steps", s.name),
            message: format!(
                "service '{}': max_steps applies to raw_tcp multi-step services; http_1_0 is one \
                 request per connection. HTTP/1.1 keep-alive is a later slice.",
                s.name
            ),
        });
    }

    // Refusal #3 — a multi-step service must fork. A sequential server
    // holding one connection open across a whole conversation is monopolised
    // for as long as the peer chooses (§5.4: the WELL-BEHAVED slow client is
    // what the step loop changes); forked isolates each conversation to one
    // child. Sequential multi-step needs poll/epoll — a separate arc.
    if multistep && s.protocol == Protocol::RawTcp && s.concurrency != ConcurrencyMode::Forked {
        errors.push(VerifyError {
            context: format!("service '{}' / concurrency", s.name),
            message: format!(
                "service '{}': a multi-step raw_tcp service must declare 'concurrency: forked'. A \
                 sequential server holding one connection open across a whole conversation is \
                 monopolised for as long as the peer chooses; forked isolates each conversation to \
                 one child.",
                s.name
            ),
        });
    }

    // Phase 10 slice 10, re-scoped by slice `multistep-1`: forked concurrency
    // is accepted on http_1_0 (one request per child) and on a raw_tcp
    // service WITH a step loop (one conversation per child). A one-shot
    // raw_tcp service stays refused: forking it buys nothing the step loop
    // does not need, and the echo emitter has no fork dispatch.
    if s.concurrency == ConcurrencyMode::Forked
        && s.protocol != Protocol::Http10
        && !multistep
    {
        errors.push(VerifyError {
            context: format!("service '{}' / concurrency", s.name),
            message: format!(
                "service '{}': concurrency: forked is accepted on http_1_0 services and on raw_tcp \
                 services with a step loop (declare max_steps + read_timeout, slice multistep-1); a \
                 one-shot raw_tcp service is sequential",
                s.name
            ),
        });
    }

    // Mutable state validation.
    // 1. Each state field must be Number- or Text-typed (bytes: refusal #7,
    //    at parse time).
    // 2. No duplicate field names.
    // 3. Each after_set must reference an existing state field.
    // 4. Refusal #1 (§5.5) — state on a raw_tcp service needs the step loop.
    //    THE ONE THAT MATTERS MOST: `compile_service` hands the one-shot
    //    identity emitter two scalars (port, max_request) and cannot see a
    //    `state:` block at all, so without this gate the declaration would
    //    compile to the identity echo and be DROPPED silently — the exact
    //    hazard the HTTP constant fast path already records for `after:`.
    //    Keyed on the step loop's PRESENCE (`max_steps`), not on forked:
    //    forked without max_steps is already refused above, and one refusal
    //    per missing key is what keeps each attributable.
    if (!s.state_fields.is_empty() || !s.after_sets.is_empty())
        && s.protocol == Protocol::RawTcp
        && s.max_steps.is_none()
    {
        errors.push(VerifyError {
            context: format!("service '{}' / state", s.name),
            message: format!(
                "service '{}': a raw_tcp service declaring state must also declare 'max_steps' (and \
                 'read_timeout' and 'concurrency: forked'). Without a step loop the one-shot emitter \
                 would compile this to the identity echo and DROP the state declaration silently.",
                s.name
            ),
        });
    }
    // 5. An `after:` mutation together with `concurrency: forked` is REFUSED.
    //
    // Not a degraded mode — a WRITE-ONLY declaration. `fork()` is per-accept
    // and http_1_0 serves exactly one request per connection, so every child
    // starts from the parent's *unchanged* slots, runs its `after:` block
    // against its own copy-on-write page, and `sys_exit(0)`s. The parent
    // never re-reads the slot and no sibling ever sees it, so the mutation
    // is observed by nobody, ever. Measured on a running binary: the counter
    // service (which answers count:0 / count:1 / count:2 / count:3 under the
    // default sequential mode) answers count:0 to EVERY request once
    // `concurrency: forked` is added — a constant, not per-connection
    // counting.
    //
    // Making the mutation propagate would mean shared memory between
    // processes, which docs/effect-model.md refuses on principle (see its
    // "Pthreads / shared-memory concurrency" entry: locks need a memory
    // model, and a memory model is a research problem in its own right).
    // So this combination is UNBUILDABLE under the standing effect model,
    // not merely unbuilt — which is why it is refused at verify time rather
    // than documented as a caveat.
    //
    // Deliberately keyed on `after_sets`, NOT on `state_fields`: a `state:`
    // block with no `after:` block is a per-process CONSTANT, and a constant
    // reads identically under both concurrency modes. Refusing that shape
    // would reject a valid program, the one direction this project's
    // verifier checks must never move in.
    //
    // RE-KEYED by slice `multistep-1` (design §6.1), not reused: the refusal
    // exists because WITHOUT A LOOP the child's mutation is observed by
    // nobody. With a step loop the mutation IS observed — by the next step
    // of the same connection, in the same child — so the condition gains
    // `&& no step loop`. Still keyed on `after_sets`, for the reason above.
    if !s.after_sets.is_empty()
        && s.concurrency == ConcurrencyMode::Forked
        && !multistep
    {
        errors.push(VerifyError {
            context: format!("service '{}' / after + concurrency", s.name),
            message: format!(
                "service mutates state in its 'after:' block ({} set(s): [{}]) while declaring \
                 'concurrency: forked'; the combination is refused because the mutation would be write-only. \
                 fork() is per-accept and http_1_0 serves one request per connection, so every child starts \
                 from the parent's unchanged slots, mutates its own copy-on-write page, and exits — no request \
                 ever observes a mutation, so the state reads as a constant rather than counting per \
                 connection. Propagating it would require shared memory between processes, which \
                 docs/effect-model.md refuses on principle ('Pthreads / shared-memory concurrency'), so this \
                 is unbuildable rather than unbuilt. Remove 'concurrency: forked' to keep the mutation, or \
                 remove the 'after:' block to keep forked concurrency — or, on a raw_tcp service, declare \
                 max_steps + read_timeout: with a step loop the mutation is observed by the next step of \
                 the same connection in the same child (slice multistep-1).",
                s.after_sets.len(),
                s.after_sets.iter().map(|st| st.field_name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        });
    }
    {
        let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for sf in &s.state_fields {
            match sf.ty {
                Type::Number => {}
                Type::Text => {
                    // Refusal #2 — the declared bound sizes a buffer inside
                    // the one service frame.
                    let n = sf.max_bytes.unwrap_or(0);
                    if n < 1 || n > TEXT_STATE_MAX_BYTES {
                        errors.push(VerifyError {
                            context: format!("service '{}' / state / {}", s.name, sf.name),
                            message: format!(
                                "state field '{}': declared bound {} must be in 1..={}; the state block shares \
                                 one stack frame with max_request and every handler let",
                                sf.name, n, TEXT_STATE_MAX_BYTES
                            ),
                        });
                    }
                    // Refusal #3 — the init copy is a compile-time-sized
                    // `rep movsb` into that buffer.
                    if let StateInit::Text(lit) = &sf.init {
                        let l = lit.as_bytes().len() as i64;
                        if l > n {
                            errors.push(VerifyError {
                                context: format!("service '{}' / state / {}", s.name, sf.name),
                                message: format!(
                                    "state field '{}': initial value is {} bytes but the declared bound is {}",
                                    sf.name, l, n
                                ),
                            });
                        }
                    }
                }
                _ => {
                    errors.push(VerifyError {
                        context: format!("service '{}' / state / {}", s.name, sf.name),
                        message: format!(
                            "state field '{}' must be type 'number' or 'text'; got {:?}",
                            sf.name, sf.ty
                        ),
                    });
                }
            }
            if !seen_names.insert(sf.name.as_str()) {
                errors.push(VerifyError {
                    context: format!("service '{}' / state / {}", s.name, sf.name),
                    message: format!("duplicate state field name '{}'", sf.name),
                });
            }
        }
    }
    for aset in &s.after_sets {
        // Slice entropy-1, refusal row 11: a draw in an `after:` set. The
        // type check further down would ALSO refuse it (a `bytes` value
        // against a number or text field), but with the wrong diagnosis —
        // "wrong type" rather than "not supported here". Named first so the
        // author is pointed at the slice that lifts it, and so a future
        // bytes-typed state field (text-state-4) does not silently admit a
        // draw into state without its own argument.
        let mut draws: Vec<String> = Vec::new();
        collect_random_names(&aset.value, &mut draws);
        for name in &draws {
            errors.push(VerifyError {
                context: format!("service '{}' / after / set {}", s.name, aset.field_name),
                message: format!(
                    "after: set {}: random('{}') is not supported here (slice entropy-6: a draw \
                     stored into service state needs its own secrecy argument — state is not secret)",
                    aset.field_name, name
                ),
            });
        }
        if !s.state_fields.iter().any(|sf| sf.name == aset.field_name) {
            errors.push(VerifyError {
                context: format!("service '{}' / after / set {}", s.name, aset.field_name),
                message: format!(
                    "after block sets unknown state field '{}'; declared state fields: [{}]",
                    aset.field_name,
                    s.state_fields.iter().map(|sf| sf.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            });
        }
    }

    // ── SECURITY: type-check every `set <field> = <expr>` ─────────────────
    //
    // A `set` whose RHS does not produce the state field's declared type was
    // NEVER checked, and for a NUMBER field the consequence is a remotely
    // reachable ASLR disclosure: `set count = req.path` on `count : number`
    // compiled to `mov rax, [rbp - <req.path's ptr slot>]` followed by a
    // store into the number slot, so the HTTP read buffer's ADDRESS became
    // the counter — and the handler then serves it. Measured on
    // `examples/counter_service.verbose` with only that one line changed:
    // three requests answered `count:0`, then `count:140733822829620` twice,
    // and the value is a canonical `0x7ffd…` stack address that DIFFERS on
    // every restart. Same family as the 2026-08-20 `t.s * 2` leak — the
    // verifier certifying a program whose emitter then does pointer
    // arithmetic — but reachable by an unauthenticated remote client rather
    // than printed by a CLI binary the operator ran themselves.
    //
    // Fixed by routing the RHS through `check_expr_against`, the SAME
    // bidirectional checker that already covers a rule's body, every `let`
    // RHS, and every binary operand. A second mechanism is precisely how
    // this hole existed: the `after:` block was simply never handed to the
    // checker at all.
    //
    // `state.<field>` resolves through the EXISTING `bindings` map rather
    // than through a new lookup path — a synthetic concept whose fields ARE
    // the state fields, registered under the name `state`, so
    // `infer_expr_type`'s `Field(Ident(b), f)` arm answers with the declared
    // type. No new arm, no second notion of what `state.x` means.
    //
    // Errors are re-contexted to `service '<s>' / after / set <f>`, because
    // `check_expr_against` names the RULE it was given and a reader must be
    // pointed at the `after:` line, not at the handler's `logic:`.
    //
    // A TEXT field was already refused for every Number/Bool source, but
    // INCIDENTALLY — `text_source_worst_case` (the overflow gate below) has
    // no arm for an arithmetic / `now_unix()` / `length()` shape and reports
    // "no compile-time byte bound". True, and the wrong diagnosis. The type
    // check runs FIRST and the bound gate is skipped for a set it already
    // flagged, so each offence yields exactly one attributable error.
    let state_type_errors: HashSet<&str> = if s.state_fields.is_empty() {
        HashSet::new()
    } else {
        let state_concept = Concept {
            name: "<service state>".to_string(),
            intention: String::new(),
            source: SourceRef { file: "<builtin>".to_string(), line: 0 },
            fields: s
                .state_fields
                .iter()
                .map(|sf| Field {
                    name: sf.name.clone(),
                    ty: sf.ty.clone(),
                    range: sf.max_bytes.map(|n| (0, n)),
                })
                .collect(),
            variants: vec![],
        };
        let mut set_bindings: HashMap<String, &Concept> = HashMap::new();
        set_bindings.insert("state".to_string(), &state_concept);
        let handler_input_concept = match &handler.input_ty {
            Type::Named(n) => concepts.get(n).copied(),
            _ => None,
        };
        let mut flagged: HashSet<&str> = HashSet::new();
        for aset in &s.after_sets {
            let Some(sf) = s.state_fields.iter().find(|sf| sf.name == aset.field_name) else {
                continue; // unknown field already reported above
            };
            let mut local: Vec<VerifyError> = Vec::new();
            check_expr_against(
                &aset.value,
                &sf.ty,
                handler,
                all_rules,
                handler_input_concept,
                concepts,
                &set_bindings,
                &mut local,
            );
            if !local.is_empty() {
                flagged.insert(sf.name.as_str());
            }
            for e in local {
                errors.push(VerifyError {
                    context: format!("service '{}' / after / set {}", s.name, aset.field_name),
                    message: format!(
                        "after: set '{}' = <expr>: {} (state field '{}' is declared '{}')",
                        aset.field_name,
                        e.message,
                        sf.name,
                        type_display(&sf.ty),
                    ),
                });
            }
        }
        flagged
    };

    // ── Slice `text-state-1`: the compile-time overflow gate ──────────────
    //
    // `docs/text-state-fields-design.md` §3.3 option E. For each
    // `set <f> = <expr>` where `f` is a TEXT state field, prove
    // `worst_case(expr) <= N` from declarations alone, or refuse. A program
    // that passes carries a proof rather than an assertion and emits ZERO
    // check bytes; the 13-byte backstop the emitter keeps at the copy site is
    // unreachable by construction.
    //
    // The gate is sound only because every source's compile-time bound is
    // ALREADY runtime-enforced at the point the bytes enter the process:
    // req.method / req.path by `emit_token_len_guard` in the HTTP parse,
    // req.body by the kernel's `read(client_fd, buf, max_request)` count,
    // read() by the resource's `max:`, fetch() by `max_response`, another
    // state field by this very gate. **That is a stated dependency: weaken any
    // one of those and this gate breaks silently.**
    //
    // Why not a runtime `cmp` + `sys_exit(1)` as the primary gate: this is a
    // LISTENER, not a one-shot rule binary. A client who could drive the
    // source past N would have a one-packet DoS. The emitter already made
    // exactly this call, in writing, for the HTTP-parse bound guards — an
    // over-long path closes that connection instead of killing the process.
    if !s.state_fields.is_empty() {
        let text_state_bounds: HashMap<&str, i64> = s
            .state_fields
            .iter()
            .filter(|sf| sf.ty == Type::Text)
            .map(|sf| (sf.name.as_str(), sf.max_bytes.unwrap_or(0)))
            .collect();
        if !text_state_bounds.is_empty() {
            // req.method / req.path bounds come from the concept so the
            // parser and this sizer can never disagree; req.body's bound is
            // THIS service's `max_request` (the concept carries the
            // program-wide maximum, which is a looser but still-true bound —
            // native synthesises its own per-service copy, so the tight one
            // is the honest one to check against).
            let req_concept = match &handler.input_ty {
                Type::Named(n) => concepts.get(n).copied(),
                _ => None,
            };
            for aset in &s.after_sets {
                let Some(sf) = s.state_fields.iter().find(|sf| sf.name == aset.field_name) else {
                    continue; // unknown field already reported above
                };
                if sf.ty != Type::Text {
                    continue;
                }
                // Already reported as a TYPE error above; a second
                // "no compile-time byte bound" line for the same `set`
                // would be an unattributable duplicate.
                if state_type_errors.contains(sf.name.as_str()) {
                    continue;
                }
                let n = sf.max_bytes.unwrap_or(0);
                match text_source_worst_case(
                    &aset.value,
                    &handler.input_name,
                    req_concept,
                    s.max_request as i64,
                    &text_state_bounds,
                    &handler.logic.bindings,
                    resource_max_bytes,
                    connection_max_response,
                    0,
                ) {
                    // Refusal #5 — no compile-time bound for this shape.
                    // The accepted-source list is protocol-aware (slice
                    // multistep-1, design §5.5 #6): a raw_tcp input field is
                    // BYTES and can never be a text source, so naming
                    // `<inp>.method / .path / .body` there would send the
                    // author to fields the concept does not have.
                    Err(kind) => errors.push(VerifyError {
                        context: format!("service '{}' / after / set {}", s.name, aset.field_name),
                        message: match s.protocol {
                            Protocol::Http10 => format!(
                                "after: set '{}' = <expr>: source shape '{}' has no compile-time byte bound, so the \
                                 copy into a fixed {}-byte buffer cannot be proved safe. Slice text-state-1 accepts \
                                 literals, {inp}.method / {inp}.path / {inp}.body, state fields, read(), fetch(), \
                                 handler text lets, substring of those, and concat of those.",
                                aset.field_name, kind, n, inp = handler.input_name
                            ),
                            Protocol::RawTcp => format!(
                                "after: set '{}' = <expr>: source shape '{}' has no compile-time byte bound, so the \
                                 copy into a fixed {}-byte buffer cannot be proved safe. On a raw_tcp service the \
                                 accepted sources are literals, state fields, handler text lets, substring of \
                                 those, and concat of those — the input field {inp}.<field> is bytes, not text, \
                                 and reaches a text set only through a handler let built from length() / \
                                 byte_at() (slice multistep-1).",
                                aset.field_name, kind, n, inp = handler.input_name
                            ),
                        },
                    }),
                    // Refusal #6 — bounded, but too big. Named explicitly so a
                    // reader is not left thinking the bound is merely small.
                    Ok(w) if w > n => errors.push(VerifyError {
                        context: format!("service '{}' / after / set {}", s.name, aset.field_name),
                        message: format!(
                            "after: set '{}' = <expr>: worst case {} bytes exceeds the declared bound {}. \
                             Append-accumulation (concat(state.{}, …)) can never satisfy this — it needs a \
                             declared overflow policy, which is slice text-state-2.",
                            aset.field_name, w, n, aset.field_name
                        ),
                    }),
                    Ok(_) => {}
                }
            }
        }
        // Refusals #8 / #9 — read positions this slice has not wired.
        check_state_text_read_positions(s, handler, errors);
    }

    // Cross-check: handler's `reads:` paths of the form `state.X` must
    // reference actual state fields declared in this service.
    if !s.state_fields.is_empty() {
        let handler_facts = collect_logic_facts(&handler.logic);
        for path in &handler_facts.reads {
            if path.len() == 2 && path[0] == "state" {
                let field_name = &path[1];
                if !s.state_fields.iter().any(|sf| &sf.name == field_name) {
                    errors.push(VerifyError {
                        context: format!("service '{}' / handler '{}' / reads", s.name, s.handler),
                        message: format!(
                            "handler reads state.{} but service declares no state field '{}'; declared: [{}]",
                            field_name,
                            field_name,
                            s.state_fields.iter().map(|sf| sf.name.as_str()).collect::<Vec<_>>().join(", ")
                        ),
                    });
                }
            }
        }
    }
}

/// Slice `text-state-1`: the compile-time upper bound, in bytes, of a text
/// expression usable as a `set <text state field> = <expr>` source.
///
/// `Ok(n)` is a proof that the expression can never produce more than `n`
/// bytes at runtime. `Err(kind)` names the shape that has no such bound; the
/// caller turns it into refusal #5.
///
/// Every accepted row of `docs/text-state-fields-design.md` §3.3 whose native
/// producer (`emit_text_produce_ptrlen`) exists today:
///
/// | shape                    | bound                                    |
/// |--------------------------|------------------------------------------|
/// | text literal             | its exact byte length                    |
/// | `<input>.method/path`    | the built-in concept's declared `[..N]`  |
/// | `<input>.body`           | the service's `max_request`              |
/// | `state.<g>`              | `g`'s declared `[..N]`                   |
/// | `read(r)`                | `r`'s `max:`                             |
/// | `fetch(c, _)`            | `c`'s `max_response:`                    |
/// | a handler text `let`     | recursive over its RHS                   |
/// | `substring(t, _, _)`     | bound of `t` (a slice ≤ its haystack)    |
/// | `concat(a, b, …)`        | the sum of the arg bounds                |
///
/// DELIBERATELY ABSENT, each refused rather than guessed at: `json_escape`
/// (§3.3 bounds it at `2 × bound(inner)`, but `emit_text_produce_ptrlen` has
/// no arm for it, so accepting it here would push a verified program into an
/// emitter refusal), a text-returning rule `Call` (bounded only by recursing
/// through another rule's body — its own slice), and every Number-producing
/// shape (§6.1's accepted list is text sources only).
#[allow(clippy::too_many_arguments)]
fn text_source_worst_case(
    expr: &Expr,
    input_name: &str,
    req_concept: Option<&Concept>,
    max_request: i64,
    text_state_bounds: &HashMap<&str, i64>,
    handler_lets: &[(String, Expr)],
    resource_max_bytes: &HashMap<&str, i64>,
    connection_max_response: &HashMap<&str, i64>,
    depth: u32,
) -> Result<i64, String> {
    // A handler `let` can only reference bindings declared before it, so the
    // recursion is over a DAG and cannot cycle. The cap is a belt-and-braces
    // guard against a future shape that could.
    if depth > 16 {
        return Err("deeply nested let chain".to_string());
    }
    let recur = |e: &Expr| {
        text_source_worst_case(
            e,
            input_name,
            req_concept,
            max_request,
            text_state_bounds,
            handler_lets,
            resource_max_bytes,
            connection_max_response,
            depth + 1,
        )
    };
    match expr {
        Expr::Text(s) => Ok(s.as_bytes().len() as i64),
        Expr::Field(base, fname) => match base.as_ref() {
            Expr::Ident(b) if b == input_name => {
                if fname == "body" {
                    return Ok(max_request);
                }
                let f = req_concept
                    .and_then(|c| c.fields.iter().find(|f| &f.name == fname))
                    .ok_or_else(|| format!("{}.{}", input_name, fname))?;
                match (&f.ty, f.range) {
                    (Type::Text, Some((_, max))) => Ok(max),
                    _ => Err(format!("{}.{}", input_name, fname)),
                }
            }
            Expr::Ident(b) if b == "state" => text_state_bounds
                .get(fname.as_str())
                .copied()
                .ok_or_else(|| format!("state.{} (not a text state field)", fname)),
            _ => Err(format!("field access on '{:?}'", base)),
        },
        Expr::Read(name) => resource_max_bytes
            .get(name.as_str())
            .copied()
            .ok_or_else(|| format!("read({}) — undeclared resource", name)),
        Expr::Fetch(name, _) => connection_max_response
            .get(name.as_str())
            .copied()
            .ok_or_else(|| format!("fetch({}, …) — undeclared connection", name)),
        Expr::Ident(name) => {
            let (_, rhs) = handler_lets
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("identifier '{}'", name))?;
            recur(rhs)
        }
        Expr::Substring(t, _, _) => recur(t),
        Expr::Concat(args) => {
            let mut total: i64 = 0;
            for a in args {
                total = total.saturating_add(recur(a)?);
            }
            Ok(total)
        }
        other => Err(expr_shape_name(other).to_string()),
    }
}

/// A short, source-shaped name for an expression, for refusal #5's message.
fn expr_shape_name(e: &Expr) -> &'static str {
    match e {
        Expr::Number(_) => "number literal",
        Expr::Bytes(_) => "byte-string literal",
        Expr::Binary(_, _, _) => "arithmetic / comparison",
        Expr::Call(_, _) => "rule call",
        Expr::If(_, _, _) => "if / then / else",
        Expr::JsonEscape(_) => "json_escape(...)",
        Expr::ParseInt(_) => "parse_int(...)",
        Expr::NowUnix => "now_unix()",
        Expr::Length(_) => "length(...)",
        Expr::StartsWith(_, _) => "starts_with(...)",
        Expr::EndsWith(_, _) => "ends_with(...)",
        Expr::Contains(_, _) => "contains(...)",
        Expr::ByteAt(_, _) => "byte_at(...)",
        Expr::Record(_, _) => "record construction",
        _ => "unsupported expression",
    }
}

/// Slice `text-state-1` refusals #8 and #9: `state.<f>` on a TEXT state field
/// is wired into three read positions only — a `concat` argument, `length`'s
/// operand, and an `HttpResponse.body` value directly. Every other text
/// primitive dispatches through its own field arm gated on
/// `base == input_name`, and widening seven of them at once is a different
/// slice. Refuse each with a breadcrumb naming the emitter that would have to
/// grow the arm.
///
/// The alternative — leaving them to fall through to a generic native
/// "shape not supported" error — is what this project calls an unattributable
/// refusal: the message would name neither the offender nor the slice.
fn check_state_text_read_positions(
    s: &Service,
    handler: &Rule,
    errors: &mut Vec<VerifyError>,
) {
    let text_fields: HashSet<&str> = s
        .state_fields
        .iter()
        .filter(|sf| sf.ty == Type::Text)
        .map(|sf| sf.name.as_str())
        .collect();
    if text_fields.is_empty() {
        return;
    }
    let mut hits: Vec<(String, &'static str, &'static str)> = Vec::new();
    let mut walk_all = |e: &Expr, hits: &mut Vec<(String, &'static str, &'static str)>| {
        walk_state_text_read_positions(e, &text_fields, hits);
    };
    walk_all(&handler.logic.value, &mut hits);
    for (_, e) in &handler.logic.bindings {
        walk_all(e, &mut hits);
    }
    for aset in &s.after_sets {
        walk_all(&aset.value, &mut hits);
    }
    for (field, primitive, site) in hits {
        errors.push(VerifyError {
            context: format!("service '{}' / state / {}", s.name, field),
            message: format!(
                "{}: 'state.{}' is a text state field; its (ptr, len) load is not wired into {} in slice \
                 text-state-1. Bind it to a handler let first, or wait for slice text-state-2.",
                primitive, field, site
            ),
        });
    }
}

/// The walk behind `check_state_text_read_positions`. Reports one hit per
/// (primitive, state field) occurrence, in source order — a `Vec`, never a
/// set, so the diagnostic order is deterministic.
fn walk_state_text_read_positions(
    e: &Expr,
    text_fields: &HashSet<&str>,
    hits: &mut Vec<(String, &'static str, &'static str)>,
) {
    // Is this expression `state.<a text state field>`?
    let as_state_text = |x: &Expr| -> Option<String> {
        if let Expr::Field(base, fname) = x {
            if matches!(base.as_ref(), Expr::Ident(b) if b == "state")
                && text_fields.contains(fname.as_str())
            {
                return Some(fname.clone());
            }
        }
        None
    };
    let mut flag = |x: &Expr,
                    prim: &'static str,
                    site: &'static str,
                    hits: &mut Vec<(String, &'static str, &'static str)>| {
        if let Some(f) = as_state_text(x) {
            hits.push((f, prim, site));
        }
    };
    match e {
        Expr::StartsWith(a, b) => {
            flag(a, "starts_with", "emit_starts_with_load_text (native.rs)", hits);
            flag(b, "starts_with", "emit_starts_with_load_text (native.rs)", hits);
        }
        Expr::EndsWith(a, b) => {
            flag(a, "ends_with", "emit_starts_with_load_text (native.rs)", hits);
            flag(b, "ends_with", "emit_starts_with_load_text (native.rs)", hits);
        }
        Expr::Contains(a, b) => {
            flag(a, "contains", "emit_starts_with_load_text (native.rs)", hits);
            flag(b, "contains", "emit_starts_with_load_text (native.rs)", hits);
        }
        Expr::Substring(t, _, _) => {
            flag(t, "substring", "emit_text_produce_ptrlen's Substring arm (native.rs)", hits);
        }
        Expr::JsonEscape(inner) => {
            flag(inner, "json_escape", "emit_concat_to_buffer_impl's JsonEscapedText arm (native.rs)", hits);
        }
        Expr::ParseInt(inner) => {
            flag(inner, "parse_int", "emit_parse_int (native.rs)", hits);
        }
        Expr::Binary(op, a, b) if matches!(op, BinOp::Eq | BinOp::NotEq) => {
            flag(a, "text equality", "emit_eval_expr's text-comparison arms (native.rs)", hits);
            flag(b, "text equality", "emit_eval_expr's text-comparison arms (native.rs)", hits);
        }
        _ => {}
    }
    // Recurse into every child regardless of the arm above, so a refused
    // position nested inside an accepted one is still found.
    walk_expr_children(e, &mut |child| walk_state_text_read_positions(child, text_fields, hits));
}

/// Apply `f` to every direct sub-expression of `e`.
///
/// Enumerated arm-for-arm rather than caught by a catch-all, so a new `Expr`
/// variant is a COMPILE error here instead of a silently-unvisited subtree.
/// (That is the same discipline `count_badcall_ast` in gen0 had to be taught
/// the hard way — twelve node families stubbed to a flat `0`.)
pub(crate) fn walk_expr_children(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    match e {
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) | Expr::Read(_)
        | Expr::Random(_) | Expr::NowUnix => {}
        Expr::Field(b, _) => f(b),
        Expr::Not(a)
        | Expr::Neg(a)
        | Expr::Ok(a)
        | Expr::Err(a)
        | Expr::JsonEscape(a)
        | Expr::ParseInt(a)
        | Expr::Length(a)
        | Expr::Abs(a)
        | Expr::Le32(a)
        | Expr::Le64(a)
        | Expr::ArenaScope(a)
        | Expr::AbortIf(a)
        | Expr::BitNot(a)
        | Expr::Fetch(_, a) => f(a),
        Expr::Binary(_, a, b)
        | Expr::StartsWith(a, b)
        | Expr::EndsWith(a, b)
        | Expr::Contains(a, b)
        | Expr::Min(a, b)
        | Expr::Max(a, b)
        | Expr::BitAnd(a, b)
        | Expr::BitOr(a, b)
        | Expr::BitXor(a, b)
        | Expr::Shl(a, b)
        | Expr::Shr(a, b)
        | Expr::ByteAt(a, b)
        | Expr::Quantifier(_, a, _, b)
        | Expr::Map(a, _, b)
        | Expr::Filter(a, _, b) => {
            f(a);
            f(b);
        }
        Expr::If(a, b, c) | Expr::Substring(a, b, c) | Expr::Fold(a, b, _, _, c) => {
            f(a);
            f(b);
            f(c);
        }
        Expr::MatchResult(scrut, _, ok_body, _, err_body) => {
            f(scrut);
            f(ok_body);
            f(err_body);
        }
        Expr::FoldBytes(a, b, _, _, _, c) => {
            f(a);
            f(b);
            f(c);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                f(a);
            }
        }
        Expr::Record(_, fields) | Expr::VariantConstruct(_, _, fields) => {
            for (_, v) in fields {
                f(v);
            }
        }
        Expr::MatchVariant(scrut, arms) => {
            f(scrut);
            for arm in arms {
                f(&arm.body);
            }
        }
    }
}

/// Phase 8 slices 8a/8b/8c — type-check a log content expression against
/// the closed log-scope grammar.
///
/// Accepted shapes (recursively for `concat`):
///   - `text` / `number` literal
///   - `Field(Ident("req"), name)` where `name` is a declared HttpRequest
///     field (slice 8a: `method`, `path`)
///   - `Field(Ident("req"), "timestamp")` — synthetic Unix-seconds slot
///     populated once per accept loop (slice 8c)
///   - `Field(Ident("resp"), "status")` — handler-populated status (slice 8b)
///   - `Field(Ident("resp"), "body")`   — handler-populated body  (slice 8b)
///   - `concat(arg, ...)` where every arg is itself accepted and produces
///     a scalar (text, number, or bool — the existing concat fill rule)
///
/// Anything else (if/else, rule calls, record construction, arbitrary let
/// bindings, unknown fields) is rejected with a precise message.
fn verify_log_content(
    expr: &Expr,
    req_concept: Option<&Concept>,
    resp_concept: Option<&Concept>,
    expected: &Type,
) -> Result<(), String> {
    let ty = log_content_type(expr, req_concept, resp_concept)?;
    if &ty != expected {
        return Err(format!(
            "expression has type '{}' but log content must be '{}'",
            type_display(&ty),
            type_display(expected),
        ));
    }
    Ok(())
}

/// Walk a log content expression, returning its inferred type if it
/// matches the closed grammar, or an error otherwise. Used by
/// `verify_log_content` and recursively to validate `concat` arguments.
fn log_content_type(
    expr: &Expr,
    req_concept: Option<&Concept>,
    resp_concept: Option<&Concept>,
) -> Result<Type, String> {
    match expr {
        Expr::Text(_) => Ok(Type::Text),
        Expr::Number(_) => Ok(Type::Number),
        Expr::Field(base, field_name) => {
            let base_name = match base.as_ref() {
                Expr::Ident(n) => n,
                _ => {
                    return Err(format!(
                        "field access base must be `req` or `resp`, got a non-identifier expression"
                    ))
                }
            };
            match base_name.as_str() {
                "req" => {
                    if field_name == "timestamp" {
                        return Ok(Type::Number);
                    }
                    let c = req_concept.ok_or_else(|| {
                        "log content references `req` but the handler input is not a named concept".to_string()
                    })?;
                    let f = c.fields.iter().find(|f| &f.name == field_name).ok_or_else(|| {
                        format!(
                            "`req.{}` is not a declared HttpRequest field; allowed: {}, plus the synthetic `req.timestamp`",
                            field_name,
                            c.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    })?;
                    Ok(f.ty.clone())
                }
                "resp" => {
                    let c = resp_concept.ok_or_else(|| {
                        "log content references `resp` but the handler output is not a named concept".to_string()
                    })?;
                    let f = c.fields.iter().find(|f| &f.name == field_name).ok_or_else(|| {
                        format!(
                            "`resp.{}` is not a declared HttpResponse field; allowed: {}",
                            field_name,
                            c.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", ")
                        )
                    })?;
                    Ok(f.ty.clone())
                }
                other => Err(format!(
                    "log content can read fields of `req` or `resp` only; got `{}`",
                    other
                )),
            }
        }
        Expr::Concat(args) => {
            for (i, arg) in args.iter().enumerate() {
                let arg_ty = log_content_type(arg, req_concept, resp_concept)
                    .map_err(|m| format!("concat arg #{}: {}", i + 1, m))?;
                match arg_ty {
                    Type::Number | Type::Bool | Type::Text => {}
                    other => {
                        return Err(format!(
                            "concat arg #{} has type '{}'; only scalar text/number/bool allowed",
                            i + 1,
                            type_display(&other),
                        ))
                    }
                }
            }
            Ok(Type::Text)
        }
        // Phase 12 (json_escape): allowed inside a log content as long as
        // the inner expression is itself allowed by this grammar AND
        // produces text. The transform's output is text by construction.
        Expr::JsonEscape(inner) => {
            let inner_ty = log_content_type(inner, req_concept, resp_concept)
                .map_err(|m| format!("json_escape arg: {}", m))?;
            match inner_ty {
                Type::Text => Ok(Type::Text),
                other => Err(format!(
                    "json_escape arg has type '{}'; json_escape only accepts text",
                    type_display(&other),
                )),
            }
        }
        // Phase 12 (parse_int): inner must be text; output is number. Same
        // shape as JsonEscape, but the produced type is Number — a literal
        // `parse_int(...)` inside a log content is an unusual but legal way
        // to lift a textual count into a numeric position.
        Expr::ParseInt(inner) => {
            let inner_ty = log_content_type(inner, req_concept, resp_concept)
                .map_err(|m| format!("parse_int arg: {}", m))?;
            match inner_ty {
                Type::Text => Ok(Type::Number),
                other => Err(format!(
                    "parse_int arg has type '{}'; parse_int only accepts text",
                    type_display(&other),
                )),
            }
        }
        // `length(<text_expr>)` — inner must be text; output is number.
        // Same shape as ParseInt.
        Expr::Length(inner) => {
            let inner_ty = log_content_type(inner, req_concept, resp_concept)
                .map_err(|m| format!("length arg: {}", m))?;
            match inner_ty {
                Type::Text => Ok(Type::Number),
                other => Err(format!(
                    "length arg has type '{}'; length only accepts text",
                    type_display(&other),
                )),
            }
        }
        other => Err(format!(
            "expression `{}` is not allowed in a log content; allowed: text/number literals, `req.field`, `resp.field`, `concat(...)`, `json_escape(...)`, `parse_int(...)`, `length(...)`",
            describe_expr_kind(other)
        )),
    }
}

/// Short label for an Expr variant — used in user-facing log errors so the
/// message says "if/else" instead of dumping the whole AST.
fn describe_expr_kind(e: &Expr) -> &'static str {
    match e {
        Expr::Number(_) => "number",
        Expr::Text(_) => "text",
        Expr::Bytes(_) => "bytes",
        Expr::Ident(_) => "identifier",
        Expr::Field(_, _) => "field access",
        Expr::Binary(_, _, _) => "binary op",
        Expr::Neg(_) => "negation",
        Expr::Not(_) => "boolean not",
        Expr::If(_, _, _) => "if/else",
        Expr::Quantifier(_, _, _, _) => "quantifier",
        Expr::Map(_, _, _) => "map",
        Expr::Filter(_, _, _) => "filter",
        Expr::Fold(_, _, _, _, _) => "fold",
        Expr::Call(_, _) => "rule call",
        Expr::Ok(_) => "Ok(...)",
        Expr::Err(_) => "Err(...)",
        Expr::MatchResult(_, _, _, _, _) => "match_result",
        Expr::Record(_, _) => "record construction",
        Expr::Concat(_) => "concat",
        Expr::Read(_) => "read",
        Expr::Random(_) => "random",
        Expr::Fetch(_, _) => "fetch",
        Expr::JsonEscape(_) => "json_escape",
        Expr::ParseInt(_) => "parse_int",
        Expr::NowUnix => "now_unix",
        Expr::StartsWith(_, _) => "starts_with",
        Expr::Contains(_, _) => "contains",
        Expr::EndsWith(_, _) => "ends_with",
        Expr::Length(_) => "length",
        Expr::Abs(_) => "abs", Expr::BitAnd(_,_) => "band", Expr::BitOr(_,_) => "bor", Expr::BitXor(_,_) => "bxor", Expr::BitNot(_) => "bnot", Expr::Shl(_,_) => "shl", Expr::Shr(_,_) => "shr",
        Expr::Le32(_) => "le32", Expr::Le64(_) => "le64",
        Expr::Min(_, _) => "min",
        Expr::Max(_, _) => "max",
        Expr::ArenaScope(_) => "arena_scope",
        Expr::AbortIf(_) => "abort_if",
        Expr::Substring(_, _, _) => "substring",
        Expr::ByteAt(_, _) => "byte_at",
        Expr::FoldBytes(_, _, _, _, _, _) => "fold_bytes",
        Expr::VariantConstruct(_, _, _) => "variant construction",
        Expr::MatchVariant(_, _) => "pattern match",
    }
}

/// Helper for verify_service: enforce that the handler's input or output
/// (for an Http10 service) is exactly the expected compiler-provided
/// concept (`HttpRequest` or `HttpResponse`). Any other type — including a
/// user-declared concept with a different shape that happens to have the
/// same fields — is rejected.
fn check_http10_binding(
    ty: &Type,
    rule_name: &str,
    position: &str,
    expected_concept: &str,
    s: &Service,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = || {
        format!(
            "service '{}' / handler '{}' / {}",
            s.name, rule_name, position
        )
    };
    match ty {
        Type::Named(n) if n == expected_concept => {
            // Correct — the built-in was already injected into concepts.
        }
        _ => {
            errors.push(VerifyError {
                context: ctx(),
                message: format!(
                    "http_1_0 handler {} must be the built-in concept '{}'; got {}",
                    position,
                    expected_concept,
                    type_display(ty)
                ),
            });
        }
    }
}

/// Helper for verify_service: enforce that a handler's input or output
/// (for a RawTcp service) is a Named concept with exactly one `bytes[..N]`
/// field where N equals the service's declared max_request. Any other shape
/// is rejected with a specific error naming the offending position.
fn check_raw_tcp_binding(
    ty: &Type,
    rule_name: &str,
    position: &str,
    expected_bound: i64,
    concepts: &HashMap<String, &Concept>,
    s: &Service,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = || {
        format!(
            "service '{}' / handler '{}' / {}",
            s.name, rule_name, position
        )
    };
    let concept_name = match ty {
        Type::Named(n) => n,
        _ => {
            errors.push(VerifyError {
                context: ctx(),
                message: format!(
                    "raw_tcp handler {} must be a Named concept with one bytes field; got {}",
                    position,
                    type_display(ty)
                ),
            });
            return;
        }
    };
    let concept = match concepts.get(concept_name.as_str()) {
        Some(c) => *c,
        None => {
            errors.push(VerifyError {
                context: ctx(),
                message: format!("unknown concept '{}'", concept_name),
            });
            return;
        }
    };
    if concept.fields.len() != 1 {
        errors.push(VerifyError {
            context: ctx(),
            message: format!(
                "raw_tcp handler {} concept '{}' must have exactly one field (has {})",
                position,
                concept_name,
                concept.fields.len()
            ),
        });
        return;
    }
    let field = &concept.fields[0];
    if !matches!(field.ty, Type::Bytes) {
        errors.push(VerifyError {
            context: ctx(),
            message: format!(
                "raw_tcp handler {} concept '{}' field '{}' must be bytes; got {}",
                position,
                concept_name,
                field.name,
                type_display(&field.ty)
            ),
        });
        return;
    }
    match field.range {
        Some((0, max)) if max == expected_bound => {
            // matches exactly — good
        }
        Some((_, max)) => {
            errors.push(VerifyError {
                context: ctx(),
                message: format!(
                    "raw_tcp handler {} concept '{}' field '{}' bound is [..{}]; must equal service max_request {}",
                    position, concept_name, field.name, max, expected_bound
                ),
            });
        }
        None => {
            errors.push(VerifyError {
                context: ctx(),
                message: format!(
                    "raw_tcp handler {} concept '{}' field '{}' must declare an explicit bytes bound [..{}]",
                    position, concept_name, field.name, expected_bound
                ),
            });
        }
    }
}

fn verify_concept(c: &Concept, base_dir: &StdPath, errors: &mut Vec<VerifyError>) {
    if let Err(msg) = verify_source_ref(&c.source, base_dir) {
        errors.push(VerifyError {
            context: format!("concept '{}' / @source", c.name),
            message: msg,
        });
    }
}

/// Off-stack mmap arena (2026-06-22): the arena emitter stores node
/// indices as 64-bit values (the 16-bit-index reasoning never constrained
/// storage — confirmed in the B.4 recon), so the `max_nodes` ceiling can
/// rise well past 65535 now that the arena is mmap-backed (off-stack). Raised
/// 4M -> 8M for the stdin-channel self-hosting milestone: the front end parsing
/// its full ~850 KB self-source peaks around ~5.3M nodes (VExpr's working
/// max_nodes is 6M). At 8_000_000 a worst-case wide-variant arena is a few
/// hundred MB, which fails gracefully via the MAP_FAILED abort if the host
/// can't back it.
const PHASE_B1_MAX_NODES: u32 = 8_000_000;

/// `max_depth` stays at the 16-bit ceiling. Raising it would be FALSE
/// EXPLICITATION: there is no runtime recursion-depth check yet (the 8 MB
/// stack is the real wall — see docs/self-hosting-capacity-design.md), so a
/// declared `max_depth` above what the stack can actually hold is an
/// unbacked promise. Tie this to a real runtime check before raising it.
const PHASE_B1_MAX_DEPTH: u32 = 65535;

/// Phase B slice 1: verify a `concept_group` block. Checks the @source
/// ref, the `max_depth` / `max_nodes` bounds, and the inner concepts'
/// well-formedness:
///   - every inner concept must be a sum-type (non-empty `variants`);
///     record-shape concepts (with `fields`) belong at top level — a
///     group exists to carry mutually-recursive sum types
///   - every `Type::Named(N)` inside a variant payload must resolve to
///     either a primitive type, another concept in the SAME group, or
///     a top-level concept; cross-group references are refused in B.1
///   - cycles within the group are EXPECTED — they are the whole point
///     of a `concept_group`, not refused
fn verify_concept_group(
    g: &ConceptGroup,
    group_concept_owner: &HashMap<String, String>,
    base_dir: &StdPath,
    errors: &mut Vec<VerifyError>,
) {
    if let Err(msg) = verify_source_ref(&g.source, base_dir) {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / @source", g.name),
            message: msg,
        });
    }

    if g.max_depth == 0 {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_depth", g.name),
            message: "max_depth must be greater than zero — a recursive tree must allow at least one level".into(),
        });
    }
    if g.max_depth > PHASE_B1_MAX_DEPTH {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_depth", g.name),
            message: format!(
                "max_depth {} exceeds the ceiling of {} (no runtime recursion-depth check exists yet — raising this would be an unbacked promise; see docs/self-hosting-capacity-design.md)",
                g.max_depth, PHASE_B1_MAX_DEPTH
            ),
        });
    }
    if g.max_nodes == 0 {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_nodes", g.name),
            message: "max_nodes must be greater than zero — a recursive tree must allow at least one node".into(),
        });
    }
    if g.max_nodes > PHASE_B1_MAX_NODES {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_nodes", g.name),
            message: format!(
                "max_nodes {} exceeds the ceiling of {} (the mmap arena is off-stack but still bounded; an absurd count would mmap-fail at runtime — see docs/arena-allocation-design.md)",
                g.max_nodes, PHASE_B1_MAX_NODES
            ),
        });
    }
    // Off-stack mmap arena (2026-06-22): `arena_size = max_nodes *
    // entry_size` is held as an i32 in the native emitter (the disp32
    // node_count offset and the `sub rsp` / mmap-size math all use i32). A
    // wide-variant group at the raised max_nodes ceiling could overflow
    // i32 and wrap to a nonsensical (possibly negative) size. Refuse it
    // here so the emitter never sees a wrapped arena_size. entry_size is
    // computed with the same per-field byte widths the native arena layout
    // uses (Text = 16 B, everything else 8 B, +1 tag byte, padded to 8).
    {
        let group_names: std::collections::HashSet<&str> =
            g.concepts.iter().map(|c| c.name.as_str()).collect();
        let field_width = |ty: &Type| -> i64 {
            match ty {
                Type::Text => 16,
                _ => 8,
            }
        };
        let max_payload: i64 = g.concepts.iter()
            .flat_map(|c| c.variants.iter())
            .map(|v| v.fields.iter().map(|f| field_width(&f.ty)).sum::<i64>())
            .max()
            .unwrap_or(0);
        let _ = &group_names; // (group_names kept for clarity of intent)
        let raw_entry = 1 + max_payload;
        let entry_size = (raw_entry + 7) & !7; // round up to 8
        let arena_size = (g.max_nodes as i64) * entry_size;
        if arena_size > i32::MAX as i64 {
            errors.push(VerifyError {
                context: format!("concept_group '{}' / max_nodes", g.name),
                message: format!(
                    "arena size = max_nodes ({}) × entry_size ({}) = {} bytes exceeds i32::MAX; lower max_nodes (the native arena size is an i32)",
                    g.max_nodes, entry_size, arena_size
                ),
            });
        }
    }

    // Build the set of concept names this group owns. Used to admit
    // intra-group references in variant payloads (the recursive ones).
    let in_group: HashSet<&str> = g.concepts.iter().map(|c| c.name.as_str()).collect();

    for c in &g.concepts {
        // Walk @source on inner concepts too — same audit trail.
        if let Err(msg) = verify_source_ref(&c.source, base_dir) {
            errors.push(VerifyError {
                context: format!(
                    "concept_group '{}' / concept '{}' / @source",
                    g.name, c.name
                ),
                message: msg,
            });
        }

        // Slice 1: every concept in a group must be a sum type. A
        // record-shape concept lives at the top level — a group exists
        // to carry sum types whose variants compose recursively. The
        // parser already forbids `concept Foo` with neither `fields:`
        // nor `variants:` (concept_must_have_one_shape), so a non-empty
        // `fields` here is the only failure mode.
        if !c.fields.is_empty() {
            errors.push(VerifyError {
                context: format!(
                    "concept_group '{}' / concept '{}'",
                    g.name, c.name
                ),
                message: format!(
                    "concept '{}' in concept_group '{}' must be a sum type (use `variants:`, not `fields:`); record concepts belong at top level",
                    c.name, g.name
                ),
            });
            continue;
        }
        if c.variants.is_empty() {
            // Defensive — parser forbids this today, but guard so the
            // walk below doesn't silently pass an empty concept.
            errors.push(VerifyError {
                context: format!(
                    "concept_group '{}' / concept '{}'",
                    g.name, c.name
                ),
                message: format!(
                    "concept '{}' in concept_group '{}' must declare at least one variant",
                    c.name, g.name
                ),
            });
            continue;
        }

        // For each variant, validate type references in payload fields.
        // Intra-group: OK (recursive). Cross-group: refused in B.1.
        // Top-level concept: OK (the group consumes a sibling, no cycle
        // through the group walls). Primitives: OK.
        for variant in &c.variants {
            for field in &variant.fields {
                check_group_payload_type(
                    g,
                    c,
                    variant,
                    field,
                    &field.ty,
                    &in_group,
                    group_concept_owner,
                    errors,
                );
            }
        }
    }

    // Phase B slice 4b: variant names must be unique across all concepts
    // in the group. The native emitter uses a flat variant-name → tag
    // map; collisions would make MatchVariant dispatch ambiguous.
    let mut seen_variants: HashMap<&str, &str> = HashMap::new();
    for c in &g.concepts {
        for v in &c.variants {
            if let Some(prev_concept) = seen_variants.get(v.name.as_str()) {
                errors.push(VerifyError {
                    context: format!(
                        "concept_group '{}' / concept '{}' / variant '{}'",
                        g.name, c.name, v.name
                    ),
                    message: format!(
                        "variant name '{}' collides with a variant in concept '{}' — \
                         variant names must be unique across all concepts in a concept_group",
                        v.name, prev_concept
                    ),
                });
            } else {
                seen_variants.insert(v.name.as_str(), c.name.as_str());
            }
        }
    }
}

/// Phase B slice 1: helper for `verify_concept_group`. Walks a variant
/// payload field's type and refuses cross-group references with a clear
/// breadcrumb. Other shapes (primitives, `Type::Result(...)`,
/// `Type::Collection(...)`) pass through — they'll be re-validated by
/// the existing rule-level type checker when a rule eventually consumes
/// the value, which is slice B.3+.
fn check_group_payload_type(
    g: &ConceptGroup,
    c: &Concept,
    variant: &Variant,
    field: &Field,
    ty: &Type,
    in_group: &HashSet<&str>,
    group_concept_owner: &HashMap<String, String>,
    errors: &mut Vec<VerifyError>,
) {
    match ty {
        Type::Named(n) => {
            // Intra-group reference: the recursive case. Always OK.
            if in_group.contains(n.as_str()) {
                return;
            }
            // Cross-group reference: refused in B.1. Cross-group
            // recursion needs a verifier strategy for the SCC bound,
            // which is a later slice.
            if let Some(other_group) = group_concept_owner.get(n) {
                if other_group != &g.name {
                    errors.push(VerifyError {
                        context: format!(
                            "concept_group '{}' / concept '{}' / variant '{}' / field '{}'",
                            g.name, c.name, variant.name, field.name
                        ),
                        message: format!(
                            "field type '{}' refers to a concept in a DIFFERENT concept_group ('{}') — cross-group references are not supported until a later slice",
                            n, other_group
                        ),
                    });
                    return;
                }
            }
            // Otherwise it's a top-level concept (or undeclared);
            // we leave the existence check to the standard concept-
            // resolution pass that fires when a rule consumes the
            // value. B.1 is parser + verifier only and rules cannot
            // reference group concepts yet, so the dangling-reference
            // case shows up the moment B.3 wires the interpreter.
        }
        Type::Result(t, e) => {
            check_group_payload_type(g, c, variant, field, t, in_group, group_concept_owner, errors);
            check_group_payload_type(g, c, variant, field, e, in_group, group_concept_owner, errors);
        }
        // Collection(inner) where inner is the name of a type. For
        // intra-group recursion via `collection(Foo)` we'd need
        // payload-level recursion through a list — the design doc
        // ships it as `list<T> [..N]` and we deliberately defer to
        // slice B.1b. Today we don't refuse it (a `collection(T)`
        // referring to a group concept would surface again when a
        // rule consumes the value), but flag a clear breadcrumb if
        // it does point at the same group so the deferral is loud.
        Type::Collection(inner) => {
            if in_group.contains(inner.as_str()) {
                errors.push(VerifyError {
                    context: format!(
                        "concept_group '{}' / concept '{}' / variant '{}' / field '{}'",
                        g.name, c.name, variant.name, field.name
                    ),
                    message: format!(
                        "collection({}) of a group-internal concept is deferred to slice B.1b; declare a non-collection field for now",
                        inner
                    ),
                });
            }
        }
        // Primitives — nothing to validate here.
        Type::Number | Type::Bool | Type::Text | Type::Bytes => {}
    }
}

/// Phase B slice 1: refuse a rule whose `input:` or `output:` (or
/// transitively, `context:`) references a concept declared inside a
/// `concept_group`. The interpreter / native / wasm code paths for
/// group-typed values do not exist yet — interpreter lands in B.3,
/// native in B.4+. Refusing here keeps the slice honest: a program
/// with a `concept_group` can compile a non-group rule fine, but the
/// moment a rule tries to consume a group value the verifier names
/// the slice that will lift the restriction.
fn refuse_rule_using_group_type(
    rule: &Rule,
    group_concept_owner: &HashMap<String, String>,
    errors: &mut Vec<VerifyError>,
) {
    let mut check_ty = |label: &str, ty: &Type| {
        let referenced = group_concept_name(ty);
        for name in referenced {
            if let Some(group_name) = group_concept_owner.get(name) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / {}", rule.name, label),
                    message: format!(
                        "rule '{}' uses a concept_group type ('{}' in group '{}') — Phase B slice 3+ wires recursive types through rules; use --run only when that ships",
                        rule.name, name, group_name
                    ),
                });
            }
        }
    };
    check_ty("input", &rule.input_ty);
    check_ty("output", &rule.output_ty);
    if let Some(ctx_ty) = &rule.context_ty {
        check_ty("context", ctx_ty);
    }
}

/// Phase B slice 1: collect every `Type::Named` name referenced by a
/// type. Returns a Vec of borrowed names from the type tree (no clones
/// during the walk). Used by `refuse_rule_using_group_type` — we want
/// every concept name a type mentions, not just the top-level one, so
/// `Result(Stmt, text)` is flagged the same way as `Stmt`.
fn group_concept_name(ty: &Type) -> Vec<&str> {
    let mut out = Vec::new();
    fn walk<'a>(ty: &'a Type, out: &mut Vec<&'a str>) {
        match ty {
            Type::Named(n) => out.push(n.as_str()),
            Type::Collection(n) => out.push(n.as_str()),
            Type::Result(t, e) => {
                walk(t, out);
                walk(e, out);
            }
            Type::Number | Type::Bool | Type::Text | Type::Bytes => {}
        }
    }
    walk(ty, &mut out);
    out
}

fn verify_rule(
    rule: &Rule,
    concepts: &HashMap<String, &Concept>,
    all_rules: &[&Rule],
    all_resources: &HashSet<String>,
    all_connections: &HashSet<String>,
    all_entropies: &HashSet<String>,
    group_concept_owner: &HashMap<String, String>,
    base_dir: &StdPath,
    errors: &mut Vec<VerifyError>,
) {
    if let Err(msg) = verify_source_ref(&rule.source, base_dir) {
        errors.push(VerifyError {
            context: format!("rule '{}' / @source", rule.name),
            message: msg,
        });
    }

    if rule.logic.target != rule.output_name {
        errors.push(VerifyError {
            context: format!("rule '{}' / logic", rule.name),
            message: format!(
                "logic assigns to '{}' but rule's output is '{}'",
                rule.logic.target, rule.output_name
            ),
        });
    }

    let input_concept: Option<&Concept> = match &rule.input_ty {
        Type::Named(n) => match concepts.get(n) {
            Some(c) => Some(*c),
            None => {
                errors.push(VerifyError {
                    context: format!("rule '{}' / input", rule.name),
                    message: format!("unknown type '{}'", n),
                });
                None
            }
        },
        _ => None,
    };

    // The `context:` binding is the rule's second concept in scope. Resolved
    // the same way the input is; a name that does not resolve to a record
    // concept simply yields None and everything downstream stays silent.
    let context_concept: Option<&Concept> = rule
        .context_ty
        .as_ref()
        .and_then(|t| record_concept_of(t, concepts));

    // Every OTHER binding whose type is a known record concept — the context
    // binding plus each typeable top-level `let`. Consulted both by the
    // field-existence loop below and, through `check_expr_against`, by
    // `infer_expr_type`, so the two halves of the check agree by construction.
    let bindings = collect_binding_concepts(rule, all_rules, input_concept, concepts);

    let mut facts = collect_logic_facts(&rule.logic);
    // Transitive resource/connection reads via `match_result` chains.
    // When a rule does `match_result(callee(input), ...)`, the native
    // emitter inlines the callee's body INTO the outer rule's frame,
    // which means the callee's resource and connection reads happen at
    // the outer rule's runtime layer — and the outer rule's prologue
    // needs them in its `reads:` declaration to allocate the right
    // slots. The verifier surfaces this as a legitimate read of the
    // outer rule, not as an "extra" entry.
    augment_facts_with_transitive_match_result_reads(
        rule, all_rules, all_resources, all_connections, all_entropies, &mut facts,
    );

    for path in &facts.reads {
        if let Some(msg) = validate_read_path(
            path,
            rule,
            input_concept,
            context_concept,
            all_resources,
            all_connections,
            all_entropies,
        ) {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: msg,
            });
        }
    }

    // The same field-existence check, for a `.field` whose base is a `let`
    // bound to a record. `local_reads` holds exactly those paths (see
    // `LogicFacts`); a base absent from `bindings` is a binding this pass
    // cannot type, and stays silent. Sorted so the diagnostic order does not
    // depend on hash iteration.
    let mut local: Vec<&Vec<String>> = facts.local_reads.iter().collect();
    local.sort();
    for path in local {
        if path.len() < 2 {
            continue;
        }
        if let Some(c) = bindings.get(path[0].as_str()) {
            if let Some(msg) = concept_field_error(c, &path[1], path) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: msg,
                });
            }
        }
    }

    for call_path in &facts.calls {
        if call_path.len() == 1 {
            let call_name = &call_path[0];
            if !all_rules.iter().any(|r| r.name == *call_name) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / calls", rule.name),
                    message: format!("calls unknown rule '{}'", call_name),
                });
            }
        }
    }

    // Arity of every rule-call site. `facts.calls` only carries callee NAMES
    // (it feeds the purity proof), so the arity check needs its own walk over
    // the logic — see `check_call_arity`.
    check_call_arity(rule, errors);

    check_purity(rule, &facts, errors);
    check_termination(rule, concepts, group_concept_owner, errors);

    if let Some(hints) = &rule.hints {
        check_hints(rule, hints, &facts, concepts, errors);
    }

    if let Some(caller_layer) = rule.layer {
        check_layer_discipline(rule, caller_layer, &facts, all_rules, errors);
    }

    // The binding map the TYPE CHECK gets, with every name that is also a
    // lambda / `match` arm binder somewhere in the logic removed.
    //
    // `infer_expr_type` resolves a bare `Ident` through this map (that is what
    // catches `let p = mk(i)` then `out = p * 1000`), and the arm bodies of
    // `match_result` / `match` ARE visited by the type check while their
    // binders' scope is not tracked. Without this filter a binder shadowing a
    // record-typed `let` would be read as that let and could produce an error
    // about a name that, inside that arm, means something else. Filtering only
    // ever REMOVES inference, so it cannot invent a diagnostic.
    //
    // The field-existence loop above keeps the unfiltered map on purpose: it
    // is driven by `facts.local_reads`, from which `collect_expr_facts` has
    // already dropped every binder-rooted path.
    let mut shadowed = collect_lambda_bound_names(&rule.logic.value);
    for (_, rhs) in &rule.logic.bindings {
        shadowed.extend(collect_lambda_bound_names(rhs));
    }
    let typed_bindings: HashMap<String, &Concept> = bindings
        .iter()
        .filter(|(name, _)| !shadowed.contains(name.as_str()))
        .map(|(name, c)| (name.clone(), *c))
        .collect();
    let bindings = typed_bindings;

    // Every `let` RHS, checked against ITS OWN inferred type.
    //
    // Until this landed, `check_expr_against` ran on `rule.logic.value` and
    // nothing else, so a `let` RHS was never type-checked at all — and that
    // makes every operand check in this pass one `let` away from being
    // bypassed. Measured: `let z = t.s * 2` then `out = z` on a text field
    // verified clean and its native binary printed a randomized stack
    // address, exactly as the direct `out = t.s * 2` form did, because
    // `infer_expr_type(Ident("z"))` is None so the body check stays silent.
    //
    // A `let` has no DECLARED type to check against, so the expected type is
    // the RHS's own inferred type: the outer comparison is then true by
    // construction and can never fire, and the whole effect of the call is to
    // RECURSE into the sub-expressions. Un-inferable RHSes (`map`/`fold`/
    // `Ok(..)`/a lambda-bound var) yield None and are skipped, which is the
    // same conservative posture the body check already takes.
    for (_, rhs) in &rule.logic.bindings {
        if let Some(t) = infer_expr_type(rhs, rule, all_rules, input_concept, &bindings) {
            check_expr_against(
                rhs,
                &t,
                rule,
                all_rules,
                input_concept,
                concepts,
                &bindings,
                errors,
            );
        }
    }

    // Type-shape check: the logic expression must be compatible with the
    // declared output_ty. We do bidirectional checking from the top down —
    // Ok/Err can only appear where a Result is expected, branches of if/else
    // and match_result inherit the expected type, and inferable leaf types
    // (literals, arithmetic, comparisons, rule calls, input fields) are
    // compared exactly. When inference is not possible (let-bound vars,
    // lambda-bound vars, Map/Filter/Fold bodies), we stay silent rather than
    // false-positive — the evolution rule says we never fabricate proofs we
    // cannot verify.
    check_expr_against(
        &rule.logic.value,
        &rule.output_ty,
        rule,
        all_rules,
        input_concept,
        concepts,
        &bindings,
        errors,
    );
}

/// Bidirectional type check. `expected` is the type the surrounding context
/// expects this expression to produce. Errors are emitted for:
///   - Ok/Err constructors where the expected type is not a Result,
///   - Ok(x) where x's inferable type != T (in Result(T, _)),
///   - Err(e) where e's inferable type != E (in Result(_, E)),
///   - Map/Filter outside a Collection context,
///   - Record(C) where C is unknown, or field set differs from C's declaration,
///     or a field's inferable type differs from C's declared field type,
///   - Any other inferable expression whose type != expected.
/// Slice `rawtcp-inspect-0`: is `expr` a `Field` on the rule's INPUT whose
/// DECLARED type is `Type::Bytes`? Returns the operand's display form
/// (`req.data`) so a refusal can name it. This is the ONE shape the
/// byte-addressed gate widens to — never `Type::Bytes` at large.
fn bytes_input_field(expr: &Expr, rule: &Rule, input_concept: Option<&Concept>) -> Option<String> {
    if let Expr::Field(base, fname) = expr {
        if let Expr::Ident(b) = base.as_ref() {
            if b == &rule.input_name {
                let is_bytes = input_concept
                    .and_then(|c| c.fields.iter().find(|f| &f.name == fname))
                    .map(|f| matches!(f.ty, Type::Bytes))
                    .unwrap_or(false);
                if is_bytes {
                    return Some(format!("{}.{}", b, fname));
                }
            }
        }
    }
    None
}

/// Slice `rawtcp-inspect-0`, refusal #2 of docs/multistep-connection-design.md
/// §4.4: a TEXT primitive (`starts_with` / `ends_with` / `contains` /
/// `json_escape` / `parse_int`, and text `==`) handed a bytes-typed operand.
/// The bytes/text isolation is deliberate, and the generic mismatch message
/// (`expression has type 'bytes' but context expects 'text'`) is true but
/// unhelpful — it reads like a missing cast. Push the breadcrumb and return
/// `true` so the caller skips the generic check for that operand; return
/// `false` for a non-bytes operand so the existing path runs unchanged.
fn refuse_bytes_in_text_prim(
    prim: &str,
    operand: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    bindings: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) -> bool {
    match infer_expr_type(operand, rule, all_rules, input_concept, bindings) {
        Some(Type::Bytes) => {
            let shown = bytes_input_field(operand, rule, input_concept)
                .unwrap_or_else(|| describe_expr_kind(operand).to_string());
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "{}: '{}' is bytes; this primitive checks its operand against text and the \
                     bytes/text isolation is deliberate. Convert explicitly with byte_at, or wait \
                     for slice rawtcp-inspect-0b",
                    prim, shown
                ),
            });
            true
        }
        _ => false,
    }
}

/// Slice `rawtcp-inspect-0`, refusal #1 of docs/multistep-connection-design.md
/// §4.4: `substring` over a bytes operand. Deferred to slice
/// `rawtcp-inspect-0b` for a reason of TYPE, not of difficulty (§4.2):
/// `byte_at` / `length` produce Numbers, which every sink already accepts,
/// whereas a bytes slice produces a bytes VALUE whose only sinks are the
/// response field and a bytes `concat` — and the bytes concat is streamed
/// with no sizing pass, so the slice arm is a streaming-ABI question, not an
/// operand-gate one. Returns `true` when it refused (so the caller skips the
/// generic text check on the operand).
fn refuse_bytes_substring(
    operand: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    bindings: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) -> bool {
    match infer_expr_type(operand, rule, all_rules, input_concept, bindings) {
        Some(Type::Bytes) => {
            let shown = bytes_input_field(operand, rule, input_concept)
                .unwrap_or_else(|| describe_expr_kind(operand).to_string());
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "substring: '{}' is bytes; a bytes slice produces a bytes value whose only sinks \
                     are the response field and a bytes concat, and the bytes concat is streamed \
                     with no sizing pass. Slice rawtcp-inspect-0b",
                    shown
                ),
            });
            true
        }
        _ => false,
    }
}

/// Operand check for the two BYTE-ADDRESSED text primitives, `byte_at` and
/// `length`. Both answer a question about a run of BYTES, so both accept a
/// `b"..."` byte-string LITERAL in addition to the usual text shapes.
///
/// Why a literal and nothing else. `b"..."` is the only place in the language
/// where `\xNN` is legal (`src/lexer.rs`), so it is the only expression that
/// can name an arbitrary byte — including `\x00` and anything `>= 0x80`. A
/// `text` literal cannot: `Expr::Text` is a Rust `String`, the lexer builds it
/// with `s.push(ch as char)`, and every backend reads it back with
/// `.as_bytes()`, so a scalar `>= 0x80` would round-trip as TWO UTF-8 bytes.
/// That makes `b"..."` the honest way to declare a constant byte table, and
/// indexing it with `byte_at` the honest way to read one.
///
/// Deliberately NOT widened: `le32`/`le64`, or a bytes `concat`. Those are
/// runtime byte values with no length the emitter can load (a bytes concat
/// is STREAMED with no sizing pass — `emit_streaming_bytes_body`'s contract),
/// so each is refused here BY NAME rather than by the generic text mismatch.
/// Everything else (`starts_with`, `contains`, `ends_with`, `substring`,
/// `parse_int`, `json_escape`, text `concat`) still checks its operand
/// against `Type::Text`, so a `b"..."` there stays a verify error and the
/// `bytes`/`text` isolation documented on `Type::Bytes` is preserved.
///
/// Slice `rawtcp-inspect-0` (docs/multistep-connection-design.md §4.1-3)
/// admits a THIRD shape: a `Field` whose base is the rule's input and whose
/// DECLARED type is `Type::Bytes` — `req.data` in a `raw_tcp` handler. The
/// gate's stated criterion ("a compile-time length") is not weakened; it is
/// met differently: for a bytes input field the length is the `read`
/// syscall's return value, which the service emitter stores in a slot it
/// owns and every reader loads — the same shape as `read(<resource>)`'s
/// `len_slot`. The honest restatement of the criterion is "a length the
/// emitter knows, either as a constant or in a slot it owns". That is why
/// the widening is to the INPUT FIELD and never to `Type::Bytes` at large:
/// a streamed bytes `concat` has no length anywhere. Outside a service the
/// native rule-binary prologue still refuses a bytes input field (`only
/// number/text today`), so the verify-side admission cannot reach a rule
/// binary that would mis-lower it — the emitter is the backstop.
fn check_byte_addressable_operand(
    prim: &str,
    expr: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    all_concepts: &HashMap<String, &Concept>,
    bindings: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    // A `b"..."` literal and a `random(<entropy>)` draw have a COMPILE-TIME
    // length — the literal by construction, the draw by declaration plus the
    // getrandom(2) contract. A bytes INPUT FIELD has a RUNTIME length in a
    // slot the service emitter owns (slice rawtcp-inspect-0). Every other
    // text primitive keeps checking against `Type::Text`, so the bytes/text
    // isolation is intact.
    if matches!(expr, Expr::Bytes(_) | Expr::Random(_)) {
        return;
    }
    if bytes_input_field(expr, rule, input_concept).is_some() {
        return;
    }
    // Refusal #3 of docs/multistep-connection-design.md §4.4: a bytes
    // expression that is NOT one of the three admitted shapes — a bytes
    // `concat`, `le32(...)` / `le64(...)`. Its length is nowhere the emitter
    // can load, so name the shape and the admitted set rather than fall
    // through to `expression has type 'bytes' but context expects 'text'`,
    // which would send the reader looking for a text conversion that does
    // not exist.
    if let Some(Type::Bytes) = infer_expr_type(expr, rule, all_rules, input_concept, bindings) {
        errors.push(VerifyError {
            context: format!("rule '{}' / logic", rule.name),
            message: format!(
                "{}: operand has no length the emitter can load — a bytes {} is streamed with no \
                 sizing pass (emit_streaming_bytes_body, native.rs). Admitted bytes operands: a \
                 b\"...\" literal, random(<name>), and a raw_tcp input field (slice rawtcp-inspect-0)",
                prim,
                describe_expr_kind(expr),
            ),
        });
        return;
    }
    check_expr_against(
        expr,
        &Type::Text,
        rule,
        all_rules,
        input_concept,
        all_concepts,
        bindings,
        errors,
    );
}

/// Operand check for `==` and `!=`, the only binary operators whose operand
/// type is not fixed by the operator.
///
/// The rule is the interpreter's, verbatim: `eval_expr` has exactly two arms
/// per operator — `(Number, Number)` and `(Text, Text)` — and everything else
/// falls to `cannot apply {op} to {l} and {r}`. So the two operands must have
/// the SAME type, and that type must be Number or Text. Bool is deliberately
/// NOT comparable: the interpreter refuses `Bool == Bool`, and inventing a
/// verifier rule the executors do not implement is the inverse of the defect
/// this whole check exists to close.
///
/// Conservative where inference is: when NEITHER side is inferable (a
/// lambda-bound var, a `let` this pass cannot type) nothing is reported, which
/// is the standing posture of the surrounding pass. When exactly one side is
/// inferable and comparable, the other is checked against it — that catches a
/// nested violation such as `t.n == (t.s * 2)` without inventing a type for
/// the un-inferable operand.
fn check_equality_operands(
    l: &Expr,
    r: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    all_concepts: &HashMap<String, &Concept>,
    bindings: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    let comparable = |t: &Type| matches!(t, Type::Number | Type::Text);
    let lt = infer_expr_type(l, rule, all_rules, input_concept, bindings);
    let rt = infer_expr_type(r, rule, all_rules, input_concept, bindings);
    // Slice rawtcp-inspect-0, refusal #2: a bytes operand on either side of
    // `==` / `!=` gets the same named breadcrumb the text primitives give,
    // instead of the generic "compares numbers or text" message. Both sides
    // are probed so `b"\x01" == req.data` names the field, not the literal.
    let mut bytes_refused = false;
    for operand in [l, r] {
        if refuse_bytes_in_text_prim("==", operand, rule, all_rules, input_concept, bindings, errors) {
            bytes_refused = true;
        }
    }
    if bytes_refused {
        return;
    }
    match (lt, rt) {
        (Some(a), Some(b)) => {
            if a != b {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "'==' / '!=' compares two values of the same type, but the left operand has type '{}' and the right operand has type '{}'",
                        type_display(&a),
                        type_display(&b),
                    ),
                });
            } else if !comparable(&a) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "'==' / '!=' compares numbers or text, but both operands have type '{}'",
                        type_display(&a),
                    ),
                });
            } else {
                check_expr_against(l, &a, rule, all_rules, input_concept, all_concepts, bindings, errors);
                check_expr_against(r, &a, rule, all_rules, input_concept, all_concepts, bindings, errors);
            }
        }
        (Some(t), None) | (None, Some(t)) => {
            if comparable(&t) {
                check_expr_against(l, &t, rule, all_rules, input_concept, all_concepts, bindings, errors);
                check_expr_against(r, &t, rule, all_rules, input_concept, all_concepts, bindings, errors);
            } else {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "'==' / '!=' compares numbers or text, but one operand has type '{}'",
                        type_display(&t),
                    ),
                });
            }
        }
        (None, None) => {}
    }
}

fn check_expr_against(
    expr: &Expr,
    expected: &Type,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    all_concepts: &HashMap<String, &Concept>,
    bindings: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    match (expr, expected) {
        (Expr::Ok(inner), Type::Result(t, _)) => {
            check_expr_against(inner, t, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Err(inner), Type::Result(_, e)) => {
            check_expr_against(inner, e, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Ok(_), other) | (Expr::Err(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "Result constructor (Ok/Err) used where the expected type is '{}'; only allowed when output is a Result type",
                    type_display(other),
                ),
            });
        }
        (Expr::If(cond, then_e, else_e), _) => {
            check_expr_against(cond, &Type::Bool, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(then_e, expected, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(else_e, expected, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        // Arithmetic, ordering, equality and logical OPERANDS.
        //
        // This arm exists for the OPERANDS, not the result — the same reason
        // the bitwise arms further down exist, one level up. `infer_expr_type`
        // already reports the RESULT type of every `Expr::Binary` (Number for
        // `+ - * / %`, Bool for the rest), so the catch-all was checking the
        // outer context and never RECURSING into the children. `t.s * 2` on a
        // TEXT field therefore verified clean, and native then did pointer
        // arithmetic on the argv pointer and printed a randomized stack
        // address to stdout; `t.s > 1` compiled to a predicate that is `true`
        // for every input, which silently breaks the bool exit-code contract
        // (`if ./check "$x"` always passes). The interpreter refuses both, so
        // the verifier was certifying a program one executor refuses and the
        // other silently mis-answers.
        //
        // The operand rules are exactly the interpreter's — `eval_expr`'s
        // `Expr::Binary` match is the semantic reference, and it has arms for
        // Number/Number arithmetic and ordering, Bool/Bool `and`/`or`, and
        // `==` / `!=` over Number/Number OR Text/Text. Text EQUALITY is a
        // shipped feature (`examples/clients.verbose`, `allowlist.verbose`,
        // `access_check.verbose`, …) and stays legal; text ORDERING never was.
        (Expr::Binary(op, l, r), expected_outer) => {
            let produced = match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => Type::Number,
                _ => Type::Bool,
            };
            match op {
                // Both operands number-typed; result number (arithmetic) or
                // bool (ordering).
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
                | BinOp::Gt | BinOp::Lt | BinOp::GtEq | BinOp::LtEq => {
                    check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
                    check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
                BinOp::And | BinOp::Or => {
                    check_expr_against(l, &Type::Bool, rule, all_rules, input_concept, all_concepts, bindings, errors);
                    check_expr_against(r, &Type::Bool, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
                BinOp::Eq | BinOp::NotEq => {
                    check_equality_operands(l, r, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
            }
            // Outer-context check. Deliberately the SAME message the
            // catch-all below produces for this expression today, so adding
            // the arm does not change any existing diagnostic.
            if expected_outer != &produced {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "expression has type '{}' but context expects '{}'",
                        type_display(&produced),
                        type_display(expected_outer),
                    ),
                });
            }
        }
        // `not <bool>` and `-<number>` — the UNARY members of the family
        // above, with the same defect: `infer_expr_type` reports Bool / Number
        // for the node and the catch-all never looked at the child, so
        // `not t.s` and `-t.s` on a text field both verified clean.
        (Expr::Not(inner), Type::Bool) => {
            check_expr_against(inner, &Type::Bool, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Not(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "expression has type 'bool' but context expects '{}'",
                    type_display(other),
                ),
            });
        }
        (Expr::Neg(inner), Type::Number) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Neg(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "expression has type 'number' but context expects '{}'",
                    type_display(other),
                ),
            });
        }
        // Phase 11 slice 1: fetch(<connection>, <request_bytes>) — request
        // bytes must produce text. The fetch itself produces text; the
        // outer-context check is handled by the fall-through arm via
        // `infer_expr_type(Expr::Fetch(..))` returning Text.
        (Expr::Fetch(_, req), expected_outer) => {
            check_expr_against(req, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
            // Outer-context check: fetch returns text. If context expected
            // something else, surface the same error the fall-through arm
            // would produce.
            if expected_outer != &Type::Text {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "fetch produces text but the expected type is '{}'",
                        type_display(expected_outer),
                    ),
                });
            }
        }
        // Phase 12 (json_escape): json_escape produces text and requires
        // its inner expression to produce text. Mirrors the Fetch arm's
        // shape — recurse on the inner with expected=Text, then surface
        // an outer-context error when the surrounding type isn't text.
        (Expr::JsonEscape(inner), Type::Text) => {
            if !refuse_bytes_in_text_prim("json_escape", inner, rule, all_rules, input_concept, bindings, errors) {
                check_expr_against(inner, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
            }
        }
        (Expr::JsonEscape(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "json_escape produces text but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // Phase 12 (parse_int): mirrors JsonEscape's structure but the
        // outer-context type is Number (parse_int returns a number). Inner
        // must still produce text.
        (Expr::ParseInt(inner), Type::Number) => {
            if !refuse_bytes_in_text_prim("parse_int", inner, rule, all_rules, input_concept, bindings, errors) {
                check_expr_against(inner, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
            }
        }
        (Expr::ParseInt(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "parse_int produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `starts_with(<haystack>, <needle>)` produces bool. When the
        // surrounding context expects bool, recurse into both children with
        // expected=Text so the verifier rejects number arguments. When the
        // context expects something else, surface a clear mismatch naming
        // `starts_with` (mirror of the JsonEscape/ParseInt arms).
        (Expr::StartsWith(h, n), Type::Bool) => {
            for operand in [h, n] {
                if !refuse_bytes_in_text_prim("starts_with", operand, rule, all_rules, input_concept, bindings, errors) {
                    check_expr_against(operand, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
            }
        }
        (Expr::StartsWith(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "starts_with produces bool but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `contains(<haystack>, <needle>)` produces bool. Same shape as
        // StartsWith: when context is bool, both children must be text;
        // otherwise surface a mismatch naming `contains`.
        (Expr::Contains(h, n), Type::Bool) => {
            for operand in [h, n] {
                if !refuse_bytes_in_text_prim("contains", operand, rule, all_rules, input_concept, bindings, errors) {
                    check_expr_against(operand, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
            }
        }
        (Expr::Contains(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "contains produces bool but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `ends_with(<haystack>, <needle>)` produces bool. Same shape as
        // StartsWith / Contains: when context is bool, both children must be
        // text; otherwise surface a mismatch naming `ends_with`.
        (Expr::EndsWith(h, n), Type::Bool) => {
            for operand in [h, n] {
                if !refuse_bytes_in_text_prim("ends_with", operand, rule, all_rules, input_concept, bindings, errors) {
                    check_expr_against(operand, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
                }
            }
        }
        (Expr::EndsWith(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "ends_with produces bool but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `length(<text_expr>)` produces number. When the surrounding
        // context expects number, recurse into the inner with expected=Text —
        // or accept a `b"..."` byte-string literal, whose length is likewise a
        // byte count. See check_byte_addressable_operand for why the literal
        // and only the literal. Otherwise surface a clear mismatch (mirror of
        // the ParseInt arms).
        (Expr::Length(inner), Type::Number) => {
            check_byte_addressable_operand("length", inner, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Length(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "length produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `abs(<number_expr>)` produces number. Differs from ParseInt/Length/
        // JsonEscape: inner is number, not text. When the surrounding context
        // expects number, recurse into the inner with expected=Number; the
        // verifier will reject text/bool args via that recursion.
        (Expr::Abs(inner), Type::Number) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Abs(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "abs produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `min(<a>, <b>)` produces number. Both children must be number-typed;
        // recurse against Type::Number so non-number args are rejected through
        // the usual channel. Mirror of the Abs arms, but with two children.
        (Expr::Min(l, r), Type::Number) => {
            check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Min(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "min produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `max(<a>, <b>)` — same shape as Min: both children number-typed,
        // outer produces number.
        (Expr::Max(l, r), Type::Number) => {
            check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Max(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "max produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // The bitwise primitives — `band` / `bor` / `bxor` / `shl` / `shr`
        // (two number children) and `bnot` (one). All produce number.
        //
        // These arms exist for the OPERANDS, not the result: `infer_expr_type`
        // already reports Number for every bitwise node, so the catch-all was
        // checking the outer type. What it never did was RECURSE, so
        // `band(p.name, p.n)` on a text field passed verification while the
        // structurally identical `min(p.name, p.n)` was rejected. Recursing
        // with expected=Number closes that asymmetry — same shape as the
        // Min/Max arms directly above.
        (Expr::BitAnd(l, r), Type::Number)
        | (Expr::BitOr(l, r), Type::Number)
        | (Expr::BitXor(l, r), Type::Number)
        | (Expr::Shl(l, r), Type::Number)
        | (Expr::Shr(l, r), Type::Number) => {
            check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::BitNot(inner), Type::Number) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::BitAnd(_, _), other)
        | (Expr::BitOr(_, _), other)
        | (Expr::BitXor(_, _), other)
        | (Expr::BitNot(_), other)
        | (Expr::Shl(_, _), other)
        | (Expr::Shr(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "{} produces number but the expected type is '{}'",
                    describe_expr_kind(expr),
                    type_display(other),
                ),
            });
        }
        // `substring(<text>, <start>, <end>)` produces text. When the context
        // expects text, recurse into the first child with expected=Text and
        // into start/end with expected=Number so non-conforming argument types
        // are rejected through the usual channel. Otherwise surface a clear
        // mismatch (mirror of the JsonEscape/Length arms but with three
        // children).
        (Expr::Substring(t, s, e), Type::Text) => {
            if !refuse_bytes_substring(t, rule, all_rules, input_concept, bindings, errors) {
                check_expr_against(t, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
            }
            check_expr_against(s, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(e, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        // A bytes operand in ANY expected context — `Frame { data:
        // substring(req.data, 0, 2) }` expects bytes — gets refusal #1's
        // breadcrumb rather than "substring produces text but the expected
        // type is 'bytes'", which is true and points nowhere.
        (Expr::Substring(t, _, _), _)
            if refuse_bytes_substring(t, rule, all_rules, input_concept, bindings, errors) => {}
        (Expr::Substring(_, _, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "substring produces text but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `byte_at(<text>, <index>)` produces a number (the byte value at the
        // given offset, in 0..256). When the context expects number, recurse
        // into the first child with expected=Text and into the index child
        // with expected=Number. Otherwise surface a clear mismatch (mirror of
        // the Substring arms, but with two children and a Number return).
        // The haystack may also be a `b"..."` byte-string literal — a declared
        // constant byte table read by index. See check_byte_addressable_operand.
        (Expr::ByteAt(t, i), Type::Number) => {
            check_byte_addressable_operand("byte_at", t, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(i, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::ByteAt(_, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "byte_at produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `fold_bytes(<text>, <init>, acc, byte, idx => <body>)` produces a
        // number (the final accumulator value). When the context expects
        // number, recurse into text with expected=Text and into init with
        // expected=Number. The body is left unchecked here (its three
        // lambda-bound vars — acc, byte, idx — aren't tracked in this
        // pass), consistent with how Fold's body is handled. Otherwise
        // surface a clear mismatch.
        (Expr::FoldBytes(t, init, _, _, _, _body), Type::Number) => {
            check_expr_against(t, &Type::Text, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(init, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::FoldBytes(_, _, _, _, _, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "fold_bytes produces number but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        (Expr::MatchResult(_target, _, ok_body, _, err_body), _) => {
            // Both arms must produce `expected`. The target should be a Result —
            // checking that requires inferring through lambda bindings, which
            // this pass does not track. Skipped, not fabricated.
            check_expr_against(ok_body, expected, rule, all_rules, input_concept, all_concepts, bindings, errors);
            check_expr_against(err_body, expected, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        // Map and Filter only fit a Collection context. Their bodies depend
        // on lambda-bound variables we do not yet track, so the body is left
        // unchecked, but the SHAPE (collection-producing) is enforced.
        (Expr::Map(_, _, _) | Expr::Filter(_, _, _), Type::Collection(_)) => {}
        (Expr::Map(_, _, _), other) | (Expr::Filter(_, _, _), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "map/filter produces a collection but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // concat(e1, e2, ...) produces text. If the context expects text,
        // verify each arg is scalar (number/bool/text); anything else
        // (collection, Result, record) is a type error — concat only
        // serializes scalar values.
        (Expr::Concat(args), Type::Text) => {
            for arg in args {
                if let Some(inferred) = infer_expr_type(arg, rule, all_rules, input_concept, bindings) {
                    match inferred {
                        Type::Number | Type::Bool | Type::Text => {}
                        Type::Bytes => {
                            errors.push(VerifyError {
                                context: format!("rule '{}' / logic", rule.name),
                                message:
                                    "concat mixes bytes and text: a bytes argument (b\"...\" / le32 / le64) \
                                     cannot appear in a text concat — bytes and text never implicitly convert"
                                        .to_string(),
                            });
                        }
                        other => {
                            errors.push(VerifyError {
                                context: format!("rule '{}' / logic", rule.name),
                                message: format!(
                                    "concat argument has type '{}'; concat only accepts scalar values (number, bool, text)",
                                    type_display(&other),
                                ),
                            });
                        }
                    }
                }
                // Else: not inferable — conservative silence.
            }
        }
        // Backend brick b2: a bytes concat. EVERY argument must be bytes-typed
        // (a `b"..."` literal, le32/le64, or another bytes value). A number
        // that should become bytes goes through le32/le64 explicitly — no
        // implicit itoa here. Mixing bytes with text/number is a type error.
        (Expr::Concat(args), Type::Bytes) => {
            for arg in args {
                // Each arg must itself check out as bytes; recurse so le32/le64
                // arg-type errors and nested-concat errors surface.
                check_expr_against(arg, &Type::Bytes, rule, all_rules, input_concept, all_concepts, bindings, errors);
                if let Some(inferred) = infer_expr_type(arg, rule, all_rules, input_concept, bindings) {
                    if inferred != Type::Bytes {
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", rule.name),
                            message: format!(
                                "concat mixes bytes and text: a bytes concat only accepts bytes arguments \
                                 (b\"...\" / le32 / le64), but this argument has type '{}'",
                                type_display(&inferred),
                            ),
                        });
                    }
                }
            }
        }
        (Expr::Concat(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "concat produces text or bytes but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // le32/le64: number → bytes. Inner must be number-typed; recurse with
        // expected=Number so text/bool args are rejected through the usual
        // channel (mirror of the Abs arms).
        (Expr::Le32(inner), Type::Bytes) | (Expr::Le64(inner), Type::Bytes) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::Le32(_), other) | (Expr::Le64(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "le32/le64 produce bytes but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `arena_scope(inner)` : bytes — a declared arena-reclaim boundary
        // for the streaming emitter. Only valid in a bytes (streaming)
        // position; inner must itself be bytes-typed (its bytes are streamed
        // unchanged, then the arena node-count is restored). Restricting it
        // to a bytes context is what makes it sound: a stored / let-bound
        // result would dangle after the reset.
        (Expr::ArenaScope(inner), Type::Bytes) => {
            check_expr_against(inner, &Type::Bytes, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        // `arena_scope(inner)` : number — the SCALAR form (slice 2). The inner
        // walk's arena nodes are reclaimed and the inner's NUMBER is returned
        // unchanged. Sound for exactly this expected type and no other: an i64
        // scalar references no arena node, so nothing dangles after the reset.
        // Deliberately NOT a catch-all — keeping every other expected type on
        // the error arm below is the anti-dangling guard. In particular
        // `(ArenaScope, Named(..))` must keep erroring: a concept-typed inner
        // yields an arena INDEX, which would point into the reclaimed region.
        (Expr::ArenaScope(inner), Type::Number) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::ArenaScope(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "arena_scope(...) is only valid in a bytes (streaming) position or a number (scalar) position, but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // `abort_if(<number>)` : bytes — V3 self-verify gate. In a bytes
        // (streaming) position it EXECUTES a runtime check: nonzero →
        // sys_exit(1), fail-closed, nothing streamed; zero → falls through
        // streaming ZERO bytes, so the surrounding bytes concat is
        // byte-identical to the ungated form. The inner must be
        // number-typed. Restricting to a bytes context is the soundness
        // frame it shares with arena_scope: the gate guards a bytes
        // stream and contributes no bytes to it — a value position would
        // have nothing meaningful to evaluate to.
        (Expr::AbortIf(inner), Type::Bytes) => {
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, bindings, errors);
        }
        (Expr::AbortIf(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "abort_if(...) executes a fail-closed check and streams zero bytes; it is only valid in a bytes (streaming) position, but the expected type is '{}'",
                    type_display(other),
                ),
            });
        }
        // Phase A slice 2: variant construction —
        // `ConceptName::VariantName { field: expr, ... }`. Cross-check that
        // the concept is a sum-type concept, the variant exists, and the
        // assignment field set matches the variant's payload exactly.
        (Expr::VariantConstruct(name, variant_name, fields), expected_ty) => {
            let concept = match all_concepts.get(name) {
                Some(c) => *c,
                None => {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "variant constructor references unknown concept '{}'",
                            name
                        ),
                    });
                    return;
                }
            };
            // Concept must be a sum type (non-empty variants, empty fields).
            if concept.variants.is_empty() {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "concept '{}' is a record concept (has fields), expected sum-type concept for variant construction `{}::{}`",
                        name, name, variant_name
                    ),
                });
                return;
            }
            // Locate the named variant.
            let variant = match concept.variants.iter().find(|v| &v.name == variant_name) {
                Some(v) => v,
                None => {
                    let available: Vec<&str> = concept.variants.iter().map(|v| v.name.as_str()).collect();
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "concept '{}' has no variant named '{}' (available: {})",
                            name,
                            variant_name,
                            available.join(", ")
                        ),
                    });
                    return;
                }
            };
            // Expected type, when known, should be the named concept.
            let shape_matches = match expected_ty {
                Type::Named(n) => n == name,
                Type::Collection(elem) => elem == name, // for use inside a map body
                _ => false,
            };
            if !shape_matches {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "variant constructor '{}::{}' produces type '{}' but context expects '{}'",
                        name,
                        variant_name,
                        name,
                        type_display(expected_ty),
                    ),
                });
            }
            // Field set: every payload field must be provided, no extras.
            let provided: HashSet<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            let declared: HashSet<&str> = variant.fields.iter().map(|f| f.name.as_str()).collect();
            for missing in declared.difference(&provided) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "variant constructor '{}::{}' is missing payload field '{}'",
                        name, variant_name, missing
                    ),
                });
            }
            for extra in provided.difference(&declared) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "variant constructor '{}::{}' has unknown payload field '{}'",
                        name, variant_name, extra
                    ),
                });
            }
            // Per-field type check: each provided field's expression must
            // match the declared payload field's type (when inferable).
            for (field_name, field_expr) in fields {
                if let Some(decl) = variant.fields.iter().find(|f| &f.name == field_name) {
                    check_expr_against(
                        field_expr,
                        &decl.ty,
                        rule,
                        all_rules,
                        input_concept,
                        all_concepts,
                        bindings,
                        errors,
                    );
                }
            }
        }
        // Phase A slice 3 — pattern match over a sum-type's variants.
        // Cross-check the scrutinee resolves to a sum-type concept, every
        // arm names a real variant of that concept, binder count matches
        // payload arity, the set of arm variants equals the concept's
        // variant set exactly (exhaustiveness + no duplicate + no unknown),
        // and each arm body typechecks against the rule's expected output
        // type. Binders introduced by an arm are lambda-bound for purity's
        // `reads:` proof (handled separately in `collect_lambda_bound_names`).
        (Expr::MatchVariant(scrutinee, arms), expected) => {
            // Resolve the scrutinee's concept name. Slice-3 limit: the
            // scrutinee must infer to a `Type::Named(C)`. Common shapes
            // — input ident, VariantConstruct, Call returning Named —
            // are all covered by `infer_expr_type`; let/lambda-bound
            // scrutinees infer to None and are reported.
            let concept_name = match infer_expr_type(scrutinee, rule, all_rules, input_concept, bindings) {
                Some(Type::Named(n)) => n,
                Some(other) => {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "match scrutinee has type '{}' but pattern match requires a sum-type concept",
                            type_display(&other),
                        ),
                    });
                    return;
                }
                None => {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: "match scrutinee's type could not be inferred — \
                                  slice A.3 requires the scrutinee to be the rule \
                                  input, a variant constructor, or a rule call \
                                  returning a named sum-type concept".into(),
                    });
                    return;
                }
            };
            let concept = match all_concepts.get(&concept_name) {
                Some(c) => *c,
                None => {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "match scrutinee references unknown concept '{}'",
                            concept_name
                        ),
                    });
                    return;
                }
            };
            if concept.variants.is_empty() {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "match scrutinee has type '{}' which is a record concept (has fields), expected sum-type concept",
                        concept_name
                    ),
                });
                return;
            }
            // Walk arms: validate variant name, binder arity, and body type.
            // We track seen variant names to detect duplicates and to
            // compute the exhaustiveness diff at the end.
            let mut seen: HashSet<&str> = HashSet::new();
            for arm in arms {
                let variant = match concept.variants.iter().find(|v| v.name == arm.variant_name) {
                    Some(v) => v,
                    None => {
                        let available: Vec<&str> = concept.variants.iter().map(|v| v.name.as_str()).collect();
                        errors.push(VerifyError {
                            context: format!("rule '{}' / logic", rule.name),
                            message: format!(
                                "match arm references unknown variant '{}::{}' (available: {})",
                                concept_name,
                                arm.variant_name,
                                available.join(", ")
                            ),
                        });
                        continue;
                    }
                };
                if !seen.insert(arm.variant_name.as_str()) {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "match arm for '{}::{}' is duplicated",
                            concept_name, arm.variant_name
                        ),
                    });
                }
                if arm.binders.len() != variant.fields.len() {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "match arm '{}::{}' has {} binder(s) but the variant's payload has {} field(s)",
                            concept_name,
                            arm.variant_name,
                            arm.binders.len(),
                            variant.fields.len(),
                        ),
                    });
                }
                // Detect collisions within the same arm — two
                // positional binders that share the same name would
                // shadow each other at runtime and confuse the auditor.
                let mut arm_seen: HashSet<&str> = HashSet::new();
                for b in &arm.binders {
                    if let Some(name) = b {
                        if !arm_seen.insert(name.as_str()) {
                            errors.push(VerifyError {
                                context: format!("rule '{}' / logic", rule.name),
                                message: format!(
                                    "match arm '{}::{}' binds '{}' twice in the same arm",
                                    concept_name, arm.variant_name, name
                                ),
                            });
                        }
                    }
                }
                // Body must produce the rule's expected output type.
                // Binders are in scope for the body — the lambda-bound
                // walk (`collect_lambda_bound_names`) accounts for them
                // when purity checks the body's `reads:` proof.
                check_expr_against(&arm.body, expected, rule, all_rules, input_concept, all_concepts, bindings, errors);
            }
            // Exhaustiveness: every declared variant must have an arm.
            for declared in &concept.variants {
                if !seen.contains(declared.name.as_str()) {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "match on '{}' is not exhaustive — missing arm for variant '{}::{}'",
                            concept_name, concept_name, declared.name
                        ),
                    });
                }
            }
        }
        // Record(ConceptName) construction: cross-check field set + types.
        (Expr::Record(name, fields), expected_ty) => {
            let concept = match all_concepts.get(name) {
                Some(c) => *c,
                None => {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "record constructor references unknown concept '{}'",
                            name
                        ),
                    });
                    return;
                }
            };
            // Expected type, when known, should be the named concept.
            let shape_matches = match expected_ty {
                Type::Named(n) => n == name,
                Type::Collection(elem) => elem == name, // for use inside a map body
                _ => false, // Number/Bool/Text/Result don't match any record
            };
            if !shape_matches {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "record constructor '{}' produces type '{}' but context expects '{}'",
                        name,
                        name,
                        type_display(expected_ty),
                    ),
                });
            }
            // Field set: every declared field must be provided, no extras.
            let provided: HashSet<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
            let declared: HashSet<&str> = concept.fields.iter().map(|f| f.name.as_str()).collect();
            for missing in declared.difference(&provided) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "record constructor '{}' is missing field '{}'",
                        name, missing
                    ),
                });
            }
            for extra in provided.difference(&declared) {
                errors.push(VerifyError {
                    context: format!("rule '{}' / logic", rule.name),
                    message: format!(
                        "record constructor '{}' has unknown field '{}'",
                        name, extra
                    ),
                });
            }
            // Per-field type check: each provided field's expression must
            // match the declared field type (when inferable).
            for (field_name, field_expr) in fields {
                if let Some(decl) = concept.fields.iter().find(|f| &f.name == field_name) {
                    check_expr_against(
                        field_expr,
                        &decl.ty,
                        rule,
                        all_rules,
                        input_concept,
                        all_concepts,
                        bindings,
                        errors,
                    );
                }
            }
        }
        _ => {
            if let Some(inferred) = infer_expr_type(expr, rule, all_rules, input_concept, bindings) {
                if &inferred != expected {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / logic", rule.name),
                        message: format!(
                            "expression has type '{}' but context expects '{}'",
                            type_display(&inferred),
                            type_display(expected),
                        ),
                    });
                }
            }
            // Else: inference not possible here — stay silent.
        }
    }
}

/// Best-effort type inference. Returns None when the expression's type cannot
/// be determined without tracking let/lambda bindings or deep semantic info.
fn infer_expr_type(
    expr: &Expr,
    rule: &Rule,
    all_rules: &[&Rule],
    concept: Option<&Concept>,
    bindings: &HashMap<String, &Concept>,
) -> Option<Type> {
    match expr {
        Expr::Number(_) => Some(Type::Number),
        Expr::Text(_) => Some(Type::Text),
        // Backend brick b1: a `b"..."` byte-string literal has type bytes.
        // Bytes participates in nothing else yet (no arithmetic, concat, or
        // coercion), so this is the only inference path that produces it.
        Expr::Bytes(_) => Some(Type::Bytes),
        // Phase 9 slice 1: read(<resource>) returns text. Existence of the
        // resource is checked separately by verify_rule via a dedicated
        // walk; this inference path only needs the type.
        Expr::Read(_) => Some(Type::Text),
        // Slice entropy-1: random(<entropy>) returns BYTES — never text, so
        // every text sink (concat, HttpResponse.body, text ==, after: sets)
        // refuses it through the existing bytes/text isolation. Existence of
        // the item is checked separately by verify_program's dedicated walk.
        Expr::Random(_) => Some(Type::Bytes),
        Expr::Ident(name) if name == &rule.input_name => Some(rule.input_ty.clone()),
        // A `let` bound to a RECORD concept, or the `context:` binding. Same
        // map, same lookup and the same soundness argument as the `Field` arm
        // just below — see `collect_binding_concepts` for what is and is not
        // in it (only top-level lets whose type this pass can name, and only
        // RECORD concepts: `record_concept_of` filters sum types out).
        //
        // `collect_binding_concepts` deliberately declined to widen this arm
        // when it landed, on the grounds that it "feeds every `Ident` position
        // in the bidirectional check, so widening it would add strictness far
        // beyond a `.field` access". That is exactly right, and it is exactly
        // what is wanted here: `let p = mk(i)` then `out = p * 1000` verified
        // clean because this arm answered None, and the emitter was left to
        // refuse it alone (agg-1 refusal #5). The strictness it adds is only
        // ever about a name whose concept the pass already knows.
        //
        // The names in the map are pre-filtered by `verify_rule` against
        // every lambda / match binder in the logic, so a binder that SHADOWS
        // a record let cannot be misread as that let.
        Expr::Ident(name) => bindings.get(name.as_str()).map(|c| Type::Named(c.name.clone())),
        Expr::Field(base, field_name) => {
            if let Expr::Ident(n) = base.as_ref() {
                if n == &rule.input_name {
                    return concept.and_then(|c| {
                        c.fields
                            .iter()
                            .find(|f| &f.name == field_name)
                            .map(|f| f.ty.clone())
                    });
                }
                // The input is not the only base whose concept is known. A
                // `context:` binding and a `let` bound to a record both name a
                // concept too, and `bindings` carries exactly those (see
                // `collect_binding_concepts`). Resolving them here is what lets
                // the surrounding expression be typechecked the same way it
                // already is for an input field — one lookup, one map, no
                // second mechanism.
                //
                // `bindings` is empty for every caller with no such scope, so
                // this arm is inert there.
                if let Some(c) = bindings.get(n.as_str()) {
                    return c
                        .fields
                        .iter()
                        .find(|f| &f.name == field_name)
                        .map(|f| f.ty.clone());
                }
            }
            None
        }
        Expr::Binary(op, _, _) => match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => Some(Type::Number),
            BinOp::Gt | BinOp::Lt | BinOp::GtEq | BinOp::LtEq | BinOp::Eq | BinOp::NotEq
            | BinOp::And | BinOp::Or => Some(Type::Bool),
        },
        Expr::Not(_) => Some(Type::Bool),
        Expr::Neg(_) => Some(Type::Number),
        Expr::Call(name, _) => all_rules
            .iter()
            .find(|r| r.name == *name)
            .map(|r| r.output_ty.clone()),
        Expr::If(_, then_e, _) => infer_expr_type(then_e, rule, all_rules, concept, bindings),
        Expr::Quantifier(_, _, _, _) => Some(Type::Bool),
        Expr::Record(name, _) => Some(Type::Named(name.clone())),
        // Phase A slice 2: variant construction yields the concept type —
        // same outer shape as record construction.
        Expr::VariantConstruct(name, _, _) => Some(Type::Named(name.clone())),
        // concat is text by default, but if ANY argument is bytes-typed the
        // whole concat is bytes (backend brick b2). Mixing bytes with
        // text/number is a type error, surfaced by check_expr_against; for
        // inference we report Bytes if any inferable arg is bytes, else Text.
        Expr::Concat(args) => {
            let any_bytes = args.iter().any(|a| {
                matches!(infer_expr_type(a, rule, all_rules, concept, bindings), Some(Type::Bytes))
            });
            if any_bytes { Some(Type::Bytes) } else { Some(Type::Text) }
        }
        // le32/le64 turn a number into 4/8 little-endian bytes.
        Expr::Le32(_) | Expr::Le64(_) => Some(Type::Bytes),
        // `arena_scope(inner)` is transparent: its type IS inner's type
        // (which the bytes-context check constrains to bytes).
        Expr::ArenaScope(inner) => infer_expr_type(inner, rule, all_rules, concept, bindings),
        // `abort_if(<number>)` types as BYTES (it occupies a bytes-stream
        // position and contributes zero bytes to it) — NOT as its inner's
        // number type. Typing it bytes is what keeps an all-bytes concat's
        // mixing check sound: `concat(abort_if(e), <bytes...>)` stays an
        // all-bytes concat.
        Expr::AbortIf(_) => Some(Type::Bytes),
        // Phase 11 slice 1: fetch(<connection>, _) returns text — same
        // inference as read(<resource>). Existence of the connection and
        // type-check of the request bytes are handled separately.
        Expr::Fetch(_, _) => Some(Type::Text),
        // Phase 12 (json_escape): json_escape(<text>) returns text. The
        // inner expression's text-ness is enforced by check_expr_against;
        // here we only need the outer type for inference.
        Expr::JsonEscape(_) => Some(Type::Text),
        // Phase 12 (parse_int): parse_int(<text>) returns number. Inner
        // text-ness enforced by check_expr_against.
        Expr::ParseInt(_) => Some(Type::Number),
        // `now_unix()` returns number (Unix epoch seconds).
        Expr::NowUnix => Some(Type::Number),
        // `starts_with(<text>, <text>)` returns bool. Both arguments must be
        // text-typed; check_expr_against enforces that — here we only need
        // the outer type for inference.
        Expr::StartsWith(_, _) => Some(Type::Bool),
        // `contains(<text>, <text>)` returns bool. Same shape as
        // StartsWith — both arguments must be text-typed; the outer type
        // is fixed at bool for inference.
        Expr::Contains(_, _) => Some(Type::Bool),
        // `ends_with(<text>, <text>)` returns bool. Same shape as
        // StartsWith / Contains.
        Expr::EndsWith(_, _) => Some(Type::Bool),
        // `length(<text>)` returns number. Inner text-ness enforced by
        // check_expr_against.
        Expr::Length(_) => Some(Type::Number),
        // `abs(<number>)` returns number. Inner number-ness enforced by
        // check_expr_against.
        Expr::Abs(_) | Expr::BitNot(_) => Some(Type::Number),
        // `min(<number>, <number>)` / `max(<number>, <number>)` return number.
        // Both children are number-typed; check_expr_against enforces that.
        Expr::Min(_, _) | Expr::Max(_, _) | Expr::BitAnd(_,_) | Expr::BitOr(_,_) | Expr::BitXor(_,_) | Expr::Shl(_,_) | Expr::Shr(_,_) => Some(Type::Number),
        // `substring(<text>, <number>, <number>)` returns text. Inner shapes
        // are enforced by check_expr_against; here we only need the outer
        // type for inference.
        Expr::Substring(_, _, _) => Some(Type::Text),
        // `byte_at(<text>, <number>)` returns number. Inner shapes are
        // enforced by check_expr_against; here we only need the outer type
        // for inference.
        Expr::ByteAt(_, _) => Some(Type::Number),
        // `fold_bytes(<text>, <init>, acc, byte, idx => <body>)` returns
        // number (the final accumulator). Body shape is enforced by
        // check_expr_against; outer type is what inference cares about.
        Expr::FoldBytes(_, _, _, _, _, _) => Some(Type::Number),
        // Map/Filter/Fold/Ok/Err/MatchResult: deferred until lambda binding
        // tracking lands. Returning None means we do not check; we also do not
        // falsely accept.
        _ => None,
    }
}

fn type_display(ty: &Type) -> String {
    match ty {
        Type::Number => "number".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Text => "text".to_string(),
        Type::Bytes => "bytes".to_string(),
        Type::Collection(inner) => format!("collection({})", inner),
        Type::Named(n) => n.clone(),
        Type::Result(t, e) => format!("Result({}, {})", type_display(t), type_display(e)),
    }
}

/// Enforce the sealed-subgraph layer discipline: a rule that declares a layer
/// may only call rules that ALSO declare a layer, and only layers that its
/// own layer is allowed to call (domain->domain, application->domain|application,
/// interface->any). Crossing into unlayered code is forbidden — that would let
/// a layered rule transitively touch anything and defeat the point.
fn check_layer_discipline(
    rule: &Rule,
    caller_layer: Layer,
    facts: &LogicFacts,
    all_rules: &[&Rule],
    errors: &mut Vec<VerifyError>,
) {
    for call_path in &facts.calls {
        if call_path.len() != 1 {
            continue;
        }
        let call_name = &call_path[0];
        let callee = match all_rules.iter().find(|r| r.name == *call_name) {
            Some(r) => *r,
            None => continue, // unknown-call error is reported separately above
        };
        match callee.layer {
            None => {
                errors.push(VerifyError {
                    context: format!("rule '{}' / @layer", rule.name),
                    message: format!(
                        "rule declares layer '{}' but calls unlayered rule '{}'; a layered rule may only call other layered rules",
                        caller_layer.as_str(),
                        call_name
                    ),
                });
            }
            Some(target) if !caller_layer.can_call(target) => {
                errors.push(VerifyError {
                    context: format!("rule '{}' / @layer", rule.name),
                    message: format!(
                        "rule at layer '{}' calls '{}' at layer '{}'; '{}' rules may not call '{}' rules",
                        caller_layer.as_str(),
                        call_name,
                        target.as_str(),
                        caller_layer.as_str(),
                        target.as_str()
                    ),
                });
            }
            Some(_) => {} // allowed
        }
    }
}

fn check_hints(
    rule: &Rule,
    hints: &Hints,
    facts: &LogicFacts,
    concepts: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    if hints.vectorizable.is_some() {
        if !facts.calls.is_empty() {
            errors.push(VerifyError {
                context: format!("rule '{}' / hints.vectorizable", rule.name),
                message: "vectorizable requires no calls (element must be independent)".into(),
            });
        }
    }

    if let Some(overflow) = &hints.overflow {
        if overflow.min > overflow.max {
            errors.push(VerifyError {
                context: format!("rule '{}' / hints.overflow", rule.name),
                message: format!(
                    "invalid overflow bounds: min {} > max {}",
                    overflow.min, overflow.max
                ),
            });
        } else {
            // Build field ranges from concept (assume i64 full range if no overflow hint on fields)
            // For POC: fields are assumed to have the range declared in the overflow hint's context
            // We use a conservative default range for input fields
            let mut field_ranges: HashMap<&str, (i64, i64)> = HashMap::new();
            if let Type::Named(concept_name) = &rule.input_ty {
                if let Some(concept) = concepts.get(concept_name) {
                    for field in &concept.fields {
                        if field.ty == Type::Number {
                            let range = field.range.unwrap_or((0, i32::MAX as i64));
                            field_ranges.insert(field.name.as_str(), range);
                        }
                    }
                }
            }

            if let Some((actual_min, actual_max)) =
                compute_range(&rule.logic.value, &field_ranges, &rule.input_name)
            {
                if actual_min < overflow.min || actual_max > overflow.max {
                    errors.push(VerifyError {
                        context: format!("rule '{}' / hints.overflow", rule.name),
                        message: format!(
                            "computed range [{}, {}] exceeds declared [{}, {}]",
                            actual_min, actual_max, overflow.min, overflow.max
                        ),
                    });
                }
            }
            // If compute_range returns None, we can't verify — we accept the hint but don't optimize
        }
    }
}

fn verify_source_ref(sref: &SourceRef, base_dir: &StdPath) -> Result<(), String> {
    let path = base_dir.join(&sref.file);
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read '{}': {}", path.display(), e))?;
    let total = content.lines().count();
    let line = sref.line as usize;
    if line == 0 || line > total {
        return Err(format!(
            "line {} does not exist in '{}' (file has {} lines)",
            sref.line, sref.file, total
        ));
    }
    Ok(())
}

#[derive(Debug, Default)]
struct LogicFacts {
    reads: HashSet<Vec<String>>,
    calls: HashSet<Vec<String>>,
    /// Paths whose base is a TOP-LEVEL `let` binding.
    ///
    /// These are not reads of the input — they read a local — so they are
    /// correctly kept out of `reads` and out of the purity proof. But they
    /// still carry a `.field` access, and until this field existed they were
    /// simply DISCARDED, which is the whole reason the verifier typechecked
    /// `p.a` and never `r.a`. Partitioned here rather than dropped, then
    /// validated in `verify_rule` by the same helper the input path uses.
    ///
    /// Lambda- and arm-bound paths never reach this set: `collect_expr_facts`
    /// filters them inside each body, before the partition below sees them.
    local_reads: HashSet<Vec<String>>,
}

/// When a rule's body contains `match_result(callee(...), ...)`, the
/// native emitter inlines the callee's body into the outer rule's
/// frame. The callee's resource/connection reads (and its `now` read)
/// therefore happen during the outer rule's execution — and the
/// outer's prologue must declare them.
///
/// This pass walks the rule body for `match_result` nodes whose target
/// is a `Call(callee_name, [...])`, looks the callee up, gathers ITS
/// reads, filters to the ones that are top-level resource/connection
/// names or the synthetic `now`, and adds them to the outer rule's
/// facts. Field reads (`p.amount`-style) are NOT propagated — those
/// are bound to the callee's input variable and don't appear in the
/// outer's scope.
///
/// Cycle protection: if the callee chain ever loops, we stop at each
/// rule once via a visited set. The verifier's `calls` check elsewhere
/// catches genuine circular references; here we just refuse to recurse
/// infinitely.
fn augment_facts_with_transitive_match_result_reads(
    rule: &Rule,
    all_rules: &[&Rule],
    all_resources: &HashSet<String>,
    all_connections: &HashSet<String>,
    all_entropies: &HashSet<String>,
    facts: &mut LogicFacts,
) {
    let rules_by_name: std::collections::HashMap<&str, &Rule> =
        all_rules.iter().map(|r| (r.name.as_str(), *r)).collect();
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(rule.name.clone());
    walk_for_match_result_callees(
        &rule.logic.value,
        &rules_by_name,
        all_resources,
        all_connections,
        all_entropies,
        &mut visited,
        &mut facts.reads,
    );
    for (_, expr) in &rule.logic.bindings {
        walk_for_match_result_callees(
            expr,
            &rules_by_name,
            all_resources,
            all_connections,
            all_entropies,
            &mut visited,
            &mut facts.reads,
        );
    }
}

/// Walk an expression for `MatchResult` nodes whose target is a Call;
/// merge each callee's resource/connection/now reads into `out_reads`.
fn walk_for_match_result_callees(
    expr: &Expr,
    rules_by_name: &std::collections::HashMap<&str, &Rule>,
    all_resources: &HashSet<String>,
    all_connections: &HashSet<String>,
    all_entropies: &HashSet<String>,
    visited: &mut HashSet<String>,
    out_reads: &mut HashSet<Vec<String>>,
) {
    match expr {
        Expr::MatchResult(target, _, ok_body, _, err_body) => {
            if let Expr::Call(callee_name, _) = target.as_ref() {
                if let Some(callee) = rules_by_name.get(callee_name.as_str()) {
                    if visited.insert(callee.name.clone()) {
                        // Collect the callee's own facts and merge the
                        // resource/connection-shape reads in.
                        let callee_facts = collect_logic_facts(&callee.logic);
                        for path in &callee_facts.reads {
                            if path.len() == 1 {
                                let name = &path[0];
                                if all_resources.contains(name)
                                    || all_connections.contains(name)
                                    || all_entropies.contains(name)
                                    || name == "now"
                                {
                                    out_reads.insert(path.clone());
                                }
                            }
                        }
                        // Recurse: the callee may itself match_result on
                        // another rule. Same propagation rules apply.
                        walk_for_match_result_callees(
                            &callee.logic.value,
                            rules_by_name,
                            all_resources,
                            all_connections,
                            all_entropies,
                            visited,
                            out_reads,
                        );
                        for (_, e) in &callee.logic.bindings {
                            walk_for_match_result_callees(
                                e,
                                rules_by_name,
                                all_resources,
                                all_connections,
                                all_entropies,
                                visited,
                                out_reads,
                            );
                        }
                    }
                }
            }
            walk_for_match_result_callees(ok_body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(err_body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        // Recurse into shapes that can contain a MatchResult somewhere.
        Expr::If(c, t, e) => {
            walk_for_match_result_callees(c, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(e, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Ok(i) | Expr::Err(i) | Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i)
        | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) | Expr::BitNot(i)
        | Expr::Le32(i) | Expr::Le64(i) | Expr::ArenaScope(i) | Expr::AbortIf(i) => {
            walk_for_match_result_callees(i, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Binary(_, l, r) => {
            walk_for_match_result_callees(l, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(r, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Min(a, b) | Expr::Max(a, b) | Expr::BitAnd(a, b) | Expr::BitOr(a, b) | Expr::BitXor(a, b) | Expr::Shl(a, b) | Expr::Shr(a, b) | Expr::StartsWith(a, b)
        | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
            walk_for_match_result_callees(a, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(b, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                walk_for_match_result_callees(a, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            }
        }
        Expr::Record(_, fields) => {
            for (_, v) in fields {
                walk_for_match_result_callees(v, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            }
        }
        Expr::Fetch(_, req) => {
            walk_for_match_result_callees(req, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Fold(coll, init, _, _, body) => {
            walk_for_match_result_callees(coll, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(init, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Quantifier(_, coll, _, body)
        | Expr::Map(coll, _, body)
        | Expr::Filter(coll, _, body) => {
            walk_for_match_result_callees(coll, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::Substring(t, s, e) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(s, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(e, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::ByteAt(t, i) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(i, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        Expr::FoldBytes(t, init, _, _, _, body) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(init, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
        }
        // Phase A slice 2: recurse into each field assignment's expression.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, v) in fields {
                walk_for_match_result_callees(v, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            }
        }
        // Phase A slice 3: pattern match — recurse into scrutinee + each
        // arm's body. The MatchVariant itself doesn't have a Call target
        // shape (unlike MatchResult), so no inlined-callee fact propagation
        // here; we just walk for any MatchResult nested inside the bodies.
        Expr::MatchVariant(scrutinee, arms) => {
            walk_for_match_result_callees(scrutinee, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            for a in arms {
                walk_for_match_result_callees(&a.body, rules_by_name, all_resources, all_connections, all_entropies, visited, out_reads);
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Field(_, _) | Expr::Ident(_)
        | Expr::Read(_) | Expr::Random(_) | Expr::NowUnix => {}
    }
}

/// Every rule-call site in an expression tree, as `(callee_name, arg_count)`.
///
/// WHY A SEPARATE WALK rather than an extra out-param on `collect_expr_facts`:
/// that function filters reads by lambda scope and merges sub-scopes through
/// fresh `HashSet`s, none of which an arity check cares about — a call is a
/// call wherever it sits. This walk therefore has no scoping logic at all,
/// just "recurse into every sub-expression".
///
/// The match has NO catch-all arm on purpose: adding an `Expr` variant that
/// can hold a sub-expression must fail to compile here rather than silently
/// hide call sites from the arity check.
///
/// Every `Expr::Call` in the AST is a RULE call. The parser intercepts all 36
/// `PRIMITIVE_CALL_NAMES` in call position before the generic `Expr::Call`
/// fallback (see `parse_primary`), and record/variant construction have their
/// own nodes (`Expr::Record` / `Expr::VariantConstruct`), so no primitive and
/// no constructor can reach this collector.
fn collect_call_sites(expr: &Expr, out: &mut Vec<(String, usize)>) {
    match expr {
        Expr::Call(name, args) => {
            out.push((name.clone(), args.len()));
            for a in args {
                collect_call_sites(a, out);
            }
        }
        // Leaves — nothing to recurse into.
        Expr::Number(_)
        | Expr::Text(_)
        | Expr::Bytes(_)
        | Expr::Ident(_)
        | Expr::Read(_)
        | Expr::Random(_)
        | Expr::NowUnix => {}
        // One sub-expression.
        Expr::Field(inner, _)
        | Expr::Not(inner)
        | Expr::Neg(inner)
        | Expr::Ok(inner)
        | Expr::Err(inner)
        | Expr::JsonEscape(inner)
        | Expr::ParseInt(inner)
        | Expr::Length(inner)
        | Expr::Abs(inner)
        | Expr::Le32(inner)
        | Expr::Le64(inner)
        | Expr::BitNot(inner)
        | Expr::ArenaScope(inner)
        | Expr::AbortIf(inner)
        | Expr::Fetch(_, inner) => collect_call_sites(inner, out),
        // Two sub-expressions.
        Expr::Binary(_, l, r)
        | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r)
        | Expr::Contains(l, r)
        | Expr::ByteAt(l, r)
        | Expr::Min(l, r)
        | Expr::Max(l, r)
        | Expr::BitAnd(l, r)
        | Expr::BitOr(l, r)
        | Expr::BitXor(l, r)
        | Expr::Shl(l, r)
        | Expr::Shr(l, r) => {
            collect_call_sites(l, out);
            collect_call_sites(r, out);
        }
        // Three sub-expressions.
        Expr::If(a, b, c) | Expr::Substring(a, b, c) => {
            collect_call_sites(a, out);
            collect_call_sites(b, out);
            collect_call_sites(c, out);
        }
        // Lambda-bearing forms: the binder names are irrelevant here.
        Expr::Quantifier(_, coll, _, body) | Expr::Map(coll, _, body) | Expr::Filter(coll, _, body) => {
            collect_call_sites(coll, out);
            collect_call_sites(body, out);
        }
        Expr::Fold(coll, initial, _, _, body) => {
            collect_call_sites(coll, out);
            collect_call_sites(initial, out);
            collect_call_sites(body, out);
        }
        Expr::FoldBytes(text, initial, _, _, _, body) => {
            collect_call_sites(text, out);
            collect_call_sites(initial, out);
            collect_call_sites(body, out);
        }
        Expr::MatchResult(target, _, ok_body, _, err_body) => {
            collect_call_sites(target, out);
            collect_call_sites(ok_body, out);
            collect_call_sites(err_body, out);
        }
        // Field lists.
        Expr::Record(_, fields) | Expr::VariantConstruct(_, _, fields) => {
            for (_, e) in fields {
                collect_call_sites(e, out);
            }
        }
        Expr::Concat(args) => {
            for a in args {
                collect_call_sites(a, out);
            }
        }
        Expr::MatchVariant(scrutinee, arms) => {
            collect_call_sites(scrutinee, out);
            for arm in arms {
                collect_call_sites(&arm.body, out);
            }
        }
    }
}

/// A rule call must pass EXACTLY ONE argument.
///
/// DERIVED, not assumed. `Parser::parse_rule` makes the `input:` block
/// mandatory and `parse_binding_block` yields exactly one `(name, type)` pair,
/// so a rule structurally has exactly one input — there is no zero-input or
/// multi-input rule to call. The optional `context:` block does NOT change
/// this: a context is bound at the top-level invocation (read once from
/// argv/stdin by the emitter), never at a call site — `eval_rule_with_value`
/// inserts only `rule.input_name` into the callee's environment, and no call
/// syntax exists for supplying a context.
///
/// Both executors already enforce it — the interpreter with
/// `rule call expects 1 argument, got N` and the native emitter with
/// `native call requires exactly 1 argument`. Until this check landed the
/// VERIFIER did not, so `verbosec <file>` printed "all proofs check out" for a
/// program that neither `--run` nor `--native` would accept. That is not a
/// safety hole (it fails closed both ways) but it is a truthfulness hole in
/// the one component whose whole job is to be trustworthy.
fn check_call_arity(rule: &Rule, errors: &mut Vec<VerifyError>) {
    let mut sites: Vec<(String, usize)> = Vec::new();
    for (_, expr) in &rule.logic.bindings {
        collect_call_sites(expr, &mut sites);
    }
    collect_call_sites(&rule.logic.value, &mut sites);

    for (callee, argc) in sites {
        if argc != 1 {
            errors.push(VerifyError {
                context: format!("rule '{}' / calls", rule.name),
                message: format!(
                    "calls rule '{}' with {} argument{}; a rule call takes exactly 1 (a rule has exactly one 'input')",
                    callee,
                    argc,
                    if argc == 1 { "" } else { "s" }
                ),
            });
        }
    }
}

fn collect_logic_facts(logic: &LogicStmt) -> LogicFacts {
    let mut facts = LogicFacts::default();
    let binding_names: HashSet<String> = logic.bindings.iter().map(|(n, _)| n.clone()).collect();
    for (_, expr) in &logic.bindings {
        collect_expr_facts(expr, &mut facts.reads, &mut facts.calls);
    }
    collect_expr_facts(&logic.value, &mut facts.reads, &mut facts.calls);
    // Reads that reference let-bound names are local, not field reads — they
    // must not reach the purity proof. They are MOVED to `local_reads` rather
    // than dropped, so `verify_rule` can still check the field they name.
    let local: Vec<Vec<String>> = facts
        .reads
        .iter()
        .filter(|path| path.first().map_or(false, |name| binding_names.contains(name)))
        .cloned()
        .collect();
    for path in local {
        facts.reads.remove(&path);
        facts.local_reads.insert(path);
    }
    facts
}

fn collect_expr_facts(
    expr: &Expr,
    reads: &mut HashSet<Vec<String>>,
    calls: &mut HashSet<Vec<String>>,
) {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) => {}
        Expr::If(cond, then_e, else_e) => {
            collect_expr_facts(cond, reads, calls);
            collect_expr_facts(then_e, reads, calls);
            collect_expr_facts(else_e, reads, calls);
        }
        Expr::Not(inner) | Expr::Neg(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        Expr::Ident(_) | Expr::Field(_, _) => {
            if let Some(path) = expr_to_path(expr) {
                reads.insert(path);
            }
        }
        Expr::Binary(_, l, r) => {
            collect_expr_facts(l, reads, calls);
            collect_expr_facts(r, reads, calls);
        }
        Expr::Call(name, args) => {
            calls.insert(vec![name.clone()]);
            for arg in args {
                collect_expr_facts(arg, reads, calls);
            }
        }
        Expr::Fold(collection, initial, acc_name, item_name, body) => {
            collect_expr_facts(collection, reads, calls);
            collect_expr_facts(initial, reads, calls);
            let mut inner_reads = HashSet::new();
            let mut inner_calls = HashSet::new();
            collect_expr_facts(body, &mut inner_reads, &mut inner_calls);
            calls.extend(inner_calls);
            for path in inner_reads {
                if path.first().map(|s| s.as_str()) != Some(acc_name.as_str())
                    && path.first().map(|s| s.as_str()) != Some(item_name.as_str())
                {
                    reads.insert(path);
                }
            }
        }
        Expr::Quantifier(_, collection, var_name, predicate) => {
            collect_expr_facts(collection, reads, calls);
            // Predicate reads are scoped to the lambda variable — filter them out
            let mut inner_reads = HashSet::new();
            let mut inner_calls = HashSet::new();
            collect_expr_facts(predicate, &mut inner_reads, &mut inner_calls);
            calls.extend(inner_calls);
            for path in inner_reads {
                if path.first().map(|s| s.as_str()) != Some(var_name.as_str()) {
                    reads.insert(path);
                }
            }
        }
        Expr::Map(collection, var_name, body)
        | Expr::Filter(collection, var_name, body) => {
            // Same purity structure as Quantifier: the lambda variable shadows
            // any reads scoped to it. Reads outside the lambda scope propagate.
            collect_expr_facts(collection, reads, calls);
            let mut inner_reads = HashSet::new();
            let mut inner_calls = HashSet::new();
            collect_expr_facts(body, &mut inner_reads, &mut inner_calls);
            calls.extend(inner_calls);
            for path in inner_reads {
                if path.first().map(|s| s.as_str()) != Some(var_name.as_str()) {
                    reads.insert(path);
                }
            }
        }
        Expr::Ok(inner) | Expr::Err(inner) => {
            // Pure pass-through: the constructor adds no reads or calls of its
            // own, so the inner expression's facts are the whole story.
            collect_expr_facts(inner, reads, calls);
        }
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            // Target reads propagate. Each arm's reads propagate with its
            // bound variable scoped out — same machinery as Quantifier, applied
            // twice (once per arm).
            collect_expr_facts(target, reads, calls);
            for (var_name, body) in [(ok_var, ok_body), (err_var, err_body)] {
                let mut inner_reads = HashSet::new();
                let mut inner_calls = HashSet::new();
                collect_expr_facts(body, &mut inner_reads, &mut inner_calls);
                calls.extend(inner_calls);
                for path in inner_reads {
                    if path.first().map(|s| s.as_str()) != Some(var_name.as_str()) {
                        reads.insert(path);
                    }
                }
            }
        }
        Expr::Record(_, fields) => {
            // Record construction is a pass-through for facts: each field's
            // expression contributes its own reads and calls. The constructor
            // itself adds nothing.
            for (_, field_expr) in fields {
                collect_expr_facts(field_expr, reads, calls);
            }
        }
        Expr::Concat(args) => {
            // Same pass-through: concat adds no reads/calls of its own.
            for arg in args {
                collect_expr_facts(arg, reads, calls);
            }
        }
        // Phase 9 slice 1: a resource read contributes the resource name
        // to the rule's `reads:` purity facts. The author MUST list the
        // resource name in `proofs.purity.reads` (e.g., `reads: [config]`)
        // for the rule to verify — same discipline as field reads.
        Expr::Read(name) => {
            reads.insert(vec![name.clone()]);
        }
        // Slice entropy-1: a draw contributes the entropy name to the rule's
        // `reads:` purity facts — the author MUST list it (e.g. `reads:
        // [nonce]`), and `check_purity` reports `missing: [nonce]` /
        // `extra: [nonce]` unchanged, so every non-deterministic rule is
        // greppable in its proof block. Same shape as a resource name.
        Expr::Random(name) => {
            reads.insert(vec![name.clone()]);
        }
        // Phase 11 slice 1: a fetch contributes the connection name to
        // the rule's `reads:` facts (same single-segment shape as
        // resources). The request bytes expression is also walked so any
        // field accesses or nested reads inside the request body are
        // captured too.
        Expr::Fetch(name, req) => {
            reads.insert(vec![name.clone()]);
            collect_expr_facts(req, reads, calls);
        }
        // Phase 12 (json_escape): pure pass-through. The transform is
        // computed in-process from the inner expression's bytes — no
        // syscalls, no fresh reads. The inner expression's facts ARE the
        // facts.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // Phase 12 (parse_int): pure pass-through. The transform itself
        // makes no syscalls; the inner expression's facts are the facts.
        Expr::ParseInt(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `now_unix()` reads the system clock — a non-deterministic external
        // source. Surface it as a synthetic read of the name `now` so the
        // rule's declared `reads:` proof must list `now` (auditors grep
        // `reads:` to find every rule that touches the wall clock).
        Expr::NowUnix => {
            reads.insert(vec!["now".to_string()]);
        }
        // `starts_with(haystack, needle)` — pure: the comparison itself adds
        // no synthetic name (unlike NowUnix's `now`). Each child contributes
        // its own facts.
        Expr::StartsWith(h, n) => {
            collect_expr_facts(h, reads, calls);
            collect_expr_facts(n, reads, calls);
        }
        // `contains(haystack, needle)` — pure, same shape as StartsWith:
        // the substring test itself produces no synthetic read; each child
        // contributes its own facts.
        Expr::Contains(h, n) => {
            collect_expr_facts(h, reads, calls);
            collect_expr_facts(n, reads, calls);
        }
        // `ends_with(haystack, needle)` — pure, same shape as StartsWith /
        // Contains: each child contributes its own facts.
        Expr::EndsWith(h, n) => {
            collect_expr_facts(h, reads, calls);
            collect_expr_facts(n, reads, calls);
        }
        // `length(<text_expr>)` — pure pass-through. The byte count itself
        // adds no synthetic read; the inner expression's facts are the facts.
        Expr::Length(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `abs(<number_expr>)` — pure pass-through. The absolute value adds
        // no synthetic read; the inner expression's facts are the facts.
        Expr::Abs(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `le32(n)` / `le64(n)` — pure: a number→bytes view of the inner.
        Expr::Le32(inner) | Expr::Le64(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `arena_scope(inner)` — pure: the reclaim boundary adds no facts;
        // inner contributes its own reads/calls.
        Expr::ArenaScope(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `abort_if(inner)` — a primitive, not a call: the gate itself adds
        // no synthetic read or call fact (mirror of arena_scope); inner
        // contributes its own reads/calls. This is what lets the
        // self-source's `elf_program_src` declare only its REAL callees.
        Expr::AbortIf(inner) => {
            collect_expr_facts(inner, reads, calls);
        }
        // `min(a, b)` / `max(a, b)` — pure: branch-free scalar comparison
        // adds no synthetic read; each child contributes its own facts.
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => {
            collect_expr_facts(l, reads, calls);
            collect_expr_facts(r, reads, calls);
        }
        // `substring(text, start, end)` — pure pass-through: each child
        // contributes its own facts (e.g. `text` might be `read(buf)`).
        Expr::Substring(t, s, e) => {
            collect_expr_facts(t, reads, calls);
            collect_expr_facts(s, reads, calls);
            collect_expr_facts(e, reads, calls);
        }
        // `byte_at(text, index)` — pure pass-through: each child contributes
        // its own facts.
        Expr::ByteAt(t, i) => {
            collect_expr_facts(t, reads, calls);
            collect_expr_facts(i, reads, calls);
        }
        // `fold_bytes(text, init, acc, byte, idx => body)` — purity shape
        // mirrors Fold: text + init reads propagate as-is; body reads are
        // filtered by the three lambda-bound names (acc, byte, idx) so any
        // path like `acc.foo` or `idx.bar` inside the body does NOT escape
        // as a stale `reads:` entry. Same machinery as Fold, three names
        // instead of two.
        Expr::FoldBytes(text, initial, acc_name, byte_name, idx_name, body) => {
            collect_expr_facts(text, reads, calls);
            collect_expr_facts(initial, reads, calls);
            let mut inner_reads = HashSet::new();
            let mut inner_calls = HashSet::new();
            collect_expr_facts(body, &mut inner_reads, &mut inner_calls);
            calls.extend(inner_calls);
            for path in inner_reads {
                let base = path.first().map(|s| s.as_str());
                if base != Some(acc_name.as_str())
                    && base != Some(byte_name.as_str())
                    && base != Some(idx_name.as_str())
                {
                    reads.insert(path);
                }
            }
        }
        // Phase A slice 2: variant construction is a pass-through for facts —
        // each field assignment's expression contributes its own reads/calls.
        // Same shape as Record.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, field_expr) in fields {
                collect_expr_facts(field_expr, reads, calls);
            }
        }
        // Phase A slice 3: pattern match — scrutinee reads propagate. Each
        // arm's reads propagate with that arm's positional binders scoped
        // out (same machinery as MatchResult, generalized to N arms with
        // N positional binders; wildcards `None` cannot shadow anything).
        // Auditors find these locally-bound names by reading the arm
        // header; they are NOT external reads.
        Expr::MatchVariant(scrutinee, arms) => {
            collect_expr_facts(scrutinee, reads, calls);
            for a in arms {
                let bound: HashSet<&str> = a
                    .binders
                    .iter()
                    .filter_map(|b| b.as_deref())
                    .collect();
                let mut inner_reads = HashSet::new();
                let mut inner_calls = HashSet::new();
                collect_expr_facts(&a.body, &mut inner_reads, &mut inner_calls);
                calls.extend(inner_calls);
                for path in inner_reads {
                    let base = path.first().map(|s| s.as_str()).unwrap_or("");
                    if !bound.contains(base) {
                        reads.insert(path);
                    }
                }
            }
        }
    }
}

fn expr_to_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Ident(name) => Some(vec![name.clone()]),
        Expr::Field(base, field) => {
            let mut segs = expr_to_path(base)?;
            segs.push(field.clone());
            Some(segs)
        }
        _ => None,
    }
}

fn validate_read_path(
    path: &[String],
    rule: &Rule,
    input_concept: Option<&Concept>,
    context_concept: Option<&Concept>,
    all_resources: &HashSet<String>,
    all_connections: &HashSet<String>,
    all_entropies: &HashSet<String>,
) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let base = &path[0];
    // Accept both input name and context name (if present).
    let is_input = base == &rule.input_name;
    let is_context = rule.context_name.as_ref().map_or(false, |cn| base == cn);
    // Phase 9 slice 1: also accept top-level resource names. A resource
    // read is `read(name)` which collects to path == [name] (length 1, no
    // field access). The verify_program pass already cross-checks that
    // the resource exists; here we just permit the base.
    let is_resource = path.len() == 1 && all_resources.contains(base);
    if is_resource {
        return None;
    }
    // Phase 11 slice 1: also accept top-level connection names. A fetch
    // contributes the connection name to `reads:` exactly the way a
    // resource read does — same path shape ([name], length 1, no field).
    let is_connection = path.len() == 1 && all_connections.contains(base);
    if is_connection {
        return None;
    }
    // Slice entropy-1: also accept top-level entropy names. A draw
    // contributes the entropy name to `reads:` exactly the way a resource
    // read does — same path shape ([name], length 1, no field).
    let is_entropy = path.len() == 1 && all_entropies.contains(base);
    if is_entropy {
        return None;
    }
    // `now_unix()` synthesises a `reads: [now]` entry. Accept the synthetic
    // name `now` as a valid base (length 1, no field access) — same audit
    // shape as a resource or connection name.
    if path.len() == 1 && base == "now" {
        return None;
    }
    // `state.field` accesses for service mutable state. The base `state`
    // is a reserved synthetic scope — the service verification cross-checks
    // that each referenced field actually exists in the service's
    // `state_fields` declaration. Accepted as length-2 path only.
    if base == "state" && path.len() == 2 {
        return None;
    }
    if !is_input && !is_context {
        let scope = if let Some(cn) = &rule.context_name {
            format!("'{}' and '{}'", rule.input_name, cn)
        } else {
            format!("'{}'", rule.input_name)
        };
        return Some(format!(
            "unknown binding '{}' in path '{}'; only {} in scope",
            base,
            path.join("."),
            scope
        ));
    }
    if path.len() >= 2 {
        // Both scopes get the SAME check, from the same helper. The `context:`
        // binding used to be skipped here with the comment "we don't have the
        // concept here to validate field names" — which was simply false:
        // `Rule::context_ty` names it and `verify_rule` resolves it exactly as
        // it resolves the input's. The consequence was that `p.nosuchfield` on
        // a context concept verified clean and was then refused by the native
        // emitter ("unknown field 'nosuchfield' in native codegen") — a
        // verifier certifying a program its own backend rejects.
        let scope_concept = if is_input {
            input_concept
        } else if is_context {
            context_concept
        } else {
            None
        };
        if let Some(c) = scope_concept {
            if let Some(msg) = concept_field_error(c, &path[1], path) {
                return Some(msg);
            }
        }
    }
    None
}

/// The ONE field-existence check: a `.field` access on a value whose type is a
/// named concept must name a field that concept declares.
///
/// Extracted so that every scope which knows its base's concept — the input,
/// the `context:` binding, and a `let` bound to a record — produces the same
/// diagnostic from the same comparison. Until this existed only the input had
/// the check at all, and the other two accepted any field name whatsoever.
fn concept_field_error(c: &Concept, field_name: &str, path: &[String]) -> Option<String> {
    if c.fields.iter().any(|f| f.name == field_name) {
        return None;
    }
    Some(format!(
        "concept '{}' has no field '{}' (accessed via '{}')",
        c.name,
        field_name,
        path.join(".")
    ))
}

/// Resolve a type to the RECORD concept it denotes, or None.
///
/// Returns None for a non-named type, an unknown concept name, and — the case
/// worth stating — a SUM-TYPE concept (`variants:`, empty `fields:`). A
/// sum-type value is consumed by `match`, never by field access, and the
/// arm-binder scope is not tracked by the walks that use this. Reporting "has
/// no field" for every access on one would be a new refusal class rather than
/// the mirror of the input-field check, so those stay silent.
fn record_concept_of<'a>(
    ty: &Type,
    all_concepts: &HashMap<String, &'a Concept>,
) -> Option<&'a Concept> {
    match ty {
        Type::Named(n) => all_concepts
            .get(n)
            .copied()
            .filter(|c| c.variants.is_empty()),
        _ => None,
    }
}

/// Every binding in a rule's scope whose type is a known RECORD concept:
/// the `context:` binding, and each `let` the pass can type.
///
/// This is what `infer_expr_type` consults for a `.field` access whose base is
/// not the input, and what `verify_rule` consults to check that such a field
/// exists. Cases deliberately left OUT — inference is best-effort and silence
/// is the only safe answer when a binding's type is not known:
///
///   * a `let` whose RHS type is not inferable (`match_result`, `map` /
///     `filter` / `fold`, a lambda-bound var) — `infer_expr_type` returns None
///     and the binding is absent from the map, so nothing is checked;
///   * a `let` whose type is scalar (number / text / bool) — `.field` on it is
///     meaningless, but flagging it is a new refusal class, not this check;
///   * lambda binders and `match` / `match_result` arm binders — their scope is
///     local to a body expression, and `collect_expr_facts` already filters
///     their paths out before anything here can see them. That filtering is
///     what the "conservative on lambda/let-bound vars" posture protects, and
///     it is untouched: this map only ever gains TOP-LEVEL let names;
///   * a binding that SHADOWS the input or context name — see the guard below.
fn collect_binding_concepts<'a>(
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&'a Concept>,
    all_concepts: &HashMap<String, &'a Concept>,
) -> HashMap<String, &'a Concept> {
    let mut env: HashMap<String, &'a Concept> = HashMap::new();

    if let (Some(cn), Some(cty)) = (&rule.context_name, &rule.context_ty) {
        if let Some(c) = record_concept_of(cty, all_concepts) {
            env.insert(cn.clone(), c);
        }
    }

    // Source order, so a later binding can see an earlier one.
    for (name, rhs) in &rule.logic.bindings {
        // A `let` that shadows the input or the context name is dropped from
        // the map entirely. `collect_logic_facts` already removes such paths
        // from `reads`, so the input check does not fire on them either, and
        // deciding which of the two the author meant is precisely the kind of
        // guess the compiler must not make.
        if name == &rule.input_name || rule.context_name.as_deref() == Some(name.as_str()) {
            env.remove(name.as_str());
            continue;
        }

        // A bare alias (`let r2 = r1`, or `let r = p`). Resolved here rather
        // than by teaching `infer_expr_type`'s `Expr::Ident` arm about
        // bindings: that arm feeds every `Ident` position in the bidirectional
        // check, so widening it would add strictness far beyond a `.field`
        // access. This adds none — it only propagates a concept the map
        // already holds.
        if let Expr::Ident(src) = rhs {
            let resolved = if src == &rule.input_name {
                input_concept.filter(|c| c.variants.is_empty())
            } else {
                env.get(src.as_str()).copied()
            };
            match resolved {
                Some(c) => env.insert(name.clone(), c),
                None => env.remove(name.as_str()),
            };
            continue;
        }

        match infer_expr_type(rhs, rule, all_rules, input_concept, &env)
            .and_then(|t| record_concept_of(&t, all_concepts))
        {
            Some(c) => env.insert(name.clone(), c),
            None => env.remove(name.as_str()),
        };
    }

    env
}

/// Collect every identifier that's bound by a lambda-shaped construct
/// anywhere in the expression tree. Drives the diagnostic hint in
/// `check_purity` when an extra `reads:` entry's base ident matches.
///
/// "Lambda-bound" here means: variables whose scope is local to a body
/// expression — quantifier var (`all` / `any`), fold's acc + element,
/// map/filter element, match_result's ok_var + err_var. Field
/// accesses like `var.field` inside that body do NOT belong in
/// `reads:`; the verifier's fact-collection (`collect_expr_facts`)
/// already filters them out, so a stale entry in `reads:` is the
/// model's mistake.
fn collect_lambda_bound_names(expr: &Expr) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    fn walk(e: &Expr, out: &mut std::collections::HashSet<String>) {
        match e {
            Expr::Quantifier(_, coll, var, body) => {
                out.insert(var.clone());
                walk(coll, out);
                walk(body, out);
            }
            Expr::Map(coll, var, body) | Expr::Filter(coll, var, body) => {
                out.insert(var.clone());
                walk(coll, out);
                walk(body, out);
            }
            Expr::Fold(coll, init, acc, item, body) => {
                out.insert(acc.clone());
                out.insert(item.clone());
                walk(coll, out);
                walk(init, out);
                walk(body, out);
            }
            Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
                out.insert(ok_var.clone());
                out.insert(err_var.clone());
                walk(target, out);
                walk(ok_body, out);
                walk(err_body, out);
            }
            Expr::Binary(_, l, r) => { walk(l, out); walk(r, out); }
            Expr::If(c, t, el) => { walk(c, out); walk(t, out); walk(el, out); }
            Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i) | Expr::Length(i)
            | Expr::ParseInt(i) | Expr::JsonEscape(i) | Expr::Ok(i) | Expr::Err(i)
            | Expr::Le32(i) | Expr::Le64(i) | Expr::ArenaScope(i) | Expr::AbortIf(i) => {
                walk(i, out);
            }
            Expr::Min(a, b) | Expr::Max(a, b) | Expr::BitAnd(a, b) | Expr::BitOr(a, b) | Expr::BitXor(a, b) | Expr::Shl(a, b) | Expr::Shr(a, b) | Expr::StartsWith(a, b)
            | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
                walk(a, out); walk(b, out);
            }
            Expr::Substring(t, s, e) => {
                walk(t, out); walk(s, out); walk(e, out);
            }
            Expr::ByteAt(t, i) => {
                walk(t, out); walk(i, out);
            }
            Expr::FoldBytes(t, init, acc, byte, idx, body) => {
                out.insert(acc.clone());
                out.insert(byte.clone());
                out.insert(idx.clone());
                walk(t, out);
                walk(init, out);
                walk(body, out);
            }
            Expr::Call(_, args) | Expr::Concat(args) => {
                for a in args { walk(a, out); }
            }
            Expr::Record(_, fields) => {
                for (_, v) in fields { walk(v, out); }
            }
            // Phase A slice 2: variant construction — same shape as Record.
            Expr::VariantConstruct(_, _, fields) => {
                for (_, v) in fields { walk(v, out); }
            }
            // Phase A slice 3: pattern match — each arm's positional
            // binders (`Some(name)`) are lambda-bound in that arm's body
            // scope. Add every non-wildcard binder to the lambda-bound
            // set, then recurse into scrutinee + each arm body. Auditors
            // know these names are NOT external reads — they're the
            // payload destructuring slots.
            Expr::MatchVariant(scrutinee, arms) => {
                walk(scrutinee, out);
                for a in arms {
                    for binder in &a.binders {
                        if let Some(name) = binder {
                            out.insert(name.clone());
                        }
                    }
                    walk(&a.body, out);
                }
            }
            Expr::Fetch(_, request) => walk(request, out),
            Expr::BitAnd(a, b) | Expr::BitOr(a, b) | Expr::BitXor(a, b)
            | Expr::Shl(a, b) | Expr::Shr(a, b) => { walk(a, out); walk(b, out); }
            Expr::BitNot(i) => walk(i, out),
            // Leaves
            Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Field(_, _) | Expr::Ident(_)
            | Expr::Read(_) | Expr::Random(_) | Expr::NowUnix => {}
        }
    }
    walk(expr, &mut out);
    out
}

fn check_purity(rule: &Rule, facts: &LogicFacts, errors: &mut Vec<VerifyError>) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    let declared_reads = path_list_to_set(&rule.proofs.purity.reads);
    let declared_calls = path_list_to_set(&rule.proofs.purity.calls);

    if declared_reads != facts.reads {
        let missing: Vec<String> = facts
            .reads
            .difference(&declared_reads)
            .map(|p| p.join("."))
            .collect();
        let extra_paths: Vec<Vec<String>> = declared_reads
            .difference(&facts.reads)
            .cloned()
            .collect();
        let extra: Vec<String> = extra_paths.iter().map(|p| p.join(".")).collect();
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("missing: [{}]", missing.join(", ")));
        }
        if !extra.is_empty() {
            parts.push(format!("extra: [{}]", extra.join(", ")));
        }
        // Diagnostic hint: when an `extra` path's base identifier is
        // bound by a lambda inside the rule body (quantifier var,
        // map/filter/fold var, fold acc, match_result ok_var/err_var),
        // the most likely cause is the model emitted `reads: [..., var.f]`
        // for a field accessed inside the lambda body. Lambda-bound
        // accesses don't count as `reads:` (the verifier's
        // `collect_expr_facts` already filters them out). Surface that
        // explicitly so a generator that hit this trap can correct on
        // the first round instead of guessing.
        let lambda_bound = collect_lambda_bound_names(&rule.logic.value);
        let lambda_extras: Vec<&str> = extra_paths
            .iter()
            .filter_map(|p| p.first().map(String::as_str))
            .filter(|name| lambda_bound.contains(*name))
            .collect();
        let mut message = format!("declared reads do not match logic; {}", parts.join(", "));
        if !lambda_extras.is_empty() {
            // Dedupe + stable order for a tight message.
            let mut names: Vec<&str> = lambda_extras;
            names.sort();
            names.dedup();
            message.push_str(&format!(
                "\n  hint: '{}' {} lambda-bound by a quantifier/fold/map/filter/match_result \
                 — fields accessed through such a variable do NOT belong in `reads:`. \
                 Only fields of the rule's input concept (or top-level resource names) appear there.",
                names.join("', '"),
                if names.len() == 1 { "is" } else { "are" },
            ));
        }
        errors.push(VerifyError {
            context: ctx("purity.reads"),
            message,
        });
    }

    if declared_calls != facts.calls {
        let missing: Vec<String> = facts
            .calls
            .difference(&declared_calls)
            .map(|p| p.join("."))
            .collect();
        let extra: Vec<String> = declared_calls
            .difference(&facts.calls)
            .map(|p| p.join("."))
            .collect();
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("missing: [{}]", missing.join(", ")));
        }
        if !extra.is_empty() {
            parts.push(format!("extra: [{}]", extra.join(", ")));
        }
        errors.push(VerifyError {
            context: ctx("purity.calls"),
            message: format!("declared calls do not match logic; {}", parts.join(", ")),
        });
    }

}

fn check_termination(
    rule: &Rule,
    concepts: &HashMap<String, &Concept>,
    group_concept_owner: &HashMap<String, String>,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    match rule.proofs.termination.bound {
        Some(declared) => {
            let actual = count_operations(&rule.logic.value) as i64;
            if declared < actual {
                errors.push(VerifyError {
                    context: ctx("termination.bound"),
                    message: format!(
                        "declared bound {} is less than actual operation count {}",
                        declared, actual
                    ),
                });
            }
        }
        None => {
            errors.push(VerifyError {
                context: ctx("termination"),
                message: "termination requires a 'bound:' value".into(),
            });
        }
    }

    if let Some(ref structural_param) = rule.proofs.termination.structural {
        check_structural_recursion(rule, structural_param, concepts, group_concept_owner, errors);
    }

    if let Some(ref decreasing_field) = rule.proofs.termination.decreasing {
        check_decreasing_recursion(rule, decreasing_field, concepts, errors);
    }

    if let Some(ref increasing_field) = rule.proofs.termination.increasing {
        check_increasing_recursion(rule, increasing_field, concepts, errors);
    }
}

fn check_structural_recursion(
    rule: &Rule,
    structural_param: &str,
    concepts: &HashMap<String, &Concept>,
    group_concept_owner: &HashMap<String, String>,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    if structural_param != rule.input_name {
        errors.push(VerifyError {
            context: ctx("termination.structural"),
            message: format!(
                "structural recursion parameter '{}' must be the rule's input name '{}' (Phase C slice 1 scope)",
                structural_param, rule.input_name
            ),
        });
        return;
    }
    let concept_name = match &rule.input_ty {
        Type::Named(n) => n.as_str(),
        _ => {
            errors.push(VerifyError {
                context: ctx("termination.structural"),
                message: "structural recursion requires the input to be a named concept".into(),
            });
            return;
        }
    };
    if !group_concept_owner.contains_key(concept_name) {
        errors.push(VerifyError {
            context: ctx("termination.structural"),
            message: format!(
                "structural recursion requires concept '{}' to be inside a concept_group (Phase C slice 1 scope)",
                concept_name
            ),
        });
        return;
    }
    let concept = match concepts.get(concept_name) {
        Some(c) => *c,
        None => return,
    };
    let self_ref_fields: HashSet<String> = concept.variants.iter()
        .flat_map(|v| v.fields.iter()
            .filter(|f| matches!(&f.ty, Type::Named(n) if n == concept_name))
            .map(|f| f.name.clone()))
        .collect();

    let mut call_sites: Vec<String> = Vec::new();
    collect_recursive_call_args(&rule.logic.value, &rule.name, &mut call_sites);

    for arg_desc in &call_sites {
        if !self_ref_fields.contains(arg_desc) {
            errors.push(VerifyError {
                context: ctx("termination.structural"),
                message: format!(
                    "recursive call to '{}' passes argument '{}' which is not a structural \
                     subfield of concept '{}'. Structural recursion requires every recursive \
                     call to pass a binder that corresponds to a self-referential variant field \
                     (one of: {:?}).",
                    rule.name, arg_desc, concept_name,
                    self_ref_fields.iter().collect::<Vec<_>>()
                ),
            });
        }
    }
}

fn collect_recursive_call_args(expr: &Expr, rule_name: &str, out: &mut Vec<String>) {
    match expr {
        Expr::Call(name, args) if name == rule_name => {
            if let Some(arg) = args.first() {
                match arg {
                    Expr::Ident(n) => out.push(n.clone()),
                    _ => out.push(format!("<non-ident: {:?}>", arg)),
                }
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) | Expr::Read(_) | Expr::Random(_) | Expr::NowUnix => {}
        Expr::Field(b, _) => collect_recursive_call_args(b, rule_name, out),
        Expr::Binary(_, l, r) => { collect_recursive_call_args(l, rule_name, out); collect_recursive_call_args(r, rule_name, out); }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i)
        | Expr::Abs(i) | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) | Expr::BitNot(i)
        | Expr::Le32(i) | Expr::Le64(i) | Expr::ArenaScope(i) | Expr::AbortIf(i) => collect_recursive_call_args(i, rule_name, out),
        Expr::If(c, t, e) => { collect_recursive_call_args(c, rule_name, out); collect_recursive_call_args(t, rule_name, out); collect_recursive_call_args(e, rule_name, out); }
        Expr::Call(_, args) | Expr::Concat(args) => { for a in args { collect_recursive_call_args(a, rule_name, out); } }
        Expr::Quantifier(_, c, _, body) => { collect_recursive_call_args(c, rule_name, out); collect_recursive_call_args(body, rule_name, out); }
        Expr::Fold(c, init, _, _, body) => { collect_recursive_call_args(c, rule_name, out); collect_recursive_call_args(init, rule_name, out); collect_recursive_call_args(body, rule_name, out); }
        Expr::FoldBytes(t, init, _, _, _, body) => { collect_recursive_call_args(t, rule_name, out); collect_recursive_call_args(init, rule_name, out); collect_recursive_call_args(body, rule_name, out); }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => { collect_recursive_call_args(c, rule_name, out); collect_recursive_call_args(body, rule_name, out); }
        Expr::MatchResult(t, _, ok, _, err) => { collect_recursive_call_args(t, rule_name, out); collect_recursive_call_args(ok, rule_name, out); collect_recursive_call_args(err, rule_name, out); }
        Expr::Record(_, fields) | Expr::VariantConstruct(_, _, fields) => { for (_, e) in fields { collect_recursive_call_args(e, rule_name, out); } }
        Expr::MatchVariant(scrut, arms) => {
            collect_recursive_call_args(scrut, rule_name, out);
            for a in arms { collect_recursive_call_args(&a.body, rule_name, out); }
        }
        Expr::Fetch(_, req) => collect_recursive_call_args(req, rule_name, out),
        Expr::BitAnd(a, b) | Expr::BitOr(a, b) | Expr::BitXor(a, b)
        | Expr::Shl(a, b) | Expr::Shr(a, b) => { collect_recursive_call_args(a, rule_name, out); collect_recursive_call_args(b, rule_name, out); }
        Expr::StartsWith(h, n) | Expr::Contains(h, n) | Expr::EndsWith(h, n)
        | Expr::Min(h, n) | Expr::Max(h, n) | Expr::ByteAt(h, n) => { collect_recursive_call_args(h, rule_name, out); collect_recursive_call_args(n, rule_name, out); }
        Expr::Substring(t, s, e) => { collect_recursive_call_args(t, rule_name, out); collect_recursive_call_args(s, rule_name, out); collect_recursive_call_args(e, rule_name, out); }
    }
}

fn check_decreasing_recursion(
    rule: &Rule,
    field_name: &str,
    concepts: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);
    let concept_name = match &rule.input_ty {
        Type::Named(n) => n.as_str(),
        _ => {
            errors.push(VerifyError {
                context: ctx("termination.decreasing"),
                message: "decreasing requires the input to be a named concept".into(),
            });
            return;
        }
    };
    let concept = match concepts.get(concept_name) {
        Some(c) => *c,
        None => return,
    };
    let field = concept.fields.iter().find(|f| f.name == field_name);
    match field {
        Some(f) => {
            if !matches!(f.ty, Type::Number) {
                errors.push(VerifyError {
                    context: ctx("termination.decreasing"),
                    message: format!(
                        "field '{}' must be Number-typed for decreasing proof (got {:?})",
                        field_name, f.ty
                    ),
                });
                return;
            }
            if f.range.is_none() {
                errors.push(VerifyError {
                    context: ctx("termination.decreasing"),
                    message: format!(
                        "field '{}' must have a declared range [min, max] for decreasing proof",
                        field_name
                    ),
                });
                return;
            }
        }
        None => {
            errors.push(VerifyError {
                context: ctx("termination.decreasing"),
                message: format!(
                    "field '{}' not found on concept '{}'",
                    field_name, concept_name
                ),
            });
            return;
        }
    }
    let mut call_args: Vec<(String, Expr)> = Vec::new();
    collect_recursive_call_record_args(&rule.logic.value, &rule.name, &mut call_args);
    for (callee, arg_expr) in &call_args {
        if callee != &rule.name {
            continue;
        }
        if let Expr::Record(_, fields) = arg_expr {
            let field_expr = fields.iter().find(|(n, _)| n == field_name).map(|(_, e)| e);
            match field_expr {
                Some(e) if is_decreasing_by_positive(e, &rule.input_name, field_name) => {}
                Some(_) => {
                    errors.push(VerifyError {
                        context: ctx("termination.decreasing"),
                        message: format!(
                            "recursive call to '{}' must pass '{}.{} - k' (k > 0) for field '{}'; \
                             the expression does not match the decreasing pattern",
                            rule.name, rule.input_name, field_name, field_name
                        ),
                    });
                }
                None => {
                    errors.push(VerifyError {
                        context: ctx("termination.decreasing"),
                        message: format!(
                            "recursive call to '{}' passes a Record without field '{}'",
                            rule.name, field_name
                        ),
                    });
                }
            }
        } else {
            errors.push(VerifyError {
                context: ctx("termination.decreasing"),
                message: format!(
                    "recursive call to '{}' must pass a Record constructor (got {:?})",
                    rule.name, arg_expr
                ),
            });
        }
    }
}

fn is_decreasing_by_positive(expr: &Expr, input_name: &str, field_name: &str) -> bool {
    match expr {
        Expr::Binary(BinOp::Sub, left, right) => {
            let left_is_field = matches!(left.as_ref(),
                Expr::Field(base, fname)
                if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                   && fname == field_name
            );
            let right_is_positive = matches!(right.as_ref(), Expr::Number(k) if *k > 0);
            left_is_field && right_is_positive
        }
        _ => false,
    }
}

fn check_increasing_recursion(
    rule: &Rule,
    field_name: &str,
    concepts: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);
    let concept_name = match &rule.input_ty {
        Type::Named(n) => n.as_str(),
        _ => {
            errors.push(VerifyError {
                context: ctx("termination.increasing"),
                message: "increasing requires the input to be a named concept".into(),
            });
            return;
        }
    };
    let concept = match concepts.get(concept_name) {
        Some(c) => *c,
        None => return,
    };
    let field = concept.fields.iter().find(|f| f.name == field_name);
    match field {
        Some(f) => {
            if !matches!(f.ty, Type::Number) {
                errors.push(VerifyError {
                    context: ctx("termination.increasing"),
                    message: format!(
                        "field '{}' must be Number-typed for increasing proof (got {:?})",
                        field_name, f.ty
                    ),
                });
                return;
            }
            if f.range.is_none() {
                errors.push(VerifyError {
                    context: ctx("termination.increasing"),
                    message: format!(
                        "field '{}' must have a declared range [min, max] for increasing proof",
                        field_name
                    ),
                });
                return;
            }
        }
        None => {
            errors.push(VerifyError {
                context: ctx("termination.increasing"),
                message: format!("field '{}' not found on concept '{}'", field_name, concept_name),
            });
            return;
        }
    }
    let mut call_args: Vec<(String, Expr)> = Vec::new();
    collect_recursive_call_record_args(&rule.logic.value, &rule.name, &mut call_args);
    for (callee, arg_expr) in &call_args {
        if callee != &rule.name { continue; }
        if let Expr::Record(_, fields) = arg_expr {
            let field_expr = fields.iter().find(|(n, _)| n == field_name).map(|(_, e)| e);
            match field_expr {
                Some(e) if is_increasing_by_positive(e, &rule.input_name, field_name) => {}
                Some(_) => {
                    errors.push(VerifyError {
                        context: ctx("termination.increasing"),
                        message: format!(
                            "recursive call to '{}' must pass '{}.{} + k' (k > 0) for field '{}'",
                            rule.name, rule.input_name, field_name, field_name
                        ),
                    });
                }
                None => {
                    errors.push(VerifyError {
                        context: ctx("termination.increasing"),
                        message: format!(
                            "recursive call to '{}' passes a Record without field '{}'",
                            rule.name, field_name
                        ),
                    });
                }
            }
        }
    }
}

fn is_increasing_by_positive(expr: &Expr, input_name: &str, field_name: &str) -> bool {
    match expr {
        Expr::Binary(BinOp::Add, left, right) => {
            let left_is_field = matches!(left.as_ref(),
                Expr::Field(base, fname)
                if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                   && fname == field_name
            );
            let right_is_positive = matches!(right.as_ref(), Expr::Number(k) if *k > 0);
            if left_is_field && right_is_positive { return true; }
            let right_is_field = matches!(right.as_ref(),
                Expr::Field(base, fname)
                if matches!(base.as_ref(), Expr::Ident(n) if n == input_name)
                   && fname == field_name
            );
            let left_is_positive = matches!(left.as_ref(), Expr::Number(k) if *k > 0);
            right_is_field && left_is_positive
        }
        _ => false,
    }
}

fn collect_recursive_call_record_args(expr: &Expr, rule_name: &str, out: &mut Vec<(String, Expr)>) {
    match expr {
        Expr::Call(name, args) if name == rule_name => {
            if let Some(arg) = args.first() {
                out.push((name.clone(), arg.clone()));
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) | Expr::Read(_) | Expr::Random(_) | Expr::NowUnix => {}
        Expr::Field(b, _) => collect_recursive_call_record_args(b, rule_name, out),
        Expr::Binary(_, l, r) => { collect_recursive_call_record_args(l, rule_name, out); collect_recursive_call_record_args(r, rule_name, out); }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i)
        | Expr::Abs(i) | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) | Expr::BitNot(i)
        | Expr::Le32(i) | Expr::Le64(i) | Expr::ArenaScope(i) | Expr::AbortIf(i) => collect_recursive_call_record_args(i, rule_name, out),
        Expr::If(c, t, e) => { collect_recursive_call_record_args(c, rule_name, out); collect_recursive_call_record_args(t, rule_name, out); collect_recursive_call_record_args(e, rule_name, out); }
        Expr::Call(_, args) | Expr::Concat(args) => { for a in args { collect_recursive_call_record_args(a, rule_name, out); } }
        Expr::Quantifier(_, c, _, body) => { collect_recursive_call_record_args(c, rule_name, out); collect_recursive_call_record_args(body, rule_name, out); }
        Expr::Fold(c, init, _, _, body) => { collect_recursive_call_record_args(c, rule_name, out); collect_recursive_call_record_args(init, rule_name, out); collect_recursive_call_record_args(body, rule_name, out); }
        Expr::FoldBytes(t, init, _, _, _, body) => { collect_recursive_call_record_args(t, rule_name, out); collect_recursive_call_record_args(init, rule_name, out); collect_recursive_call_record_args(body, rule_name, out); }
        Expr::Map(c, _, body) | Expr::Filter(c, _, body) => { collect_recursive_call_record_args(c, rule_name, out); collect_recursive_call_record_args(body, rule_name, out); }
        Expr::MatchResult(t, _, ok, _, err) => { collect_recursive_call_record_args(t, rule_name, out); collect_recursive_call_record_args(ok, rule_name, out); collect_recursive_call_record_args(err, rule_name, out); }
        Expr::Record(_, fields) | Expr::VariantConstruct(_, _, fields) => { for (_, e) in fields { collect_recursive_call_record_args(e, rule_name, out); } }
        Expr::MatchVariant(scrut, arms) => {
            collect_recursive_call_record_args(scrut, rule_name, out);
            for a in arms { collect_recursive_call_record_args(&a.body, rule_name, out); }
        }
        Expr::Fetch(_, req) => collect_recursive_call_record_args(req, rule_name, out),
        Expr::BitAnd(a, b) | Expr::BitOr(a, b) | Expr::BitXor(a, b)
        | Expr::Shl(a, b) | Expr::Shr(a, b) => { collect_recursive_call_record_args(a, rule_name, out); collect_recursive_call_record_args(b, rule_name, out); }
        Expr::StartsWith(h, n) | Expr::Contains(h, n) | Expr::EndsWith(h, n)
        | Expr::Min(h, n) | Expr::Max(h, n) | Expr::ByteAt(h, n) => { collect_recursive_call_record_args(h, rule_name, out); collect_recursive_call_record_args(n, rule_name, out); }
        Expr::Substring(t, s, e) => { collect_recursive_call_record_args(t, rule_name, out); collect_recursive_call_record_args(s, rule_name, out); collect_recursive_call_record_args(e, rule_name, out); }
    }
}

/// Structural operation count of an expression — the reference `termination.bound`
/// is checked against. Crate-visible (not private) so
/// `two_generation_gen0_verifies_termination_bound_against_operation_count` can
/// DERIVE each probe's boundary from this function instead of hard-coding a
/// number: the pin then asserts gen0's cutoff is exactly verbosec's, arm by arm,
/// and cannot drift if a weight here ever changes.
pub(crate) fn count_operations(expr: &Expr) -> usize {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) => 0,
        Expr::If(c, t, e) => 1 + count_operations(c) + count_operations(t) + count_operations(e),
        Expr::Not(inner) | Expr::Neg(inner) => 1 + count_operations(inner),
        Expr::Field(base, _) => count_operations(base),
        Expr::Binary(_, l, r) => 1 + count_operations(l) + count_operations(r),
        Expr::Call(_, args) => 1 + args.iter().map(count_operations).sum::<usize>(),
        Expr::Quantifier(_, coll, _, pred) => 1 + count_operations(coll) + count_operations(pred),
        Expr::Fold(coll, init, _, _, body) => 1 + count_operations(coll) + count_operations(init) + count_operations(body),
        Expr::Map(coll, _, body) | Expr::Filter(coll, _, body) => 1 + count_operations(coll) + count_operations(body),
        Expr::Ok(inner) | Expr::Err(inner) => 1 + count_operations(inner),
        Expr::MatchResult(target, _, ok_body, _, err_body) => {
            // Dispatch costs 1; both arms contribute like if/then/else.
            1 + count_operations(target) + count_operations(ok_body) + count_operations(err_body)
        }
        Expr::Record(_, fields) => {
            // Construction itself is 1 op; each field expression contributes.
            1 + fields.iter().map(|(_, e)| count_operations(e)).sum::<usize>()
        }
        Expr::Concat(args) => {
            // 1 op for the concat call itself + each arg.
            1 + args.iter().map(count_operations).sum::<usize>()
        }
        // Phase 9 slice 1 stub: a file read costs one op (the syscall) and
        // has no Expr children to count.
        Expr::Read(_) => 1,
        // Slice entropy-1: a draw costs one op (the getrandom syscall) and
        // has no Expr children — same shape as Read.
        Expr::Random(_) => 1,
        // Phase 11 slice 1: a TCP fetch costs roughly one op (the
        // socket+connect+write+read syscall sequence is opaque to the
        // proof system) plus the cost of evaluating the request bytes.
        Expr::Fetch(_, req) => 1 + count_operations(req),
        // Phase 12 (json_escape): one op for the transform itself plus
        // the cost of evaluating the inner expression. Same shape as
        // Ok/Err's pass-through accounting.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => 1 + count_operations(inner),
        // Phase 12 (parse_int): same shape as JsonEscape — one op for
        // the scan/parse loop plus the inner.
        Expr::ParseInt(inner) => 1 + count_operations(inner),
        // `now_unix()` — one op (the clock_gettime syscall) and no inner
        // expression. Same shape as Read.
        Expr::NowUnix => 1,
        // `starts_with(haystack, needle)` — one op for the byte-compare
        // loop plus the cost of evaluating each child (same shape as Binary).
        Expr::StartsWith(h, n) => 1 + count_operations(h) + count_operations(n),
        // `contains(haystack, needle)` — naive substring search: one op
        // for the outer wrapper plus each child's cost. Worst-case
        // inner work (O(N*M)) is bounded by `max:` declarations on the
        // resources backing each side.
        Expr::Contains(h, n) => 1 + count_operations(h) + count_operations(n),
        // `ends_with(haystack, needle)` — same shape as StartsWith.
        Expr::EndsWith(h, n) => 1 + count_operations(h) + count_operations(n),
        // `length(<text_expr>)` — same shape as ParseInt: one op + inner cost.
        Expr::Length(inner) => 1 + count_operations(inner),
        // `abs(<number_expr>)` — same shape as Neg: one op + inner cost.
        Expr::Abs(inner) => 1 + count_operations(inner),
        // `le32(n)` / `le64(n)` — one op + inner cost (same shape as Abs).
        Expr::Le32(inner) | Expr::Le64(inner) | Expr::ArenaScope(inner) | Expr::AbortIf(inner) => 1 + count_operations(inner),
        // `min(a, b)` / `max(a, b)` — branch-free scalar; one op + each child.
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => 1 + count_operations(l) + count_operations(r),
        // `substring(text, start, end)` — one op for the slice operation
        // (bounds check + pointer arithmetic) plus the cost of each child.
        Expr::Substring(t, s, e) => 1 + count_operations(t) + count_operations(s) + count_operations(e),
        // `byte_at(text, index)` — one op (bounds check + load) plus the
        // cost of each child.
        Expr::ByteAt(t, i) => 1 + count_operations(t) + count_operations(i),
        // `fold_bytes(text, init, acc, byte, idx => body)` — one op for the
        // fold-machinery setup plus the cost of evaluating text, init, and
        // body. Same shape as Fold; the bound names don't contribute their
        // own ops.
        Expr::FoldBytes(t, init, _, _, _, body) => {
            1 + count_operations(t) + count_operations(init) + count_operations(body)
        }
        // Phase A slice 2: variant construction — 1 op for the tag + each
        // payload field's expression cost. Same shape as Record.
        Expr::VariantConstruct(_, _, fields) => {
            1 + fields.iter().map(|(_, e)| count_operations(e)).sum::<usize>()
        }
        // Phase A slice 3: pattern match — 1 op for the tag dispatch +
        // scrutinee cost + sum of each arm body's cost. Same shape as
        // MatchResult generalized to N arms.
        Expr::MatchVariant(scrutinee, arms) => {
            1 + count_operations(scrutinee)
                + arms.iter().map(|a| count_operations(&a.body)).sum::<usize>()
        }
    }
}

/// Interval arithmetic: compute the possible value range of an expression.
/// Returns (min, max) bounds. Used to verify overflow hints.
///
/// This is the key innovation: the compiler COMPUTES whether overflow is possible
/// instead of trusting the AI or inserting runtime checks unconditionally.
pub fn compute_range(
    expr: &Expr,
    field_ranges: &HashMap<&str, (i64, i64)>,
    input_name: &str,
) -> Option<(i64, i64)> {
    match expr {
        Expr::Number(n) => Some((*n, *n)),
        Expr::Field(base, field) => {
            if matches!(base.as_ref(), Expr::Ident(n) if n == input_name) {
                field_ranges.get(field.as_str()).copied()
            } else {
                None
            }
        }
        Expr::Binary(op, left, right) => {
            let (l_min, l_max) = compute_range(left, field_ranges, input_name)?;
            let (r_min, r_max) = compute_range(right, field_ranges, input_name)?;
            match op {
                BinOp::Add => Some((l_min.checked_add(r_min)?, l_max.checked_add(r_max)?)),
                BinOp::Sub => Some((l_min.checked_sub(r_max)?, l_max.checked_sub(r_min)?)),
                BinOp::Mul => {
                    let products = [
                        l_min.checked_mul(r_min)?,
                        l_min.checked_mul(r_max)?,
                        l_max.checked_mul(r_min)?,
                        l_max.checked_mul(r_max)?,
                    ];
                    Some((*products.iter().min()?, *products.iter().max()?))
                }
                BinOp::Mod => {
                    if r_min <= 0 && r_max >= 0 {
                        None
                    } else {
                        // x % d is in [0, d-1] for positive d, regardless of x
                        Some((0, r_max.abs() - 1))
                    }
                }
                BinOp::Div => {
                    if r_min <= 0 && r_max >= 0 {
                        None // divisor range includes zero — can't prove safe
                    } else {
                        let quotients = [
                            l_min.checked_div(r_min)?,
                            l_min.checked_div(r_max)?,
                            l_max.checked_div(r_min)?,
                            l_max.checked_div(r_max)?,
                        ];
                        Some((*quotients.iter().min()?, *quotients.iter().max()?))
                    }
                }
                _ => None, // comparisons/booleans return bool, not a range
            }
        }
        Expr::Neg(inner) => {
            let (min, max) = compute_range(inner, field_ranges, input_name)?;
            Some((-max, -min))
        }
        Expr::If(_, then_e, else_e) => {
            let (t_min, t_max) = compute_range(then_e, field_ranges, input_name)?;
            let (e_min, e_max) = compute_range(else_e, field_ranges, input_name)?;
            Some((t_min.min(e_min), t_max.max(e_max)))
        }
        Expr::Call(_, _) => None, // can't compute range through calls yet
        _ => None,
    }
}

fn path_list_to_set(paths: &[Path]) -> HashSet<Vec<String>> {
    paths.iter().map(|p| p.segments.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    use std::path::Path as StdPath;

    const VALID: &str = r#"@verbose 0.1.0

concept Invoice
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule important_invoice
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : Invoice
  output:
    important : bool
  logic:
    important = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1
"#;

    fn verify_str(src: &str) -> Vec<VerifyError> {
        let tokens = Lexer::new(src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        verify_program(&program, StdPath::new("examples"))
    }

    #[test]
    fn happy_path() {
        let errs = verify_str(VALID);
        assert!(errs.is_empty(), "expected no errors, got {:#?}", errs);
    }

    /// A rule named after a built-in primitive is REFUSED, not shadowed.
    ///
    /// Before this check, `rule band` verified clean ("all proofs check out")
    /// and then every `band(a, b)` call site resolved to bitwise AND instead
    /// of the rule — a silently wrong answer with no diagnostic anywhere in
    /// the pipeline. `native::tests::streaming_x86_divmod_logic` was the
    /// casualty that surfaced it: its `rule band(a, b) logic: out = a and b`
    /// started returning `6 & 9 == 0` instead of `1` the moment the bitwise
    /// arc taught the compiler `band`.
    /// A minimal, otherwise-valid one-rule program named `name`. Used by the
    /// collision tests below so the ONLY thing under test is the rule name.
    fn one_rule_named(name: &str) -> String {
        format!(
            r#"@verbose 0.1.0

concept Invoice
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule {}
  @intention: "x"
  @source: invoices.intent:1
  input:
    i : Invoice
  output:
    out : number
  logic:
    out = i.amount
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1
"#,
            name
        )
    }

    #[test]
    fn rule_named_after_a_primitive_is_refused() {
        let errs = verify_str(&one_rule_named("band"));
        assert!(
            errs.iter().any(|e| e.context == "rule 'band'"
                && e.message.contains("collides with the built-in primitive")),
            "expected a primitive-collision refusal for `rule band`, got {:#?}",
            errs
        );
    }

    /// The refusal covers the WHOLE closed set, not just the bitwise names
    /// that surfaced it. Every entry of `PRIMITIVE_CALL_NAMES` is intercepted
    /// by `parse_primary` in call position, so every entry is equally
    /// unreachable as a rule name.
    #[test]
    fn every_primitive_name_is_refused_as_a_rule_name() {
        for prim in PRIMITIVE_CALL_NAMES {
            let errs = verify_str(&one_rule_named(prim));
            assert!(
                errs.iter().any(|e| e.message.contains("collides with the built-in primitive")),
                "`rule {}` must be refused as a primitive collision, got {:#?}",
                prim, errs
            );
        }
    }

    /// The refusal must not over-reach: a name that merely CONTAINS a
    /// primitive, or differs in case, is a perfectly ordinary rule name.
    #[test]
    fn primitive_lookalike_rule_names_stay_legal() {
        for ok_name in ["bool_and", "band_of", "my_band", "Band", "lengths", "readx"] {
            let errs = verify_str(&one_rule_named(ok_name));
            assert!(
                !errs.iter().any(|e| e.message.contains("collides with the built-in primitive")),
                "`rule {}` must NOT be refused as a primitive collision, got {:#?}",
                ok_name, errs
            );
        }
    }

    /// `check_expr_against` must type-check bitwise OPERANDS, not just the
    /// bitwise result. `infer_expr_type` has always reported Number for every
    /// bitwise node, so the catch-all arm covered the result — but with no
    /// dedicated arm there was no recursion into the children, and
    /// `band(p.name, p.n)` on a text field verified clean while the
    /// structurally identical `min(p.name, p.n)` was rejected.
    ///
    /// All six ops are swept: a missing arm on any one of them silently
    /// reopens the hole for that op alone.
    #[test]
    fn bitwise_operands_are_type_checked() {
        // `reads:` is derived from the logic so the purity check never fires
        // and the only error a case can produce is the type error under test.
        let program = |logic: &str| -> String {
            let mut reads: Vec<&str> = Vec::new();
            if logic.contains("p.name") { reads.push("p.name"); }
            if logic.contains("p.n)") || logic.contains("p.n,") { reads.push("p.n"); }
            format!(
                r#"@verbose 0.1.0

concept P
  @intention: "x"
  @source: invoices.intent:1
  fields:
    name : text
    n : number [0, 100]

rule r
  @intention: "y"
  @source: invoices.intent:1
  input:
    p : P
  output:
    out : number
  logic:
    out = {logic}
  proofs:
    purity:
      reads   : [{}]
      calls   : []
    termination:
      bound : 10
"#,
                reads.join(", ")
            )
        };

        // A text operand in any position of any bitwise op is a type error.
        for bad in [
            "band(p.name, p.n)", "band(p.n, p.name)",
            "bor(p.name, p.n)",  "bor(p.n, p.name)",
            "bxor(p.name, p.n)", "bxor(p.n, p.name)",
            "shl(p.name, p.n)",  "shl(p.n, p.name)",
            "shr(p.name, p.n)",  "shr(p.n, p.name)",
            "bnot(p.name)",
        ] {
            let errs = verify_str(&program(bad));
            assert!(
                errs.iter().any(|e| e.message.contains("has type 'text' but context expects 'number'")),
                "`{bad}` must be rejected for its text operand; got {errs:#?}"
            );
        }

        // Number operands stay clean — the arms must not false-positive.
        for good in [
            "band(p.n, 15)", "bor(p.n, 1)", "bxor(p.n, p.n)",
            "shl(p.n, 4)", "shr(p.n, 4)", "bnot(p.n)",
            "bor(shl(p.n, 4), band(p.n, 15))",
        ] {
            let errs = verify_str(&program(good));
            assert!(errs.is_empty(), "`{good}` must verify clean; got {errs:#?}");
        }

        // A bitwise expression in a non-number context is still a mismatch,
        // and the message names the op (not a generic "expression").
        let text_out = r#"@verbose 0.1.0

concept P
  @intention: "x"
  @source: invoices.intent:1
  fields:
    n : number [0, 100]

rule r
  @intention: "y"
  @source: invoices.intent:1
  input:
    p : P
  output:
    out : text
  logic:
    out = bxor(p.n, 1)
  proofs:
    purity:
      reads   : [p.n]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(text_out);
        assert!(
            errs.iter().any(|e| e.message.contains("bxor produces number but the expected type is 'text'")),
            "bitwise in a text context must be rejected by name; got {errs:#?}"
        );
    }

    #[test]
    fn append_file_non_text_content_rejected() {
        // The content expression of append_file must produce text at runtime.
        // Passing a bare number is a type error caught at compile time.
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number

rule trig
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    b : bool
  logic:
    b = t.x > 0
  proofs:
    purity:
      reads   : [t.x]
      calls   : []
    termination:
      bound : 1

reaction bad
  @intention: "z"
  @source: invoices.intent:1
  trigger: trig
  effects:
    append_file "/tmp/x.log" t.x
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("type 'number'")
                && e.message.contains("expects 'text'")),
            "expected number/text mismatch on append_file content, got {:#?}",
            errs
        );
    }

    #[test]
    fn concat_with_collection_arg_rejected() {
        // concat only accepts scalar args (number/bool/text). Passing a
        // collection is a type error caught at compile time.
        let src = r#"@verbose 0.1.0

concept Bag
  @intention: "x"
  @source: collections.intent:1
  fields:
    items : collection(number)

rule bad
  @intention: "y"
  @source: collections.intent:2
  input:
    b : Bag
  output:
    r : text
  logic:
    r = concat("items are ", b.items)
  proofs:
    purity:
      reads   : [b.items]
      calls   : []
    termination:
      bound : 2
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("concat argument")
                && e.message.contains("scalar")),
            "expected concat-scalar-args error, got {:#?}",
            errs
        );
    }

    #[test]
    fn record_unknown_concept_rejected() {
        let src = r#"@verbose 0.1.0

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number

rule make
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Ghost
  logic:
    p = Ghost { x: i.x }
  proofs:
    purity:
      reads   : [i.x]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        // Two errors expected: unknown type 'Ghost' on output, and unknown
        // concept 'Ghost' on the constructor. We only assert the constructor
        // error is present and named.
        assert!(
            errs.iter().any(|e| e.message.contains("unknown concept 'Ghost'")),
            "expected unknown-concept-on-constructor error, got {:#?}",
            errs
        );
    }

    #[test]
    fn record_missing_field_rejected() {
        let src = r#"@verbose 0.1.0

concept Pair
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number

rule make
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x }
  proofs:
    purity:
      reads   : [i.x]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("missing field 'b'")),
            "expected missing-field error, got {:#?}",
            errs
        );
    }

    #[test]
    fn record_extra_field_rejected() {
        let src = r#"@verbose 0.1.0

concept Pair
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number

rule make
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x, b: i.x, c: i.x }
  proofs:
    purity:
      reads   : [i.x]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("unknown field 'c'")),
            "expected unknown-field error, got {:#?}",
            errs
        );
    }

    #[test]
    fn record_field_wrong_type_rejected() {
        let src = r#"@verbose 0.1.0

concept Pair
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number

rule make
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x, b: i.x > 0 }
  proofs:
    purity:
      reads   : [i.x]
      calls   : []
    termination:
      bound : 2
"#;
        let errs = verify_str(src);
        // The b field is declared number but its expression is bool.
        assert!(
            errs.iter().any(|e| e.message.contains("type 'bool'")
                && e.message.contains("expects 'number'")),
            "expected bool-vs-number type-mismatch on field b, got {:#?}",
            errs
        );
    }

    #[test]
    fn map_outside_collection_rejected() {
        // Closes the previously-silent hole: rule output is a number but logic
        // uses map(...) which produces a collection. The shape check must catch
        // this.
        let src = r#"@verbose 0.1.0

concept Bag
  @intention: "x"
  @source: collections.intent:1
  fields:
    items : collection(number)

rule wrong
  @intention: "y"
  @source: collections.intent:2
  input:
    b : Bag
  output:
    r : number
  logic:
    r = map(b.items, x => x + 1)
  proofs:
    purity:
      reads   : [b.items]
      calls   : []
    termination:
      bound : 2
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("map/filter")
                && e.message.contains("number")),
            "expected map-shape error, got {:#?}",
            errs
        );
    }

    #[test]
    fn ok_in_non_result_rule_rejected() {
        // Using Ok/Err in a rule whose output is bool (not Result) — the
        // type-shape check must flag this.
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule bad
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : bool
  logic:
    r = Ok(t.amount)
  proofs:
    purity:
      reads   : [t.amount]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("logic")
                && e.message.contains("Result constructor")),
            "expected a Result-constructor-in-non-Result-rule error, got {:#?}",
            errs
        );
    }

    #[test]
    fn ok_content_wrong_type_rejected() {
        // Declared output is Result(number, text), but the Ok arm contains a
        // text literal. The bidirectional check must catch this.
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule bad
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : Result(number, text)
  logic:
    r = if t.amount > 0 then Ok("oops") else Err("no")
  proofs:
    purity:
      reads   : [t.amount]
      calls   : []
    termination:
      bound : 3
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("logic")
                && e.message.contains("text")
                && e.message.contains("number")),
            "expected a text/number mismatch error inside Ok, got {:#?}",
            errs
        );
    }

    #[test]
    fn top_level_output_type_mismatch_rejected() {
        // Declared output is number, but the logic produces a bool
        // (a comparison). Catches the coarse shape error.
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule bad
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    r = t.amount > 0
  proofs:
    purity:
      reads   : [t.amount]
      calls   : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("logic")
                && e.message.contains("bool")
                && e.message.contains("number")),
            "expected a bool/number mismatch error at the top level, got {:#?}",
            errs
        );
    }

    /// A wrong-arity rule call is refused AT VERIFY TIME, not only at
    /// emit/run time.
    ///
    /// THE DEFECT THIS PINS was a TRUTHFULNESS hole, not a safety one. Before
    /// this check, `helper(i, i)` on a one-input rule produced:
    ///
    /// ```text
    ///   verbosec <file>            -> "verified: ...; all proofs check out"  (rc 0)
    ///   verbosec <file> --native   -> "native call requires exactly 1 argument"
    ///   verbosec <file> --run      -> "rule call expects 1 argument, got 2"
    /// ```
    ///
    /// So it failed closed in practice — no bad binary, no wrong answer — but
    /// the VERIFIER blessed a program its own emitter and interpreter both
    /// refuse. "The verifier is the durable artifact" is the project's central
    /// claim, and a reference that disagrees with itself is a counterexample
    /// to it. Same class as the bool exit-code inconsistency (PR #156).
    ///
    /// The CORRECT-ARITY TWIN is the half that makes the refusal attributable:
    /// the two programs differ in exactly one token, so a clean verdict on the
    /// twin proves the error came from the arity and not from some unrelated
    /// defect in the fixture. Without it this test would still pass against a
    /// verifier that rejected the program for the wrong reason.
    #[test]
    fn wrong_arity_rule_call_rejected_at_verify_time() {
        // `{CALL}` is the only thing that differs between the two programs.
        let program = |call: &str| {
            format!(
                r#"@verbose 0.1.0

concept Invoice
  @intention: "An invoice has an amount"
  @source: invoices.intent:1
  fields:
    amount : number [0, 1000000]

rule helper
  @intention: "doc"
  @source: invoices.intent:1
  input:
    i : Invoice
  output:
    ok : bool
  logic:
    ok = i.amount > 1000
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1

rule caller
  @intention: "doc"
  @source: invoices.intent:1
  input:
    i : Invoice
  output:
    ok : bool
  logic:
    ok = {call}
  proofs:
    purity:
      reads   : [i]
      calls   : [helper]
    termination:
      bound : 3
"#
            )
        };

        // Two arguments where the callee declares one input.
        let errs = verify_str(&program("helper(i, i)"));
        assert!(
            errs.iter().any(|e| e.context.contains("caller")
                && e.context.contains("calls")
                && e.message.contains("helper")
                && e.message.contains("2 arguments")
                && e.message.contains("exactly 1")),
            "expected a wrong-arity refusal naming the caller, the callee, and \
             expected-vs-actual; got {:#?}",
            errs
        );

        // Zero arguments is the same defect from the other side. The parser
        // accepts `helper()`, so nothing but this check stands between it and
        // a "proofs check out" verdict.
        let errs = verify_str(&program("helper()"));
        assert!(
            errs.iter().any(|e| e.message.contains("helper")
                && e.message.contains("0 arguments")
                && e.message.contains("exactly 1")),
            "expected a zero-argument call to be refused too; got {:#?}",
            errs
        );

        // THE TWIN: correct arity, otherwise byte-identical. Must verify clean.
        let errs = verify_str(&program("helper(i)"));
        assert!(
            errs.is_empty(),
            "the correct-arity twin must verify clean, or the refusal above is \
             not attributable to the arity; got {:#?}",
            errs
        );
    }

    /// A wrong-arity call is refused wherever it SITS, not only at the top of
    /// the logic — the walk has no catch-all arm, so a nested position must
    /// not be able to hide one.
    ///
    /// The three positions probed here are the ones a scoping-aware walk is
    /// most likely to drop: inside a lambda body (`count`'s predicate), inside
    /// a `let` RHS (which is collected separately from `logic.value`), and
    /// inside a `concat` argument list.
    #[test]
    fn wrong_arity_rule_call_rejected_in_nested_positions() {
        let with_logic = |lets: &str, value: &str, out_ty: &str| {
            format!(
                r#"@verbose 0.1.0

concept Line
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number [0, 1000]

concept Batch
  @intention: "x"
  @source: invoices.intent:1
  fields:
    lines : collection(Line)

rule helper
  @intention: "y"
  @source: invoices.intent:1
  input:
    l : Line
  output:
    ok : bool
  logic:
    ok = l.amount > 10
  proofs:
    purity:
      reads   : [l.amount]
      calls   : []
    termination:
      bound : 1

rule caller
  @intention: "y"
  @source: invoices.intent:1
  input:
    b : Batch
  output:
    r : {out_ty}
  logic:
{lets}    r = {value}
  proofs:
    purity:
      reads   : [b.lines]
      calls   : [helper]
    termination:
      bound : 5
"#
            )
        };

        let nested = [
            // Inside a quantifier/aggregation lambda body.
            ("lambda body", with_logic("", "count(b.lines, e => helper(e, e))", "number")),
            // Inside a `let` RHS — collected from logic.bindings, not logic.value.
            (
                "let RHS",
                with_logic(
                    "    let n = count(b.lines, e => helper(e, e))\n",
                    "n",
                    "number",
                ),
            ),
            // Inside a concat argument list.
            (
                "concat arg",
                with_logic("", "concat(\"n=\", count(b.lines, e => helper(e, e)))", "text"),
            ),
        ];

        for (position, src) in nested {
            let errs = verify_str(&src);
            assert!(
                errs.iter().any(|e| e.message.contains("helper")
                    && e.message.contains("2 arguments")),
                "a wrong-arity call in the {position} position escaped the arity \
                 check; got {errs:#?}",
            );
        }
    }

    /// Source for the binary-operand probes: one concept with a number, a
    /// text and a bytes-producing field, and one rule whose whole body is
    /// `{body}`. `{out}` is the declared output type, `{reads}` the purity
    /// proof — both are per-probe because getting either wrong makes the
    /// probe refuse for a reason that is not the one under test.
    fn operand_probe(out: &str, body: &str, reads: &str) -> String {
        format!(
            r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    n : number [0, 1000]
    m : number [0, 1000]
    s : text [..32]
    u : text [..32]

rule probe
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    out : {out}
  logic:
    out = {body}
  proofs:
    purity:
      reads   : [{reads}]
      calls   : []
    termination:
      bound : 20
"#
        )
    }

    /// Arithmetic and ORDERING operands must be Number; `and`/`or` operands
    /// must be Bool; `==` / `!=` take two operands of the same type, and that
    /// type must be Number or Text.
    ///
    /// Those are the interpreter's rules, verbatim — `eval_expr`'s
    /// `Expr::Binary` match has exactly these arms and everything else is
    /// `cannot apply {op} to {l} and {r}` at runtime. Before this check
    /// existed, `check_expr_against` had NO `Expr::Binary` arm at all, so
    /// `infer_expr_type` reported the RESULT type and the operands were never
    /// visited: `t.s * 2` on a text field verified clean and its native binary
    /// printed a randomized stack address (see
    /// `native::tests::text_operand_in_arithmetic_never_reaches_a_binary_that_leaks_a_stack_address`),
    /// while `t.s > 1` compiled to a predicate that is `true` for every input.
    ///
    /// Enumerated rather than sampled: the operator families and the operand
    /// types are both finite, and a check that is right for `*` and wrong for
    /// `%` reads exactly like a check that is right, to any test that probes
    /// one member of a family. Every REFUSE row carries a corrected twin so
    /// the refusal is attributable to the operand type and not to some other
    /// property of the probe.
    #[test]
    fn binary_operand_types_are_checked_against_interpreter_semantics() {
        // (label, out_ty, body, reads, must_verify)
        let cases: &[(&str, &str, &str, &str, bool)] = &[
            // ---- arithmetic: Number x Number ----
            ("add text lhs", "number", "t.s + 2", "t.s", false),
            ("add text rhs", "number", "2 + t.s", "t.s", false),
            ("sub text lhs", "number", "t.s - 2", "t.s", false),
            ("mul text lhs", "number", "t.s * 2", "t.s", false),
            ("mul text rhs", "number", "2 * t.s", "t.s", false),
            ("div text lhs", "number", "t.s / 2", "t.s", false),
            ("mod text lhs", "number", "t.s % 2", "t.s", false),
            ("add bool lhs", "number", "(t.n > 1) + 2", "t.n", false),
            ("add bytes lhs", "number", "b\"\\x41\" + 2", "", false),
            ("add both text", "number", "t.s + t.u", "t.s, t.u", false),
            ("add number twin", "number", "t.n + 2", "t.n", true),
            ("sub number twin", "number", "t.n - 2", "t.n", true),
            ("mul number twin", "number", "t.n * 2", "t.n", true),
            ("div number twin", "number", "t.n / 2", "t.n", true),
            ("mod number twin", "number", "t.n % 2", "t.n", true),
            ("mul two fields twin", "number", "t.n * t.m", "t.m, t.n", true),
            // ---- ordering: Number x Number ----
            ("gt text lhs", "bool", "t.s > 1", "t.s", false),
            ("lt text lhs", "bool", "t.s < 1", "t.s", false),
            ("gteq text lhs", "bool", "t.s >= 1", "t.s", false),
            ("lteq text lhs", "bool", "t.s <= 1", "t.s", false),
            ("gt text rhs", "bool", "1 > t.s", "t.s", false),
            ("gt two texts", "bool", "t.s > t.u", "t.s, t.u", false),
            ("gt bool lhs", "bool", "(t.n > 1) > 1", "t.n", false),
            ("gt number twin", "bool", "t.n > 1", "t.n", true),
            ("lt number twin", "bool", "t.n < 1", "t.n", true),
            ("gteq number twin", "bool", "t.n >= 1", "t.n", true),
            ("lteq number twin", "bool", "t.n <= 1", "t.n", true),
            // ---- equality: same type, Number or Text ----
            ("eq number vs text", "bool", "t.n == t.s", "t.n, t.s", false),
            ("eq text vs number", "bool", "t.s == t.n", "t.n, t.s", false),
            ("eq text vs numlit", "bool", "t.s == 1", "t.s", false),
            ("neq number vs text", "bool", "t.n != t.s", "t.n, t.s", false),
            ("eq bool vs bool", "bool", "(t.n > 1) == (t.m > 1)", "t.m, t.n", false),
            ("eq bytes vs bytes", "bool", "b\"\\x41\" == b\"\\x41\"", "", false),
            ("eq number twin", "bool", "t.n == t.m", "t.m, t.n", true),
            ("eq numlit twin", "bool", "t.n == 1", "t.n", true),
            // Text equality is a SHIPPED feature and must keep verifying:
            // examples/clients.verbose, allowlist.verbose, access_check.verbose.
            ("eq text vs text twin", "bool", "t.s == t.u", "t.s, t.u", true),
            ("eq text vs literal twin", "bool", "t.s == \"ab\"", "t.s", true),
            ("neq text vs literal twin", "bool", "t.s != \"ab\"", "t.s", true),
            ("eq literal vs text twin", "bool", "\"ab\" == t.s", "t.s", true),
            // ---- logical: Bool x Bool ----
            ("and text lhs", "bool", "t.s and (t.n > 1)", "t.n, t.s", false),
            ("or text rhs", "bool", "(t.n > 1) or t.s", "t.n, t.s", false),
            ("and number lhs", "bool", "t.n and (t.n > 1)", "t.n", false),
            ("and bool twin", "bool", "(t.n > 1) and (t.m > 1)", "t.m, t.n", true),
            ("or bool twin", "bool", "(t.n > 1) or (t.m > 1)", "t.m, t.n", true),
            // ---- unary: `not` is Bool, `-` is Number ----
            ("not text", "bool", "not t.s", "t.s", false),
            ("not number", "bool", "not t.n", "t.n", false),
            ("not bool twin", "bool", "not (t.n > 1)", "t.n", true),
            ("neg text", "number", "-t.s", "t.s", false),
            ("neg number twin", "number", "-t.n", "t.n", true),
            // ---- nested: the operand walk must reach through sub-expressions ----
            ("nested lhs", "number", "(t.s * 2) + t.n", "t.n, t.s", false),
            ("nested rhs", "number", "t.n + (t.s * 2)", "t.n, t.s", false),
            ("nested in if branch", "number", "if t.n > 1 then t.s * 2 else 0", "t.n, t.s", false),
            ("nested in eq operand", "bool", "t.n == (t.s * 2)", "t.n, t.s", false),
            ("nested twin", "number", "(t.n * 2) + t.m", "t.m, t.n", true),
        ];

        for (label, out_ty, body, reads, must_verify) in cases {
            let errs = verify_str(&operand_probe(out_ty, body, reads));
            if *must_verify {
                assert!(
                    errs.is_empty(),
                    "`{body}` ({label}) is valid Verbose and must still verify; got {errs:#?}",
                );
            } else {
                assert!(
                    !errs.is_empty(),
                    "`{body}` ({label}) has an operand the interpreter refuses at \
                     runtime, so the verifier must refuse it too — it verified clean",
                );
            }
        }
    }

    /// A `let` RHS is type-checked, so the operand check above cannot be
    /// bypassed by parking the expression in a binding first.
    ///
    /// `check_expr_against` used to run on `rule.logic.value` and nothing
    /// else. Measured before the fix: `let z = t.s * 2` then `out = z`
    /// verified clean and produced the SAME stack-address disclosure as the
    /// direct form, because `infer_expr_type(Ident("z"))` is None so the body
    /// check stayed silent. Every operand check in this pass was one `let`
    /// away from being bypassed.
    #[test]
    fn let_binding_rhs_is_type_checked() {
        let with_let = |rhs: &str, out_ty: &str, reads: &str| {
            format!(
                r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    n : number [0, 1000]
    s : text [..32]

rule probe
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    out : {out_ty}
  logic:
    let z = {rhs}
    out = z
  proofs:
    purity:
      reads   : [{reads}]
      calls   : []
    termination:
      bound : 20
"#
            )
        };

        for (label, rhs, out_ty, reads) in [
            ("arithmetic on text", "t.s * 2", "number", "t.s"),
            ("ordering on text", "t.s > 1", "bool", "t.s"),
            ("and on text", "t.s and (t.n > 1)", "bool", "t.n, t.s"),
            ("mixed equality", "t.n == t.s", "bool", "t.n, t.s"),
            ("neg on text", "-t.s", "number", "t.s"),
        ] {
            assert!(
                !verify_str(&with_let(rhs, out_ty, reads)).is_empty(),
                "`let z = {rhs}` ({label}) must be refused — a let RHS is not a \
                 hiding place for an operand the direct form rejects",
            );
        }

        // Corrected twins: the same shapes with number operands still verify.
        for (rhs, out_ty, reads) in [
            ("t.n * 2", "number", "t.n"),
            ("t.n > 1", "bool", "t.n"),
            ("(t.n > 1) and (t.n < 9)", "bool", "t.n"),
            ("t.n == 5", "bool", "t.n"),
            ("-t.n", "number", "t.n"),
        ] {
            let errs = verify_str(&with_let(rhs, out_ty, reads));
            assert!(errs.is_empty(), "`let z = {rhs}` must still verify; got {errs:#?}");
        }
    }

    /// A `let` bound to a record-returning rule may only be used as
    /// `<binding>.<field>` — never as a scalar.
    ///
    /// Slice agg-1 shipped record return with the emitter enforcing this
    /// alone: `docs/bytes-value-return-design.md` §6.2 records refusals #4
    /// (`out = 1 + mk(n)`) and #5 (`out = p * 1000`) as emitter-side
    /// precisely because the verifier accepted both. It accepted them for two
    /// different reasons — #4 because no arm ever recursed into a binary
    /// operand, #5 because `infer_expr_type`'s `Expr::Ident` arm answered None
    /// for every non-input name — and both are closed here.
    #[test]
    fn record_valued_bindings_and_calls_are_not_scalars() {
        let with_logic = |logic: &str| {
            format!(
                r#"@verbose 0.1.0

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : number [0, 255]
    b : number [0, 255]

concept Pair
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number [0, 255]
    y : number [0, 255]

rule mk
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair {{ x: i.b, y: i.a }}
  proofs:
    purity:
      reads   : [i.a, i.b]
      calls   : []
    termination:
      bound : 3

rule subject
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    out : number
  logic:
{logic}
  proofs:
    purity:
      reads   : [i]
      calls   : [mk]
    termination:
      bound : 20
"#
            )
        };

        for (label, logic) in [
            ("record binding as a scalar", "    let p = mk(i)\n    out = p * 1000"),
            ("record call in arithmetic", "    out = 1 + mk(i)"),
            ("record call in a let RHS", "    let z = 1 + mk(i)\n    out = z"),
            ("record binding compared", "    let p = mk(i)\n    out = if p == 1 then 2 else 3"),
        ] {
            let errs = verify_str(&with_logic(logic));
            assert!(
                errs.iter().any(|e| e.message.contains("Pair")),
                "{label} must be refused with a diagnostic naming the record \
                 concept; got {errs:#?}",
            );
        }

        // The corrected twin is exactly `examples/aggregate_pair.verbose`'s
        // shape and must keep verifying — this slice must not weaken agg-1.
        let errs = with_logic("    let p = mk(i)\n    out = p.x * 1000 + p.y");
        let errs = verify_str(&errs);
        assert!(errs.is_empty(), "the agg-1 `.field` shape must still verify; got {errs:#?}");
    }

    /// A `match_result` / `match` arm binder that shadows a record-typed
    /// `let` must not be read as that let.
    ///
    /// `infer_expr_type` now resolves a bare `Ident` through the binding map,
    /// and the arm bodies of `match_result` ARE visited by the type check
    /// while their binders' scope is NOT tracked. `verify_rule` therefore
    /// removes every lambda / arm binder name from the map before the check.
    /// Without that filter this program would be refused for a name that,
    /// inside the arm, is a plain number.
    #[test]
    fn arm_binder_shadowing_a_record_let_does_not_false_positive() {
        let src = r#"@verbose 0.1.0

concept In
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : number [0, 255]
    b : number [0, 255]

concept Pair
  @intention: "x"
  @source: invoices.intent:1
  fields:
    x : number [0, 255]
    y : number [0, 255]

rule mk
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { x: i.b, y: i.a }
  proofs:
    purity:
      reads   : [i.a, i.b]
      calls   : []
    termination:
      bound : 3

rule checked
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    r : Result(number, text)
  logic:
    r = if i.a > 10 then Ok(i.a) else Err("too small")
  proofs:
    purity:
      reads   : [i.a]
      calls   : []
    termination:
      bound : 4

rule subject
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : In
  output:
    out : Result(number, text)
  logic:
    let p = mk(i)
    out = match_result(checked(i), p => Ok(p * 2), e => Err(e))
  proofs:
    purity:
      reads   : [i]
      calls   : [checked, mk]
    termination:
      bound : 20
"#;
        let errs = verify_str(src);
        assert!(
            errs.is_empty(),
            "the arm binder `p` shadows the record-typed `let p`; inside the arm \
             it is a number and `p * 2` is valid — got {errs:#?}",
        );
    }

    /// The arity check must NOT fire on built-in primitives.
    ///
    /// Primitives legitimately take 0, 1, 2 or 3 arguments (`now_unix()`,
    /// `length(t)`, `min(a, b)`, `substring(t, a, b)`), so an arity check that
    /// saw them would reject essentially every non-trivial program. It does
    /// not see them because the parser resolves all 36 `PRIMITIVE_CALL_NAMES`
    /// in call position BEFORE the generic `Expr::Call` fallback — this test
    /// pins that reasoning to observable behaviour, so that the day a
    /// primitive stops being intercepted, this fails rather than the corpus.
    #[test]
    fn arity_check_does_not_fire_on_primitives() {
        let src = r#"@verbose 0.1.0

concept T
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number [0, 1000]
    name   : text [..64]

rule uses_primitives
  @intention: "y"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    let a = min(t.amount, 100)
    let b = length(substring(t.name, 0, 2))
    r = a + b + abs(0 - t.amount)
  proofs:
    purity:
      reads   : [t.amount, t.name]
      calls   : []
    termination:
      bound : 8
"#;
        let errs = verify_str(src);
        assert!(
            errs.is_empty(),
            "the arity check fired on a built-in primitive (0/1/2/3-arg forms \
             are all legal for primitives); got {:#?}",
            errs
        );
    }

    /// Two concepts with DISTINCT field names, a rule producing the second,
    /// and a consumer that binds it with `let`. `{BODY}` is the only thing that
    /// varies between the shapes below — so every refusal is attributable to
    /// the body under test and nothing else.
    fn let_record_program(body: &str) -> String {
        format!(
            r#"@verbose 0.1.0

concept P
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    a : number [0, 1000000]
    b : number [0, 1000000]

concept Q
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    x : number
    label : text

rule swap2
  @intention: "doc"
  @source: invoices.intent:1
  input:
    p : P
  output:
    q : Q
  logic:
    q = Q {{ x: p.b, label: "hi" }}
  proofs:
    purity:
      reads   : [p.b]
      calls   : []
    termination:
      bound : 5

rule use_it
  @intention: "doc"
  @source: invoices.intent:1
  input:
    p : P
  output:
    o : number
  logic:
    let r = swap2(p)
    o = {body}
  proofs:
    purity:
      reads   : [p]
      calls   : [swap2]
    termination:
      bound : 10
"#
        )
    }

    #[test]
    fn record_typed_let_binding_field_access_is_typechecked() {
        // The verifier has always typechecked `.field` on the INPUT concept and
        // never on a `let` bound to a record. That asymmetry let a program that
        // NEITHER backend accepts pass verification at rc 0: `--run` failed with
        // "no field 'a' on record" and `--native` refused the shape outright.
        //
        // Every bad shape below carries a corrected twin, so the refusal is
        // attributable to the defect under test rather than to anything else in
        // the fixture.

        // 1. A field of the INPUT concept, read off the Q-typed binding. The
        //    nastiest shape: `a` exists, just not on Q — and once aggregate
        //    return lands, native's bare-field-name lookup would resolve it to
        //    the INPUT's slot and print a plausible number at rc 0.
        let errs = verify_str(&let_record_program("r.a * 1000 + r.x"));
        assert!(
            errs.iter().any(|e| e.context.contains("use_it")
                && e.message.contains("concept 'Q' has no field 'a'")
                && e.message.contains("r.a")),
            "expected a field-existence refusal naming the binding's concept, \
             the field, and the path; got {:#?}",
            errs
        );

        // 2. A field of nothing at all.
        let errs = verify_str(&let_record_program("r.zzz * 1000 + r.x"));
        assert!(
            errs.iter().any(|e| e.context.contains("use_it")
                && e.message.contains("concept 'Q' has no field 'zzz'")
                && e.message.contains("r.zzz")),
            "expected a field-existence refusal for a field of nothing; got {:#?}",
            errs
        );

        // 3. TYPE mismatch: `label` exists on Q and is text, used where the
        //    declared output is number. Distinct from 1 and 2 — it exercises
        //    the inference half (the field must yield its DECLARED type), not
        //    the existence half. Only reachable in a position the bidirectional
        //    check descends into; that is exactly as far as the input-field path
        //    reaches today, and this deliberately does not go further.
        let errs = verify_str(&let_record_program("r.label"));
        assert!(
            errs.iter().any(|e| e.context.contains("use_it")
                && e.message.contains("type 'text'")
                && e.message.contains("expects 'number'")),
            "expected a type mismatch for a text field of a let-bound record \
             used where number is declared; got {:#?}",
            errs
        );

        // 4. THE CORRECTED TWIN — the same program with correct field names must
        //    still verify. This is what makes 1-3 attributable and what proves
        //    the check is not simply "refuse any field access on a let".
        let errs = verify_str(&let_record_program("r.x * 1000 + r.x"));
        assert!(
            errs.is_empty(),
            "the corrected twin must still verify; got {:#?}",
            errs
        );

        // 5. …and must still RUN, producing the value it produced before. A
        //    verifier change that quietly altered semantics would pass 1-4.
        use crate::interpreter::eval_rule;
        let src = let_record_program("r.x * 1000 + r.x");
        let tokens = Lexer::new(&src).tokenize().unwrap();
        let program = Parser::new(tokens).parse_program().unwrap();
        let rules: Vec<&Rule> = program
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Rule(r) => Some(r),
                _ => None,
            })
            .collect();
        let concepts: Vec<&Concept> = program
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Concept(c) => Some(c),
                _ => None,
            })
            .collect();
        let use_it = rules.iter().find(|r| r.name == "use_it").unwrap();
        let mut input = HashMap::new();
        input.insert("a".to_string(), crate::interpreter::Value::Number(9));
        input.insert("b".to_string(), crate::interpreter::Value::Number(7));
        let got = eval_rule(use_it, &rules, &concepts, &[], &input).unwrap();
        assert_eq!(
            format!("{:?}", got),
            format!("{:?}", crate::interpreter::Value::Number(7007)),
            "the corrected twin's runtime value changed"
        );
    }

    #[test]
    fn let_binding_field_check_stays_silent_when_the_type_is_unknown() {
        // Strictness must be EXACT. An over-strict version of the check above
        // would reject valid programs, so each shape here is a binding the pass
        // cannot type — or a binder it must not look at — and every one must
        // verify clean.
        //
        // The last two are the "conservative on lambda/let-bound vars" posture
        // the bidirectional check has always had: a lambda binder's `.field` is
        // filtered out inside `collect_expr_facts`, before the let-partition can
        // ever see it, and nothing here changes that.
        let program = |bindings: &str, body: &str, reads: &str| {
            format!(
                r#"@verbose 0.1.0

concept Item
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    v : number [0, 1000]

concept Bag
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    amount : number [0, 1000000]
    items  : collection(Item)

rule use_it
  @intention: "doc"
  @source: invoices.intent:1
  input:
    g : Bag
  output:
    o : number
  logic:
{bindings}
    o = {body}
  proofs:
    purity:
      reads   : [{reads}]
      calls   : []
    termination:
      bound : 40
"#
            )
        };

        let cases = [
            // A scalar let — `.field` on it is meaningless, but flagging that
            // is a different refusal class, not this one.
            ("    let t = g.amount", "t + g.amount", "g.amount"),
            // A collection-typed let: `map` is not inferable, so silence.
            (
                "    let m = map(g.items, e => e.v)",
                "g.amount",
                "g.amount, g.items",
            ),
            // A lambda binder's field access, in a quantifier and in a fold.
            (
                "",
                "if all(g.items, e => e.v > 0) then g.amount else 0",
                "g.amount, g.items",
            ),
            ("", "sum(g.items, e => e.v) + g.amount", "g.amount, g.items"),
            // A let whose RHS reads a lambda binder's field.
            (
                "    let s = sum(g.items, e => e.v)",
                "s + g.amount",
                "g.amount, g.items",
            ),
        ];
        for (bindings, body, reads) in cases {
            let errs = verify_str(&program(bindings, body, reads));
            assert!(
                errs.is_empty(),
                "the let-binding field check must stay silent here \
                 (bindings={bindings:?} body={body:?}); got {:#?}",
                errs
            );
        }
    }

    #[test]
    fn context_binding_field_access_is_typechecked() {
        // The `context:` binding had the IDENTICAL hole, and it was documented
        // in `validate_read_path` as a deliberate skip whose stated reason —
        // "we don't have the concept here to validate field names" — was simply
        // false: `Rule::context_ty` names it. The consequence was the same
        // verify/emit split: the verifier said "all proofs check out" and the
        // native emitter then refused with "unknown field 'nosuchfield' in
        // native codegen".
        let program = |field: &str| {
            format!(
                r#"@verbose 0.1.0

concept Policy
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    max_amount : number [0, 10000000]

concept Request
  @intention: "doc"
  @source: invoices.intent:1
  fields:
    amount : number [0, 10000000]

rule is_allowed
  @intention: "doc"
  @source: invoices.intent:1
  context:
    p : Policy
  input:
    r : Request
  output:
    allowed : bool
  logic:
    allowed = r.amount <= p.{field}
  proofs:
    purity:
      reads   : [r.amount, p.{field}]
      calls   : []
    termination:
      bound : 5
"#
            )
        };

        let errs = verify_str(&program("nosuchfield"));
        assert!(
            errs.iter().any(|e| e.context.contains("is_allowed")
                && e.message.contains("concept 'Policy' has no field 'nosuchfield'")
                && e.message.contains("p.nosuchfield")),
            "expected a field-existence refusal on the context concept; got {:#?}",
            errs
        );

        // Corrected twin.
        let errs = verify_str(&program("max_amount"));
        assert!(
            errs.is_empty(),
            "the corrected twin must still verify; got {:#?}",
            errs
        );
    }

    #[test]
    fn layer_application_calls_domain_accepted() {
        // Positive: an application rule calls a domain rule. Allowed by the
        // stratification (application can call domain or application).
        let src = r#"@verbose 0.1.0

concept Invoice
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule is_large
  @intention: "y"
  @source: invoices.intent:1
  @layer: domain
  input:
    i : Invoice
  output:
    large : bool
  logic:
    large = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1

rule flag_critical
  @intention: "y"
  @source: invoices.intent:1
  @layer: application
  input:
    i : Invoice
  output:
    flag : bool
  logic:
    flag = is_large(i)
  proofs:
    purity:
      reads   : [i]
      calls   : [is_large]
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors, got {:#?}", errs);
    }

    #[test]
    fn layer_domain_calls_application_rejected() {
        // Negative: a domain rule tries to call an application rule.
        // The sealed-subgraph discipline forbids the reverse direction.
        let src = r#"@verbose 0.1.0

concept Invoice
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule upper_orchestration
  @intention: "y"
  @source: invoices.intent:1
  @layer: application
  input:
    i : Invoice
  output:
    big : bool
  logic:
    big = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1

rule lower_domain
  @intention: "y"
  @source: invoices.intent:1
  @layer: domain
  input:
    i : Invoice
  output:
    flag : bool
  logic:
    flag = upper_orchestration(i)
  proofs:
    purity:
      reads   : [i]
      calls   : [upper_orchestration]
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("@layer")
                && e.message.contains("domain")
                && e.message.contains("application")),
            "expected a layer violation error, got {:#?}",
            errs
        );
    }

    #[test]
    fn layer_calls_unlayered_rejected() {
        // Negative: a layered rule calls an unlayered rule. The sealed-subgraph
        // rule forbids this — otherwise the layer discipline escapes transitively.
        let src = r#"@verbose 0.1.0

concept Invoice
  @intention: "x"
  @source: invoices.intent:1
  fields:
    amount : number

rule unlayered_helper
  @intention: "y"
  @source: invoices.intent:1
  input:
    i : Invoice
  output:
    big : bool
  logic:
    big = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      calls   : []
    termination:
      bound : 1

rule layered_caller
  @intention: "y"
  @source: invoices.intent:1
  @layer: application
  input:
    i : Invoice
  output:
    flag : bool
  logic:
    flag = unlayered_helper(i)
  proofs:
    purity:
      reads   : [i]
      calls   : [unlayered_helper]
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("@layer")
                && e.message.contains("unlayered")),
            "expected an unlayered-call error, got {:#?}",
            errs
        );
    }

    #[test]
    fn all_examples_with_json_run_without_panicking() {
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(all_examples_with_json_body)
            .expect("spawn test thread");
        handle.join().expect("test thread panicked");
    }

    fn all_examples_with_json_body() {
        // Integration guard: every .verbose file with a matching .json must
        // execute without runtime panic. Value::Err (a declared failure path)
        // is allowed — only eval_rule returning Err (missing field, type
        // mismatch, etc.) counts as failure. Covers the "interpreter silently
        // regressed on an example" class of bugs that parse+verify misses.
        use crate::interpreter::{eval_rule, load_json_input};
        use std::fs;

        // `examples/negative/` is excluded for the same reason as in
        // all_example_verbose_files_parse_and_verify: those fixtures are
        // deliberately invalid. Today they carry no paired `.json` so the
        // filter below would skip them anyway — the exclusion is here so that
        // adding one cannot silently turn a negative fixture into a
        // must-not-panic obligation.
        fn collect(dir: &StdPath, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if path.file_name().and_then(|s| s.to_str()) == Some("negative") {
                            continue;
                        }
                        collect(&path, out);
                    } else if path.extension().and_then(|s| s.to_str()) == Some("verbose") {
                        out.push(path);
                    }
                }
            }
        }

        let mut files = Vec::new();
        collect(StdPath::new("examples"), &mut files);

        let mut tested = 0;
        for path in &files {
            let json_path = path.with_extension("json");
            if !json_path.exists() {
                continue;
            }
            let src = fs::read_to_string(path).unwrap();
            let tokens = Lexer::new(&src).tokenize().unwrap();
            let program = Parser::new(tokens).parse_program().unwrap();
            // Files with imports need the CLI's import-resolution step;
            // the parse+verify sibling test already covers that path.
            if !program.uses.is_empty() {
                continue;
            }
            let errs = verify_program(&program, StdPath::new("examples"));
            assert!(
                errs.is_empty(),
                "verify errors in {}:\n{:#?}",
                path.display(),
                errs
            );

            // The last rule in the file is the conventional "primary" rule —
            // the one a reader of the example is meant to exercise, and the one
            // whose input type matches the records in the .json. Running it
            // also indirectly exercises any rules it composes.
            let all_rules: Vec<&Rule> = program
                .items
                .iter()
                .filter_map(|i| match i {
                    Item::Rule(r) => Some(r),
                    _ => None,
                })
                .collect();
            // Phase B slice 3: also include concepts declared inside
            // a `concept_group` so the interpreter's MatchVariant arm
            // can resolve positional binders against their declarations.
            let all_concepts: Vec<&Concept> = iter_all_concepts(&program.items).collect();
            let rule = match all_rules.last() {
                Some(r) => *r,
                None => continue,
            };
            let records = load_json_input(&json_path).unwrap_or_else(|e| {
                panic!("cannot load {}: {}", json_path.display(), e)
            });
            for (idx, record) in records.iter().enumerate() {
                let result = eval_rule(rule, &all_rules, &all_concepts, &[], record);
                assert!(
                    result.is_ok(),
                    "runtime error running rule '{}' in {} on record [{}]:\n  {}",
                    rule.name,
                    path.display(),
                    idx,
                    result.err().unwrap()
                );
                tested += 1;
            }
        }

        assert!(
            tested >= 20,
            "expected at least 20 rule-on-record evaluations, tested {}; did a .json file go empty?",
            tested
        );
    }

    #[test]
    fn all_example_verbose_files_parse_and_verify() {
        // Run on a dedicated 16 MiB stack — examples like sha256_abc.verbose
        // have 64-deep nested if/else chains that recurse through the
        // verifier's tree walkers and overflow the 2 MiB default test stack.
        let handle = std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(all_example_verbose_files_parse_and_verify_body)
            .expect("spawn test thread");
        handle.join().expect("test thread panicked");
    }

    fn all_example_verbose_files_parse_and_verify_body() {
        // Integration guard: every file under examples/ that ends in .verbose
        // must parse cleanly and verify with zero errors. If this test goes
        // red, an example or the language has drifted — the failing file name
        // and the verifier output point straight at the cause.
        //
        // ONE DIRECTORY IS EXCLUDED, and it is excluded by INTENT rather than
        // by accident: `examples/negative/` holds the NEGATIVE corpus, whose
        // whole job is to be refused (see examples/negative/README.md). Every
        // fixture there is deliberately invalid, so "verifies with zero errors"
        // is exactly the wrong assertion for it. Its own guard —
        // `two_generation_negative_corpus_sweep` in src/native.rs — asserts the
        // opposite: that verbosec refuses each one, and records whether the
        // self-hosted compiler refuses it too.
        use std::fs;

        fn collect(dir: &StdPath, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        if path.file_name().and_then(|s| s.to_str()) == Some("negative") {
                            continue;
                        }
                        collect(&path, out);
                    } else if path.extension().and_then(|s| s.to_str()) == Some("verbose") {
                        out.push(path);
                    }
                }
            }
        }

        let mut files = Vec::new();
        collect(StdPath::new("examples"), &mut files);
        assert!(
            files.len() >= 10,
            "expected at least 10 example .verbose files, found {}; did the test run from the wrong CWD?",
            files.len()
        );

        for path in &files {
            let src = fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("cannot read {}: {}", path.display(), e));
            let tokens = Lexer::new(&src).tokenize().unwrap_or_else(|e| {
                panic!("lex error in {}: {:?}", path.display(), e);
            });
            let program = Parser::new(tokens).parse_program().unwrap_or_else(|e| {
                panic!("parse error in {}: {:?}", path.display(), e);
            });
            // Files with `use` imports (module system demo) need the CLI's
            // import-resolution step before verification. The test runs
            // verify_program directly, so it skips those files — parsing
            // alone is still validated above. All other files must verify
            // clean against the file's own directory as base_dir (so
            // @source paths resolve relative to the .verbose file, not
            // hardcoded to "examples/").
            if !program.uses.is_empty() {
                continue;
            }
            let base = path.parent().unwrap_or(StdPath::new("examples"));
            let errs = verify_program(&program, base);
            assert!(
                errs.is_empty(),
                "verify errors in {}:\n{:#?}",
                path.display(),
                errs
            );
        }
    }

    #[test]
    fn map_reads_propagate_correctly() {
        // Verifier treats Map like Quantifier: the collection read is declared,
        // but the lambda variable's uses are scoped out.
        let src = r#"@verbose 0.1.0

concept Bag
  @intention: "a bag of numbers"
  @source: collections.intent:1
  fields:
    items : collection(number)

rule incremented
  @intention: "add one to each element"
  @source: collections.intent:2
  input:
    b : Bag
  output:
    r : collection(number)
  logic:
    r = map(b.items, x => x + 1)
  proofs:
    purity:
      reads   : [b.items]
      calls   : []
    termination:
      bound : 2
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors, got {:#?}", errs);
    }

    #[test]
    fn filter_missing_collection_read_rejected() {
        // If the reads declaration omits the collection being filtered,
        // the verifier must catch it — same rule as Quantifier.
        let src = r#"@verbose 0.1.0

concept Bag
  @intention: "a bag of numbers"
  @source: collections.intent:1
  fields:
    items : collection(number)

rule positives
  @intention: "keep positives"
  @source: collections.intent:2
  input:
    b : Bag
  output:
    r : collection(number)
  logic:
    r = filter(b.items, x => x > 0)
  proofs:
    purity:
      reads   : []
      calls   : []
    termination:
      bound : 3
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("purity.reads")),
            "expected a purity.reads error, got {:#?}",
            errs
        );
    }

    #[test]
    fn missing_declared_read() {
        let bad = VALID.replace("reads   : [i.amount]", "reads   : []");
        let errs = verify_str(&bad);
        assert!(
            errs.iter()
                .any(|e| e.context.contains("purity.reads") && e.message.contains("missing")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn extra_declared_read() {
        let bad = VALID.replace("reads   : [i.amount]", "reads   : [i.amount, i.other]");
        let errs = verify_str(&bad);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("extra") || e.message.contains("other")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn logic_target_mismatch() {
        let bad = VALID.replace("important = i.amount", "wrong = i.amount");
        let errs = verify_str(&bad);
        assert!(
            errs.iter().any(|e| e.context.contains("logic")
                && e.message.contains("wrong")
                && e.message.contains("important")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn unknown_field_rejected() {
        let bad = VALID
            .replace(
                "important = i.amount > 10000",
                "important = i.unknown_field > 10000",
            )
            .replace("reads   : [i.amount]", "reads   : [i.unknown_field]");
        let errs = verify_str(&bad);
        assert!(
            errs.iter().any(|e| e.message.contains("unknown_field")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn bad_source_line_rejected() {
        let bad = VALID.replace("invoices.intent:1", "invoices.intent:999");
        let errs = verify_str(&bad);
        assert!(
            errs.iter()
                .any(|e| e.context.contains("@source") && e.message.contains("999")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn insufficient_bound_rejected() {
        let bad = VALID.replace("bound : 1", "bound : 0");
        let errs = verify_str(&bad);
        assert!(
            errs.iter()
                .any(|e| e.context.contains("termination") && e.message.contains("0")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn vectorizable_with_calls_rejected() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
rule helper
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : bool
  logic:
    r = t.x > 0
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
rule test_bad
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : bool
  logic:
    r = helper(t)
  proofs:
    purity:
      reads: [t]
      calls: [helper]
    termination:
      bound: 1
  hints:
    vectorizable: "SIMD claim: no calls, no cross-element dependency"
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("vectorizable")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn overflow_hint_accepted_when_valid() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number [0, 100]
rule test
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    r = t.x + 10
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
  hints:
    overflow: [10, 110]
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn overflow_hint_rejected_when_too_tight() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number [0, 100]
rule test
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    r = t.x + 10
  proofs:
    purity:
      reads: [t.x]
      calls: []
    termination:
      bound: 1
  hints:
    overflow: [10, 100]
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("overflow") && e.message.contains("exceeds")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn calls_mismatch_detected() {
        let bad = VALID.replace("calls   : []", "calls   : [nonexistent]");
        let errs = verify_str(&bad);
        assert!(
            errs.iter().any(|e| e.message.contains("calls") || e.message.contains("nonexistent")),
            "got: {:#?}",
            errs
        );
    }

    #[test]
    fn reaction_unknown_trigger_rejected() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    x : number
reaction bad
  @intention: "t"
  @source: invoices.intent:1
  trigger: nonexistent_rule
  effects:
    print "oops"
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("trigger") && e.message.contains("nonexistent")),
            "got: {:#?}", errs
        );
    }

    #[test]
    fn let_bindings_reads_correct() {
        let src = r#"@verbose 0.1.0
concept T
  @intention: "t"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
rule test
  @intention: "t"
  @source: invoices.intent:1
  input:
    t : T
  output:
    r : number
  logic:
    let sum = t.a + t.b
    r = sum * 2
  proofs:
    purity:
      reads: [t.a, t.b]
      calls: []
    termination:
      bound: 2
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    // ─── Phase 7: service verifier tests ─────────────────────────────────

    /// Build a .verbose source for a RawTcp service with a bytes-echoing
    /// handler. Parameters let individual tests perturb one axis at a time
    /// (handler name, concept field type, concept field bound, service
    /// max_request) to test each verifier check in isolation.
    fn service_src(
        handler_name: &str,
        input_field_ty: &str,
        input_bound: i64,
        max_request: i64,
    ) -> String {
        let bound_str = if input_bound > 0 {
            format!(" [..{}]", input_bound)
        } else {
            String::new()
        };
        format!(
            "@verbose 0.1.0\n\nconcept Frame\n  @intention: \"a tcp frame\"\n  @source: invoices.intent:1\n  fields:\n    data : {ty}{bound}\n\nrule h\n  @intention: \"echo\"\n  @source: invoices.intent:1\n  input:\n    req : Frame\n  output:\n    resp : Frame\n  logic:\n    resp = Frame {{ data: req.data }}\n  proofs:\n    purity:\n      reads: [req.data]\n      calls: []\n    termination:\n      bound: 2\n\nservice s\n  @intention: \"a test service\"\n  @source: invoices.intent:1\n  listen:\n    protocol: raw_tcp\n    port: 9999\n    max_request: {mr}\n  handler: {h}\n",
            ty = input_field_ty,
            bound = bound_str,
            mr = max_request,
            h = handler_name
        )
    }

    /// The verifier's synthesised `HttpRequest.body` bound must TRACK the
    /// service's `max_request`, not be a hardcoded constant — see the
    /// doc comment on `builtin_http_request`. `method` / `path` stay the
    /// independent constants they are (they have their own runtime guards in
    /// `emit_http_parse_method_path`; body's bound is true by construction of
    /// the `read(client_fd, buf, max_request)` that produced the buffer).
    ///
    /// Asserted at this synthesis site as well as at native's, because the
    /// two are deliberately DUPLICATED shapes (see the doc comment on
    /// `http_request_builtin_concept_native`) and nothing else compares them.
    #[test]
    fn builtin_http_request_body_bound_tracks_max_request() {
        for body_max in [64i64, 4096, 65536, 1_048_576] {
            let c = builtin_http_request(body_max);
            let f = |n: &str| c.fields.iter().find(|f| f.name == n).unwrap().range;
            assert_eq!(
                f("body"),
                Some((0, body_max)),
                "body's declared bound must equal the service's max_request ({body_max}); \
                 a bound below the field's real capacity is the shape of the 2026-08-05 \
                 static-sizing overflow",
            );
            assert_eq!(f("method"), Some((0, 8)));
            assert_eq!(f("path"), Some((0, 256)));
        }
    }

    #[test]
    fn service_happy_path_bytes() {
        // Matching pair: handler takes Frame { data: bytes [..4096] },
        // service declares max_request: 4096.
        let errs = verify_str(&service_src("h", "bytes", 4096, 4096));
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn service_rejects_unknown_handler() {
        let errs = verify_str(&service_src("nonexistent_handler", "bytes", 4096, 4096));
        assert!(
            errs.iter().any(|e| e.context.contains("service 's' / handler")
                && e.message.contains("unknown rule 'nonexistent_handler'")),
            "expected unknown-handler error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_rejects_bad_source_line() {
        let src = "@verbose 0.1.0\n\nconcept Frame\n  @intention: \"t\"\n  @source: invoices.intent:1\n  fields:\n    data : bytes [..4096]\n\nrule h\n  @intention: \"echo\"\n  @source: invoices.intent:1\n  input:\n    req : Frame\n  output:\n    resp : Frame\n  logic:\n    resp = Frame { data: req.data }\n  proofs:\n    purity:\n      reads: [req.data]\n      calls: []\n    termination:\n      bound: 2\n\nservice s\n  @intention: \"svc\"\n  @source: invoices.intent:999999\n  listen:\n    protocol: raw_tcp\n    port: 9999\n    max_request: 4096\n  handler: h\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("service 's' / @source")),
            "expected service @source error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_rejects_raw_tcp_handler_with_text_field() {
        // text is not bytes — the types are deliberately isolated.
        let errs = verify_str(&service_src("h", "text", 4096, 4096));
        assert!(
            errs.iter().any(|e| e.message.contains("must be bytes")),
            "expected text-rejection error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_rejects_raw_tcp_handler_with_bytes_bound_mismatch() {
        // Handler declares [..4096] but service declares max_request: 1024.
        let errs = verify_str(&service_src("h", "bytes", 4096, 1024));
        assert!(
            errs.iter().any(|e| e.message.contains("must equal service max_request")),
            "expected bound-mismatch error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_rejects_raw_tcp_handler_with_unbounded_bytes() {
        // bytes without [..N] — explicit bound is mandatory.
        let errs = verify_str(&service_src("h", "bytes", 0, 4096));
        assert!(
            errs.iter().any(|e| e.message.contains("must declare an explicit bytes bound")),
            "expected missing-bound error, got: {:#?}",
            errs
        );
    }

    // ─── Slice rawtcp-inspect-0: a raw_tcp handler can inspect its input ──

    /// A raw_tcp service whose handler body / lets / service tail are
    /// supplied by the caller, so each refusal of design §4.4 can be probed
    /// against its minimally corrected twin.
    fn rawtcp_src(lets: &str, body: &str, reads: &str, service_tail: &str) -> String {
        format!(
            "@verbose 0.1.0\n\nconcept Frame\n  @intention: \"frame\"\n  @source: tag_probe.intent:1\n  fields:\n    data : bytes [..64]\n\nrule h\n  @intention: \"probe\"\n  @source: tag_probe.intent:2\n  input:\n    req : Frame\n  output:\n    resp : Frame\n  logic:\n{lets}    resp = {body}\n  proofs:\n    purity:\n      reads : [{reads}]\n      calls : []\n    termination:\n      bound : 9\n\nservice s\n  @intention: \"svc\"\n  @source: tag_probe.intent:3\n  listen:\n    protocol    : raw_tcp\n    port        : 18999\n    max_request : 64\n  handler: h\n{tail}",
            lets = lets, body = body, reads = reads, tail = service_tail
        )
    }

    /// Design §4.1-3: `byte_at` / `length` admit a bytes INPUT FIELD — and
    /// ONLY that shape among bytes values. The worked example is the
    /// positive half; the three twins below are the widening's edges.
    #[test]
    fn rawtcp_inspect_byte_addressed_gate_admits_a_bytes_input_field() {
        let src = std::fs::read_to_string("examples/tag_probe.verbose").expect("examples/tag_probe.verbose");
        let errs = verify_str(&src);
        assert!(errs.is_empty(), "tag_probe must verify clean: {:#?}", errs);

        // A bytes field on the input, inside an `if`, a `let`, a `le32`.
        for body in [
            "Frame { data: le32(byte_at(req.data, 0) + length(req.data)) }",
            "if length(req.data) > 3 then Frame { data: req.data } else Frame { data: b\"\\x00\" }",
        ] {
            let errs = verify_str(&rawtcp_src("", body, "req.data", ""));
            assert!(errs.is_empty(), "`{}` must verify clean: {:#?}", body, errs);
        }
        let errs = verify_str(&rawtcp_src("    let n = byte_at(req.data, 0)\n", "Frame { data: le32(n) }", "req.data", ""));
        assert!(errs.is_empty(), "a number let over byte_at must verify clean: {:#?}", errs);
    }

    /// Design §4.4 refusal #3: `byte_at` / `length` over a bytes expression
    /// that is NOT BoundText-registered — a bytes concat, `le32(...)`. The
    /// widening is to the input field, never to `Type::Bytes` at large, and
    /// the message names the admitted set.
    #[test]
    fn rawtcp_inspect_refuses_byte_addressing_a_streamed_bytes_value() {
        for (body, prim) in [
            ("Frame { data: le32(length(concat(b\"\\x00\", req.data))) }", "length"),
            ("Frame { data: le32(byte_at(concat(b\"\\x00\", req.data), 0)) }", "byte_at"),
            ("Frame { data: le32(length(le32(7))) }", "length"),
            ("Frame { data: le32(byte_at(le64(7), 0)) }", "byte_at"),
        ] {
            let errs = verify_str(&rawtcp_src("", body, "req.data", ""));
            let hit = errs.iter().find(|e| e.message.starts_with(&format!("{}: operand has no length the emitter can load", prim)));
            assert!(hit.is_some(), "`{}` must be refused by name, got: {:#?}", body, errs);
            let m = &hit.unwrap().message;
            assert!(m.contains("streamed with no sizing pass") && m.contains("raw_tcp input field"), "{}", m);
        }
        // twins: the same primitives over the ADMITTED bytes operands.
        for body in [
            "Frame { data: le32(length(req.data) + length(b\"\\x00\\x01\")) }",
            "Frame { data: le32(byte_at(req.data, 0) + byte_at(b\"\\x00\\x01\", 1)) }",
        ] {
            let errs = verify_str(&rawtcp_src("", body, "req.data", ""));
            assert!(errs.is_empty(), "`{}` must verify clean: {:#?}", body, errs);
        }
    }

    /// Design §4.4 refusal #1: `substring` over a bytes field — deferred to
    /// slice `rawtcp-inspect-0b` because it produces a bytes VALUE, whose
    /// only sinks are streamed (§4.2). Both expected contexts are probed:
    /// the response field (bytes expected) and a text let (text expected).
    #[test]
    fn rawtcp_inspect_refuses_bytes_substring_by_name() {
        for (lets, body) in [
            ("", "Frame { data: substring(req.data, 0, 1) }"),
            ("    let t = substring(req.data, 0, 1)\n", "Frame { data: le32(length(t)) }"),
        ] {
            let errs = verify_str(&rawtcp_src(lets, body, "req.data", ""));
            let hit = errs.iter().find(|e| e.message.starts_with("substring: 'req.data' is bytes"));
            assert!(hit.is_some(), "`{}` must be refused by name, got: {:#?}", body, errs);
            assert!(hit.unwrap().message.contains("rawtcp-inspect-0b"));
        }
        // twin: substring over TEXT is untouched.
        let errs = verify_str(&rawtcp_src("    let t = \"abc\"\n    let u = substring(t, 0, 1)\n", "Frame { data: le32(length(u)) }", "req.data", ""));
        assert!(errs.iter().all(|e| !e.message.starts_with("substring:")), "text substring must not trip the bytes refusal: {:#?}", errs);
    }

    /// Design §4.4 refusal #2: the TEXT primitives and text `==` over a
    /// bytes field — the bytes/text isolation, named. Each cell carries the
    /// primitive's name in the message so the offending call is one grep
    /// away, and the twin shows the explicit `byte_at` conversion.
    #[test]
    fn rawtcp_inspect_refuses_text_primitives_on_a_bytes_field_by_name() {
        let cells: [(&str, &str); 7] = [
            ("if starts_with(req.data, \"a\") then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "starts_with"),
            ("if ends_with(req.data, \"a\") then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "ends_with"),
            ("if contains(req.data, \"a\") then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "contains"),
            ("if contains(\"abc\", req.data) then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "contains"),
            ("Frame { data: le32(length(json_escape(req.data))) }", "json_escape"),
            ("Frame { data: le32(parse_int(req.data)) }", "parse_int"),
            ("if req.data == \"a\" then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "=="),
        ];
        for (body, prim) in cells {
            let errs = verify_str(&rawtcp_src("", body, "req.data", ""));
            let needle = format!("{}: 'req.data' is bytes", prim);
            let hit = errs.iter().find(|e| e.message.starts_with(&needle));
            assert!(hit.is_some(), "`{}` must be refused naming `{}`, got: {:#?}", body, prim, errs);
            let m = &hit.unwrap().message;
            assert!(m.contains("Convert explicitly with byte_at") && m.contains("rawtcp-inspect-0b"), "{}", m);
        }
        // twin: the explicit, visible conversion.
        let errs = verify_str(&rawtcp_src("", "if byte_at(req.data, 0) == 97 then Frame { data: b\"\\x01\" } else Frame { data: b\"\\x00\" }", "req.data", ""));
        assert!(errs.is_empty(), "the byte_at twin must verify clean: {:#?}", errs);
    }

    /// Design §4.4 refusal #7, RE-SCOPED by slice `multistep-1` (§5.5 #1 and
    /// the forked gate): `state:` / `after:` / `concurrency: forked` stay
    /// refused on a ONE-SHOT raw_tcp service — the shapes that keep slice
    /// 0's one-exchange contract — and each refusal now names the step loop
    /// (max_steps + read_timeout) as what lifts it. The multi-step twins
    /// live in `multistep_declaration_refusals_and_twins`.
    #[test]
    fn rawtcp_inspect_keeps_state_and_forked_refused_on_one_shot_raw_tcp() {
        let ok_body = "Frame { data: le32(length(req.data)) }";
        let errs = verify_str(&rawtcp_src("", ok_body, "req.data", "\n  state:\n    count : number = 0\n"));
        assert!(errs.iter().any(|e| e.message.contains("must also declare 'max_steps'") && e.message.contains("DROP the state declaration silently")), "{:#?}", errs);
        let errs = verify_str(&rawtcp_src("", ok_body, "req.data", "\n  state:\n    count : number = 0\n\n  after:\n    set count = state.count + 1\n"));
        assert!(errs.iter().any(|e| e.message.contains("must also declare 'max_steps'")), "{:#?}", errs);
        let errs = verify_str(&rawtcp_src("", ok_body, "req.data", "\n  concurrency: forked\n"));
        assert!(errs.iter().any(|e| e.message.contains("raw_tcp services with a step loop") && e.message.contains("multistep-1")), "{:#?}", errs);
        // twin: the plain one-shot service.
        let errs = verify_str(&rawtcp_src("", ok_body, "req.data", ""));
        assert!(errs.is_empty(), "{:#?}", errs);
    }

    // ─── Phase 7 slice 3a: Http10 service tests ─────────────────────────

    /// Build a .verbose source with an Http10 service and a handler whose
    /// input/output types are supplied by the caller. Lets tests perturb
    /// the handler shape and max_request to exercise each verifier check.
    fn http10_src(
        handler_input_ty: &str,
        handler_output_ty: &str,
        max_request: i64,
    ) -> String {
        format!(
            "@verbose 0.1.0\n\nrule h\n  @intention: \"handle\"\n  @source: invoices.intent:1\n  input:\n    req : {}\n  output:\n    resp : {}\n  logic:\n    resp = {} {{ status: 200, body: \"ok\" }}\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"http service\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: {}\n  handler: h\n",
            handler_input_ty, handler_output_ty, handler_output_ty, max_request
        )
    }

    #[test]
    fn http10_happy_path() {
        let errs = verify_str(&http10_src("HttpRequest", "HttpResponse", 4096));
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn http10_rejects_wrong_input_type() {
        // Handler input is user concept `WrongInput` instead of HttpRequest.
        let src = "@verbose 0.1.0\n\nconcept WrongInput\n  @intention: \"x\"\n  @source: invoices.intent:1\n  fields:\n    x : number\n\nrule h\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    req : WrongInput\n  output:\n    resp : HttpResponse\n  logic:\n    resp = HttpResponse { status: 200, body: \"ok\" }\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"x\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: 4096\n  handler: h\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("must be the built-in concept 'HttpRequest'")),
            "expected input-type rejection, got: {:#?}",
            errs
        );
    }

    #[test]
    fn http10_rejects_wrong_output_type() {
        // Handler output is plain `text` rather than HttpResponse.
        let src = "@verbose 0.1.0\n\nrule h\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    req : HttpRequest\n  output:\n    resp : text\n  logic:\n    resp = \"hello\"\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 0\n\nservice s\n  @intention: \"x\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: 4096\n  handler: h\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("must be the built-in concept 'HttpResponse'")),
            "expected output-type rejection, got: {:#?}",
            errs
        );
    }

    #[test]
    fn http10_rejects_max_request_below_minimum() {
        let errs = verify_str(&http10_src("HttpRequest", "HttpResponse", 32));
        assert!(
            errs.iter().any(|e| e.message.contains("requires max_request >= 64")),
            "expected max_request-floor error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn http10_rejects_user_concept_named_http_request() {
        // User declares `concept HttpRequest` — reserved name, must be
        // rejected when any Http10 service is present.
        let src = "@verbose 0.1.0\n\nconcept HttpRequest\n  @intention: \"mine\"\n  @source: invoices.intent:1\n  fields:\n    custom : number\n\nrule h\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    req : HttpRequest\n  output:\n    resp : HttpResponse\n  logic:\n    resp = HttpResponse { status: 200, body: \"ok\" }\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"x\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: 4096\n  handler: h\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("concept 'HttpRequest'") && e.message.contains("reserved")),
            "expected reserved-name error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn http10_rejects_user_concept_named_http_response() {
        let src = "@verbose 0.1.0\n\nconcept HttpResponse\n  @intention: \"mine\"\n  @source: invoices.intent:1\n  fields:\n    custom : number\n\nrule h\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    req : HttpRequest\n  output:\n    resp : HttpResponse\n  logic:\n    resp = HttpResponse { status: 200, body: \"ok\" }\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"x\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: 4096\n  handler: h\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("concept 'HttpResponse'") && e.message.contains("reserved")),
            "expected reserved-name error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn http10_allows_user_named_http_request_outside_http10_context() {
        // Without any Http10 service, `HttpRequest` is NOT reserved.
        // The user can declare their own concept with that name.
        let src = "@verbose 0.1.0\n\nconcept HttpRequest\n  @intention: \"user domain\"\n  @source: invoices.intent:1\n  fields:\n    x : number\n";
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors outside Http10 context, got: {:#?}", errs);
    }

    /// Phase 8 slice 8a/8b/8c regression helper: full Http10 service with a
    /// log content under test. The handler is fixed; only the log content
    /// expression varies.
    fn http10_log_src(log_content: &str) -> String {
        format!(
            "@verbose 0.1.0\n\nrule h\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    req : HttpRequest\n  output:\n    resp : HttpResponse\n  logic:\n    resp = HttpResponse {{ status: 200, body: \"ok\" }}\n  proofs:\n    purity:\n      reads: []\n      calls: []\n    termination:\n      bound: 1\n\nservice s\n  @intention: \"x\"\n  @source: invoices.intent:1\n  listen:\n    protocol: http_1_0\n    port: 8080\n    max_request: 4096\n  handler: h\n  log:\n    append_file \"/tmp/x.log\" {}\n",
            log_content
        )
    }

    #[test]
    fn phase8b_log_accepts_resp_status_and_body() {
        let errs =
            verify_str(&http10_log_src("concat(req.method, \" \", resp.status, \" \", resp.body)"));
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn phase8c_log_accepts_req_timestamp() {
        let errs = verify_str(&http10_log_src("concat(req.timestamp, \" \", req.method)"));
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn phase8b_log_rejects_unknown_resp_field() {
        let errs = verify_str(&http10_log_src("concat(\"x\", resp.headers)"));
        assert!(
            errs.iter().any(|e| e.message.contains("`resp.headers`")
                && e.message.contains("not a declared HttpResponse field")),
            "expected resp.headers rejection, got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase8b_log_rejects_unknown_req_field() {
        let errs = verify_str(&http10_log_src("concat(\"x\", req.user_agent)"));
        assert!(
            errs.iter().any(|e| e.message.contains("`req.user_agent`")
                && e.message.contains("not a declared HttpRequest field")),
            "expected req.user_agent rejection, got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase8_log_rejects_unknown_base_identifier() {
        // Only `req` and `resp` are valid bases — no `service`, `cfg`, etc.
        let errs = verify_str(&http10_log_src("concat(\"x\", service.name)"));
        assert!(
            errs.iter().any(|e| e.message.contains("can read fields of `req` or `resp` only")),
            "expected unknown-base rejection, got: {:#?}",
            errs
        );
    }

    /// Phase 9 slice 1 helper: a minimal program with a resource and a
    /// rule that reads it. Used by the slice 9 verifier regression tests.
    fn resource_src(reads: &str) -> String {
        format!(
            "@verbose 0.1.0\n\nresource cfg\n  @intention: \"x\"\n  @source: invoices.intent:1\n  path: \"/etc/x\"\n  max: 1024\n\nconcept C\n  @intention: \"x\"\n  @source: invoices.intent:1\n  fields:\n    x : number\n\nrule r\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    c : C\n  output:\n    out : text\n  logic:\n    out = read(cfg)\n  proofs:\n    purity:\n      reads: {}\n      calls: []\n    termination:\n      bound: 1\n",
            reads
        )
    }

    #[test]
    fn phase9_resource_happy_path() {
        let errs = verify_str(&resource_src("[cfg]"));
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }

    #[test]
    fn phase9_rejects_read_on_unknown_resource() {
        let src = "@verbose 0.1.0\n\nconcept C\n  @intention: \"x\"\n  @source: invoices.intent:1\n  fields:\n    x : number\n\nrule r\n  @intention: \"x\"\n  @source: invoices.intent:1\n  input:\n    c : C\n  output:\n    out : text\n  logic:\n    out = read(missing)\n  proofs:\n    purity:\n      reads: [missing]\n      calls: []\n    termination:\n      bound: 1\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("read('missing') references unknown resource")),
            "expected unknown-resource error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase9_rejects_read_missing_from_purity_reads() {
        // Rule reads cfg via read(cfg) but doesn't list it in purity.reads.
        let errs = verify_str(&resource_src("[]"));
        assert!(
            errs.iter().any(|e| e.message.contains("declared reads do not match logic")
                && e.message.contains("missing")
                && e.message.contains("cfg")),
            "expected purity-mismatch error for unlisted read('cfg'), got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase9_rejects_resource_max_zero() {
        let src = "@verbose 0.1.0\n\nresource bad\n  @intention: \"x\"\n  @source: invoices.intent:1\n  path: \"/etc/x\"\n  max: 0\n";
        // max=0 hits the parser's positivity check before the verifier sees it
        // (parser rejects "must be positive"). Verify the program string is
        // rejected at parse time:
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let res = crate::parser::Parser::new(tokens).parse_program();
        assert!(res.is_err(), "expected parse error for max=0, got: {:#?}", res);
    }

    #[test]
    fn phase9_rejects_resource_max_above_64mib() {
        // 64 MiB + 1 — verifier rejects (parser accepts any u32).
        let src = "@verbose 0.1.0\n\nresource huge\n  @intention: \"x\"\n  @source: invoices.intent:1\n  path: \"/etc/x\"\n  max: 67108865\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("exceeds slice-1 ceiling")),
            "expected max-too-large error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase9_rejects_duplicate_resource_name() {
        let src = "@verbose 0.1.0\n\nresource dup\n  @intention: \"a\"\n  @source: invoices.intent:1\n  path: \"/a\"\n  max: 1\n\nresource dup\n  @intention: \"b\"\n  @source: invoices.intent:1\n  path: \"/b\"\n  max: 1\n";
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate resource name 'dup'")),
            "expected duplicate-resource error, got: {:#?}",
            errs
        );
    }

    /// A named top-level item declared twice is REFUSED, with the offender
    /// named — and the correct-arity-style TWIN (one copy renamed) verifies
    /// clean, so the refusal is attributable to the duplication and nothing
    /// else.
    ///
    /// Verified to FAIL against f2e9a0e: before this check, `verify_program`
    /// returned zero errors for every one of these programs ("all proofs
    /// check out"), and the two backends then silently DISAGREED about which
    /// definition wins — `--run` binds the first (`all_rules.iter().find`),
    /// `--native` binds the last (`HashMap<name, &Rule>` overwrites). Measured
    /// on the rule case: 6 vs 105 from the same source, both at exit 0. Same
    /// family as the arity check (PR #163): the verifier certifying a program
    /// its executors mishandle.
    #[test]
    fn duplicate_top_level_item_name_rejected_at_verify_time() {
        // THE HEADLINE: two `rule f`, with a caller in between. `{SECOND}` is
        // the only thing that varies — `rule f` (duplicate) vs `rule g`
        // (renamed twin).
        let rule_program = |second_name: &str| {
            format!(
                r#"@verbose 0.1.0

concept N
  @intention: "n"
  @source: invoices.intent:1
  fields:
    v : number

rule f
  @intention: "first f: v + 1"
  @source: invoices.intent:1
  input:
    n : N
  output:
    out : number
  logic:
    out = n.v + 1
  proofs:
    purity:
      reads   : [n.v]
      calls   : []
    termination:
      bound : 1

rule caller
  @intention: "calls f"
  @source: invoices.intent:1
  input:
    n : N
  output:
    out : number
  logic:
    out = f(n)
  proofs:
    purity:
      reads   : [n]
      calls   : [f]
    termination:
      bound : 1

rule {second_name}
  @intention: "second: v + 100"
  @source: invoices.intent:1
  input:
    n : N
  output:
    out : number
  logic:
    out = n.v + 100
  proofs:
    purity:
      reads   : [n.v]
      calls   : []
    termination:
      bound : 1
"#,
                second_name = second_name
            )
        };

        let errs = verify_str(&rule_program("f"));
        assert!(
            errs.iter().any(|e| e.context.contains("rule 'f'")
                && e.message.contains("duplicate rule name 'f'")),
            "expected a duplicate-rule refusal naming 'f'; got {:#?}",
            errs
        );

        // THE TWIN: rename the second copy to `g`. Otherwise byte-identical.
        // Must verify clean, or the refusal above is not attributable to the
        // duplication.
        let errs = verify_str(&rule_program("g"));
        assert!(
            errs.is_empty(),
            "the renamed twin must verify clean; got {:#?}",
            errs
        );

        // The family, one probe per remaining silently-overwritten kind:
        // top-level concept, reaction, service, concept_group. (Resources and
        // connections already had their own duplicate check — covered by
        // phase9_rejects_duplicate_resource_name and the connection tests.)
        let dup_concept = r#"@verbose 0.1.0

concept C
  @intention: "first C"
  @source: invoices.intent:1
  fields:
    v : number

concept C
  @intention: "second C"
  @source: invoices.intent:1
  fields:
    v : number
"#;
        let errs = verify_str(dup_concept);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate concept name 'C'")),
            "expected a duplicate-concept refusal naming 'C'; got {:#?}",
            errs
        );

        let dup_reaction = r#"@verbose 0.1.0

concept P
  @intention: "p"
  @source: invoices.intent:1
  fields:
    amount : number

rule is_big
  @intention: "big"
  @source: invoices.intent:1
  input:
    p : P
  output:
    big : bool
  logic:
    big = p.amount > 100
  proofs:
    purity:
      reads   : [p.amount]
      calls   : []
    termination:
      bound : 1

reaction log_it
  @intention: "first"
  @source: invoices.intent:1
  trigger: is_big
  effects:
    append_file "/tmp/verbose_dup_rx.log" "A\n"

reaction log_it
  @intention: "second"
  @source: invoices.intent:1
  trigger: is_big
  effects:
    append_file "/tmp/verbose_dup_rx.log" "B\n"
"#;
        let errs = verify_str(dup_reaction);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate reaction name 'log_it'")),
            "expected a duplicate-reaction refusal naming 'log_it'; got {:#?}",
            errs
        );

        let dup_service = r#"@verbose 0.1.0

rule handler_a
  @intention: "a"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "A" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service api
  @intention: "first"
  @source: invoices.intent:1
  listen:
    protocol    : http_1_0
    port        : 18999
    max_request : 1024
  handler: handler_a

service api
  @intention: "second"
  @source: invoices.intent:1
  listen:
    protocol    : http_1_0
    port        : 19000
    max_request : 1024
  handler: handler_a
"#;
        let errs = verify_str(dup_service);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate service name 'api'")),
            "expected a duplicate-service refusal naming 'api'; got {:#?}",
            errs
        );

        let dup_group = r#"@verbose 0.1.0

concept_group G [max_depth: 10, max_nodes: 100]
  @intention: "first G"
  @source: invoices.intent:1
  concept E1
    @intention: "e1"
    @source: invoices.intent:1
    variants:
      A of (v: number)

concept_group G [max_depth: 20, max_nodes: 200]
  @intention: "second G"
  @source: invoices.intent:1
  concept E2
    @intention: "e2"
    @source: invoices.intent:1
    variants:
      B of (w: number)
"#;
        let errs = verify_str(dup_group);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate concept_group name 'G'")),
            "expected a duplicate-concept_group refusal naming 'G'; got {:#?}",
            errs
        );
    }

    #[test]
    fn phase8_log_rejects_if_else_in_content() {
        // The log scope is a closed grammar — control flow stays out so
        // the audit line shape is statically obvious from the source.
        let errs = verify_str(&http10_log_src(
            "if req.method == \"GET\" then \"got\" else \"other\"",
        ));
        assert!(
            errs.iter().any(|e| e.message.contains("not allowed in a log content")
                && e.message.contains("if/else")),
            "expected if/else rejection, got: {:#?}",
            errs
        );
    }

    /// Regression: when a model declares a lambda-bound field in the
    /// `reads:` proof (e.g. `b.copies` in `count(lib.books, b => b.copies < 5)`),
    /// the verifier rejects with the existing "extra: [...]" message AND
    /// appends a hint identifying `b` as lambda-bound. The hint was the
    /// load-bearing addition: the hold-out eval (2026-05-05) showed both
    /// Sonnet 4.6 and Opus 4.7 falling into this trap on intents that
    /// use a quantifier — without the hint, the diagnostic looked like
    /// any other "reads" mismatch and the model couldn't tell that
    /// removing the entry is the fix (not adding it).
    #[test]
    fn purity_extra_reads_hints_at_lambda_bound_var() {
        let errs = verify_str(
            r#"@verbose 0.1.0

concept Book
  @intention: "b"
  @source: invoices.intent:1

  fields:
    copies : number


concept Library
  @intention: "l"
  @source: invoices.intent:1

  fields:
    books : collection(Book)


rule low_stock_count
  @intention: "n"
  @source: invoices.intent:1

  input:
    lib : Library

  output:
    n : number

  logic:
    n = count(lib.books, b => b.copies < 5)

  proofs:
    purity:
      reads   : [lib.books, b.copies]
      calls   : []
    termination:
      bound : 4
"#,
        );
        let purity_err = errs
            .iter()
            .find(|e| e.message.contains("declared reads do not match logic"))
            .unwrap_or_else(|| panic!("no purity error in: {:#?}", errs));
        // The base error stays exactly as before (so existing
        // matchers / generators don't break).
        assert!(
            purity_err.message.contains("extra: [b.copies]"),
            "missing extra-reads breadcrumb; got: {}",
            purity_err.message,
        );
        // The new hint identifies `b` as lambda-bound and tells the
        // model what to do about it.
        assert!(
            purity_err.message.contains("hint:") && purity_err.message.contains("'b' is lambda-bound"),
            "missing lambda-bound hint; got: {}",
            purity_err.message,
        );
        assert!(
            purity_err.message.contains("do NOT belong in `reads:`"),
            "missing actionable instruction in hint; got: {}",
            purity_err.message,
        );
    }

    /// The hint must NOT fire when the extra read is just a stale
    /// input-field reference (no lambda binding involved). Otherwise
    /// the model would get told "remove this from `reads:`" for cases
    /// where the actual fix is to remove the dead field from the
    /// declaration. Keeps the hint specific to the lambda trap.
    #[test]
    fn purity_extra_reads_no_hint_when_not_lambda_bound() {
        let errs = verify_str(
            r#"@verbose 0.1.0

concept Inv
  @intention: "i"
  @source: invoices.intent:1

  fields:
    amount : number
    other  : number


rule check
  @intention: "c"
  @source: invoices.intent:1

  input:
    i : Inv

  output:
    ok : bool

  logic:
    ok = i.amount > 100

  proofs:
    purity:
      reads   : [i.amount, i.other]
      calls   : []
    termination:
      bound : 1
"#,
        );
        let purity_err = errs
            .iter()
            .find(|e| e.message.contains("declared reads do not match logic"))
            .unwrap_or_else(|| panic!("no purity error: {:#?}", errs));
        assert!(
            purity_err.message.contains("extra: [i.other]"),
            "missing extra-reads breadcrumb; got: {}",
            purity_err.message,
        );
        assert!(
            !purity_err.message.contains("hint:"),
            "hint should NOT fire for non-lambda-bound base ident; got: {}",
            purity_err.message,
        );
    }

    /// Phase A slice 2 — variant construction.
    ///
    /// A rule whose `output` is a sum-type concept can construct a variant
    /// in its logic via `ConceptName::VariantName { field: expr, ... }` or
    /// `ConceptName::VariantName` (no payload). The verifier cross-checks
    /// concept-name resolution, sum-type-ness, variant existence, and the
    /// payload field set against the declaration.
    ///
    /// Pinned cases:
    ///   (a) Happy path: variant with payload — accepts
    ///   (b) Happy path: variant without payload (`Token::Eof`) — accepts
    ///   (c) Unknown variant name → rejected with breadcrumb
    ///   (d) Missing payload field → rejected
    ///   (e) Extra payload field → rejected
    ///   (f) VariantConstruct on a record concept → rejected
    ///   (g) VariantConstruct on unknown concept → rejected
    #[test]
    fn phase_a2_variant_construct_verifier() {
        let common_concepts = r#"@verbose 0.1.0

concept Input
  @intention: "input record"
  @source: invoices.intent:1
  fields:
    id : number [0, 1000]

concept Token
  @intention: "a tagged token"
  @source: invoices.intent:1
  variants:
    Ident of (name : text)
    Int of (value : number)
    Eof

"#;

        let happy_payload = format!("{}{}", common_concepts, r#"rule make_int_token
  @intention: "wrap id into a Token::Int"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = Token::Int { value: i.id }
  proofs:
    purity:
      reads : [i.id]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&happy_payload);
        assert!(errs.is_empty(), "(a) happy-path payload should verify, got: {:#?}", errs);

        let happy_no_payload = format!("{}{}", common_concepts, r#"rule make_eof
  @intention: "produce Token::Eof regardless of input"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = Token::Eof
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&happy_no_payload);
        assert!(errs.is_empty(), "(b) no-payload variant should verify, got: {:#?}", errs);

        let unknown_variant = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = Token::Float { value: i.id }
  proofs:
    purity:
      reads : [i.id]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&unknown_variant);
        assert!(
            errs.iter().any(|e| e.message.contains("no variant named 'Float'")),
            "(c) unknown variant should be rejected: {:#?}", errs
        );

        let missing_field = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = Token::Int { }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&missing_field);
        assert!(
            errs.iter().any(|e| e.message.contains("missing payload field 'value'")),
            "(d) missing field should be rejected: {:#?}", errs
        );

        let extra_field = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = Token::Int { value: i.id, junk: 99 }
  proofs:
    purity:
      reads : [i.id]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&extra_field);
        assert!(
            errs.iter().any(|e| e.message.contains("unknown payload field 'junk'")),
            "(e) extra field should be rejected: {:#?}", errs
        );

        let on_record_concept = r#"@verbose 0.1.0

concept RecordConcept
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : number

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    r : RecordConcept
  output:
    r2 : RecordConcept
  logic:
    r2 = RecordConcept::Foo
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1
"#;
        let errs = verify_str(on_record_concept);
        assert!(
            errs.iter().any(|e| e.message.contains("is a record concept")),
            "(f) variant construction on record concept should be rejected: {:#?}", errs
        );

        let unknown_concept = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    t : Token
  logic:
    t = NonExistent::Foo
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&unknown_concept);
        assert!(
            errs.iter().any(|e| e.message.contains("unknown concept 'NonExistent'")),
            "(g) unknown concept should be rejected: {:#?}", errs
        );
    }

    /// Phase A slice 3 — pattern match across the variants of a sum-type
    /// concept. A rule whose input is a sum-type concept can destructure
    /// it with `match e: VarA(...) => ... ; VarB(...) => ... ; VarC => ...`.
    /// The verifier cross-checks the scrutinee's resolved concept, the
    /// arm-variant set (exhaustiveness + no extras + no duplicates), the
    /// per-arm binder arity, and the per-arm body type against the rule's
    /// declared output type. Binders introduced by an arm are lambda-bound
    /// for purity (so the body's `reads:` proof does not flag them as
    /// extra external reads).
    ///
    /// Pinned cases:
    ///   (a) Happy path: exhaustive match on a 3-variant concept — accepts
    ///   (b) Missing arm (non-exhaustive) → rejected
    ///   (c) Unknown variant name → rejected
    ///   (d) Wrong binder count → rejected
    ///   (e) Duplicate arm for same variant → rejected
    ///   (f) Match on a record concept → rejected
    ///   (g) Match on unresolvable scrutinee → rejected
    ///   (h) Duplicate binder within one arm → rejected
    #[test]
    fn phase_a3_match_variant_verifier() {
        let common_concepts = r#"@verbose 0.1.0

concept Token
  @intention: "a tagged token"
  @source: invoices.intent:1
  variants:
    Ident of (name : text)
    Int of (value : number)
    Eof

"#;

        // (a) Happy: exhaustive match, each arm produces a number.
        let happy = format!("{}{}", common_concepts, r#"rule token_length
  @intention: "compute a numeric proxy for the token"
  @source: invoices.intent:1
  input:
    t : Token
  output:
    n : number
  logic:
    n = match t:
      Ident(_) => 1
      Int(v) => v
      Eof => 0

  proofs:
    purity:
      reads : [t]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&happy);
        assert!(errs.is_empty(), "(a) exhaustive match should verify, got: {:#?}", errs);

        // (b) Missing arm → non-exhaustive.
        let missing_arm = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : Token
  output:
    n : number
  logic:
    n = match t:
      Ident(_) => 1
      Int(v) => v

  proofs:
    purity:
      reads : [t]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&missing_arm);
        assert!(
            errs.iter().any(|e| e.message.contains("not exhaustive") && e.message.contains("Token::Eof")),
            "(b) missing-Eof arm should be rejected: {:#?}", errs
        );

        // (c) Unknown variant name.
        let unknown_arm = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : Token
  output:
    n : number
  logic:
    n = match t:
      Ident(_) => 1
      Int(v) => v
      Eof => 0
      Float(x) => x

  proofs:
    purity:
      reads : [t]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&unknown_arm);
        assert!(
            errs.iter().any(|e| e.message.contains("unknown variant 'Token::Float'")),
            "(c) unknown variant should be rejected: {:#?}", errs
        );

        // (d) Wrong binder count.
        let wrong_arity = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : Token
  output:
    n : number
  logic:
    n = match t:
      Ident(a, b) => 1
      Int(v) => v
      Eof => 0

  proofs:
    purity:
      reads : [t]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&wrong_arity);
        assert!(
            errs.iter().any(|e| e.message.contains("Token::Ident") && e.message.contains("2 binder") && e.message.contains("1 field")),
            "(d) wrong arity should be rejected with arity diagnostic: {:#?}", errs
        );

        // (e) Duplicate arm for the same variant.
        let dup_arm = format!("{}{}", common_concepts, r#"rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    t : Token
  output:
    n : number
  logic:
    n = match t:
      Ident(_) => 1
      Ident(_) => 2
      Int(v) => v
      Eof => 0

  proofs:
    purity:
      reads : [t]
      calls : []
    termination:
      bound : 1
"#);
        let errs = verify_str(&dup_arm);
        assert!(
            errs.iter().any(|e| e.message.contains("Token::Ident") && e.message.contains("duplicated")),
            "(e) duplicate arm should be rejected: {:#?}", errs
        );

        // (f) Match on a record concept.
        let on_record = r#"@verbose 0.1.0

concept Recd
  @intention: "x"
  @source: invoices.intent:1
  fields:
    a : number

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    r : Recd
  output:
    n : number
  logic:
    n = match r:
      Foo => 0

  proofs:
    purity:
      reads : [r]
      calls : []
    termination:
      bound : 1
"#;
        let errs = verify_str(on_record);
        assert!(
            errs.iter().any(|e| e.message.contains("record concept")),
            "(f) match on record should be rejected: {:#?}", errs
        );

        // (h) Duplicate binder within one arm.
        let dup_binder = r#"@verbose 0.1.0

concept Pair
  @intention: "x"
  @source: invoices.intent:1
  variants:
    Two of (a : number, b : number)

rule bad
  @intention: "x"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    n : number
  logic:
    n = match p:
      Two(x, x) => x

  proofs:
    purity:
      reads : [p]
      calls : []
    termination:
      bound : 1
"#;
        let errs = verify_str(dup_binder);
        assert!(
            errs.iter().any(|e| e.message.contains("Pair::Two") && e.message.contains("binds 'x' twice")),
            "(h) duplicate binder should be rejected: {:#?}", errs
        );
    }

    // ── Phase B slice 1 — concept_group declaration ─────────────────
    //
    // A `concept_group` declares mutually-recursive sum-type concepts
    // sharing a single set of `[max_depth, max_nodes]` bounds. Slice 1
    // is parser + verifier only: the construct is accepted at the top
    // level, refused inside a rule that consumes it, and rejected when
    // its bounds are absurd. See docs/recursive-types-design.md §4 / §5.

    const VALID_GROUP_SRC: &str = r#"@verbose 0.1.0

concept_group AST [max_depth: 30, max_nodes: 5000]
  @intention: "a tiny AST"
  @source: invoices.intent:1

  concept Expr
    @intention: "an expression"
    @source: invoices.intent:1
    variants:
      Int    of (value : number)
      Binary of (op : text, lhs : Expr, rhs : Expr)

  concept Stmt
    @intention: "a statement"
    @source: invoices.intent:1
    variants:
      Return of (e : Expr)
      Skip
"#;

    #[test]
    fn phase_b1_concept_group_parses() {
        // Confirms the parser materialises a `ConceptGroup` with the
        // declared header bounds and the inner concepts in source
        // order. No verifier interaction — just the AST shape.
        let tokens = crate::lexer::Lexer::new(VALID_GROUP_SRC).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();
        let group = program.items.iter().find_map(|it| match it {
            Item::ConceptGroup(g) => Some(g),
            _ => None,
        }).expect("expected a ConceptGroup item");
        assert_eq!(group.name, "AST");
        assert_eq!(group.max_depth, 30);
        assert_eq!(group.max_nodes, 5000);
        assert_eq!(group.concepts.len(), 2);
        assert_eq!(group.concepts[0].name, "Expr");
        assert_eq!(group.concepts[1].name, "Stmt");
        // Inner concepts must be sum-typed.
        assert!(group.concepts[0].fields.is_empty());
        assert_eq!(group.concepts[0].variants.len(), 2);
        // The recursive Binary variant references Expr in its payload.
        let binary = &group.concepts[0].variants[1];
        assert_eq!(binary.name, "Binary");
        assert!(matches!(binary.fields[1].ty, Type::Named(ref n) if n == "Expr"));
    }

    #[test]
    fn phase_b1_concept_group_verifies() {
        // A well-formed group with no consuming rule must verify clean.
        let errs = verify_str(VALID_GROUP_SRC);
        assert!(
            errs.is_empty(),
            "expected no verify errors on a valid concept_group, got {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b1_rejects_zero_max_depth() {
        let src = r#"@verbose 0.1.0

concept_group AST [max_depth: 0, max_nodes: 100]
  @intention: "x"
  @source: invoices.intent:1

  concept Expr
    @intention: "e"
    @source: invoices.intent:1
    variants:
      Int of (n : number)
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("max_depth")
                && e.message.contains("greater than zero")),
            "expected max_depth=0 rejection, got {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b1_rejects_zero_max_nodes() {
        let src = r#"@verbose 0.1.0

concept_group AST [max_depth: 10, max_nodes: 0]
  @intention: "x"
  @source: invoices.intent:1

  concept Expr
    @intention: "e"
    @source: invoices.intent:1
    variants:
      Int of (n : number)
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("max_nodes")
                && e.message.contains("greater than zero")),
            "expected max_nodes=0 rejection, got {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b1_accepts_max_nodes_over_65535_now_mmap_backed() {
        // Off-stack mmap arena (2026-06-22): the 16-bit ceiling is lifted.
        // 100000 nodes (was rejected pre-mmap) is now accepted — the arena
        // is mmap-backed, indices are 64-bit, and 100000 is well under the
        // 8_000_000 ceiling.
        let src = r#"@verbose 0.1.0

concept_group AST [max_depth: 10, max_nodes: 100000]
  @intention: "x"
  @source: invoices.intent:1

  concept Expr
    @intention: "e"
    @source: invoices.intent:1
    variants:
      Int of (n : number)
"#;
        let errs = verify_str(src);
        assert!(
            !errs.iter().any(|e| e.context.contains("max_nodes")),
            "expected max_nodes=100000 to be ACCEPTED now (mmap-backed), got {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b1_rejects_max_nodes_over_8m() {
        // The mmap arena is off-stack but still bounded — node counts past
        // the 8_000_000 ceiling are refused. The ceiling rose 4M -> 8M for
        // the stdin-channel self-hosting milestone (the full self-source
        // parse peaks ~5.3M nodes; the working VExpr max_nodes is 6M).
        let src = r#"@verbose 0.1.0

concept_group AST [max_depth: 10, max_nodes: 9000000]
  @intention: "x"
  @source: invoices.intent:1

  concept Expr
    @intention: "e"
    @source: invoices.intent:1
    variants:
      Int of (n : number)
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("max_nodes")
                && e.message.contains("ceiling")),
            "expected max_nodes=9000000 rejection, got {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b3_rule_using_group_type_verifies_for_interpreter() {
        // Phase B slice 3 lifts the slice-1 verifier refusal: a rule
        // whose input or output is a concept declared inside a
        // `concept_group` is now ACCEPTED at verify time and runnable
        // via `--run`. Native still refuses (slice B.4+ wires arena
        // emit); that refusal moves to `compile_native_code`.
        let src = r#"@verbose 0.1.0

concept_group AST [max_depth: 5, max_nodes: 50]
  @intention: "x"
  @source: invoices.intent:1

  concept Expr
    @intention: "e"
    @source: invoices.intent:1
    variants:
      Int of (n : number)

rule ok
  @intention: "y"
  @source: invoices.intent:1
  input:
    e : Expr
  output:
    n : number
  logic:
    n = 0
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.is_empty(),
            "B.3 lifted the slice-1 refusal; this rule must verify cleanly. Got: {:#?}",
            errs
        );
    }

    #[test]
    fn phase_b1_iter_all_concepts_includes_group_concepts() {
        // The `iter_all_concepts` helper must surface concepts declared
        // inside a concept_group; otherwise downstream consumers
        // (name-resolution, codegen, optimizer) would silently treat
        // group concepts as undeclared.
        let tokens = crate::lexer::Lexer::new(VALID_GROUP_SRC).tokenize().unwrap();
        let program = crate::parser::Parser::new(tokens).parse_program().unwrap();
        let names: Vec<&str> = iter_all_concepts(&program.items).map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"Expr"), "expected Expr in iter, got {:?}", names);
        assert!(names.contains(&"Stmt"), "expected Stmt in iter, got {:?}", names);
    }

    // ─── Mutable state: verifier tests ───────────────────────────────────

    #[test]
    fn service_state_after_set_unknown_field_rejected() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "ok" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    counter : number = 0
  after:
    set nonexistent = 1
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.context.contains("after") && e.message.contains("nonexistent")),
            "expected unknown-state-field error in after block, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_state_duplicate_field_rejected() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: "ok" }
  proofs:
    purity:
      reads : []
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    counter : number = 0
    counter : number = 5
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("duplicate state field")),
            "expected duplicate-state-field error, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_state_handler_reads_cross_checked() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: concat("c:", state.bogus) }
  proofs:
    purity:
      reads : [state.bogus]
      calls : []
    termination:
      bound : 1

service s
  @intention: "test"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    counter : number = 0
"#;
        let errs = verify_str(src);
        assert!(
            errs.iter().any(|e| e.message.contains("state.bogus") && e.message.contains("no state field")),
            "expected cross-check error for state.bogus, got: {:#?}",
            errs
        );
    }

    #[test]
    fn service_state_valid_counter_passes() {
        let src = r#"@verbose 0.1.0

rule h
  @intention: "t"
  @source: invoices.intent:1
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    resp = HttpResponse { status: 200, body: concat("count:", state.counter) }
  proofs:
    purity:
      reads : [state.counter]
      calls : []
    termination:
      bound : 3

service s
  @intention: "test"
  @source: invoices.intent:1
  listen:
    protocol: http_1_0
    port: 9999
    max_request: 4096
  handler: h
  state:
    counter : number = 0
  after:
    set counter = state.counter + 1
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "valid counter service should verify cleanly, got: {:#?}", errs);
    }
}
