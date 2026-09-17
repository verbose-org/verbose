use super::*;
use crate::{
    interpreter::{self, Value as RuntimeValue},
    lexer::Lexer,
    parser::Parser,
};
use std::{fs, path::Path, process::Command};

fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn fixture(expr: &str, lets: &str) -> Program {
    parse(&format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "bounded numeric input"
  @source: invoices.intent:1
  fields:
    x : number [-10, 10]
    y : number [1, 5]
rule checked
  @intention: "checked arithmetic"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
{lets}    out = {expr}
  proofs:
    purity:
      reads : [i.x, i.y]
      calls : []
    termination:
      bound : 1000
  hints:
    overflow : [-1000, 1000]
"#
    ))
}
fn rule(p: &mut Program) -> &mut Rule {
    p.items
        .iter_mut()
        .find_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .unwrap()
}
fn fields(p: &mut Program) -> &mut Vec<Field> {
    &mut p
        .items
        .iter_mut()
        .find_map(|i| match i {
            Item::Concept(c) => Some(c),
            _ => None,
        })
        .unwrap()
        .fields
}
fn full(p: &mut Program) {
    let h = rule(p).hints.as_mut().unwrap().overflow.as_mut().unwrap();
    h.min = i64::MIN;
    h.max = i64::MAX;
}
fn refuses(p: &Program, needle: &str) {
    let errors = verify(p);
    assert!(
        errors.iter().any(|e| e.message.contains(needle)),
        "expected {needle}: {errors:?}"
    );
}
fn eval(
    p: &Program,
    name: &str,
    x: i64,
    y: i64,
) -> Result<RuntimeValue, interpreter::RuntimeError> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let input_fields = &concepts[0].fields;
    interpreter::eval_rule(
        rules.iter().find(|r| r.name == name).unwrap(),
        &rules,
        &concepts,
        &[],
        &HashMap::from([
            (input_fields[0].name.clone(), RuntimeValue::Number(x)),
            (input_fields[1].name.clone(), RuntimeValue::Number(y)),
        ]),
    )
}
fn differential(p: &Program, name: &str, inputs: &[(i64, i64)]) {
    assert!(verify(p).is_empty(), "{:?}", verify(p));
    let path = format!("/tmp/verbose-numeric-{}", std::process::id());
    for p in [p.clone(), crate::optimizer::optimize_program(p).0] {
        crate::native::compile_native(&p, name, &path, false, false).unwrap();
        for &(x, y) in inputs {
            let expected = eval(&p, name, x, y);
            let actual = Command::new(&path)
                .args([x.to_string(), y.to_string()])
                .output()
                .unwrap();
            assert!(actual.stderr.is_empty(), "{actual:?}");
            match expected {
                Ok(value) => {
                    assert_eq!(actual.stdout, format!("{value}\n").as_bytes(), "{x},{y}");
                    assert_eq!(
                        actual.status.code(),
                        Some(if matches!(value, RuntimeValue::Bool(false)) {
                            1
                        } else {
                            0
                        })
                    );
                }
                Err(_) => {
                    assert_eq!(actual.status.code(), Some(1), "{x},{y}: {actual:?}");
                    assert!(actual.stdout.is_empty());
                }
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn numeric_contract_checks_eager_lets_conditions_and_intermediate_values() {
    let max = i64::MAX;
    for (expr, lets, needle) in [
        (
            "0".into(),
            format!("    let unused = {max} + 1\n"),
            "let 'unused'",
        ),
        (
            format!("if {max} + 1 > 0 then 0 else 0"),
            "".into(),
            "if condition",
        ),
        (
            format!("({max} + i.y) - i.y"),
            "".into(),
            "intermediate range",
        ),
        (
            "if i.x > 0 then 0 else 1 / (i.y - 1)".into(),
            "".into(),
            "includes zero",
        ),
        (
            "if i.x > 0 and 1 / (i.y - 1) > 0 then 0 else 1".into(),
            "".into(),
            "includes zero",
        ),
        (
            "parse_int(\"1\")".into(),
            "".into(),
            "unsupported expression",
        ),
        (
            "if 0 == 0 then 1 else parse_int(\"bad\")".into(),
            "".into(),
            "unsupported expression",
        ),
        ("unknown".into(), "".into(), "unknown scalar binding"),
    ] {
        refuses(&fixture(&expr, &lets), needle);
    }
    let mut p = fixture("i.x", "");
    fields(&mut p)[0].range = None;
    refuses(
        &p,
        "computed range [-9223372036854775808, 9223372036854775807]",
    );
    full(&mut p);
    assert!(verify(&p).is_empty());
    differential(
        &p,
        "checked",
        &[(i64::MIN, 1), (-1, 1), (0, 1), (i64::MAX, 1)],
    );
    for expr in ["-i.x", "abs(i.x)", "i.x / -1", "i.x % -1"] {
        let mut p = fixture(expr, "");
        full(&mut p);
        fields(&mut p)[0].range = None;
        refuses(&p, "overflow");
    }
}

#[test]
fn numeric_contract_lexical_scope_calls_and_signed_arithmetic_agree() {
    let inputs: Vec<_> = (-10..=10)
        .map(|x| (x, 3))
        .chain([(-11, 3), (11, 3), (0, 0), (0, 6)])
        .collect();
    for expr in [
        "i.x / 2",
        "i.x / -2",
        "i.x % 3",
        "i.x % -3",
        "-i.x",
        "abs(i.x)",
        "min(i.x, i.y)",
        "max(i.x, i.y)",
        "if not (i.x > 0) then i.y else i.x",
        "if (i.x > 0) == (i.y > 0) then 1 else 0",
        "if i.x > 0 and i.y > 1 or i.x < -5 then i.x else i.y",
    ] {
        differential(&fixture(expr, ""), "checked", &inputs);
    }
    let mut collision = fixture("i.__bounds_slot_0 + i.__bounds_slot_1", "");
    fields(&mut collision)[0].name = "__bounds_slot_0".into();
    fields(&mut collision)[1].name = "__bounds_slot_1".into();
    differential(&collision, "checked", &inputs);
    let mut p = fixture(
        "i.x + alias + x",
        "    let x = i.x + 1\n    let alias = x\n    let x = 3\n",
    );
    differential(&p, "checked", &inputs);
    let mut caller = rule(&mut p).clone();
    caller.name = "caller".into();
    caller.input_name = "incoming".into();
    caller.hints = None;
    caller.logic.bindings = vec![
        ("x".into(), Expr::Number(50)),
        (
            "saved".into(),
            Expr::Call("checked".into(), vec![Expr::Ident("incoming".into())]),
        ),
    ];
    caller.logic.value = Expr::Binary(
        BinOp::Add,
        Box::new(Expr::Ident("saved".into())),
        Box::new(Expr::Ident("x".into())),
    );
    p.items.push(Item::Rule(caller));
    differential(&p, "caller", &inputs);
    if let Item::Rule(r) = p.items.last_mut().unwrap() {
        r.output_ty = Type::Bool;
        r.logic.value = Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::Ident("saved".into())),
            Box::new(Expr::Number(0)),
        );
    }
    differential(&p, "caller", &inputs);
    if let Item::Rule(r) = p.items.last_mut().unwrap() {
        r.output_ty = Type::Number;
        r.logic.bindings.clear();
        r.logic.value = Expr::If(
            Box::new(Expr::Binary(
                BinOp::Eq,
                Box::new(Expr::Number(0)),
                Box::new(Expr::Number(1)),
            )),
            Box::new(Expr::Call(
                "checked".into(),
                vec![Expr::Ident("incoming".into())],
            )),
            Box::new(Expr::Number(0)),
        );
    }
    differential(&p, "caller", &[(-11, 1), (0, 1), (11, 1)]);
    for expr in ["i.x / 2", "i.x / -2", "i.x % 7", "i.x % -7"] {
        let mut p = fixture(expr, "");
        full(&mut p);
        fields(&mut p)[0].range = None;
        differential(
            &p,
            "checked",
            &[
                (i64::MIN, 1),
                (i64::MIN + 1, 1),
                (-1, 1),
                (0, 1),
                (i64::MAX, 1),
            ],
        );
    }
    let mut p = fixture("i.x", "    let i = 0\n");
    refuses(&p, "unshadowed");
    rule(&mut p).logic.value = Expr::Ident("i".into());
    differential(&p, "checked", &inputs);
}

#[test]
fn numeric_contract_rejects_unproved_calls_types_and_cycles() {
    let mut p = fixture("i.x", "");
    let mut callee = rule(&mut p).clone();
    callee.name = "callee".into();
    callee.hints = None;
    p.items.push(Item::Rule(callee));
    rule(&mut p).logic.value = Expr::Call("callee".into(), vec![Expr::Ident("i".into())]);
    assert!(verify(&p).is_empty());
    if let Item::Rule(r) = p.items.last_mut().unwrap() {
        r.logic.bindings.push((
            "hidden".into(),
            Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Number(i64::MAX)),
                Box::new(Expr::Number(1)),
            ),
        ));
    }
    refuses(&p, "let 'hidden'");
    if let Item::Rule(r) = p.items.last_mut().unwrap() {
        r.logic.bindings.clear();
        r.logic.value = Expr::Call("checked".into(), vec![Expr::Ident("i".into())]);
    }
    refuses(&p, "recursion");
    for expr in ["callee(Input { x: 100, y: 1 })", "callee(i.x)", "callee()"] {
        let mut bad = fixture(expr, "");
        bad.items.push(p.items.last().unwrap().clone());
        refuses(&bad, "requires callee(input)");
    }
    let mut p = fixture("if i.x then 0 else 1", "");
    refuses(&p, "expected bool");
    rule(&mut p).logic.value = Expr::Ident("true".into());
    refuses(&p, "unknown scalar binding");
    let p = fixture("if i.x > 0 then 1 else i.x > 1", "");
    refuses(&p, "different types");
}

#[test]
fn numeric_contract_interval_arithmetic_covers_concrete_signed_values() {
    for a in -4..=4 {
        for b in a..=4 {
            for c in -4..=4 {
                for d in c..=4 {
                    for op in [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Mod] {
                        if let Ok(Value::Number(lo, hi)) = arithmetic(op, (a, b), (c, d)) {
                            for x in a..=b {
                                for y in c..=d {
                                    let value = match op {
                                        BinOp::Add => x + y,
                                        BinOp::Sub => x - y,
                                        BinOp::Mul => x * y,
                                        BinOp::Div => x / y,
                                        BinOp::Mod => x % y,
                                        _ => unreachable!(),
                                    };
                                    assert!((lo..=hi).contains(&value), "{op:?} [{a},{b}] [{c},{d}]: {x},{y} -> {value}, claimed {lo},{hi}");
                                }
                            }
                        } else {
                            assert!(matches!(op, BinOp::Div | BinOp::Mod) && c <= 0 && d >= 0);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn numeric_contract_native_entry_refuses_malformed_or_out_of_range_numbers() {
    let mut p = fixture("i.x", "");
    full(&mut p);
    fields(&mut p)[0].range = None;
    let bin = format!("/tmp/verbose-numeric-parse-{}", std::process::id());
    crate::native::compile_native(&p, "checked", &bin, false, false).unwrap();
    for s in [
        "",
        "-",
        "+1",
        " 1",
        "1 ",
        "1x",
        "--1",
        "9223372036854775808",
        "-9223372036854775809",
        "18446744073709551616",
    ] {
        let out = Command::new(&bin).args([s, "1"]).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{s}: {out:?}");
        assert!(out.stdout.is_empty());
        assert!(out.stderr.is_empty());
    }
    for (s, expected) in [
        ("-0", "0"),
        ("00001", "1"),
        ("-9223372036854775808", "-9223372036854775808"),
        ("9223372036854775807", "9223372036854775807"),
    ] {
        let out = Command::new(&bin).args([s, "1"]).output().unwrap();
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(out.stdout, format!("{expected}\n").as_bytes());
    }
    for args in [vec!["1"], vec!["2", "1", "3"]] {
        let out = Command::new(&bin).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert_eq!(
            out.stdout,
            if args.len() == 1 {
                b"".as_slice()
            } else {
                b"2\n".as_slice()
            }
        );
        assert_eq!(
            out.stderr,
            if args.len() == 1 {
                b"error: not enough arguments\n".as_slice()
            } else {
                b"".as_slice()
            }
        );
    }
    let out = Command::new(&bin)
        .args(["5", "1", "-3", "2"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"5\n-3\n");
    for i in 2..130 {
        fields(&mut p).push(Field {
            name: format!("unused_{i}"),
            ty: Type::Number,
            range: None,
        });
    }
    crate::native::compile_native(&p, "checked", &bin, false, false).unwrap();
    let out = Command::new(&bin).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stderr, b"error: not enough arguments\n");
    let out = Command::new(&bin).args(vec!["1"; 130]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"1\n");
    fs::remove_file(bin).unwrap();
}

#[test]
fn numeric_contract_backend_refusals_preserve_artifacts() {
    let p = fixture("i.x", "");
    let bin = format!("/tmp/verbose-numeric-refusal-{}", std::process::id());
    fs::write(&bin, b"existing").unwrap();
    assert!(crate::wasm::compile_wasm(&p, "checked", &bin)
        .unwrap_err()
        .message
        .contains("strict overflow"));
    for (stdin, stream) in [(true, false), (false, true)] {
        assert!(
            crate::native::compile_native(&p, "checked", &bin, stdin, stream)
                .unwrap_err()
                .message
                .contains("argv")
        );
    }
    assert!(crate::native::compile_native_stdin_raw(&p, "checked", &bin).is_err());
    assert!(
        crate::native::compile_http_server(&p, "checked", 18080, &bin)
            .unwrap_err()
            .message
            .contains("strict overflow")
    );
    assert!(
        crate::native::compile_native_multi(&p, &["checked", "checked"], &bin, true, false)
            .is_err()
    );
    let bad = fixture("i.x + 9223372036854775807", "");
    assert!(crate::native::compile_native(&bad, "checked", &bin, false, false).is_err());
    assert_eq!(fs::read(&bin).unwrap(), b"existing");
    fs::remove_file(bin).unwrap();
}

#[test]
fn numeric_contract_existing_examples_verify() {
    for f in [
        "generated",
        "showcase",
        "pricing",
        "deadcode",
        "strict_overflow",
    ] {
        let p = parse(&fs::read_to_string(format!("examples/{f}.verbose")).unwrap());
        let errors = crate::verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{f}: {errors:?}");
    }
}

#[test]
fn numeric_contract_no_invented_optimizer_domain_or_lost_scope() {
    let mut p = fixture("if i.x < 0 then 1 else 2", "");
    fields(&mut p)[0].range = None;
    differential(
        &p,
        "checked",
        &[(-1, 1), (0, 1), (i64::MIN, 1), (i64::MAX, 1)],
    );
    // Also prevent the legacy optimizer from inventing a nonnegative domain.
    rule(&mut p).hints = None;
    let (optimized, _) = crate::optimizer::optimize_program(&p);
    assert!(matches!(
        rule(&mut optimized.clone()).logic.value,
        Expr::If(..)
    ));
    assert_eq!(eval(&optimized, "checked", -1, 1).unwrap().to_string(), "1");
    let p = fixture(
        "if i.x > 0 and i.y > 0 then 1 else 2",
        "    let false = 99\n",
    );
    differential(&p, "checked", &[(-1, 1), (1, 1)]);
}

#[test]
fn numeric_contract_unknown_analysis_and_effect_boundaries_refuse() {
    let mut p = fixture("i.x", "");
    rule(&mut p).context_name = Some("ctx".into());
    refuses(&p, "context inputs");
    rule(&mut p).context_name = None;
    fields(&mut p)[0].ty = Type::Collection("Input".into());
    refuses(&p, "flat concept");
    fields(&mut p)[0].ty = Type::Number;
    let mut e = Expr::Number(0);
    for _ in 0..260 {
        e = Expr::Neg(Box::new(e));
    }
    rule(&mut p).logic.value = e;
    refuses(&p, "256 expression levels");
    let mut p = fixture("i.x", "");
    p.items.push(Item::Reaction(Reaction {
        name: "audit".into(),
        intention: "audit".into(),
        source: SourceRef {
            file: "invoices.intent".into(),
            line: 1,
        },
        trigger: "checked".into(),
        effects: vec![Effect::Print(vec![Expr::Number(0)])],
    }));
    refuses(&p, "service/reaction");
    let mut p = parse(include_str!("../../examples/http_bounded.verbose"));
    let mut r = rule(&mut fixture("i.x", "")).clone();
    r.name = "checked_number".into();
    p.items.push(Item::Rule(r));
    for item in &mut p.items {
        if let Item::Service(s) = item {
            s.logs.push(LogBlock {
                effect: Effect::AppendFile {
                    path: "/tmp/unused".into(),
                    content: Expr::Call("checked_number".into(), vec![Expr::Number(0)]),
                },
                on_error: ErrorPolicy::Abort,
            });
        }
    }
    refuses(&p, "service/reaction");
}

#[test]
#[ignore = "builds the full self-hosted compiler; run with the two_generation bootstrap"]
fn two_generation_numeric_contract_self_hosted_refuses_before_emission() {
    use std::io::Write;
    use std::process::Stdio;
    let p = parse(include_str!("../../examples/vexprparse.verbose"));
    let dir = std::env::temp_dir().join(format!("verbose-numeric-self-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = include_str!("../../examples/strict_overflow.verbose");
    let clean = source
        .replace("overflow : [-150, 150]", "cache_result : \"overflow\"")
        .replace("overflow : [-299, 301]", "cache_result : \"overflow\"")
        .replace("reading", "overflow")
        .replace("product", "hints");
    for entry in ["elf_program_src", "x86_program_src"] {
        let bin = dir.join(entry);
        crate::native::compile_native_stdin_raw(&p, entry, bin.to_str().unwrap()).unwrap();
        for (src, expected_status) in [
            (source.to_owned(), 1),
            (source.replace("[-150, 150]", "[0, 0]"), 1),
            (
                source.replace("  hints:\n    overflow : [-150, 150]\n", ""),
                1,
            ),
            (clean.clone(), 0),
        ] {
            let mut child = Command::new("sh")
                .args([
                    "-c",
                    "ulimit -s unlimited; exec \"$1\" 0",
                    "numeric-contract",
                    bin.to_str().unwrap(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(src.as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert_eq!(out.status.code(), Some(expected_status), "{entry}: {out:?}");
            assert_eq!(out.stdout.is_empty(), expected_status != 0);
            assert!(out.stderr.is_empty());
        }
    }
    fs::remove_dir_all(dir).unwrap();
}
