use super::*;
use crate::{lexer::Lexer, native, optimizer, parser::Parser, verifier};
use std::{fs, path::Path, process::Command};

const RULES: &str = include_str!("../../../examples/sequential_stack.verbose");
const DECLARATION: &str = include_str!("../../../examples/execution_stack.verbose");

fn program(rules: &str, phases: &str) -> Program {
    let declaration = DECLARATION[DECLARATION.find("execution inspect_readings").unwrap()..]
        .replace("clamp, nonnegative, label", phases)
        .replace("native_stack: 192", "native_stack: 2097152");
    let p = Parser::new(
        Lexer::new(&format!("{rules}\n{declaration}"))
            .tokenize()
            .unwrap(),
    )
    .parse_program()
    .unwrap();
    let errors = verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    p
}

fn records(rows: &[(&str, i64)], number_field: &str) -> Vec<HashMap<String, Value>> {
    rows.iter()
        .map(|(title, value)| {
            HashMap::from([
                ("title".into(), Value::Text((*title).into())),
                (number_field.into(), Value::Number(*value)),
            ])
        })
        .collect()
}

#[test]
fn execution_interpreter_visits_complete_phases_and_stops_after_false() {
    let p = program(RULES, "clamp, nonnegative, label");
    let batch = records(&[("x", 2), ("y", -1), ("z", 1000)], "value");
    let original = batch.clone();
    let mut events = Vec::new();
    assert_eq!(
        run(&p, "inspect_readings", &batch, |phase, rule, row, value| {
            events.push((phase, rule.name.clone(), row, value));
            Ok(())
        })
        .unwrap(),
        1
    );
    assert_eq!(batch, original);
    assert_eq!(
        events
            .iter()
            .map(|e| (e.0, e.2, e.3.clone()))
            .collect::<Vec<_>>(),
        vec![
            (1, 0, Value::Number(2)),
            (1, 1, Value::Number(-1)),
            (1, 2, Value::Number(100)),
            (2, 0, Value::Bool(true)),
            (2, 1, Value::Bool(false)),
            (2, 2, Value::Bool(true)),
        ]
    );
    assert!(events[3..].iter().all(|e| e.1 == "nonnegative"));
    let p = program(RULES, "clamp, label");
    let mut out = Vec::new();
    assert_eq!(
        write(&p, "inspect_readings", &batch, false, &mut out).unwrap(),
        0
    );
    assert_eq!(out, b"2\n-1\n100\nx:2\ny:-1\nz:1000\n");
}

#[test]
fn execution_interpreter_matches_optimized_native_values_output_and_status() {
    let dir = std::env::temp_dir().join(format!(
        "verbose-execution-reference-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("run");
    for phases in [
        "clamp, label",
        "clamp, nonnegative, label",
        "nonnegative, clamp",
        "label, nonnegative",
        "label, clamp, label",
    ] {
        let p = program(RULES, phases);
        let optimized = optimizer::optimize_program(&p).0;
        native::compile_native(
            &optimized,
            "inspect_readings",
            binary.to_str().unwrap(),
            false,
            false,
        )
        .unwrap();
        for rows in [
            vec![("a", 2), ("b", 1000)],
            vec![("éééé", -1), ("x", 2)],
            vec![("", i64::MIN), ("🚀", i64::MAX)],
            vec![("a,{\"}\\\n", 0), ("x\t", -100)],
        ] {
            let mut stdout = Vec::new();
            let status = write(
                &p,
                "inspect_readings",
                &records(&rows, "value"),
                false,
                &mut stdout,
            )
            .unwrap();
            let args: Vec<_> = rows
                .iter()
                .flat_map(|(s, n)| [s.to_string(), n.to_string()])
                .collect();
            let out = Command::new(&binary).args(args).output().unwrap();
            assert_eq!(
                (out.status.code(), out.stdout, out.stderr),
                (Some(status), stdout, vec![]),
                "{phases}"
            );
        }
    }
    // Different input binders, expanded calls, aliases, shadowing and flat
    // record outputs retain their original AST semantics as well.
    let p = program(
        include_str!("../../../examples/retained_stack.verbose"),
        "prepare, analyze, prepare",
    );
    native::compile_native(
        &optimizer::optimize_program(&p).0,
        "inspect_readings",
        binary.to_str().unwrap(),
        false,
        false,
    )
    .unwrap();
    let rows = [("café", i64::MIN), ("a\"\n\\", i64::MAX)];
    let mut stdout = Vec::new();
    assert_eq!(
        write(
            &p,
            "inspect_readings",
            &records(&rows, "code"),
            false,
            &mut stdout
        )
        .unwrap(),
        0
    );
    let out = Command::new(&binary)
        .args(
            rows.iter()
                .flat_map(|(s, n)| [s.to_string(), n.to_string()]),
        )
        .output()
        .unwrap();
    assert_eq!(
        (out.status.code(), out.stdout, out.stderr),
        (Some(0), stdout, vec![])
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn execution_interpreter_runtime_and_writer_failures_preserve_only_completed_prefix() {
    let p = program(RULES, "clamp, label");
    for bad in [
        HashMap::new(),
        HashMap::from([
            ("title".into(), Value::Text("too long!".into())),
            ("value".into(), Value::Number(0)),
        ]),
        HashMap::from([
            ("title".into(), Value::Text("x".into())),
            ("value".into(), Value::Text("0".into())),
        ]),
    ] {
        let mut batch = records(&[("x", 2)], "value");
        batch.push(bad);
        batch.extend(records(&[("later", 3)], "value"));
        let mut stdout = Vec::new();
        let e = write(&p, "inspect_readings", &batch, false, &mut stdout).unwrap_err();
        assert!(e.message.contains("phase 1 ('clamp'), record 1"), "{e}");
        assert_eq!(stdout, b"2\n");
        stdout.clear();
        assert!(write(&p, "inspect_readings", &batch, true, &mut stdout).is_err());
        assert_eq!(
            stdout,
            b"[{\"phase\":1,\"rule\":\"clamp\",\"record\":0,\"value\":2}]\n"
        );
    }
    let mut count = 0;
    let e = run(
        &p,
        "inspect_readings",
        &records(&[("x", 2), ("y", 3)], "value"),
        |_, _, _, _| {
            count += 1;
            Err(error("writer failed"))
        },
    )
    .unwrap_err();
    assert_eq!(count, 1);
    assert!(e.message.contains("record 0: writer failed"));
    assert!(run(&p, "inspect_readings", &[], |_, _, _, _| panic!())
        .unwrap_err()
        .message
        .contains("at least one"));
    // Invalid unselected declarations cannot be bypassed through the reference.
    let mut p = p;
    if let Item::Execution(e) = p.items.last_mut().unwrap() {
        e.mode = ExecutionMode::Sequential { native_stack: 1 };
    }
    assert!(run(
        &p,
        "inspect_readings",
        &records(&[("x", 2)], "value"),
        |_, _, _, _| panic!()
    )
    .is_err());
}

#[test]
fn execution_json_reader_handles_escaped_delimiters_unicode_and_i64_without_truncation() {
    let parsed = input::parse(r#" [ {"title":"a,{\"}\\\n\t\b\f\/\r\u0000\u00e9\ud83d\ude80", "value":-9223372036854775808}, {"title":"🚀", "value":9223372036854775807} ] "#).unwrap();
    assert_eq!(
        parsed[0]["title"],
        Value::Text("a,{\"}\\\n\t\u{8}\u{c}/\r\0é🚀".into())
    );
    assert_eq!(parsed[0]["value"], Value::Number(i64::MIN));
    assert_eq!(parsed[1]["value"], Value::Number(i64::MAX));
    for s in [
        "",
        "{}",
        "[",
        "[{},]",
        "[{,}]",
        "[{\"x\":1,}]",
        "[{}]x",
        "[{},",
        "[{\"x\":01}]",
        "[{\"x\":+1}]",
        "[{\"x\":1.0}]",
        "[{\"x\":1e1}]",
        "[{\"x\":9223372036854775808}]",
        "[{\"x\":-9223372036854775809}]",
        "[{\"x\":1,\"x\":2}]",
        "[{\"x\":null}]",
        "[{\"x\":[]}]",
        "[{\"x\":{}}]",
        "[{x:1}]",
        "[{\"x\":\"\n\"}]",
        r#"[{"x":"\q"}]"#,
        r#"[{"x":"\é"}]"#,
        r#"[{"x":"\ud800"}]"#,
        r#"[{"x":"\ud800\u0000"}]"#,
        r#"[{"x":"\udc00"}]"#,
        r#"[{"x":"\u12é0"}]"#,
    ] {
        assert!(input::parse(s).is_err(), "{s:?}");
    }
    // Every UTF-8-safe prefix of a valid input is either complete or refused;
    // malformed string boundaries must never panic or accept trailing garbage.
    let s = r#"[{"title":"café🚀","value":0}]"#;
    for (end, _) in s.char_indices().skip(1) {
        assert!(input::parse(&s[..end]).is_err());
    }
}

#[test]
fn execution_json_output_escapes_all_controls_and_orders_record_fields() {
    let value = Value::Record(HashMap::from([
        ("z".into(), Value::Number(i64::MIN)),
        ("a".into(), Value::Text("\"\\\n\r\t\0é🚀".into())),
    ]));
    let mut out = Vec::new();
    json_value(&mut out, &value).unwrap();
    assert_eq!(
        String::from_utf8(out.clone()).unwrap(),
        "{\"a\":\"\\\"\\\\\\u000a\\u000d\\u0009\\u0000é🚀\",\"z\":-9223372036854775808}"
    );
    let decoded = input::parse(&format!("[{}]", String::from_utf8(out).unwrap())).unwrap();
    assert_eq!(Value::Record(decoded[0].clone()), value);
}

#[test]
fn concurrent_reference_matches_sequential_and_native_across_admission_limits() {
    let dir = std::env::temp_dir().join(format!(
        "verbose-concurrent-reference-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let binary = dir.join("run");
    let concurrent_binary = dir.join("concurrent");
    for phases in [
        "clamp, nonnegative, label".to_string(),
        "label, clamp, label".into(),
        vec!["label"; 64].join(", "),
    ] {
        let sequential = program(RULES, &phases);
        native::compile_native(
            &optimizer::optimize_program(&sequential).0,
            "inspect_readings",
            binary.to_str().unwrap(),
            false,
            false,
        )
        .unwrap();
        for (limit, result_batch) in [(1, 1), (2, 1), (64, 1), (1, 8), (2, 8), (64, 8), (2, 32)] {
            let mut concurrent = sequential.clone();
            if let Item::Execution(e) = concurrent.items.last_mut().unwrap() {
                e.mode = ExecutionMode::Concurrent {
                    result_batch,
                    native_memory: Some(1_000_000),
                    max_in_flight: limit,
                };
            }
            native::compile_native(
                &concurrent,
                "inspect_readings",
                concurrent_binary.to_str().unwrap(),
                false,
                false,
            )
            .unwrap();
            for rows in [
                vec![("café", 2), ("b", 1000)],
                vec![("", i64::MIN), ("🚀", i64::MAX)],
                vec![("a,{\"}\\\n", -1), ("next", 1)],
            ] {
                let data = records(&rows, "value");
                let out = Command::new(&binary)
                    .args(
                        rows.iter()
                            .flat_map(|(s, n)| [s.to_string(), n.to_string()]),
                    )
                    .output()
                    .unwrap();
                let mut raw = Vec::new();
                let status =
                    write(&concurrent, "inspect_readings", &data, false, &mut raw).unwrap();
                assert_eq!(
                    (out.status.code(), out.stdout, out.stderr),
                    (Some(status), raw, vec![])
                );
                let native_concurrent = Command::new("timeout")
                    .arg("10s")
                    .arg(&concurrent_binary)
                    .args(
                        rows.iter()
                            .flat_map(|(s, n)| [s.to_string(), n.to_string()]),
                    )
                    .output()
                    .unwrap();
                let mut expected = Vec::new();
                let status =
                    write(&concurrent, "inspect_readings", &data, false, &mut expected).unwrap();
                assert_eq!(
                    (
                        native_concurrent.status.code(),
                        native_concurrent.stdout,
                        native_concurrent.stderr
                    ),
                    (Some(status), expected, vec![])
                );
                let (mut a, mut b) = (Vec::new(), Vec::new());
                assert_eq!(
                    write(&sequential, "inspect_readings", &data, true, &mut a).unwrap(),
                    write(&concurrent, "inspect_readings", &data, true, &mut b).unwrap()
                );
                assert_eq!(a, b);
            }
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn concurrent_records_and_runtime_errors_keep_the_sequential_prefix() {
    let sequential = program(
        include_str!("../../../examples/retained_stack.verbose"),
        "prepare, analyze, prepare",
    );
    let mut concurrent = sequential.clone();
    if let Item::Execution(e) = concurrent.items.last_mut().unwrap() {
        e.mode = ExecutionMode::Concurrent {
            result_batch: 8,
            max_in_flight: 2,
            native_memory: None,
        };
    }
    for data in [
        records(&[("café", i64::MIN), ("a\"\n\\", i64::MAX)], "code"),
        records(&[("ok", 0), ("too long!", 1), ("later", 2)], "code"),
        vec![],
    ] {
        for json in [false, true] {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let ra =
                write(&sequential, "inspect_readings", &data, json, &mut a).map_err(|e| e.message);
            let rb =
                write(&concurrent, "inspect_readings", &data, json, &mut b).map_err(|e| e.message);
            assert_eq!(ra, rb);
            assert_eq!(a, b);
        }
    }
}
