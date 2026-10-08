//! Folding must preserve both values and observable evaluation failures.
use crate::{
    ast::*,
    interpreter::{self, Value},
    lexer::Lexer,
    optimizer,
    parser::Parser,
    verifier,
};
use std::{collections::HashMap, path::Path};

const MIN: &str = "(-9223372036854775807 - 1)";

fn source(body: &str) -> String {
    let reads = ["i.n", "i.s"]
        .into_iter()
        .filter(|r| body.contains(r))
        .collect::<Vec<_>>()
        .join(", ");
    include_str!("../examples/boolean_guards.verbose")
        .replace(
            "    out = if i.n >= 0 and i.n < length(i.s) and byte_at(i.s, i.n) > 0 then 1 else 0",
            body,
        )
        .replace("reads: [i.n, i.s]", &format!("reads: [{reads}]"))
}

fn parse(src: &str) -> Program {
    Parser::new(Lexer::new(src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

fn checked(src: &str) -> Program {
    let p = parse(src);
    let errors = verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}\n{src}");
    p
}

fn eval(p: &Program, s: &str, n: i64) -> Result<Value, String> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([
            ("s".into(), Value::Text(s.into())),
            ("n".into(), Value::Number(n)),
        ]),
    )
    .map_err(|e| e.message)
}

#[test]
fn constant_folding_signed_edges_are_total() {
    for op in [BinOp::Div, BinOp::Mod] {
        for (a, b) in [(i64::MIN, -1), (0, 0), (1, 0), (i64::MAX, 0)] {
            let expr = Expr::Binary(op, Box::new(Expr::Number(a)), Box::new(Expr::Number(b)));
            assert!(matches!(
                optimizer::optimize_expr(&expr, "i", &HashMap::new()),
                Expr::Binary(_, _, _)
            ));
        }
        for (a, b) in [
            (i64::MIN, 1),
            (i64::MIN, 2),
            (i64::MAX, -1),
            (-7, 3),
            (7, -3),
            (0, -1),
        ] {
            let expr = Expr::Binary(op, Box::new(Expr::Number(a)), Box::new(Expr::Number(b)));
            let expected = if op == BinOp::Div { a / b } else { a % b };
            assert!(
                matches!(optimizer::optimize_expr(&expr, "i", &HashMap::new()), Expr::Number(n) if n == expected)
            );
        }
    }
    let expr = Expr::Neg(Box::new(Expr::Number(i64::MIN)));
    assert!(matches!(
        optimizer::optimize_expr(&expr, "i", &HashMap::new()),
        Expr::Number(i64::MIN)
    ));
}

#[test]
fn constant_folding_does_not_treat_unknown_evaluation_as_pure() {
    for operand in [
        Expr::Call("unknown".into(), vec![Expr::Ident("i".into())]),
        Expr::AbortIf(Box::new(Expr::Number(1))),
        Expr::ArenaScope(Box::new(Expr::Number(7))),
    ] {
        for (a, b) in [
            (operand.clone(), Expr::Number(0)),
            (Expr::Number(0), operand),
        ] {
            let expr = Expr::Binary(BinOp::Mul, Box::new(a), Box::new(b));
            assert!(matches!(
                optimizer::optimize_expr(&expr, "i", &HashMap::new()),
                Expr::Binary(BinOp::Mul, _, _)
            ));
        }
    }
    // Cheap, verified input reads can still disappear. Checked arithmetic can
    // disappear when every intermediate and condition is safe too.
    let input = Expr::Field(Box::new(Expr::Ident("i".into())), "n".into());
    for operand in [
        input.clone(),
        Expr::Binary(BinOp::Add, Box::new(input), Box::new(Expr::Number(1))),
    ] {
        let expr = Expr::Binary(BinOp::Mul, Box::new(operand), Box::new(Expr::Number(0)));
        assert!(matches!(
            optimizer::optimize_expr(&expr, "i", &HashMap::from([("n", (0, 10))])),
            Expr::Number(0)
        ));
    }
}

#[test]
fn constant_folding_guards_do_not_waive_source_proofs() {
    let src = source("    out = if 1 == 1 then 7 else 1 / 0");
    let p = checked(&src);
    assert_eq!(eval(&p, "", 0), Ok(Value::Number(7)));
    let strict = parse(&format!("{src}  hints:\n    overflow: [-100, 100]\n"));
    let errors = verifier::verify_program(&strict, Path::new("examples"));
    assert!(
        errors.iter().any(|e| e.message.contains("zero")),
        "{errors:?}"
    );
    let bad_type = parse(&source("    out = if 1 == 1 then 7 else 1 / \"x\""));
    assert!(!verifier::verify_program(&bad_type, Path::new("examples")).is_empty());
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
mod native {
    use super::*;
    use std::{fs, os::unix::process::ExitStatusExt, process::Command};

    // Check BOTH emission entry points: the native emitter also simplifies
    // expressions when it is called directly with an unoptimized source AST.
    fn outputs(p: &Program, s: &str, n: i64, mut check: impl FnMut(std::process::Output)) {
        let path =
            std::env::temp_dir().join(format!("verbose-constant-fold-{}", std::process::id()));
        for emitted in [p.clone(), optimizer::optimize_program(p).0] {
            crate::native::compile_native(
                &emitted,
                "guarded_byte",
                path.to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
            check(
                Command::new(&path)
                    .args([s, &n.to_string()])
                    .output()
                    .unwrap(),
            );
            // A core-dump handler may still hold the old inode after SIGFPE.
            // The next compilation must create a fresh executable.
            fs::remove_file(&path).unwrap();
        }
    }

    fn succeeds(p: &Program, n: i64, value: i64) {
        outputs(p, "a", n, |out| {
            assert_eq!(
                (out.status.code(), out.stdout, out.stderr),
                (Some(0), format!("{value}\n").into_bytes(), vec![]),
            )
        });
    }

    fn traps(p: &Program, n: i64) {
        outputs(p, "a", n, |out| {
            assert_eq!(
                (out.status.signal(), out.stdout, out.stderr),
                (Some(8), vec![], vec![]), // Linux SIGFPE from the existing idiv.
            )
        });
    }

    #[test]
    fn constant_folding_native_skips_only_unselected_failures() {
        for failure in [
            "1 / 0".into(),
            "1 % 0".into(),
            format!("{MIN} / -1"),
            format!("{MIN} % -1"),
        ] {
            for expr in [
                format!("if i.n == 0 or ({failure}) > 0 then 7 else 9"),
                format!("if i.n != 0 and ({failure}) > 0 then 9 else 7"),
                format!("if i.n == 0 then 7 else {failure}"),
            ] {
                let p = checked(&source(&format!("    out = {expr}")));
                assert_eq!(eval(&p, "a", 0), Ok(Value::Number(7)), "{expr}");
                succeeds(&p, 0, 7);
                traps(&p, 1);
            }
        }
        for (expr, expected) in [
            (format!("-{MIN}"), i64::MIN),
            (format!("{MIN} / 1"), i64::MIN),
            ("9223372036854775807 / -1".into(), -i64::MAX),
            ("-7 / 3".into(), -2),
            ("-7 % 3".into(), -1),
        ] {
            succeeds(&checked(&source(&format!("    out = {expr}"))), 0, expected);
        }
    }

    #[test]
    fn constant_folding_native_preserves_zero_products_lets_and_calls() {
        for body in [
            "    out = (1 / i.n) * 0",
            "    out = 0 * (1 / i.n)",
            "    out = 0 * ((1 % i.n) * 0)",
            "    let unused = (1 / i.n) * 0\n    out = 7",
        ] {
            let p = checked(&source(body));
            assert!(eval(&p, "a", 0).unwrap_err().contains("zero"));
            traps(&p, 0);
            let Value::Number(expected) = eval(&p, "a", 1).unwrap() else {
                panic!()
            };
            succeeds(&p, 1, expected);
        }
        let helper = source("    out = 1 / i.n")
            .split("rule guarded_byte")
            .nth(1)
            .unwrap()
            .to_owned();
        let src = format!(
            "{}\nrule helper{helper}",
            source("    out = helper(i) * 0")
                .replace("reads: []", "reads: [i]")
                .replace("calls: []", "calls: [helper]")
        );
        let p = checked(&src);
        assert!(eval(&p, "a", 0).unwrap_err().contains("zero"));
        traps(&p, 0);
        succeeds(&p, 1, 0);

        for expr in ["byte_at(i.s, 0) * 0", "0 * byte_at(i.s, 0)"] {
            let p = checked(&source(&format!("    out = {expr}")));
            assert!(eval(&p, "", 0).unwrap_err().contains("byte_at"));
            outputs(&p, "", 0, |out| {
                assert_eq!(
                    (out.status.code(), out.stdout, out.stderr),
                    (Some(1), vec![], vec![]),
                )
            });
            succeeds(&p, 0, 0);
        }
    }

    #[test]
    fn constant_folding_native_keeps_conditions_hidden_by_ranges() {
        for expr in [
            "if (if 1 / i.n > 0 then 1 else 2) > 0 then 7 else 9",
            "(if 1 / i.n > 0 then 1 else 2) * 0",
            "if (if 1 % i.n == 0 then 1 else 2) + 1 > 0 then 7 else 9",
        ] {
            let p = checked(&source(&format!("    out = {expr}")));
            assert!(eval(&p, "a", 0).unwrap_err().contains("zero"));
            traps(&p, 0);
            let Value::Number(expected) = eval(&p, "a", 1).unwrap() else {
                panic!()
            };
            succeeds(&p, 1, expected);
        }
    }
}
