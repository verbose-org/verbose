use super::*;
use crate::{lexer::Lexer, parser::Parser, verifier, wasm};
use std::{fs, path::Path, process::Command};

const RULES: &str = include_str!("../../../examples/retained_stack.verbose");
const SOURCE: &str = include_str!("../../../examples/pipeline_stack.verbose");

fn source(phases: &str) -> String {
    format!(
        "{}\n{}",
        RULES.split("rule analyze").next().unwrap(),
        SOURCE[SOURCE.find("execution ").unwrap()..].replace("prepare, forward, render", phases)
    )
}
fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn fixture(phases: &str) -> Program {
    parse(&source(phases))
}
fn checked(p: &Program) {
    let errors = verifier::verify_program(p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
}
fn declaration(p: &mut Program) -> &mut Execution {
    p.items
        .iter_mut()
        .find_map(|i| match i {
            Item::Execution(e) => Some(e),
            _ => None,
        })
        .unwrap()
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
fn report(p: &Program) -> Report {
    let super::super::Report::Pipeline(r) = super::super::report(p, "prepare_readings").unwrap()
    else {
        panic!("expected pipeline")
    };
    r
}

#[test]
fn pipeline_parser_closes_resource_and_workload_fields() {
    checked(&fixture("prepare, forward, render"));
    for change in [
        "  max_in_flight: 2\n", "  native_memory: 20480\n", "  result_batch: 32\n",
        "  workload:\n    objective: elapsed\n    case common:\n      weight: 1\n      records: 1\n",
    ] {
        let tokens = Lexer::new(&format!("{}{change}", source("prepare, render"))).tokenize().unwrap();
        assert!(Parser::new(tokens).parse_program().unwrap_err().message.contains("pipeline"));
    }
    let text = source("prepare, render").replace("  native_stack: 512\n", "");
    assert!(Parser::new(Lexer::new(&text).tokenize().unwrap())
        .parse_program()
        .is_err());
    let mut p = fixture("prepare, render");
    declaration(&mut p).workload = Some(Workload {
        objective: WorkloadObjective::Elapsed,
        cases: vec![WorkloadCase {
            name: "common".into(),
            weight: 1,
            records: 1,
            target_us: None,
        }],
    });
    assert!(super::super::verify(&p)[0]
        .message
        .contains("does not accept workload"));
}

#[test]
fn pipeline_checks_links_arity_and_source_contracts() {
    for (phases, expected) in [
        ("forward, render", "must match pipeline value"),
        ("prepare, prepare", "must match pipeline value"),
        ("prepare, render, forward", "intermediate pipeline result"),
        ("prepare, missing", "no rule"),
        ("prepare, prepare_readings", "nested executions"),
        ("prepare", "2..=64"),
        ("", "2..=64"),
    ] {
        let p = fixture(phases);
        assert!(
            super::super::verify(&p)
                .iter()
                .any(|e| e.message.contains(expected)),
            "{phases}"
        );
    }
    let names = std::iter::once("prepare")
        .chain(std::iter::repeat("forward").take(62))
        .chain(std::iter::once("render"))
        .collect::<Vec<_>>()
        .join(", ");
    checked(&fixture(&names));
    assert!(
        super::super::verify(&fixture(&format!("{names}, render")))[0]
            .message
            .contains("2..=64")
    );
    // Source proof checks are still required: the declaration is not a purity
    // or termination proof for the source rules it selects.
    let text =
        source("prepare, render").replace("reads: [reading.title, reading.code]", "reads: []");
    assert!(!verifier::verify_program(&parse(&text), Path::new("examples")).is_empty());
}

#[test]
fn pipeline_proves_transferred_capacities_and_ranges_not_just_concept_names() {
    for (from, to, expected) in [
        (
            "code: if reading.code > 0 then 1 else -1",
            "code: reading.code",
            "cannot prove argument range",
        ),
        (
            "title: concat(\"[\", reading.title, \"]\")",
            "title: concat(\"[[\", reading.title, \"]]\")",
            "exceeds declared [..10]",
        ),
    ] {
        let p = parse(&source("prepare, forward, render").replace(from, to));
        let errors = super::super::verify(&p);
        assert!(
            errors.iter().any(|e| e.message.contains(expected)),
            "{errors:?}"
        );
    }
    let mut p = fixture("prepare, forward, render");
    rule(&mut p, "prepare").logic.value = Expr::If(
        Box::new(Expr::Binary(
            BinOp::Gt,
            Box::new(Expr::Field(
                Box::new(Expr::Ident("reading".into())),
                "code".into(),
            )),
            Box::new(Expr::Number(0)),
        )),
        Box::new(rule(&mut p, "prepare").logic.value.clone()),
        Box::new(Expr::Record(
            "Prepared".into(),
            vec![
                ("code".into(), Expr::Number(100)),
                ("title".into(), Expr::Text("".into())),
            ],
        )),
    );
    assert!(super::super::verify(&p)
        .iter()
        .any(|e| e.message.contains("cannot prove argument range")));
}

#[test]
fn pipeline_refuses_unknown_effectful_recursive_and_contextual_shapes() {
    for value in [
        Expr::Call("prepare".into(), vec![Expr::Ident("reading".into())]),
        Expr::Call("missing".into(), vec![Expr::Ident("reading".into())]),
        Expr::Binary(
            BinOp::Add,
            Box::new(Expr::Number(i64::MAX)),
            Box::new(Expr::Number(1)),
        ),
    ] {
        let mut p = fixture("prepare, render");
        rule(&mut p, "prepare")
            .logic
            .bindings
            .push(("unused".into(), value));
        assert!(!super::super::verify(&p).is_empty());
    }
    for attribute in ["context", "hint"] {
        let mut p = fixture("prepare, render");
        let r = rule(&mut p, "prepare");
        if attribute == "context" {
            r.context_name = Some("settings".into());
        } else {
            r.hints = Some(Hints {
                vectorizable: None,
                parallel: None,
                cache_result: None,
                overflow: None,
            });
        }
        assert!(!super::super::verify(&p).is_empty());
    }
    // An eager unused effect cannot disappear through call expansion.
    let text = source("prepare, render").replace(
        "    out = Prepared",
        "    let time = now_unix()\n    out = Prepared",
    );
    let errors = super::super::verify(&parse(&text));
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unsupported expression")),
        "{errors:?}"
    );
}

#[test]
fn pipeline_uses_one_checked_frame_and_matches_explicit_call_lowering() {
    let p = fixture("prepare, forward, render");
    checked(&p);
    let r = report(&p);
    let mut exact = p.clone();
    declaration(&mut exact).mode = ExecutionMode::Pipeline {
        native_stack: r.invocation.stack_bound_bytes() as u32,
    };
    checked(&exact);
    assert_eq!(report(&exact).invocation, r.invocation);
    let calls = &r.invocation.text_frame.as_ref().unwrap().calls;
    assert_eq!(
        calls.iter().map(|c| c.callee.as_str()).collect::<Vec<_>>(),
        ["prepare", "forward", "render"]
    );
    assert!(calls[1].live_caller_buffer_capacity_bytes > 0);

    let explicit = format!(
        r#"{}
rule composed
  @intention: "Compose with ordinary checked lets"
  @source: pipeline_stack.intent:1
  input:
    original : Reading
  output:
    out : text [..31]
  logic:
    let first = prepare(original)
    let second = forward(first)
    out = render(second)
  proofs:
    purity:
      reads: [original]
      calls: [prepare, forward, render]
    termination:
      bound: 50
"#,
        RULES.split("rule analyze").next().unwrap()
    );
    let control = parse(&explicit);
    checked(&control);
    let (control_code, control_layout) = native::pipeline_layout(&control, "composed").unwrap();
    assert_eq!(
        control_layout.stack_bound_bytes(),
        r.invocation.stack_bound_bytes()
    );
    let dir = std::env::temp_dir().join(format!("verbose-pipeline-compose-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let a = dir.join("pipeline");
    let b = dir.join("explicit");
    native::compile_native(&control, "composed", b.to_str().unwrap(), false, false).unwrap();
    for source in [&p, &exact] {
        native::compile_native(
            source,
            "prepare_readings",
            a.to_str().unwrap(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(&fs::read(&a).unwrap()[120..], control_code);
        for args in [
            vec!["café", "42", "sample", "-42"],
            vec!["", "-9223372036854775808"],
            vec!["éééé", "9223372036854775807"],
            vec!["ok", "1", "too long!", "2"],
            vec![],
        ] {
            let x = Command::new(&a).args(&args).output().unwrap();
            let y = Command::new(&b).args(&args).output().unwrap();
            assert_eq!(
                (x.status, x.stdout, x.stderr),
                (y.status, y.stdout, y.stderr)
            );
        }
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn pipeline_unselected_limits_and_unsupported_backends_preserve_artifacts() {
    let p = fixture("prepare, forward, render");
    let bound = report(&p).invocation.stack_bound_bytes() as u32;
    let mut bad = p.clone();
    let mut unused = declaration(&mut bad).clone();
    unused.name = "unused".into();
    unused.mode = ExecutionMode::Pipeline {
        native_stack: bound - 1,
    };
    bad.items.push(Item::Execution(unused));
    let path = format!("/tmp/verbose-pipeline-refusal-{}", std::process::id());
    fs::write(&path, b"preserve").unwrap();
    for selected in ["prepare_readings", "render"] {
        assert!(native::compile_native(&bad, selected, &path, false, false)
            .unwrap_err()
            .message
            .contains("unused"));
    }
    for (stdin, stream) in [(true, false), (false, true)] {
        assert!(
            native::compile_native(&p, "prepare_readings", &path, stdin, stream)
                .unwrap_err()
                .message
                .contains("argv")
        );
    }
    assert!(native::compile_native_stdin_raw(&p, "prepare_readings", &path).is_err());
    assert!(
        native::compile_native_multi(&p, &["prepare_readings", "render"], &path, false, false)
            .is_err()
    );
    assert!(native::compile_http_server(&p, "prepare_readings", 12345, &path).is_err());
    assert!(wasm::compile_wasm(&p, "prepare_readings", &path)
        .unwrap_err()
        .message
        .contains("execution"));
    assert_eq!(fs::read(&path).unwrap(), b"preserve");
    fs::remove_file(&path).unwrap();
    assert!(native::compile_native(&bad, "prepare_readings", &path, false, false).is_err());
    assert!(!Path::new(&path).exists());
}

#[test]
fn pipeline_optimizer_preserves_source_layout_without_text_output_annotations() {
    let mut p = fixture("prepare, forward, render");
    let r = rule(&mut p, "render");
    r.output_ty = Type::Bool;
    r.output_text_max = None;
    r.logic.value = Expr::Binary(
        BinOp::Gt,
        Box::new(Expr::Number(1)),
        Box::new(Expr::Number(0)),
    );
    r.proofs.purity.reads.clear();
    checked(&p);
    assert!(crate::text_bounds::active_rules(&p).is_empty());
    let before = prepare(&p, super::super::find(&p, "prepare_readings").unwrap()).unwrap();
    let optimized = crate::optimizer::optimize_program(&p).0;
    checked(&optimized);
    let after = prepare(
        &optimized,
        super::super::find(&optimized, "prepare_readings").unwrap(),
    )
    .unwrap();
    assert_eq!(before.0, after.0);
    assert_eq!(before.1.json(), after.1.json());
}
