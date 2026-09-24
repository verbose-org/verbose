use super::*;
use crate::{lexer::Lexer, parser::Parser, verifier, wasm};
use std::{fs, path::Path};

const PHASES: &str = include_str!("../../examples/sequential_stack.verbose");
fn source() -> String {
    let declaration = include_str!("../../examples/concurrent_execution.verbose");
    format!(
        "{PHASES}\n{}",
        &declaration[declaration.find("execution inspect_together").unwrap()..]
    )
}
fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

#[test]
fn concurrent_execution_parser_closes_mode_specific_resource_fields() {
    for (old, new) in [
        ("max_in_flight: 2", "max_in_flight: 0"),
        ("max_in_flight: 2", "max_in_flight: 65"),
        ("max_in_flight: 2", "max_in_flight: -1"),
        ("max_in_flight: 2", "native_stack: 192"),
        ("max_in_flight: 2", "max_in_flight: 2\n  native_stack: 192"),
        ("max_in_flight: 2", "max_in_flight: 2\n  max_in_flight: 2"),
        ("  max_in_flight: 2\n", ""),
        ("mode: concurrent", "mode: sequential"),
        ("mode: concurrent", "mode: unknown"),
        ("on_failure: stop", "on_failure: continue"),
    ] {
        assert!(
            Parser::new(Lexer::new(&source().replace(old, new)).tokenize().unwrap())
                .parse_program()
                .is_err(),
            "{old} -> {new}"
        );
    }
    for limit in [1, 2, 64] {
        let p = parse(&source().replace("max_in_flight: 2", &format!("max_in_flight: {limit}")));
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
        assert!(
            matches!(&p.items.last().unwrap(), Item::Execution(Execution {
            mode: ExecutionMode::Concurrent { max_in_flight, .. }, .. }) if *max_in_flight == limit)
        );
    }
    // These remain ordinary field/local/rule identifiers outside the attribute.
    let p = parse(&PHASES.replace("rule label", "rule max_in_flight"));
    assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
}

#[test]
fn result_batch_is_closed_defaulted_and_checked_in_unselected_asts() {
    let original = source();
    let p = parse(&original);
    let Item::Execution(e) = p.items.last().unwrap() else {
        unreachable!()
    };
    assert!(matches!(
        e.mode,
        ExecutionMode::Concurrent {
            result_batch: 1,
            ..
        }
    ));
    for capacity in [1, 8, 1024] {
        let p = parse(&format!("{original}  result_batch: {capacity}\n"));
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
        let Item::Execution(e) = p.items.last().unwrap() else {
            unreachable!()
        };
        assert!(
            matches!(e.mode, ExecutionMode::Concurrent { result_batch, .. } if result_batch == capacity)
        );
    }
    for tail in [
        "result_batch: 0",
        "result_batch: -1",
        "result_batch: 1025",
        "result_batch: 2\n  result_batch: 2",
    ] {
        assert!(Parser::new(
            Lexer::new(&format!("{original}  {tail}\n"))
                .tokenize()
                .unwrap()
        )
        .parse_program()
        .is_err());
    }
    let sequential = original
        .replace("mode: concurrent", "mode: sequential")
        .replace("max_in_flight: 2", "native_stack: 192\n  result_batch: 1");
    assert!(Parser::new(Lexer::new(&sequential).tokenize().unwrap())
        .parse_program()
        .unwrap_err()
        .to_string()
        .contains("sequential execution does not accept result_batch"));
    for capacity in [0, 1025] {
        let mut p = p.clone();
        let Item::Execution(e) = p.items.last_mut().unwrap() else {
            unreachable!()
        };
        let ExecutionMode::Concurrent { result_batch, .. } = &mut e.mode else {
            unreachable!()
        };
        *result_batch = capacity;
        assert!(verify(&p)[0].message.contains("result_batch"));
    }
}

#[test]
fn concurrent_contract_gates_unknown_phases_and_all_backend_artifacts() {
    let p = parse(&source());
    let path =
        std::env::temp_dir().join(format!("verbose-concurrent-refusal-{}", std::process::id()));
    fs::write(&path, b"existing").unwrap();
    let path_str = path.to_str().unwrap();
    let e = native::compile_native(&p, "inspect_together", path_str, false, false).unwrap_err();
    assert!(e.message.contains("native_memory is required"));
    assert!(report(&p, "inspect_together")
        .unwrap_err()
        .message
        .contains("no native stack report"));
    assert!(wasm::compile_wasm(&p, "inspect_together", path_str)
        .unwrap_err()
        .message
        .contains("source execution"));
    for (stdin, stream) in [(true, false), (false, true)] {
        assert!(native::compile_native(&p, "inspect_together", path_str, stdin, stream).is_err());
    }
    assert!(native::compile_native_stdin_raw(&p, "inspect_together", path_str).is_err());
    assert!(native::compile_native_multi(
        &p,
        &["clamp", "inspect_together"],
        path_str,
        false,
        false
    )
    .is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing");
    fs::remove_file(path).unwrap();
    for (old, new, expected) in [
        ("input: Reading", "input: Missing", "unknown input"),
        ("[clamp, nonnegative, label]", "[clamp, missing]", "no rule"),
        (
            "[clamp, nonnegative, label]",
            "[clamp, inspect_together]",
            "nested executions",
        ),
        ("native_stack: 192", "native_stack: 191", "exceeds"),
        ("execution inspect_together", "execution label", "collides"),
        (
            "concurrent_execution.intent:1",
            "concurrent_execution.intent:999",
            "@source",
        ),
        (
            "out = concat(item.title, \":\", item.value)",
            "out = concat(item.title, \":\", now_unix())",
            "unsupported expression in bounded text analysis: effect",
        ),
    ] {
        let errors =
            verifier::verify_program(&parse(&source().replace(old, new)), Path::new("examples"));
        assert!(
            errors.iter().any(|e| e.to_string().contains(expected)),
            "{old} -> {new}: {errors:?}"
        );
    }
    let mut unknown = p.clone();
    for item in &mut unknown.items {
        if let Item::Rule(r) = item {
            r.hints = None;
            r.output_text_max = None;
        }
    }
    assert!(verify(&unknown)[0]
        .message
        .contains("concurrent phase analysis unavailable"));
    let mut recursive = p.clone();
    if let Some(Item::Rule(r)) = recursive
        .items
        .iter_mut()
        .find(|i| matches!(i, Item::Rule(r) if r.name == "clamp"))
    {
        r.logic.value = Expr::Call("clamp".into(), vec![Expr::Ident(r.input_name.clone())]);
    }
    assert!(verify(&recursive)[0]
        .message
        .contains("concurrent phase analysis unavailable"));
    let mut p = p;
    if let Item::Execution(e) = p.items.last_mut().unwrap() {
        e.mode = ExecutionMode::Concurrent {
            result_batch: 1,
            max_in_flight: 0,
            native_memory: None,
        };
    }
    assert!(verify(&p)[0].message.contains("max_in_flight"));
}
