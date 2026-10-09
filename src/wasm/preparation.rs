//! Source-order preparation for the existing, deliberately restricted inliner.
use super::*;
use std::collections::HashSet;

pub(super) fn inline_callees<'a>(
    entry: &'a Rule,
    rules: &HashMap<&str, &'a Rule>,
) -> Result<Vec<&'a Rule>, WasmError> {
    fn visit<'a>(
        expr: &Expr,
        caller: &'a Rule,
        input_available: bool,
        rules: &HashMap<&str, &'a Rule>,
        active: &mut Vec<&'a str>,
        seen: &mut HashSet<&'a str>,
        callees: &mut Vec<&'a Rule>,
    ) -> Result<(), WasmError> {
        if let Expr::Call(name, args) = expr {
            let callee = *rules.get(name.as_str()).ok_or_else(|| WasmError {
                message: format!("unknown rule '{name}'"),
            })?;
            if args.len() != 1
                || !matches!(&args[0], Expr::Ident(n) if n == &caller.input_name)
                || !input_available
                || callee.input_ty != caller.input_ty
            {
                return Err(WasmError {
                    message: format!(
                    "WASM call into '{name}' requires the current input and the same input concept"
                ),
                });
            }
            if !callee.logic.bindings.is_empty() {
                return Err(WasmError {
                    message: format!(
                        "call into rule '{name}' with let bindings is not yet supported"
                    ),
                });
            }
            if active.contains(&callee.name.as_str()) {
                return Err(WasmError {
                    message: format!("WASM recursive call into '{name}' is not supported"),
                });
            }
            if seen.insert(&callee.name) {
                callees.push(callee);
                active.push(&callee.name);
                visit(
                    &callee.logic.value,
                    callee,
                    true,
                    rules,
                    active,
                    seen,
                    callees,
                )?;
                active.pop();
            }
            return Ok(());
        }
        let mut result = Ok(());
        crate::verifier::walk_expr_children(expr, &mut |child| {
            if result.is_ok() {
                result = visit(child, caller, input_available, rules, active, seen, callees);
            }
        });
        result
    }
    let mut callees = Vec::new();
    let mut seen = HashSet::new();
    let mut active = vec![entry.name.as_str()];
    let mut input_available = true;
    for (name, rhs) in &entry.logic.bindings {
        visit(
            rhs,
            entry,
            input_available,
            rules,
            &mut active,
            &mut seen,
            &mut callees,
        )?;
        input_available &= name != &entry.input_name;
    }
    visit(
        &entry.logic.value,
        entry,
        input_available,
        rules,
        &mut active,
        &mut seen,
        &mut callees,
    )?;
    Ok(callees)
}

pub(super) fn uses_text_equality(
    expr: &Expr,
    fields: &HashMap<&str, FieldShape>,
    bindings: &HashMap<&str, BindingShape>,
    rules: &HashMap<&str, &Rule>,
) -> bool {
    if let Expr::Binary(BinOp::Eq | BinOp::NotEq, left, right) = expr {
        if expr_yields_text(left, fields, bindings, rules)
            || expr_yields_text(right, fields, bindings, rules)
        {
            return true;
        }
    }
    // Branch binders shadow outer lets. Only the top-level Call scrutinee is
    // supported by emission; other shapes still reach its explicit refusal.
    if let Expr::MatchResult(target, ok, yes, err, no) = expr {
        if let Some((yes_bindings, no_bindings)) =
            result_branch_bindings(target, ok, err, bindings, rules)
        {
            return uses_text_equality(yes, fields, &yes_bindings, rules)
                || uses_text_equality(no, fields, &no_bindings, rules);
        }
    }
    let mut found = false;
    crate::verifier::walk_expr_children(expr, &mut |child| {
        found |= uses_text_equality(child, fields, bindings, rules);
    });
    found
}

/// Placeholder indices carry only the branch-local type shape during planning.
pub(super) fn result_branch_bindings<'a>(
    target: &Expr,
    ok: &'a str,
    err: &'a str,
    bindings: &HashMap<&'a str, BindingShape>,
    rules: &HashMap<&str, &Rule>,
) -> Option<(
    HashMap<&'a str, BindingShape>,
    HashMap<&'a str, BindingShape>,
)> {
    let Expr::Call(name, _) = target else {
        return None;
    };
    let Type::Result(ok_ty, _) = &rules.get(name.as_str())?.output_ty else {
        return None;
    };
    let mut yes = bindings.clone();
    yes.insert(
        ok,
        if **ok_ty == Type::Text {
            BindingShape::Text { ptr: 0, len: 0 }
        } else {
            BindingShape::Number(0)
        },
    );
    let mut no = bindings.clone();
    no.insert(err, BindingShape::Text { ptr: 0, len: 0 });
    Some((yes, no))
}
