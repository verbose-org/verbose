use super::*;

#[test]
fn pipeline_shared_frame_bound_matches_emitted_stack_depth() {
    let source = include_str!("../../../examples/pipeline_stack.verbose");
    let declaration = &source[source.find("execution ").unwrap()..];
    for phases in ["prepare, render", "prepare, forward, render", "prepare, forward, forward"] {
        let p = parse(&format!("{}\n{}", include_str!("../../../examples/retained_stack.verbose"),
            declaration.replace("prepare, forward, render", phases)));
        verified(&p);
        let crate::execution::Report::Pipeline(report) = crate::execution::report(&p, "prepare_readings").unwrap()
            else { panic!("expected pipeline") };
        let bytes = native_bytes(&p, "prepare_readings");
        assert_eq!(machine_stack_peak(&bytes[120..]), report.invocation.stack_bound_bytes());
    }
}

#[test]
fn record_arithmetic_stack_bound_matches_placed_operands_and_signed_instructions() {
    for expr in [
        "concat(i.code + 1, i.code - 1, i.code * 3)",
        "concat(i.code / (-3), i.code % (-3))",
        "concat(abs(i.code), min(i.code, 2), max(-i.code, -2))",
        "concat((i.code + 1) * (i.code - 2) / 3)",
    ] {
        let source = text_source(expr, "").replace("code : number\n", "code : number [-100, 100]\n");
        check_layout(parse(&source), "checked");
    }
    let p = parse(include_str!("../../../examples/pipeline_totals.verbose"));
    verified(&p);
    let crate::execution::Report::Pipeline(report) = crate::execution::report(&p, "totals").unwrap()
        else { panic!("expected pipeline") };
    assert_eq!(machine_stack_peak(&native_bytes(&p, "totals")[120..]), report.invocation.stack_bound_bytes());
}

fn text_source(expr: &str, bindings: &str) -> String {
    let reads = ["i.title", "i.code"]
        .into_iter()
        .filter(|field| expr.contains(field) || bindings.contains(field))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Bound text input bytes"
  @source: invoices.intent:1
  fields:
    title : text [..8]
    code : number
rule checked
  @intention: "Bound the complete native entry stack"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : text [..128]
  logic:
{bindings}    out = {expr}
  proofs:
    native_stack: 4096
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 1000
"#
    )
}

fn check_layout(mut p: Program, name: &str) -> Report {
    let report = native::stack_report(&p, name).unwrap();
    let text = report.text_frame.as_ref().unwrap();
    assert_eq!(text.saved_register_bytes, 16);
    assert_eq!(
        report.frame_bytes,
        report.input_slot_bytes + report.bookkeeping_bytes
    );
    assert_eq!(
        report.stack_bound_bytes(),
        8 + report.frame_bytes
            + report.input_stack_bytes.max(
                16 + text.frame_bytes()
                    + report.expression_stack_bytes.max(report.output_stack_bytes)
            )
    );
    // Check exact limits against instructions, not another AST estimate.
    rule(&mut p, name).proofs.native_stack = Some(report.stack_bound_bytes() as u32);
    verified(&p);
    let bytes = native_bytes(&p, name);
    assert_eq!(
        machine_stack_peak(&bytes[120..]),
        report.stack_bound_bytes(),
        "{name}"
    );
    rule(&mut p, name).proofs.native_stack = None;
    assert_eq!(bytes, native_bytes(&p, name));
    rule(&mut p, name).proofs.native_stack = Some(report.stack_bound_bytes() as u32 - 1);
    assert!(verify(&p)
        .iter()
        .any(|e| e.context.contains(name) && e.message.contains("exceeds declared")));
    report
}

#[test]
fn text_stack_matches_instructions_for_nested_frames_aliases_calls_and_branches() {
    for (expr, bindings) in [
        ("\"\"", ""), ("\"é\0Z\"", ""), ("i.title", ""),
        ("concat(i.title, i.code)", ""),
        ("concat(concat(i.title, i.code), concat(i.code, i.title))", ""),
        ("if i.code > 0 then concat(i.title, i.code) else concat(\"other\", i.title)", ""),
        ("if i.title == \"hello\" or length(i.title) == 0 then i.title else \"x\"", ""),
        ("concat(saved, second)", "    let first = concat(i.title, i.code)\n    let saved = first\n    let first = \"shadow\"\n    let second = concat(i.title, i.code)\n"),
    ] {
        check_layout(parse(&text_source(expr, bindings)), "checked");
    }
    for s in [
        include_str!("../../../examples/bounded_text_storage.verbose"),
        include_str!("../../../examples/bounded_text_inputs.verbose"),
        include_str!("../../../examples/bounded_text_branches.verbose"),
    ] {
        let p = parse(s);
        for item in &p.items {
            if let Item::Rule(r) = item {
                check_layout(p.clone(), &r.name);
            }
        }
    }
    // Scalar consumers still retain construction scratch in their bound.
    for expr in ["length(checked(other))", "checked(other) == other.title"] {
        let output = if expr.starts_with("length") {
            "number"
        } else {
            "bool"
        };
        let reads = if output == "number" {
            "other"
        } else {
            "other, other.title"
        };
        let caller = format!(
            r#"
rule consumer
  @intention: "Count formatter storage before a scalar result"
  @source: invoices.intent:1
  input:
    other : Input
  output:
    out : {output}
  logic:
    out = {expr}
  proofs:
    purity:
      reads: [{reads}]
      calls: [checked]
    termination:
      bound: 100
"#
        );
        let p = parse(&(text_source("concat(i.title, i.code)", "") + &caller));
        let r = check_layout(p, "consumer");
        assert_eq!(r.expression_stack_bytes, 24);
        assert_eq!(
            r.output_stack_bytes,
            if output == "number" { 24 } else { 0 }
        );
    }
}

#[test]
fn text_stack_counts_declared_destinations_and_reuses_dead_or_exclusive_buffers() {
    let s = text_source("i.title", "");
    let p = parse(&s);
    let a = check_layout(p, "checked");
    let b = check_layout(
        parse(&s.replace("text [..128]", "text [..1048576]")),
        "checked",
    );
    assert_eq!(a.text_frame.as_ref().unwrap().buffer_bytes, 128);
    assert_eq!(b.text_frame.as_ref().unwrap().buffer_bytes, 1048576);
    assert_eq!(b.stack_bound_bytes() - a.stack_bound_bytes(), 1048576 - 128);
    assert_eq!(a.expression_stack_bytes, 0);
    assert_eq!(a.output_stack_bytes, 0); // The newline lives in the code, not stack.

    let bindings = "    let first = concat(i.title, i.code)\n    let saved = first\n    let size = length(saved)\n    let second = concat(i.title, i.code)\n";
    let dead = check_layout(
        parse(&text_source("concat(size, second)", bindings)),
        "checked",
    );
    let live = check_layout(
        parse(&text_source("concat(saved, second)", bindings)),
        "checked",
    );
    assert!(
        live.text_frame.unwrap().buffer_bytes > dead.text_frame.unwrap().buffer_bytes,
        "a later alias use must keep the first destination alive"
    );

    // Structured alternatives reserve only their larger region, while the
    // descriptor/scalar slots from both emitted arms remain reserved.
    let branch = check_layout(
        parse(&text_source(
            "if i.code > 0 then concat(i.title, i.code) else concat(i.code, i.title)",
            "",
        )),
        "checked",
    );
    let sequence = check_layout(
        parse(&text_source(
            "concat(concat(i.title, i.code), concat(i.code, i.title))",
            "",
        )),
        "checked",
    );
    assert!(sequence.text_frame.unwrap().buffer_bytes > branch.text_frame.unwrap().buffer_bytes);
}

#[test]
fn text_stack_modes_and_transitive_contexts_refuse_without_overwriting_artifacts() {
    let mut p = parse(include_str!(
        "../../../examples/bounded_text_storage.verbose"
    ));
    rule(&mut p, "piece").proofs.native_stack = Some(4096);
    verified(&p);
    let path = format!("/tmp/verbose-text-stack-modes-{}", std::process::id());
    fs::write(&path, b"existing").unwrap();
    for name in ["piece", "choose_text"] {
        for err in [
            native::compile_native(&p, name, &path, true, false),
            native::compile_native(&p, name, &path, false, true),
            native::compile_native_stdin_raw(&p, name, &path),
            native::compile_native_multi(&p, &[name, name], &path, true, false),
            native::compile_http_server(&p, name, 18999, &path),
        ] {
            assert!(err.unwrap_err().message.contains("native_stack"));
            assert_eq!(fs::read(&path).unwrap(), b"existing");
        }
    }
    assert!(crate::wasm::compile_wasm(&p, "piece", &path)
        .unwrap_err()
        .message
        .contains("native_stack"));
    rule(&mut p, "piece").proofs.native_stack = Some(1);
    assert!(
        native::compile_native(&p, "choose_text", &path, false, false)
            .unwrap_err()
            .message
            .contains("exceeds declared")
    );
    assert_eq!(fs::read(&path).unwrap(), b"existing");

    let mut state = parse(include_str!("../../../examples/bounded_text_state.verbose"));
    rule(&mut state, "wrap_body").proofs.native_stack = Some(65536);
    assert!(verify(&state).iter().any(
        |e| e.context.contains("service 'remember_http'") && e.message.contains("native_stack")
    ));
    assert!(native::compile_service(&state, "remember_http", &path)
        .unwrap_err()
        .message
        .contains("native_stack"));
    assert_eq!(fs::read(&path).unwrap(), b"existing");
    // A separately declared pure entry does not restrict unrelated services.
    rule(&mut state, "wrap_body").proofs.native_stack = None;
    state.items.extend(parse(&text_source("i.title", "")).items);
    assert!(verify(&state).is_empty(), "{:?}", verify(&state));
    native::compile_service(&state, "remember_http", &path).unwrap();
    fs::remove_file(&path).unwrap();

    let mut unknown = parse(&text_source("substring(i.title, 0, 1)", ""));
    assert!(verify(&unknown)
        .iter()
        .any(|e| e.message.contains("unavailable")));
    rule(&mut unknown, "checked").output_text_max = None;
    assert!(native::stack_report(&unknown, "checked").is_err());
}

#[test]
fn text_stack_preserves_interpreted_values_native_bytes_guards_and_repeated_records() {
    let mut p = parse(&text_source("concat(saved, \"|\", i.code, \"\0Z\")",
        "    let first = concat(\"[\", i.title, \"]\")\n    let saved = first\n    let first = \"shadow\"\n"));
    verified(&p);
    let path = format!("/tmp/verbose-text-stack-values-{}", std::process::id());
    native::compile_native(&p, "checked", &path, false, false).unwrap();
    let annotated = fs::read(&path).unwrap();
    let report = native::stack_report(&p, "checked").unwrap();
    assert_eq!(
        machine_stack_peak(&annotated[120..]),
        report.stack_bound_bytes()
    );
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let mut args = Vec::new();
    let mut expected = Vec::new();
    for title in ["", "a", "abcdefgh", "éééé"] {
        for code in [i64::MIN, -1, 0, 1, i64::MAX] {
            let input = HashMap::from([
                ("title".into(), interpreter::Value::Text(title.into())),
                ("code".into(), interpreter::Value::Number(code)),
            ]);
            let value = interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).unwrap();
            let oracle = format!("[{title}]|{code}\0Z");
            assert_eq!(value, interpreter::Value::Text(oracle.clone()));
            expected.extend_from_slice(format!("{oracle}\n").as_bytes());
            args.extend([title.to_string(), code.to_string()]);
        }
    }
    let out = Command::new(&path).args(args).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    assert_eq!(out.stdout, expected);
    let invalid: Vec<_> = [vec![], vec!["ééééé", "0"], vec!["a"], vec!["ok", "1", "b"]]
        .into_iter()
        .map(|args| {
            (
                args.clone(),
                Command::new(&path).args(args).output().unwrap(),
            )
        })
        .collect();
    rule(&mut p, "checked").proofs.native_stack = None;
    native::compile_native(&p, "checked", &path, false, false).unwrap();
    assert_eq!(annotated, fs::read(&path).unwrap());
    for (args, before) in invalid {
        let after = Command::new(&path).args(args).output().unwrap();
        assert_eq!(
            (before.status, before.stdout, before.stderr),
            (after.status, after.stdout, after.stderr)
        );
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn text_stack_rejects_service_handlers_reaction_triggers_and_effect_calls() {
    let original = parse(include_str!("../../../examples/http_bounded_text.verbose"));
    for name in ["response_text", "handle"] {
        let mut p = original.clone();
        rule(&mut p, name).proofs.native_stack = Some(65536);
        let errors = verify(&p);
        assert!(
            errors
                .iter()
                .any(|e| e.context.contains("service 'bounded_text_http'")
                    && e.message.contains("pure native argv")),
            "{errors:?}"
        );
    }
    let mut p = original;
    rule(&mut p, "response_text").proofs.native_stack = Some(65536);
    // A log call can reach the same declaration even when the handler does not.
    let Item::Service(service) = p.items.last_mut().unwrap() else {
        panic!()
    };
    service.handler = "unrelated".into();
    service.logs.push(LogBlock {
        effect: Effect::AppendFile {
            path: "/tmp/never-written".into(),
            content: Expr::Call("response_text".into(), vec![Expr::Ident("req".into())]),
        },
        on_error: ErrorPolicy::Drop,
    });
    assert!(context_errors(&p)
        .iter()
        .any(|e| e.message.contains("response_text")));

    p.items.pop();
    let source = rule(&mut p, "handle").source.clone();
    for (trigger, effects) in [
        ("handle", vec![]),
        (
            "unrelated",
            vec![Effect::Print(vec![Expr::Call(
                "handle".into(),
                vec![Expr::Ident("req".into())],
            )])],
        ),
    ] {
        let mut p = p.clone();
        p.items.push(Item::Reaction(Reaction {
            name: "event".into(),
            intention: "Refuse uncounted effects".into(),
            source: source.clone(),
            trigger: trigger.into(),
            effects,
        }));
        assert!(verify(&p)
            .iter()
            .any(|e| e.context.contains("reaction 'event'") && e.message.contains("native_stack")));
    }
}
