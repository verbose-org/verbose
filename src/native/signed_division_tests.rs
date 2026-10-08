//! Signed division semantics and the storage cost of its constant lowering.
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
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

fn source(body: &str, kind: &str, domain: &str, calls: &str) -> String {
    let reads = if body.contains("i.n") { "i.n" } else { "" };
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "Signed dividend"
  @source: invoices.intent:1
  fields:
    n : number{domain}
rule quotient
  @intention: "Divide with truncation toward zero"
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

fn eval(p: &Program, n: i64) -> Result<Value, String> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([("n".into(), Value::Number(n))]),
    )
    .map_err(|e| e.message)
}

struct Executable(PathBuf);

impl Executable {
    fn new(p: &Program) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // Never rewrite the inode of a previous arithmetic-trap probe.
        let path = std::env::temp_dir().join(format!(
            "verbose-signed-div-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);
        compile_native(p, "quotient", path.to_str().unwrap(), false, false).unwrap();
        Self(path)
    }

    fn run(&self, inputs: &[i64]) -> Output {
        Command::new(&self.0)
            .args(inputs.iter().map(i64::to_string))
            .output()
            .unwrap()
    }
}

impl Drop for Executable {
    fn drop(&mut self) {
        fs::remove_file(&self.0).unwrap();
    }
}

fn differential(p: &Program, inputs: &[i64]) {
    let expected: String = inputs
        .iter()
        .map(|&n| format!("{}\n", eval(p, n).unwrap()))
        .collect();
    for emitted in [p.clone(), optimizer::optimize_program(p).0] {
        let binary = Executable::new(&emitted);
        let out = binary.run(inputs);
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), expected.as_bytes().to_vec(), vec![]),
            "{inputs:?}"
        );
    }
}

fn samples(divisor: i64) -> Vec<i64> {
    let mut values: Vec<_> = (-16..=16).collect();
    values.extend([i64::MIN, i64::MIN + 1, i64::MAX - 1, i64::MAX]);
    for multiple in [-2i128, -1, 1, 2] {
        for offset in -1..=1 {
            if let Ok(n) = i64::try_from(multiple * divisor as i128 + offset) {
                values.push(n);
            }
        }
    }
    let mut state = 0xd1b54a32d192ed03u64;
    for _ in 0..32 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        values.push(state as i64);
    }
    values.sort_unstable();
    values.dedup();
    values
}

#[test]
fn signed_division_all_positive_powers_match_the_interpreter() {
    for k in 0..=62 {
        let d = 1i64 << k;
        let p = checked(&source(&format!("    out = i.n / {d}"), "number", "", ""));
        let inputs = samples(d);
        for &n in &inputs {
            assert_eq!(eval(&p, n), Ok(Value::Number(n / d)), "{n}/{d}");
        }
        differential(&p, &inputs);
    }
}

#[test]
fn signed_division_composes_with_bindings_calls_and_text() {
    let inputs = samples(8);
    for body in [
        "    out = (i.n / 2) / 4",
        "    out = (i.n / 2) + (i.n / 4)",
        "    out = (i.n / 2) - (i.n / 8)",
        "    out = 23 + i.n / 2",
        "    out = i.n / 2 + 23",
        "    out = (i.n / 2) / (if i.n < 0 then 4 else 8)",
        "    out = (if i.n < 0 then i.n / 2 else i.n / 4) / 2",
        "    let part = i.n / 2\n    let alias = part\n    let part = i.n / 4\n    out = alias + part",
        "    out = if i.n < 0 and i.n / 2 < 0 then i.n / 4 else i.n / 8",
    ] {
        differential(&checked(&source(body, "number", "", "")), &inputs);
    }
    let helper = source("    out = i.n / 2", "number", "", "")
        .split("rule quotient")
        .nth(1)
        .unwrap()
        .replace("i : Input", "value : Input")
        .replace("i.n", "value.n");
    let src = format!(
        "{}\nrule half{helper}",
        source("    out = half(i) / 4 + i.n / 2", "number", "", "half")
            .replace("reads: [i.n]", "reads: [i.n, i]")
    );
    differential(&checked(&src), &inputs);
    differential(
        &checked(&source(
            "    out = concat(\"half:\", i.n / 2, \" quarter:\", i.n / 4)",
            "text",
            "",
            "",
        )),
        &inputs,
    );

    // Collection lowering has live loop registers around scalar evaluation.
    let src = source(
        "    out = sum(i.items, v => v.n / 2 + v.n / 4)",
        "number",
        "",
        "",
    )
    .replace("n : number", "items : collection(Item)")
    .replace(
        "concept Input",
        "concept Item\n  @intention: \"Signed collection element\"\n  @source: invoices.intent:1\n  fields:\n    n : number\nconcept Input",
    )
    .replace("reads: []", "reads: [i.items]");
    let p = checked(&src);
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Rule(r) => Some(r),
            _ => None,
        })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let items = [-31, -9, -1, 0, 1, 9, 63];
    let expected = interpreter::eval_rule(
        rules[0],
        &rules,
        &concepts,
        &[],
        &HashMap::from([(
            "items".into(),
            Value::List(
                items
                    .iter()
                    .map(|&n| Value::Record(HashMap::from([("n".into(), Value::Number(n))])))
                    .collect(),
            ),
        )]),
    )
    .unwrap();
    // Native collection input starts with its element count.
    let argv: Vec<_> = std::iter::once(items.len() as i64)
        .chain(items.iter().copied())
        .collect();
    for emitted in [p.clone(), optimizer::optimize_program(&p).0] {
        let out = Executable::new(&emitted).run(&argv);
        assert_eq!(
            (out.status.code(), out.stdout, out.stderr),
            (Some(0), format!("{expected}\n").into_bytes(), vec![])
        );
    }
}

#[test]
fn signed_division_recursive_calls_preserve_live_values() {
    let p = checked(&source(
        "    out = if i.n == 0 then 0 else 1 + quotient(Input { n: i.n / 2 })",
        "number",
        "",
        "quotient",
    ));
    for emitted in [p.clone(), optimizer::optimize_program(&p).0] {
        let binary = Executable::new(&emitted);
        for n in [i64::MIN, -129, -128, -7, -1, 0, 1, 7, 128, 129, i64::MAX] {
            let out = binary.run(&[n]);
            assert_eq!(
                (out.status.code(), out.stdout, out.stderr),
                (
                    Some(0),
                    format!("{}\n", eval(&p, n).unwrap()).into_bytes(),
                    vec![]
                )
            );
        }
    }
}

#[test]
fn signed_division_preserves_required_and_skipped_failures() {
    for expr in [
        "(1 / i.n) / 2",
        "(1 / i.n) / 1",
        "(if 1 / i.n > 0 then 4 else 8) / 2",
        "0 * ((1 / i.n) / 2)",
    ] {
        let p = checked(&source(&format!("    out = {expr}"), "number", "", ""));
        assert!(eval(&p, 0).unwrap_err().contains("division by zero"));
        for emitted in [p.clone(), optimizer::optimize_program(&p).0] {
            let out = Executable::new(&emitted).run(&[0]);
            assert_eq!(
                (out.status.signal(), out.stdout, out.stderr),
                (Some(8), vec![], vec![]),
                "{expr}"
            );
        }
        differential(&p, &[1, 2, 3, -1, -2, -3]);
    }
    for expr in [
        "if i.n == 0 then 7 else (1 / i.n) / 2",
        "if i.n == 0 or (1 / i.n) / 2 > 0 then 7 else 9",
    ] {
        differential(
            &checked(&source(&format!("    out = {expr}"), "number", "", "")),
            &[0, 1, -1, 2, -2],
        );
    }
}

#[test]
fn signed_division_other_divisors_and_source_contracts_remain_checked() {
    for d in [-2, -4, -(1i64 << 62), i64::MIN, 3, 7, 10] {
        let literal = if d == i64::MIN {
            "(-9223372036854775807 - 1)".into()
        } else {
            d.to_string()
        };
        let p = checked(&source(
            &format!("    out = i.n / {literal}"),
            "number",
            "",
            "",
        ));
        differential(&p, &samples(8));
    }
    let strict = checked(&format!(
        "{}  hints:\n    overflow: [-100, 100]\n",
        source("    out = i.n / 2", "number", " [-100, 100]", "")
    ));
    differential(&strict, &[-100, -7, -1, 0, 1, 7, 100]);
    for src in [
        source(
            "    out = if 1 == 1 then 7 else i.n / \"2\"",
            "number",
            "",
            "",
        ),
        format!(
            "{}  hints:\n    overflow: [-100, 100]\n",
            source(
                "    out = if 1 == 1 then 7 else i.n / 0",
                "number",
                " [-100, 100]",
                ""
            )
        ),
    ] {
        assert!(!verifier::verify_program(&parse(&src), Path::new("examples")).is_empty());
    }
}

#[test]
fn signed_division_sequences_use_registers_and_retain_nonnegative_fast_path() {
    let input = Expr::Field(Box::new(Expr::Ident("i".into())), "n".into());
    for k in 0..=62u8 {
        let expr = Expr::Binary(
            BinOp::Div,
            Box::new(input.clone()),
            Box::new(Expr::Number(1i64 << k)),
        );
        for nonnegative in [false, true] {
            let ranges = HashMap::from([("n", (if nonnegative { 0 } else { i64::MIN }, i64::MAX))]);
            let mut code = vec![];
            emit_eval_expr(
                &mut code,
                &expr,
                "i",
                &HashMap::from([("n", -8)]),
                &HashMap::new(),
                &ranges,
                &HashMap::new(),
                None,
                None,
            )
            .unwrap();
            // Exact instruction budgets pin the no-stack/no-allocation path;
            // the independent execution matrix above checks the arithmetic.
            let mut expected = vec![0x48, 0x8b, 0x45, 0xf8]; // load input
            if k != 0 {
                if nonnegative {
                    expected.extend_from_slice(&[0x48, 0xc1, 0xe8, k]);
                } else {
                    expected.extend_from_slice(&[
                        0x48,
                        0x99,
                        0x48,
                        0xc1,
                        0xea,
                        64 - k,
                        0x48,
                        0x01,
                        0xd0,
                        0x48,
                        0xc1,
                        0xf8,
                        k,
                    ]);
                }
            }
            assert_eq!(code, expected, "k={k}, nonnegative={nonnegative}");
        }
    }
    for domain in [" [0, 100]", " [-100, -1]", " [-100, 100]"] {
        let values = if domain.starts_with(" [0") {
            vec![0, 1, 7, 8, 99, 100]
        } else if domain.ends_with("-1]") {
            vec![-100, -99, -8, -7, -1]
        } else {
            vec![-100, -7, -1, 0, 1, 7, 100]
        };
        differential(
            &checked(&source("    out = i.n / 8", "number", domain, "")),
            &values,
        );
    }
}
