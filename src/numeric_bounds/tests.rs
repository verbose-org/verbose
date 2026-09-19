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

fn native_bytes(p: &Program, name: &str) -> Vec<u8> {
    let path = format!("/tmp/verbose-numeric-layout-{}", std::process::id());
    crate::native::compile_native(p, name, &path, false, false).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    bytes
}

fn frame_bytes(bytes: &[u8]) -> u32 {
    let prologue = [0x55, 0x48, 0x89, 0xe5, 0x48, 0x81, 0xec];
    let at = bytes
        .windows(prologue.len())
        .position(|b| b == prologue)
        .unwrap()
        + prologue.len();
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

#[test]
fn numeric_native_precomputes_only_after_source_verification() {
    let p = fixture(
        "if i.x <= 10 then answer else 42",
        "    let factor = 12 / -5\n    let alias = factor\n    let answer = abs(-7) + alias + (-7 % 3) + min(2, 3)\n",
    );
    assert_eq!(
        native_bytes(&p, "checked"),
        native_bytes(&fixture("6", ""), "checked")
    );
    differential(
        &p,
        "checked",
        &[(-10, 1), (10, 5), (-11, 1), (11, 1), (0, 6)],
    );
    for expr in [
        "if 1 == 1 then 6 else 1 / 0",
        "if i.x <= 10 then 6 else parse_int(\"bad\")",
    ] {
        assert!(native_opt::lower(&fixture(expr, "")).is_err());
    }
    let bad = fixture("6", "    let unused = 9223372036854775807 + 1\n");
    assert!(native_opt::lower(&bad).is_err());
    let mut p = fixture("i.x", "");
    let mut caller = rule(&mut p).clone();
    caller.name = "caller".into();
    caller.hints = None;
    caller.logic.value = Expr::If(
        Box::new(Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::Number(0)),
            Box::new(Expr::Number(1)),
        )),
        Box::new(Expr::Call("checked".into(), vec![Expr::Ident("i".into())])),
        Box::new(Expr::Number(7)),
    );
    p.items.push(Item::Rule(caller));
    assert!(active_rules(&p).contains("caller"));
    assert!(!active_rules(&native_opt::lower(&p).unwrap()).contains("caller"));
    // Even when its last checked call disappears, this entry keeps ALL guards.
    differential(&p, "caller", &[(0, 1), (11, 1), (-11, 1), (0, 6), (0, 0)]);
    for op in [BinOp::Gt, BinOp::Lt] {
        let Item::Rule(caller) = p.items.last_mut().unwrap() else {
            unreachable!()
        };
        caller.output_ty = Type::Bool;
        caller.logic.value = Expr::Binary(
            op,
            Box::new(Expr::Call("checked".into(), vec![Expr::Ident("i".into())])),
            Box::new(Expr::Number(-1001)),
        );
        differential(&p, "caller", &[(0, 1), (10, 5), (11, 1)]);
    }
    for (expr, expected) in [
        ("(-9223372036854775807 - 1) / 2", i64::MIN / 2),
        ("(-9223372036854775807 - 1) % 7", i64::MIN % 7),
    ] {
        let mut p = fixture(expr, "");
        full(&mut p);
        let mut constant = fixture("0", "");
        full(&mut constant);
        rule(&mut constant).logic.value = Expr::Number(expected);
        assert_eq!(
            native_bytes(&p, "checked"),
            native_bytes(&constant, "checked")
        );
        differential(&p, "checked", &[(0, 1), (11, 1)]);
    }

    // Native expansion is still bounded on the original source, before a
    // constant branch could erase a large acyclic call tree.
    let mut p = fixture("0", "");
    let mut previous = "checked".to_owned();
    for i in 0..17 {
        let mut next = rule(&mut p).clone();
        next.name = format!("chain_{i}");
        next.hints = None;
        let call = Expr::Call(previous, vec![Expr::Ident("i".into())]);
        next.logic.value = Expr::Binary(BinOp::Add, Box::new(call.clone()), Box::new(call));
        previous = next.name.clone();
        p.items.push(Item::Rule(next));
    }
    let mut caller = rule(&mut p).clone();
    caller.name = "caller".into();
    caller.hints = None;
    caller.logic.value = Expr::If(
        Box::new(Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::Number(0)),
            Box::new(Expr::Number(0)),
        )),
        Box::new(Expr::Number(7)),
        Box::new(Expr::Call(previous, vec![Expr::Ident("i".into())])),
    );
    p.items.push(Item::Rule(caller));
    assert!(native_opt::lower(&p).is_ok());
    let path = format!("/tmp/verbose-numeric-limit-{}", std::process::id());
    fs::write(&path, b"existing").unwrap();
    let error = crate::native::compile_native(&p, "caller", &path, false, false).unwrap_err();
    assert!(error.message.contains("100000 nodes"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), b"existing");
    fs::remove_file(path).unwrap();
}

#[test]
fn numeric_native_reuses_scratch_without_clobbering_live_values() {
    let leaf = (0..24).fold("i.x".to_owned(), |expr, _| format!("{expr} + i.y"));
    let mut p = fixture(&leaf, "    let x = i.x + i.y\n");
    let mut caller = rule(&mut p).clone();
    caller.name = "caller".into();
    caller.input_name = "incoming".into();
    caller.hints = None;
    caller.logic.bindings = vec![
        (
            "x".into(),
            Expr::Field(Box::new(Expr::Ident("incoming".into())), "x".into()),
        ),
        ("alias".into(), Expr::Ident("x".into())),
    ];
    let call = Expr::Call("checked".into(), vec![Expr::Ident("incoming".into())]);
    let mut frames = Vec::new();
    for n in [2, 48] {
        let sum = (1..n).fold(call.clone(), |acc, _| {
            Expr::Binary(BinOp::Add, Box::new(acc), Box::new(call.clone()))
        });
        caller.logic.value = Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Ident("alias".into())),
            Box::new(Expr::If(
                Box::new(Expr::Binary(
                    BinOp::Lt,
                    Box::new(Expr::Ident("x".into())),
                    Box::new(Expr::Number(0)),
                )),
                Box::new(sum.clone()),
                Box::new(Expr::Neg(Box::new(sum))),
            )),
        );
        p.items.push(Item::Rule(caller.clone()));
        differential(&p, "caller", &[(-10, 1), (-1, 5), (0, 3), (10, 5), (11, 1)]);
        frames.push(frame_bytes(&native_bytes(&p, "caller")));
        p.items.pop();
    }
    assert_eq!(
        frames[0], frames[1],
        "sequential calls and exclusive branches reuse scratch"
    );
    assert!(
        frames[0] <= 128,
        "only the live scalar values need slots: {frames:?}"
    );
}

#[test]
fn numeric_native_local_storage_depends_on_lifetime_not_binding_count() {
    let inputs = [(-10, 1), (-1, 5), (0, 3), (10, 5), (11, 1), (0, 6)];
    for shadow in [false, true] {
        let mut frames = Vec::new();
        for n in [2, 128] {
            let mut lets = "    let value = i.x\n".to_owned();
            let mut previous = "value".to_owned();
            for i in 0..n {
                let name = if shadow {
                    "value".into()
                } else {
                    format!("v{i}")
                };
                lets += &format!("    let {name} = {previous} + i.y\n");
                previous = name;
            }
            let p = fixture(&previous, &lets);
            differential(&p, "checked", &inputs);
            frames.push(frame_bytes(&native_bytes(&p, "checked")));
        }
        assert_eq!(frames[0], frames[1], "shadow={shadow}: {frames:?}");
        assert!(frames[0] <= 96, "{frames:?}");
    }
    let short = fixture("i.x", "    let unused = i.x + i.y\n");
    let long = fixture("i.x", &"    let unused = i.x + i.y\n".repeat(128));
    assert_eq!(
        frame_bytes(&native_bytes(&short, "checked")),
        frame_bytes(&native_bytes(&long, "checked"))
    );
    // Unused nonconstant lets still execute; only their persistent stores vanish.
    assert!(native_bytes(&long, "checked").len() > native_bytes(&short, "checked").len());
    differential(&long, "checked", &inputs);
}

#[test]
fn numeric_native_local_reuse_preserves_aliases_branches_and_caller_values() {
    let inputs: Vec<_> = (-10..=10)
        .flat_map(|x| (1..=5).map(move |y| (x, y)))
        .chain([(-11, 1), (11, 1), (0, 6)])
        .collect();
    for (expr, lets) in [
        (
            "branch + alias",
            "    let left = i.x * 2\n    let late = i.y * 3\n    let hole = left + 1\n    let left = hole + i.y\n    let alias = late\n    let late = left * 2\n    let unused = i.x + i.y\n    let branch = if i.x < 0 then alias + left else late + alias\n",
        ),
        (
            "if alias == flag then i.x else i.y",
            "    let flag = i.x < 0\n    let other = i.y > 3\n    let alias = flag\n    let flag = other\n",
        ),
        (
            "x + i.x + y",
            "    let x = i.y\n    let y = x + i.x\n    let x = y + 1\n",
        ),
        ("j", "    let i = i.x\n    let j = i + 1\n"),
    ] {
        differential(&fixture(expr, lets), "checked", &inputs);
    }
    let mut p = fixture(
        "value",
        &("    let value = i.x\n".to_owned() + &"    let value = value + i.y\n".repeat(48)),
    );
    let mut caller = rule(&mut fixture(
        "keep + choice + checked(i)",
        "    let keep = i.x\n    let first = checked(i)\n    let alias = first\n    let first = checked(i) + keep\n    let choice = if i.x < 0 then alias else first\n    let unused = checked(i)\n",
    ))
    .clone();
    caller.name = "caller".into();
    caller.hints = None;
    p.items.push(Item::Rule(caller));
    differential(&p, "caller", &inputs);
    assert!(frame_bytes(&native_bytes(&p, "caller")) <= 128);
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
fn numeric_guards_prove_selected_arms_and_keep_native_semantics() {
    let inputs: Vec<_> = (-10..=10)
        .flat_map(|x| (1..=5).map(move |y| (x, y)))
        .chain([(-11, 1), (11, 1), (0, 0), (0, 6)])
        .collect();
    for (expr, lets) in [
        ("if i.x > 0 then 100 / i.x else 0", ""),
        ("if 0 < i.x then 100 / i.x else 0", ""),
        ("if i.x >= 1 then 100 / i.x else 0", ""),
        ("if 1 <= i.x then 100 / i.x else 0", ""),
        ("if i.x < 0 then 100 / i.x else 0", ""),
        ("if 0 > i.x then 100 / i.x else 0", ""),
        ("if i.x <= -1 then 100 / i.x else 0", ""),
        ("if -1 >= i.x then 100 / i.x else 0", ""),
        ("if i.x == 2 then 100 / i.x else 0", ""),
        ("if 2 != i.x then 0 else 100 / i.x", ""),
        ("if not (i.x <= 0) then 100 / i.x else 0", ""),
        ("if i.x <= 0 then 0 else 100 / i.x", ""),
        ("if i.x > 0 and i.x <= 5 then 100 / i.x else 0", ""),
        ("if i.x <= 0 or i.x > 5 then 0 else 100 / i.x", ""),
        ("if not (i.x <= 0 or i.x > 5) then 100 % i.x else 0", ""),
        (
            "if i.x >= 0 then if i.x != 0 then 100 / i.x else 0 else 0",
            "",
        ),
        (
            "if i.x <= 0 then if i.x == 0 then 0 else 100 / i.x else 0",
            "",
        ),
        (
            "if d > 0 then 100 / d else if d < 0 then 100 / d else 0",
            "    let first = i.x + i.y\n    let d = first\n",
        ),
        (
            "if d > 0 then 100 / d + alias else 0",
            "    let d = i.x\n    let alias = d\n    let d = i.y - 1\n",
        ),
        (
            "if x > 0 then 100 / x + i.x else 0",
            "    let x = i.y - 1\n",
        ),
        ("if i > 0 then 100 / i else 0", "    let i = i.x\n"),
        (
            "saved + i.x",
            "    let saved = if i.x > 0 then 100 / i.x else 0\n",
        ),
    ] {
        differential(&fixture(expr, lets), "checked", &inputs);
    }
    let mut p = fixture("if i.x < 0 then -i.x else i.x", "");
    let h = rule(&mut p)
        .hints
        .as_mut()
        .unwrap()
        .overflow
        .as_mut()
        .unwrap();
    h.min = 0;
    h.max = 10;
    differential(&p, "checked", &inputs);
    for &(x, y) in inputs
        .iter()
        .filter(|&&(x, y)| (-10..=10).contains(&x) && (1..=5).contains(&y))
    {
        assert_eq!(
            eval(&p, "checked", x, y).unwrap().to_string(),
            x.abs().to_string()
        );
    }
}

#[test]
fn numeric_guards_check_i64_edges_without_overflowing_the_proof() {
    // The lexer cannot spell MIN as a negative literal: its unsigned token
    // would exceed MAX. Exercise the verifier/native AST boundary explicitly.
    let extreme_fixture = |expr: &str| {
        let mut p = fixture(&expr.replace("-9223372036854775808", "minimum"), "");
        let r = rule(&mut p);
        r.logic.value =
            crate::optimizer::substitute_ident(&r.logic.value, "minimum", &Expr::Number(i64::MIN));
        fields(&mut p)[0].range = None;
        full(&mut p);
        p
    };
    let inputs = [
        i64::MIN,
        i64::MIN + 1,
        -2,
        -1,
        0,
        1,
        2,
        i64::MAX - 1,
        i64::MAX,
    ]
    .map(|x| (x, 1));
    for expr in [
        "if i.x < 9223372036854775807 then i.x + 1 else i.x",
        "if i.x > -9223372036854775808 then i.x - 1 else i.x",
        "if i.x != -9223372036854775808 then -i.x else 0",
        "if i.x == -9223372036854775808 then 0 else abs(i.x)",
        "if i.x > -9223372036854775808 then i.x / -1 else 0",
        "if i.x <= -9223372036854775808 then 0 else i.x % -1",
        "if i.x > 0 then 100 / i.x else if i.x < 0 then 100 / i.x else 0",
    ] {
        let p = extreme_fixture(expr);
        differential(&p, "checked", &inputs);
    }
    for expr in [
        "if i.x < -9223372036854775808 then 1 / 0 else 0",
        "if i.x > 9223372036854775807 then 1 / 0 else 0",
        "if i.x > -2 then abs(i.x) else -i.x",
    ] {
        let p = extreme_fixture(expr);
        assert!(!verify(&p).is_empty(), "{expr}");
    }
}

#[test]
fn numeric_guards_do_not_invent_or_leak_proof_facts() {
    for (expr, lets, needle) in [
        (
            "if i.x > 0 then unsafe_value else 0",
            "    let unsafe_value = 100 / i.x\n",
            "let 'unsafe_value'",
        ),
        (
            "if i.x > 0 and 100 / i.x > 0 then 1 else 0",
            "",
            "if condition",
        ),
        (
            "if i.x == 0 or 100 / i.x > 0 then 1 else 0",
            "",
            "if condition",
        ),
        (
            "if i.x > 0 then 100 / i.x else 100 / i.x",
            "",
            "else branch",
        ),
        (
            "(if i.x > 0 then 100 / i.x else 0) + 100 / i.x",
            "",
            "includes zero",
        ),
        (
            "100 / i.x",
            "    let safe = if i.x > 0 then 100 / i.x else 0\n",
            "includes zero",
        ),
        (
            "if alias > 0 then 100 / i.x else 0",
            "    let alias = i.x\n",
            "includes zero",
        ),
        (
            "if alias > 0 then 100 / d else 0",
            "    let d = i.x\n    let alias = d\n    let d = i.y - 1\n",
            "includes zero",
        ),
        (
            "if positive then 100 / d else 0",
            "    let d = i.x\n    let positive = d > 0\n    let d = i.y - 1\n",
            "includes zero",
        ),
        (
            "if i.x > 0 or i.y > 0 then 100 / i.x else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x > 0 and i.y > 0 then 0 else 100 / i.x",
            "",
            "includes zero",
        ),
        (
            "if i.x + 1 > 0 then 100 / (i.x + 1) else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x > 0 and i.x < 0 then 100 / i.x else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x > 0 then if i.x < 0 then 1 / 0 else 0 else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x > 0 and i.x < 0 then parse_int(\"bad\") else 0",
            "",
            "unsupported expression",
        ),
        ("if i.x > 0 then 1 else i.x > 1", "", "different types"),
    ] {
        refuses(&fixture(expr, lets), needle);
    }
    // A caller's guard cannot narrow the independently checked callee domain.
    let mut p = fixture("if i.x > 0 then callee(i) else 0", "");
    let mut callee = rule(&mut fixture("100 / i.x", "")).clone();
    callee.name = "callee".into();
    callee.hints = None;
    p.items.push(Item::Rule(callee));
    refuses(&p, "includes zero");
    // A checked callee may establish its OWN guard over the full public domain.
    if let Item::Rule(callee) = p.items.last_mut().unwrap() {
        callee.logic.value = rule(&mut fixture("if i.x > 0 then 100 / i.x else 0", ""))
            .logic
            .value
            .clone();
        callee.input_name = "other".into();
        callee.logic.value = crate::optimizer::substitute_ident(
            &callee.logic.value,
            "i",
            &Expr::Ident("other".into()),
        );
    }
    differential(&p, "checked", &[(-10, 1), (0, 1), (1, 1), (10, 1), (11, 1)]);
}

#[test]
fn numeric_guards_callee_result_facts_remain_valid_during_native_folding() {
    let mut p = fixture(
        "if denominator > 0 then 100 / denominator else 0",
        "    let denominator = positive(i)\n",
    );
    let mut callee = rule(&mut fixture("if i.x < 0 then -i.x + 1 else i.x + 1", "")).clone();
    callee.name = "positive".into();
    callee.hints = None;
    p.items.push(Item::Rule(callee));
    let inputs: Vec<_> = (-10..=10)
        .map(|x| (x, 1))
        .chain([(-11, 1), (11, 1)])
        .collect();
    differential(&p, "checked", &inputs);
    // The callee's checked [1, 11] makes the caller's condition redundant.
    let lowered = native_opt::lower(&p).unwrap();
    assert!(!matches!(
        rule(&mut lowered.clone()).logic.value,
        Expr::If(..)
    ));
    for x in -10i64..=10 {
        assert_eq!(
            eval(&p, "checked", x, 1).unwrap().to_string(),
            (100 / (x.abs() + 1)).to_string()
        );
    }
}

#[test]
fn numeric_nonzero_guards_preserve_arithmetic_and_lexical_values() {
    let inputs: Vec<_> = (-10..=10)
        .flat_map(|x| (1..=5).map(move |y| (x, y)))
        .chain([(-11, 1), (11, 1), (0, 0), (0, 6)])
        .collect();
    for (expr, lets) in [
        ("if i.x != 0 then 100 / i.x else 0", ""),
        ("if 0 != i.x then 100 % i.x else 0", ""),
        ("if i.x == 0 then 0 else 100 / i.x", ""),
        ("if not (i.x == 0) then 100 / i.x else 0", ""),
        ("if i.x != 0 then 100 / -i.x else 0", ""),
        ("if i.x != 0 then 100 / abs(i.x) else 0", ""),
        ("if i.x != 0 then 100 / min(i.x, -1) else 0", ""),
        ("if i.x != 0 then 100 / max(i.x, 1) else 0", ""),
        ("if i.x != -1 then 100 / (i.x + 1) else 0", ""),
        ("if i.x != 1 then 100 / (i.x - 1) else 0", ""),
        ("if i.x != 0 then 100 / (i.x * 2) else 0", ""),
        ("if i.x != 0 and i.x != 2 then 100 / i.x else 0", ""),
        ("if divisor != 0 then 100 / divisor else 0", "    let divisor = i.x + i.y\n"),
        ("100 / (divisor + 1)", "    let divisor = if i.x < 0 then -2 else 2\n"),
        ("100 / alias", "    let divisor = if i.x < 0 then -2 else 2\n    let alias = divisor\n    let divisor = 0\n"),
        ("if i.x != 0 then if i.x == 0 then 7 else 100 / i.x else 0", ""),
    ] {
        differential(&fixture(expr, lets), "checked", &inputs);
    }
    let mut p = fixture("if i.x != 0 then 100 / i.x else 0", "");
    fields(&mut p)[0].range = None;
    differential(
        &p,
        "checked",
        &[
            (i64::MIN, 1),
            (i64::MIN + 1, 1),
            (-1, 1),
            (0, 1),
            (1, 1),
            (i64::MAX, 1),
        ],
    );
}

#[test]
fn numeric_nonzero_still_requires_the_min_over_minus_one_guard() {
    let values = [
        i64::MIN,
        i64::MIN + 1,
        -2,
        -1,
        0,
        1,
        2,
        i64::MAX - 1,
        i64::MAX,
    ];
    let inputs: Vec<_> = values
        .iter()
        .flat_map(|&x| values.map(|y| (x, y)))
        .collect();
    for expr in [
        "if i.y != 0 and i.y != -1 then i.x / i.y else 0",
        "if i.y != -1 and i.y != 0 then i.x % i.y else 0",
        "if i.y == 0 or i.y == -1 then 0 else i.x / i.y",
        "if i.y != 0 and i.x >= -9223372036854775807 then i.x / i.y else 0",
    ] {
        let mut p = fixture(expr, "");
        for f in fields(&mut p) {
            f.range = None;
        }
        full(&mut p);
        differential(&p, "checked", &inputs);
    }
    for expr in [
        "if i.y != 0 then i.x / i.y else 0",
        "if i.y != 0 then i.x % i.y else 0",
    ] {
        let mut p = fixture(expr, "");
        for f in fields(&mut p) {
            f.range = None;
        }
        full(&mut p);
        refuses(&p, "MIN / -1");
    }
}

#[test]
fn numeric_nonzero_callee_results_preserve_public_contracts_and_native_folding() {
    let mut p = fixture("100 / saved", "    let saved = denominator(i)\n");
    let mut callee = rule(&mut fixture("if i.x < 0 then -2 else 2", "")).clone();
    callee.name = "denominator".into();
    callee.hints = None;
    callee.input_name = "other".into();
    callee.logic.value =
        crate::optimizer::substitute_ident(&callee.logic.value, "i", &Expr::Ident("other".into()));
    p.items.push(Item::Rule(callee));
    let inputs: Vec<_> = (-10..=10)
        .map(|x| (x, 1))
        .chain([(-11, 1), (11, 1)])
        .collect();
    differential(&p, "checked", &inputs);
    for x in -10..=10 {
        assert_eq!(
            eval(&p, "checked", x, 1).unwrap().to_string(),
            if x < 0 { "-50" } else { "50" }
        );
    }
    // An explicit public interval promises the full interval, including zero.
    let hints = rule(&mut fixture("0", "")).hints.clone();
    if let Item::Rule(callee) = p.items.last_mut().unwrap() {
        callee.hints = hints;
    }
    refuses(&p, "includes zero");
}

#[test]
fn numeric_nonzero_never_reuses_stale_exclusions_or_hides_widened_holes() {
    for (expr, lets, needle) in [
        (
            "if i.x != 0 then 100 / (i.x * 0) else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x != 0 then 100 / (i.x + 1) else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x != 0 then 100 / min(i.x, 0) else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x != 0 then 100 / max(i.x, 0) else 0",
            "",
            "includes zero",
        ),
        (
            "if alias != 0 then 100 / i.x else 0",
            "    let alias = i.x\n",
            "includes zero",
        ),
        (
            "if i.x != 0 and 100 / i.x > 0 then 1 else 0",
            "",
            "if condition",
        ),
        (
            "if i.x != 0 then 100 / i.x else 100 / i.x",
            "",
            "else branch",
        ),
        (
            "if i.x != 0 and i.x == 0 then 100 / i.x else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x != 0 then if i.x == 0 then 1 / 0 else 0 else 0",
            "",
            "includes zero",
        ),
        (
            "if i.x != 0 and i.x != 2 then 100 / (i.x - 2) else 0",
            "",
            "includes zero",
        ),
        (
            "100 / divisor",
            "    let divisor = if i.x < 0 then -2 else if i.y < 3 then 2 else 4\n",
            "includes zero",
        ),
    ] {
        refuses(&fixture(expr, lets), needle);
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
                        if let Ok(Value::Number(ranges)) = arithmetic(op, (a, b), (c, d)) {
                            let (lo, hi) = ranges.hull();
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
        "guarded_numeric",
        "nonzero_numeric",
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
    // The old invented nonnegative domain selected unsigned constant division
    // even for negative runtime inputs, including outside overflow contracts.
    let mut legacy = fixture("i.x / 100", "");
    rule(&mut legacy).hints = None;
    fields(&mut legacy)[0].range = None;
    differential(
        &legacy,
        "checked",
        &[
            (i64::MIN, 1),
            (-101, 1),
            (-100, 1),
            (-1, 1),
            (0, 1),
            (100, 1),
            (i64::MAX, 1),
        ],
    );
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
            (
                include_str!("../../examples/guarded_numeric.verbose").to_owned(),
                1,
            ),
            (
                include_str!("../../examples/nonzero_numeric.verbose").to_owned(),
                1,
            ),
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
