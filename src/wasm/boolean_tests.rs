//! Execute source and optimized modules, including observable skipped work.
use super::*;
use crate::{
    interpreter::{self, Value},
    lexer::Lexer,
    optimizer,
    parser::Parser,
    verifier,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

const RUNTIME: &str = include_str!("../../tests/support/wasm_boolean.js");

fn source(body: &str, kind: &str, calls: &str) -> String {
    let mut reads: Vec<_> = ["i.s", "i.n"]
        .into_iter()
        .filter(|name| body.contains(name))
        .collect();
    if body.contains("helper(i)") {
        reads.push("i");
    }
    let reads = reads.join(", ");
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Boolean evaluation input"
  @source: invoices.intent:1
  fields:
    s : text
    n : number
rule probe
  @intention: "Evaluate a guarded expression"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : {kind}
  logic:
{body}
  proofs:
    purity:
      reads: [{reads}]
      calls: [{calls}]
    termination:
      bound: 1000
"#
    )
}

fn parse(src: &str) -> Program {
    Parser::new(Lexer::new(src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

fn checked(src: &str) -> Program {
    let p = parse(src);
    let errors = verifier::verify_program(&p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}\n{src}");
    p
}

fn expected(p: &Program, text: &str, number: i64) -> String {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| match i {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    match interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([
            ("s".into(), Value::Text(text.into())),
            ("n".into(), Value::Number(number)),
        ]),
    ) {
        Ok(Value::Bool(b)) => (b as u8).to_string(),
        Ok(Value::Number(n)) => format!("{n}n"),
        Err(_) => "'trap'".into(),
        other => panic!("unexpected interpreter result: {other:?}"),
    }
}

struct Suite {
    dir: PathBuf,
    next: usize,
    script: String,
}

impl Suite {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "verbose-wasm-bool-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            next: 0,
            script: RUNTIME.into(),
        }
    }

    fn modules(&mut self, p: &Program) -> [PathBuf; 2] {
        [p.clone(), optimizer::optimize_program(p).0].map(|emitted| {
            let path = self.dir.join(format!("{}.wasm", self.next));
            self.next += 1;
            compile_wasm(&emitted, "probe", path.to_str().unwrap()).unwrap();
            path
        })
    }

    fn compare(&mut self, p: &Program, inputs: &[(&str, i64)]) {
        for path in self.modules(p) {
            for &(s, n) in inputs {
                self.script.push_str(&format!(
                    "\ncheck({path:?}, {s:?}, {n}n, {});",
                    expected(p, s, n)
                ));
            }
        }
    }

    fn run(&self) {
        match Command::new("node").arg("--version").output() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "node unavailable: skipping WASM runtime assertions (required by normal CI)"
                );
                return;
            }
            Ok(out) => assert!(out.status.success(), "{out:?}"),
            Err(e) => panic!("cannot start node: {e}"),
        }
        let script = format!("{}\nconsole.log('ok');", self.script);
        let out = Command::new("node")
            .args(["-e", &script, "probe"])
            .output()
            .unwrap();
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), b"ok\n".to_vec(), vec![])
        );
    }
}

impl Drop for Suite {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn wasm_boolean_truth_tables_nested_negation_and_text() {
    let mut suite = Suite::new();
    let inputs = [
        ("", 0),
        ("a", 0),
        ("", 1),
        ("a", 1),
        ("\0", -1),
        ("é🦀", i64::MIN),
        ("é🦀", i64::MAX),
    ];
    for expr in [
        "i.n > 0 and length(i.s) > 0",
        "i.n > 0 or length(i.s) > 0",
        "not (i.n > 0)",
        "not not (i.n > 0)",
        "not (i.n > 0 and not (length(i.s) > 0))",
        "(i.n > 0 or i.n == 0) and (length(i.s) == 0 or not (i.n < 0))",
        "if i.n > 0 and length(i.s) > 0 then not (i.n == 1) else i.n < 0 or length(i.s) == 0",
        "starts_with(i.s, \"é\") and not ends_with(i.s, \"a\")",
        "contains(i.s, \"🦀\") or starts_with(i.s, \"a\")",
    ] {
        suite.compare(
            &checked(&source(&format!("    out = {expr}"), "bool", "")),
            &inputs,
        );
        suite.compare(
            &checked(&source(
                &format!("    out = if {expr} then 7 else 9"),
                "number",
                "",
            )),
            &inputs,
        );
    }
    suite.compare(&checked(&source(
        "    let left = i.n > 0\n    let alias = left\n    let right = not (length(i.s) == 0)\n    out = alias and right or not left",
        "bool", "")), &inputs);
    suite.run();
}

#[test]
fn wasm_boolean_skips_only_unrequired_failures() {
    let mut suite = Suite::new();
    for rhs in ["10 / i.n > 0", "10 % i.n > 0", "parse_int(i.s) > 0"] {
        for expr in [
            format!("i.n != 0 and ({rhs})"),
            format!("i.n == 0 or ({rhs})"),
            format!("i.n == 0 and ({rhs})"),
            format!("i.n != 0 or ({rhs})"),
            format!("({rhs}) and i.n != 0"),
            format!("({rhs}) or i.n == 0"),
            format!("not (i.n != 0 and ({rhs}))"),
        ] {
            suite.compare(
                &checked(&source(&format!("    out = {expr}"), "bool", "")),
                &[("", 0), ("12", 1), ("-2", -1), ("x", 2)],
            );
        }
        suite.compare(
            &checked(&source(
                &format!("    let eager = {rhs}\n    out = i.n == 0 or eager"),
                "bool",
                "",
            )),
            &[("", 0), ("x", 1)],
        );
    }
    for rhs in [
        "1 / 0 > 0",
        "1 % 0 > 0",
        "(-9223372036854775807 - 1) / -1 > 0",
    ] {
        for (lhs, op, expected) in [
            ("1 == 1", "or", "1"),
            ("1 == 2", "and", "0"),
            ("1 == 1", "and", "'trap'"),
            ("1 == 2", "or", "'trap'"),
        ] {
            let p = checked(&source(
                &format!("    out = {lhs} {op} ({rhs})"),
                "bool",
                "",
            ));
            for path in suite.modules(&p) {
                suite
                    .script
                    .push_str(&format!("\ncheck({path:?}, '', 0n, {expected});"));
            }
        }
    }
    // WebAssembly rem_s deliberately differs from x86 idiv at MIN % -1.
    let p = checked(&source(
        "    out = i.n == 0 and (-9223372036854775807 - 1) % -1 == 0",
        "bool",
        "",
    ));
    for path in suite.modules(&p) {
        suite
            .script
            .push_str(&format!("\ncheck({path:?}, '', 0n, 1);"));
    }
    suite.run();
}

#[test]
fn wasm_boolean_composes_with_supported_calls_and_results() {
    let mut suite = Suite::new();
    let helper = source("    out = not (10 / i.n < 0)", "bool", "");
    let helper = helper
        .split("rule probe")
        .nth(1)
        .unwrap()
        .replace("i : Input", "value : Input")
        .replace("i.n", "value.n")
        .replace("i.s", "value.s");
    let src = format!(
        "{}\nrule helper{helper}",
        source("    out = i.n == 0 or helper(i)", "bool", "helper")
    );
    suite.compare(
        &checked(&src),
        &[("", 0), ("", 1), ("", -1), ("", i64::MIN), ("", i64::MAX)],
    );
    let p = checked(&source(
        "    out = if not (i.n != 0 and 10 / i.n < 0) then Ok(7) else Err(\"bad\")",
        "Result(number, text)",
        "",
    ));
    for path in suite.modules(&p) {
        suite.script.push_str(&format!(r#"
for (const n of [0n, 1n, -1n]) {{
    const e = load({path:?});
    const value = invoke({path:?}, '', n);
    assert.equal(value[0], n < 0n ? 1 : 0);
    if (value[0] === 0) assert.equal(value[1], 7n);
    else assert.equal(new TextDecoder().decode(new Uint8Array(e.memory.buffer, value[2], value[3])), 'bad');
}}
"#));
    }
    suite.run();
}

#[test]
fn wasm_boolean_evaluates_operands_once_in_source_order() {
    let mut suite = Suite::new();
    for op in ["and", "or"] {
        let p = checked(&source(&format!("    let flag = length(concat(i.s, \"L\")) > i.n {op} length(concat(i.s, \"R\")) > 0\n    out = concat(i.s, \"Z\")"), "text", ""));
        for path in suite.modules(&p) {
            // Three one-byte literals occupy [1024,1027); the existing
            // allocator starts at the next 16-byte boundary, 1040.
            // Output bytes and pointer expose duplicate or reordered work.
            suite.script.push_str(&format!(
                r#"
for (const n of [0n, 4n]) {{
    const e = load({path:?});
    const memory = new Uint8Array(e.memory.buffer);
    memory.fill(0, 1040, 2048);
    const [ptr, len] = invoke({path:?}, 'é', n);
    const rhs = {op:?} === 'and' ? n === 0n : n === 4n;
    const trace = encoder.encode(rhs ? 'éLéRéZ' : 'éLéZ');
    assert.deepEqual(memory.slice(1040, 1040 + trace.length), trace);
    assert.equal(memory[1040 + trace.length], 0);
    assert.equal(ptr, 1040 + trace.length - 3);
    assert.equal(len, 3);
}}
"#
            ));
        }
    }
    suite.run();
}

#[test]
fn wasm_boolean_keeps_source_and_backend_refusals() {
    for expr in [
        "1 or i.n > 0",
        "i.n > 0 and \"yes\"",
        "not i.n",
        "1 == 1 or missing(i)",
    ] {
        assert!(
            !verifier::verify_program(
                &parse(&source(&format!("    out = {expr}"), "bool", "")),
                Path::new("examples")
            )
            .is_empty(),
            "{expr}"
        );
    }
    let src = source("    out = i.n == 0 or parse_int(i.s) > 0", "bool", "")
        .replace("reads: [i.s, i.n]", "reads: [i.n]");
    assert!(!verifier::verify_program(&parse(&src), Path::new("examples")).is_empty());
    let suite = Suite::new();
    let path = suite.dir.join("refused.wasm");
    let p = checked(&source(
        "    out = i.n == 0 or byte_at(i.s, 0) > 0",
        "bool",
        "",
    ));
    for emitted in [p.clone(), optimizer::optimize_program(&p).0] {
        fs::write(&path, b"preserve me").unwrap();
        let err = compile_wasm(&emitted, "probe", path.to_str().unwrap()).unwrap_err();
        assert!(err.message.contains("unsupported expression"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), b"preserve me");
    }
}

fn uleb(bytes: &[u8], pos: &mut usize) -> usize {
    let mut n = 0;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        n |= ((b & 127) as usize) << shift;
        if b < 128 {
            return n;
        }
        shift += 7;
    }
}

#[test]
fn wasm_boolean_scalar_lowering_adds_no_storage_or_imports() {
    let mut suite = Suite::new();
    for (expr, body_len) in [
        ("i.n > 0 and i.n < 2", 22),
        ("i.n > 0 or i.n < 2", 22),
        ("not (i.n > 0)", 11),
    ] {
        let src = source(&format!("    out = {expr}"), "bool", "")
            .replace("    s : text\n", "")
            .replace("reads: [i.s, i.n]", "reads: [i.n]");
        for path in suite.modules(&checked(&src)) {
            let bytes = fs::read(&path).unwrap();
            let mut pos = 8;
            let mut found = false;
            while pos < bytes.len() {
                let section = bytes[pos];
                pos += 1;
                let len = uleb(&bytes, &mut pos);
                let end = pos + len;
                assert!(
                    ![2, 5, 6, 11].contains(&section),
                    "unexpected imports/memory/globals/data"
                );
                if section == 10 {
                    assert_eq!(uleb(&bytes, &mut pos), 1); // only the rule
                    assert_eq!(uleb(&bytes, &mut pos), body_len);
                    assert_eq!(uleb(&bytes, &mut pos), 0); // no locals
                    found = true;
                }
                pos = end;
            }
            assert!(found);
            suite.script.push_str(&format!(
                "\nassert.equal(typeof load({path:?}).probe(1n), 'number');"
            ));
        }
    }
    suite.run();
}
