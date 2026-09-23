use super::*;

const SOURCE: &str = include_str!("../../../examples/retained_stack.verbose");

#[test]
fn retained_record_aliases_are_counted_once_and_composition_has_its_own_budget() {
    let mut p = parse(SOURCE);
    verified(&p);
    let report = native::stack_report(&p, "analyze").unwrap();
    let frame = report.text_frame.as_ref().unwrap();
    assert_eq!(report.stack_bound_bytes(), 288);
    assert_eq!((frame.slot_bytes, frame.buffer_bytes), (88, 96));
    let values: Vec<_> = frame
        .calls
        .iter()
        .map(|c| {
            (
                c.callee.as_str(),
                c.parent_call,
                c.live_caller_buffer_capacity_bytes,
                c.retained_caller_buffer_capacity_bytes,
            )
        })
        .collect();
    assert_eq!(
        values,
        vec![
            ("prepare", None, 48, 48),
            ("forward", None, 64, 64),
            ("render", None, 64, 64)
        ]
    );
    let bytes = native_bytes(&p, "analyze");
    assert_eq!(machine_stack_peak(&bytes[120..]), 288);
    let aliases = parse(&SOURCE.replace(
        "    let staged = Prepared",
        "    let alias = preserved\n    let preserved = alias\n    let staged = Prepared",
    ));
    verified(&aliases);
    assert_eq!(report, native::stack_report(&aliases, "analyze").unwrap());
    assert_eq!(bytes, native_bytes(&aliases, "analyze"));
    // A renamed record is still live if any later alias consumes its title.
    let dead = parse(&SOURCE.replace(
        "concat(rendered, \" | \", preserved.title)",
        "concat(rendered, \" | \")",
    ));
    verified(&dead);
    let dead = native::stack_report(&dead, "analyze").unwrap();
    let last = dead.text_frame.as_ref().unwrap().calls.last().unwrap();
    assert_eq!(last.live_caller_buffer_capacity_bytes, 64);
    assert_eq!(last.retained_caller_buffer_capacity_bytes, 48);
    rule(&mut p, "analyze").proofs.native_stack = None;
    assert_eq!(bytes, native_bytes(&p, "analyze"));
    rule(&mut p, "analyze").proofs.native_stack = Some(287);
    assert!(verify(&p)
        .iter()
        .any(|e| e.context.contains("analyze")
            && e.message.contains("288 bytes exceeds declared 287")));
    let path = format!("/tmp/verbose-retention-refusal-{}", std::process::id());
    fs::write(&path, b"existing").unwrap();
    assert!(native::compile_native(&p, "analyze", &path, false, false).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"existing");
    fs::remove_file(path).unwrap();
}

#[test]
fn nested_call_reports_include_the_active_parent_destination_without_adding_code() {
    let wrapper = r#"
rule wrapper
  @intention: "Forward a text destination through a nested call"
  @source: retained_stack.intent:5
  input:
    record : Prepared
  output:
    out : text [..31]
  logic:
    out = render(record)
  proofs:
    purity:
      reads: [record]
      calls: [render]
    termination:
      bound: 10
"#;
    let p = parse(
        &(SOURCE
            .replace("render(preserved)", "wrapper(preserved)")
            .replace("[prepare, forward, render]", "[prepare, forward, wrapper]")
            + wrapper),
    );
    verified(&p);
    let report = native::stack_report(&p, "analyze").unwrap();
    let calls = &report.text_frame.as_ref().unwrap().calls;
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[2].callee, "wrapper");
    assert_eq!(calls[2].parent_call, None);
    assert_eq!(calls[2].retained_caller_buffer_capacity_bytes, 64);
    assert_eq!(calls[3].callee, "render");
    assert_eq!(calls[3].parent_call, Some(3));
    assert_eq!(calls[3].live_caller_buffer_capacity_bytes, 96);
    assert_eq!(calls[3].retained_caller_buffer_capacity_bytes, 96);
    assert_eq!(
        native_bytes(&p, "analyze"),
        native_bytes(&parse(SOURCE), "analyze")
    );
    let sequence = native::sequential_stack_report(&p, &["analyze", "analyze"]).unwrap();
    assert_eq!(sequence.phases, vec![report.clone(), report]);
    assert_eq!(sequence.stack_bound_bytes(), 288);
}
