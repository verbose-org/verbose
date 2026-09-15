use super::*;

const INPUT_SOURCE: &str = include_str!("../../../examples/bounded_text_inputs.verbose");

fn input_fixture() -> Program {
    parse(INPUT_SOURCE)
}

fn field<'a>(p: &'a mut Program, concept: &str, name: &str) -> &'a mut Field {
    p.items
        .iter_mut()
        .find_map(|item| match item {
            Item::Concept(c) if c.name == concept => c.fields.iter_mut().find(|f| f.name == name),
            _ => None,
        })
        .unwrap()
}

fn refusal(p: &Program, message: &str) {
    rejects(p, message);
    let file = format!("/tmp/verbose-text-input-refusal-{}", std::process::id());
    fs::write(&file, b"existing artifact").unwrap();
    let error = crate::native::compile_native(p, "compose_text", &file, false, false).unwrap_err();
    assert!(error.message.contains(message), "{error}");
    assert_eq!(fs::read(&file).unwrap(), b"existing artifact");
    fs::remove_file(&file).unwrap();
    assert!(crate::native::compile_native(p, "compose_text", &file, false, false).is_err());
    assert!(!Path::new(&file).exists());
}

#[test]
fn text_inputs_compose_records_calls_aliases_and_shadowing() {
    let mut p = input_fixture();
    let errors = crate::verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
    differential(&p, "compose_text");
    let (optimized, _) = crate::optimizer::optimize_program(&p);
    differential(&optimized, "compose_text");
    let root = rule(&mut p, "compose_text");
    root.logic.bindings.clear();
    root.logic.value = expression("render_text(pack_text(request))");
    differential(&p, "compose_text");

    let root = rule(&mut p, "compose_text");
    root.logic.bindings = vec![("request".into(), expression("pack_text(request)"))];
    root.logic.value = expression("render_text(request)");
    differential(&p, "compose_text");

    let root = rule(&mut p, "compose_text");
    root.logic.bindings = vec![
        ("original".into(), Expr::Ident("request".into())),
        (
            "request".into(),
            expression("TextInput { title: \"new\", code: 1 }"),
        ),
    ];
    root.logic.value =
        expression("concat(render_text(pack_text(original)), render_text(pack_text(request)))");
    differential(&p, "compose_text");
}

#[test]
fn text_inputs_keep_earlier_fields_and_caller_aliases_alive() {
    let mut p = input_fixture();
    let root = rule(&mut p, "compose_text");
    root.logic.bindings = vec![
        ("title".into(), expression("concat(\"[\", request.title, \"]\")")),
        ("record".into(), expression("FormatInput { title: title, code: if length(concat(\"overwrite candidate\", request.title)) > 0 then 1 else 0 }")),
        ("alias".into(), Expr::Ident("record".into())),
        ("record".into(), expression("FormatInput { code: 0, title: \"new\" }")),
        ("first".into(), expression("render_text(alias)")),
        ("second".into(), expression("render_text(record)")),
    ];
    root.logic.value = expression("concat(first, second, title)");
    differential(&p, "compose_text");

    // A returned record may itself borrow one of the caller's buffers. Keep
    // that owner through a subsequent formatter evaluation and final use.
    let mut identity = rule(&mut p, "render_text").clone();
    identity.name = "identity".into();
    identity.output_ty = Type::Named("FormatInput".into());
    identity.output_text_max = None;
    identity.logic.bindings.clear();
    identity.logic.value = Expr::Ident(identity.input_name.clone());
    p.items.push(Item::Rule(identity));
    let root = rule(&mut p, "compose_text");
    root.logic.bindings[4].1 = expression("identity(alias)");
    root.logic.value = expression("concat(first.title, second, title)");
    differential(&p, "compose_text");
}

#[test]
fn text_inputs_check_numeric_ranges_and_length_bounds() {
    for value in [i64::MIN, -1, 0, 1, i64::MAX] {
        let mut p = input_fixture();
        field(&mut p, "FormatInput", "code").range = Some((value, value));
        let root = rule(&mut p, "compose_text");
        root.logic.bindings.clear();
        root.logic.value = expression("render_text(FormatInput { title: request.title, code: 0 })");
        // The lexer cannot spell i64::MIN as a negated positive literal;
        // exercise that AST value and the ABI extrema independently.
        let Expr::Call(_, args) = &mut root.logic.value else {
            unreachable!()
        };
        let Expr::Record(_, fields) = &mut args[0] else {
            unreachable!()
        };
        fields.iter_mut().find(|(n, _)| n == "code").unwrap().1 = Expr::Number(value);
        differential(&p, "compose_text");
    }
    let mut p = input_fixture();
    field(&mut p, "FormatInput", "code").range = Some((0, 8));
    let root = rule(&mut p, "compose_text");
    root.logic.bindings.clear();
    root.logic.value = expression(
        "render_text(FormatInput { title: request.title, code: length(request.title) })",
    );
    differential(&p, "compose_text");
    field(&mut p, "FormatInput", "code").range = Some((1, 8));
    refusal(&p, "cannot prove argument range [0, 8]");

    field(&mut p, "FormatInput", "code").range = Some((0, 8));
    let mut measure = rule(&mut p, "pack_text").clone();
    measure.name = "measure".into();
    measure.output_ty = Type::Number;
    measure.logic.value = expression("length(req.title)");
    p.items.push(Item::Rule(measure));
    rule(&mut p, "compose_text").logic.value =
        expression("render_text(FormatInput { title: request.title, code: measure(request) })");
    differential(&p, "compose_text");
    field(&mut p, "FormatInput", "code").range = Some((0, 7));
    refusal(&p, "cannot prove argument range [0, 8]");
}

#[test]
fn text_inputs_preserve_empty_nul_and_multibyte_field_values() {
    for text in ["", "\0é", "x\0ééé\0z"] {
        let mut p = input_fixture();
        field(&mut p, "FormatInput", "title").range = Some((0, text.len() as i64));
        let root = rule(&mut p, "compose_text");
        root.logic.bindings.clear();
        root.logic.value = expression("render_text(FormatInput { title: \"\", code: 0 })");
        let Expr::Call(_, args) = &mut root.logic.value else {
            unreachable!()
        };
        let Expr::Record(_, fields) = &mut args[0] else {
            unreachable!()
        };
        fields.iter_mut().find(|(n, _)| n == "title").unwrap().1 = Expr::Text(text.into());
        differential(&p, "compose_text");
    }
}

#[test]
fn text_inputs_refuse_unproved_or_unsupported_transfers_before_emission() {
    for (body, diagnostic) in [
        ("render_text(request)", "expected Named(\"FormatInput\")"),
        ("render_text()", "exactly one record input"),
        ("render_text(request, request)", "exactly one record input"),
        ("render_text(FormatInput { title: concat(\"long\", request.title), code: 0 })", "input field 'title'"),
        ("render_text(FormatInput { title: request.title, code: request.code })", "cannot prove argument range"),
        ("render_text(FormatInput { title: request.title, code: 2 })", "cannot prove argument range"),
        ("render_text(FormatInput { title: request.title })", "fields must match"),
        ("render_text(FormatInput { title: \"x\", title: \"y\" })", "duplicate record field"),
        ("render_text(FormatInput { title: 0, code: 0 })", "expected Text"),
        ("render_text(FormatInput { title: request.title, code: now_unix() })", "effect"),
        ("render_text(if request.code > 0 then FormatInput { title: \"x\", code: 0 } else FormatInput { title: \"too long for the field\", code: 0 })", "input field 'title'"),
        ("if request.code > 0 then render_text(FormatInput { title: \"too long for the field\", code: 0 }) else \"ok\"", "input field 'title'"),
    ] {
        let mut p = input_fixture();
        rule(&mut p, "compose_text").logic.value = expression(body);
        refusal(&p, diagnostic);
    }
    let mut p = input_fixture();
    field(&mut p, "TextInput", "title").range = None;
    refusal(&p, "capacity");
    let mut p = input_fixture();
    field(&mut p, "FormatInput", "title").range = None;
    refusal(&p, "declared byte capacity is unknown");

    // A narrower incidental argument does not specialize a callee's body:
    // the public ten-byte input contract still determines its output bound.
    let mut p = input_fixture();
    rule(&mut p, "render_text").output_text_max = Some(30);
    refusal(&p, "up to 31 bytes");

    let mut p = input_fixture();
    rule(&mut p, "pack_text").logic.value =
        expression("FormatInput { title: req.title, code: req.code }");
    refusal(&p, "cannot prove argument range");
}

#[test]
fn text_inputs_native_entry_guards_justify_forwarded_numeric_ranges() {
    use std::io::Write;
    use std::process::Stdio;
    let mut p = input_fixture();
    field(&mut p, "TextInput", "code").range = Some((-1, 1));
    let root = rule(&mut p, "compose_text");
    root.logic.bindings.clear();
    root.logic.value =
        expression("render_text(FormatInput { title: request.title, code: request.code })");
    assert!(verify(&p).is_empty(), "{:?}", verify(&p));
    // Check the callee as a CLI entry as well as the call whose static proof
    // trusts the caller's declared numeric range. Every input channel uses
    // the same guards before the first body or constructor evaluation.
    for entry in ["render_text", "compose_text"] {
        for mode in ["argv", "stdin", "stream", "raw"] {
            let bin = format!("/tmp/verbose-text-input-guards-{}", std::process::id());
            if mode == "raw" {
                crate::native::compile_native_stdin_raw(&p, entry, &bin).unwrap();
            } else {
                crate::native::compile_native(&p, entry, &bin, mode == "stdin", mode == "stream")
                    .unwrap();
            }
            for code in [i64::MIN, -2, -1, 0, 1, 2, i64::MAX] {
                let reference = eval(&p, entry, "short", code);
                let mut command = Command::new(&bin);
                if mode == "argv" {
                    command.args(["short", &code.to_string()]);
                } else if mode == "raw" {
                    command.arg(code.to_string());
                }
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let mut stdin = child.stdin.take().unwrap();
                if mode == "raw" {
                    stdin.write_all(b"short").unwrap();
                } else if mode != "argv" {
                    stdin
                        .write_all(format!("short {code}\n").as_bytes())
                        .unwrap();
                }
                drop(stdin);
                let output = child.wait_with_output().unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(if reference.is_ok() { 0 } else { 1 }),
                    "{entry} {mode} {code}: {output:?}"
                );
                assert_eq!(
                    output.stdout,
                    reference
                        .map(|v| format!("{v}\n").into_bytes())
                        .unwrap_or_default(),
                    "{entry} {mode} {code}"
                );
                assert!(output.stderr.is_empty());
            }
            fs::remove_file(bin).unwrap();
        }
    }
}

#[test]
fn text_inputs_interpreter_guards_and_wasm_refusal_remain_explicit() {
    let p = input_fixture();
    assert!(eval(&p, "render_text", "12345678901", 0).is_err());
    assert!(eval(&p, "render_text", "short", 2).is_err());
    assert!(eval(&p, "render_text", "short", -1).is_ok());
    let bin = format!("/tmp/verbose-text-input-wasm-{}", std::process::id());
    fs::write(&bin, b"existing artifact").unwrap();
    let error = crate::wasm::compile_wasm(&p, "compose_text", &bin).unwrap_err();
    assert!(error.message.contains("bounded text"), "{error}");
    assert_eq!(fs::read(&bin).unwrap(), b"existing artifact");
    fs::remove_file(&bin).unwrap();
}
