//! Named constructor layout in the Verbose evaluator and native emitter.
use crate::{
    ast::*,
    interpreter::{self, Value},
    lexer::Lexer,
    native, optimizer,
    parser::Parser,
    verifier,
};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::Path,
    process::{Command, Output, Stdio},
};

pub(crate) fn parse(src: &str) -> Program {
    Parser::new(Lexer::new(src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

pub(crate) fn source(body: &str, extra: &str, calls: &str) -> String {
    let reads = if body.contains("i.n") { "i.n" } else { "" };
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Unused or numeric probe input"
  @source: invoices.intent:1
  fields:
    n : number
concept Pair
  @intention: "A named pair"
  @source: invoices.intent:1
  fields:
    first : number
    second : number
concept Triple
  @intention: "Three distinct named slots"
  @source: invoices.intent:1
  fields:
    a : number
    b : number
    c : number
concept TextRow
  @intention: "Mixed existing field representations"
  @source: invoices.intent:1
  fields:
    n : number
    label : text
    flag : bool
concept Envelope
  @intention: "Nested record storage"
  @source: invoices.intent:1
  fields:
    pair : Pair
    weight : number
concept_group Values [max_depth: 128, max_nodes: 10000]
  @intention: "Variant payloads use declared positional binders"
  @source: invoices.intent:1
  concept Choice
    @intention: "Three fields, nested values and empty payloads"
    @source: invoices.intent:1
    variants:
      Trio of (a : number, b : number, c : number)
      Link of (value : number, next : Choice)
      Label of (n : number, text : text)
      Empty
rule probe
  @intention: "Observe named field values after source-order construction"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
{body}
  proofs:
    purity:
      reads: [{reads}]
      calls: [{calls}]
    termination:
      bound: 10000
{extra}"#
    )
}

pub(crate) fn send(executable: &Path, src: &str, index: usize, unlimited: bool) -> Output {
    let mut command = if unlimited {
        let mut c = Command::new("sh");
        c.args([
            "-c",
            "ulimit -s unlimited; exec \"$1\" \"$2\"",
            "constructor-probe",
        ])
        .arg(executable);
        c
    } else {
        Command::new(executable)
    };
    let mut child = command
        .arg(index.to_string())
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
    child.wait_with_output().unwrap()
}

pub(crate) fn install(output: Output, path: &Path) {
    assert_eq!((output.status.code(), output.stderr), (Some(0), vec![]));
    assert!(output.stdout.starts_with(b"\x7fELF"));
    fs::write(path, output.stdout).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

pub(crate) fn interpreted(p: &Program) -> Result<Value, interpreter::RuntimeError> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let input = HashMap::from([("n".to_string(), Value::Number(0))]);
    interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input)
}

fn cases() -> Vec<(String, i64)> {
    let mut cases = vec![];
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let fields = order
            .map(|i| format!("{}: {}", ["a", "b", "c"][i], i + 1))
            .join(", ");
        cases.push((
            source(
                &format!("    let p = Triple {{ {fields} }}\n    out = p.a * 100 + p.b * 10 + p.c"),
                "",
                "",
            ),
            123,
        ));
        cases.push((source(&format!("    let p = Choice::Trio {{ {fields} }}\n    out = match p:\n      Trio(a, b, c) => a * 100 + b * 10 + c\n      Link(value, next) => 0\n      Label(n, text) => 0\n      Empty => 0"), "", ""), 123));
    }
    for (body, expected) in [
        ("    let p = Pair { second: 2, first: 1 }\n    out = p.first * 10 + p.second", 12),
        ("    let p = Pair { second: 2, first: 1 }\n    let q = p\n    let p = Pair { first: 8, second: 9 }\n    out = q.first * 100 + q.second * 10 + p.second", 129),
        ("    let p = Pair { second: if 1 < 2 then 2 else 8, first: 1 }\n    out = p.first * 10 + p.second", 12),
        ("    let p = Envelope { weight: 7, pair: Pair { second: 2, first: 1 } }\n    out = p.pair.first * 100 + p.pair.second * 10 + p.weight", 127),
        ("    let p = TextRow { flag: 1 < 2, label: \"é\\n🦀\", n: 9 }\n    out = if p.flag then p.n + length(p.label) else 0", 16),
        ("    let p = TextRow { label: \"é\\n🦀\", n: 9, flag: 1 < 2 }\n    let q = p\n    out = if q.label == \"é\\n🦀\" then q.n else 0", 9),
        ("    let p = Choice::Link { next: Choice::Trio { c: 3, a: 1, b: 2 }, value: 7 }\n    out = match p:\n      Link(value, next) => value * 1000 + decode(next)\n      Trio(a, b, c) => 0\n      Label(n, text) => 0\n      Empty => 0", 7123),
        ("    let p = Choice::Label { text: \"é\\n\", n: 7 }\n    out = match p:\n      Label(n, text) => n + length(text)\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Empty => 0", 10),
        ("    let p = Choice::Empty\n    out = match p:\n      Empty => 42\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Label(n, text) => 0", 42),
    ] { let helper = r#"
rule decode
  @intention: "Read the declared positions of a nested variant"
  @source: invoices.intent:1
  input:
    v : Choice
  output:
    out : number
  logic:
    out = match v:
      Trio(a, b, c) => a * 100 + b * 10 + c
      Link(value, next) => 0
      Label(n, text) => 0
      Empty => 0
  proofs:
    purity:
      reads: [v]
      calls: []
    termination:
      bound: 32
"#;
        let (extra, calls) = if body.contains("decode(") { (helper, "decode") } else { ("", "") };
        cases.push((source(body, extra, calls), expected)); }
    let helper = r#"
rule combine
  @intention: "Consume a record passed through a call"
  @source: invoices.intent:1
  input:
    p : Pair
  output:
    out : number
  logic:
    out = p.first * 10 + p.second
  proofs:
    purity:
      reads: [p.first, p.second]
      calls: []
    termination:
      bound: 32
"#;
    cases.push((source("    let p = Pair { second: combine(Pair { second: 4, first: 3 }), first: combine(Pair { second: 2, first: 1 }) }\n    out = combine(p)",helper,"combine"),154));
    cases
}

fn invalid_sources() -> Vec<String> {
    let mut out = vec![];
    for literal in [
        "Pair { first: 1 }",
        "Pair { first: 1, missing: 2 }",
        "Pair { first: 1, second: 2, extra: 3 }",
        "Pair { first: 1, first: 2 }",
        "Pair { first: 1, second: 2, first: 3 }",
        "Unknown { first: 1 }",
        "Unknown {}",
        "Choice::Missing",
        "Choice { a: 1, b: 2, c: 3 }",
        "Pair::Pair { first: 1, second: 2 }",
        "Choice::Trio { a: 1, b: 2 }",
        "Choice::Trio { c: 3, a: 1, a: 2 }",
        "Choice::Empty { a: 1 }",
        "Envelope { weight: 1, pair: Pair { second: 2 } }",
    ] {
        out.push(source(
            &format!("    let p = {literal}\n    out = 7"),
            "",
            "",
        ));
    }
    // A full structural walk must reach constructors that type inference skips.
    for body in [
        "    out = if 1 < 2 then 7 else length(Pair { first: 1 })",
        "    out = length(Pair { first: 1 })",
        "    out = match Choice::Empty:\n      Empty => length(Pair { first: 1 })\n      Trio(a, b, c) => 0\n      Link(value, next) => 0\n      Label(n, text) => 0",
        "    out = match_result(Ok(7), value => value, error => length(Pair { first: 1 }))",
    ] { out.push(source(body,"","")); }
    out.push(
        source(
            "    let p = Pair { first: 1, second: 2 }\n    out = 7",
            "",
            "",
        )
        .replace("    second : number", "    first : number"),
    );
    out.push(
        source(
            "    out = sum(i.items, x => length(Pair { first: x }))",
            "",
            "",
        )
        .replace(
            "    n : number\nconcept Pair",
            "    items : collection(number)\nconcept Pair",
        )
        .replace("reads: []", "reads: [i.items]"),
    );
    let service = include_str!("../examples/hello_http.verbose");
    out.extend([
        service.replace("status: 200", "missing: 200"),
        service.replace(
            "body: \"Hello from Verbose over HTTP!\"",
            "status: \"Hello from Verbose over HTTP!\"",
        ),
        service.replace(", body: \"Hello from Verbose over HTTP!\"", ""),
    ]);
    out
}

/// Both the ordinary test and gen0/gen1 bootstrap use this complete matrix.
pub(crate) fn assert_constructors(compiler: &Path, evaluator: &Path, base: &Path) {
    let binary = base.join("constructor-probe");
    for (src, expected) in cases() {
        let p = parse(&src);
        assert!(
            verifier::verify_program(&p, Path::new("examples")).is_empty(),
            "{src}\n{:?}",
            verifier::verify_program(&p, Path::new("examples"))
        );
        assert_eq!(interpreted(&p).unwrap(), Value::Number(expected), "{src}");
        // Compare Rust native where accepted and pin its legacy local-record
        // refusal. Its variant path is not an initializer-order oracle.
        if src.contains("let p = Triple {")
            || src.contains("let p = Pair { second: 2, first: 1 }\n    out")
        {
            let reference = base.join("constructor-rust-reference");
            let compiled = native::compile_native(
                &optimizer::optimize_program(&p).0,
                "probe",
                reference.to_str().unwrap(),
                false,
                false,
            );
            match compiled {
                Ok(()) => {
                    let r = Command::new(reference).arg("0").output().unwrap();
                    assert_eq!(
                        (r.status.code(), r.stdout, r.stderr),
                        (Some(0), format!("{expected}\n").into_bytes(), vec![])
                    );
                }
                Err(e) => assert!(e.message.contains("rich operations (collection/result/record/concat) not supported in native backend"), "{e}"),
            }
        }
        let evaluated = send(evaluator, &src, 0, false);
        assert_eq!(
            (evaluated.status.code(), evaluated.stdout, evaluated.stderr),
            (Some(0), format!("{expected}\n").into_bytes(), vec![]),
            "evaluator: {src}"
        );
        install(send(compiler, &src, 0, false), &binary);
        let r = Command::new(&binary).arg("0").output().unwrap();
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(0), format!("{expected}\n").into_bytes(), vec![]),
            "emitted: {src}"
        );
    }
    let record = source("    out = Pair { second: 2, first: 1 }", "", "")
        .replace("    out : number", "    out : Pair");
    let p = parse(&record);
    assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
    assert_eq!(
        interpreted(&p).unwrap(),
        Value::Record(HashMap::from([
            ("first".into(), Value::Number(1)),
            ("second".into(), Value::Number(2))
        ]))
    );
    install(send(compiler, &record, 0, false), &binary);
    let r = Command::new(&binary).arg("0").output().unwrap();
    assert_eq!(
        (r.status.code(), r.stdout, r.stderr),
        (Some(0), b"{\"first\":1,\"second\":2}\n".to_vec(), vec![])
    );

    // Keep eager, once-in-source-order field lowering. Sorting expressions
    // would reverse which of these two distinct failures is observed.
    for (fields, status, signal) in [
        (
            "second: byte_at(\"x\", i.n + 2), first: 1 / i.n",
            Some(1),
            None,
        ),
        (
            "first: 1 / i.n, second: byte_at(\"x\", i.n + 2)",
            None,
            Some(8),
        ),
        ("first: 7, second: byte_at(\"x\", i.n + 2)", Some(1), None),
    ] {
        let src = source(
            &format!("    let p = Pair {{ {fields} }}\n    out = p.first"),
            "",
            "",
        );
        let p = parse(&src);
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
        assert!(interpreted(&p).is_err());
        install(send(compiler, &src, 0, false), &binary);
        let r = Command::new(&binary).arg("0").output().unwrap();
        assert_eq!(
            (r.status.code(), r.status.signal(), r.stdout, r.stderr),
            (status, signal, vec![], vec![]),
            "{src}"
        );
    }
    for src in invalid_sources() {
        let _ = parse(&src);
        let r = send(compiler, &src, 0, false);
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(1), vec![], vec![]),
            "refusal: {src}"
        );
    }
}

fn assert_raw_refusals(raw: &Path) {
    let service = send(
        raw,
        include_str!("../examples/hello_http.verbose"),
        0,
        false,
    );
    assert!(service.status.success() && !service.stdout.is_empty());
    assert!(service.stderr.is_empty());
    for src in invalid_sources() {
        let r = send(raw, &src, 0, false);
        assert_eq!(
            (r.status.code(), r.stdout, r.stderr),
            (Some(1), vec![], vec![]),
            "raw refusal: {src}"
        );
    }
}

pub(crate) fn assert_emitted_drivers(compiler: &Path, base: &Path) {
    let src = fs::read_to_string("examples/vexprparse.verbose").unwrap();
    for name in ["x86_program_src", "type_check", "check_program"] {
        let index = src
            .lines()
            .filter(|l| l.starts_with("rule "))
            .position(|l| l == format!("rule {name}"))
            .unwrap();
        let driver = base.join(format!("constructor-{name}"));
        install(send(compiler, &src, index, true), &driver);
        if name == "x86_program_src" {
            assert_raw_refusals(&driver);
        } else {
            // Both drivers themselves pass TCheckListState with src/prog in
            // reverse order. Exercise the real self-source sites as well as
            // synthetic programs, after each bootstrap generation.
            for (literal, expected) in [
                ("Pair { second: 2, first: 1 }", b"0\n"),
                ("Pair { first: 1 }", b"1\n"),
                ("Pair { first: \"wrong\", second: 2 }", b"1\n"),
            ] {
                let probe = source(&format!("    let p = {literal}\n    out = 7"), "", "");
                let r = send(&driver, &probe, 0, false);
                assert_eq!(
                    (r.status.code(), r.stdout, r.stderr),
                    (Some(0), expected.to_vec(), vec![]),
                    "{name}: {probe}"
                );
            }
        }
    }
    crate::selfhost_constructor_type_tests::assert_types(
        compiler,
        &base.join("constructor-x86_program_src"),
        &base.join("constructor-type_check"),
        base,
    );
    let gate_index = src.lines().filter(|l| l.starts_with("rule "))
        .position(|l| l == "rule collection_lowering_check").unwrap();
    let gate = base.join("collection_lowering_check");
    install(send(compiler, &src, gate_index, true), &gate);
    crate::selfhost_collection_tests::assert_collections(
        compiler, &base.join("constructor-x86_program_src"),
        &base.join("constructor-type_check"), &gate, base,
    );
}

#[test]
fn selfhost_named_constructors_preserve_values_and_evaluation_order() {
    let src = fs::read_to_string("examples/vexprparse.verbose").unwrap();
    let p = parse(&src);
    let base = std::env::temp_dir().join(format!("verbose-constructors-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let compiler = base.join("compiler");
    let evaluator = base.join("evaluator");
    let raw = base.join("raw");
    for (name, path) in [
        ("elf_program_src", &compiler),
        ("eval_main", &evaluator),
        ("x86_program_src", &raw),
    ] {
        native::compile_native_stdin_raw(&p, name, path.to_str().unwrap()).unwrap();
    }
    assert_constructors(&compiler, &evaluator, &base);
    assert_raw_refusals(&raw);
    fs::remove_dir_all(&base).unwrap();
}
