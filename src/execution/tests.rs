use super::*;
use crate::{lexer::Lexer, parser::Parser, verifier, wasm};
use std::{fs, path::Path, process::Command};

const PHASES: &str = include_str!("../../examples/sequential_stack.verbose");
const DECLARATION: &str = "\nexecution inspect_readings\n  @intention: \"bounded ordered phases\"\n  @source: execution_stack.intent:1\n  input: Reading\n  mode: sequential\n  phases: [clamp, nonnegative, label]\n  on_failure: stop\n  native_stack: 192\n";

fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn fixture() -> Program {
    parse(&format!("{PHASES}{DECLARATION}"))
}
fn verified(p: &Program) {
    let errors = verifier::verify_program(p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn execution_parser_requires_closed_unique_fields_and_preserves_ordinary_identifiers() {
    for line in DECLARATION.lines().filter(|l| l.starts_with("  ")) {
        for text in [
            DECLARATION.replace(&format!("{line}\n"), ""),
            DECLARATION.replace(line, &format!("{line}\n{line}")),
        ] {
            let tokens = Lexer::new(&format!("{PHASES}{text}")).tokenize().unwrap();
            assert!(Parser::new(tokens).parse_program().is_err(), "{text}");
        }
    }
    for (old, new) in [
        ("mode: sequential", "mode: concurrent"),
        ("on_failure: stop", "on_failure: continue"),
        ("native_stack: 192", "native_stack: 0"),
        ("native_stack: 192", "native_stack: 2097153"),
        ("native_stack: 192", "native_stack: -1"),
        ("native_stack: 192", "native_stack: 9223372036854775807"),
        ("input: Reading", "workers: 2"),
        ("[clamp, nonnegative, label]", "[clamp(), label]"),
    ] {
        assert!(Parser::new(
            Lexer::new(&format!("{PHASES}{}", DECLARATION.replace(old, new)))
                .tokenize()
                .unwrap()
        )
        .parse_program()
        .is_err());
    }
    // The contextual top-level keyword does not reserve rule/field/let names.
    let p = parse(&PHASES.replace("rule label", "rule execution"));
    verified(&p);
    verified(&parse(&format!("{PHASES}{DECLARATION}")));
}

#[test]
fn execution_checks_phase_types_names_bounds_and_unselected_declarations() {
    for (old, new, expected) in [
        ("input: Reading", "input: Missing", "unknown input"),
        ("execution inspect_readings", "execution label", "collides"),
        ("execution inspect_readings", "execution concat", "collides"),
        (
            "phases: [clamp, nonnegative, label]",
            "phases: []",
            "2..=64",
        ),
        (
            "phases: [clamp, nonnegative, label]",
            "phases: [label]",
            "2..=64",
        ),
        (
            "phases: [clamp, nonnegative, label]",
            "phases: [clamp, missing]",
            "no rule",
        ),
        (
            "phases: [clamp, nonnegative, label]",
            "phases: [clamp, inspect_readings]",
            "nested executions",
        ),
        (
            "native_stack: 192",
            "native_stack: 191",
            "192 bytes exceeds declared 191",
        ),
        (
            "@source: execution_stack.intent:1",
            "@source: execution_stack.intent:9999",
            "@source",
        ),
    ] {
        let p = parse(&format!("{PHASES}{}", DECLARATION.replace(old, new)));
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(
            errors.iter().any(|e| e.to_string().contains(expected)),
            "{errors:?}"
        );
    }
    let p = parse(&format!("{PHASES}{DECLARATION}{DECLARATION}"));
    assert!(verify(&p)[0].message.contains("duplicate"));
    let p = parse(&format!(
        "{PHASES}{DECLARATION}{}",
        DECLARATION
            .replace("inspect_readings", "unused")
            .replace("native_stack: 192", "native_stack: 191")
    ));
    assert!(report(&p, "inspect_readings")
        .unwrap_err()
        .message
        .contains("unused"));
    let path = format!("/tmp/verbose-execution-refusal-{}", std::process::id());
    fs::write(&path, b"existing").unwrap();
    for name in ["inspect_readings", "label"] {
        assert!(native::compile_native(&p, name, &path, false, false)
            .unwrap_err()
            .message
            .contains("unused"));
        assert_eq!(fs::read(&path).unwrap(), b"existing");
    }
    fs::remove_file(path).unwrap();
    let many = DECLARATION.replace(
        "[clamp, nonnegative, label]",
        &format!("[{}]", vec!["clamp"; 65].join(",")),
    );
    assert!(verify(&parse(&format!("{PHASES}{many}")))[0]
        .message
        .contains("2..=64"));
    let mut p = fixture();
    for item in &mut p.items {
        if let Item::Rule(r) = item {
            if r.name == "label" {
                r.input_ty = Type::Named("Other".into());
            }
        }
    }
    assert!(verify(&p)[0]
        .message
        .contains("input must be concept 'Reading'"));
}

#[test]
fn execution_uses_the_same_bytes_and_budget_as_the_explicit_phase_selection() {
    let path = format!("/tmp/verbose-execution-run-{}", std::process::id());
    let control = format!("{path}-control");
    for names in [
        vec!["clamp", "label"],
        vec!["clamp", "nonnegative", "label"],
        vec!["label", "clamp", "label"],
        vec!["label"; 64],
    ] {
        let declaration = DECLARATION.replace(
            "[clamp, nonnegative, label]",
            &format!("[{}]", names.join(",")),
        );
        let p = parse(&format!("{PHASES}{declaration}"));
        verified(&p);
        let r = report(&p, "inspect_readings").unwrap();
        assert_eq!(r.sequence.stack_bound_bytes(), 192);
        assert_eq!(
            r.sequence,
            native::sequential_stack_report(&parse(PHASES), &names).unwrap()
        );
        assert!(r.json().contains("\"declared_bytes\":192"));
        assert!(r.json().contains("\"execution\":\"inspect_readings\""));
        native::compile_native(&p, "inspect_readings", &path, false, false).unwrap();
        native::compile_native_multi(&parse(PHASES), &names, &control, false, false).unwrap();
        assert_eq!(fs::read(&path).unwrap(), fs::read(&control).unwrap());
        for args in [
            vec!["a", "2", "b", "1000"],
            vec!["éééé", "-1", "b", "2"],
            vec!["x", "9223372036854775807"],
            vec!["x", "-9223372036854775808"],
            vec!["partial"],
            vec!["x", "bad"],
            vec![],
        ] {
            let a = Command::new(&path).args(&args).output().unwrap();
            let b = Command::new(&control).args(&args).output().unwrap();
            assert_eq!(
                (a.status, a.stdout, a.stderr),
                (b.status, b.stdout, b.stderr)
            );
        }
    }
    native::compile_native(&fixture(), "inspect_readings", &path, false, false).unwrap();
    let out = Command::new(&path)
        .args(["x", "2", "y", "-1", "z", "3"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"2\n-1\n3\ntrue\nfalse\ntrue\n");
    assert!(out.stderr.is_empty());
    fs::remove_file(path).unwrap();
    fs::remove_file(control).unwrap();
}

#[test]
fn execution_unknown_lowering_and_backend_modes_fail_before_writing() {
    let p = fixture();
    let path = format!("/tmp/verbose-execution-modes-{}", std::process::id());
    fs::write(&path, b"existing").unwrap();
    for (stdin, stream) in [(true, false), (false, true), (true, true)] {
        assert!(
            native::compile_native(&p, "inspect_readings", &path, stdin, stream)
                .unwrap_err()
                .message
                .contains("argv")
        );
    }
    assert!(
        native::compile_native_stdin_raw(&p, "inspect_readings", &path)
            .unwrap_err()
            .message
            .contains("argv")
    );
    assert!(
        native::compile_native_multi(&p, &["inspect_readings", "clamp"], &path, false, false)
            .unwrap_err()
            .message
            .contains("selected alone")
    );
    assert!(
        native::compile_http_server(&p, "inspect_readings", 9999, &path)
            .unwrap_err()
            .message
            .contains("execution")
    );
    for name in ["inspect_readings", "clamp"] {
        assert!(wasm::compile_wasm(&p, name, &path)
            .unwrap_err()
            .message
            .contains("execution"));
    }
    assert_eq!(fs::read(&path).unwrap(), b"existing");
    fs::remove_file(path).unwrap();
    // Remove all strict numeric contracts: a source execution does not silently
    // upgrade legacy arithmetic or fabricate a stack proof for unknown lowering.
    let source = format!("{PHASES}{DECLARATION}")
        .replace("    native_stack: 104\n", "")
        .replace("    native_stack: 88\n", "")
        .replace("  hints:\n    overflow: [-100, 100]\n", "");
    assert!(verify(&parse(&source))
        .iter()
        .any(|e| e.message.contains("strict numeric contract")));
}

#[test]
fn execution_self_hosted_gate_is_declaration_scoped_before_all_output_paths() {
    use std::process::Stdio;
    let compiler = parse(include_str!("../../examples/vexprparse.verbose"));
    let dir = std::env::temp_dir().join(format!("verbose-execution-gen0-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // No overflow, bounded text or rule stack annotations can mask this gate.
    let control = r#"@verbose 0.1.0
concept Input
  @intention: "input"
  @source: invoices.intent:1
  fields:
    x : number
rule calculate
  @intention: "identity"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
    out = i.x
  proofs:
    purity:
      reads: [i.x]
      calls: []
    termination:
      bound: 10
"#;
    let declaration = DECLARATION
        .replace("Reading", "Input")
        .replace("[clamp, nonnegative, label]", "[calculate, calculate]");
    let cases = [
        (control.to_string(), false),
        (format!("{control}{declaration}"), true),
        (format!("{control}{}", declaration.replace("mode: sequential", "mode: concurrent")
            .replace("native_stack: 192", "max_in_flight: 2")), true),
        (
            format!(
                "@verbose 0.1.0\n{declaration}{}",
                control.strip_prefix("@verbose 0.1.0\n").unwrap()
            ),
            true,
        ),
        (control.replace("rule calculate", "rule execution"), false),
        (
            control
                .replace("x : number", "execution : number")
                .replace("i.x", "i.execution"),
            false,
        ),
        (
            control
                .replace("out = i.x", "let execution = i.x\n    out = execution")
                .replace("\"identity\"", "\"execution inspect_readings\""),
            false,
        ),
        (format!("{control}\n-- execution ignored\n"), false),
    ];
    for entry in ["verify_errors", "x86_program_src", "elf_program_src"] {
        let bin = dir.join(entry);
        native::compile_native_stdin_raw(&compiler, entry, bin.to_str().unwrap()).unwrap();
        for (case, refused) in &cases {
            let input = dir.join("input.verbose");
            fs::write(&input, case).unwrap();
            let out = Command::new("sh")
                .args(["-c", "ulimit -s unlimited; exec \"$1\" 0", "sh"])
                .arg(&bin)
                .stdin(fs::File::open(input).unwrap())
                .stdout(Stdio::piped())
                .output()
                .unwrap();
            if entry == "verify_errors" {
                assert!(out.status.success(), "{out:?}");
                let count: u32 = String::from_utf8(out.stdout)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert_eq!(count > 0, *refused, "{case}");
            } else if *refused {
                assert_eq!(out.status.code(), Some(1), "{entry}: {case}");
                assert!(out.stdout.is_empty());
            } else {
                assert!(
                    out.status.success() && !out.stdout.is_empty(),
                    "{entry}: {case}\n{out:?}"
                );
            }
        }
    }
    fs::remove_dir_all(dir).unwrap();
}
