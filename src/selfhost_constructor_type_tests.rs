//! Field type obligations in the compiler written in Verbose.
use crate::{interpreter::Value, native, selfhost_constructor_tests::*, verifier};
use std::{fs, path::Path, process::Command};

fn make(body: &str) -> String {
    source(body, "", "")
}

fn extra_concepts(src: String, declarations: &str) -> String {
    src.replace("rule probe\n", &format!("{declarations}\nrule probe\n"))
}

const OTHER: &str = r#"concept Other
  @intention: "Same fields do not establish nominal identity"
  @source: invoices.intent:1
  fields:
    first : number
    second : number
"#;

fn scalar_helper(input: &str, output: &str, body: &str) -> String {
    format!(
        r#"
rule helper
  @intention: "A declared scalar contract"
  @source: invoices.intent:1
  input:
    value : {input}
  output:
    out : {output}
  logic:
{body}
  proofs:
    purity:
      reads: [value]
      calls: []
    termination:
      bound: 128
"#
    )
}

fn cases() -> Vec<(String, i64)> {
    let mut out = vec![];
    for (body, expected) in [
        ("    let p = Pair { second: 2, first: 1 }\n    out = p.first * 10 + p.second", 12),
        ("    let a = 7\n    let b = a\n    let a = \"later\"\n    let p = Pair { first: b, second: 2 }\n    out = p.first", 7),
        ("    let a = 7\n    let a = a + 1\n    let p = Pair { first: a, second: 2 }\n    out = p.first", 8),
        ("    let p = TextRow { flag: 1 < 2, n: 9, label: \"é🦀\" }\n    let q = p\n    let row = TextRow { label: q.label, n: q.n, flag: q.flag }\n    out = row.n + length(row.label)", 15),
        ("    let p = Pair { first: if 1 < 2 then 1 else 2, second: byte_at(\"abc\", 1) }\n    out = p.first + p.second", 99),
        ("    let p = Pair { first: length(\"é🦀\"), second: max(3, 7) }\n    out = p.first + p.second", 13),
        ("    let p = Pair { first: bnot(0), second: band(7, 3) }\n    out = p.first + p.second", 2),
        ("    let a = Pair { first: 1, second: 2 }\n    let p = Envelope { pair: a, weight: 7 }\n    out = p.pair.first + p.weight", 8),
    ] {
        out.push((make(body),expected));
    }
    let record_helper = r#"
rule consume
  @intention: "Consume a mixed record constructed in a match arm"
  @source: invoices.intent:1
  input:
    row : TextRow
  output:
    out : number
  logic:
    out = row.n + length(row.label)
  proofs:
    purity:
      reads: [row.n, row.label]
      calls: []
    termination:
      bound: 32
"#;
    out.push((source("    let text = 99\n    let p = Choice::Label { text: \"é\", n: 7 }\n    out = match p:\n      Label(n, text) => consume(TextRow { n: n, label: text, flag: n > 0 })\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => 0", record_helper, "consume"),9));
    // Scalar parameter types must not be replaced by the old number default.
    out.push((source("    out = helper(\"é\")", &scalar_helper("text", "number", "    let p = TextRow { n: 7, label: value, flag: 1 < 2 }\n    out = p.n + length(p.label)"), "helper"),9));
    out.push((source("    out = helper(1 < 2)", &scalar_helper("bool", "number", "    let p = TextRow { n: 7, label: \"x\", flag: value }\n    out = if p.flag then p.n else 0"), "helper"),7));
    out.push((
        source(
            "    let p = Pair { first: helper(7), second: 2 }\n    out = p.first",
            &scalar_helper("number", "number", "    out = value + 1"),
            "helper",
        ),
        8,
    ));
    out.push((source(
        "    let p = Pair { first: take(Pair { second: 2, first: 7 }), second: 2 }\n    out = p.first",
        TAKE, "take",
    ), 7));
    out.push((source("    let text = 7\n    out = match Choice::Empty:\n      Label(n, text) => consume(TextRow { n: n, label: text, flag: n > 0 })\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => take(Pair { first: text, second: 2 })", &(record_helper.to_owned() + TAKE), "consume, take"),7));
    out.push((make("    out = match_result(Ok(7), value => length(\"x\") + take(Pair { first: 1, second: 2 }), error => 0)").replace("calls: []", "calls: [take]")+r#"
rule take
  @intention: "Consume an independent constructor in a Result arm"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    out : number
  logic:
    out = p.first
  proofs:
    purity:
      reads: [p.first]
      calls: []
    termination:
      bound: 16
"#,2));
    out
}

fn rejected() -> Vec<String> {
    let mut out = vec![];
    for literal in [
        "Pair { first: \"wrong\", second: 2 }",
        "Pair { first: 1 < 2, second: 2 }",
        "Pair { first: b\"x\", second: 2 }",
        "TextRow { n: 7, label: 2, flag: 1 < 2 }",
        "TextRow { n: 7, label: 1 < 2, flag: 1 < 2 }",
        "TextRow { n: 7, label: \"x\", flag: 1 }",
        "TextRow { n: 7, label: \"x\", flag: \"wrong\" }",
        "Choice::Label { n: 7, text: 8 }",
        "Choice::Trio { a: 1, b: \"wrong\", c: 3 }",
        "Choice::Link { value: 7, next: Pair { first: 1, second: 2 } }",
        "Envelope { weight: 7, pair: Choice::Empty }",
        "Envelope { weight: 7, pair: Pair { first: \"wrong\", second: 2 } }",
        "Pair { first: if 1 < 2 then 7 else \"wrong\", second: 2 }",
        "Pair { first: missing, second: 2 }",
        "Pair { first: (1 < 2) + 1, second: 2 }",
        "Pair { first: byte_at(\"abc\", \"x\"), second: 2 }",
        "Pair { first: length(7), second: 2 }",
        "Pair { first: max(1 < 2, 3), second: 2 }",
        "Pair { first: band(\"x\", 3), second: 2 }",
        "TextRow { n: 7, label: substring(\"abc\", \"x\", 1), flag: 1 < 2 }",
    ] {
        out.push(make(&format!("    let ignored = {literal}\n    out = 7")));
    }
    for body in [
        "    let s = \"wrong\"\n    let a = s\n    let p = Pair { first: a, second: 2 }\n    out = 7",
        "    let n = 7\n    let n = \"wrong\"\n    let p = Pair { first: n, second: 2 }\n    out = 7",
        "    let i = \"wrong\"\n    let p = Pair { first: i.n, second: 2 }\n    out = 7",
        "    out = if 1 < 2 then 7 else take(Pair { first: \"wrong\", second: 2 })",
    ] {out.push(source(body, TAKE, if body.contains("take(") {"take"} else {""}));}
    out.push(extra_concepts(
        make(
            "    let p = Envelope { pair: Other { first: 1, second: 2 }, weight: 7 }\n    out = 7",
        ),
        OTHER,
    ));
    out.push(source(
        "    out = helper(\"x\")",
        &scalar_helper(
            "text",
            "number",
            "    let p = Pair { first: value, second: 2 }\n    out = 7",
        ),
        "helper",
    ));
    out.push(source(
        "    out = helper(1 < 2)",
        &scalar_helper(
            "bool",
            "number",
            "    let p = Pair { first: value, second: 2 }\n    out = 7",
        ),
        "helper",
    ));
    out.push(source(
        "    let p = Pair { first: helper(\"x\"), second: 2 }\n    out = 7",
        &scalar_helper("number", "number", "    out = value + 1"),
        "helper",
    ));
    out.push(extra_concepts(source(
        "    let p = Pair { first: take(Other { first: 7, second: 2 }), second: 2 }\n    out = 7",
        TAKE, "take",
    ), OTHER));
    out.push(source(
        "    let p = Pair { first: helper(\"x\"), second: 2 }\n    out = 7",
        &scalar_helper("text", "text", "    out = value"),
        "helper",
    ));
    for field in ["status: \"wrong\", body: \"ok\"", "status: 200, body: 7"] {
        out.push(include_str!("../examples/hello_http.verbose").replace(
            "status: 200, body: \"Hello from Verbose over HTTP!\"",
            field,
        ));
    }
    out
}

const TAKE: &str = r#"
rule take
  @intention: "A declared numeric record consumer"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    out : number
  logic:
    out = p.first
  proofs:
    purity:
      reads: [p.first]
      calls: []
    termination:
      bound: 16
"#;

fn collection_probe(value: &str) -> String {
    source(
        &format!("    out = sum(i.items, x => take(Pair {{ first: {value}, second: 2 }}))"),
        TAKE,
        "take",
    )
    .replace(
        "    n : number\nconcept Pair",
        "    items : collection(number)\nconcept Pair",
    )
    .replace("reads: []", "reads: [i.items]")
}

/// Same obligation matrix in the ordinary test and after both generations.
pub(crate) fn assert_types(compiler: &Path, raw: &Path, checker: &Path, base: &Path) {
    let executable = base.join("constructor-types-value");
    for (src, want) in cases() {
        let p = parse(&src);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{errors:?}\n{src}");
        assert_eq!(interpreted(&p).unwrap(), Value::Number(want), "{src}");
        let checked = send(checker, &src, 0, false);
        assert_eq!(
            (checked.status.code(), checked.stdout, checked.stderr),
            (Some(0), b"0\n".to_vec(), vec![]),
            "type check: {src}"
        );
        install(send(compiler, &src, 0, false), &executable);
        let r = Command::new(&executable).arg("0").output().unwrap();
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(0), format!("{want}\n").into_bytes(), vec![]),
            "{src}"
        );
    }
    for src in rejected() {
        let p = parse(&src);
        assert!(
            !verifier::verify_program(&p, Path::new("examples")).is_empty(),
            "Rust reference accepted: {src}"
        );
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![]),
                "{backend:?}: {src}"
            );
        }
    }
    // Rust's legacy constructor walk uses the final let environment and misses
    // these nested binder types. Pin the stricter self-hosted refusals separately.
    let future =
        make("    let p = Pair { first: later, second: 2 }\n    let later = 7\n    out = 7");
    let arm = source("    let p = Choice::Label { n: 7, text: \"x\" }\n    out = match p:\n      Label(n, text) => take(Pair { first: text, second: n })\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => 0", TAKE, "take");
    let result = source("    let value = 7\n    out = match_result(Ok(\"x\"), value => take(Pair { first: value, second: 2 }), error => 0)", TAKE, "take");
    for future in [future, arm, result] {
        assert!(verifier::verify_program(&parse(&future), Path::new("examples")).is_empty());
        for backend in [compiler, raw] {
            let r = send(backend, &future, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![])
            );
        }
    }
    // A known collection binder works; mismatches in its body are traversed.
    for value in ["x", "\"wrong\""] {
        let src = collection_probe(value);
        let p = parse(&src);
        assert_eq!(
            verifier::verify_program(&p, Path::new("examples")).is_empty(),
            value == "x"
        );
        let r = send(compiler, &src, 0, false);
        assert_eq!(
            r.status.code(),
            Some(if value == "x" { 0 } else { 1 }),
            "{src}"
        );
        if value == "x" {
            install(r, &executable);
            let result = Command::new(&executable)
                .args(["3", "2", "5", "9"])
                .output()
                .unwrap();
            assert_eq!(
                (result.status.code(), result.stdout, result.stderr),
                (Some(0), b"16\n".to_vec(), vec![])
            );
        } else {
            assert!(r.stdout.is_empty() && r.stderr.is_empty());
        }
        let r = send(raw, &src, 0, false);
        assert_eq!(r.status.code(), Some(if value == "x" { 0 } else { 1 }));
        assert_eq!(r.stdout.is_empty(), value != "x");
        assert!(r.stderr.is_empty());
    }
    for expression in [
        "fold(i.items, 0, acc, x => acc + take(Pair { first: x, second: 2 }))",
        "sum(filter(i.items, x => x > 2), x => take(Pair { first: x, second: 2 }))",
        "sum(map(i.items, x => x + 1), x => take(Pair { first: x, second: 2 }))",
    ] {
        let src = collection_probe("x").replace(
            "sum(i.items, x => take(Pair { first: x, second: 2 }))",
            expression,
        );
        assert!(
            verifier::verify_program(&parse(&src), Path::new("examples")).is_empty(),
            "{src}"
        );
        let r = send(checker, &src, 0, false);
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(0), b"0\n".to_vec(), vec![]),
            "{src}"
        );
        // Nested map/filter lowering is a separate legacy limitation; only
        // direct collection folds are a runtime oracle here.
        if expression.starts_with("fold(") {
            install(send(compiler, &src, 0, false), &executable);
            let r = Command::new(&executable)
                .args(["3", "2", "5", "9"])
                .output()
                .unwrap();
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(0), b"16\n".to_vec(), vec![]),
                "{src}"
            );
        }
    }
    // Resource reads are outside the strict initializer classifier. This is
    // the one deliberate acceptance change in the existing example corpus.
    for backend in [compiler, raw] {
        let r = send(
            backend,
            include_str!("../examples/tagged_bonuses.verbose"),
            0,
            false,
        );
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(1), vec![], vec![])
        );
    }
    // Result component inference is deliberately unsupported, even for valid
    // numeric binders. The outer binding must not supply the missing type.
    for body in [
        "    let value = 7\n    out = match_result(Ok(8), value => take(Pair { first: value, second: 2 }), error => 0)",
        "    let value = match_result(Ok(8), value => value, error => 0)\n    let p = Pair { first: value, second: 2 }\n    out = 7",
    ] {
        let src = source(body, TAKE, if body.contains("take(") { "take" } else { "" });
        assert!(verifier::verify_program(&parse(&src), Path::new("examples")).is_empty(), "{src}");
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(1), vec![], vec![]), "{src}");
        }
    }
    // Unsupported storage is a capability refusal, not an implicit scalar.
    for ty in ["bytes", "collection(number)", "Result(number, text)"] {
        let src=extra_concepts(make("    let p = Container { data: i.data }\n    out = 7"), &format!("concept Container\n  @intention: \"Unsupported stored type\"\n  @source: invoices.intent:1\n  fields:\n    data : {ty}\n"))
            .replace("    n : number\nconcept Pair",&format!("    data : {ty}\nconcept Pair")).replace("reads: []","reads: [i.data]");
        assert!(
            verifier::verify_program(&parse(&src), Path::new("examples")).is_empty(),
            "{src}"
        );
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![]),
                "{src}"
            );
        }
    }
}

#[test]
fn selfhost_constructor_values_obey_declared_types_and_scopes() {
    let p = parse(include_str!("../examples/vexprparse.verbose"));
    let base =
        std::env::temp_dir().join(format!("verbose-constructor-types-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let compiler = base.join("compiler");
    let raw = base.join("raw");
    let checker = base.join("checker");
    for (name, path) in [
        ("elf_program_src", &compiler),
        ("x86_program_src", &raw),
        ("type_check", &checker),
    ] {
        native::compile_native_stdin_raw(&p, name, path.to_str().unwrap()).unwrap();
    }
    assert_types(&compiler, &raw, &checker, &base);
    fs::remove_dir_all(base).unwrap();
}
