use super::*;

pub(super) fn batched(mut p: Program, capacity: u32) -> Program {
    let Item::Execution(e) = p.items.last_mut().unwrap() else {
        unreachable!()
    };
    let ExecutionMode::Concurrent { result_batch, .. } = &mut e.mode else {
        unreachable!()
    };
    *result_batch = capacity;
    p
}

#[test]
fn native_result_batches_match_default_for_full_partial_and_multiple_waves() {
    for (rules, names) in [
        (RULES, "clamp, nonnegative, label"),
        (RULES, "label, clamp, label"),
        (
            include_str!("../../../../examples/retained_stack.verbose"),
            "prepare, analyze, prepare",
        ),
    ] {
        let p = program(rules, names, 2, Some(1_000_000));
        let reference = binary(&compile(&p, entry(&p)).unwrap(), "batch-reference");
        for capacity in [2, 8, 32, 1024] {
            for limit in [1, 2, 64] {
                let p = batched(program(rules, names, limit, Some(1_000_000)), capacity);
                checked(&p);
                let code = compile(&p, entry(&p)).unwrap();
                assert_eq!(code, compile(&p, entry(&p)).unwrap());
                let path = binary(&code, "batch-values");
                for count in [1, 7, 8, 9, 33] {
                    let rows: Vec<_> = (0..count)
                        .map(|i| (if i % 2 == 0 { "é" } else { "" }, i))
                        .collect();
                    let input = args(&rows);
                    let a = run(&path, &input);
                    let b = run(&reference, &input);
                    assert_eq!(
                        (a.status.code(), a.stdout, a.stderr),
                        (b.status.code(), b.stdout, b.stderr),
                        "{names}/{capacity}/{limit}/{count}"
                    );
                }
                let input = args(&[("🚀", i64::MIN), ("quote\"\n", -1), ("", i64::MAX)]);
                let a = run(&path, &input);
                let b = run(&reference, &input);
                assert_eq!(
                    (a.status.code(), a.stdout, a.stderr),
                    (b.status.code(), b.stdout, b.stderr)
                );
                fs::remove_file(path).unwrap();
            }
        }
        fs::remove_file(reference).unwrap();
    }
}

#[test]
fn native_result_batches_flush_valid_prefix_before_bad_input() {
    for names in ["clamp, label", "label, clamp"] {
        let p = program(RULES, names, 2, Some(1_000_000));
        let reference = binary(&compile(&p, entry(&p)).unwrap(), "batch-bad-reference");
        for capacity in [2, 8, 32] {
            let p = batched(p.clone(), capacity);
            let path = binary(&compile(&p, entry(&p)).unwrap(), "batch-bad");
            for count in [0, 1, 7, 8, 9, 16] {
                for bad in [
                    vec!["x"],
                    vec!["x", "1x"],
                    vec!["x", ""],
                    vec!["x", "9223372036854775808"],
                    vec!["x", "-9223372036854775809"],
                    vec!["too long!", "1"],
                ] {
                    let mut input = args(&vec![("ok", 2); count]);
                    input.extend(bad.into_iter().map(String::from));
                    let a = run(&path, &input);
                    let b = run(&reference, &input);
                    assert_eq!(a.status.code(), Some(1));
                    assert_eq!(
                        (a.status.code(), a.stdout, a.stderr),
                        (b.status.code(), b.stdout, b.stderr),
                        "{names}/{capacity}/{count}"
                    );
                }
            }
            let out = run(&path, &[]);
            assert_eq!(
                (out.status.code(), out.stdout, out.stderr),
                (Some(1), vec![], vec![])
            );
            fs::remove_file(path).unwrap();
        }
        fs::remove_file(reference).unwrap();
    }
}

#[test]
fn native_result_batches_count_actual_bytes_and_preserve_stack_bounds() {
    let base = program(RULES, "clamp, nonnegative, label", 2, Some(1_000_000));
    let old = report(&base, entry(&base)).unwrap();
    for capacity in [1, 2, 32, 128, 1024] {
        let mut p = batched(base.clone(), capacity);
        let r = report(&p, entry(&p)).unwrap();
        assert_eq!(r.result_batch, capacity as usize);
        for (l, previous) in r.lanes.iter().zip(&old.lanes) {
            assert_eq!(l.result_bytes, previous.output_bytes);
            assert_eq!(l.output_bytes, previous.output_bytes * capacity as usize);
            assert_eq!(l.stack_bytes, previous.stack_bytes);
            assert_eq!(l.stack_reserved, previous.stack_reserved);
        }
        assert_eq!(
            r.reserved_bytes,
            match capacity {
                1 | 2 | 32 => 20480,
                128 => 24576,
                _ => 57344,
            }
        );
        let Item::Execution(e) = p.items.last_mut().unwrap() else {
            unreachable!()
        };
        let ExecutionMode::Concurrent { native_memory, .. } = &mut e.mode else {
            unreachable!()
        };
        *native_memory = Some(r.reserved_bytes as u32);
        checked(&p);
        let prepared = prepare(&p, entry(&p)).unwrap();
        let emitted = emit::compile(&prepared).unwrap();
        for ((start, end), body) in emitted.workers.iter().zip(&prepared.bodies) {
            assert_eq!(stack_peak(&emitted.code[*start..*end]), body.stack_bytes);
        }
        assert_eq!(emitted.syscalls.iter().filter(|(_, n)| *n == 9).count(), 1);
        assert_eq!(emitted.syscalls.iter().filter(|(_, n)| *n == 11).count(), 1);
        let Item::Execution(e) = p.items.last_mut().unwrap() else {
            unreachable!()
        };
        let ExecutionMode::Concurrent { native_memory, .. } = &mut e.mode else {
            unreachable!()
        };
        *native_memory = Some(r.reserved_bytes as u32 - 1);
        let path = binary(b"unchanged", "batch-artifact");
        let before = fs::read(&path).unwrap();
        assert!(compile_native(&p, "clamp", &path, false, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::remove_file(path).unwrap();
    }
    // Exercise reservation refusal without asking the compiler to emit a huge
    // source literal. These are the checked layout inputs supplied by bodies.
    let large = batched(base, 1024);
    let bodies = vec![Body {
        code: vec![],
        frame_bytes: 8,
        stack_bytes: 8,
        output_bytes: 268_435_456,
    }];
    assert!(layout::plan(entry(&large), &bodies)
        .unwrap_err()
        .message
        .contains("256 MiB"));
    let overflow = vec![Body {
        code: vec![],
        frame_bytes: 8,
        stack_bytes: 8,
        output_bytes: usize::MAX,
    }];
    assert!(layout::plan(entry(&large), &overflow)
        .unwrap_err()
        .message
        .contains("capacity overflow"));
}

#[test]
fn native_result_batches_large_nul_records_and_reused_lanes() {
    let rules = RULES
        .replace("out : text [..29]", "out : text [..9030]")
        .replace(
            "concat(item.title, \":\", item.value)",
            &format!("concat(item.title, \"\0{}\", item.value)", "x".repeat(9000)),
        )
        .replace("native_stack: 192", "native_stack: 16000");
    let base = program(
        &rules,
        "label, clamp, label, nonnegative",
        2,
        Some(1_000_000),
    );
    let p = batched(base.clone(), 8);
    checked(&p);
    let reference = binary(
        &compile(&base, entry(&base)).unwrap(),
        "batch-large-reference",
    );
    let path = binary(&compile(&p, entry(&p)).unwrap(), "batch-large");
    let input = args(&vec![("é", i64::MAX); 17]);
    let a = run(&path, &input);
    let b = run(&reference, &input);
    assert_eq!(
        (a.status.code(), a.stdout, a.stderr),
        (b.status.code(), b.stdout, b.stderr)
    );
    fs::remove_file(path).unwrap();
    fs::remove_file(reference).unwrap();
    let p = batched(
        program(RULES, &vec!["label"; 64].join(", "), 64, Some(1_000_000)),
        32,
    );
    let path = binary(&compile(&p, entry(&p)).unwrap(), "batch-64");
    let out = run(&path, &args(&vec![("é", 7); 33]));
    assert_eq!(
        (out.status.code(), out.stdout),
        (Some(0), "é:7\n".repeat(64 * 33).into_bytes())
    );
    fs::remove_file(path).unwrap();
}
