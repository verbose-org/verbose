//! Packed text publication, lexical capabilities and per-element storage reuse.
use crate::{
    ast::{iter_all_concepts, Item},
    interpreter::{self, Value},
    native,
    selfhost_constructor_tests::{install, parse, send},
    selfhost_scalar_map_tests::{assert_run, emitted, eval, expected, program, row, FIELDS},
    verifier,
};
use std::{
    collections::HashMap, fs, os::unix::process::ExitStatusExt, path::Path, process::Command,
};

fn text(body: &str) -> String {
    program(body, "text", FIELDS, "", "", "")
}

pub(crate) fn assert_text_maps(compiler: &Path, raw: &Path, base: &Path) {
    let bin = base.join("text-map");
    let rust = base.join("text-map-rust");
    // Pin the new corpus entry's behavior, not only its acceptance count.
    emitted(compiler, include_str!("../examples/boolean_guards.verbose"), &bin);
    for (s, n, value) in [
        ("", 0, 0), ("a", 0, 1), ("a", 1, 0), ("a", -1, 0),
        ("é🦀", 1, 1), ("é🦀", 5, 1), ("é🦀", 6, 0),
        ("x", i64::MIN, 0), ("x", i64::MAX, 0),
    ] {
        assert_run(&bin, &[s.into(), n.to_string()],
            &(0, format!("{value}\n").into_bytes()), "boolean_guards");
    }
    let mut cases = vec![
        (text("e.name"), true),
        (text("\"\""), true),
        (text("\"é€🦀é\""), true),
        (text("\"aé\\n\\t\\\"\\\\z\""), true),
        (text("if e.n >= 0 then e.name else \"negative\""), false),
        (text("if not (e.name == \"é🦀\") then \"other\" else e.name"), false),
        (text("substring(e.name, 0, length(e.name))"), false),
        (text("substring(if e.n > 0 then e.name else \"\", 0, 0)"), false),
        (text("if length(e.name) == 0 or byte_at(e.name, 0) > 0 then e.name else \"other\""), false),
        (text("if length(e.name) > 0 and byte_at(e.name, 0) > 0 then e.name else \"empty\""), false),
        (text("if e.n > 0 and (length(e.name) == 0 or byte_at(e.name, 0) > 0) then e.name else \"other\""), false),
        (text("substring(\"hello\", max(0, min(e.n, 5)), 5)"), false),
        (text("if e.n >= 0 then substring(e.name, 0, length(e.name)) else substring(\"negative\", 0, 3)"), false),
        (program("if e.n >= 0 then captured else later", "text", FIELDS,
            "    let later = \"first\"\n    let captured = later\n    let later = \"last\"\n    let e = 123\n", "", ""), false),
        (program("saved", "text", FIELDS,
            "    let value = if 1 == 1 then \"kept\" else \"unused\"\n    let saved = substring(value, 0, 4)\n    let value = \"later\"\n", "", ""), false),
    ];
    cases.push((
        text("if e.n > i.limit then e.name else \"small\"")
            .replace(
                "    items : collection(Row)",
                "    limit : number\n    items : collection(Row)",
            )
            .replace("reads: [i.items]", "reads: [i.items, i.limit]"),
        false,
    ));
    for (src, rust_oracle) in cases {
        let p = parse(&src);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{errors:?}\n{src}");
        emitted(compiler, &src, &bin);
        let r = send(raw, &src, 0, false);
        assert_eq!((r.status.code(), r.stderr), (Some(0), vec![]), "{src}");
        assert!(!r.stdout.is_empty());
        if rust_oracle {
            native::compile_native(&p, "probe", rust.to_str().unwrap(), false, false).unwrap();
        }
        for rows in [
            vec![],
            vec![("é🦀", -1), ("", 0), ("last", 5)],
            vec![("first", i64::MIN), ("é🦀", i64::MAX)],
            vec![("line\nnext", 1), ("\t", 2)],
        ] {
            let limit = src.contains("limit : number").then_some(1);
            let mut args: Vec<String> = limit.into_iter().map(|v| v.to_string()).collect();
            args.push(rows.len().to_string());
            for (name, n) in &rows {
                args.extend([(*name).into(), n.to_string()]);
            }
            let result = expected(&src, rows.iter().map(|(s, n)| row(s, *n)).collect(), limit);
            assert_run(&bin, &args, &result, &src);
            if rust_oracle {
                assert_run(&rust, &args, &result, &src);
            }
        }
    }
    // Number elements use the same text printer without an element record.
    let src = text("if e >= 0 then \"positive\" else \"negative\"")
        .replace("items : collection(Row)", "items : collection(number)");
    emitted(compiler, &src, &bin);
    for rows in [vec![], vec![i64::MIN, 0, i64::MAX]] {
        let mut args = vec![rows.len().to_string()];
        args.extend(rows.iter().map(i64::to_string));
        let result = expected(&src, rows.into_iter().map(Value::Number).collect(), None);
        assert_run(&bin, &args, &result, "number to text");
    }
    // Native slices are bytes. The Rust interpreter deliberately substitutes U+FFFD
    // for an invalid UTF-8 fragment, so it cannot be the byte-output oracle here.
    let src = text("substring(e.name, 1, 2)");
    emitted(compiler, &src, &bin);
    assert_run(
        &bin,
        &["1".into(), "é🦀".into(), "0".into()],
        &(0, vec![0xa9, b'\n']),
        "partial UTF-8 byte",
    );
    assert_eq!(
        eval(&src, vec![row("é🦀", 0)], None).unwrap(),
        Value::List(vec![Value::Text("�".into())])
    );

    // The legacy source transport is NUL-terminated. Mutate only a unique data
    // literal in an emitted image to prove the printer uses length, not strlen.
    emitted(compiler, &text("\"nul-marker-xyz\""), &bin);
    let mut image = fs::read(&bin).unwrap();
    let needle = b"nul-marker-xyz";
    let offsets: Vec<_> = image
        .windows(needle.len())
        .enumerate()
        .filter_map(|(i, w)| (w == needle).then_some(i))
        .collect();
    assert_eq!(offsets.len(), 1);
    image[offsets[0] + 3] = 0;
    fs::write(&bin, &image).unwrap();
    assert_run(
        &bin,
        &["2".into(), "a".into(), "1".into(), "b".into(), "2".into()],
        &(0, b"nul\0marker-xyz\nnul\0marker-xyz\n".to_vec()),
        "counted NUL publication",
    );

    let src = text("e.name");
    emitted(compiler, &src, &bin);
    for args in [
        vec![],
        vec!["-1"],
        vec!["9223372036854775807"],
        vec!["-9223372036854775808"],
        vec!["2", "a", "1"],
        vec!["1", "a"],
    ] {
        assert_run(
            &bin,
            &args.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
            &(1, vec![]),
            "invalid count/tail",
        );
    }
    for len in [65535, 65536, 65537] {
        let mut output = "x".repeat(len).into_bytes();
        output.push(b'\n');
        let result = if len <= 65536 {
            (0, output)
        } else {
            (1, vec![])
        };
        assert_run(
            &bin,
            &["1".into(), "x".repeat(len), "1".into()],
            &result,
            "text copy bound",
        );
    }
    assert_run(
        &bin,
        &[
            "2".into(),
            "kept".into(),
            "1".into(),
            "x".repeat(65537),
            "2".into(),
        ],
        &(1, b"kept\n".to_vec()),
        "late copy failure",
    );
    // The last fixed text slot may fill completely; subsequent elements overwrite it.
    let fields = (0..16)
        .map(|n| format!("    text{n} : text"))
        .collect::<Vec<_>>()
        .join("\n");
    let src = program("e.text15", "text", &fields, "", "", "");
    emitted(compiler, &src, &bin);
    let mut args = vec!["2".into()];
    args.extend((0..16).map(|n| {
        if n == 15 {
            "z".repeat(65536)
        } else {
            "é".into()
        }
    }));
    args.extend((0..16).map(|n| if n == 15 { "last".into() } else { "x".into() }));
    assert_run(
        &bin,
        &args,
        &(0, format!("{}\nlast\n", "z".repeat(65536)).into_bytes()),
        "last text slot",
    );

    let src = text("substring(e.name, 0, e.n)");
    emitted(compiler, &src, &bin);
    for n in [-1, 2, i64::MIN, i64::MAX] {
        assert!(eval(&src, vec![row("a", 1), row("b", n)], None).is_err());
        assert_run(
            &bin,
            &[
                "2".into(),
                "a".into(),
                "1".into(),
                "b".into(),
                n.to_string(),
            ],
            &(1, b"a\n".to_vec()),
            "late bounds failure",
        );
    }
    let src = program(
        "e.name",
        "text",
        FIELDS,
        "    let unused = byte_at(\"\", 0)\n",
        "",
        "",
    );
    emitted(compiler, &src, &bin);
    assert!(eval(&src, vec![], None).is_err());
    assert_run(
        &bin,
        &["0".into()],
        &(1, vec![]),
        "eager failing unused let",
    );

    let mut refused = vec![
        text("concat(e.name, \"!\")"),
        text("substring(concat(e.name, \"!\"), 0, 1)"),
        text("if e.n > 0 then e.name else concat(\"bad\", \"!\")"),
        text("if length(concat(\"a\", \"b\")) > 0 then e.name else \"\""),
        text("if (if e.n > 0 then e.name else \"x\") == \"x\" then e.name else \"\""),
        text("if sum(i.items, x => x.n) > 0 then e.name else \"\""),
        text("missing"),
        text("e.n"),
        program(
            "alias",
            "text",
            FIELDS,
            "    let s = concat(\"a\", \"b\")\n    let alias = s\n    let s = \"c\"\n",
            "",
            "",
        ),
        program(
            "e.name",
            "text",
            FIELDS,
            "    let unused = concat(\"a\", \"b\")\n",
            "",
            "",
        ),
        program(
            "if alias == \"a\" then e.name else \"\"",
            "text",
            FIELDS,
            "    let s = if 1 == 1 then \"a\" else \"b\"\n    let alias = s\n",
            "",
            "",
        ),
        program(
            "e.name",
            "text",
            FIELDS,
            "    let unknown = missing\n",
            "",
            "",
        ),
        program(
            "e.name",
            "text",
            FIELDS,
            "    let pair = Pair { first: 1, second: 2 }\n",
            "",
            "",
        ),
    ];
    let call = r#"
rule label
  @intention: "Text call has no packed value lowering"
  @source: invoices.intent:1
  input:
    p : Row
  output:
    out : text
  logic:
    out = p.name
  proofs:
    purity:
      reads: [p.name]
      calls: []
    termination:
      bound: 32
"#;
    refused.push(program("label(e)", "text", FIELDS, "", call, "label"));
    refused.push(program(
        "substring(label(e), 0, 0)",
        "text",
        FIELDS,
        "",
        call,
        "label",
    ));
    refused.push(program(
        "if length(label(e)) > 0 then e.name else \"\"",
        "text",
        FIELDS,
        "",
        call,
        "label",
    ));
    refused.push(program(
        "e.text16",
        "text",
        &(0..17)
            .map(|n| format!("    text{n} : text"))
            .collect::<Vec<_>>()
            .join("\n"),
        "",
        "",
        "",
    ));
    for src in refused {
        for backend in [compiler, raw] {
            let r = send(backend, &src, 0, false);
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(1), vec![], vec![]),
                "{src}"
            );
        }
    }
    // All original rules are checked regardless of entry selection.
    let good = text("e.name");
    let selected =
        good[good.find("rule probe\n").unwrap()..].replace("rule probe", "rule selected");
    let bad = text("concat(e.name, \"!\")");
    for src in [
        format!("{bad}\n{selected}"),
        format!(
            "{}{}\n{}",
            &good[..good.find("rule probe\n").unwrap()],
            selected,
            &bad[bad.find("rule probe\n").unwrap()..]
        ),
    ] {
        for backend in [compiler, raw] {
            for entry in [0, 1] {
                let r = send(backend, &src, entry, false);
                assert_eq!(
                    (r.status.code(), r.stdout, r.stderr),
                    (Some(1), vec![], vec![])
                );
            }
        }
    }
    assert_payroll(compiler, &bin, &rust);

    // An element record still uses an arena node. Thousands of publications must
    // work within one page, and fail if the per-element restore is disabled.
    emitted(compiler, &text("e.name"), &bin);
    let mut image = fs::read(&bin).unwrap();
    let mmap = [0x48, 0xbe, 0, 0, 0, 0, 6, 0, 0, 0];
    let offsets: Vec<_> = image
        .windows(mmap.len())
        .enumerate()
        .filter_map(|(i, w)| (w == mmap).then_some(i))
        .collect();
    assert_eq!(offsets.len(), 1);
    image[offsets[0] + 2..offsets[0] + 10].copy_from_slice(&4096u64.to_le_bytes());
    fs::write(&bin, &image).unwrap();
    let mut args = vec!["4000".into()];
    for n in 0..4000 {
        args.extend([format!("é{n}"), n.to_string()]);
    }
    let output = (0..4000)
        .map(|n| format!("é{n}\n"))
        .collect::<String>()
        .into_bytes();
    assert_run(&bin, &args, &(0, output), "one-page text element reuse");
    let restore = [0x41, 0x5e, 0x49, 0x83, 0xc1, 0x08];
    let offsets: Vec<_> = image
        .windows(restore.len())
        .enumerate()
        .filter_map(|(i, w)| (w == restore).then_some(i))
        .collect();
    assert_eq!(offsets.len(), 1);
    image[offsets[0]..offsets[0] + 2].copy_from_slice(&[0x58, 0x90]);
    fs::write(&bin, &image).unwrap();
    let control = Command::new(&bin).args(&args).output().unwrap();
    assert!(
        matches!(control.status.signal(), Some(11) | Some(7)),
        "{:?}",
        control.status
    );
}

fn assert_payroll(compiler: &Path, bin: &Path, rust: &Path) {
    let src = include_str!("../examples/payroll.verbose");
    let p = parse(src);
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    for (entry, rule) in rules.iter().enumerate() {
        let a = send(compiler, src, entry, false);
        let b = send(compiler, src, entry, false);
        assert_eq!(a.stdout, b.stdout, "payroll reproducibility");
        install(a, bin);
        native::compile_native(&p, &rule.name, rust.to_str().unwrap(), false, false).unwrap();
        for rows in [
            vec![],
            vec![("Alice", 60000), ("Bob", 45000), ("Carol", 90000)],
        ] {
            let mut args = vec![rows.len().to_string()];
            for (name, n) in &rows {
                args.extend([(*name).into(), n.to_string()]);
            }
            let input = HashMap::from([(
                "employees".into(),
                Value::List(
                    rows.iter()
                        .map(|(s, n)| {
                            Value::Record(HashMap::from([
                                ("name".into(), Value::Text((*s).into())),
                                ("salary".into(), Value::Number(*n)),
                            ]))
                        })
                        .collect(),
                ),
            )]);
            let value = interpreter::eval_rule(rule, &rules, &concepts, &[], &input).unwrap();
            let r = Command::new(rust).args(&args).output().unwrap();
            assert_eq!((r.status.code(), r.stderr), (Some(0), vec![]));
            assert_run(bin, &args, &(0, r.stdout.clone()), &rule.name);
            // JSON record layout is compared to Rust native; scalar lists also
            // have an independent original-AST value/formatting oracle.
            if entry >= 2 {
                let expected = if let Value::List(items) = value {
                    items.iter().map(|v| format!("{v}\n")).collect::<String>()
                } else {
                    format!("{value}\n")
                };
                assert_eq!(r.stdout, expected.into_bytes());
            }
        }
    }
}

#[test]
fn selfhost_text_maps_publish_live_spans_and_refuse_unknown_lowering() {
    let p = parse(include_str!("../examples/vexprparse.verbose"));
    let base = std::env::temp_dir().join(format!("verbose-text-map-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let compiler = base.join("compiler");
    let raw = base.join("raw");
    for (name, path) in [("elf_program_src", &compiler), ("x86_program_src", &raw)] {
        native::compile_native_stdin_raw(&p, name, path.to_str().unwrap()).unwrap();
    }
    assert_text_maps(&compiler, &raw, &base);
    fs::remove_dir_all(base).unwrap();
}
