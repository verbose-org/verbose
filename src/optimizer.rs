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

/// Optimize all rules in a program. Non-destructive: returns a new program.
pub fn optimize_program(program: &Program) -> Program {
    let concepts: HashMap<&str, &Concept> = program
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Concept(c) => Some((c.name.as_str(), c)),
            _ => None,
        })
        .collect();

    Program {
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
            })
            .collect(),
    }
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

        Expr::Quantifier(kind, coll, var, pred) => Expr::Quantifier(
            *kind,
            Box::new(optimize_expr(coll, input_name, field_ranges)),
            var.clone(),
            Box::new(optimize_expr(pred, input_name, field_ranges)),
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
}
