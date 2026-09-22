use super::*;

fn fixture() -> Program {
    parse(include_str!("../../../examples/sequential_stack.verbose"))
}
fn sequence_bytes(p: &Program, names: &[&str]) -> Vec<u8> {
    let path = format!("/tmp/verbose-sequence-stack-{}", std::process::id());
    native::compile_native_multi(p, names, &path, false, false).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    bytes
}

#[test]
fn sequential_stack_is_maximum_of_released_frames_and_matches_instructions() {
    let numeric = parse(include_str!("../../../examples/native_stack.verbose"));
    let text = parse(include_str!("../../../examples/text_stack.verbose"));
    let records = parse(include_str!(
        "../../../examples/bounded_text_branches.verbose"
    ));
    for (mut p, names) in [
        (numeric, vec!["clamp", "magnitude", "clamp"]),
        (
            text,
            vec!["repeat_reading", "format_reading", "repeat_reading"],
        ),
        (records, vec!["pack_text", "compose_text", "pack_text"]),
        (fixture(), vec!["clamp", "nonnegative", "label"]),
        (fixture(), vec!["label", "clamp", "nonnegative", "label"]),
        (fixture(), vec!["label"; 64]),
    ] {
        verified(&p);
        let report = native::sequential_stack_report(&p, &names).unwrap();
        assert_eq!(report.phases.len(), names.len());
        for (name, phase) in names.iter().zip(&report.phases) {
            assert_eq!(phase, &native::stack_report(&p, name).unwrap());
        }
        for (name, phase) in names.iter().zip(&report.phases) {
            rule(&mut p, name).proofs.native_stack = Some(phase.stack_bound_bytes() as u32);
        }
        verified(&p);
        let bytes = sequence_bytes(&p, &names);
        assert_eq!(
            machine_stack_peak(&bytes[120..]),
            report.stack_bound_bytes(),
            "{names:?}"
        );
        assert_eq!(
            report.stack_bound_bytes(),
            report
                .phases
                .iter()
                .map(Report::stack_bound_bytes)
                .max()
                .unwrap()
        );
        assert!(
            report.stack_bound_bytes() < report.phases.iter().map(Report::stack_bound_bytes).sum()
        );
        for item in &mut p.items {
            if let Item::Rule(r) = item {
                r.proofs.native_stack = None;
            }
        }
        assert_eq!(
            bytes,
            sequence_bytes(&p, &names),
            "declarations add no instructions"
        );
        let small = &report.phases[0];
        rule(&mut p, &small.rule).proofs.native_stack = Some(small.stack_bound_bytes() as u32 - 1);
        assert!(native::sequential_stack_report(&p, &names)
            .unwrap_err()
            .message
            .contains("exceeds declared"));
    }
}

fn differential(p: &Program, names: &[&str], args: &[&str]) {
    let path = format!("/tmp/verbose-sequence-run-{}", std::process::id());
    native::compile_native_multi(p, names, &path, false, false).unwrap();
    let actual = Command::new(&path).args(args).output().unwrap();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = Some(0);
    // Operational oracle: execute the same standalone entries in order,
    // stopping on the first nonzero result, exactly like a shell && chain.
    for name in names {
        native::compile_native(p, name, &path, false, false).unwrap();
        let out = Command::new(&path).args(args).output().unwrap();
        stdout.extend(out.stdout);
        stderr.extend(out.stderr);
        status = out.status.code();
        if status != Some(0) {
            break;
        }
    }
    assert_eq!(
        (actual.status.code(), actual.stdout, actual.stderr),
        (status, stdout, stderr),
        "{names:?}, {args:?}"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn sequential_phases_preserve_outputs_and_stop_after_failed_phase() {
    let p = fixture();
    for names in [
        vec!["clamp", "nonnegative", "label"],
        vec!["label", "nonnegative", "clamp"],
        vec!["clamp", "label", "clamp", "label"],
    ] {
        for args in [
            vec!["éééé", "9223372036854775807", "", "0"],
            vec!["x", "-9223372036854775808", "y", "1"],
            vec!["x", "1", "y", "-1", "z", "2"],
            vec![],
            vec!["unfinished"],
            vec!["ok", "1", "unfinished"],
            vec!["ééééé", "1"],
            vec!["x", "9223372036854775808"],
        ] {
            differential(&p, &names, &args);
        }
    }
    let path = format!("/tmp/verbose-sequence-order-{}", std::process::id());
    native::compile_native_multi(&p, &["clamp", "nonnegative", "label"], &path, false, false)
        .unwrap();
    let out = Command::new(&path)
        .args(["x", "2", "y", "-1", "z", "3"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"2\n-1\n3\ntrue\nfalse\ntrue\n");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stderr.is_empty());
    let out = Command::new(&path)
        .args(["a", "2", "b", "1000"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"2\n100\ntrue\ntrue\na:2\nb:1000\n");
    assert!(out.status.success());
    fs::remove_file(path).unwrap();
}

#[test]
fn sequential_refusals_validate_all_phases_before_writing() {
    let mut p = fixture();
    let mut legacy = rule(&mut p, "clamp").clone();
    legacy.name = "legacy".into();
    legacy.hints = None;
    legacy.proofs.native_stack = None;
    p.items.push(Item::Rule(legacy));
    verified(&p);
    let path = format!("/tmp/verbose-sequence-refused-{}", std::process::id());
    fs::write(&path, b"existing artifact").unwrap();
    for names in [
        vec!["clamp", "missing"],
        vec!["clamp", "legacy"],
        vec!["legacy", "clamp"],
        vec!["clamp", ""],
        vec!["clamp"; 65],
    ] {
        assert!(
            native::sequential_stack_report(&p, &names).is_err(),
            "{names:?}"
        );
        assert!(native::compile_native_multi(&p, &names, &path, false, false).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"existing artifact");
    }
    for names in [vec![], vec!["clamp"]] {
        assert!(native::sequential_stack_report(&p, &names)
            .unwrap_err()
            .message
            .contains("2..=64"));
    }
    for (stdin, stream) in [(true, false), (false, true), (true, true)] {
        assert!(
            native::compile_native_multi(&p, &["clamp", "label"], &path, stdin, stream)
                .unwrap_err()
                .message
                .contains("argv records only")
        );
    }
    let other_input = parse(include_str!(
        "../../../examples/bounded_text_branches.verbose"
    ));
    let names = ["pack_text", "render_text"];
    assert!(native::sequential_stack_report(&other_input, &names)
        .unwrap_err()
        .message
        .contains("same input concept"));
    assert!(native::compile_native_multi(&other_input, &names, &path, false, false).is_err());
    // Even an unselected helper's declaration remains independently checked.
    rule(&mut p, "nonnegative").proofs.native_stack = Some(1);
    assert!(
        native::compile_native_multi(&p, &["clamp", "label"], &path, false, false)
            .unwrap_err()
            .message
            .contains("nonnegative")
    );
    assert_eq!(fs::read(&path).unwrap(), b"existing artifact");
    fs::remove_file(path).unwrap();
}
