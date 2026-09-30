//! Constructor obligations must use the value visible at the source position.
use crate::{
    ast::*,
    interpreter::Value,
    native,
    selfhost_constructor_tests::{interpreted, parse, source},
    selfhost_constructor_type_tests::TAKE,
    verifier,
};
use std::{fs, path::Path, process::Command};

fn check(src: &str) -> Vec<verifier::VerifyError> {
    verifier::verify_program(&parse(src), Path::new("examples"))
}
fn probe(body: &str) -> String {
    source(
        body,
        if body.contains("take(") { TAKE } else { "" },
        if body.contains("take(") { "take" } else { "" },
    )
}
fn rejects(src: &str, field: &str) {
    let errors = check(src);
    assert!(
        errors.iter().any(|e| e.context.contains("constructor")
            && e.context.contains(&format!("field '{field}'"))),
        "{src}\n{errors:?}"
    );
}
fn accepts(src: &str) {
    let errors = check(src);
    assert!(errors.is_empty(), "{src}\n{errors:?}");
}
fn binders(value: &str) -> String {
    probe(&format!("    let text = 7\n    out = match Choice::Empty:\n      Label(n, text) => take(Pair {{ first: {value}, second: n }})\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => take(Pair {{ first: text, second: 2 }})"))
}

#[test]
fn constructor_scope_lets_aliases_and_unknown_rebindings() {
    for body in [
        "    let p = Pair { first: later, second: 2 }\n    let later = 7\n    out = 0",
        "    let alias = later\n    let later = 7\n    let p = Pair { first: alias, second: 2 }\n    out = 0",
        "    let x = \"bad\"\n    let alias = x\n    let x = 7\n    let p = Pair { first: alias, second: 2 }\n    out = 0",
        "    let x = 7\n    let x = missing\n    let p = Pair { first: x, second: 2 }\n    out = 0",
        "    let i = \"bad\"\n    let p = Pair { first: i.n, second: 2 }\n    out = 0",
        "    let i = \"bad\"\n    let p = Pair { first: i, second: 2 }\n    out = 0",
        "    let p = Pair { first: p.first, second: 2 }\n    out = 0",
    ] { rejects(&probe(body), "first"); }
    for body in [
        "    let x = 7\n    let alias = x\n    let x = \"bad\"\n    let p = Pair { first: alias, second: 2 }\n    out = p.first",
        "    let x = 7\n    let x = x + 1\n    let p = Pair { first: x, second: 2 }\n    out = p.first",
        "    let i = 7\n    let p = Pair { first: i, second: 2 }\n    out = p.first",
        "    let p = Pair { first: 7, second: 2 }\n    let p = Pair { first: p.first + 1, second: 2 }\n    out = p.first",
    ] { accepts(&probe(body)); }
}

#[test]
fn constructor_scope_variant_binders_shadow_without_leaking() {
    rejects(&binders("text"), "first");
    accepts(&binders("n"));
    // The rule input name can itself be a numeric or text payload binder.
    rejects(
        &binders("text")
            .replace("Label(n, text)", "Label(n, i)")
            .replace("first: text, second: n", "first: i, second: n"),
        "first",
    );
    accepts(
        &binders("n")
            .replace("Label(n, text)", "Label(i, text)")
            .replace("first: n, second: n", "first: i, second: i"),
    );
    rejects(
        &probe("    out = if 1 < 2 then 7 else take(Pair { first: \"bad\", second: 2 })"),
        "first",
    );
    rejects(&probe("    let p = Choice::Link { value: 7, next: Choice::Label { n: \"bad\", text: \"x\" } }\n    out = 0"), "n");
}

#[test]
fn constructor_scope_result_payloads_and_absent_arms() {
    for (target, body, valid) in [
        ("Ok(7)", "v", true),
        ("Ok(\"x\")", "v", false),
        ("Err(7)", "e", true),
        ("Err(\"x\")", "e", false),
        ("if 1 < 2 then Ok(7) else Err(\"x\")", "v", true),
        ("if 1 < 2 then Ok(7) else Ok(\"x\")", "v", false),
        ("Ok(7)", "e", false), // outer `e` must not type the absent payload
        ("Err(\"x\")", "v", false),
    ] {
        let (yes, no) = if body == "v" {
            ("take(Pair { first: v, second: 2 })", "0")
        } else {
            ("0", "take(Pair { first: e, second: 2 })")
        };
        let src = probe(&format!("    let v = 7\n    let e = 7\n    let r = {target}\n    let alias = r\n    let r = 0\n    out = match_result(alias, v => {yes}, e => {no})"));
        if valid {
            accepts(&src);
        } else {
            rejects(&src, "first");
        }
    }
    let helper = r#"
rule result
  @intention: "Declared payloads bind both arms"
  @source: invoices.intent:1
  input:
    value : number
  output:
    out : Result(number, text)
  logic:
    out = if value > 0 then Ok(value) else Err("failed")
  proofs:
    purity:
      reads: [value]
      calls: []
    termination:
      bound: 32
"#;
    for (err, valid) in [("length(e)", true), ("e", false)] {
        let src = source(&format!("    out = match_result(result(7), v => take(Pair {{ first: v, second: 2 }}), e => take(Pair {{ first: {err}, second: 2 }}))"), &(TAKE.to_owned()+helper), "result, take");
        if valid {
            accepts(&src);
        } else {
            rejects(&src, "first");
        }
    }
}

fn collection(body: &str) -> String {
    probe(body)
        .replacen(
            "    n : number\nconcept Pair",
            "    n : number\n    items : collection(text)\nconcept Pair",
            1,
        )
        .replacen("reads: []", "reads: [i.items]", 1)
}
#[test]
fn constructor_scope_collection_and_fold_binders() {
    for expr in [
        "sum(i.items, x => take(Pair { first: VALUE, second: 2 }))",
        "sum(filter(i.items, x => x == \"x\"), x => take(Pair { first: VALUE, second: 2 }))",
        "sum(map(i.items, x => x), x => take(Pair { first: VALUE, second: 2 }))",
        "fold(i.items, 0, acc, x => acc + take(Pair { first: VALUE, second: 2 }))",
    ] {
        for (value, valid) in [("x", false), ("length(x)", true)] {
            let src = collection(&format!(
                "    let x = 7\n    out = {}",
                expr.replace("VALUE", value)
            ));
            if valid {
                accepts(&src);
            } else {
                rejects(&src, "first");
            }
        }
    }
    // A nonnumeric fold accumulator must not inherit a same-named numeric let.
    rejects(&collection("    let acc = 7\n    out = fold(i.items, \"x\", acc, x => take(Pair { first: acc, second: 2 }))"), "first");
    // Byte/index binders are numbers; the source and initial value use outer scope.
    accepts(&probe("    let x = \"ab\"\n    out = fold_bytes(x, 0, acc, x, idx => acc + take(Pair { first: x, second: idx }))"));
    rejects(&probe("    out = fold_bytes(\"ab\", 0, acc, byte, idx => take(TextRow { n: acc, label: byte, flag: idx > 0 }))"), "label");
}

#[test]
fn constructor_scope_fold_requires_a_stable_accumulator() {
    let bad = collection("    let unused = fold(i.items, 0, acc, x => concat(\"x\", take(Pair { first: acc, second: 2 })))\n    out = 0");
    let errors = check(&bad);
    assert!(
        errors
            .iter()
            .any(|e| e.context.contains("constructor in fold")
                && e.message.contains("across iterations")),
        "{errors:?}"
    );
    accepts(&collection("    let unused = fold(i.items, 0, acc, x => take(Pair { first: acc + length(x), second: 2 }))\n    out = 0"));
}

#[test]
fn constructor_scope_dependencies_must_establish_the_payload_type() {
    for rhs in [
        "1 + missing",
        "if 1 < 2 then 7 else \"bad\"",
        "if 1 then 7 else 8",
        "byte_at(\"abc\", \"bad\")",
        "length(7)",
        "take(7)",
        "match_result(7, v => 1, e => 2)",
    ] {
        let src = probe(&format!(
            "    let value = {rhs}\n    let p = Pair {{ first: value, second: 2 }}\n    out = 0"
        ));
        rejects(&src, "first");
    }
    accepts(&probe("    let value = match_result(Ok(7), v => v, e => 0)\n    let p = Pair { first: value, second: 2 }\n    out = p.first"));
}

#[test]
fn constructor_scope_preserves_rust_storage_types() {
    let declarations = r#"concept Stored
  @intention: "The Rust checker retains existing storage types"
  @source: invoices.intent:1
  fields:
    data : bytes
    result : Result(number, text)
    items : collection(text)
"#;
    for (result, valid) in [
        ("Ok(7)", true),
        ("Err(\"x\")", true),
        ("Ok(\"x\")", false),
        ("Err(7)", false),
    ] {
        let src = collection(&format!("    let p = Stored {{ data: b\"\\x00\", result: {result}, items: i.items }}\n    out = 0")).replace("rule probe\n", &format!("{declarations}\nrule probe\n"));
        if valid {
            accepts(&src);
        } else {
            rejects(&src, "result");
        }
    }
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn constructor_scope_valid_values_match_original_interpreter_and_native() {
    let path =
        std::env::temp_dir().join(format!("verbose-constructor-scope-{}", std::process::id()));
    for (body, expected) in [
        ("    let x = 7\n    let alias = x\n    let x = \"text\"\n    out = take(Pair { first: alias, second: 2 })", 7),
        ("    let x = 7\n    let x = x + 1\n    out = take(Pair { first: x, second: 2 })", 8),
        ("    let value = \"outer\"\n    out = match_result(Ok(7), value => take(Pair { first: value, second: 2 }), error => 0)", 7),
        ("    let text = 99\n    out = match Choice::Label { n: 7, text: \"é\" }:\n      Label(n, text) => take(Pair { first: n + length(text), second: 2 })\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => 0", 9),
    ] {
        let src = probe(body); accepts(&src);
        let p = parse(&src);
        assert_eq!(interpreted(&p).unwrap(), Value::Number(expected));
        let p = crate::optimizer::optimize_program(&p).0;
        let compiled = native::compile_native(&p, "probe", path.to_str().unwrap(), false, false);
        if body.contains("match_result(Ok(") {
            assert!(compiled.unwrap_err().message.contains("match_result target must be a rule call"));
            continue;
        }
        compiled.unwrap_or_else(|e| panic!("{body}: {e}"));
        let output = Command::new(&path).arg("0").output().unwrap();
        assert_eq!((output.status.code(),output.stdout,output.stderr),(Some(0),format!("{expected}\n").into_bytes(),vec![]),"{body}");
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn constructor_scope_service_diagnostics_keep_the_field_and_state_declaration() {
    let src = fs::read_to_string("examples/last_path_service.verbose").unwrap();
    accepts(&src);
    let bad = src.replace("status: 200", "status: state.last");
    let errors = check(&bad);
    assert!(
        errors.iter().any(|e| {
            e.context == "service 'memo' / handler 'recall' / logic"
                && e.message
                    .contains("constructor 'HttpResponse' / field 'status'")
                && e.message.contains("state.last is declared 'text [..256]'")
        }),
        "{errors:?}"
    );
}
