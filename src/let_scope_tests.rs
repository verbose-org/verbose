//! Differential regressions for the text-literal propagation environment.
use crate::{ast::*, interpreter::{self, Value}, lexer::Lexer, optimizer, parser::Parser, verifier};
use std::{collections::HashMap, path::Path};

fn parse_program(body: &str, output: &str) -> Program {
    let reads = ["i.s", "i.n", "i.items"].into_iter().filter(|f| body.contains(f)).collect::<Vec<_>>().join(", ");
    let extra = if body.contains("i.items") { "    items : collection(number)\n" } else { "" };
    let src = format!(r#"@verbose 0.1.0
concept Input
  @intention: "Inputs for lexical scope regressions"
  @source: invoices.intent:1
  fields:
    s : text
    n : number
{extra}rule probe
  @intention: "Preserve source-order binding values through optimization"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : {output}
  logic:
{body}
  proofs:
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 100
"#);
    Parser::new(Lexer::new(&src).tokenize().unwrap()).parse_program().unwrap()
}

fn program(body: &str, output: &str) -> Program {
    let p = parse_program(body, output);
    let errors = verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{body}\n{errors:?}");
    p
}

fn eval(p: &Program, text: &str, n: i64) -> Result<Value, String> {
    let rules: Vec<_> = p.items.iter().filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None }).collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let input = HashMap::from([
        ("s".into(), Value::Text(text.into())), ("n".into(), Value::Number(n)),
        ("items".into(), Value::List(vec![Value::Number(1), Value::Number(2), Value::Number(3)])),
    ]);
    interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).map_err(|e| e.message)
}

type Case = (&'static str, &'static str, fn(&str, i64) -> Value);
fn cases() -> Vec<Case> {
    vec![
        ("    let x = \"old\"\n    let first = x\n    let x = \"new\"\n    out = concat(first, x)", "text", |_, _| Value::Text("oldnew".into())),
        ("    let x = \"é\\n\"\n    let first = x\n    let x = \"€\"\n    let second = x\n    let x = \"🦀\"\n    out = concat(first, second, x)", "text", |_, _| Value::Text("é\n€🦀".into())),
        ("    let x = \"old\"\n    let first = x\n    let x = i.s\n    out = concat(first, x)", "text", |s, _| Value::Text(format!("old{s}"))),
        ("    let x = \"old\"\n    let x = i.n\n    out = x", "number", |_, n| Value::Number(n)),
        ("    let x = \"abc\"\n    let x = length(x)\n    out = x", "number", |_, _| Value::Number(3)),
        ("    let x = \"old\"\n    let x = x\n    out = x", "text", |_, _| Value::Text("old".into())),
        ("    let x = \"old\"\n    let y = x\n    let x = y\n    let y = \"new\"\n    out = concat(x, y)", "text", |_, _| Value::Text("oldnew".into())),
        ("    let x = \"old\"\n    let first = x\n    let x = \"new\"\n    out = if i.n > 0 then byte_at(first, 0) else byte_at(x, 0)", "number", |_, n| Value::Number(if n > 0 { 111 } else { 110 })),
        ("    let x = i.s\n    let first = length(x)\n    let x = \"new\"\n    out = first + length(x)", "number", |s, _| Value::Number(s.len() as i64 + 3)),
    ]
}

#[test]
fn text_let_shadowing_preserves_interpretation() {
    for (body, output, expected) in cases() {
        let p = program(body, output);
        let optimized = optimizer::optimize_program(&p).0;
        for (text, n) in [("é\n🦀", 7), ("Z", -2)] {
            let expected = Ok(expected(text, n));
            assert_eq!(eval(&p, text, n), expected, "original: {body}");
            assert_eq!(eval(&optimized, text, n), expected, "optimized: {body}");
        }
    }
}

#[test]
fn text_let_shadowing_respects_nested_binders_and_eager_failures() {
    for (expr, expected) in [
        ("sum(i.items, x => x) + length(x)", 7),
        ("fold(i.items, length(x), x, item => x + item) + length(x)", 8),
        ("fold_bytes(\"ab\", 0, acc, x, idx => acc + x) + length(x)", 196),
        ("match_result(Err(\"inner\"), value => 0, x => length(x)) + length(x)", 6),
    ] {
        let p = program(&format!("    let x = \"old\"\n    let x = \"X\"\n    out = {expr}"), "number");
        assert_eq!(eval(&p, "", 0), Ok(Value::Number(expected)), "original: {expr}");
        assert_eq!(eval(&optimizer::optimize_program(&p).0, "", 0), Ok(Value::Number(expected)), "optimized: {expr}");
    }
    let p = program("    let x = \"old\"\n    let x = parse_int(i.s)\n    let x = \"new\"\n    out = x", "text");
    let original = eval(&p, "invalid", 0);
    assert!(original.is_err(), "the overwritten binding still evaluates eagerly");
    assert_eq!(eval(&optimizer::optimize_program(&p).0, "invalid", 0), original);
}

#[test]
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
fn text_let_shadowing_native_matches_source_values() {
    use std::{fs, process::Command};
    let path = std::env::temp_dir().join(format!("verbose-let-scope-{}", std::process::id()));
    for (body, output, expected) in cases() {
        let p = optimizer::optimize_program(&program(body, output)).0;
        crate::native::compile_native(&p, "probe", path.to_str().unwrap(), false, false)
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        for (text, n) in [("é\n🦀", 7), ("Z", -2)] {
            let r = Command::new(&path).args([text, &n.to_string()]).output().unwrap();
            let bytes = match expected(text, n) {
                Value::Text(s) => format!("{s}\n").into_bytes(),
                Value::Number(n) => format!("{n}\n").into_bytes(),
                _ => unreachable!(),
            };
            assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(0), bytes, vec![]), "{body}");
        }
    }
    let p = optimizer::optimize_program(&program("    let x = \"old\"\n    let x = parse_int(i.s)\n    let x = \"new\"\n    out = x", "text")).0;
    crate::native::compile_native(&p, "probe", path.to_str().unwrap(), false, false).unwrap();
    let r = Command::new(&path).args(["invalid", "0"]).output().unwrap();
    assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(1), vec![], vec![]));
    fs::remove_file(path).unwrap();
}

#[test]
fn text_let_shadowing_wasm_matches_source_values() {
    use std::{fs, process::Command};
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("node unavailable; skipping WASM execution");
        return;
    }
    let path = std::env::temp_dir().join(format!("verbose-let-scope-{}.wasm", std::process::id()));
    for (body, output, expected) in cases() {
        let p = optimizer::optimize_program(&program(body, output)).0;
        crate::wasm::compile_wasm(&p, "probe", path.to_str().unwrap())
            .unwrap_or_else(|e| panic!("{body}: {e}"));
        let script = r#"
const fs = require('fs');
(async () => {
  const {instance: {exports: e}} = await WebAssembly.instantiate(fs.readFileSync(process.argv[1]));
  const text = new TextEncoder().encode(process.argv[2]);
  new Uint8Array(e.memory.buffer).set(text, 4096);
  const result = e.probe(4096, text.length, BigInt(process.argv[3]));
  if (Array.isArray(result)) {
    process.stdout.write(Buffer.from(new Uint8Array(e.memory.buffer).subarray(result[0], result[0] + result[1])));
  } else process.stdout.write(String(result));
})().catch(e => { console.error(e); process.exit(1); });
"#;
        for (text, n) in [("é\n🦀", 7), ("Z", -2)] {
            let r = Command::new("node").args(["-e", script, path.to_str().unwrap(), text, &n.to_string()]).output().unwrap();
            let bytes = match expected(text, n) {
                Value::Text(s) => s.into_bytes(), Value::Number(n) => n.to_string().into_bytes(), _ => unreachable!(),
            };
            assert_eq!((r.status.code(), r.stdout, r.stderr), (Some(0), bytes, vec![]), "{body}");
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn text_let_shadowing_checks_rhs_types_in_source_order() {
    for (body, output, needle) in [
        ("    let x = 7\n    let y = length(x)\n    let x = \"abc\"\n    out = y", "number", "has type 'number' but context expects 'text'"),
        ("    let x = \"abc\"\n    let y = x + 1\n    let x = 7\n    out = y", "number", "has type 'text' but context expects 'number'"),
        ("    let x = \"abc\"\n    let x = x\n    out = x + 1", "number", "has type 'text' but context expects 'number'"),
    ] {
        let p = parse_program(body, output);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.iter().any(|e| e.message.contains(needle)), "{body}\n{errors:?}");
    }
}
