use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path as StdPath;

use crate::ast::*;

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
        vec![builtin_http_request(), builtin_http_response()]
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
                verify_rule(r, &concepts, &all_rules, &all_resources, &all_connections, &group_concept_owner, base_dir, &mut errors);
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
                            check_expr_against(
                                content,
                                &Type::Text,
                                rule,
                                &all_rules,
                                input_concept,
                                &concepts,
                                &mut errors,
                            );
                        }
                    }
                }
            }
            Item::Service(s) => verify_service(s, &concepts, &all_rules, base_dir, &mut errors),
            Item::Resource(r) => verify_resource_stub(r, base_dir, &mut errors),
            Item::Connection(c) => verify_connection_stub(c, base_dir, &mut errors),
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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => {}
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
        Expr::JsonEscape(inner) => collect_read_names(inner, out),
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
        Expr::Abs(inner) => collect_read_names(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children; either
        // side may carry a `read(...)` (e.g. `min(amount, parse_int(read(cap)))`).
        Expr::Min(l, r) | Expr::Max(l, r) => {
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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => {}
        Expr::Read(_) => {}
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
        Expr::JsonEscape(inner) => collect_fetch_names(inner, out),
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
        Expr::Abs(inner) => collect_fetch_names(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) => {
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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => {}
        Expr::Read(_) => {}
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
        Expr::JsonEscape(inner) => collect_fetch_names_with_dups(inner, out),
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
        Expr::Abs(inner) => collect_fetch_names_with_dups(inner, out),
        // `min(a, b)` / `max(a, b)` — recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) => {
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
///   body   : text [..4096] — the bytes after the \r\n\r\n delimiter; capped
///                            by the service's `max_request` at runtime.
///                            Stored as (ptr, len) — body may contain
///                            arbitrary bytes so NUL-termination is unsafe.
fn builtin_http_request() -> Concept {
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
                range: Some((0, 4096)),
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
fn verify_service(
    s: &Service,
    concepts: &HashMap<String, &Concept>,
    all_rules: &[&Rule],
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

    // Phase 10 slice 10: forked concurrency is currently restricted to
    // http_1_0. raw_tcp services that fork would also need the parent to
    // close the client fd before re-entering accept (same shape) but the
    // recon explicitly scoped this slice to Http10; lifting the
    // restriction is one slice, not a free side-effect.
    if s.concurrency == ConcurrencyMode::Forked && s.protocol != Protocol::Http10 {
        errors.push(VerifyError {
            context: format!("service '{}' / concurrency", s.name),
            message: "Phase 10: concurrency: forked currently restricted to http_1_0 services".into(),
        });
    }

    // Mutable state validation.
    // 1. Each state field must be Number-typed (text state is a follow-up).
    // 2. No duplicate field names.
    // 3. Each after_set must reference an existing state field.
    // 4. State is restricted to http_1_0 services in this slice.
    if !s.state_fields.is_empty() && s.protocol != Protocol::Http10 {
        errors.push(VerifyError {
            context: format!("service '{}' / state", s.name),
            message: "mutable state is currently restricted to http_1_0 services".into(),
        });
    }
    {
        let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for sf in &s.state_fields {
            if sf.ty != Type::Number {
                errors.push(VerifyError {
                    context: format!("service '{}' / state / {}", s.name, sf.name),
                    message: format!(
                        "state field '{}' must be type 'number' in this slice; got {:?}",
                        sf.name, sf.ty
                    ),
                });
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
        Expr::Fetch(_, _) => "fetch",
        Expr::JsonEscape(_) => "json_escape",
        Expr::ParseInt(_) => "parse_int",
        Expr::NowUnix => "now_unix",
        Expr::StartsWith(_, _) => "starts_with",
        Expr::Contains(_, _) => "contains",
        Expr::EndsWith(_, _) => "ends_with",
        Expr::Length(_) => "length",
        Expr::Abs(_) => "abs",
        Expr::Min(_, _) => "min",
        Expr::Max(_, _) => "max",
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

/// Phase B slice 1: bounds ceiling on `max_depth` and `max_nodes`. Picked
/// at 65535 (16-bit) so the future arena-emitter (B.4+) can use 16-bit
/// indices unconditionally — see docs/recursive-types-design.md §6 / Q2.
/// The actual emitter exploitation lands in B.4; B.1 just refuses
/// declarations that would later force a wider index width.
const PHASE_B1_MAX_BOUND: u32 = 65535;

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
    if g.max_depth > PHASE_B1_MAX_BOUND {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_depth", g.name),
            message: format!(
                "max_depth {} exceeds the slice-1 ceiling of {} (16-bit index budget — see docs/recursive-types-design.md §6 / Q2)",
                g.max_depth, PHASE_B1_MAX_BOUND
            ),
        });
    }
    if g.max_nodes == 0 {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_nodes", g.name),
            message: "max_nodes must be greater than zero — a recursive tree must allow at least one node".into(),
        });
    }
    if g.max_nodes > PHASE_B1_MAX_BOUND {
        errors.push(VerifyError {
            context: format!("concept_group '{}' / max_nodes", g.name),
            message: format!(
                "max_nodes {} exceeds the slice-1 ceiling of {} (16-bit index budget — see docs/recursive-types-design.md §6 / Q2)",
                g.max_nodes, PHASE_B1_MAX_BOUND
            ),
        });
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
        rule, all_rules, all_resources, all_connections, &mut facts,
    );

    for path in &facts.reads {
        if let Some(msg) = validate_read_path(path, rule, input_concept, all_resources, all_connections) {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: msg,
            });
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

    check_purity(rule, &facts, errors);
    check_termination(rule, concepts, group_concept_owner, errors);

    if let Some(hints) = &rule.hints {
        check_hints(rule, hints, &facts, concepts, errors);
    }

    if let Some(caller_layer) = rule.layer {
        check_layer_discipline(rule, caller_layer, &facts, all_rules, errors);
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
fn check_expr_against(
    expr: &Expr,
    expected: &Type,
    rule: &Rule,
    all_rules: &[&Rule],
    input_concept: Option<&Concept>,
    all_concepts: &HashMap<String, &Concept>,
    errors: &mut Vec<VerifyError>,
) {
    match (expr, expected) {
        (Expr::Ok(inner), Type::Result(t, _)) => {
            check_expr_against(inner, t, rule, all_rules, input_concept, all_concepts, errors);
        }
        (Expr::Err(inner), Type::Result(_, e)) => {
            check_expr_against(inner, e, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(cond, &Type::Bool, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(then_e, expected, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(else_e, expected, rule, all_rules, input_concept, all_concepts, errors);
        }
        // Phase 11 slice 1: fetch(<connection>, <request_bytes>) — request
        // bytes must produce text. The fetch itself produces text; the
        // outer-context check is handled by the fall-through arm via
        // `infer_expr_type(Expr::Fetch(..))` returning Text.
        (Expr::Fetch(_, req), expected_outer) => {
            check_expr_against(req, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(inner, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(inner, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(h, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(n, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(h, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(n, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(h, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(n, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
        // context expects number, recurse into the inner with expected=Text.
        // Otherwise surface a clear mismatch (mirror of the ParseInt arms).
        (Expr::Length(inner), Type::Number) => {
            check_expr_against(inner, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(inner, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(l, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(r, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
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
        // `substring(<text>, <start>, <end>)` produces text. When the context
        // expects text, recurse into the first child with expected=Text and
        // into start/end with expected=Number so non-conforming argument types
        // are rejected through the usual channel. Otherwise surface a clear
        // mismatch (mirror of the JsonEscape/Length arms but with three
        // children).
        (Expr::Substring(t, s, e), Type::Text) => {
            check_expr_against(t, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(s, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(e, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
        }
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
        (Expr::ByteAt(t, i), Type::Number) => {
            check_expr_against(t, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(i, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(t, &Type::Text, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(init, &Type::Number, rule, all_rules, input_concept, all_concepts, errors);
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
            check_expr_against(ok_body, expected, rule, all_rules, input_concept, all_concepts, errors);
            check_expr_against(err_body, expected, rule, all_rules, input_concept, all_concepts, errors);
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
                if let Some(inferred) = infer_expr_type(arg, rule, all_rules, input_concept) {
                    match inferred {
                        Type::Number | Type::Bool | Type::Text => {}
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
        (Expr::Concat(_), other) => {
            errors.push(VerifyError {
                context: format!("rule '{}' / logic", rule.name),
                message: format!(
                    "concat produces text but the expected type is '{}'",
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
            let concept_name = match infer_expr_type(scrutinee, rule, all_rules, input_concept) {
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
                check_expr_against(&arm.body, expected, rule, all_rules, input_concept, all_concepts, errors);
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
                        errors,
                    );
                }
            }
        }
        _ => {
            if let Some(inferred) = infer_expr_type(expr, rule, all_rules, input_concept) {
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
) -> Option<Type> {
    match expr {
        Expr::Number(_) => Some(Type::Number),
        Expr::Text(_) => Some(Type::Text),
        // Phase 9 slice 1: read(<resource>) returns text. Existence of the
        // resource is checked separately by verify_rule via a dedicated
        // walk; this inference path only needs the type.
        Expr::Read(_) => Some(Type::Text),
        Expr::Ident(name) if name == &rule.input_name => Some(rule.input_ty.clone()),
        Expr::Ident(_) => None, // let/lambda-bound — not tracked in this pass
        Expr::Field(base, field_name) => {
            if let (Expr::Ident(n), Some(c)) = (base.as_ref(), concept) {
                if n == &rule.input_name {
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
        Expr::If(_, then_e, _) => infer_expr_type(then_e, rule, all_rules, concept),
        Expr::Quantifier(_, _, _, _) => Some(Type::Bool),
        Expr::Record(name, _) => Some(Type::Named(name.clone())),
        // Phase A slice 2: variant construction yields the concept type —
        // same outer shape as record construction.
        Expr::VariantConstruct(name, _, _) => Some(Type::Named(name.clone())),
        Expr::Concat(_) => Some(Type::Text),
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
        Expr::Abs(_) => Some(Type::Number),
        // `min(<number>, <number>)` / `max(<number>, <number>)` return number.
        // Both children are number-typed; check_expr_against enforces that.
        Expr::Min(_, _) | Expr::Max(_, _) => Some(Type::Number),
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
        &mut visited,
        &mut facts.reads,
    );
    for (_, expr) in &rule.logic.bindings {
        walk_for_match_result_callees(
            expr,
            &rules_by_name,
            all_resources,
            all_connections,
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
                            visited,
                            out_reads,
                        );
                        for (_, e) in &callee.logic.bindings {
                            walk_for_match_result_callees(
                                e,
                                rules_by_name,
                                all_resources,
                                all_connections,
                                visited,
                                out_reads,
                            );
                        }
                    }
                }
            }
            walk_for_match_result_callees(ok_body, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(err_body, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        // Recurse into shapes that can contain a MatchResult somewhere.
        Expr::If(c, t, e) => {
            walk_for_match_result_callees(c, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(e, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Ok(i) | Expr::Err(i) | Expr::Not(i) | Expr::Neg(i) | Expr::Abs(i)
        | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => {
            walk_for_match_result_callees(i, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Binary(_, l, r) => {
            walk_for_match_result_callees(l, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(r, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Min(a, b) | Expr::Max(a, b) | Expr::StartsWith(a, b)
        | Expr::EndsWith(a, b) | Expr::Contains(a, b) => {
            walk_for_match_result_callees(a, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(b, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Call(_, args) | Expr::Concat(args) => {
            for a in args {
                walk_for_match_result_callees(a, rules_by_name, all_resources, all_connections, visited, out_reads);
            }
        }
        Expr::Record(_, fields) => {
            for (_, v) in fields {
                walk_for_match_result_callees(v, rules_by_name, all_resources, all_connections, visited, out_reads);
            }
        }
        Expr::Fetch(_, req) => {
            walk_for_match_result_callees(req, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Fold(coll, init, _, _, body) => {
            walk_for_match_result_callees(coll, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(init, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Quantifier(_, coll, _, body)
        | Expr::Map(coll, _, body)
        | Expr::Filter(coll, _, body) => {
            walk_for_match_result_callees(coll, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::Substring(t, s, e) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(s, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(e, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::ByteAt(t, i) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(i, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        Expr::FoldBytes(t, init, _, _, _, body) => {
            walk_for_match_result_callees(t, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(init, rules_by_name, all_resources, all_connections, visited, out_reads);
            walk_for_match_result_callees(body, rules_by_name, all_resources, all_connections, visited, out_reads);
        }
        // Phase A slice 2: recurse into each field assignment's expression.
        Expr::VariantConstruct(_, _, fields) => {
            for (_, v) in fields {
                walk_for_match_result_callees(v, rules_by_name, all_resources, all_connections, visited, out_reads);
            }
        }
        // Phase A slice 3: pattern match — recurse into scrutinee + each
        // arm's body. The MatchVariant itself doesn't have a Call target
        // shape (unlike MatchResult), so no inlined-callee fact propagation
        // here; we just walk for any MatchResult nested inside the bodies.
        Expr::MatchVariant(scrutinee, arms) => {
            walk_for_match_result_callees(scrutinee, rules_by_name, all_resources, all_connections, visited, out_reads);
            for a in arms {
                walk_for_match_result_callees(&a.body, rules_by_name, all_resources, all_connections, visited, out_reads);
            }
        }
        Expr::Number(_) | Expr::Text(_) | Expr::Field(_, _) | Expr::Ident(_)
        | Expr::Read(_) | Expr::NowUnix => {}
    }
}

fn collect_logic_facts(logic: &LogicStmt) -> LogicFacts {
    let mut facts = LogicFacts::default();
    let binding_names: HashSet<String> = logic.bindings.iter().map(|(n, _)| n.clone()).collect();
    for (_, expr) in &logic.bindings {
        collect_expr_facts(expr, &mut facts.reads, &mut facts.calls);
    }
    collect_expr_facts(&logic.value, &mut facts.reads, &mut facts.calls);
    // Remove reads that reference let-bound names (they're local, not field reads)
    facts.reads.retain(|path| {
        path.first().map_or(true, |name| !binding_names.contains(name))
    });
    facts
}

fn collect_expr_facts(
    expr: &Expr,
    reads: &mut HashSet<Vec<String>>,
    calls: &mut HashSet<Vec<String>>,
) {
    match expr {
        Expr::Number(_) | Expr::Text(_) => {}
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
        Expr::JsonEscape(inner) => {
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
        // `min(a, b)` / `max(a, b)` — pure: branch-free scalar comparison
        // adds no synthetic read; each child contributes its own facts.
        Expr::Min(l, r) | Expr::Max(l, r) => {
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
    all_resources: &HashSet<String>,
    all_connections: &HashSet<String>,
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
        // For context fields, we don't have the concept here to validate field names,
        // so skip field validation (the native backend will catch unknown fields).
        if let Some(c) = input_concept {
            if is_input {
                let field_name = &path[1];
                if !c.fields.iter().any(|f| &f.name == field_name) {
                    return Some(format!(
                        "concept '{}' has no field '{}' (accessed via '{}')",
                        c.name,
                        field_name,
                        path.join(".")
                    ));
                }
            }
        }
    }
    None
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
            | Expr::ParseInt(i) | Expr::JsonEscape(i) | Expr::Ok(i) | Expr::Err(i) => {
                walk(i, out);
            }
            Expr::Min(a, b) | Expr::Max(a, b) | Expr::StartsWith(a, b)
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
            // Leaves
            Expr::Number(_) | Expr::Text(_) | Expr::Field(_, _) | Expr::Ident(_)
            | Expr::Read(_) | Expr::NowUnix => {}
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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) | Expr::Read(_) | Expr::NowUnix => {}
        Expr::Field(b, _) => collect_recursive_call_args(b, rule_name, out),
        Expr::Binary(_, l, r) => { collect_recursive_call_args(l, rule_name, out); collect_recursive_call_args(r, rule_name, out); }
        Expr::Not(i) | Expr::Neg(i) | Expr::Ok(i) | Expr::Err(i)
        | Expr::Abs(i) | Expr::Length(i) | Expr::ParseInt(i) | Expr::JsonEscape(i) => collect_recursive_call_args(i, rule_name, out),
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
        Expr::StartsWith(h, n) | Expr::Contains(h, n) | Expr::EndsWith(h, n)
        | Expr::Min(h, n) | Expr::Max(h, n) | Expr::ByteAt(h, n) => { collect_recursive_call_args(h, rule_name, out); collect_recursive_call_args(n, rule_name, out); }
        Expr::Substring(t, s, e) => { collect_recursive_call_args(t, rule_name, out); collect_recursive_call_args(s, rule_name, out); collect_recursive_call_args(e, rule_name, out); }
    }
}

fn count_operations(expr: &Expr) -> usize {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => 0,
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
        // Phase 11 slice 1: a TCP fetch costs roughly one op (the
        // socket+connect+write+read syscall sequence is opaque to the
        // proof system) plus the cost of evaluating the request bytes.
        Expr::Fetch(_, req) => 1 + count_operations(req),
        // Phase 12 (json_escape): one op for the transform itself plus
        // the cost of evaluating the inner expression. Same shape as
        // Ok/Err's pass-through accounting.
        Expr::JsonEscape(inner) => 1 + count_operations(inner),
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
        // `min(a, b)` / `max(a, b)` — branch-free scalar; one op + each child.
        Expr::Min(l, r) | Expr::Max(l, r) => 1 + count_operations(l) + count_operations(r),
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
        // Integration guard: every .verbose file with a matching .json must
        // execute without runtime panic. Value::Err (a declared failure path)
        // is allowed — only eval_rule returning Err (missing field, type
        // mismatch, etc.) counts as failure. Covers the "interpreter silently
        // regressed on an example" class of bugs that parse+verify misses.
        use crate::interpreter::{eval_rule, load_json_input};
        use std::fs;

        fn collect(dir: &StdPath, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
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
                let result = eval_rule(rule, &all_rules, &all_concepts, record);
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
        // Integration guard: every file under examples/ that ends in .verbose
        // must parse cleanly and verify with zero errors. If this test goes
        // red, an example or the language has drifted — the failing file name
        // and the verifier output point straight at the cause.
        use std::fs;

        fn collect(dir: &StdPath, out: &mut Vec<std::path::PathBuf>) {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
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
    fn phase_b1_rejects_max_nodes_over_65535() {
        // Verifier refuses node counts past 16-bit so the future
        // arena emitter can use 16-bit indices unconditionally
        // (docs/recursive-types-design.md §6 / Q2).
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
            errs.iter().any(|e| e.context.contains("max_nodes")
                && e.message.contains("16-bit")),
            "expected max_nodes=100000 rejection with 16-bit breadcrumb, got {:#?}",
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
