use super::*;
use crate::{interpreter, lexer::Lexer, native, parser::Parser, verifier};
use std::{collections::HashMap, fs, path::Path, process::Command};

fn source(expr: &str, bindings: &str) -> String {
    let reads = if expr.contains("i.y") || bindings.contains("i.y") {
        "i.x, i.y"
    } else {
        "i.x"
    };
    format!(
        r#"@verbose 0.1.0
concept Input
  @intention: "bounded input"
  @source: invoices.intent:1
  fields:
    x : number [-10, 10]
    y : number [1, 5]
rule checked
  @intention: "checked stack use"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
{bindings}    out = {expr}
  proofs:
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 1000
    native_stack: 512
  hints:
    overflow: [-1000, 1000]
"#
    )
}

fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn rule<'a>(p: &'a mut Program, name: &str) -> &'a mut Rule {
    p.items
        .iter_mut()
        .find_map(|i| match i {
            Item::Rule(r) if r.name == name => Some(r),
            _ => None,
        })
        .unwrap()
}
fn verified(p: &Program) {
    let errors = verifier::verify_program(p, Path::new("examples"));
    assert!(errors.is_empty(), "{errors:?}");
}
fn native_bytes(p: &Program, name: &str) -> Vec<u8> {
    let path = format!("/tmp/verbose-stack-layout-{}", std::process::id());
    native::compile_native(p, name, &path, false, false).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    bytes
}

// Independent instruction-level check of the emitted CFG. Follow both sides of
// every condition and skip inline string data via the emitter's actual jumps.
// Re-entering an instruction must have the same stack depth, including loops.
// This deliberately does not consume layout metadata or source expressions.
fn machine_stack_peak(code: &[u8]) -> usize {
    let mut work = vec![(0usize, 0i64)];
    let mut depths = HashMap::new();
    let mut peak = 0;
    while let Some((pc, mut depth)) = work.pop() {
        if pc == code.len() {
            continue;
        }
        assert!(pc < code.len(), "invalid branch target {pc}");
        if let Some(old) = depths.insert(pc, depth) {
            assert_eq!(old, depth, "unbalanced stack at {pc}");
            continue;
        }
        let len = crate::validate_x86::decode_instruction_length(code, pc)
            .unwrap_or_else(|| panic!("cannot decode instruction at {pc}"));
        let ins = &code[pc..pc + len];
        match ins {
            [0x50..=0x57] => depth += 8,
            [0x58..=0x5f] => depth -= 8,
            [0x48, 0x81, 0xec, a, b, c, d] => depth += i32::from_le_bytes([*a, *b, *c, *d]) as i64,
            [0x48, 0x83, 0xec, n] => depth += *n as i8 as i64,
            [0x48, 0x83, 0xc4, n] => depth -= *n as i8 as i64,
            [0xe8, ..] | [0xc3] => panic!("unexpected runtime call/return at {pc}"),
            _ => {}
        }
        assert!(depth >= 0);
        peak = peak.max(depth as usize);
        let next = pc + len;
        let branch = match ins {
            [0xeb, n] | [0x70..=0x7f, n] => Some(*n as i8 as i64),
            [0xe9, a, b, c, d] | [0x0f, 0x80..=0x8f, a, b, c, d] => {
                Some(i32::from_le_bytes([*a, *b, *c, *d]) as i64)
            }
            _ => None,
        };
        if let Some(delta) = branch {
            work.push(((next as i64 + delta) as usize, depth));
        }
        if !matches!(ins[0], 0xe9 | 0xeb) {
            work.push((next, depth));
        }
    }
    peak
}

#[test]
fn native_stack_syntax_is_closed_and_cannot_be_overwritten() {
    let s = source("i.x + i.y", "");
    verified(&parse(&s));
    for replacement in [
        "native_stack: 0",
        "native_stack: -1",
        "native_stack: 2097153",
        "native_stack: 1 + 1",
        "native_stack: \"512\"",
        "stack: 512",
        "native_stack: 512\n    native_stack: 1024",
    ] {
        assert!(
            Parser::new(
                Lexer::new(&s.replace("native_stack: 512", replacement))
                    .tokenize()
                    .unwrap()
            )
            .parse_program()
            .is_err(),
            "{replacement}"
        );
    }
    let duplicate = s.replace("  hints:", "  proofs:\n    purity:\n      reads: [i.x, i.y]\n      calls: []\n    termination:\n      bound: 1000\n  hints:");
    assert!(Parser::new(Lexer::new(&duplicate).tokenize().unwrap())
        .parse_program()
        .is_err());
    let first = s
        .replace("    native_stack: 512\n", "")
        .replace("  proofs:\n", "  proofs:\n    native_stack: 512\n");
    verified(&parse(&first));
}

#[test]
fn native_stack_bound_matches_emitted_stack_and_exact_limit() {
    for (expr, bindings) in [
        ("i.x", ""), ("i.x + i.y", ""), ("i.x / i.y", ""),
        ("i.x % i.y", ""), ("abs(i.x) + min(i.x, i.y)", ""),
        ("max(i.x, i.y) - min(i.x, i.y)", ""),
        ("if i.x > 0 then (i.x + i.y) * (i.x - i.y) else -i.x", ""),
        ("alias + keep", "    let keep = i.x * i.y\n    let old = i.x + i.y\n    let alias = old\n    let old = i.x - i.y\n    let dead = old * i.y\n"),
    ] {
        let mut p = parse(&source(expr, bindings));
        verified(&p);
        let report = native::numeric_stack_report(&p, "checked").unwrap();
        let before = native_bytes(&p, "checked");
        assert_eq!(machine_stack_peak(&before[120..]), report.stack_bound_bytes(), "{expr}");
        assert_eq!(report.frame_bytes, report.input_slot_bytes + report.shared_slot_bytes + report.bookkeeping_bytes);
        rule(&mut p, "checked").proofs.native_stack = Some(report.stack_bound_bytes() as u32);
        verified(&p);
        assert_eq!(native_bytes(&p, "checked"), before);
        rule(&mut p, "checked").proofs.native_stack = None;
        assert_eq!(native_bytes(&p, "checked"), before, "the contract adds no runtime instructions");
        rule(&mut p, "checked").proofs.native_stack = Some(report.stack_bound_bytes() as u32 - 1);
        assert!(verify(&p)[0].message.contains("exceeds declared"));
        assert!(verifier::verify_program(&p, Path::new("examples")).iter().any(|e| e.context.contains("proofs.native_stack")));
    }
}

#[test]
fn native_stack_includes_callees_and_checks_every_declaration() {
    let helper = source("max(i.x, i.y)", "").replace("rule checked", "rule helper");
    let caller = r#"
rule checked
  @intention: "calls and live locals"
  @source: invoices.intent:1
  input:
    different : Input
  output:
    out : bool
  logic:
    let keep = different.x + different.y
    let one = helper(different)
    let two = helper(different)
    out = if keep > one then two < keep else one == two
  proofs:
    native_stack: 512
    purity:
      reads: [different, different.x, different.y]
      calls: [helper]
    termination:
      bound: 100
"#;
    let mut p = parse(&(helper + caller));
    verified(&p);
    for name in ["helper", "checked"] {
        let report = native::numeric_stack_report(&p, name).unwrap();
        let bytes = native_bytes(&p, name);
        assert_eq!(
            report.stack_bound_bytes(),
            machine_stack_peak(&bytes[120..])
        );
        rule(&mut p, name).proofs.native_stack = Some(report.stack_bound_bytes() as u32);
    }
    verified(&p);
    assert_eq!(
        native::numeric_stack_report(&p, "checked")
            .unwrap()
            .output_stack_bytes,
        0
    );
    rule(&mut p, "helper").proofs.native_stack = Some(1);
    assert!(verify(&p).iter().any(|e| e.context.contains("helper")));
    let path = format!("/tmp/verbose-stack-refusal-{}", std::process::id());
    fs::write(&path, b"existing artifact").unwrap();
    assert!(native::compile_native(&p, "checked", &path, false, false).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing artifact");
    fs::remove_file(path).unwrap();
}

#[test]
fn native_stack_unknown_analysis_and_unsupported_modes_refuse_before_artifacts() {
    let p = parse(&source("i.x + i.y", ""));
    let path = format!("/tmp/verbose-stack-mode-{}", std::process::id());
    fs::write(&path, b"existing artifact").unwrap();
    assert!(native::compile_native(&p, "checked", &path, true, false).is_err());
    assert!(native::compile_native(&p, "checked", &path, true, true).is_err());
    assert!(native::compile_native_stdin_raw(&p, "checked", &path).is_err());
    assert!(
        native::compile_native_multi(&p, &["checked", "checked"], &path, false, false).is_err()
    );
    assert!(native::compile_http_server(&p, "checked", 9999, &path).is_err());
    assert!(crate::wasm::compile_wasm(&p, "checked", &path)
        .unwrap_err()
        .message
        .contains("native_stack"));
    assert_eq!(fs::read(&path).unwrap(), b"existing artifact");
    fs::remove_file(&path).unwrap();
    let mut unmarked = p.clone();
    rule(&mut unmarked, "checked").hints = None;
    assert!(verify(&unmarked)[0]
        .message
        .contains("requires the strict numeric contract"));
    for expr in [
        "checked(i)",
        "length(\"text\")",
        "if i.x > 100 then 1 / 0 else i.x",
        "i.x + 9223372036854775807",
    ] {
        let p = parse(&source(expr, ""));
        assert!(!verify(&p).is_empty(), "{expr}");
    }
}

#[test]
fn native_stack_counts_carried_text_input_guards_even_for_constant_boolean_output() {
    let helper = source("i.x + i.y", "")
        .replace("rule checked", "rule helper")
        .replace(
            "    y : number [1, 5]",
            "    y : number [1, 5]\n    label : text [..4]",
        );
    let caller = r#"
rule checked
  @intention: "A constant boolean still needs input guard scratch"
  @source: invoices.intent:1
  input:
    i : Input
  output:
    out : bool
  logic:
    out = if i.x < -100 then helper(i) > 0 else i.x == i.x
  proofs:
    native_stack: 80
    purity:
      reads: [i, i.x]
      calls: [helper]
    termination:
      bound: 100
"#;
    let mut p = parse(&(helper + caller));
    verified(&p);
    let report = native::numeric_stack_report(&p, "checked").unwrap();
    assert_eq!(report.input_stack_bytes, 8);
    assert_eq!(report.expression_stack_bytes, 0);
    assert_eq!(report.output_stack_bytes, 0);
    assert_eq!(report.stack_bound_bytes(), 80);
    let bytes = native_bytes(&p, "checked");
    assert_eq!(machine_stack_peak(&bytes[120..]), 80);
    let path = format!("/tmp/verbose-stack-text-input-{}", std::process::id());
    native::compile_native(&p, "checked", &path, false, false).unwrap();
    for label in ["", "abcd", "éé"] {
        let out = Command::new(&path)
            .args(["1", "2", label])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stdout, b"true\n");
    }
    let out = Command::new(&path)
        .args(["1", "2", "ééé"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    fs::remove_file(path).unwrap();
    rule(&mut p, "checked").proofs.native_stack = Some(79);
    assert!(verify(&p)[0]
        .message
        .contains("80 bytes exceeds declared 79"));
}

#[test]
fn native_stack_preserves_numeric_results_entry_guards_and_record_reuse() {
    let p = parse(&source("if i.x < 0 then -i.x + i.y else i.x * i.y", ""));
    verified(&p);
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let concepts: Vec<_> = iter_all_concepts(&p.items).collect();
    let path = format!("/tmp/verbose-stack-behavior-{}", std::process::id());
    native::compile_native(&p, "checked", &path, false, false).unwrap();
    let control_path = format!("{path}-control");
    let mut control = p.clone();
    rule(&mut control, "checked").proofs.native_stack = None;
    native::compile_native(&control, "checked", &control_path, false, false).unwrap();
    let mut args = vec![];
    let mut expected = String::new();
    for x in -10..=10 {
        for y in 1..=5 {
            let oracle = if x < 0 { -x + y } else { x * y };
            let value = interpreter::eval_rule(
                rules[0],
                &rules,
                &concepts,
                &[],
                &HashMap::from([
                    ("x".into(), interpreter::Value::Number(x)),
                    ("y".into(), interpreter::Value::Number(y)),
                ]),
            )
            .unwrap();
            assert_eq!(value.to_string(), oracle.to_string());
            args.extend([x.to_string(), y.to_string()]);
            expected.push_str(&format!("{oracle}\n"));
        }
    }
    let out = Command::new(&path).args(&args).output().unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, expected.as_bytes());
    assert!(out.stderr.is_empty());
    for args in [
        vec!["-11", "1"],
        vec!["10", "6"],
        vec!["x", "1"],
        vec!["1"],
        vec!["9223372036854775808", "1"],
        vec![],
    ] {
        let out = Command::new(&path).args(&args).output().unwrap();
        let old = Command::new(&control_path).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1));
        assert!(out.stdout.is_empty());
        assert_eq!(
            (out.status, out.stdout, out.stderr),
            (old.status, old.stdout, old.stderr)
        );
    }
    let out = Command::new(&path).args(["1", "2", "1"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(out.stdout, b"2\n");
    fs::remove_file(path).unwrap();
    fs::remove_file(control_path).unwrap();
}

#[test]
fn native_stack_self_hosted_refusal_is_scoped_and_precedes_raw_and_elf_output() {
    use std::process::Stdio;
    let compiler = parse(&fs::read_to_string("examples/vexprparse.verbose").unwrap());
    let dir = std::env::temp_dir().join(format!("verbose-stack-gen0-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // Omit overflow entirely so its existing capability refusal cannot hide
    // a missing native_stack gate. The control must be accepted by gen0.
    let control = source("i.x + i.y", "")
        .replace("    native_stack: 512\n", "")
        .replace("  hints:\n    overflow: [-1000, 1000]\n", "");
    let cases = [
        (control.clone(), false),
        (
            control.replace("  proofs:\n", "  proofs:\n    native_stack : 512\n"),
            true,
        ),
        (
            control.replace(
                "      bound: 1000\n",
                "      bound: 1000\n    native_stack: 512\n",
            ),
            true,
        ),
        (
            control
                .replace(
                    "    out = i.x + i.y",
                    "    let native_stack = i.x\n    out = native_stack + i.y",
                )
                .replace("checked stack use", "native_stack: 1"),
            false,
        ),
        (
            control
                .replace("    x :", "    native_stack :")
                .replace("i.x", "i.native_stack"),
            false,
        ),
        (format!("{control}\n-- native_stack: 1\n"), false),
    ];
    let input = dir.join("input.verbose");
    for entry in ["elf_program_src", "x86_program_src", "verify_errors"] {
        let bin = dir.join(entry);
        native::compile_native_stdin_raw(&compiler, entry, bin.to_str().unwrap()).unwrap();
        for (case, refused) in &cases {
            fs::write(&input, case).unwrap();
            // The tokenizer is recursive; pass the binary as a positional
            // shell argument, never interpolate source text or paths as code.
            let out = Command::new("sh")
                .args(["-c", "ulimit -s unlimited; exec \"$1\" 0", "sh"])
                .arg(&bin)
                .stdin(fs::File::open(&input).unwrap())
                .stdout(Stdio::piped())
                .output()
                .unwrap();
            if entry == "verify_errors" {
                assert!(out.status.success(), "{out:?}");
                let n: u32 = String::from_utf8(out.stdout)
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                assert_eq!(n > 0, *refused, "{case}");
            } else if *refused {
                assert_eq!(out.status.code(), Some(1), "{entry}: {case}");
                assert!(out.stdout.is_empty(), "{entry}: refusal after emission");
            } else {
                assert!(out.status.success(), "{entry}: {case}\n{out:?}");
                assert!(!out.stdout.is_empty());
            }
        }
    }
    fs::remove_dir_all(dir).unwrap();
}
