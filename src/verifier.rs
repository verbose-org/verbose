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
    collect_expr_facts(&logic.value, &mut facts.reads, &mut facts.calls);
    facts
}

fn collect_expr_facts(
    expr: &Expr,
    reads: &mut HashSet<Vec<String>>,
    calls: &mut HashSet<Vec<String>>,
) {
    match expr {
        Expr::Number(_) => {}
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
        Expr::Number(_) | Expr::Ident(_) => 0,
        Expr::Field(base, _) => count_operations(base),
        Expr::Binary(_, l, r) => 1 + count_operations(l) + count_operations(r),
        Expr::Call(_, args) => 1 + args.iter().map(count_operations).sum::<usize>(),
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
}
