//! Counted-byte equality through actual source and optimized WASM modules.
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

fn rule(name: &str, input: &str, kind: &str, body: &str, calls: &[&str]) -> String {
    let mut reads = ["a", "b", "n"]
        .map(|f| format!("{input}.{f}"))
        .into_iter()
        .filter(|f| body.contains(f))
        .collect::<Vec<_>>();
    if !calls.is_empty() {
        reads.push(input.into());
    }
    format!(
        r#"
rule {name}
  @intention: "Compare counted text"
  @source: invoices.intent:1
  input:
    {input} : Input
  output:
    out : {kind}
  logic:
{body}
  proofs:
    purity:
      reads: [{}]
      calls: [{}]
    termination:
      bound: 1000
"#,
        reads.join(", "),
        calls.join(", ")
    )
}
fn source(kind: &str, body: &str, helpers: &[(&str, &str, &str, &str, &[&str])]) -> String {
    let mut src = String::from(
        r#"@verbose 0.1.0
concept Input
  @intention: "Counted text input"
  @source: invoices.intent:1
  fields:
    a : text
    b : text
    n : number
"#,
    );
    let calls = helpers
        .iter()
        .filter(|h| body.contains(&format!("{}(", h.0)))
        .map(|h| h.0)
        .collect::<Vec<_>>();
    src.push_str(&rule("probe", "i", kind, body, &calls));
    for (name, input, kind, body, calls) in helpers {
        src.push_str(&rule(name, input, kind, body, calls));
    }
    src
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
fn expected(p: &Program, a: &str, b: &str, n: i64) -> String {
    let rules = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect::<Vec<_>>();
    let concepts = iter_all_concepts(&p.items).collect::<Vec<_>>();
    fn js(v: Value) -> String {
        match v {
            Value::Bool(b) => (b as u8).to_string(),
            Value::Number(n) => format!("{n}n"),
            Value::Text(t) => format!("{:?}", t.as_bytes()),
            Value::Ok(v) => format!("[0, {}]", js(*v)),
            Value::Err(v) => format!("[1, {}]", js(*v)),
            other => panic!("unexpected {other:?}"),
        }
    }
    match interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([
            ("a".into(), Value::Text(a.into())),
            ("b".into(), Value::Text(b.into())),
            ("n".into(), Value::Number(n)),
        ]),
    ) {
        Ok(v) => js(v),
        Err(_) => "'trap'".into(),
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
            "verbose-wasm-eq-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self {
            dir,
            next: 0,
            script: format!(
                "{}\n{}",
                include_str!("../../tests/support/wasm_boolean.js"),
                r#"
function invoke2(path, a, b, n) {
    const e = load(path), memory = new Uint8Array(e.memory.buffer);
    memory.set(a, 60000); memory.set(b, 62000);
    const result = e.probe(60000, a.length, 62000, b.length, n);
    if (!Array.isArray(result)) return result;
    const bytes = (ptr, len) => Array.from(memory.subarray(ptr, ptr + len));
    if (result.length === 2) return bytes(...result);
    if (result.length === 4) return result[0] === 0 ? [0, result[1]] : [1, bytes(result[2], result[3])];
    return result[0] === 0 ? [0, bytes(result[1], result[2])] : [1, bytes(result[3], result[4])];
}
function check2(path, a, b, n, expected) {
    load(path);
    if (expected === 'trap') assert.throws(() => invoke2(path, a, b, n), WebAssembly.RuntimeError);
    else assert.deepEqual(invoke2(path, a, b, n), expected, `${path}: ${a}; ${b}; ${n}`);
}
"#
            ),
        }
    }
    fn modules(&mut self, p: &Program) -> [PathBuf; 2] {
        [p.clone(), optimizer::optimize_program(p).0].map(|p| {
            let path = self.dir.join(format!("{}.wasm", self.next));
            self.next += 1;
            compile_wasm(&p, "probe", path.to_str().unwrap()).unwrap();
            path
        })
    }
    fn compare(&mut self, p: &Program, inputs: &[(&str, &str, i64)]) {
        for path in self.modules(p) {
            for &(a, b, n) in inputs {
                self.script.push_str(&format!(
                    "\ncheck2({path:?}, {:?}, {:?}, {n}n, {});",
                    a.as_bytes(),
                    b.as_bytes(),
                    expected(p, a, b, n)
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
            Err(e) => panic!("{e}"),
        }
        let script = self.dir.join("runtime.js");
        fs::write(&script, format!("{}\nconsole.log('ok');", self.script)).unwrap();
        let out = Command::new("node").arg(script).output().unwrap();
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
fn wasm_text_equality_byte_matrix() {
    let mut s = Suite::new();
    let cases = [
        ("", "", 0),
        ("", "a", 0),
        ("a", "", 0),
        ("abc", "abc", 0),
        ("abc", "abcd", 0),
        ("abcd", "abc", 0),
        ("xbc", "abc", 0),
        ("abc", "abx", 0),
        ("a\0b", "a\0c", 0),
        ("a\0b", "a\0b", 0),
        ("\0", "", 0),
        ("\0", "\0", 0),
        ("é🦀中", "é🦀中", 0),
        ("é", "e\u{301}", 0),
        ("é", "è", 0),
        ("🦀", "🦁", 0),
    ];
    for expr in [
        "i.a == i.b",
        "i.a != i.b",
        "i.a == \"é🦀中\"",
        "\"a\0b\" != i.a",
        "\"é\" == \"é\"",
        "\"a\0b\" == \"a\0c\"",
    ] {
        s.compare(
            &checked(&source("bool", &format!("    out = {expr}"), &[])),
            &cases,
        );
    }
    s.run();
}

#[test]
fn wasm_text_equality_lexical_lets_and_composition() {
    let mut s = Suite::new();
    let inputs = [
        ("", "", 0),
        ("42", "42", 42),
        ("x\0y", "x\0z", -1),
        ("é", "é", i64::MAX),
    ];
    for body in [
        "    let x = i.a\n    let a = x\n    let x = i.b\n    out = a == x",
        "    let x = i.a\n    let a = x\n    let x = i.n\n    out = a == i.b and x == i.n",
        "    let x = i.n\n    let a = concat(x)\n    let x = i.a\n    out = a == x",
        "    let x = i.a\n    let same = x == i.b\n    let alias = same\n    let same = 1 == 2\n    out = not same and (alias or x != i.b)",
        "    out = if i.a == i.b then not (i.a != i.b) else i.a != i.b",
        "    out = (i.a == i.b) and starts_with(i.a, i.b)",
        "    out = json_escape(i.a) == json_escape(i.b)",
        "    out = concat(length(i.a), i.a) != concat(length(i.b), i.b)",
    ] { s.compare(&checked(&source("bool", body, &[])), &inputs); }
    s.run();
}

#[test]
fn wasm_text_equality_calls_and_result_binders() {
    let mut s = Suite::new();
    let cases = [
        ("", "", 0),
        ("é\0", "é\0", 2),
        ("a", "b", -1),
        ("22", "22", 22),
    ];
    let helpers = [
        (
            "label",
            "other",
            "text",
            "    out = json_escape(other.a)",
            &[][..],
        ),
        (
            "equal",
            "renamed",
            "bool",
            "    out = label(renamed) == json_escape(renamed.b)",
            &["label"][..],
        ),
    ];
    s.compare(
        &checked(&source(
            "bool",
            "    out = equal(i) and label(i) != \"bad\"",
            &helpers,
        )),
        &cases,
    );
    // A text alias of a call result must get a pointer/length pair.
    s.compare(
        &checked(&source(
            "bool",
            "    let x = label(i)\n    let y = x\n    out = y == json_escape(i.b)",
            &helpers[..1],
        )),
        &cases,
    );
    s.compare(
        &checked(&source(
            "bool",
            "    out = literal(i) == i.a",
            &[("literal", "arg", "text", "    out = \"é\0\"", &[])],
        )),
        &cases,
    );
    s.compare(
        &checked(&source(
            "bool",
            "    out = formatted(i) == i.b",
            &[(
                "formatted",
                "arg",
                "text",
                "    out = concat(max(arg.n, parse_int(arg.a)))",
                &[],
            )],
        )),
        &cases,
    );
    let helper = [(
        "get",
        "arg",
        "Result(text, text)",
        "    out = if arg.n > 0 then Ok(arg.a) else Err(arg.b)",
        &[][..],
    )];
    s.compare(&checked(&source("Result(text, text)", "    let v = i.n\n    out = match_result(get(i), v => if v == i.b then Ok(v) else Err(\"different\"), v => if v != i.a then Err(v) else Ok(v))", &helper)), &cases);
    let helper = [(
        "get",
        "arg",
        "Result(number, text)",
        "    out = if arg.a == arg.b then Ok(arg.n) else Err(arg.a)",
        &[][..],
    )];
    s.compare(
        &checked(&source(
            "Result(number, text)",
            "    out = match_result(get(i), v => Ok(v), e => if e == i.a then Ok(7) else Err(e))",
            &helper,
        )),
        &cases,
    );
    // The number binder hides an outer text binding; planning must still
    // reserve the formatter used while evaluating a comparison operand.
    s.compare(&checked(&source("Result(number, text)",
        "    let v = i.a\n    out = match_result(get(i), v => if concat(v) == i.b then Ok(v) else Err(\"different\"), e => Err(e))",
        &helper)), &cases);
    s.run();
}

#[test]
fn wasm_text_equality_eager_failures_and_guards() {
    let mut s = Suite::new();
    for body in [
        "    out = concat(parse_int(i.a)) == i.b",
        "    out = i.b != concat(parse_int(i.a))",
        "    out = i.n == 0 or concat(parse_int(i.a)) == i.b",
        "    out = i.n != 0 and i.b != concat(parse_int(i.a))",
        "    let eager = concat(parse_int(i.a)) == i.b\n    out = i.n == 0 or eager",
        "    out = concat(10 / i.n) == i.b",
    ] {
        s.compare(
            &checked(&source("bool", body, &[])),
            &[("bad", "", 0), ("bad", "", 1), ("42", "42", 2)],
        );
    }
    s.run();
}

#[test]
fn wasm_text_equality_counts_operand_evaluation_and_checks_reads() {
    let mut s = Suite::new();
    let p = checked(&source("text", "    let same = concat(i.a, \"L\") == concat(i.b, \"R\")\n    out = concat(if same then 1 else 0)", &[]));
    for path in s.modules(&p) {
        s.script.push_str(&format!(
            r#"
{{
    const e = load({path:?}); const memory = new Uint8Array(e.memory.buffer);
    memory.set([65], 60000); memory.set([66, 67], 62000);
    const [ptr, len] = e.probe(60000, 1, 62000, 2, 0n);
    // The allocator begins after the two literal bytes, aligned to 16.
    assert.equal(ptr, 1045); assert.equal(len, 1);
    assert.deepEqual(Array.from(memory.subarray(1040, ptr + len)), [65, 76, 66, 67, 82, 48]);
}}
"#
        ));
    }
    for op in ["==", "!="] {
        let p = checked(&source("bool", &format!("    out = i.a {op} i.b"), &[]));
        let yes = if op == "==" { 1 } else { 0 };
        for path in s.modules(&p) {
            s.script.push_str(&format!(
                r#"
{{
    const e = load({path:?});
    assert.equal(e.probe(65536, 0, -1, 0, 0n), {yes});
    assert.equal(e.probe(-1, 1, -1, 0, 0n), 1 - {yes});
    assert.throws(() => e.probe(65536, 1, 65536, 1, 0n), WebAssembly.RuntimeError);
    assert.throws(() => e.probe(0, -1, 0, -1, 0n), WebAssembly.RuntimeError);
    new Uint8Array(e.memory.buffer)[65535] = 42;
    assert.equal(e.probe(65535, 1, 65535, 1, 0n), {yes});
}}
"#
            ));
        }
    }
    s.run();
}

#[test]
fn wasm_text_equality_refuses_unsupported_shapes_without_artifact() {
    let s = Suite::new();
    let path = s.dir.join("existing.wasm");
    for (src, diagnostic) in [
        (
            source(
                "bool",
                "    out = (if i.n > 0 then i.a else i.b) == i.a",
                &[],
            ),
            "text-valued conditionals",
        ),
        (
            source("text", "    out = if i.n > 0 then i.a else i.b", &[]),
            "text-valued conditionals",
        ),
        (
            source(
                "bool",
                "    out = helper(i) == i.a",
                &[("helper", "j", "text", "    let x = j.a\n    out = x", &[])],
            ),
            "with let bindings",
        ),
        (
            source(
                "bool",
                "    out = concat(helper(i)) == i.a",
                &[("helper", "j", "text", "    out = concat(j.a, \"x\")", &[])],
            ),
            "nested concat",
        ),
    ] {
        let p = checked(&src);
        fs::write(&path, b"preserve").unwrap();
        let e = compile_wasm(&p, "probe", path.to_str().unwrap()).unwrap_err();
        assert!(e.message.contains(diagnostic), "{e:?}");
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
    }
    // Invalid source is rejected before lowering or optimization can hide it.
    for expr in ["i.a == i.n", "i.a != (i.n > 0)", "1 == 1 or i.a == i.n"] {
        let p = parse(&source("bool", &format!("    out = {expr}"), &[]));
        assert!(!verifier::verify_program(&p, Path::new("examples")).is_empty());
    }
    // Direct backend callers also get a finite, named failure on recursion.
    let p = parse(&source(
        "bool",
        "    out = helper(i)",
        &[("helper", "j", "bool", "    out = helper(j)", &["helper"])],
    ));
    assert!(compile_wasm(&p, "probe", path.to_str().unwrap())
        .unwrap_err()
        .message
        .contains("recursive call"));
}

fn uleb(bytes: &[u8], pos: &mut usize) -> usize {
    let mut value = 0;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        value |= ((b & 127) as usize) << shift;
        if b < 128 {
            return value;
        }
        shift += 7;
    }
}

#[test]
fn wasm_text_equality_uses_only_shared_fixed_scratch() {
    let mut s = Suite::new();
    for (expr, locals) in [
        ("i.a == i.b", 6),
        ("i.a != i.b and i.b == i.a", 6),
        ("starts_with(i.a, i.b) and i.a == i.b", 6),
        ("i.n == 0", 0),
        ("i.n != 1", 0),
    ] {
        let p = checked(&source("bool", &format!("    out = {expr}"), &[]));
        for path in s.modules(&p) {
            let bytes = fs::read(&path).unwrap();
            let mut pos = 8;
            let mut found = false;
            while pos < bytes.len() {
                let section = bytes[pos];
                pos += 1;
                let len = uleb(&bytes, &mut pos);
                let end = pos + len;
                assert!(
                    ![2, 6, 11].contains(&section),
                    "unexpected imports/globals/data"
                );
                if section == 5 {
                    assert_eq!(&bytes[pos..end], &[1, 0, 1]);
                } // existing one page
                if section == 10 {
                    assert_eq!(uleb(&bytes, &mut pos), 1); // no helper function
                    let _body_len = uleb(&bytes, &mut pos);
                    assert_eq!(uleb(&bytes, &mut pos), usize::from(locals != 0));
                    if locals != 0 {
                        assert_eq!(uleb(&bytes, &mut pos), locals);
                        assert_eq!(bytes[pos], 0x7f);
                    }
                    found = true;
                }
                pos = end;
            }
            assert!(found);
        }
    }
    // Text producers require memory even when the input has only numbers.
    let src = source("bool", "    out = concat(i.n) == concat(i.n)", &[])
        .replace("    a : text\n    b : text\n", "");
    for path in s.modules(&checked(&src)) {
        s.script
            .push_str(&format!("\nassert.equal(load({path:?}).probe(42n), 1);"));
    }
    s.run();
}
