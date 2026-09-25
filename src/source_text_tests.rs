//! Source-to-observable-byte regressions, independent of the parsed AST oracle.
use crate::{ast::*, interpreter::{self, Value}, lexer::Lexer, native, optimizer,
    parser::Parser, verifier, wasm};
use std::{collections::HashMap, fs, io::Write, os::unix::fs::PermissionsExt,
    path::Path, process::{Command, Stdio}};

fn source(expr: &str, output: &str) -> String {
    let reads = ["i.s", "i.index"].into_iter().filter(|field| expr.contains(field))
        .collect::<Vec<_>>().join(", ");
    format!(r#"@verbose 0.1.0
concept Input
  @intention: "UTF-8 input"
  @source: invoices.intent:1
  fields:
    s : text [..128]
    index : number
rule probe
  @intention: "Preserve UTF-8 source bytes"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : {output}
  logic:
    out = {expr}
  proofs:
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 100
"#)
}

fn parse(source: &str) -> Program {
    Parser::new(Lexer::new(source).tokenize().unwrap()).parse_program().unwrap()
}

// Expected results name the original source bytes, not values re-read from AST.
fn cases() -> Vec<(String, &'static str, &'static str, i64, Value)> {
    let mut cases = vec![
        ("\"é€🦀e\u{301}\"".into(), "text", "", 0, Value::Text("é€🦀e\u{301}".into())),
        ("concat(\"é\", i.s, \"🦀\")".into(), "text", "€", 0, Value::Text("é€🦀".into())),
        ("length(\"é€🦀\")".into(), "number", "", 0, Value::Number(9)),
        ("if i.s == \"é\" then 1 else 0".into(), "number", "é", 0, Value::Number(1)),
        ("if i.s == \"é\" then 1 else 0".into(), "number", "e\u{301}", 0, Value::Number(0)),
    ];
    for (index, byte) in "é€🦀".bytes().enumerate() {
        cases.push(("byte_at(\"é€🦀\", i.index)".into(), "number", "", index as i64, Value::Number(byte as i64)));
    }
    cases
}

fn expected_stdout(value: &Value) -> Vec<u8> {
    match value {
        Value::Text(s) => [s.as_bytes(), b"\n"].concat(),
        Value::Number(n) => format!("{n}\n").into_bytes(),
        _ => unreachable!(),
    }
}

#[test]
fn source_utf8_interpreter_and_native_preserve_text_operations() {
    let path = std::env::temp_dir().join(format!("verbose-source-utf8-{}", std::process::id()));
    let mut fixtures = cases();
    // gen0's ordinary AstStr output still writes escape spellings verbatim;
    // that separate existing gap is documented in docs/known-gaps.md.
    fixtures.push(("\"é\\n€\\r\\t🦀\\\\\\\"\"".into(), "text", "", 0, Value::Text("é\n€\r\t🦀\\\"".into())));
    // The self-hosted compiler's legacy stdin-raw transport is NUL-terminated.
    fixtures.push(("\"é\0🦀\"".into(), "text", "", 0, Value::Text("é\0🦀".into())));
    for (expr, output, text, index, expected) in fixtures {
        let p = parse(&source(&expr, output));
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty(), "{expr}");
        // Exercise both original-AST semantics and the compiler's folding path.
        for p in [&p, &optimizer::optimize_program(&p).0] {
            let rules: Vec<_> = p.items.iter().filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None }).collect();
            let concepts: Vec<_> = p.items.iter().filter_map(|i| if let Item::Concept(c) = i { Some(c) } else { None }).collect();
            let input = HashMap::from([("s".into(), Value::Text(text.into())), ("index".into(), Value::Number(index))]);
            assert_eq!(interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).unwrap(), expected, "{expr}");
            native::compile_native(p, "probe", path.to_str().unwrap(), false, false).unwrap();
            let result = Command::new(&path).args([text, &index.to_string()]).output().unwrap();
            assert_eq!((result.status.code(), result.stdout, result.stderr), (Some(0), expected_stdout(&expected), vec![]), "{expr}");
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn source_utf8_text_bounds_count_bytes_without_normalization() {
    let path = std::env::temp_dir().join(format!("verbose-source-utf8-bound-{}", std::process::id()));
    for text in ["é", "€", "🦀", "e\u{301}", "é\0🦀"] {
        let literal = format!("\"{text}\"");
        let p = parse(&source(&literal, &format!("text [..{}]", text.len())));
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
        native::compile_native(&p, "probe", path.to_str().unwrap(), false, false).unwrap();
        let result = Command::new(&path).args(["", "0"]).output().unwrap();
        assert_eq!((result.status.code(), result.stdout, result.stderr),
            (Some(0), expected_stdout(&Value::Text(text.into())), vec![]));
        let artifact = fs::read(&path).unwrap();
        let p = parse(&source(&literal, &format!("text [..{}]", text.len() - 1)));
        assert!(!crate::text_bounds::verify(&p).is_empty(), "{text:?} must not fit a smaller byte bound");
        assert!(native::compile_native(&p, "probe", path.to_str().unwrap(), false, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), artifact);
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn source_utf8_wasm_embeds_exact_literal_bytes() {
    let path = std::env::temp_dir().join(format!("verbose-source-utf8-{}.wasm", std::process::id()));
    let text = "é€🦀\0e\u{301}";
    let p = parse(&source(&format!("\"{text}\""), "text"));
    wasm::compile_wasm(&p, "probe", path.to_str().unwrap()).unwrap();
    // Active data segment: end of offset expression, byte length, raw content.
    let payload = [vec![0x0b, text.len() as u8], text.as_bytes().to_vec()].concat();
    let bytes = fs::read(&path).unwrap();
    assert!(bytes.windows(payload.len()).any(|w| w == payload));
    fs::remove_file(path).unwrap();
}

#[test]
#[ignore = "builds the self-hosted emitter; run with the two_generation bootstrap suite"]
fn two_generation_source_utf8_literals_match_reference() {
    let base = std::env::temp_dir().join(format!("verbose-source-utf8-gen0-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let compiler = base.join("compiler");
    let output = base.join("probe");
    let p = parse(&fs::read_to_string("examples/vexprparse.verbose").unwrap());
    native::compile_native_stdin_raw(&p, "elf_program_src", compiler.to_str().unwrap()).unwrap();
    for (expr, output_type, text, index, expected) in cases() {
        let src = source(&expr, output_type);
        let mut child = Command::new(&compiler).arg("0").stdin(Stdio::piped())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(src.as_bytes()).unwrap();
        let emitted = child.wait_with_output().unwrap();
        assert!(emitted.status.success(), "{expr}: {:?}", emitted.stderr);
        assert!(emitted.stdout.starts_with(b"\x7fELF"), "{expr}: no ELF");
        fs::write(&output, emitted.stdout).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o755)).unwrap();
        let result = Command::new(&output).args([text, &index.to_string()]).output().unwrap();
        assert_eq!((result.status.code(), result.stdout, result.stderr), (Some(0), expected_stdout(&expected), vec![]), "{expr}");
    }
    fs::remove_dir_all(base).unwrap();
}
