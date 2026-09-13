use super::*;
use crate::{interpreter, lexer::Lexer, parser::Parser};
use std::{fs, path::Path, process::Command};

const SOURCE: &str = include_str!("../../examples/bounded_text.verbose");
fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn fixture() -> Program {
    parse(SOURCE)
}
fn rule<'a>(p: &'a mut Program, name: &str) -> &'a mut Rule {
    p.items
        .iter_mut()
        .find_map(|i| match i {
            Item::Rule(r) if r.name == name => Some(r),
            _ => None,
        })
        .unwrap()
}
fn expression(s: &str) -> Expr {
    let source = SOURCE.replace("concat(\"[\", item.title, \"]\", item.code)", s);
    rule(&mut parse(&source), "label").logic.value.clone()
}
fn rejects(p: &Program, message: &str) {
    let errors = verify(p);
    assert!(
        errors.iter().any(|e| e.message.contains(message)),
        "expected {message}: {errors:?}"
    );
}
fn eval(
    p: &Program,
    name: &str,
    title: &str,
    code: i64,
) -> Result<interpreter::Value, interpreter::RuntimeError> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let r = rules.iter().find(|r| r.name == name).unwrap();
    interpreter::eval_rule(
        r,
        &rules,
        &concepts,
        &[],
        &HashMap::from([
            ("title".into(), interpreter::Value::Text(title.into())),
            ("code".into(), interpreter::Value::Number(code)),
        ]),
    )
}
fn differential(p: &Program, name: &str) {
    assert!(verify(p).is_empty(), "{:?}", verify(p));
    let bin = format!("/tmp/verbose-text-bounds-{}", std::process::id());
    crate::native::compile_native(p, name, &bin, false, false).unwrap();
    let first = fs::read(&bin).unwrap();
    crate::native::compile_native(p, name, &bin, false, false).unwrap();
    assert_eq!(
        first,
        fs::read(&bin).unwrap(),
        "non-deterministic storage emission"
    );
    for title in ["", "hello", "world", "abcdefgh", "éééé"] {
        for code in [i64::MIN, -1, 0, 1, i64::MAX] {
            let reference = eval(p, name, title, code).unwrap();
            let output = Command::new(&bin)
                .args([title, &code.to_string()])
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(if matches!(reference, interpreter::Value::Bool(false)) {
                    1
                } else {
                    0
                }),
                "{title} {code}: {output:?}"
            );
            assert!(output.stderr.is_empty());
            assert_eq!(
                output.stdout,
                format!("{reference}\n").as_bytes(),
                "{title} {code}"
            );
        }
    }
    let invalid = Command::new(&bin).args(["ééééé", "1"]).output().unwrap();
    assert_eq!(invalid.status.code(), Some(1));
    assert!(invalid.stdout.is_empty());
    assert!(eval(p, name, "ééééé", 1).is_err());
    fs::remove_file(bin).unwrap();
}

#[test]
fn text_bounds_composition_and_utf8() {
    let p = fixture();
    assert!(crate::verifier::verify_program(&p, Path::new("examples")).is_empty());
    differential(&p, "decorated_label");
    let (optimized, _) = crate::optimizer::optimize_program(&p);
    differential(&optimized, "decorated_label");
    assert_eq!(
        eval(&p, "decorated_label", "abcdefgh", i64::MIN)
            .unwrap()
            .to_string()
            .len(),
        32
    );
}

#[test]
fn text_bounds_lexical_aliases_and_branches() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.logic.bindings = vec![
        ("first".into(), expression("item.title")),
        ("alias".into(), Expr::Ident("first".into())),
        ("first".into(), Expr::Text("rebound".into())),
    ];
    r.logic.value = expression("if item.code > 0 then concat(alias, first) else alias");
    let errors = crate::verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    differential(&p, "decorated_label");
    let r = rule(&mut p, "label");
    r.logic
        .bindings
        .push(("item".into(), Expr::Text("shadowed".into())));
    r.logic.value = Expr::Ident("item".into());
    differential(&p, "decorated_label");
}

#[test]
fn text_bounds_calls_twice_and_unannotated_caller() {
    let mut p = fixture();
    let r = rule(&mut p, "decorated_label");
    r.output_text_max = None;
    r.logic.bindings.push((
        "second".into(),
        Expr::Call("label".into(), vec![Expr::Ident("request".into())]),
    ));
    r.logic.value = Expr::Concat(vec![
        Expr::Ident("alias".into()),
        Expr::Ident("second".into()),
    ]);
    differential(&p, "decorated_label");
    assert!(
        crate::wasm::compile_wasm(&p, "decorated_label", "/tmp/verbose-text-unused-wasm")
            .unwrap_err()
            .message
            .contains("bounded text")
    );
    let r = rule(&mut p, "decorated_label");
    r.logic.bindings.clear();
    r.logic.value = expression("if request.code == 0 then \"bypass\" else label(request)");
    differential(&p, "decorated_label");
    assert!(eval(&p, "decorated_label", "too long for the declared input", 0).is_err());
}

#[test]
fn text_bounds_call_dag_expansion_limit() {
    let mut p = fixture();
    let mut prior = "label".to_string();
    rule(&mut p, "label").logic.value = Expr::Text(String::new());
    rule(&mut p, "label").output_text_max = Some(0);
    let template = rule(&mut p, "label").clone();
    for i in 0..20 {
        let mut r = template.clone();
        r.name = format!("double_{i}");
        let call = Expr::Call(prior.clone(), vec![Expr::Ident(r.input_name.clone())]);
        r.logic.value = Expr::Concat(vec![call.clone(), call]);
        prior = r.name.clone();
        p.items.push(Item::Rule(r));
    }
    assert!(verify(&p).is_empty());
    let err =
        crate::native::compile_native(&p, &prior, "/tmp/verbose-storage-rejected", false, false)
            .unwrap_err();
    assert!(
        err.message.contains("invocation frame") || err.message.contains("call expansion limit"),
        "{err}"
    );
}

#[test]
fn text_bounds_literal_aliases_do_not_expand() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.output_text_max = Some(1_048_576);
    r.logic.bindings = vec![("x0".into(), Expr::Text("x".repeat(1_048_576)))];
    for i in 1..18 {
        r.logic
            .bindings
            .push((format!("x{i}"), Expr::Ident(format!("x{}", i - 1))));
    }
    r.logic.value = Expr::Ident("x17".into());
    let r = rule(&mut p, "decorated_label");
    r.output_text_max = None;
    r.logic.value = Expr::Ident("alias".into());
    assert!(verify(&p).is_empty());
    let bin = format!("/tmp/verbose-storage-literal-{}", std::process::id());
    crate::native::compile_native(&p, "label", &bin, false, false).unwrap();
    let output = Command::new(&bin).args(["", "0"]).output().unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        format!("{}\n", "x".repeat(1_048_576)).as_bytes()
    );
    assert!(fs::metadata(&bin).unwrap().len() < 1_060_000);
    fs::remove_file(bin).unwrap();
}

#[test]
fn text_bounds_stdin_channels_enforce_inputs() {
    use std::io::Write;
    use std::process::Stdio;
    let mut p = fixture();
    if let Item::Concept(c) = &mut p.items[0] {
        c.fields.pop();
    }
    let r = rule(&mut p, "label");
    r.logic.value = expression("concat(\"[\", item.title, \"]\")");
    for mode in ["stdin", "raw", "stream"] {
        let bin = format!("/tmp/verbose-text-input-{}-{mode}", std::process::id());
        if mode == "raw" {
            crate::native::compile_native_stdin_raw(&p, "decorated_label", &bin).unwrap();
        } else {
            crate::native::compile_native(
                &p,
                "decorated_label",
                &bin,
                mode == "stdin",
                mode == "stream",
            )
            .unwrap();
        }
        for (input, success) in [("éééé", true), ("ééééé", false)] {
            let mut child = Command::new(&bin)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(
                    if mode == "raw" {
                        input.as_bytes().to_vec()
                    } else {
                        format!("{input}\n").into_bytes()
                    }
                    .as_slice(),
                )
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(if success { 0 } else { 1 }),
                "{mode}: {output:?}"
            );
            assert_eq!(
                output.stdout,
                if success {
                    format!("<[{input}]>\n").into_bytes()
                } else {
                    vec![]
                },
                "{mode}"
            );
            assert!(output.stderr.is_empty());
        }
        fs::remove_file(bin).unwrap();
    }
}

#[test]
fn text_bounds_empty_nul_and_unknown() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.output_text_max = Some(0);
    r.logic.value = Expr::Text(String::new());
    differential(&p, "decorated_label");
    let r = rule(&mut p, "label");
    r.output_text_max = Some(3);
    r.logic.value = Expr::Text("\0é".into());
    differential(&p, "decorated_label");
    rule(&mut p, "label").output_text_max = Some(2);
    rejects(&p, "up to 3 bytes");
    p = fixture();
    if let Item::Concept(c) = &mut p.items[0] {
        c.fields[0].range = None;
    }
    rejects(&p, "capacity is unknown");
    rule(&mut p, "label").logic.value = expression("if item.code == 0 then \"ok\" else item.title");
    rejects(&p, "capacity is unknown");
    p = fixture();
    rule(&mut p, "label").logic.value = expression("concat(item.title, -1)");
    differential(&p, "decorated_label");
}

#[test]
fn text_bounds_refusals_are_explicit() {
    let cases = [
        (
            "concat(item.title, item.title, item.code)",
            "up to 36 bytes",
        ),
        ("concat(item.code > 0)", "concat requires"),
        ("if item.code then \"ok\" else \"bad\"", "expected Bool"),
        ("if item.code == 0 then \"ok\" else 1", "expected Number"),
        ("missing", "unknown binding"),
        ("concat(now_unix())", "effect"),
        ("substring(item.title, 0, 1)", "substring"),
        ("json_escape(item.title)", "json_escape"),
        ("concat(item.code + 1)", "numeric arithmetic"),
        ("label(item)", "recursion"),
        (
            "label(LabelInput { title: \"too long for input\", code: 1 })",
            "original input",
        ),
        ("try_byte_at(b\"x\", item.code)", "Result"),
    ];
    for (body, diagnostic) in cases {
        let mut p = fixture();
        rule(&mut p, "label").logic.value = expression(body);
        rejects(&p, diagnostic);
    }
    let mut p = fixture();
    rule(&mut p, "label").output_text_max = Some(31);
    rejects(&p, "up to 33 bytes"); // A caller uses the declared contract.
    p = fixture();
    rule(&mut p, "decorated_label").logic.bindings.insert(
        0,
        (
            "request".into(),
            expression("LabelInput { title: \"longlonglong\", code: 1 }"),
        ),
    );
    rejects(&p, "original input");
}

#[test]
fn text_bounds_analysis_and_expansion_limits() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.logic.bindings = vec![("x".into(), Expr::Text("x".into()))];
    for _ in 0..64 {
        r.logic.bindings.push((
            "x".into(),
            Expr::Concat(vec![Expr::Ident("x".into()), Expr::Ident("x".into())]),
        ));
    }
    r.logic.value = Expr::Ident("x".into());
    rejects(&p, "capacity arithmetic overflow");
    // Empty doubling remains within capacity and must now compile linearly:
    // each let reads the previous value rather than expanding its expression.
    let r = rule(&mut p, "label");
    r.logic.bindings[0].1 = Expr::Text(String::new());
    assert!(verify(&p).is_empty());
    differential(&p, "decorated_label");
}

#[test]
fn text_bounds_parser_backend_and_legacy_boundaries() {
    for bound in ["[..-1]", "[..1048577]", "[0, 30]"] {
        let source = SOURCE.replace("[..30]", bound);
        assert!(Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .is_err());
    }
    let source = SOURCE.replace("text [..30]", "number [..30]");
    assert!(Parser::new(Lexer::new(&source).tokenize().unwrap())
        .parse_program()
        .is_err());
    let mut p = fixture();
    let output = format!("/tmp/verbose-text-refuse-{}", std::process::id());
    assert!(!Path::new(&output).exists());
    let e = crate::wasm::compile_wasm(&p, "decorated_label", &output).unwrap_err();
    assert!(e.message.contains("bounded text"));
    assert!(!Path::new(&output).exists());
    rule(&mut p, "label").output_text_max = Some(1);
    assert!(
        crate::native::compile_native(&p, "decorated_label", &output, false, false)
            .unwrap_err()
            .message
            .contains("exceeds")
    );
    assert!(!Path::new(&output).exists());
    for i in &mut p.items {
        if let Item::Rule(r) = i {
            r.output_text_max = None;
        }
    }
    assert!(verify(&p).is_empty());
    assert!(active_rules(&p).is_empty());
}

#[test]
fn text_bounds_self_hosted_refuses_before_artifact() {
    use std::io::Write;
    use std::process::Stdio;
    let p = parse(include_str!("../../examples/vexprparse.verbose"));
    for entry in ["elf_program_src", "x86_program_src"] {
        let bin = format!("/tmp/verbose-text-self-{}-{entry}", std::process::id());
        crate::native::compile_native_stdin_raw(&p, entry, &bin).unwrap();
        for source in [
            SOURCE.to_string(),
            SOURCE.replace("[..30]", "[..0]"),
            include_str!("../../examples/http_bounded_text.verbose").to_string(),
        ] {
            let mut child = Command::new(&bin)
                .arg("0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(1), "{entry}: {output:?}");
            assert!(
                output.stdout.is_empty(),
                "{entry}: unsupported annotation emitted an artifact"
            );
        }
        // Existing bounded concept fields, comments and string contents do not
        // activate an output annotation. Two rules test the section reset.
        let source = SOURCE
            .replace("text [..30]", "text")
            .replace("text [..32]", "text")
            .replace(
                "let alias = message",
                "let alias = message\n    let note = \"output: text [..1]\"",
            );
        let mut child = Command::new(&bin)
            .arg("0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{entry}: control {output:?}");
        assert!(!output.stdout.is_empty());
        fs::remove_file(bin).unwrap();
    }
}

#[test]
fn text_storage_nested_branches_and_short_circuit() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.output_text_max = Some(64);
    r.logic.bindings = vec![
        ("a".into(), expression("if item.code > 0 then concat(\"yes:\", item.title) else concat(\"no:\", item.title)")),
        ("b".into(), expression("concat(\"[\", if length(a) > 5 then concat(a, \"!\") else concat(\"?\", a), \"]\")")),
        ("a".into(), expression("concat(b, if item.title == \"\" then \"empty\" else item.title)")),
    ];
    r.logic.value = expression("concat(a, if (item.code > 0 and item.title != \"\") or not (length(b) > 4) then b else concat(\"<\", b, \">\"))");
    rule(&mut p, "decorated_label").output_text_max = Some(66);
    differential(&p, "decorated_label");
    // Scalars consume already materialized text, including a call on an
    // untaken RHS. Boolean entry status must still accumulate per record.
    let r = rule(&mut p, "decorated_label");
    r.output_text_max = None;
    r.output_ty = Type::Bool;
    r.logic.value =
        expression("(request.code > 0 and length(label(request)) > 5) or (alias == message)");
    differential(&p, "decorated_label");
    rule(&mut p, "decorated_label").logic.value =
        expression("request.code > 0 and (alias != message or request.title == \"hello\")");
    differential(&p, "decorated_label");
    let r = rule(&mut p, "decorated_label");
    r.output_ty = Type::Number;
    r.logic.value = expression("if alias == message then length(label(request)) else request.code");
    differential(&p, "decorated_label");
}

#[test]
fn text_storage_record_fields_preserve_lexical_values() {
    let mut p = fixture();
    let r = rule(&mut p, "decorated_label");
    r.output_text_max = None;
    r.output_ty = Type::Named("LabelInput".into());
    r.logic.bindings.extend([
        (
            "record".into(),
            expression("LabelInput { title: alias, code: length(message) }"),
        ),
        ("message".into(), Expr::Text("shadow".into())),
    ]);
    r.logic.value = expression("LabelInput { code: record.code, title: record.title }");
    assert!(verify(&p).is_empty());
    let bin = format!("/tmp/verbose-storage-record-{}", std::process::id());
    crate::native::compile_native(&p, "decorated_label", &bin, false, false).unwrap();
    let result = Command::new(&bin).args(["é", "42"]).output().unwrap();
    assert!(result.status.success());
    assert!(result.stderr.is_empty());
    assert_eq!(
        result.stdout,
        "{\"title\":\"[é]42\",\"code\":6}\n".as_bytes()
    );
    let interpreter::Value::Record(fields) = eval(&p, "decorated_label", "é", 42).unwrap() else {
        panic!("expected record");
    };
    assert_eq!(fields["title"].to_string(), "[é]42");
    assert_eq!(fields["code"].to_string(), "6");
    rule(&mut p, "decorated_label").logic.value = Expr::Ident("request".into());
    crate::native::compile_native(&p, "decorated_label", &bin, false, false).unwrap();
    let result = Command::new(&bin).args(["hello", "-42"]).output().unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout, b"{\"title\":\"hello\",\"code\":-42}\n");
    fs::remove_file(bin).unwrap();
}

#[test]
fn text_storage_limits_include_unused_work_and_preserve_artifacts() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.logic.value = Expr::Text("ok".into());
    r.logic.bindings = (0..12)
        .map(|i| {
            (
                format!("unused_{i}"),
                Expr::Concat(vec![
                    Expr::Text("x".repeat(200_000)),
                    expression("item.title"),
                ]),
            )
        })
        .collect();
    assert!(verify(&p).is_empty());
    let bin = format!("/tmp/verbose-storage-limit-{}", std::process::id());
    assert!(!Path::new(&bin).exists());
    let err = crate::native::compile_native(&p, "label", &bin, false, false).unwrap_err();
    assert!(
        err.message
            .contains("invocation frame exceeds 2097152 bytes"),
        "{err}"
    );
    assert!(!Path::new(&bin).exists());
    fs::write(&bin, b"existing artifact").unwrap();
    assert!(crate::native::compile_native(&p, "label", &bin, false, false).is_err());
    assert_eq!(fs::read(&bin).unwrap(), b"existing artifact");
    fs::remove_file(&bin).unwrap();
    // Unused literals don't need writable buffers, but their code/data copies
    // still have a separate compiler budget. Aliases don't spend that budget.
    rule(&mut p, "label").logic.bindings = (0..17)
        .map(|i| (format!("literal_{i}"), Expr::Text("x".repeat(1_048_576))))
        .collect();
    assert!(verify(&p).is_empty());
    let err = crate::native::compile_native(&p, "label", &bin, false, false).unwrap_err();
    assert!(
        err.message.contains("literal expansion exceeds 16 MiB"),
        "{err}"
    );
    assert!(!Path::new(&bin).exists());
}

#[test]
fn text_storage_reclaims_each_argv_record_and_stream_line() {
    use std::process::Stdio;
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.logic.bindings = vec![(
        "eager".into(),
        Expr::Concat(vec![
            Expr::Text("x".repeat(16_384)),
            expression("item.title"),
        ]),
    )];
    r.logic.value = expression("concat(item.title, item.code)");
    let bin = format!("/tmp/verbose-storage-reuse-{}", std::process::id());
    let input_file = format!("{bin}.input");
    let count = 600;
    let expected = "<é42>\n".repeat(count);
    for stream in [false, true] {
        crate::native::compile_native(&p, "decorated_label", &bin, false, stream).unwrap();
        let mut command = Command::new("sh");
        command.args(["-c", "ulimit -s 256; exec \"$@\"", "storage-test", &bin]);
        if stream {
            fs::write(&input_file, "é 42\n".repeat(count)).unwrap();
            command.stdin(Stdio::from(fs::File::open(&input_file).unwrap()));
        } else {
            for _ in 0..count {
                command.args(["é", "42"]);
            }
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "stream={stream}: {output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, expected.as_bytes(), "stream={stream}");
    }
    fs::remove_file(bin).unwrap();
    fs::remove_file(input_file).unwrap();
}

#[test]
fn text_storage_example_matches_interpreter() {
    let p = parse(include_str!("../../examples/bounded_text_storage.verbose"));
    let errors = crate::verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    differential(&p, "reuse_text");
    differential(&p, "choose_text");
}

#[test]
fn text_storage_compares_counted_values_past_nul() {
    let mut p = fixture();
    let r = rule(&mut p, "label");
    r.logic.bindings = vec![(
        "x".into(),
        Expr::If(
            Box::new(expression("item.code > 0")),
            Box::new(Expr::Text("a\0x".into())),
            Box::new(Expr::Text("a\0y".into())),
        ),
    )];
    r.logic.value = Expr::If(
        Box::new(Expr::Binary(
            BinOp::Eq,
            Box::new(Expr::Ident("x".into())),
            Box::new(Expr::Text("a\0x".into())),
        )),
        Box::new(Expr::Text("same".into())),
        Box::new(Expr::Text("different".into())),
    );
    differential(&p, "decorated_label");
    let r = rule(&mut p, "label");
    r.logic.value = expression("if \"\" == \"\" then \"empty match\" else \"wrong\"");
    differential(&p, "decorated_label");
}
