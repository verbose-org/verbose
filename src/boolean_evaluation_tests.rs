//! Source boolean semantics, including errors that make evaluation observable.
use crate::{
    ast::*,
    interpreter::{self, Value},
    lexer::Lexer,
    parser::Parser,
    verifier,
};
use std::{collections::HashMap, path::Path};

fn source(body: &str, kind: &str, extra: &str, calls: &str) -> String {
    let reads = ["i.s", "i.n", "i.items"]
        .into_iter()
        .filter(|name| body.contains(name))
        .collect::<Vec<_>>()
        .join(", ");
    let items = if body.contains("i.items") {
        "    items : collection(number)\n"
    } else {
        ""
    };
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Boolean evaluation input"
  @source: invoices.intent:1
  fields:
    s : text
    n : number
{items}rule probe
  @intention: "Evaluate guarded expressions"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : {kind}
  logic:
{body}
  proofs:
    purity:
      reads: [{reads}]
      calls: [{calls}]
    termination:
      bound: 1000
{extra}"#
    )
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
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let input = HashMap::from([
        ("s".into(), Value::Text(s.into())),
        ("n".into(), Value::Number(n)),
        (
            "items".into(),
            Value::List(vec![Value::Number(0), Value::Number(2)]),
        ),
    ]);
    interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).map_err(|e| e.message)
}

#[test]
fn boolean_truth_tables_and_nested_guards() {
    for op in ["and", "or"] {
        let p = checked(&source(
            &format!("    out = i.n > 0 {op} length(i.s) > 0"),
            "bool",
            "",
            "",
        ));
        for (s, n) in [("", 0), ("x", 0), ("", 1), ("x", 1)] {
            let expected = if op == "and" {
                n > 0 && !s.is_empty()
            } else {
                n > 0 || !s.is_empty()
            };
            assert_eq!(eval(&p, s, n), Ok(Value::Bool(expected)));
        }
    }
    for (body, expected_empty) in [
        ("length(i.s) == 0 or byte_at(i.s, 0) > 0", true),
        ("length(i.s) > 0 and byte_at(i.s, 0) > 0", false),
        (
            "i.n > 0 and (length(i.s) == 0 or byte_at(i.s, 0) > 0)",
            true,
        ),
    ] {
        let p = checked(&source(&format!("    out = {body}"), "bool", "", ""));
        for s in ["", "a", "é🦀", "\0"] {
            let expected = if s.is_empty() {
                expected_empty
            } else {
                s.as_bytes()[0] > 0
            };
            assert_eq!(eval(&p, s, 1), Ok(Value::Bool(expected)), "{body}, {s:?}");
        }
    }
}

#[test]
fn boolean_errors_only_from_evaluated_operands() {
    for (rhs, diagnostic) in [
        ("byte_at(i.s, i.n) > 0", "byte_at index out of range"),
        (
            "length(substring(i.s, 0, i.n)) > 0",
            "substring bounds out of range",
        ),
        ("10 / i.n > 0", "division by zero"),
        ("10 % i.n > 0", "modulo by zero"),
    ] {
        let n = if rhs.contains("substring") { 1 } else { 0 };
        for (op, skip_lhs, required_lhs) in
            [("and", "1 == 2", "1 == 1"), ("or", "1 == 1", "1 == 2")]
        {
            let skipped = checked(&source(
                &format!("    out = {skip_lhs} {op} ({rhs})"),
                "bool",
                "",
                "",
            ));
            assert_eq!(eval(&skipped, "", n), Ok(Value::Bool(op == "or")));
            for expr in [
                format!("{required_lhs} {op} ({rhs})"),
                format!("({rhs}) {op} {skip_lhs}"),
            ] {
                let p = checked(&source(&format!("    out = {expr}"), "bool", "", ""));
                assert!(eval(&p, "", n).unwrap_err().contains(diagnostic), "{expr}");
            }
        }
    }
}

#[test]
fn boolean_aliases_calls_and_collections_keep_source_order() {
    let p = checked(&source("    let gate = i.n > 0\n    let saved = gate\n    let gate = i.n < 0\n    out = saved or (gate and byte_at(i.s, 0) > 0)", "bool", "", ""));
    assert_eq!(eval(&p, "", 1), Ok(Value::Bool(true)));
    assert_eq!(eval(&p, "", 0), Ok(Value::Bool(false)));
    assert!(eval(&p, "", -1).is_err());

    let extra = source(
        "    let first = byte_at(i.s, 0)\n    out = first > 0",
        "bool",
        "",
        "",
    )
    .split("rule probe")
    .nth(1)
    .unwrap()
    .to_owned();
    let extra = format!("rule read_first{extra}");
    let call_source = source(
        "    out = i.n == 0 or read_first(i)",
        "bool",
        &extra,
        "read_first",
    )
    .replace("reads: [i.n]", "reads: [i.n, i]");
    let p = checked(&call_source);
    assert_eq!(eval(&p, "", 0), Ok(Value::Bool(true)));
    assert!(eval(&p, "", 1).unwrap_err().contains("byte_at"));
    assert_eq!(eval(&p, "é", 1), Ok(Value::Bool(true)));
    let undeclared = parse(&call_source.replace("calls: [read_first]", "calls: []"));
    assert!(!verifier::verify_program(&undeclared, Path::new("examples")).is_empty());

    let p = checked(&source(
        "    out = map(i.items, n => n == 0 or 10 / n > 0)",
        "collection(bool)",
        "",
        "",
    ));
    assert_eq!(
        eval(&p, "", 0),
        Ok(Value::List(vec![Value::Bool(true), Value::Bool(true)]))
    );

    let p = checked(&source(
        "    let first = byte_at(i.s, 0)\n    out = 1 == 1 or first > 0",
        "bool",
        "",
        "",
    ));
    assert!(
        eval(&p, "", 0).unwrap_err().contains("byte_at"),
        "lets remain eager"
    );
}

#[test]
fn boolean_guards_do_not_waive_source_verification() {
    let missing_read = source("    out = 1 == 1 or byte_at(i.s, 0) > 0", "bool", "", "")
        .replace("reads: [i.s]", "reads: []");
    for src in [
        missing_read,
        source("    out = 1 == 1 or 7", "bool", "", ""),
        source("    out = 1 == 2 and \"text\"", "bool", "", ""),
    ] {
        assert!(
            !verifier::verify_program(&parse(&src), Path::new("examples")).is_empty(),
            "{src}"
        );
    }
    // Also defend direct evaluator callers without coercing scalars to bool.
    for (expr, side) in [
        ("1 or (10 / i.n > 0)", "left"),
        ("\"x\" and (10 / i.n > 0)", "left"),
        ("1 == 1 and 1", "right"),
        ("1 == 2 or \"x\"", "right"),
    ] {
        let p = parse(&source(&format!("    out = {expr}"), "bool", "", ""));
        assert!(
            eval(&p, "", 0)
                .unwrap_err()
                .contains(&format!("requires bool on the {side}")),
            "{expr}"
        );
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn boolean_native_matches_guarded_source_values() {
    use std::{fs, process::Command};
    let path = std::env::temp_dir().join(format!("verbose-boolean-{}", std::process::id()));
    for condition in [
        "length(i.s) == 0 or byte_at(i.s, 0) > 0",
        "length(i.s) > 0 and byte_at(i.s, 0) > 0",
        "i.n <= 0 or (length(i.s) > 0 and byte_at(i.s, 0) > 0)",
        "i.n == 0 or 10 / i.n > 0",
        "1 == 1 or byte_at(i.s, i.n) > 0",
        "1 == 2 and byte_at(i.s, i.n) > 0",
    ] {
        let p = checked(&source(
            &format!("    out = if {condition} then 7 else 9"),
            "number",
            "",
            "",
        ));
        let optimized = crate::optimizer::optimize_program(&p).0;
        for emitted in [&p, &optimized] {
            crate::native::compile_native(emitted, "probe", path.to_str().unwrap(), false, false)
                .unwrap();
            for (s, n) in [("", 0), ("a", 1), ("é🦀", -1)] {
                let expected = eval(&p, s, n).unwrap();
                let out = Command::new(&path)
                    .args([s, &n.to_string()])
                    .output()
                    .unwrap();
                assert_eq!(
                    (out.status.code(), out.stdout, out.stderr),
                    (Some(0), format!("{expected}\n").into_bytes(), vec![]),
                    "{condition}"
                );
            }
        }
    }
    fs::remove_file(path).unwrap();
}
