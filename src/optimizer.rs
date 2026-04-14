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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => 1,
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

    Rule {
        name: rule.name.clone(),
        intention: rule.intention.clone(),
        source: rule.source.clone(),
        input_name: rule.input_name.clone(),
        input_ty: rule.input_ty.clone(),
        output_name: rule.output_name.clone(),
        output_ty: rule.output_ty.clone(),
        logic: LogicStmt {
            bindings: rule
                .logic
                .bindings
                .iter()
                .map(|(name, expr)| (name.clone(), optimize_expr(expr, &rule.input_name, &fr)))
                .collect(),
            target: rule.logic.target.clone(),
            value: optimize_expr(&rule.logic.value, &rule.input_name, &fr),
        },
        proofs: rule.proofs.clone(),
        hints: rule.hints.clone(),
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
        Expr::Number(_) | Expr::Text(_) | Expr::Ident(_) => expr.clone(),

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
    }
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
}
