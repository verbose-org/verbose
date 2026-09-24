use super::*;
use crate::{lexer::Lexer, parser::Parser, verifier};
use std::{fs, path::Path};

const PROFILE: &str = "  workload:\n    objective: elapsed\n    case interactive:\n      weight: 99\n      records: 1\n      target_us: 2000\n    case bulk:\n      weight: 1\n      records: 4096\n      target_us: 100000\n";
const DECLARATION: &str = "\nexecution inspect\n  @intention: \"predicted reading workload\"\n  @source: execution_stack.intent:1\n  input: Reading\n  mode: sequential\n  phases: [clamp, nonnegative, label]\n  on_failure: stop\n  native_stack: 192\n";
fn source() -> String {
    format!(
        "{}{DECLARATION}",
        include_str!("../../../examples/sequential_stack.verbose")
    )
}
fn parse(s: &str) -> Result<Program, crate::parser::ParseError> {
    Parser::new(Lexer::new(s).tokenize().unwrap()).parse_program()
}
fn profiled() -> Program {
    parse(&format!("{}{PROFILE}", source())).unwrap()
}
fn profile_mut(p: &mut Program) -> &mut Workload {
    let Item::Execution(e) = p.items.last_mut().unwrap() else {
        panic!()
    };
    e.workload.as_mut().unwrap()
}

#[test]
fn workload_parser_closes_nested_fields_and_preserves_following_declarations() {
    let original = format!("{}{PROFILE}", source());
    let p = parse(&original).unwrap();
    assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
    let mut invalid = vec![
        original.replace("  workload:", &format!("{PROFILE}  workload:")),
        original.replace("case bulk:", "case interactive:"),
        original.replace("    objective: elapsed\n", ""),
        format!("{}  workload:\n    objective: elapsed\n", source()),
    ];
    for (old, new) in [
        ("objective: elapsed", "objective: fastest"),
        (
            "objective: elapsed",
            "objective: elapsed\n    objective: cpu",
        ),
        ("objective: elapsed", "prediction: observed"),
        ("weight: 99", "weight: 99\n      weight: 1"),
        ("records: 1", "records: 1\n      records: 3"),
        ("target_us: 2000", "target_us: 2000\n      target_us: 1"),
        ("target_us: 2000", "deadline: 2000"),
        ("      weight: 99\n", ""),
        ("      records: 4096\n", ""),
    ] {
        invalid.push(original.replace(old, new));
    }
    for field in ["weight: 99", "records: 4096", "target_us: 2000"] {
        let key = field.split(':').next().unwrap();
        for value in ["0", "-1", "1.5", "\"1\"", "9223372036854775807"] {
            invalid.push(original.replace(field, &format!("{key}: {value}")));
        }
        let beyond = if key == "target_us" {
            1_000_000_001
        } else {
            1_000_001
        };
        invalid.push(original.replace(field, &format!("{key}: {beyond}")));
    }
    for text in invalid {
        assert!(parse(&text).is_err(), "{text}");
    }
    // The nested parser consumes exactly its own dedents, even in the middle
    // of an execution; the next declaration remains independently checked.
    for text in [
        source().replace(
            "\n  native_stack: 192",
            &format!("\n{PROFILE}  native_stack: 192"),
        ),
        format!(
            "{original}{}",
            DECLARATION.replace("execution inspect", "execution second")
        ),
    ] {
        let p = parse(&text).unwrap();
        assert!(verifier::verify_program(&p, Path::new("examples")).is_empty());
    }
    for name in [
        "workload",
        "case",
        "objective",
        "weight",
        "records",
        "target_us",
    ] {
        let text = source()
            .replace("rule label", &format!("rule {name}"))
            .replace("nonnegative, label]", &format!("nonnegative, {name}]"));
        assert!(verifier::verify_program(&parse(&text).unwrap(), Path::new("examples")).is_empty());
    }
}

#[test]
fn workload_reports_exact_invocation_and_volume_shares_without_pricing_work() {
    let p = profiled();
    let r = report(&p, "inspect").unwrap();
    assert_eq!(
        (r.total_weight(), r.weighted_records(), r.batches(1)),
        (100, 4195, None)
    );
    assert!(r.json().contains(
        "\"expected_full_success_phase_evaluations\":{\"numerator\":12585,\"denominator\":100}"
    ));
    assert!(r.json().contains("\"scope\":\"additional_entry_stack\""));
    assert!(r.json().contains("\"measurements_available\":false"));
    assert_eq!(r.json(), report(&p, "inspect").unwrap().json());
    assert!(r.to_string().contains("99/4195 record volume"));
    for b in [1, 32, 1024] {
        let text = format!("{}{PROFILE}", source())
            .replace("mode: sequential", "mode: concurrent")
            .replace(
                "\n  native_stack: 192",
                &format!("\n  max_in_flight: 2\n  result_batch: {b}"),
            );
        let p = parse(&text).unwrap();
        let r = report(&p, "inspect").unwrap();
        assert_eq!(r.batches(1), Some(3));
        assert_eq!(r.batches(b), Some(3));
        assert_eq!(r.batches(b + 1), Some(6));
        assert!(!r.native_budget_present);
        assert!(r
            .json()
            .contains("\"scope\":\"concurrent_execution_reservation\""));
        assert!(r.native_json.contains("\"declared_bytes\":null"));
        let mut p = p.clone();
        profile_mut(&mut p).objective = WorkloadObjective::Cpu;
        assert!(report(&p, "inspect")
            .unwrap()
            .json()
            .contains("\"measurement_metric\":\"cpu_us\""));
    }
}

#[test]
fn workload_invalid_unselected_ast_refuses_before_reporting_or_artifact_changes() {
    for (kind, expected) in [
        ("weight", "weight"),
        ("records", "records"),
        ("target", "target_us"),
        ("empty", "1..=16"),
        ("many", "1..=16"),
        ("duplicate", "duplicate"),
        ("name", "identifier"),
    ] {
        let mut p = profiled();
        let w = profile_mut(&mut p);
        match kind {
            "weight" => w.cases[0].weight = 0,
            "records" => w.cases[0].records = u32::MAX,
            "target" => w.cases[0].target_us = Some(0),
            "empty" => w.cases.clear(),
            "many" => w.cases = vec![w.cases[0].clone(); 17],
            "duplicate" => w.cases[1].name = w.cases[0].name.clone(),
            _ => w.cases[0].name = "bad\"name".into(),
        }
        assert!(report(&p, "clamp").unwrap_err().message.contains(expected));
        let path = format!("/tmp/verbose-workload-refuse-{}", std::process::id());
        fs::write(&path, b"preserve").unwrap();
        assert!(native::compile_native(&p, "clamp", &path, false, false)
            .unwrap_err()
            .message
            .contains(expected));
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn workload_maximum_arithmetic_and_repeated_phases_are_exact() {
    let cases = (0..16).map(|n| format!("    case c{n}:\n      weight: 1000000\n      records: 1000000\n      target_us: 1000000000\n")).collect::<String>();
    let text = format!("{}  workload:\n    objective: cpu\n{cases}", source()).replace(
        "[clamp, nonnegative, label]",
        &format!("[{}]", vec!["clamp"; 64].join(",")),
    );
    let p = parse(&text).unwrap();
    let r = report(&p, "inspect").unwrap();
    assert_eq!(r.weighted_records(), 16_000_000_000_000);
    assert!(r
        .json()
        .contains("\"numerator\":1024000000000000,\"denominator\":16000000"));
    let text = format!("{text}    case extra:\n      weight: 1\n      records: 1\n");
    assert!(parse(&text).unwrap_err().message.contains("1..=16"));
}
