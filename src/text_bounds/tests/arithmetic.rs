use super::*;

fn source(expr: &str, bindings: &str, lo: i64, hi: i64) -> String {
    // An unannotated number already means the complete i64 domain; the lexer
    // cannot spell MIN's positive magnitude as one signed numeric token.
    let range = if (lo, hi) == (i64::MIN, i64::MAX) { String::new() }
        else { format!(" [{lo}, {hi}]") };
    format!(r#"@verbose 0.1.0
concept Input
  @intention: "Give arithmetic explicit input domains"
  @source: pipeline_totals.intent:1
  fields:
    title : text [..8]
    x : number{range}
    y : number [-9, -1]
rule render
  @intention: "Check arithmetic before formatting its result"
  @source: pipeline_totals.intent:3
  input:
    i : Input
  output:
    out : text [..128]
  logic:
{bindings}    out = {expr}
  proofs:
    purity:
      reads: [i.title, i.x, i.y]
      calls: []
    termination:
      bound: 1000
"#)
}

// Source proof correctness is checked by the public examples and CLI tests.
// These probes isolate interval/storage behavior using the public backend gate.
fn compare(expr: &str, bindings: &str, lo: i64, hi: i64, xs: &[i64]) {
    let p = parse(&source(expr, bindings, lo, hi));
    assert!(verify(&p).is_empty(), "{:?}", verify(&p));
    let path = format!("/tmp/verbose-record-arithmetic-{}", std::process::id());
    crate::native::compile_native(&p, "render", &path, false, false).unwrap();
    let bytes = fs::read(&path).unwrap();
    let optimized = crate::optimizer::optimize_program(&p).0;
    crate::native::compile_native(&optimized, "render", &path, false, false).unwrap();
    assert_eq!(bytes, fs::read(&path).unwrap());
    let rules: Vec<_> = p.items.iter().filter_map(|i| match i { Item::Rule(r) => Some(r), _ => None }).collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    for &x in xs {
        for y in [-9, -3, -1] {
            for title in ["", "café", "éééé"] {
                let data = HashMap::from([
                    ("title".into(), interpreter::Value::Text(title.into())),
                    ("x".into(), interpreter::Value::Number(x)),
                    ("y".into(), interpreter::Value::Number(y)),
                ]);
                let reference = interpreter::eval_rule(rules[0], &rules, &concepts, &[], &data).unwrap();
                let result = Command::new(&path).args([title, &x.to_string(), &y.to_string()]).output().unwrap();
                assert_eq!((result.status.code(), result.stdout, result.stderr),
                    (Some(0), format!("{reference}\n").into_bytes(), vec![]), "{expr}: x={x} y={y}");
            }
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn record_arithmetic_matches_original_ast_for_signed_operations_and_text_lifetimes() {
    for expr in [
        "concat(i.title, i.x + i.y)", "concat(i.x - i.y, i.title)",
        "concat(i.x * i.y)", "concat(i.x / i.y, \":\", i.x % i.y)",
        "concat(-i.x, \":\", abs(i.x))", "concat(min(i.x, i.y), \":\", max(i.x, i.y))",
        "concat((i.x + 4) * (i.y - 2) / 2)",
        "if i.x * i.y >= 0 then concat(i.x + i.y) else concat(i.x - i.y)",
    ] {
        compare(expr, "", -100, 100, &[-100, -8, -1, 0, 1, 8, 100]);
    }
    compare("concat(saved, alias + i.y, first, saved)",
        "    let saved = concat(\"[\", i.title, \"]\")\n    let first = i.x * 2\n    let alias = first\n    let first = concat(i.title, i.x / i.y)\n",
        -100, 100, &[-100, 0, 100]);
}

#[test]
fn record_arithmetic_preserves_i64_boundaries_and_truncation_toward_zero() {
    for expr in [
        "concat(i.x / 2, \":\", i.x % 2)", "concat(i.x / (-2), \":\", i.x % (-2))",
        "concat(i.x / (-9223372036854775807 - 1), \":\", i.x % (-9223372036854775807 - 1))",
        "concat(min(i.x, i.y), \":\", max(i.x, i.y))",
        "concat(min(max(i.x, -100), 100) * 10)",
        "concat(i.x + 0, \":\", i.x - 0)", "concat(i.x * 1)",
    ] {
        compare(expr, "", i64::MIN, i64::MAX, &[i64::MIN, i64::MIN + 1, -7, -1, 0, 1, 7, i64::MAX]);
    }
    compare("concat(-i.x, \":\", abs(i.x))", "", i64::MIN + 1, i64::MAX,
        &[i64::MIN + 1, -1, 0, 1, i64::MAX]);
}

#[test]
fn record_arithmetic_refuses_unsafe_intermediates_and_unknown_relations_before_emission() {
    let path = format!("/tmp/verbose-record-arithmetic-refusal-{}", std::process::id());
    fs::write(&path, b"preserve").unwrap();
    for (expr, bindings, message) in [
        ("concat(i.x + 1)", "", "may overflow i64"),
        ("concat(i.x - 1)", "", "may overflow i64"),
        ("concat(i.x * 2)", "", "may overflow i64"),
        ("concat(min(i.x * 2, 100))", "", "may overflow i64"),
        ("concat(-i.x)", "", "at MIN"), ("concat(abs(i.x))", "", "at MIN"),
        ("concat(i.x / (-1))", "", "MIN / -1"), ("concat(i.x % (-1))", "", "MIN % -1"),
        ("concat(1 / i.x)", "", "includes zero"),
        ("concat(if i.x != 0 then 1 / i.x else 0)", "", "includes zero"),
        ("concat(i.x - i.x)", "", "may overflow i64"),
        ("\"safe\"", "    let unused = i.x + 1\n", "may overflow i64"),
        ("if 1 == 1 then \"safe\" else concat(i.x + 1)", "", "may overflow i64"),
        ("concat(i.title + 1)", "", "expected Number"),
        ("concat(min(i.x, i.title))", "", "expected Number"),
        ("concat(abs(i.x > 0))", "", "expected Number"),
    ] {
        let p = parse(&source(expr, bindings, i64::MIN, i64::MAX));
        rejects(&p, message);
        let error = crate::native::compile_native(&p, "render", &path, false, false).unwrap_err();
        assert!(error.message.contains(message), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
    }
    fs::remove_file(path).unwrap();
    rejects(&parse(&source("concat(abs(i.x))", "", 1, -2)), "invalid numeric interval");
}

#[test]
fn record_arithmetic_computes_and_checks_transfer_intervals_at_public_boundaries() {
    let source = include_str!("../../../examples/pipeline_totals.verbose");
    let p = parse(source);
    let errors = crate::verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    for source in [
        source.replace("total : number [0, 1000000000]", "total : number [0, 999999999]"),
        source.replace("item.quantity * item.unit_price", "item.quantity * item.unit_price + 1"),
    ] {
        let errors = crate::execution::verify(&parse(&source));
        assert!(errors.iter().any(|e| e.message.contains("cannot prove argument range")), "{errors:?}");
    }
}
