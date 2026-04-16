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
    let concepts: HashMap<String, &Concept> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Concept(c) => Some((c.name.clone(), c)),
            _ => None,
        })
        .collect();
    let all_rules: Vec<&Rule> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();

    for item in &program.items {
        match item {
            Item::Concept(c) => verify_concept(c, base_dir, &mut errors),
            Item::Rule(r) => verify_rule(r, &concepts, &all_rules, base_dir, &mut errors),
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
        }
    }
    errors
}

fn verify_concept(c: &Concept, base_dir: &StdPath, errors: &mut Vec<VerifyError>) {
    if let Err(msg) = verify_source_ref(&c.source, base_dir) {
        errors.push(VerifyError {
            context: format!("concept '{}' / @source", c.name),
            message: msg,
        });
    }
}

fn verify_rule(
    rule: &Rule,
    concepts: &HashMap<String, &Concept>,
    all_rules: &[&Rule],
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

    let facts = collect_logic_facts(&rule.logic);

    for path in &facts.reads {
        if let Some(msg) = validate_read_path(path, rule, input_concept) {
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
    check_termination(rule, errors);
    check_determinism(rule, &facts, errors);

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
        Expr::Concat(_) => Some(Type::Text),
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
        if !matches!(rule.proofs.purity.verdict, PurityVerdict::Pure) {
            errors.push(VerifyError {
                context: format!("rule '{}' / hints.vectorizable", rule.name),
                message: "vectorizable requires pure verdict".into(),
            });
        }
    }

    if hints.parallel.is_some() {
        if !matches!(rule.proofs.purity.verdict, PurityVerdict::Pure) {
            errors.push(VerifyError {
                context: format!("rule '{}' / hints.parallel", rule.name),
                message: "parallel requires pure verdict (no side effects between elements)".into(),
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
) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let base = &path[0];
    if base != &rule.input_name {
        return Some(format!(
            "unknown binding '{}' in path '{}'; only '{}' is in scope",
            base,
            path.join("."),
            rule.input_name
        ));
    }
    if path.len() >= 2 {
        if let Some(c) = input_concept {
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
    None
}

fn check_purity(rule: &Rule, facts: &LogicFacts, errors: &mut Vec<VerifyError>) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    let declared_reads = path_list_to_set(&rule.proofs.purity.reads);
    let declared_writes = path_list_to_set(&rule.proofs.purity.writes);
    let declared_calls = path_list_to_set(&rule.proofs.purity.calls);

    if declared_reads != facts.reads {
        let missing: Vec<String> = facts
            .reads
            .difference(&declared_reads)
            .map(|p| p.join("."))
            .collect();
        let extra: Vec<String> = declared_reads
            .difference(&facts.reads)
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
            context: ctx("purity.reads"),
            message: format!("declared reads do not match logic; {}", parts.join(", ")),
        });
    }

    if !declared_writes.is_empty() {
        errors.push(VerifyError {
            context: ctx("purity.writes"),
            message: "declared writes must be empty (POC grammar has no write operations)".into(),
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

    match &rule.proofs.purity.verdict {
        PurityVerdict::Pure => {}
        PurityVerdict::Impure => {
            if declared_writes.is_empty() && declared_calls.is_empty() {
                errors.push(VerifyError {
                    context: ctx("purity.verdict"),
                    message: "verdict 'impure' is inconsistent with empty writes and calls".into(),
                });
            }
        }
        PurityVerdict::PureExcept(exceptions) => {
            let exc_set = path_list_to_set(exceptions);
            for c in &facts.calls {
                if !exc_set.contains(c) {
                    errors.push(VerifyError {
                        context: ctx("purity.verdict"),
                        message: format!("call '{}' not listed in pure_except(...)", c.join(".")),
                    });
                }
            }
        }
    }
}

fn check_termination(rule: &Rule, errors: &mut Vec<VerifyError>) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    match rule.proofs.termination.form {
        TerminationForm::ConstantBound => match rule.proofs.termination.bound {
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
                    message: "constant_bound requires a 'bound:' value".into(),
                });
            }
        },
        TerminationForm::VariableBound | TerminationForm::DecreasingRecursion => {
            errors.push(VerifyError {
                context: ctx("termination.form"),
                message: format!(
                    "termination form {:?} not supported by POC grammar",
                    rule.proofs.termination.form
                ),
            });
        }
        TerminationForm::Unproven => {}
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
    }
}

fn check_determinism(rule: &Rule, _facts: &LogicFacts, errors: &mut Vec<VerifyError>) {
    let ctx = |sub: &str| format!("rule '{}' / {}", rule.name, sub);

    match rule.proofs.determinism.form {
        DeterminismForm::Total => {
            // 'total' is valid if all called rules are themselves deterministic.
            // For now we trust this — transitive determinism checking is a Phase 2 feature.
        }
        DeterminismForm::Conditional => {
            errors.push(VerifyError {
                context: ctx("determinism.form"),
                message: "'conditional' determinism requires 'hypotheses:' (not yet supported)"
                    .into(),
            });
        }
        DeterminismForm::Nondeterministic => {}
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
  @source: invoices.intent:2
  input:
    i : Invoice
  output:
    important : bool
  logic:
    important = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    t : T
  output:
    b : bool
  logic:
    b = t.x > 0
  proofs:
    purity:
      reads   : [t.x]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total

reaction bad
  @intention: "z"
  @source: invoices.intent:2
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 2
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    i : In
  output:
    p : Ghost
  logic:
    p = Ghost { x: i.x }
  proofs:
    purity:
      reads   : [i.x]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x }
  proofs:
    purity:
      reads   : [i.x]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x, b: i.x, c: i.x }
  proofs:
    purity:
      reads   : [i.x]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    i : In
  output:
    p : Pair
  logic:
    p = Pair { a: i.x, b: i.x > 0 }
  proofs:
    purity:
      reads   : [i.x]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 2
    determinism:
      form : total
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 2
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    t : T
  output:
    r : bool
  logic:
    r = Ok(t.amount)
  proofs:
    purity:
      reads   : [t.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    t : T
  output:
    r : Result(number, text)
  logic:
    r = if t.amount > 0 then Ok("oops") else Err("no")
  proofs:
    purity:
      reads   : [t.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 3
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    t : T
  output:
    r : number
  logic:
    r = t.amount > 0
  proofs:
    purity:
      reads   : [t.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total

rule flag_critical
  @intention: "y"
  @source: invoices.intent:2
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
      writes  : []
      calls   : [is_large]
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total

rule lower_domain
  @intention: "y"
  @source: invoices.intent:2
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
      writes  : []
      calls   : [upper_orchestration]
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
  @source: invoices.intent:2
  input:
    i : Invoice
  output:
    big : bool
  logic:
    big = i.amount > 10000
  proofs:
    purity:
      reads   : [i.amount]
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total

rule layered_caller
  @intention: "y"
  @source: invoices.intent:2
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
      writes  : []
      calls   : [unlayered_helper]
      verdict : pure
    termination:
      form  : constant_bound
      bound : 1
    determinism:
      form : total
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
            let rule = match all_rules.last() {
                Some(r) => *r,
                None => continue,
            };
            let records = load_json_input(&json_path).unwrap_or_else(|e| {
                panic!("cannot load {}: {}", json_path.display(), e)
            });
            for (idx, record) in records.iter().enumerate() {
                let result = eval_rule(rule, &all_rules, record);
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : constant_bound
      bound : 2
    determinism:
      form : total
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
      writes  : []
      calls   : []
      verdict : pure
    termination:
      form  : variable_bound
    determinism:
      form : total
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
        let bad = VALID.replace("invoices.intent:2", "invoices.intent:999");
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
      writes: []
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 1
    determinism:
      form: total
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
      writes: []
      calls: [helper]
      verdict: pure
    termination:
      form: constant_bound
      bound: 1
    determinism:
      form: total
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
      writes: []
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 1
    determinism:
      form: total
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
      writes: []
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 1
    determinism:
      form: total
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
      writes: []
      calls: []
      verdict: pure
    termination:
      form: constant_bound
      bound: 2
    determinism:
      form: total
"#;
        let errs = verify_str(src);
        assert!(errs.is_empty(), "expected no errors, got: {:#?}", errs);
    }
}
