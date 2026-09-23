use super::*;
use crate::{lexer::Lexer, parser::Parser, verifier};
use std::{
    fs,
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

fn program(rules: &str, names: &str, limit: usize, budget: Option<usize>) -> Program {
    let budget = budget.map_or(String::new(), |b| format!("  native_memory: {b}\n"));
    let src=format!("{rules}\nexecution together\n  @intention: \"Concurrent native test\"\n  @source: concurrent_execution.intent:1\n  input: {}\n  mode: concurrent\n  phases: [{names}]\n  on_failure: stop\n  max_in_flight: {limit}\n{budget}",if rules.contains("concept Reading") {"Reading"} else {"Input"});
    Parser::new(Lexer::new(&src).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
const RULES: &str = include_str!("../../../examples/sequential_stack.verbose");
fn entry(p: &Program) -> &Execution {
    crate::execution::find(p, "together").unwrap()
}
fn checked(p: &Program) {
    assert!(verifier::verify_program(p, Path::new("examples")).is_empty());
}
fn binary(code: &[u8], suffix: &str) -> String {
    let path = format!(
        "/tmp/verbose-native-concurrent-{}-{suffix}",
        std::process::id()
    );
    write_native_elf(code, &path).unwrap();
    path
}
fn run(path: &str, args: &[String]) -> std::process::Output {
    let out = Command::new("timeout")
        .args(["10s", path])
        .args(args)
        .output()
        .unwrap();
    assert!(
        ![Some(124), Some(137)].contains(&out.status.code()),
        "worker cleanup timed out"
    );
    out
}
fn args(rows: &[(&str, i64)]) -> Vec<String> {
    rows.iter()
        .flat_map(|(s, n)| [s.to_string(), n.to_string()])
        .collect()
}

#[test]
fn native_concurrent_matches_sequential_across_waves_and_results() {
    for (rules, names) in [
        (RULES, "clamp, nonnegative, label"),
        (RULES, "label, clamp, label"),
        (
            include_str!("../../../examples/retained_stack.verbose"),
            "prepare, analyze, prepare",
        ),
    ] {
        let names_vec: Vec<_> = names.split(", ").collect();
        for limit in [1, 2, 64] {
            let p = program(rules, names, limit, Some(1_000_000));
            checked(&p);
            let native = binary(&compile(&p, entry(&p)).unwrap(), "values");
            let sequential = binary(&sequential::compile(&p, &names_vec).unwrap(), "sequential");
            for rows in [
                vec![("café", 2), ("b", 1000)],
                vec![("", i64::MIN), ("🚀", i64::MAX)],
                vec![("a\"\\\n", -1), ("next", 3)],
                vec![("", 0)],
            ] {
                let input = args(&rows);
                let a = run(&native, &input);
                let b = run(&sequential, &input);
                assert_eq!(
                    (a.status.code(), a.stdout, a.stderr),
                    (b.status.code(), b.stdout, b.stderr),
                    "{names}/{limit}"
                );
            }
            fs::remove_file(native).unwrap();
            fs::remove_file(sequential).unwrap();
        }
    }
    let p = program(RULES, &vec!["label"; 64].join(", "), 64, Some(1_000_000));
    let code = compile(&p, entry(&p)).unwrap();
    assert_eq!(code, compile(&p, entry(&p)).unwrap());
    let path = binary(&code, "64");
    let out = run(&path, &args(&[("é", 7)]));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, "é:7\n".repeat(64).as_bytes());
    fs::remove_file(path).unwrap();
}

#[test]
fn native_concurrent_input_errors_preserve_only_completed_records() {
    for names in ["clamp, label", "label, clamp"] {
        let p = program(RULES, names, 2, Some(20480));
        let path = binary(&compile(&p, entry(&p)).unwrap(), "input");
        for invalid in [
            vec![],
            vec!["x"],
            vec!["x", ""],
            vec!["x", "1x"],
            vec!["x", "9223372036854775808"],
            vec!["x", "-9223372036854775809"],
            vec!["too long!", "1"],
        ] {
            let input: Vec<_> = invalid.iter().map(|s| s.to_string()).collect();
            let out = run(&path, &input);
            assert_eq!(out.status.code(), Some(1));
            assert!(out.stdout.is_empty());
            if !invalid.is_empty() {
                let mut input = args(&[("ok", 2)]);
                input.extend(invalid.iter().map(|s| s.to_string()));
                let out = run(&path, &input);
                assert_eq!(out.status.code(), Some(1));
                assert_eq!(
                    out.stdout,
                    if names.starts_with("clamp") {
                        b"2\n".as_slice()
                    } else {
                        b"ok:2\n".as_slice()
                    }
                );
            }
        }
        fs::remove_file(path).unwrap();
    }
    // The later phase fails on an unused input bound while the earlier phase
    // already has a complete record. All phases enforce all declared fields.
}

#[test]
fn native_concurrent_layout_budget_and_artifact_gates() {
    let p = program(RULES, "clamp, nonnegative, label", 2, None);
    checked(&p);
    let report = report(&p, entry(&p)).unwrap();
    assert_eq!(report.reserved_bytes, 20480);
    assert_eq!(report.control_reserved, 4096);
    assert_eq!(report.lanes.len(), 2);
    assert_eq!(report.lanes[0].output_bytes, 30);
    assert_eq!(report.lanes[0].stack_bytes, report.phases[2].stack_bytes);
    assert_eq!(report.lanes[1].output_bytes, 6);
    let mut cursor = report.control_reserved;
    for lane in &report.lanes {
        assert_eq!(lane.stack, cursor + 4096);
        assert!(lane.stack_reserved >= lane.stack_bytes);
        cursor = lane.stack + lane.stack_reserved;
    }
    assert_eq!(cursor, report.reserved_bytes);
    assert!(compile(&p, entry(&p))
        .unwrap_err()
        .message
        .contains("native_memory is required"));
    let exact = program(RULES, "clamp, nonnegative, label", 2, Some(20480));
    checked(&exact);
    let generous = program(RULES, "clamp, nonnegative, label", 2, Some(20481));
    assert_eq!(
        compile(&exact, entry(&exact)).unwrap(),
        compile(&generous, entry(&generous)).unwrap()
    );
    let low = program(RULES, "clamp, nonnegative, label", 2, Some(20479));
    assert!(verifier::verify_program(&low, Path::new("examples"))
        .iter()
        .any(|e| e.to_string().contains("20480 bytes exceeds declared 20479")));
    let path = format!("/tmp/verbose-concurrent-gate-{}", std::process::id());
    fs::write(&path, b"keep").unwrap();
    assert!(compile_native(&low, "clamp", &path, false, false).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"keep");
    for (stdin, stream) in [(true, false), (false, true)] {
        assert!(compile_native(&exact, "together", &path, stdin, stream).is_err());
    }
    assert!(compile_native_stdin_raw(&exact, "together", &path).is_err());
    assert!(crate::wasm::compile_wasm(&exact, "clamp", &path).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"keep");
    fs::remove_file(path).unwrap();
}

#[test]
fn native_concurrent_syscall_failures_cancel_and_join_admitted_workers() {
    let p = program(RULES, "clamp, nonnegative, label", 2, Some(20480));
    let prepared = prepare(&p, entry(&p)).unwrap();
    let emission = emit::compile(&prepared).unwrap();
    for nr in [9, 10, 14, 56, 1, 202] {
        let occurrences: Vec<_> = emission.syscalls.iter().filter(|(_, n)| *n == nr).collect();
        for (index, &&(site, _)) in occurrences.iter().enumerate() {
            // Every clone site, both initial protection failures, and one
            // write/futex error suffice to exercise distinct cleanup boundaries.
            if (nr == 202 && index > 0) || (nr != 56 && index > 1) {
                break;
            }
            let mut code = emission.code.clone();
            if nr == 202 {
                for &&(at, _) in &occurrences {
                    code[at..at + 4].copy_from_slice(&9999i32.to_le_bytes());
                }
            } else {
                code[site..site + 4].copy_from_slice(&9999i32.to_le_bytes());
            }
            let path = binary(&code, "fault");
            let out = run(&path, &args(&[("a", 2), ("b", 3)]));
            assert_eq!(
                out.status.code(),
                Some(1),
                "syscall {nr}, occurrence {index}"
            );
            if nr == 56 && index == 2 {
                assert_eq!(out.stdout, b"2\n3\ntrue\ntrue\n");
            } else if [9, 10, 14, 56].contains(&nr) {
                assert!(out.stdout.is_empty(), "{nr}/{index}");
            }
            fs::remove_file(path).unwrap();
        }
    }
}

#[test]
fn native_concurrent_backpressure_keeps_threads_bounded_and_joins_on_broken_pipe() {
    let p = program(RULES, "label, label, label, label", 2, Some(20480));
    let path = binary(&compile(&p, entry(&p)).unwrap(), "backpressure");
    let input: Vec<_> = (0..5000)
        .flat_map(|_| ["12345678".to_string(), "9223372036854775807".to_string()])
        .collect();
    let mut child = Command::new(&path)
        .args(&input)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    let mut observed = false;
    while Instant::now() < until {
        let threads = fs::read_dir(format!("/proc/{}/task", child.id()))
            .unwrap()
            .count();
        assert!(threads <= 3, "admitted more than two workers");
        if threads == 3 {
            observed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if !observed {
        let _ = child.kill();
        let _ = child.wait();
        panic!("no concurrent native workers observed");
    }
    // Closing the only reader turns the coordinator write into EPIPE. Later
    // workers can be waiting on publication; cancellation must release them.
    drop(child.stdout.take());
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("broken-pipe cleanup blocked");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Delayed consumption drains all waves without losing or reordering bytes.
    let mut child = Command::new(&path)
        .args(&input)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let r = stdout.read_to_end(&mut bytes);
        tx.send((r, bytes)).unwrap();
    });
    let output = match rx.recv_timeout(Duration::from_secs(10)) {
        Ok((r, b)) => {
            r.unwrap();
            b
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            reader.join().unwrap();
            panic!("output drain blocked")
        }
    };
    reader.join().unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(output, b"12345678:9223372036854775807\n".repeat(20000));
    fs::remove_file(path).unwrap();
}

// Decode the worker CFG independently of its reported layout. External jumps
// only target the process-fatal boundary. Follow both arms, including body data
// skips and loop edges; all visits must agree on rsp/rbp ownership.
fn stack_peak(code: &[u8]) -> usize {
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct S {
        depth: i64,
        bp: i64,
        saved: Vec<(i64, i64)>,
    }
    let mut todo = vec![(0usize, S::default())];
    let mut seen = HashMap::new();
    let mut peak = 0;
    while let Some((pc, mut s)) = todo.pop() {
        if pc == code.len() {
            continue;
        }
        if let Some(old) = seen.insert(pc, s.clone()) {
            assert_eq!(old, s, "unbalanced at {pc}");
            continue;
        }
        let len = crate::validate_x86::decode_instruction_length(code, pc)
            .unwrap_or_else(|| panic!("decode {pc}"));
        let ins = &code[pc..pc + len];
        match ins {
            [0x55] => {
                s.depth += 8;
                s.saved.push((s.depth, s.bp));
            }
            [0x5d] => {
                let (d, b) = s.saved.pop().unwrap();
                assert_eq!(d, s.depth);
                s.bp = b;
                s.depth -= 8;
            }
            [0x50..=0x57] => s.depth += 8,
            [0x58..=0x5f] => s.depth -= 8,
            [0x48, 0x89, 0xe5] => s.bp = s.depth,
            [0x48, 0x89, 0xec] => s.depth = s.bp,
            [0x48, 0x81, 0xec, a, b, c, d] => {
                s.depth += i32::from_le_bytes([*a, *b, *c, *d]) as i64
            }
            [0x48, 0x83, 0xec, n] => s.depth += *n as i8 as i64,
            [0x48, 0x83, 0xc4, n] => s.depth -= *n as i8 as i64,
            [0xe8, ..] | [0xc3] => panic!("unexpected runtime call"),
            _ => {}
        }
        assert!(s.depth >= 0);
        peak = peak.max(s.depth as usize);
        let next = pc + len;
        let delta = match ins {
            [0xeb, n] | [0x70..=0x7f, n] => Some(*n as i8 as i64),
            [0xe9, a, b, c, d] | [0x0f, 0x80..=0x8f, a, b, c, d] => {
                Some(i32::from_le_bytes([*a, *b, *c, *d]) as i64)
            }
            _ => None,
        };
        if let Some(d) = delta {
            let target = next as i64 + d;
            if target >= 0 {
                todo.push((target as usize, s.clone()));
            }
        }
        if !matches!(ins[0], 0xe9 | 0xeb) {
            todo.push((next, s));
        }
    }
    peak
}
#[test]
fn native_concurrent_report_covers_actual_worker_stack_and_closed_syscalls() {
    for (rules, names) in [
        (RULES, "clamp, nonnegative, label"),
        (
            include_str!("../../../examples/retained_stack.verbose"),
            "prepare, analyze, prepare",
        ),
    ] {
        let p = program(rules, names, 2, Some(1_000_000));
        let prepared = prepare(&p, entry(&p)).unwrap();
        let emission = emit::compile(&prepared).unwrap();
        for ((start, end), body) in emission.workers.iter().zip(&prepared.bodies) {
            assert_eq!(stack_peak(&emission.code[*start..*end]), body.stack_bytes);
        }
        assert_eq!(emission.syscalls.iter().filter(|(_, n)| *n == 9).count(), 1);
        assert_eq!(
            emission.syscalls.iter().filter(|(_, n)| *n == 11).count(),
            1
        );
        assert_eq!(
            emission.syscalls.iter().filter(|(_, n)| *n == 56).count(),
            3
        );
        assert!(emission
            .syscalls
            .iter()
            .all(|(_, n)| [1, 9, 10, 11, 14, 56, 60, 202, 231].contains(n)));
    }
    for bytes in [
        &[0x41, 0x87, 0x87, 0, 0, 0, 0][..],
        &[0xf0, 0x41, 0x0f, 0xb1, 0x17][..],
    ] {
        assert_eq!(
            crate::validate_x86::decode_instruction_length(bytes, 0),
            Some(bytes.len())
        );
        assert!(crate::validate_x86::validate_code(&bytes[..bytes.len() - 1]).is_err());
    }
}

#[test]
fn native_concurrent_large_buffers_nul_and_numeric_aliases() {
    let rules = RULES
        .replace("out : text [..29]", "out : text [..9030]")
        .replace(
            "concat(item.title, \":\", item.value)",
            &format!("concat(item.title, \"{}\", item.value)", "x".repeat(9000)),
        )
        .replace("native_stack: 192", "native_stack: 16000");
    let p = program(&rules, "clamp, label, label, nonnegative", 2, Some(100000));
    checked(&p);
    let prepared = prepare(&p, entry(&p)).unwrap();
    assert!(prepared
        .report
        .lanes
        .iter()
        .all(|l| l.output_bytes >= 9029 && l.stack_reserved >= 12288));
    let path = binary(&compile(&p, entry(&p)).unwrap(), "large");
    let output = run(&path, &args(&[("é", i64::MAX)]));
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        format!(
            "100\né{}{}\né{}{}\ntrue\n",
            "x".repeat(9000),
            i64::MAX,
            "x".repeat(9000),
            i64::MAX
        )
        .as_bytes()
    );
    fs::remove_file(path).unwrap();
    let rules = RULES.replace(
        "concat(item.title, \":\", item.value)",
        "concat(item.title, \"\0\", item.value)",
    );
    let p = program(&rules, "label, label", 2, Some(20480));
    let path = binary(&compile(&p, entry(&p)).unwrap(), "nul");
    let out = run(&path, &args(&[("a", 2)]));
    assert_eq!(
        (out.status.code(), out.stdout),
        (Some(0), b"a\x002\na\x002\n".to_vec())
    );
    fs::remove_file(path).unwrap();
    let rules = include_str!("../../../examples/native_stack.verbose").replace(
        "let alias = limited",
        "let alias = limited\n    let limited = alias",
    );
    let p = program(&rules, "magnitude, clamp, magnitude", 2, Some(20480));
    checked(&p);
    let path = binary(&compile(&p, entry(&p)).unwrap(), "aliases");
    let out = run(
        &path,
        &[i64::MIN.to_string(), "0".into(), i64::MAX.to_string()],
    );
    assert_eq!(
        (out.status.code(), out.stdout),
        (
            Some(0),
            b"100\n0\n100\n-100\n0\n100\n100\n0\n100\n".to_vec()
        )
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn native_concurrent_instruction_graph_rejects_bad_targets_and_skips_data() {
    assert!(emit::validate_graph(&[0xe9, 1, 0, 0, 0, 0x3a, 0x90]).is_ok());
    assert!(emit::validate_graph(&[0xe9, 4, 0, 0, 0, 0x90]).is_err());
    // Both arms: the conditional targets the middle of a subsequent mov.
    assert!(emit::validate_graph(&[0x74, 1, 0x48, 0xc7, 0xc0, 1, 0, 0, 0]).is_err());
    assert!(emit::validate_graph(&[0xe8, 0, 0, 0, 0]).is_err());
}
