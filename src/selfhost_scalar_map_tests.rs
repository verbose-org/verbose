//! Scalar maps: values, exit status, input boundaries and bounded arena reuse.
use crate::{
    ast::{iter_all_concepts, Item},
    interpreter::{self, Value},
    native,
    selfhost_constructor_tests::*,
    selfhost_constructor_type_tests::TAKE,
    verifier,
};
use std::{
    collections::HashMap, fs, os::unix::process::ExitStatusExt, path::Path, process::Command,
};

fn program(body: &str, kind: &str, fields: &str, lets: &str, extra: &str, calls: &str) -> String {
    source(&format!("{lets}    out = map(i.items, e => {body})"), extra, calls)
        .replace("    n : number\nconcept Pair", &format!("    items : collection(Row)\nconcept Row\n  @intention: \"A map input element\"\n  @source: invoices.intent:1\n  fields:\n{fields}\nconcept Pair"))
        .replacen("    out : number\n  logic:", &format!("    out : collection({kind})\n  logic:"), 1)
        .replacen("reads: []", "reads: [i.items]", 1)
}
const FIELDS: &str = "    name : text\n    n : number";
fn number(body: &str) -> String {
    program(body, "number", FIELDS, "", "", "")
}
fn boolean(body: &str) -> String {
    program(body, "bool", FIELDS, "", "", "")
}
fn row(name: &str, n: i64) -> Value {
    Value::Record(HashMap::from([
        ("name".into(), Value::Text(name.into())),
        ("n".into(), Value::Number(n)),
    ]))
}
fn eval(
    src: &str,
    rows: Vec<Value>,
    limit: Option<i64>,
) -> Result<Value, interpreter::RuntimeError> {
    let p = parse(src);
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let mut input = HashMap::from([("items".into(), Value::List(rows))]);
    if let Some(n) = limit {
        input.insert("limit".into(), Value::Number(n));
    }
    interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input)
}
fn assert_run(bin: &Path, argv: &[String], expected: &(i32, Vec<u8>), context: &str) {
    let r = Command::new(bin).args(argv).output().unwrap();
    assert_eq!(
        (r.status.code(), r.stdout, r.stderr),
        (Some(expected.0), expected.1.clone(), vec![]),
        "{context}\n{argv:?}"
    );
}
fn emitted(compiler: &Path, src: &str, bin: &Path) {
    let a = send(compiler, src, 0, false);
    let b = send(compiler, src, 0, false);
    assert_eq!(
        (a.status.code(), &a.stdout, &a.stderr),
        (b.status.code(), &b.stdout, &b.stderr),
        "reproducibility: {src}"
    );
    install(a, bin);
}
fn expected(src: &str, rows: Vec<Value>, limit: Option<i64>) -> (i32, Vec<u8>) {
    let Value::List(values) = eval(src, rows, limit).unwrap() else {
        panic!("collection required")
    };
    let status = if values.contains(&Value::Bool(false)) {
        1
    } else {
        0
    };
    (
        status,
        values
            .iter()
            .map(|v| format!("{v}\n"))
            .collect::<String>()
            .into_bytes(),
    )
}

// The self-hosted expression parser already has AstBool literals; the Rust
// grammar does not. Use equivalent comparisons only in the reference fixtures.
fn reference_source(src: &str) -> String {
    src.replace("=> true", "=> (1 == 1)")
        .replace("=> false", "=> (1 == 0)")
        .replace("then true", "then (1 == 1)")
        .replace("else false", "else (1 == 0)")
}

pub(crate) fn assert_scalar_maps(compiler: &Path, raw: &Path, base: &Path) {
    let bin = base.join("scalar-map");
    let rust = base.join("scalar-map-rust");
    let mut cases = vec![
        (number("e.n"), true),
        (number("length(e.name)"), true),
        (boolean("e.n >= 65"), true),
        (boolean("e.name == \"é🦀\""), true),
        (boolean("not (e.n >= 65)"), true),
        (boolean("true"), true),
        (boolean("false"), true),
        (boolean("if e.n >= 65 then true else false"), true),
        (program("e.n >= a", "bool", FIELDS, "    let threshold = 65\n    let a = threshold\n    let threshold = 100\n    let e = 999\n", "", ""), false),
        (program("e.name == a", "bool", FIELDS, "    let text = \"é🦀\"\n    let a = text\n    let text = \"later\"\n", "", ""), false),
        (program("take(Pair { first: e.n, second: length(e.name) })", "number", FIELDS, "", TAKE, "take"), false),
    ];
    let predicate = TAKE
        .replace("out : number", "out : bool")
        .replace("out = p.first", "out = p.first >= 65");
    cases.push((
        program(
            "not take(Pair { first: e.n, second: 0 })",
            "bool",
            FIELDS,
            "",
            &predicate,
            "take",
        ),
        false,
    ));
    // Numeric metadata is retained while a direct trailing collection is consumed.
    cases.push((
        boolean("e.n >= i.limit")
            .replace(
                "    items : collection(Row)",
                "    limit : number\n    items : collection(Row)",
            )
            .replace("reads: [i.items]", "reads: [i.items, i.limit]"),
        false,
    ));
    for (src, rust_oracle) in cases {
        let reference = reference_source(&src);
        let p = parse(&reference);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{errors:?}\n{src}");
        emitted(compiler, &src, &bin);
        let r = send(raw, &src, 0, false);
        assert_eq!(r.status.code(), Some(0), "{src}");
        assert!(!r.stdout.is_empty() && r.stderr.is_empty());
        if rust_oracle {
            native::compile_native(&p, "probe", rust.to_str().unwrap(), false, false).unwrap();
        }
        for rows in [
            vec![],
            vec![("a", 70), ("b", 80)],
            vec![("a", 20), ("b", 30)],
            vec![("é🦀", 64), ("", 65), ("last", 70)],
            vec![("", i64::MIN), ("", i64::MAX)],
        ] {
            let limit = src.contains("limit : number").then_some(65);
            let mut args: Vec<String> = limit.into_iter().map(|v| v.to_string()).collect();
            args.push(rows.len().to_string());
            for (name, n) in &rows {
                args.push((*name).into());
                args.push(n.to_string());
            }
            let expected = expected(
                &reference,
                rows.iter().map(|(name, n)| row(name, *n)).collect(),
                limit,
            );
            assert_run(&bin, &args, &expected, &src);
            if rust_oracle {
                assert_run(&rust, &args, &expected, &src);
            }
        }
    }
    // Scalar-number input gets the same boolean formatting/status convention.
    for body in ["e > 0", "not (e > 0)", "true", "false"] {
        let src = boolean(body).replace("items : collection(Row)", "items : collection(number)");
        let reference = reference_source(&src);
        let p = parse(&reference);
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
        emitted(compiler, &src, &bin);
        native::compile_native(&p, "probe", rust.to_str().unwrap(), false, false).unwrap();
        for rows in [vec![], vec![1, 2], vec![-1, 0, 2], vec![i64::MIN, i64::MAX]] {
            let mut args = vec![rows.len().to_string()];
            args.extend(rows.iter().map(i64::to_string));
            let expected = expected(
                &reference,
                rows.into_iter().map(Value::Number).collect(),
                None,
            );
            assert_run(&bin, &args, &expected, &src);
            assert_run(&rust, &args, &expected, &src);
        }
    }
    // A nested reduction clobbers r8/r9. The outer number-only element must continue.
    let src = program(
        "e.n + sum(i.items, other => other.n)",
        "number",
        "    n : number",
        "",
        "",
        "",
    );
    emitted(compiler, &src, &bin);
    let rows = vec![2, 5, 9];
    let expected = expected(
        &src,
        rows.iter()
            .map(|n| Value::Record(HashMap::from([("n".into(), Value::Number(*n))])))
            .collect(),
        None,
    );
    assert_run(
        &bin,
        &["3".into(), "2".into(), "5".into(), "9".into()],
        &expected,
        &src,
    );

    // Exercise independent text slots through the final mapped argv page.
    let fields = (0..16)
        .map(|n| format!("    text{n} : text"))
        .collect::<Vec<_>>()
        .join("\n");
    let src = program(
        "byte_at(e.text0, 0) + byte_at(e.text15, 65535)",
        "number",
        &fields,
        "",
        "",
        "",
    );
    emitted(compiler, &src, &bin);
    let mut args = vec!["1".into()];
    args.extend((0..16).map(|n| {
        if n == 15 {
            "z".repeat(65536)
        } else {
            "é".into()
        }
    }));
    assert_run(&bin, &args, &(0, b"317\n".to_vec()), "last text slot");

    // Pin the original user-facing example and its boolean process status.
    let src = include_str!("../examples/retirement.verbose");
    emitted(compiler, src, &bin);
    let p = parse(src);
    native::compile_native(
        &p,
        "retirement_status",
        rust.to_str().unwrap(),
        false,
        false,
    )
    .unwrap();
    for (args, expected) in [
        (vec!["0"], (0, vec![])),
        (
            vec!["3", "alice", "64", "bob", "65", "carol", "70"],
            (1, b"false\ntrue\ntrue\n".to_vec()),
        ),
        (
            vec!["2", "bob", "65", "carol", "70"],
            (0, b"true\ntrue\n".to_vec()),
        ),
    ] {
        let args: Vec<_> = args.into_iter().map(str::to_owned).collect();
        assert_run(&bin, &args, &expected, "retirement");
        assert_run(&rust, &args, &expected, "Rust retirement");
    }

    // Argv count checks run before loading or printing the first element.
    let src = number("e.n");
    emitted(compiler, &src, &bin);
    for args in [
        vec!["-1"],
        vec!["-9223372036854775808"],
        vec!["9223372036854775807"],
        vec!["2", "x", "1"],
        vec!["1", "x"],
    ] {
        assert_run(
            &bin,
            &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            &(1, vec![]),
            "malformed count/tail",
        );
    }
    // A checked copy may fill its complete 64 KiB slot; the next byte refuses.
    let src = number("length(e.name)");
    emitted(compiler, &src, &bin);
    for len in [65535, 65536, 65537] {
        assert_run(
            &bin,
            &["1".into(), "x".repeat(len), "7".into()],
            &(
                if len <= 65536 { 0 } else { 1 },
                if len <= 65536 {
                    format!("{len}\n").into_bytes()
                } else {
                    vec![]
                },
            ),
            "text capacity",
        );
    }
    // Native streamed publication keeps the completed prefix after a later failure.
    let src = number("byte_at(\"a\", e.n)");
    emitted(compiler, &src, &bin);
    assert!(eval(&src, vec![row("x", 0), row("y", 1)], None).is_err());
    assert_run(
        &bin,
        &["2".into(), "x".into(), "0".into(), "y".into(), "1".into()],
        &(1, b"97\n".to_vec()),
        "late body failure",
    );
    let src = boolean("e.n > 0");
    emitted(compiler, &src, &bin);
    assert_run(
        &bin,
        &[
            "2".into(),
            "ok".into(),
            "1".into(),
            "x".repeat(65537),
            "2".into(),
        ],
        &(1, b"true\n".to_vec()),
        "late copy failure",
    );

    let mut refused = vec![
        // The uncalled text projection is outside this slice, at every entry.
        include_str!("../examples/payroll.verbose").to_owned(),
        program("e.n > 0", "number", FIELDS, "", "", ""),
        program("e.n", "bool", FIELDS, "", "", ""),
        program("e.name", "text", FIELDS, "", "", ""),
        number("missing"),
        boolean("e.n > 0").replace("name : text", "name : bool"),
        boolean("e.n > 0").replace("name : text", "name : Pair"),
        boolean("e.n > 0").replace(
            "items : collection(Row)",
            "prefix : text\n    items : collection(Row)",
        ),
        boolean("e.n > 0").replace(
            "items : collection(Row)",
            "items : collection(Row)\n    suffix : number",
        ),
        boolean("e.n > 0")
            .replace(
                "    out = map(i.items",
                "    let rows = i\n    out = map(rows.items",
            )
            .replace("reads: [i.items]", "reads: [i]"),
        boolean("e.n > 0")
            .replace(
                "    out = map(i.items",
                "    let i = i\n    out = map(i.items",
            )
            .replace("reads: [i.items]", "reads: [i]"),
        boolean("e.n > 0").replace(
            "    out = map(i.items",
            "    let rows = i.items\n    out = map(rows",
        ),
        boolean("e.n > 0").replace("map(i.items", "filter(i.items"),
        number("e.n + sum(i.items, other => other.n)"),
    ];
    // Do not let a call hide a nested traversal that could replace text slots.
    let extra = r#"
rule total
  @intention: "A nested traversal"
  @source: invoices.intent:1
  input:
    v : Input
  output:
    out : number
  logic:
    out = sum(v.items, x => x.n)
  proofs:
    purity:
      reads: [v.items]
      calls: []
    termination:
      bound: 128
"#;
    refused.push(
        program(
            "total(i) + length(e.name)",
            "number",
            FIELDS,
            "",
            extra,
            "total",
        )
        .replace("reads: [i.items]", "reads: [i.items, i]"),
    );
    let fields = (0..17)
        .map(|n| format!("    text{n} : text"))
        .chain(["    n : number".into()])
        .collect::<Vec<_>>()
        .join("\n");
    refused.push(program("e.n > 0", "bool", &fields, "", "", ""));
    for src in refused {
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout.len(), r.stderr),
                (Some(1), 0, vec![]),
                "{src}"
            );
        }
    }
    // Check all parsed rules, even before or after the selected ELF entry.
    let good = boolean("e.n > 0");
    let selected =
        good[good.find("rule probe\n").unwrap()..].replace("rule probe", "rule selected");
    let bad = program("e.n", "bool", FIELDS, "", "", "");
    let bad_rule = &bad[bad.find("rule probe\n").unwrap()..];
    for src in [
        format!("{bad}\n{selected}"),
        format!(
            "{}{}\n{}",
            &good[..good.find("rule probe\n").unwrap()],
            selected,
            bad_rule
        ),
    ] {
        for backend in [compiler, raw] {
            for entry in [0, 1] {
                let r = send(backend, &src, entry, false);
                assert_eq!(
                    (r.status.code(), r.stdout.len(), r.stderr),
                    (Some(1), 0, vec![]),
                    "unselected incompatible map"
                );
            }
        }
    }
    // A page-sized arena must suffice for thousands of independent element/body nodes.
    // Shrink only the arena mmap reservation in this test image. The negative control
    // discards the saved mark instead of restoring it, proving the cap is effective.
    let src = program(
        "take(Pair { first: e.n, second: 0 })",
        "number",
        FIELDS,
        "",
        TAKE,
        "take",
    );
    emitted(compiler, &src, &bin);
    let mut image = fs::read(&bin).unwrap();
    let mmap = [0x48, 0xbe, 0, 0, 0, 0, 6, 0, 0, 0];
    let sites: Vec<_> = image
        .windows(mmap.len())
        .enumerate()
        .filter_map(|(i, w)| (w == mmap).then_some(i))
        .collect();
    assert_eq!(sites.len(), 1);
    image[sites[0] + 2..sites[0] + 10].copy_from_slice(&4096u64.to_le_bytes());
    fs::write(&bin, &image).unwrap();
    let mut args = vec!["4000".into()];
    for _ in 0..4000 {
        args.extend(["x".into(), "7".into()]);
    }
    assert_run(
        &bin,
        &args,
        &(0, "7\n".repeat(4000).into_bytes()),
        "one-page arena reuse",
    );
    let restore = [0x41, 0x5e, 0x49, 0x83, 0xc1, 0x08];
    let sites: Vec<_> = image
        .windows(restore.len())
        .enumerate()
        .filter_map(|(i, w)| (w == restore).then_some(i))
        .collect();
    assert_eq!(sites.len(), 1);
    image[sites[0]..sites[0] + 2].copy_from_slice(&[0x58, 0x90]);
    fs::write(&bin, &image).unwrap();
    let control = Command::new(&bin).args(&args).output().unwrap();
    assert!(
        matches!(control.status.signal(), Some(11) | Some(7)),
        "negative arena control must exhaust its page: {:?}",
        control.status
    );
}

#[test]
fn selfhost_scalar_maps_preserve_values_status_and_bounded_storage() {
    let p = parse(include_str!("../examples/vexprparse.verbose"));
    let base = std::env::temp_dir().join(format!("verbose-scalar-map-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let compiler = base.join("compiler");
    let raw = base.join("raw");
    for (name, path) in [("elf_program_src", &compiler), ("x86_program_src", &raw)] {
        native::compile_native_stdin_raw(&p, name, path.to_str().unwrap()).unwrap();
    }
    assert_scalar_maps(&compiler, &raw, &base);
    fs::remove_dir_all(base).unwrap();
}
