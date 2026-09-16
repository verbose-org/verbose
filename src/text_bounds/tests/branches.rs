use super::*;

const SOURCE: &str = include_str!("../../../examples/bounded_text_branches.verbose");

fn fixture() -> Program {
    parse(SOURCE)
}

fn root(p: &mut Program, bindings: &[(&str, &str)], body: &str) {
    let r = rule(p, "compose_text");
    r.logic.bindings = bindings
        .iter()
        .map(|(n, e)| ((*n).into(), expression(e)))
        .collect();
    r.logic.value = expression(body);
}

fn refusal(p: &Program, message: &str) {
    rejects(p, message);
    let file = format!("/tmp/verbose-text-branch-refusal-{}", std::process::id());
    fs::write(&file, b"preserve").unwrap();
    let error = crate::native::compile_native(p, "compose_text", &file, false, false).unwrap_err();
    assert!(error.message.contains(message), "{error}");
    assert_eq!(fs::read(&file).unwrap(), b"preserve");
    fs::remove_file(&file).unwrap();
    assert!(crate::native::compile_native(p, "compose_text", &file, false, false).is_err());
    assert!(!Path::new(&file).exists());
}

#[test]
fn text_branches_records_calls_field_order_and_shadowing() {
    let mut p = fixture();
    let errors = crate::verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    differential(&p, "compose_text");
    // A record entry retains native JSON output (the interpreter's Display
    // format for a record is diagnostic text, unlike its scalar output).
    let bin = format!("/tmp/verbose-text-branch-record-{}", std::process::id());
    crate::native::compile_native(&p, "pack_text", &bin, false, false).unwrap();
    for title in ["", "café", "éééé"] {
        for code in [i64::MIN, -1, 0, 1, i64::MAX] {
            let interpreter::Value::Record(fields) = eval(&p, "pack_text", title, code).unwrap()
            else {
                unreachable!()
            };
            let output = Command::new(&bin)
                .args([title, &code.to_string()])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(0));
            assert!(output.stderr.is_empty());
            assert_eq!(
                output.stdout,
                format!(
                    "{{\"title\":{},\"code\":{}}}\n",
                    crate::value_to_json(&fields["title"]),
                    crate::value_to_json(&fields["code"])
                )
                .as_bytes()
            );
        }
    }
    fs::remove_file(bin).unwrap();
    let (optimized, _) = crate::optimizer::optimize_program(&p);
    differential(&optimized, "compose_text");
    root(&mut p, &[], "render_text(if request.code > 0 then pack_text(request) else FormatInput { code: 0, title: request.title })");
    differential(&p, "compose_text");
    root(&mut p, &[
        ("request", "if request.code > 0 then pack_text(request) else FormatInput { title: \"zero\", code: 0 }"),
        ("saved", "request"),
        ("request", "FormatInput { code: 0, title: \"new\" }"),
    ], "concat(render_text(saved), render_text(request), saved.title)");
    differential(&p, "compose_text");
}

#[test]
fn text_branches_original_inputs_join_constructed_and_returned_values() {
    let mut p = fixture();
    for value in [
        "if request.code > 0 then request else TextInput { code: 0, title: \"new\" }",
        "if request.code > 0 then TextInput { title: \"new\", code: 0 } else request",
        "if request.code > 0 then request else request",
    ] {
        root(
            &mut p,
            &[("selected", value)],
            "render_text(pack_text(selected))",
        );
        differential(&p, "compose_text");
        rule(&mut p, "compose_text").logic.value =
            expression("concat(selected.title, \":\", selected.code)");
        differential(&p, "compose_text");
    }
    let mut identity = rule(&mut p, "render_text").clone();
    identity.name = "identity".into();
    identity.output_ty = Type::Named("FormatInput".into());
    identity.output_text_max = None;
    identity.logic.value = Expr::Ident(identity.input_name.clone());
    p.items.push(Item::Rule(identity));
    root(&mut p, &[
        ("selected", "if request.code > 0 then identity(pack_text(request)) else identity(FormatInput { code: 0, title: request.title })"),
        ("later", "render_text(FormatInput { title: \"later\", code: 1 })"),
    ], "concat(selected.title, later, render_text(selected))");
    differential(&p, "compose_text");
}

#[test]
fn text_branches_nested_joins_preserve_both_owners_across_later_work() {
    let mut p = fixture();
    root(&mut p, &[
        ("left", "concat(\"[\", request.title, \"]\")"),
        ("right", "concat(\"<\", request.title, \">\")"),
        ("picked", "if request.code > 0 then FormatInput { title: left, code: 1 } else FormatInput { code: -1, title: right }"),
        ("saved", "picked"),
        ("picked", "if request.code == 0 then FormatInput { code: 0, title: concat(\"\", request.title) } else saved"),
        ("noise", "concat(\"overwrite candidate\", request.title, request.title)"),
        ("later", "render_text(picked)"),
    ], "concat(saved.title, picked.title, left, right, later)");
    differential(&p, "compose_text");
    // Repeated joins of the same descriptors must not expand owner sets or
    // duplicate evaluation. They retain the two original buffers.
    let r = rule(&mut p, "compose_text");
    for _ in 0..128 {
        r.logic.bindings.push((
            "picked".into(),
            expression("if request.code > 0 then saved else picked"),
        ));
    }
    differential(&p, "compose_text");
}

#[test]
fn text_branches_empty_nul_utf8_and_boolean_fields() {
    let mut p = fixture();
    for (a, b) in [("", "\0é"), ("ééééé", ""), ("a\0b", "é\0")] {
        rule(&mut p, "pack_text").logic.value = Expr::If(
            Box::new(expression("req.code > 0")),
            Box::new(Expr::Record(
                "FormatInput".into(),
                vec![
                    ("title".into(), Expr::Text(a.into())),
                    ("code".into(), Expr::Number(1)),
                ],
            )),
            Box::new(Expr::Record(
                "FormatInput".into(),
                vec![
                    ("code".into(), Expr::Number(-1)),
                    ("title".into(), Expr::Text(b.into())),
                ],
            )),
        );
        differential(&p, "compose_text");
    }
    let mut concept = iter_all_concepts(&p.items)
        .find(|c| c.name == "FormatInput")
        .unwrap()
        .clone();
    concept.name = "Flagged".into();
    concept
        .fields
        .iter_mut()
        .find(|f| f.name == "code")
        .unwrap()
        .ty = Type::Bool;
    concept
        .fields
        .iter_mut()
        .find(|f| f.name == "code")
        .unwrap()
        .range = None;
    p.items.push(Item::Concept(concept));
    root(&mut p, &[("picked", "if request.code > 0 then Flagged { title: request.title, code: request.code > 0 } else Flagged { code: request.code > 0, title: \"no\" }")],
        "if picked.code then picked.title else concat(\"[\", picked.title, \"]\")");
    differential(&p, "compose_text");
}

#[test]
fn text_branches_refuse_invalid_alternatives_before_artifact_emission() {
    for (value, diagnostic) in [
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else FormatInput { code: 0, title: \"elevenbytes\" }", "input field 'title'"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 2 } else FormatInput { code: 0, title: \"ok\" }", "argument range [0, 2]"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else FormatInput { code: request.code, title: \"ok\" }", "argument range"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else request", "expected Named"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else FormatInput { code: \"bad\", title: \"ok\" }", "expected Number"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else FormatInput { title: \"ok\" }", "fields must match"),
        ("if request.code > 0 then FormatInput { title: \"ok\", code: 0 } else FormatInput { code: now_unix(), title: \"ok\" }", "effect"),
    ] {
        let mut p = fixture();
        root(&mut p, &[("selected", value)], "render_text(selected)");
        refusal(&p, diagnostic);
    }
    let mut p = fixture();
    root(&mut p, &[("selected", "if 1 == 1 then FormatInput { title: \"ok\", code: 0 } else FormatInput { title: \"elevenbytes\", code: 0 }")], "render_text(selected)");
    refusal(&p, "input field 'title'"); // Untaken does not mean unchecked.

    root(&mut p, &[("selected", "if request.code > 0 then FormatInput { title: \"elevenbytes\", code: 1 } else FormatInput { title: \"ok\", code: 0 }")],
        "if selected.code == 0 then render_text(selected) else \"unused\"");
    refusal(&p, "input field 'title'"); // No correlation between joined fields.

    let mut p = fixture();
    root(
        &mut p,
        &[(
            "selected",
            "if request.code > 0 then request else TextInput { title: \"ok\", code: 0 }",
        )],
        "selected.title",
    );
    for item in &mut p.items {
        if let Item::Concept(c) = item {
            if c.name == "TextInput" {
                c.fields[0].range = None;
            }
        }
    }
    refusal(&p, "capacity");
}

#[test]
fn text_branches_share_large_exclusive_buffers_but_keep_the_invocation_limit() {
    let mut p = fixture();
    root(&mut p, &[], "\"\"");
    let record = || {
        Expr::Record(
            "FormatInput".into(),
            vec![
                (
                    "title".into(),
                    Expr::Concat(vec![Expr::Text("x".repeat(1_048_576))]),
                ),
                ("code".into(), Expr::Number(0)),
            ],
        )
    };
    rule(&mut p, "compose_text").logic.bindings.push((
        "unused".into(),
        Expr::If(
            Box::new(expression("request.code > 0")),
            Box::new(record()),
            Box::new(record()),
        ),
    ));
    assert!(verify(&p).is_empty());
    // The two 1 MiB alternatives now share storage. An unused let is still
    // eager; both runtime choices execute safely within the 2 MiB frame cap.
    differential(&p, "compose_text");
    // A second result live alongside the selected one still cannot fit. Its
    // owner must remain distinct regardless of which branch was selected.
    let r = rule(&mut p, "compose_text");
    r.logic.bindings.push(("other".into(), record()));
    r.logic.value = expression("concat(length(unused.title), length(other.title))");
    let file = format!("/tmp/verbose-text-branch-frame-{}", std::process::id());
    fs::write(&file, b"preserve").unwrap();
    let error = crate::native::compile_native(&p, "compose_text", &file, false, false).unwrap_err();
    assert!(
        error.message.contains("invocation frame exceeds"),
        "{error}"
    );
    assert_eq!(fs::read(&file).unwrap(), b"preserve");
    fs::remove_file(file).unwrap();
}

#[test]
fn text_branches_overlay_nested_owned_records_without_changing_evaluation() {
    let mut p = fixture();
    root(&mut p, &[
        ("outer", "concat(\"OUTER:\", request.title)"),
        ("selected", "if request.code > 0 then (if request.code == 1 then FormatInput { title: concat(\"yes:\", request.title), code: 1 } else FormatInput { code: 0, title: concat(\"max:\", request.title) }) else FormatInput { title: concat(\"no:\", request.title), code: -1 }"),
        ("saved", "selected"),
        ("selected", "pack_text(request)"),
        ("noise", "concat(\"later:\", request.title, request.title)"),
    ], "concat(outer, saved.title, selected.title, saved.title)");
    differential(&p, "compose_text");
    let (optimized, _) = crate::optimizer::optimize_program(&p);
    differential(&optimized, "compose_text");
    // An earlier concat operand is live while a later operand chooses and
    // writes branch-local destinations. Nor may a shared output destination
    // overlap its branch's temporary, borrowed as an operand of the final copy.
    root(&mut p, &[], "concat(concat(\"first:\", request.title), if request.code > 0 then concat(\"second:\", request.title) else concat(\"third:\", request.title))");
    differential(&p, "compose_text");
}

#[test]
fn text_branches_wasm_refuses_without_replacing_artifacts() {
    let p = fixture();
    let file = format!("/tmp/verbose-text-branch-wasm-{}", std::process::id());
    fs::write(&file, b"preserve").unwrap();
    let error = crate::wasm::compile_wasm(&p, "compose_text", &file).unwrap_err();
    assert!(error.message.contains("bounded text"), "{error}");
    assert_eq!(fs::read(&file).unwrap(), b"preserve");
    fs::remove_file(file).unwrap();
}

#[test]
fn text_branches_count_implicit_input_field_joins_in_the_analysis_budget() {
    let mut p = fixture();
    for item in &mut p.items {
        if let Item::Concept(c) = item {
            if c.name == "TextInput" {
                let template = c.fields[1].clone();
                for i in 0..2048 {
                    let mut f = template.clone();
                    f.name = format!("extra_{i}");
                    c.fields.push(f);
                }
            }
        }
    }
    root(&mut p, &[], "\"\"");
    let r = rule(&mut p, "compose_text");
    for _ in 0..64 {
        r.logic.bindings.push((
            "unused".into(),
            expression("if request.code > 0 then request else request"),
        ));
    }
    refusal(&p, "analysis limit exceeded");
}

#[test]
fn text_branches_stdin_channels_reclaim_selected_values_between_records() {
    use std::io::Write;
    use std::process::Stdio;
    let p = fixture();
    for mode in ["stdin", "stream", "raw"] {
        let bin = format!("/tmp/verbose-text-branch-channel-{}", std::process::id());
        if mode == "raw" {
            crate::native::compile_native_stdin_raw(&p, "compose_text", &bin).unwrap();
        } else {
            crate::native::compile_native(
                &p,
                "compose_text",
                &bin,
                mode == "stdin",
                mode == "stream",
            )
            .unwrap();
        }
        let records: Vec<_> = if mode == "raw" {
            vec![("éé", -1)]
        } else {
            vec![("éé", 1), ("abcdefgh", -1), ("x", 0), ("last", i64::MAX)]
        };
        let expected: String = records
            .iter()
            .map(|(title, code)| format!("{}\n", eval(&p, "compose_text", title, *code).unwrap()))
            .collect();
        let mut command = Command::new(&bin);
        if mode == "raw" {
            command.arg("-1");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let input: String = if mode == "raw" {
            "éé".into()
        } else {
            records
                .iter()
                .map(|(title, code)| format!("{title} {code}\n"))
                .collect()
        };
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(0), "{mode}: {output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, expected.as_bytes(), "{mode}");
        fs::remove_file(bin).unwrap();
    }
}
