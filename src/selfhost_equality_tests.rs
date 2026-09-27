//! Value comparisons in the evaluator written in Verbose, including bootstrap.
use crate::{
    ast::*,
    interpreter::{self, Value},
    lexer::Lexer,
    native,
    parser::Parser,
    verifier,
};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn parse(src: &str) -> Program {
    Parser::new(Lexer::new(src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

fn source(bindings: &str, expr: &str) -> String {
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Unused evaluator input"
  @source: invoices.intent:1
  fields:
    n : number
rule probe
  @intention: "Compare text values by their decoded bytes"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
{bindings}    out = if {expr} then 17 else 29
  proofs:
    purity:
      reads: []
      calls: []
    termination:
      bound: 4000
"#
    )
}

fn assert_value(evaluator: &Path, src: &str, expected: i64, verified: bool) {
    let p = parse(src);
    if verified {
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{src}\n{errors:?}");
        let rules: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
            .collect();
        let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
        assert_eq!(
            interpreter::eval_rule(rules[0], &rules, &concepts, &[], &HashMap::new()).unwrap(),
            Value::Number(expected),
            "original AST: {src}"
        );
    }
    // Self-hosted entry selection routes ScanState.source through stdin because
    // its declared capacity exceeds argv's per-argument limit. Use that same
    // transport for the Rust-built reference evaluator.
    let mut child = Command::new(evaluator)
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
        .write_all(src.as_bytes())
        .unwrap();
    let r = child.wait_with_output().unwrap();
    assert_eq!(
        (r.status.code(), r.stdout, r.stderr),
        (Some(0), format!("{expected}\n").into_bytes(), vec![]),
        "self-hosted evaluator: {src}"
    );
}

fn assert_equality(evaluator: &Path) {
    for (bindings, left, right, equal) in [
        ("", r#""""#, r#""""#, true),
        ("", r#""a""#, r#""a""#, true),
        ("", r#""""#, r#""a""#, false),
        ("", r#""a""#, r#""""#, false),
        ("", r#""abc""#, r#""xbc""#, false),
        ("", r#""abc""#, r#""axc""#, false),
        ("", r#""abc""#, r#""abx""#, false),
        ("", r#""ab""#, r#""abc""#, false),
        ("", r#""abc""#, r#""ab""#, false),
        ("", r#""é€🦀""#, r#""é€🦀""#, true),
        ("", r#""é€🦀""#, r#""é€🦁""#, false),
        ("", r#""é""#, "\"e\u{301}\"", false),
        ("", r#""a\n\r\t\\\"é""#, r#""a\n\r\t\\\"é""#, true),
        ("", r#""a\nb""#, r#""a\\nb""#, false),
        ("", r#""\t""#, "\"\t\"", true),
        ("", r#"substring("é\n🦀", 2, 3)"#, r#""\n""#, true),
        ("", r#"substring("é\n🦀", 3, 7)"#, r#""🦀""#, true),
        ("", r#"substring("é\n🦀", 3, 3)"#, r#""""#, true),
        ("", r#"substring("a\\nb", 1, 3)"#, r#""\n""#, false),
        ("", r#"concat("a", 7)"#, r#""a7""#, true),
        ("", r#"concat("a", 7)"#, r#""a8""#, false),
        ("", r#""a7""#, r#"concat("a", 7)"#, true),
        (
            "",
            r#"concat("é", concat("\n", -42), "🦀")"#,
            r#""é\n-42🦀""#,
            true,
        ),
        ("", r#"concat("a", "b")"#, r#"concat("ab", "")"#, true),
        ("", r#"concat("", "")"#, r#""""#, true),
        ("", r#"concat(0)"#, r#""0""#, true),
        (
            "",
            r#"concat(9223372036854775807)"#,
            r#""9223372036854775807""#,
            true,
        ),
        (
            "",
            r#"concat(0 - 9223372036854775807 - 1)"#,
            r#""-9223372036854775808""#,
            true,
        ),
        (
            "",
            r#"concat(0 - 9223372036854775807 - 1)"#,
            r#""-9223372036854775807""#,
            false,
        ),
        (
            "    let x = \"old\"\n    let a = x\n    let x = \"new\"\n",
            "a",
            "x",
            false,
        ),
        (
            "    let x = \"é\\n\"\n    let x = x\n",
            "x",
            r#""é\n""#,
            true,
        ),
        (
            "    let x = concat(\"a\", 7)\n    let a = x\n    let x = \"a8\"\n",
            "a",
            "x",
            false,
        ),
        ("", r#"if 3 > 2 then "a" else "b""#, r#""a""#, true),
        ("", r#"if "a" == "a" then "b" else "c""#, r#""b""#, true),
        (
            "    let a = \"é\\n\"\n    let n = if a == \"é\\n\" then 7 else 8\n    let fresh = concat(\"fresh\", 42)\n",
            "concat(a, n, fresh)",
            r#""é\n7fresh42""#,
            true,
        ),
        ("", "7", "7", true),
        ("", "7", "8", false),
        (
            "",
            "0 - 9223372036854775807 - 1",
            "0 - 9223372036854775807 - 1",
            true,
        ),
        (
            "",
            "0 - 9223372036854775807 - 1",
            "9223372036854775807",
            false,
        ),
    ] {
        for op in ["==", "!="] {
            let src = source(bindings, &format!("({left}) {op} ({right})"));
            assert_value(
                evaluator,
                &src,
                if equal == (op == "==") { 17 } else { 29 },
                true,
            );
        }
    }

    // Long equal spans and a final-byte mismatch exercise the cursor's end.
    let text = "é\\n🦀".repeat(256);
    for (other, expected) in [
        (text.clone(), 17),
        (format!("{text}x"), 29),
        (format!("{}🦁", text.strip_suffix('🦀').unwrap()), 29),
    ] {
        assert_value(
            evaluator,
            &source("", &format!("\"{text}\" == \"{other}\"")),
            expected,
            true,
        );
    }

    let helper = r#"
rule label
  @intention: "Return a text value through a call"
  @source: invoices.intent:1
  input:
    n : number
  output:
    out : text
  logic:
    out = if n == 0 then "é\n" else concat("é", "\t")
  proofs:
    purity:
      reads: [n]
      calls: []
    termination:
      bound: 20
"#;
    for (expr, expected) in [
        (r#"label(0) == "é\n""#, 17),
        ("label(0) == label(1)", 29),
        (r#"label(1) != "é\n""#, 17),
    ] {
        let src = source("", expr).replace("calls: []", "calls: [label]") + helper;
        assert_value(evaluator, &src, expected, true);
    }

    // The legacy evaluator has no verification/error channel. Pin defensive
    // mixed-kind behavior separately; it is not a source-language extension.
    for (left, right) in [
        (r#""0""#, "0"),
        ("0", r#""0""#),
        (r#"b"x""#, r#"b"x""#),
        (r#"concat(b"x", b"y")"#, r#"concat(b"x", b"y")"#),
        ("Input { n: 1 }", "Input { n: 1 }"),
    ] {
        for op in ["==", "!="] {
            let src = source("", &format!("{left} {op} {right}"));
            assert!(!verifier::verify_program(&parse(&src), Path::new("examples")).is_empty());
            assert_value(evaluator, &src, if op == "==" { 29 } else { 17 }, false);
        }
    }
    // Bool values use VNum in this evaluator; the source verifier separately
    // refuses bool == bool, so preserve this unchecked behavior explicitly.
    for (expr, expected) in [
        ("true == true", 17),
        ("true == false", 29),
        ("true != false", 17),
    ] {
        assert_value(evaluator, &source("", expr), expected, false);
    }
}

#[test]
fn selfhost_evaluator_compares_decoded_text_values() {
    let src = fs::read_to_string("examples/vexprparse.verbose").unwrap();
    let evaluator =
        std::env::temp_dir().join(format!("verbose-equality-eval-{}", std::process::id()));
    native::compile_native_stdin_raw(&parse(&src), "eval_main", evaluator.to_str().unwrap())
        .unwrap();
    assert_equality(&evaluator);
    fs::remove_file(evaluator).unwrap();
}

/// The bootstrap calls this for gen0 and gen1, exercising the evaluator itself
/// after self-hosted emission, rather than just target text-comparison code.
pub(crate) fn assert_emitted_evaluator(compiler: &Path, base: &Path) -> PathBuf {
    let src = fs::read_to_string("examples/vexprparse.verbose").unwrap();
    let index = src
        .lines()
        .filter(|line| line.starts_with("rule "))
        .position(|line| line == "rule eval_main")
        .unwrap();
    let mut child = Command::new("sh")
        .args([
            "-c",
            "ulimit -s unlimited; exec \"$1\" \"$2\"",
            "emit-evaluator",
        ])
        .arg(compiler)
        .arg(index.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(src.as_bytes())
        .unwrap();
    let r = child.wait_with_output().unwrap();
    assert_eq!(r.status.code(), Some(0), "{:?}", r.stderr);
    assert!(r.stderr.is_empty());
    assert!(r.stdout.starts_with(b"\x7fELF"));
    let evaluator = base.join("emitted-evaluator");
    fs::write(&evaluator, r.stdout).unwrap();
    fs::set_permissions(&evaluator, fs::Permissions::from_mode(0o755)).unwrap();
    assert_equality(&evaluator);
    evaluator
}
