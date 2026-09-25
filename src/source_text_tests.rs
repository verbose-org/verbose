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
        ("\"é\\n€\\r\\t🦀\\\\\\\"\"".into(), "text", "", 0, Value::Text("é\n€\r\t🦀\\\"".into())),
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
fn selfhost_text_evaluator_decodes_lengths_reads_and_slice_boundaries() {
    let p = parse(&fs::read_to_string("examples/vexprparse.verbose").unwrap());
    let path = std::env::temp_dir().join(format!("verbose-text-escape-eval-{}", std::process::id()));
    native::compile_native(&p, "eval_main", path.to_str().unwrap(), false, false).unwrap();
    for (expr, expected) in [
        (r#"length("a\nb")"#, b"3\n"),
        (r#"length(substring("a\nb", 1, 3))"#, b"2\n"),
        (r#"length(substring("a\\nb", 1, 3))"#, b"2\n"),
        (r#"length(substring("a\nb", 1, 1))"#, b"0\n"),
        (r#"length(substring("a\nb", 0, 4))"#, b"1\n"), // invalid slice -> defensive VNum(0), whose printed length is 1
    ] {
        let src = format!("rule main\n  logic:\n    out = {expr}\n");
        let r = Command::new(&path).args([&src, "0"]).output().unwrap();
        assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(0), expected.to_vec(), vec![]));
    }
    for (expr, expected) in [
        (r#"byte_at("a\nb", 1)"#, b"10\n"),
        (r#"byte_at(substring("a\nb", 1, 3), 0)"#, b"10\n"),
        (r#"byte_at(substring("a\\nb", 1, 3), 0)"#, b"92\n"),
    ] {
        let src = format!("rule main\n  logic:\n    out = {expr}\n");
        let r = Command::new(&path).args([&src, "0"]).output().unwrap();
        assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(0), expected.to_vec(), vec![]));
    }
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
    assert_selfhost_text_escapes(&compiler, &base);
    fs::remove_dir_all(base).unwrap();
}

/// Also run by the fixed-point bootstrap on gen1, not just the Rust-built gen0.
pub(crate) fn assert_selfhost_text_escapes(compiler: &Path, base: &Path) {
    let binary = base.join("escape-probe");
    let reference = base.join("escape-reference");
    let emit = |source: &str| {
        let mut child = Command::new(compiler).arg("0").stdin(Stdio::piped())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(source.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let check = |src: &str, stdout: &[u8], stderr: &[u8], status: i32| {
        let p = parse(src);
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty(), "{src}");
        // Match the CLI pipeline; some legacy scalar/text forms require folding.
        native::compile_native(&optimizer::optimize_program(&p).0, "probe", reference.to_str().unwrap(), false, false)
            .unwrap_or_else(|e| panic!("reference: {src}: {e}"));
        let r = Command::new(&reference).args(["a\nb", "0"]).output().unwrap();
        assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(status), stdout.to_vec(), stderr.to_vec()), "reference: {src}");
        let r = emit(src);
        assert!(r.status.success() && r.stdout.starts_with(b"\x7fELF"), "{src}: {:?}", r.stderr);
        // A complete ELF: both load sizes must include all constant-data padding.
        let size = u64::from_le_bytes(r.stdout[96..104].try_into().unwrap()) as usize;
        assert_eq!(size, r.stdout.len());
        assert_eq!(&r.stdout[96..104], &r.stdout[104..112]);
        fs::write(&binary, &r.stdout).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        let result = Command::new(&binary).args(["a\nb", "0"]).output().unwrap();
        assert_eq!((result.status.code(), result.stdout, result.stderr), (Some(status), stdout.to_vec(), stderr.to_vec()), "self-hosted: {src}");
        r.stdout
    };
    for (expr, ty, stdout, stderr, status) in [
        (r#""a\nb\r\t\\\"é€🦀""#, "text", "a\nb\r\t\\\"é€🦀\n".as_bytes(), &b""[..], 0),
        (r#""\\n""#, "text", &b"\\n\n"[..], &b""[..], 0),
        (r#"concat("", "a\n", "b\t")"#, "text", &b"a\nb\t\n"[..], &b""[..], 0),
        (r#"if i.index == 0 then "a\n" else "b\t""#, "text", &b"a\n\n"[..], &b""[..], 0),
        (r#"if i.index == 1 then "a\n" else "b\t""#, "text", &b"b\t\n"[..], &b""[..], 0),
        (r#"length("a\nb")"#, "number", &b"3\n"[..], &b""[..], 0),
        (r#"byte_at("a\nb", 1)"#, "number", &b"10\n"[..], &b""[..], 0),
        (r#"byte_at("a\nb", 2)"#, "number", &b"98\n"[..], &b""[..], 0),
        (r#"byte_at("a\nb", 3)"#, "number", &b""[..], &b""[..], 1),
        (r#"byte_at(substring("é\n🦀", 2, 3), 0)"#, "number", &b"10\n"[..], &b""[..], 0),
        (r#"length(substring("a\nb", 1, 3))"#, "number", &b"2\n"[..], &b""[..], 0),
        (r#"byte_at(substring("a\nb", 1, 3), 0)"#, "number", &b"10\n"[..], &b""[..], 0),
        (r#"length(substring("a\nb", 0, 4))"#, "number", &b""[..], &b""[..], 1),
        (r#"if i.s == "a\nb" then 7 else 0"#, "number", &b"7\n"[..], &b""[..], 0),
        (r#"if i.s == "a\\nb" then 0 else 7"#, "number", &b"7\n"[..], &b""[..], 0),
        (r#"Err("oops\né")"#, "Result(number, text)", &b""[..], "oops\né\n".as_bytes(), 1),
        (r#"Ok("yes\né")"#, "Result(text, text)", "yes\né\n".as_bytes(), &b""[..], 0),
        (r#"b"\x41\n\x00\xff""#, "bytes", &b"A\n\0\xff"[..], &b""[..], 0),
    ] {
        check(&source(expr, ty), stdout, stderr, status);
    }
    // Text aliases are usable by scalar consumers; direct alias printing has
    // a pre-existing self-hosted type-classification gap (see known-gaps.md).
    let src = source("byte_at(first, 3)", "number").replace("    out = byte_at", "    let text = \"old\\n\"\n    let first = text\n    out = byte_at");
    check(&src, b"10\n", b"", 0);
    let relay = source(r#""call\né""#, "text").split("rule probe").nth(1).unwrap().to_string();
    let src = source("relay(i)", "text").replace("reads: []", "reads: [i]").replace("calls: []", "calls: [relay]") + "\nrule relay" + &relay;
    check(&src, "call\né\n".as_bytes(), b"", 0);
    let callee = source(r#"Err("a\nb")"#, "Result(number, text)").split("rule probe").nth(1).unwrap().to_string();
    let src = source("match_result(fetch_result(i), value => value, error => length(error))", "number")
        .replace("reads: []", "reads: [i]").replace("calls: []", "calls: [fetch_result]") + "\nrule fetch_result" + &callee;
    check(&src, b"3\n", b"", 0);
    check(include_str!("../tests/fixtures/text_escapes_record.verbose"), b"10\n", b"", 0);
    check(include_str!("../tests/fixtures/text_escapes_variant.verbose"), b"10\n", b"", 0);
    // Exercise every word alignment and compare the entire transformed data
    // image with an independent lexer-based oracle, including metadata/comments.
    for alignment in 0..4 {
        let src = source(r#"concat("a\n", "\t", "é\"", "\\n")"#, "text")
            .replace("Preserve UTF-8 source bytes", r#"Preserve \"UTF-8\" source bytes"#);
        let src = format!("-- {} comment has \\q and \"quotes\"\n{src}", " ".repeat(alignment));
        let bytes = check(&src, "a\n\té\"\\n\n".as_bytes(), b"", 0);
        let mut expected = src.as_bytes().to_vec();
        for token in Lexer::new(&src).tokenize().unwrap() {
            let crate::lexer::TokenKind::StringLit(decoded) = token.kind else { continue };
            // The self-hosted tokenizer discards attribute lines. Validate
            // their grammar separately but leave their unused storage raw.
            if src.lines().nth(token.line - 1).unwrap().trim_start().starts_with('@') { continue; }
            let start = src.split_inclusive('\n').take(token.line - 1).map(str::len).sum::<usize>() + token.col;
            let mut end = start;
            while src.as_bytes()[end] != b'"' {
                end += if src.as_bytes()[end] == b'\\' { 2 } else { 1 };
            }
            expected[start..end].fill(0);
            expected[start..start + decoded.len()].copy_from_slice(decoded.as_bytes());
        }
        expected.resize((expected.len() + 3) & !3, 0);
        assert!(bytes.ends_with(&expected), "constant-data offsets/alignment: {alignment}");
    }
    for bad in [r#""bad\q""#, r#""bad\x41""#, r#""bad\u00e9""#, r#""bad\é""#, "\"bad", "\"bad\\", "\"bad\\\""] {
        let src = source(bad, "text");
        assert!(Lexer::new(&src).tokenize().is_err());
        let r = emit(&src);
        assert_eq!((r.status.code(), r.stdout), (Some(1), vec![]), "invalid text: {bad}");
        // Even an unused invalid rule must block the whole artifact.
        let unused = src.replacen("@verbose 0.1.0\n", "", 1).replace("concept Input", "concept Unused").replace("rule probe", "rule unused");
        let r = emit(&(source("1", "number") + &unused));
        assert_eq!((r.status.code(), r.stdout), (Some(1), vec![]), "unused invalid text: {bad}");
    }
    for bad in [r#"bad\q"#, r#"bad\x41"#, "bad\\", "bad\n"] {
        let r = emit(&source("1", "number").replace("UTF-8 input", bad));
        assert_eq!((r.status.code(), r.stdout), (Some(1), vec![]), "invalid metadata: {bad:?}");
    }
}
