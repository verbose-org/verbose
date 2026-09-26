//! Lexical environments in the compiler written in Verbose, through gen1 too.
use crate::{
    ast::*,
    interpreter::{self, Value},
    lexer::Lexer,
    native, optimizer,
    parser::Parser,
    verifier,
};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn parse(src: &str) -> Program {
    Parser::new(Lexer::new(src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}

fn source(body: &str, ty: &str) -> String {
    let reads = ["i.s", "i.n"]
        .into_iter()
        .filter(|f| body.contains(f))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Inputs for lexical binding probes"
  @source: invoices.intent:1
  fields:
    s : text
    n : number
rule probe
  @intention: "Preserve lexical binding values and representation"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : {ty}
  logic:
{body}
  proofs:
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 200
"#
    )
}

fn emit(compiler: &Path, src: &str) -> std::process::Output {
    let mut child = Command::new(compiler)
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
    child.wait_with_output().unwrap()
}

enum NativeReference {
    Compare,
    Refuses(&'static str),
    // The parent Rust native emitter reuses the old BoundText classification.
    // See known-gaps.md; the original interpreter remains the value oracle.
    DynamicRebindingGap,
}

fn cases() -> Vec<(&'static str, &'static str, Value, NativeReference)> {
    vec![
        ("    let x = \"old\"\n    let first = x\n    let x = \"new\"\n    out = concat(first, x)", "text", Value::Text("oldnew".into()), NativeReference::Compare),
        ("    let x = \"é\\n\"\n    let a = x\n    let x = \"€\"\n    let b = x\n    let x = \"🦀\"\n    out = concat(a, b, x)", "text", Value::Text("é\n€🦀".into()), NativeReference::Compare),
        ("    let x = \"old\"\n    let x = x\n    let y = x\n    out = y", "text", Value::Text("old".into()), NativeReference::Compare),
        ("    let x = 3\n    let x = x + 1\n    let a = x\n    let x = x + 2\n    out = a * 10 + x", "number", Value::Number(46), NativeReference::Compare),
        ("    let x = \"abc\"\n    let x = length(x)\n    out = x", "number", Value::Number(3), NativeReference::Compare),
        ("    let x = \"abc\"\n    let a = x\n    let x = 7\n    out = concat(a, x)", "text", Value::Text("abc7".into()), NativeReference::Refuses("concat argument type not yet supported")),
        ("    let x = 7\n    let a = x\n    let x = \"abc\"\n    out = concat(a, x)", "text", Value::Text("7abc".into()), NativeReference::Refuses("concat argument type not yet supported")),
        ("    let x = \"old\"\n    let a = x\n    let x = i.s\n    let b = x\n    let x = \"new\"\n    out = concat(a, b, x)", "text", Value::Text("oldé\nnew".into()), NativeReference::Compare),
        ("    let x = i.s\n    let a = x\n    let x = i.n\n    out = concat(a, x)", "text", Value::Text("é\n7".into()), NativeReference::DynamicRebindingGap),
        ("    let x = \"same\"\n    let a = x\n    let x = \"else\"\n    out = if a == \"same\" then byte_at(x, 0) else 0", "number", Value::Number(101), NativeReference::Refuses("text literals not supported in native backend")),
        ("    let x = \"same\"\n    let a = x\n    let x = \"else\"\n    out = if a == x then 0 else length(a)", "number", Value::Number(4), NativeReference::Refuses("text literals not supported in native backend")),
        ("    let e = \"outer\"\n    let value = match_result(Err(\"x\"), ok => ok, e => length(e))\n    let e = \"new\"\n    out = value + length(e)", "number", Value::Number(4), NativeReference::Refuses("match_result target must be a rule call")),
        ("    let x = 100\n    let value = match_result(Ok(2), x => x + 1, e => 0)\n    let x = 20\n    out = value + x", "number", Value::Number(23), NativeReference::Refuses("match_result target must be a rule call")),
    ]
}

/// Used again by the fixed-point test with the compiler emitted by gen0.
pub(crate) fn assert_selfhost_bindings(compiler: &Path, base: &Path, evaluator: Option<&Path>) {
    let binary = base.join("bindings-probe");
    let reference = base.join("bindings-reference");
    for (body, ty, expected, native_refusal) in cases() {
        let src = source(body, ty);
        let p = parse(&src);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{src}\n{errors:?}");
        let rules: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
            .collect();
        let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
        let input = HashMap::from([
            ("s".into(), Value::Text("é\n".into())),
            ("n".into(), Value::Number(7)),
        ]);
        assert_eq!(
            interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).unwrap(),
            expected,
            "{src}"
        );
        let stdout = match &expected {
            Value::Text(s) => format!("{s}\n").into_bytes(),
            Value::Number(n) => format!("{n}\n").into_bytes(),
            _ => unreachable!(),
        };
        // eval_main has no input marshalling and its legacy text equality
        // coerces both spans to zero. Neither is an oracle for those forms.
        if let Some(evaluator) =
            evaluator.filter(|_| ty == "number" && !body.contains("i.") && !body.contains("=="))
        {
            let r = Command::new(evaluator).args([&src, "0"]).output().unwrap();
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(0), stdout.clone(), vec![]),
                "self-hosted evaluator: {src}"
            );
        }
        match native_refusal {
            NativeReference::DynamicRebindingGap => {}
            mode => {
                let compiled = native::compile_native(
                    &optimizer::optimize_program(&p).0,
                    "probe",
                    reference.to_str().unwrap(),
                    false,
                    false,
                );
                if let NativeReference::Refuses(needle) = mode {
                    let error =
                        compiled.expect_err(&format!("Rust native unexpectedly accepts: {src}"));
                    assert!(
                        error.message.contains(needle),
                        "Rust native refusal: {src}: {error}"
                    );
                } else {
                    compiled.unwrap_or_else(|e| panic!("Rust native: {src}: {e}"));
                    let r = Command::new(&reference)
                        .args(["é\n", "7"])
                        .output()
                        .unwrap();
                    assert_eq!(
                        (r.status.code(), r.stdout, r.stderr),
                        (Some(0), stdout.clone(), vec![]),
                        "Rust native: {src}"
                    );
                }
            }
        }
        assert_emitted(compiler, &src, &binary, &["é\n", "7"], stdout);
    }
    assert_aggregate_and_parameter_scopes(compiler, base, evaluator);

    // A long rule used to exhaust the fixed compiler arena by retaining every
    // copied binding view. Each view must die after its size/emission walk.
    let mut body = String::new();
    for i in 0..1800 {
        body.push_str(&format!("    let value_{i} = {i}\n"));
    }
    body.push_str("    out = value_1799");
    let src = source(&body, "number").replace("bound: 200", "bound: 4000");
    assert_emitted(compiler, &src, &binary, &["", "0"], b"1799\n".to_vec());
    for body in [
        "    let x = later\n    let later = 3\n    out = x",
        "    let x = x + 1\n    out = x",
        "    let x = \"text\"\n    let x = x\n    out = x + 1",
        "    let x = concat(\"fresh\", \"text\")\n    out = length(x)",
    ] {
        let src = source(body, "number");
        let r = emit(compiler, &src);
        assert_eq!(r.status.code(), Some(1), "must refuse: {src}");
        assert!(
            r.stdout.is_empty(),
            "unsupported program emitted artifact bytes: {src}"
        );
    }
    let src = source(
        "    let x = byte_at(\"a\", 3)\n    let x = 7\n    out = x",
        "number",
    );
    let r = emit(compiler, &src);
    assert!(r.status.success() && r.stdout.starts_with(b"\x7fELF"));
    fs::write(&binary, r.stdout).unwrap();
    let r = Command::new(&binary).args(["", "0"]).output().unwrap();
    assert_eq!(
        (r.status.code(), r.stdout, r.stderr),
        (Some(1), vec![], vec![]),
        "overwritten let must still fail eagerly"
    );
}

fn assert_emitted(compiler: &Path, src: &str, binary: &Path, args: &[&str], stdout: Vec<u8>) {
    let emitted = emit(compiler, src);
    assert!(
        emitted.status.success() && emitted.stdout.starts_with(b"\x7fELF"),
        "emit: {src}\n{:?}",
        emitted.stderr
    );
    assert_eq!(
        u64::from_le_bytes(emitted.stdout[96..104].try_into().unwrap()) as usize,
        emitted.stdout.len()
    );
    fs::write(binary, emitted.stdout).unwrap();
    fs::set_permissions(binary, fs::Permissions::from_mode(0o755)).unwrap();
    let r = Command::new(binary).args(args).output().unwrap();
    assert_eq!(
        (r.status.code(), r.stdout, r.stderr),
        (Some(0), stdout, vec![]),
        "self-hosted native: {src}"
    );
}

fn assert_aggregate_and_parameter_scopes(compiler: &Path, base: &Path, evaluator: Option<&Path>) {
    let record = source("    let x = Pair { first: 3, second: 7 }\n    let a = x\n    let x = Other { second: 9, first: 4 }\n    out = a.first * 100 + a.second * 10 + x.first", "number").replace("rule probe", r#"concept Pair
  @intention: "First field order"
  @source: invoices.intent:1
  fields:
    first : number
    second : number
concept Other
  @intention: "Second field order"
  @source: invoices.intent:1
  fields:
    second : number
    first : number
rule probe"#);
    let reduction = source(
        "    let x = 100\n    let n = sum(i.items, x => x)\n    let x = 7\n    out = n + x",
        "number",
    )
    .replace(
        "    s : text\n    n : number",
        "    items : collection(number)",
    )
    .replace("reads: []", "reads: [i.items]");
    let mut probes = vec![
        (
            record,
            Value::Number(374),
            HashMap::new(),
            vec!["", "0"],
            true,
        ),
        (
            reduction,
            Value::Number(13),
            HashMap::from([(
                "items".into(),
                Value::List(vec![Value::Number(1), Value::Number(2), Value::Number(3)]),
            )]),
            vec!["3", "1", "2", "3"],
            false,
        ),
    ];
    for (ty, arg, value, expected) in [
        ("number", "3", "4", Value::Number(4)),
        ("text", "\"old\"", "\"new\"", Value::Text("new".into())),
    ] {
        let mut src =
            source(&format!("    out = helper({arg})"), ty).replace("calls: []", "calls: [helper]");
        src.push_str(&format!(
            r#"rule helper
  @intention: "A local shadows an unused parameter"
  @source: invoices.intent:1
  input:
    x : {ty}
  output:
    out : {ty}
  logic:
    let x = {value}
    out = x
  proofs:
    purity:
      reads: []
      calls: []
    termination:
      bound: 200
"#
        ));
        probes.push((src, expected, HashMap::new(), vec!["", "0"], ty == "number"));
    }
    // These additional self-hosted frame/layout probes use the original AST
    // interpreter, independently of legacy Rust-native aggregate/call subsets.
    for (src, expected, input, args, scalar_eval) in probes {
        let p = parse(&src);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{src}\n{errors:?}");
        let rules: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
            .collect();
        let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
        assert_eq!(
            interpreter::eval_rule(rules[0], &rules, &concepts, &[], &input).unwrap(),
            expected,
            "{src}"
        );
        let stdout = format!("{expected}\n").into_bytes();
        if let Some(evaluator) = evaluator.filter(|_| scalar_eval) {
            let r = Command::new(evaluator).args([&src, "0"]).output().unwrap();
            assert_eq!(
                (r.status.code(), r.stdout, r.stderr),
                (Some(0), stdout.clone(), vec![]),
                "self-hosted evaluator: {src}"
            );
        }
        assert_emitted(compiler, &src, &base.join("scope-probe"), &args, stdout);
    }
}

#[test]
fn selfhost_lexical_bindings_match_reference() {
    let base =
        std::env::temp_dir().join(format!("verbose-selfhost-bindings-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let p = parse(&fs::read_to_string("examples/vexprparse.verbose").unwrap());
    let compiler = base.join("compiler");
    let evaluator = base.join("evaluator");
    native::compile_native_stdin_raw(&p, "elf_program_src", compiler.to_str().unwrap()).unwrap();
    native::compile_native(&p, "eval_main", evaluator.to_str().unwrap(), false, false).unwrap();
    assert_selfhost_bindings(&compiler, &base, Some(&evaluator));
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn scalar_arena_scope_reclaims_nodes_and_preserves_values() {
    // 100 nodes fit any single walk, but not the accumulated walks for n=20.
    // Reclamation must also preserve a live value allocated before the scope.
    let template = r#"@verbose 0.1.0

concept_group AST [max_depth: 30, max_nodes: 100]
  @intention: "tiny AST: Int and Add"
  @source: invoices.intent:1

  concept Expr
    @intention: "int literal or sum"
    @source: invoices.intent:1
    variants:
      Int of (value : number)
      Add of (lhs : Expr, rhs : Expr)


concept Seed
  @intention: "a small seed"
  @source: invoices.intent:1

  fields:
    n : number [0, 20]


rule build_chain
  @intention: "Build Add(Int(n), Add(Int(n-1), ... Int(0)))"
  @source: invoices.intent:1

  input:
    s : Seed

  output:
    e : Expr

  logic:
    e = if s.n == 0 then Expr::Int { value: 0 } else Expr::Add { lhs: Expr::Int { value: s.n }, rhs: build_chain(Seed { n: s.n - 1 }) }

  proofs:
    purity:
      reads   : [s.n]
      calls   : [build_chain]
    termination:
      bound : 100
      decreasing : n


rule eval_expr
  @intention: "Sum every integer in an Expr"
  @source: invoices.intent:1

  input:
    e : Expr

  output:
    out : number

  logic:
    out = match e:
      Int(value) => value
      Add(lhs, rhs) => eval_expr(lhs) + eval_expr(rhs)

  proofs:
    purity:
      reads   : [e]
      calls   : [eval_expr]
    termination:
      bound : 100
      structural : e


rule show
  @intention: "Sum freshly built chains while reclaiming each walk's arena nodes"
  @source: invoices.intent:1

  input:
    s : Seed

  output:
    out : number

  logic:
    out = if s.n == 0 then eval_expr(build_chain(s)) else SCOPE_OPENeval_expr(build_chain(s))SCOPE_CLOSE + show(Seed { n: s.n - 1 })

  proofs:
    purity:
      reads   : [s.n, s]
      calls   : [eval_expr, build_chain, show]
    termination:
      bound : 100
      decreasing : n

rule check
  @intention: "Keep earlier arena values alive across numeric reclaim scopes"
  @source: invoices.intent:1
  input:
    s : Seed
  output:
    out : number
  logic:
    let persistent = build_chain(Seed { n: 1 })
    let work = show(s)
    out = work + eval_expr(persistent)
  proofs:
    purity:
      reads: [s]
      calls: [build_chain, show, eval_expr]
    termination:
      bound: 100
"#;
    let base = std::env::temp_dir().join(format!("verbose-scalar-arena-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    for (name, open, close) in [
        ("plain", "", ""),
        ("scoped", "arena_scope(", ")"),
        ("nested", "arena_scope(arena_scope(", "))"),
    ] {
        let src = template
            .replace("SCOPE_OPEN", open)
            .replace("SCOPE_CLOSE", close);
        let p = parse(&src);
        let errors = verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{errors:?}");
        let binary = base.join(name);
        native::compile_native(&p, "check", binary.to_str().unwrap(), false, false).unwrap();
        for (n, expected) in [(3, "11\n"), (20, "1541\n")] {
            let r = Command::new(&binary).arg(n.to_string()).output().unwrap();
            if name == "plain" && n == 20 {
                assert_eq!(
                    r.status.code(),
                    Some(1),
                    "unscoped walks must exhaust the unchanged arena"
                );
                assert!(r.stdout.is_empty());
            } else {
                assert_eq!(
                    (r.status.code(), r.stdout, r.stderr),
                    (Some(0), expected.as_bytes().to_vec(), vec![]),
                    "{name} n={n}"
                );
            }
        }
    }
    fs::remove_dir_all(base).unwrap();
}
