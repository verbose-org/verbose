/// Platform-independent AST optimizations.
///
/// These transformations work on the expression tree BEFORE any backend
/// sees it. They reduce, simplify, and eliminate — producing a smaller
/// AST that every backend (x86, ARM, RISC-V, WASM) benefits from equally.
///
/// This is the "universal optimization layer":
///   IR → verifier → optimizer → backend (platform-specific)
///
/// The optimizer never changes semantics. Every transformation preserves
/// the original result for all valid inputs.

use std::collections::HashMap;

use crate::ast::*;
use crate::verifier::compute_range;

#[derive(Debug, Default)]
pub struct OptStats {
    pub nodes_before: usize,
    pub nodes_after: usize,
}

impl std::fmt::Display for OptStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let eliminated = self.nodes_before.saturating_sub(self.nodes_after);
        if eliminated == 0 {
            write!(f, "  (no AST nodes eliminated)")
        } else {
            write!(
                f,
                "  {} AST node(s) eliminated ({} → {})",
                eliminated, self.nodes_before, self.nodes_after
            )
        }
    }
}

fn count_nodes(expr: &Expr) -> usize {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) => 1,
        Expr::Field(base, _) => 1 + count_nodes(base),
        Expr::Binary(_, l, r) => 1 + count_nodes(l) + count_nodes(r),
        Expr::Not(i) | Expr::Neg(i) => 1 + count_nodes(i),
        Expr::If(c, t, e) => 1 + count_nodes(c) + count_nodes(t) + count_nodes(e),
        Expr::Call(_, args) => 1 + args.iter().map(count_nodes).sum::<usize>(),
        Expr::Quantifier(_, c, _, p) => 1 + count_nodes(c) + count_nodes(p),
        Expr::Fold(c, i, _, _, b) => 1 + count_nodes(c) + count_nodes(i) + count_nodes(b),
        Expr::Map(c, _, b) | Expr::Filter(c, _, b) => 1 + count_nodes(c) + count_nodes(b),
        Expr::Ok(inner) | Expr::Err(inner) => 1 + count_nodes(inner),
        Expr::MatchResult(t, _, ob, _, eb) => 1 + count_nodes(t) + count_nodes(ob) + count_nodes(eb),
        Expr::Record(_, fields) => 1 + fields.iter().map(|(_, e)| count_nodes(e)).sum::<usize>(),
        Expr::Concat(args) => 1 + args.iter().map(count_nodes).sum::<usize>(),
        // Phase 9 slice 1 stub: a Read carries a resource name (no child Expr
        // to recurse on), so it counts as one node.
        Expr::Read(_) => 1,
        // Phase 11 slice 1: a Fetch carries the connection name plus a
        // request bytes Expr — count this node + recurse.
        Expr::Fetch(_, req) => 1 + count_nodes(req),
        // Phase 12 (json_escape): one node + recurse on the inner.
        Expr::JsonEscape(inner) | Expr::BitNot(inner) => 1 + count_nodes(inner),
        // Phase 12 (parse_int): same shape as JsonEscape — one node + recurse.
        Expr::ParseInt(inner) => 1 + count_nodes(inner),
        // `now_unix()` — leaf node (no children to recurse on), counts as one.
        Expr::NowUnix => 1,
        // `starts_with(haystack, needle)` — count this node + recurse into
        // both children (same shape as Binary).
        Expr::StartsWith(h, n) => 1 + count_nodes(h) + count_nodes(n),
        // `contains(haystack, needle)` — same shape as StartsWith: count
        // this node + recurse into both children.
        Expr::Contains(h, n) => 1 + count_nodes(h) + count_nodes(n),
        // `ends_with(haystack, needle)` — same shape as StartsWith.
        Expr::EndsWith(h, n) => 1 + count_nodes(h) + count_nodes(n),
        // `length(<text_expr>)` — same shape as ParseInt: one node + recurse.
        Expr::Length(inner) => 1 + count_nodes(inner),
        // `abs(<number_expr>)` — same shape as Neg: one node + recurse.
        Expr::Abs(inner) | Expr::BitNot(inner) => 1 + count_nodes(inner),
        // `le32(n)` / `le64(n)` — one node + recurse (same shape as Abs).
        Expr::Le32(inner) | Expr::Le64(inner) => 1 + count_nodes(inner),
        // `arena_scope(inner)` — one node + recurse (transparent wrapper).
        Expr::ArenaScope(inner) => 1 + count_nodes(inner),
        // `min(a, b)` / `max(a, b)` — same shape as Binary: count this node
        // + recurse into both children.
        Expr::Min(l, r) | Expr::Max(l, r) | Expr::BitAnd(l, r) | Expr::BitOr(l, r) | Expr::BitXor(l, r) | Expr::Shl(l, r) | Expr::Shr(l, r) => 1 + count_nodes(l) + count_nodes(r),
        // `substring(text, start, end)` — three children, all expressions.
        Expr::Substring(t, s, e) => 1 + count_nodes(t) + count_nodes(s) + count_nodes(e),
        // `byte_at(text, index)` — two children, all expressions.
        Expr::ByteAt(t, i) => 1 + count_nodes(t) + count_nodes(i),
        // `fold_bytes(text, init, acc, byte, idx => body)` — three Expr
        // children (text, init, body); the three bound names are strings,
        // not nodes.
        Expr::FoldBytes(t, init, _, _, _, body) => {
            1 + count_nodes(t) + count_nodes(init) + count_nodes(body)
        }
        // Phase A slice 2: variant construction — one node + each payload
        // field's expression cost. Same shape as Record.
        Expr::VariantConstruct(_, _, fields) => {
            1 + fields.iter().map(|(_, e)| count_nodes(e)).sum::<usize>()
        }
        // Phase A slice 3: pattern match — one node + scrutinee + sum of
        // each arm body's cost. Same shape as MatchResult, generalized to
        // N arms.
        Expr::MatchVariant(scrutinee, arms) => {
            1 + count_nodes(scrutinee)
                + arms.iter().map(|a| count_nodes(&a.body)).sum::<usize>()
        }
    }
}

/// Optimize all rules in a program. Non-destructive: returns a new program + stats.
pub fn optimize_program(program: &Program) -> (Program, OptStats) {
    // Count nodes before optimization
    let nodes_before: usize = program
        .items
        .iter()
        .map(|i| match i {
            Item::Rule(r) => {
                let binding_nodes: usize = r.logic.bindings.iter().map(|(_, e)| count_nodes(e)).sum();
                binding_nodes + count_nodes(&r.logic.value)
            }
            _ => 0,
        })
        .sum();

    let concepts: HashMap<&str, &Concept> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Concept(c) => Some((c.name.as_str(), c)),
            _ => None,
        })
        .collect();

    let optimized = Program {
        version: program.version.clone(),
        uses: vec![],
        items: program
            .items
            .iter()
            .map(|item| match item {
                Item::Concept(c) => Item::Concept(c.clone()),
                Item::Rule(r) => {
                    let field_ranges = concept_field_ranges(r, &concepts);
                    Item::Rule(optimize_rule(r, &field_ranges))
                }
                Item::Reaction(rx) => Item::Reaction(rx.clone()),
                // Services carry no logic expression to optimise; pass through.
                Item::Service(s) => Item::Service(s.clone()),
                // Phase 9 slice 1 stub: resources are declarative (no logic
                // expression to optimise); pass through unchanged.
                Item::Resource(r) => Item::Resource(r.clone()),
                // Phase 11 slice 1 stub: connections are declarative; the
                // request bytes live inside the rule's logic and are
                // optimised through `optimize_expr` like any other Expr.
                Item::Connection(c) => Item::Connection(c.clone()),
                // Phase B slice 1 stub: a concept_group is a declarative
                // type container with no expressions to optimise. The
                // optimizer treats it the same as a single Concept —
                // pass through unchanged. Field-range collection
                // (`concept_field_ranges`) already operates on top-level
                // concepts only; rules using group concepts are refused
                // by the verifier before reaching the optimizer, so the
                // optimizer never has to resolve a group-internal field
                // type today.
                Item::ConceptGroup(g) => Item::ConceptGroup(g.clone()),
            })
            .collect(),
    };
    // Count nodes after optimization
    let nodes_after: usize = optimized
        .items
        .iter()
        .map(|i| match i {
            Item::Rule(r) => {
                let binding_nodes: usize = r.logic.bindings.iter().map(|(_, e)| count_nodes(e)).sum();
                binding_nodes + count_nodes(&r.logic.value)
            }
            _ => 0,
        })
        .sum();

    (
        optimized,
        OptStats {
            nodes_before,
            nodes_after,
        },
    )
}

fn concept_field_ranges<'a>(
    rule: &Rule,
    concepts: &HashMap<&str, &'a Concept>,
) -> HashMap<String, (i64, i64)> {
    let mut ranges = HashMap::new();
    if let Type::Named(ref name) = rule.input_ty {
        if let Some(concept) = concepts.get(name.as_str()) {
            for field in &concept.fields {
                if field.ty == Type::Number {
                    let range = field.range.unwrap_or((0, i32::MAX as i64));
                    ranges.insert(field.name.clone(), range);
                }
            }
        }
    }
    ranges
}

fn optimize_rule(rule: &Rule, field_ranges: &HashMap<String, (i64, i64)>) -> Rule {
    let fr: HashMap<&str, (i64, i64)> = field_ranges
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();

    // Text-literal let inlining: `let sep = " | "` followed by
    // `concat(..., sep, ...)` folds to `concat(..., " | ", ...)` at every
    // reference site, and the binding drops out of the list. The native
    // backend's `emit_eval_expr` produces a scalar i64 in rax; text values
    // don't fit that shape and were rejected outright. Inlining at the
    // optimiser level keeps every backend on the same semantics path
    // (interpreter, transpiler, native, wasm) and removes the "text lets
    // are rejected" asymmetry without threading a text-value calling
    // convention through every emitter.
    //
    // Scope: only text *literals* (Expr::Text) are inlined. Non-literal
    // text bindings (`let x = concat(...)`, `let x = req.field`) still
    // fall through to the backend and may error — their fix needs real
    // runtime slot allocation and is left to a future slice.
    let (inlined_bindings, inlined_logic) =
        inline_text_literal_lets(&rule.logic.bindings, &rule.logic.value);

    Rule {
        name: rule.name.clone(),
        intention: rule.intention.clone(),
        source: rule.source.clone(),
        input_name: rule.input_name.clone(),
        input_ty: rule.input_ty.clone(),
        output_name: rule.output_name.clone(),
        output_ty: rule.output_ty.clone(),
        logic: LogicStmt {
            bindings: inlined_bindings
                .iter()
                .map(|(name, expr)| (name.clone(), optimize_expr(expr, &rule.input_name, &fr)))
                .collect(),
            target: rule.logic.target.clone(),
            value: optimize_expr(&inlined_logic, &rule.input_name, &fr),
        },
        proofs: rule.proofs.clone(),
        hints: rule.hints.clone(),
        layer: rule.layer,
        context_name: rule.context_name.clone(),
        context_ty: rule.context_ty.clone(),
    }
}

/// Walk the let bindings in source order; each `let name = Text(literal)`
/// is removed and every later reference to `name` (in subsequent bindings
/// and in the final logic) is substituted with the literal. Bindings whose
/// RHS is not a text literal after earlier substitutions are kept in
/// place. Returns the kept bindings and the rewritten logic.
fn inline_text_literal_lets(
    bindings: &[(String, Expr)],
    logic: &Expr,
) -> (Vec<(String, Expr)>, Expr) {
    let mut kept: Vec<(String, Expr)> = Vec::new();
    let mut substitutions: Vec<(String, Expr)> = Vec::new();

    for (name, expr) in bindings {
        // Apply earlier text-literal substitutions to this binding's RHS
        // first — a later binding can reference an earlier text let.
        let substituted = substitutions
            .iter()
            .fold(expr.clone(), |acc, (n, r)| substitute_ident(&acc, n, r));
        match &substituted {
            Expr::Text(_) => {
                substitutions.push((name.clone(), substituted));
            }
            _ => {
                kept.push((name.clone(), substituted));
            }
        }
    }

    let rewritten_logic = substitutions
        .iter()
        .fold(logic.clone(), |acc, (n, r)| substitute_ident(&acc, n, r));

    (kept, rewritten_logic)
}

/// Substitute every free occurrence of `Expr::Ident(name)` in `expr` with
/// `replacement`, respecting lambda / fold / match-result scopes. When a
/// sub-expression rebinds `name` as a lambda or match-arm variable, that
/// sub-tree is left untouched so shadowing is preserved.
pub fn substitute_ident(expr: &Expr, name: &str, replacement: &Expr) -> Expr {
    match expr {
        Expr::Ident(n) if n == name => replacement.clone(),
        Expr::Ident(_) | Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) => expr.clone(),
        Expr::Field(base, f) => Expr::Field(
            Box::new(substitute_ident(base, name, replacement)),
            f.clone(),
        ),
        Expr::Binary(op, l, r) => Expr::Binary(
            *op,
            Box::new(substitute_ident(l, name, replacement)),
            Box::new(substitute_ident(r, name, replacement)),
        ),
        Expr::Not(e) => Expr::Not(Box::new(substitute_ident(e, name, replacement))),
        Expr::Neg(e) => Expr::Neg(Box::new(substitute_ident(e, name, replacement))),
        Expr::Call(f, args) => Expr::Call(
            f.clone(),
            args.iter().map(|a| substitute_ident(a, name, replacement)).collect(),
        ),
        Expr::If(c, t, e) => Expr::If(
            Box::new(substitute_ident(c, name, replacement)),
            Box::new(substitute_ident(t, name, replacement)),
            Box::new(substitute_ident(e, name, replacement)),
        ),
        Expr::Quantifier(kind, target, var, body) => {
            let new_target = substitute_ident(target, name, replacement);
            let new_body = if var == name {
                (**body).clone()
            } else {
                substitute_ident(body, name, replacement)
            };
            Expr::Quantifier(*kind, Box::new(new_target), var.clone(), Box::new(new_body))
        }
        Expr::Fold(target, init, acc, var, body) => {
            let new_target = substitute_ident(target, name, replacement);
            let new_init = substitute_ident(init, name, replacement);
            let shadowed = acc == name || var == name;
            let new_body = if shadowed {
                (**body).clone()
            } else {
                substitute_ident(body, name, replacement)
            };
            Expr::Fold(
                Box::new(new_target),
                Box::new(new_init),
                acc.clone(),
                var.clone(),
                Box::new(new_body),
            )
        }
        Expr::Map(target, var, body) => {
            let new_target = substitute_ident(target, name, replacement);
            let new_body = if var == name {
                (**body).clone()
            } else {
                substitute_ident(body, name, replacement)
            };
            Expr::Map(Box::new(new_target), var.clone(), Box::new(new_body))
        }
        Expr::Filter(target, var, body) => {
            let new_target = substitute_ident(target, name, replacement);
            let new_body = if var == name {
                (**body).clone()
            } else {
                substitute_ident(body, name, replacement)
            };
            Expr::Filter(Box::new(new_target), var.clone(), Box::new(new_body))
        }
        Expr::Ok(e) => Expr::Ok(Box::new(substitute_ident(e, name, replacement))),
        Expr::Err(e) => Expr::Err(Box::new(substitute_ident(e, name, replacement))),
        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => {
            let new_target = substitute_ident(target, name, replacement);
            let new_ok = if ok_var == name {
                (**ok_body).clone()
            } else {
                substitute_ident(ok_body, name, replacement)
            };
            let new_err = if err_var == name {
                (**err_body).clone()
            } else {
                substitute_ident(err_body, name, replacement)
            };
            Expr::MatchResult(
                Box::new(new_target),
                ok_var.clone(),
                Box::new(new_ok),
                err_var.clone(),
                Box::new(new_err),
            )
        }
        Expr::Record(name_c, fields) => Expr::Record(
            name_c.clone(),
            fields
                .iter()
                .map(|(n, v)| (n.clone(), substitute_ident(v, name, replacement)))
                .collect(),
        ),
        Expr::Concat(args) => Expr::Concat(
            args.iter().map(|a| substitute_ident(a, name, replacement)).collect(),
        ),
        // Phase 9 slice 1 stub: Read carries a resource name (no Expr child
        // to substitute into); pass through unchanged.
        Expr::Read(n) => Expr::Read(n.clone()),
        // Phase 11 slice 1: substitute through the request bytes Expr.
        Expr::Fetch(n, req) => Expr::Fetch(
            n.clone(),
            Box::new(substitute_ident(req, name, replacement)),
        ),
        // Phase 12 (json_escape): substitute through the inner expression.
        Expr::JsonEscape(inner) => Expr::JsonEscape(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // Phase 12 (parse_int): substitute through the inner expression.
        Expr::ParseInt(inner) => Expr::ParseInt(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // `now_unix()` carries no inner expression and binds no name —
        // substitution is a no-op.
        Expr::NowUnix => expr.clone(),
        // `starts_with(haystack, needle)` — substitute through both children.
        Expr::StartsWith(h, n) => Expr::StartsWith(
            Box::new(substitute_ident(h, name, replacement)),
            Box::new(substitute_ident(n, name, replacement)),
        ),
        // `contains(haystack, needle)` — substitute through both children.
        Expr::Contains(h, n) => Expr::Contains(
            Box::new(substitute_ident(h, name, replacement)),
            Box::new(substitute_ident(n, name, replacement)),
        ),
        // `ends_with(haystack, needle)` — substitute through both children.
        Expr::EndsWith(h, n) => Expr::EndsWith(
            Box::new(substitute_ident(h, name, replacement)),
            Box::new(substitute_ident(n, name, replacement)),
        ),
        // `length(<text_expr>)` — substitute through the inner expression.
        Expr::Length(inner) => Expr::Length(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // `abs(<number_expr>)` — substitute through the inner expression.
        Expr::Abs(inner) => Expr::Abs(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // `le32(n)` / `le64(n)` — substitute through the inner expression.
        Expr::Le32(inner) => Expr::Le32(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        Expr::Le64(inner) => Expr::Le64(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // `arena_scope(inner)` — substitute through the inner expression.
        Expr::ArenaScope(inner) => Expr::ArenaScope(
            Box::new(substitute_ident(inner, name, replacement)),
        ),
        // `min(a, b)` — substitute through both children.
        Expr::Min(l, r) => Expr::Min(
            Box::new(substitute_ident(l, name, replacement)),
            Box::new(substitute_ident(r, name, replacement)),
        ),
        // `max(a, b)` — substitute through both children.
        Expr::Max(l, r) => Expr::Max(
            Box::new(substitute_ident(l, name, replacement)),
            Box::new(substitute_ident(r, name, replacement)),
        ),
        // `substring(text, start, end)` — substitute through all three children.
        Expr::Substring(t, s, e) => Expr::Substring(
            Box::new(substitute_ident(t, name, replacement)),
            Box::new(substitute_ident(s, name, replacement)),
            Box::new(substitute_ident(e, name, replacement)),
        ),
        // `byte_at(text, index)` — substitute through both children.
        Expr::ByteAt(t, i) => Expr::ByteAt(
            Box::new(substitute_ident(t, name, replacement)),
            Box::new(substitute_ident(i, name, replacement)),
        ),
        // `fold_bytes(text, init, acc, byte, idx => body)` — substitute
        // through text + init unconditionally. The body sees three bound
        // names that shadow the outer scope: if any of them matches the
        // ident being substituted, skip the substitution inside body
        // (mirror of Fold's `shadowed` guard, extended to three names).
        Expr::FoldBytes(t, init, acc, byte, idx, body) => {
            let new_text = substitute_ident(t, name, replacement);
            let new_init = substitute_ident(init, name, replacement);
            let shadowed = acc == name || byte == name || idx == name;
            let new_body = if shadowed {
                (**body).clone()
            } else {
                substitute_ident(body, name, replacement)
            };
            Expr::FoldBytes(
                Box::new(new_text),
                Box::new(new_init),
                acc.clone(),
                byte.clone(),
                idx.clone(),
                Box::new(new_body),
            )
        }
        // Phase A slice 2: variant construction — substitute through each
        // payload field's expression. Same shape as Record.
        Expr::VariantConstruct(concept_name, variant_name, fields) => Expr::VariantConstruct(
            concept_name.clone(),
            variant_name.clone(),
            fields
                .iter()
                .map(|(n, v)| (n.clone(), substitute_ident(v, name, replacement)))
                .collect(),
        ),
        // Phase A slice 3: pattern match — substitute through the
        // scrutinee unconditionally. For each arm, if ANY binder
        // (`Some(b) == name`) shadows the substituted ident, leave that
        // arm's body untouched; otherwise recurse. Mirror of the
        // Map/Filter/Fold shadowing guard, generalized to N positional
        // binders per arm (wildcards `None` cannot shadow anything).
        Expr::MatchVariant(scrutinee, arms) => {
            let new_scrut = substitute_ident(scrutinee, name, replacement);
            let new_arms: Vec<MatchArm> = arms
                .iter()
                .map(|a| {
                    let shadowed = a
                        .binders
                        .iter()
                        .any(|b| b.as_deref() == Some(name));
                    let new_body = if shadowed {
                        a.body.clone()
                    } else {
                        substitute_ident(&a.body, name, replacement)
                    };
                    MatchArm {
                        variant_name: a.variant_name.clone(),
                        binders: a.binders.clone(),
                        body: new_body,
                    }
                })
                .collect();
            Expr::MatchVariant(Box::new(new_scrut), new_arms)
        }
        Expr::BitAnd(a, b) => Expr::BitAnd(Box::new(substitute_ident(a, name, replacement)), Box::new(substitute_ident(b, name, replacement))),
        Expr::BitOr(a, b) => Expr::BitOr(Box::new(substitute_ident(a, name, replacement)), Box::new(substitute_ident(b, name, replacement))),
        Expr::BitXor(a, b) => Expr::BitXor(Box::new(substitute_ident(a, name, replacement)), Box::new(substitute_ident(b, name, replacement))),
        Expr::BitNot(i) => Expr::BitNot(Box::new(substitute_ident(i, name, replacement))),
        Expr::Shl(a, b) => Expr::Shl(Box::new(substitute_ident(a, name, replacement)), Box::new(substitute_ident(b, name, replacement))),
        Expr::Shr(a, b) => Expr::Shr(Box::new(substitute_ident(a, name, replacement)), Box::new(substitute_ident(b, name, replacement))),
    }
}

/// Recursively optimize an expression. Applied bottom-up:
/// optimize children first, then try to simplify the parent.
pub fn optimize_expr(
    expr: &Expr,
    input_name: &str,
    field_ranges: &HashMap<&str, (i64, i64)>,
) -> Expr {
    match expr {
        Expr::Number(_) | Expr::Text(_) | Expr::Bytes(_) | Expr::Ident(_) => expr.clone(),

        Expr::Field(base, field) => {
            Expr::Field(Box::new(optimize_expr(base, input_name, field_ranges)), field.clone())
        }

        Expr::Not(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            // not not x → x
            if let Expr::Not(double_inner) = &inner {
                return *double_inner.clone();
            }
            Expr::Not(Box::new(inner))
        }

        Expr::Neg(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            // -(-x) → x
            if let Expr::Neg(double_inner) = &inner {
                return *double_inner.clone();
            }
            // -(literal) → literal
            if let Expr::Number(n) = &inner {
                return Expr::Number(-n);
            }
            Expr::Neg(Box::new(inner))
        }

        Expr::Binary(op, left, right) => {
            let left = optimize_expr(left, input_name, field_ranges);
            let right = optimize_expr(right, input_name, field_ranges);

            // Constant folding: both sides are numbers
            if let (Expr::Number(l), Expr::Number(r)) = (&left, &right) {
                let result = match op {
                    BinOp::Add => Some(l.wrapping_add(*r)),
                    BinOp::Sub => Some(l.wrapping_sub(*r)),
                    BinOp::Mul => Some(l.wrapping_mul(*r)),
                    BinOp::Div if *r != 0 => Some(l / r),
                    BinOp::Mod if *r != 0 => Some(l % r),
                    BinOp::Gt => return Expr::Number(if l > r { 1 } else { 0 }),
                    BinOp::Lt => return Expr::Number(if l < r { 1 } else { 0 }),
                    BinOp::GtEq => return Expr::Number(if l >= r { 1 } else { 0 }),
                    BinOp::LtEq => return Expr::Number(if l <= r { 1 } else { 0 }),
                    BinOp::Eq => return Expr::Number(if l == r { 1 } else { 0 }),
                    BinOp::NotEq => return Expr::Number(if l != r { 1 } else { 0 }),
                    _ => None,
                };
                if let Some(val) = result {
                    return Expr::Number(val);
                }
            }

            // Algebraic identities
            match op {
                BinOp::Add => {
                    if matches!(&right, Expr::Number(0)) {
                        return left;
                    }
                    if matches!(&left, Expr::Number(0)) {
                        return right;
                    }
                }
                BinOp::Sub => {
                    if matches!(&right, Expr::Number(0)) {
                        return left;
                    }
                }
                BinOp::Mul => {
                    if matches!(&right, Expr::Number(0)) || matches!(&left, Expr::Number(0)) {
                        return Expr::Number(0);
                    }
                    if matches!(&right, Expr::Number(1)) {
                        return left;
                    }
                    if matches!(&left, Expr::Number(1)) {
                        return right;
                    }
                }
                BinOp::Div => {
                    if matches!(&right, Expr::Number(1)) {
                        return left;
                    }
                }
                _ => {}
            }

            Expr::Binary(*op, Box::new(left), Box::new(right))
        }

        Expr::If(cond, then_e, else_e) => {
            let cond = optimize_expr(cond, input_name, field_ranges);
            let then_e = optimize_expr(then_e, input_name, field_ranges);
            let else_e = optimize_expr(else_e, input_name, field_ranges);

            // Static branch elimination via interval arithmetic
            if let Some(always) = try_static_eval(&cond, field_ranges, input_name) {
                return if always { then_e } else { else_e };
            }

            Expr::If(Box::new(cond), Box::new(then_e), Box::new(else_e))
        }

        Expr::Call(name, args) => Expr::Call(
            name.clone(),
            args.iter()
                .map(|a| optimize_expr(a, input_name, field_ranges))
                .collect(),
        ),

        Expr::Fold(coll, init, acc, item, body) => Expr::Fold(
            Box::new(optimize_expr(coll, input_name, field_ranges)),
            Box::new(optimize_expr(init, input_name, field_ranges)),
            acc.clone(),
            item.clone(),
            Box::new(optimize_expr(body, input_name, field_ranges)),
        ),

        Expr::Quantifier(kind, coll, var, pred) => Expr::Quantifier(
            *kind,
            Box::new(optimize_expr(coll, input_name, field_ranges)),
            var.clone(),
            Box::new(optimize_expr(pred, input_name, field_ranges)),
        ),

        Expr::Map(coll, var, body) => Expr::Map(
            Box::new(optimize_expr(coll, input_name, field_ranges)),
            var.clone(),
            Box::new(optimize_expr(body, input_name, field_ranges)),
        ),

        Expr::Filter(coll, var, pred) => Expr::Filter(
            Box::new(optimize_expr(coll, input_name, field_ranges)),
            var.clone(),
            Box::new(optimize_expr(pred, input_name, field_ranges)),
        ),

        Expr::Ok(inner) => Expr::Ok(Box::new(optimize_expr(inner, input_name, field_ranges))),
        Expr::Err(inner) => Expr::Err(Box::new(optimize_expr(inner, input_name, field_ranges))),

        Expr::MatchResult(target, ok_var, ok_body, err_var, err_body) => Expr::MatchResult(
            Box::new(optimize_expr(target, input_name, field_ranges)),
            ok_var.clone(),
            Box::new(optimize_expr(ok_body, input_name, field_ranges)),
            err_var.clone(),
            Box::new(optimize_expr(err_body, input_name, field_ranges)),
        ),

        Expr::Record(name, fields) => Expr::Record(
            name.clone(),
            fields
                .iter()
                .map(|(n, e)| (n.clone(), optimize_expr(e, input_name, field_ranges)))
                .collect(),
        ),

        // `concat(...)` — recurse on each arg, then fold to a single
        // text literal when every arg is a Text or Number literal.
        // Numbers go through stdlib `to_string` (matches native's itoa
        // byte-for-byte: signed decimal, no leading zeros, '-' prefix
        // on negatives). The fold cascades — `length(concat("a",
        // "b", 42))` lowers to `length("ab42")` and then to
        // `Number(4)` via the existing length fold below. Mixed
        // args with field accesses / calls / arithmetic stay unfolded
        // since their values aren't known at compile time.
        Expr::Concat(args) => {
            let optimized: Vec<Expr> = args
                .iter()
                .map(|e| optimize_expr(e, input_name, field_ranges))
                .collect();
            let mut all_literal = true;
            let mut joined = String::new();
            for arg in &optimized {
                match arg {
                    Expr::Text(s) => joined.push_str(s),
                    Expr::Number(n) => joined.push_str(&n.to_string()),
                    _ => {
                        all_literal = false;
                        break;
                    }
                }
            }
            if all_literal {
                return Expr::Text(joined);
            }
            Expr::Concat(optimized)
        }
        // Phase 9 slice 1 stub: a file read has no compile-time optimisation
        // path (the contents aren't known until runtime); pass through.
        Expr::Read(name) => Expr::Read(name.clone()),
        // Phase 11 slice 1: a TCP fetch has no compile-time optimisation
        // path (response bytes aren't known until runtime); recurse into
        // the request bytes Expr.
        Expr::Fetch(name, req) => Expr::Fetch(
            name.clone(),
            Box::new(optimize_expr(req, input_name, field_ranges)),
        ),
        // Phase 12 (json_escape): if the inner is a text literal, fold
        // the escape at compile time — emits no runtime loop, the
        // resulting Text literal flows through the existing concat
        // machinery exactly like a hand-written escaped string.
        // Otherwise, recurse and keep the JsonEscape wrapper for the
        // backend to lower at runtime.
        Expr::JsonEscape(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            if let Expr::Text(s) = &inner {
                return Expr::Text(escape_json_string(s));
            }
            Expr::JsonEscape(Box::new(inner))
        }
        // Phase 12 (parse_int): if the inner is a text literal, fold the
        // parse at compile time — emits no runtime scan loop, the resulting
        // Number literal flows through the existing arithmetic machinery.
        // On parse failure, KEEP `Expr::ParseInt(Box::new(inner))` unchanged
        // so the runtime path can fail-closed (sys_exit(1) in native).
        Expr::ParseInt(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            if let Expr::Text(s) = &inner {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return Expr::Number(n);
                }
            }
            Expr::ParseInt(Box::new(inner))
        }
        // `now_unix()` cannot be folded at compile time — the clock value is
        // unknown until runtime. Pass through unchanged.
        Expr::NowUnix => expr.clone(),
        // `starts_with(haystack, needle)` — recurse into both children, then
        // fold to `0`/`1` when both have collapsed to text literals (bool
        // result encoded as a 0/1 number per the existing convention used
        // by Eq/Lt/Gt comparisons above).
        Expr::StartsWith(h, n) => {
            let h_opt = optimize_expr(h, input_name, field_ranges);
            let n_opt = optimize_expr(n, input_name, field_ranges);
            if let (Expr::Text(s1), Expr::Text(s2)) = (&h_opt, &n_opt) {
                return Expr::Number(if s1.as_bytes().starts_with(s2.as_bytes()) { 1 } else { 0 });
            }
            Expr::StartsWith(Box::new(h_opt), Box::new(n_opt))
        }
        // `contains(haystack, needle)` — same fold shape as StartsWith. When
        // both children collapse to text literals, fold to 0/1. Edge case:
        // an empty needle MUST yield true (matches stdlib `str::contains`),
        // but `windows(0)` would also yield true via per-position empty
        // matches — handle the empty case explicitly so the optimizer
        // matches the runtime semantics bit-for-bit.
        Expr::Contains(h, n) => {
            let h_opt = optimize_expr(h, input_name, field_ranges);
            let n_opt = optimize_expr(n, input_name, field_ranges);
            if let (Expr::Text(s1), Expr::Text(s2)) = (&h_opt, &n_opt) {
                let result = if s2.is_empty() {
                    1
                } else if s1.as_bytes().windows(s2.as_bytes().len()).any(|w| w == s2.as_bytes()) {
                    1
                } else {
                    0
                };
                return Expr::Number(result);
            }
            Expr::Contains(Box::new(h_opt), Box::new(n_opt))
        }
        // `ends_with(haystack, needle)` — recurse into both children, then
        // fold to 0/1 when both have collapsed to text literals. Empty needle
        // is always true (matches stdlib `str::ends_with` and the runtime).
        Expr::EndsWith(h, n) => {
            let h_opt = optimize_expr(h, input_name, field_ranges);
            let n_opt = optimize_expr(n, input_name, field_ranges);
            if let (Expr::Text(s1), Expr::Text(s2)) = (&h_opt, &n_opt) {
                let result = if s2.is_empty() || s1.as_bytes().ends_with(s2.as_bytes()) {
                    1
                } else {
                    0
                };
                return Expr::Number(result);
            }
            Expr::EndsWith(Box::new(h_opt), Box::new(n_opt))
        }
        // `length(<text_expr>)` — if the inner is a text literal, fold the
        // byte count at compile time. Otherwise recurse and keep the
        // wrapper for the backend to lower at runtime.
        Expr::Length(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            if let Expr::Text(s) = &inner {
                return Expr::Number(s.as_bytes().len() as i64);
            }
            Expr::Length(Box::new(inner))
        }
        // `abs(<number_expr>)` — if the inner is a number literal, fold to
        // the absolute value at compile time. `wrapping_abs` avoids panic
        // on i64::MIN (mirrors Neg's wrapping fold convention). Otherwise
        // recurse and keep the wrapper for the backend.
        Expr::Abs(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            if let Expr::Number(n) = &inner {
                return Expr::Number(n.wrapping_abs());
            }
            Expr::Abs(Box::new(inner))
        }
        // `le32(n)` / `le64(n)` — when the inner folds to a number literal,
        // fold to a `b"..."` bytes literal at compile time (little-endian low
        // 4 / 8 bytes). Otherwise recurse and keep the wrapper for the backend.
        Expr::Le32(inner) | Expr::Le64(inner) => {
            let width = if matches!(expr, Expr::Le64(_)) { 8 } else { 4 };
            let inner = optimize_expr(inner, input_name, field_ranges);
            if let Expr::Number(n) = &inner {
                let le = n.to_le_bytes();
                return Expr::Bytes(le[..width].to_vec());
            }
            if width == 8 { Expr::Le64(Box::new(inner)) } else { Expr::Le32(Box::new(inner)) }
        }
        // `arena_scope(inner)` — recurse into inner and KEEP the wrapper.
        // Never fold it away: the wrapper carries the reclaim boundary the
        // native backend exploits, and it must survive to the emitter even
        // when inner is otherwise constant-foldable.
        Expr::ArenaScope(inner) => {
            let inner = optimize_expr(inner, input_name, field_ranges);
            Expr::ArenaScope(Box::new(inner))
        }
        // `min(a, b)` — recurse into both children. When both collapse to
        // number literals, fold to the smaller at compile time. Otherwise
        // rebuild and let the backend lower at runtime (native: cmp + cmovg).
        Expr::Min(l, r) => {
            let l_opt = optimize_expr(l, input_name, field_ranges);
            let r_opt = optimize_expr(r, input_name, field_ranges);
            if let (Expr::Number(a), Expr::Number(b)) = (&l_opt, &r_opt) {
                return Expr::Number((*a).min(*b));
            }
            Expr::Min(Box::new(l_opt), Box::new(r_opt))
        }
        // `max(a, b)` — same fold shape as Min: literal-literal folds to the
        // larger; otherwise rebuild for the backend (native: cmp + cmovl).
        Expr::Max(l, r) => {
            let l_opt = optimize_expr(l, input_name, field_ranges);
            let r_opt = optimize_expr(r, input_name, field_ranges);
            if let (Expr::Number(a), Expr::Number(b)) = (&l_opt, &r_opt) {
                return Expr::Number((*a).max(*b));
            }
            Expr::Max(Box::new(l_opt), Box::new(r_opt))
        }
        // `substring(text, start, end)` — recurse into all three children.
        // Compile-time fold when every child has collapsed to a literal
        // and the bounds are valid; otherwise rebuild and let the backend
        // lower at runtime. Out-of-range literal bounds keep the wrapper
        // so the runtime path can fail-closed (sys_exit(1) in native).
        Expr::Substring(t, s, e) => {
            let t_opt = optimize_expr(t, input_name, field_ranges);
            let s_opt = optimize_expr(s, input_name, field_ranges);
            let e_opt = optimize_expr(e, input_name, field_ranges);
            if let (Expr::Text(text), Expr::Number(start), Expr::Number(end)) =
                (&t_opt, &s_opt, &e_opt)
            {
                let bytes = text.as_bytes();
                let len = bytes.len() as i64;
                if *start >= 0 && *end >= 0 && *end <= len && *start <= *end {
                    let slice = &bytes[*start as usize..*end as usize];
                    if let Ok(s) = std::str::from_utf8(slice) {
                        return Expr::Text(s.to_string());
                    }
                }
            }
            Expr::Substring(Box::new(t_opt), Box::new(s_opt), Box::new(e_opt))
        }
        // `byte_at(text, index)` — recurse into both children. Compile-time
        // fold when both have collapsed to literals AND the index is in
        // range. Out-of-range literal indices keep the wrapper so the
        // runtime path can fail-closed (sys_exit(1) in native).
        Expr::ByteAt(t, i) => {
            let t_opt = optimize_expr(t, input_name, field_ranges);
            let i_opt = optimize_expr(i, input_name, field_ranges);
            if let (Expr::Text(text), Expr::Number(idx)) = (&t_opt, &i_opt) {
                if let Some(&b) = text.as_bytes().get(*idx as usize) {
                    return Expr::Number(b as i64);
                }
            }
            Expr::ByteAt(Box::new(t_opt), Box::new(i_opt))
        }
        // `fold_bytes(text, init, acc, byte, idx => body)` — recurse on
        // text + init + body. No literal fold today: the body refers to
        // three bound names whose values vary per byte at runtime, so a
        // compile-time fold would need a mini-interpreter we don't ship.
        // Pass-through, same shape as Fold.
        Expr::FoldBytes(t, init, acc, byte, idx, body) => Expr::FoldBytes(
            Box::new(optimize_expr(t, input_name, field_ranges)),
            Box::new(optimize_expr(init, input_name, field_ranges)),
            acc.clone(),
            byte.clone(),
            idx.clone(),
            Box::new(optimize_expr(body, input_name, field_ranges)),
        ),
        // Phase A slice 2: variant construction — recurse through each
        // payload field's expression. No literal fold today (variants
        // have no compile-time equivalent yet). Same shape as Record.
        Expr::VariantConstruct(concept_name, variant_name, fields) => Expr::VariantConstruct(
            concept_name.clone(),
            variant_name.clone(),
            fields
                .iter()
                .map(|(n, e)| (n.clone(), optimize_expr(e, input_name, field_ranges)))
                .collect(),
        ),
        // Phase A slice 3: pattern match — recurse on scrutinee + each
        // arm's body. No constant-fold today (slice A.3 is the verifier +
        // interpreter wiring; folding requires the constructor side too).
        Expr::MatchVariant(scrutinee, arms) => Expr::MatchVariant(
            Box::new(optimize_expr(scrutinee, input_name, field_ranges)),
            arms.iter()
                .map(|a| MatchArm {
                    variant_name: a.variant_name.clone(),
                    binders: a.binders.clone(),
                    body: optimize_expr(&a.body, input_name, field_ranges),
                })
                .collect(),
        ),
        Expr::BitAnd(a, b) => Expr::BitAnd(Box::new(optimize_expr(a, input_name, field_ranges)), Box::new(optimize_expr(b, input_name, field_ranges))),
        Expr::BitOr(a, b) => Expr::BitOr(Box::new(optimize_expr(a, input_name, field_ranges)), Box::new(optimize_expr(b, input_name, field_ranges))),
        Expr::BitXor(a, b) => Expr::BitXor(Box::new(optimize_expr(a, input_name, field_ranges)), Box::new(optimize_expr(b, input_name, field_ranges))),
        Expr::BitNot(i) => Expr::BitNot(Box::new(optimize_expr(i, input_name, field_ranges))),
        Expr::Shl(a, b) => Expr::Shl(Box::new(optimize_expr(a, input_name, field_ranges)), Box::new(optimize_expr(b, input_name, field_ranges))),
        Expr::Shr(a, b) => Expr::Shr(Box::new(optimize_expr(a, input_name, field_ranges)), Box::new(optimize_expr(b, input_name, field_ranges))),
    }
}

/// Phase 12 (json_escape) compile-time escape. Mirrors the runtime
/// transform exactly so a literal-folded result is bit-for-bit identical
/// to what the runtime loop would emit.
///
/// Five JSON-significant bytes are escaped:
///   `"` (0x22) → `\"` (0x5C 0x22)
///   `\` (0x5C) → `\\` (0x5C 0x5C)
///   `\n` (0x0A) → `\n` literal (0x5C 0x6E)
///   `\r` (0x0D) → `\r` literal (0x5C 0x72)
///   `\t` (0x09) → `\t` literal (0x5C 0x74)
///
/// Other bytes (including `\b`, `\f`, control chars below 0x20) pass
/// through unchanged in this slice — `\u00XX` lands in a follow-up if a
/// real use case appears.
pub fn escape_json_string(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        match b {
            0x22 => { out.push('\\'); out.push('"'); }
            0x5C => { out.push('\\'); out.push('\\'); }
            0x0A => { out.push('\\'); out.push('n'); }
            0x0D => { out.push('\\'); out.push('r'); }
            0x09 => { out.push('\\'); out.push('t'); }
            other => out.push(other as char),
        }
    }
    out
}

/// Try to statically determine a boolean expression's result.
fn try_static_eval(
    expr: &Expr,
    field_ranges: &HashMap<&str, (i64, i64)>,
    input_name: &str,
) -> Option<bool> {
    match expr {
        Expr::Binary(op, left, right) => {
            let (l_min, l_max) = compute_range(left, field_ranges, input_name)?;
            let (r_min, r_max) = compute_range(right, field_ranges, input_name)?;
            match op {
                BinOp::Gt => {
                    if l_min > r_max { Some(true) }
                    else if l_max <= r_min { Some(false) }
                    else { None }
                }
                BinOp::Lt => {
                    if l_max < r_min { Some(true) }
                    else if l_min >= r_max { Some(false) }
                    else { None }
                }
                BinOp::GtEq => {
                    if l_min >= r_max { Some(true) }
                    else if l_max < r_min { Some(false) }
                    else { None }
                }
                BinOp::LtEq => {
                    if l_max <= r_min { Some(true) }
                    else if l_min > r_max { Some(false) }
                    else { None }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr<'a>(ranges: &'a [(&'a str, i64, i64)]) -> HashMap<&'a str, (i64, i64)> {
        ranges.iter().map(|(n, min, max)| (*n, (*min, *max))).collect()
    }

    #[test]
    fn fold_constants() {
        let expr = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Number(100)),
            Box::new(Expr::Number(20)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Number(120)));
    }

    #[test]
    fn eliminate_multiply_zero() {
        let expr = Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Ident("x".into())),
            Box::new(Expr::Number(0)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Number(0)));
    }

    #[test]
    fn eliminate_add_zero() {
        let expr = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Ident("x".into())),
            Box::new(Expr::Number(0)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Ident(_)));
    }

    #[test]
    fn double_negation_eliminated() {
        let expr = Expr::Not(Box::new(Expr::Not(Box::new(Expr::Ident("x".into())))));
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Ident(_)));
    }

    #[test]
    fn dead_branch_eliminated() {
        // if temp > 100 then 2 else 1, with temp ∈ [0, 50]
        let expr = Expr::If(
            Box::new(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "temp".into())),
                Box::new(Expr::Number(100)),
            )),
            Box::new(Expr::Number(2)),
            Box::new(Expr::Number(1)),
        );
        let ranges = fr(&[("temp", 0, 50)]);
        let result = optimize_expr(&expr, "i", &ranges);
        // temp ∈ [0,50], 50 ≤ 100 → condition always false → returns else branch (1)
        assert!(matches!(result, Expr::Number(1)));
    }

    #[test]
    fn nested_fold() {
        // (10 + 20) * 3 → 90
        let expr = Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Number(10)),
                Box::new(Expr::Number(20)),
            )),
            Box::new(Expr::Number(3)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Number(90)));
    }

    #[test]
    fn dead_branch_always_true() {
        // if temp > 0 then 1 else 2, with temp ∈ [10, 50]
        // 10 > 0 always → returns then branch (1)
        let expr = Expr::If(
            Box::new(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "temp".into())),
                Box::new(Expr::Number(0)),
            )),
            Box::new(Expr::Number(1)),
            Box::new(Expr::Number(2)),
        );
        let ranges = fr(&[("temp", 10, 50)]);
        let result = optimize_expr(&expr, "i", &ranges);
        assert!(matches!(result, Expr::Number(1)));
    }

    #[test]
    fn no_dead_branch_when_range_overlaps() {
        // if temp > 30 then 1 else 0, with temp ∈ [0, 50]
        // Could be true or false → both branches kept
        let expr = Expr::If(
            Box::new(Expr::Binary(
                BinOp::Gt,
                Box::new(Expr::Field(Box::new(Expr::Ident("i".into())), "temp".into())),
                Box::new(Expr::Number(30)),
            )),
            Box::new(Expr::Number(1)),
            Box::new(Expr::Number(0)),
        );
        let ranges = fr(&[("temp", 0, 50)]);
        let result = optimize_expr(&expr, "i", &ranges);
        assert!(matches!(result, Expr::If(_, _, _))); // NOT eliminated
    }

    #[test]
    fn multiply_by_one_eliminated() {
        let expr = Expr::Binary(
            BinOp::Mul,
            Box::new(Expr::Number(1)),
            Box::new(Expr::Ident("x".into())),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Ident(_)));
    }

    #[test]
    fn divide_by_one_eliminated() {
        let expr = Expr::Binary(
            BinOp::Div,
            Box::new(Expr::Ident("x".into())),
            Box::new(Expr::Number(1)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Ident(_)));
    }

    /// `concat(<all-literal args>)` folds to a single `Expr::Text` at
    /// compile time. Numbers go through stdlib `to_string`, which matches
    /// native's itoa byte-for-byte (signed decimal, no leading zeros).
    /// Mixed concats with field accesses / calls / arithmetic stay
    /// unfolded.
    ///
    /// The fold cascades downstream: `length(concat("a", "b"))` lowers
    /// to `length("ab")` which the existing length fold turns into
    /// `Number(2)` — pinned in the second case below.
    #[test]
    fn concat_of_literals_folds_to_text() {
        // (a) all-text args
        let expr = Expr::Concat(vec![
            Expr::Text("Hello, ".into()),
            Expr::Text("World".into()),
            Expr::Text("!".into()),
        ]);
        let result = optimize_expr(&expr, "i", &HashMap::new());
        match result {
            Expr::Text(s) => assert_eq!(s, "Hello, World!"),
            other => panic!("expected Text, got {:?}", other),
        }

        // (b) mixed text + number args — number formatted via to_string
        let expr = Expr::Concat(vec![
            Expr::Text("amount=".into()),
            Expr::Number(42),
            Expr::Text(", ratio=".into()),
            Expr::Number(-7),
        ]);
        let result = optimize_expr(&expr, "i", &HashMap::new());
        match result {
            Expr::Text(s) => assert_eq!(s, "amount=42, ratio=-7"),
            other => panic!("expected Text, got {:?}", other),
        }

        // (c) cascade: length(concat-of-literals) → length(literal) → Number
        let expr = Expr::Length(Box::new(Expr::Concat(vec![
            Expr::Text("ab".into()),
            Expr::Number(123),
        ])));
        let result = optimize_expr(&expr, "i", &HashMap::new());
        match result {
            Expr::Number(n) => assert_eq!(n, 5, "len('ab123') = 5"),
            other => panic!("expected Number(5), got {:?}", other),
        }

        // (d) non-literal arg blocks the fold
        let expr = Expr::Concat(vec![
            Expr::Text("user=".into()),
            Expr::Field(Box::new(Expr::Ident("o".into())), "name".into()),
        ]);
        let result = optimize_expr(&expr, "o", &HashMap::new());
        assert!(
            matches!(result, Expr::Concat(_)),
            "concat with a Field arg should stay unfolded"
        );

        // (e) single-arg concat of literal still folds
        let expr = Expr::Concat(vec![Expr::Text("alone".into())]);
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(
            matches!(&result, Expr::Text(s) if s == "alone"),
            "single-arg concat of a literal collapses to the literal"
        );
    }

    #[test]
    fn subtract_zero_eliminated() {
        let expr = Expr::Binary(
            BinOp::Sub,
            Box::new(Expr::Ident("x".into())),
            Box::new(Expr::Number(0)),
        );
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Ident(_)));
    }

    #[test]
    fn neg_literal_folded() {
        let expr = Expr::Neg(Box::new(Expr::Number(42)));
        let result = optimize_expr(&expr, "i", &HashMap::new());
        assert!(matches!(result, Expr::Number(-42)));
    }

    #[test]
    fn text_literal_let_inlined_into_logic() {
        // let sep = " | " ; concat("a", sep, "b")  →  concat("a", " | ", "b")
        let bindings = vec![("sep".to_string(), Expr::Text(" | ".to_string()))];
        let logic = Expr::Concat(vec![
            Expr::Text("a".to_string()),
            Expr::Ident("sep".to_string()),
            Expr::Text("b".to_string()),
        ]);
        let (kept, rewritten) = inline_text_literal_lets(&bindings, &logic);
        assert!(kept.is_empty(), "text-literal binding should be removed");
        match rewritten {
            Expr::Concat(args) => {
                assert_eq!(args.len(), 3);
                assert!(matches!(&args[1], Expr::Text(s) if s == " | "));
            }
            other => panic!("expected Concat, got {:?}", other),
        }
    }

    #[test]
    fn text_literal_let_inlined_through_chain() {
        // let a = "x" ; let b = a ; concat(a, b)  →  concat("x", "x")
        let bindings = vec![
            ("a".to_string(), Expr::Text("x".to_string())),
            ("b".to_string(), Expr::Ident("a".to_string())),
        ];
        let logic = Expr::Concat(vec![
            Expr::Ident("a".to_string()),
            Expr::Ident("b".to_string()),
        ]);
        let (kept, rewritten) = inline_text_literal_lets(&bindings, &logic);
        assert!(kept.is_empty(), "both bindings should resolve to text literals");
        match rewritten {
            Expr::Concat(args) => {
                assert!(matches!(&args[0], Expr::Text(s) if s == "x"));
                assert!(matches!(&args[1], Expr::Text(s) if s == "x"));
            }
            other => panic!("expected Concat, got {:?}", other),
        }
    }

    #[test]
    fn non_text_let_not_inlined() {
        // let n = 42 ; n + 1  → kept as-is (number lets ride the existing path)
        let bindings = vec![("n".to_string(), Expr::Number(42))];
        let logic = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Ident("n".to_string())),
            Box::new(Expr::Number(1)),
        );
        let (kept, rewritten) = inline_text_literal_lets(&bindings, &logic);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0, "n");
        // Logic unchanged — still references `n`.
        assert!(matches!(rewritten, Expr::Binary(BinOp::Add, _, _)));
    }

    #[test]
    fn text_let_does_not_cross_lambda_shadow() {
        // let sep = " | " ; map(xs, sep => sep)
        // The lambda's `sep` shadows the let; the lambda body must not be
        // rewritten to Text(" | ").
        let bindings = vec![("sep".to_string(), Expr::Text(" | ".to_string()))];
        let logic = Expr::Map(
            Box::new(Expr::Field(
                Box::new(Expr::Ident("input".to_string())),
                "xs".to_string(),
            )),
            "sep".to_string(),
            Box::new(Expr::Ident("sep".to_string())),
        );
        let (kept, rewritten) = inline_text_literal_lets(&bindings, &logic);
        assert!(kept.is_empty());
        match rewritten {
            Expr::Map(_, var, body) => {
                assert_eq!(var, "sep");
                // Body still references the lambda's `sep`, not the literal.
                assert!(matches!(*body, Expr::Ident(ref n) if n == "sep"));
            }
            other => panic!("expected Map, got {:?}", other),
        }
    }
}
