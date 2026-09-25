use super::*;

pub(super) const SOURCE: &str = include_str!("../../../examples/http_stack.verbose");

pub(super) fn service(p: &mut Program) -> &mut Service {
    p.items.iter_mut().find_map(|i| match i { Item::Service(s) => Some(s), _ => None }).unwrap()
}

pub(super) fn binary(p: &Program) -> Vec<u8> {
    let path = format!("/tmp/verbose-http-stack-layout-{}", std::process::id());
    native::compile_service(p, "bounded_http", &path).unwrap();
    let bytes = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    bytes
}

// Follow machine branches and explicit rsp/rbp changes, independently of all
// AST/layout metadata. Failure edges can join at different depths: close/reset
// must converge before re-entering accept. Unlike argv, exit has no fallthrough.
fn service_machine_peak(code: &[u8]) -> usize {
    service_machine_peak_with_logs(code, &[])
}

// Dynamic allocations receive independent fixture bounds, not report metadata.
// Locations are pinned by the opcode and then validated on reachable CFG paths.
pub(super) fn service_machine_peak_with_logs(code: &[u8], dynamic_buffers: &[usize]) -> usize {
    let sites: Vec<_> = code.windows(3).enumerate().filter_map(|(pc, b)|
        (b == [0x48, 0x29, 0xc4]).then_some(pc)).collect();
    assert_eq!(sites.len(), dynamic_buffers.len());
    let dynamic: HashMap<_, _> = sites.into_iter().zip(dynamic_buffers.iter().copied()).collect();
    #[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
    struct State {
        depth: i64,
        rbp: Option<i64>,
        r10: Option<i64>,
        r9: Option<i64>,
        saved: Vec<(i64, Option<i64>)>,
    }
    let mut work = vec![(0, State::default())];
    let mut seen = std::collections::HashSet::new();
    let mut peak = 0;
    while let Some((pc, mut state)) = work.pop() {
        if pc == code.len() { continue; }
        assert!(pc < code.len(), "invalid branch target {pc}");
        if !seen.insert((pc, state.clone())) { continue; }
        assert!(seen.len() < 100_000, "stack states fail to converge across request reset");
        let len = crate::validate_x86::decode_instruction_length(code, pc)
            .unwrap_or_else(|| panic!("undecodable instruction at {pc}"));
        let ins = &code[pc..pc + len];
        match ins {
            [0x55] => { state.depth += 8; state.saved.push((state.depth, state.rbp)); }
            [0x50..=0x57] | [0x6a, _] => state.depth += 8,
            [0x58..=0x5b] | [0x5e..=0x5f] => state.depth -= 8,
            [0x48, 0x89, 0xe5] => state.rbp = Some(state.depth),
            [0x49, 0x89, 0xea] => state.r10 = state.rbp,
            [0x4c, 0x8b, 0x55, 0x08] => {
                let slot = state.rbp.unwrap() - 8;
                state.r10 = state.saved.iter().find(|(d, _)| *d == slot).expect("missing outer rbp save").1;
            }
            [0x4c, 0x89, 0xd5] => state.rbp = state.r10,
            [0x48, 0x81, 0xec, a, b, c, d] => state.depth += i32::from_le_bytes([*a, *b, *c, *d]) as i64,
            [0x49, 0x89, 0xe1] => state.r9 = Some(state.depth),
            [0x4c, 0x89, 0xcc] => state.depth = state.r9.expect("dynamic log cleanup without save"),
            [0x48, 0x29, 0xc4] => state.depth += *dynamic.get(&pc).expect("unknown dynamic allocation") as i64,
            [0x48, 0x81, 0xc4, a, b, c, d] => state.depth -= i32::from_le_bytes([*a, *b, *c, *d]) as i64,
            [0x48, 0x83, 0xec, n] => state.depth += *n as i8 as i64,
            [0x48, 0x83, 0xc4, n] => state.depth -= *n as i8 as i64,
            [0x48, 0x8d, 0xa5, a, b, c, d] => {
                state.depth = state.rbp.expect("reset without frame") - i32::from_le_bytes([*a, *b, *c, *d]) as i64;
                state.saved.retain(|(d, _)| *d <= state.depth);
                state.r10 = None;
                state.r9 = None;
            }
            [0xe8, ..] | [0xc3] => panic!("unaccounted runtime call/return at {pc}"),
            _ => {}
        }
        assert!((0..3_145_728).contains(&state.depth));
        peak = peak.max(state.depth as usize);
        // All service exits load SYS_exit immediately before optional rdi setup.
        if ins == [0x0f, 0x05] && [7, 10, 14].iter().any(|&n|
            pc >= n && code[pc-n..pc-n+7] == [0x48, 0xc7, 0xc0, 60, 0, 0, 0]) { continue; }
        let next = pc + len;
        let delta = match ins {
            [0xeb, n] | [0x70..=0x7f, n] => Some(*n as i8 as i64),
            [0xe9, a, b, c, d] | [0x0f, 0x80..=0x8f, a, b, c, d] => Some(i32::from_le_bytes([*a, *b, *c, *d]) as i64),
            _ => None,
        };
        if let Some(delta) = delta { work.push(((next as i64 + delta) as usize, state.clone())); }
        if !matches!(ins[0], 0xe9 | 0xeb) { work.push((next, state)); }
    }
    peak
}

#[test]
fn http_stack_layout_covers_all_dispatch_modes_and_request_failure_resets() {
    for mode in ["", "  concurrency: forked\n", "  concurrency: forked\n  max_connections: 4\n",
        "  concurrency: pooled\n  workers: 2\n", "  concurrency: pooled\n  workers: 64\n"] {
        let source = SOURCE.replace("  concurrency: pooled\n  workers: 2\n", mode);
        for max in [64, 4096] {
            let mut p = parse(&source.replace("max_request: 4096", &format!("max_request: {max}")));
            verified(&p);
            let report = native::service_stack_report(&p, "bounded_http").unwrap();
            let bytes = binary(&p);
            assert_eq!(report.stack_bound_bytes(), service_machine_peak(&bytes[120..]), "{mode}, {max}");
            assert_eq!(report.frame_bytes, report.request_buffer_bytes + report.request_metadata_bytes
                + report.io_bookkeeping_bytes + report.dispatch_bookkeeping_bytes);
            service(&mut p).native_stack = Some(report.stack_bound_bytes() as u32);
            verified(&p);
            assert_eq!(bytes, binary(&p));
            service(&mut p).native_stack = None;
            assert_eq!(bytes, binary(&p), "declaration must add no runtime instructions");
            service(&mut p).native_stack = Some(report.stack_bound_bytes() as u32 - 1);
            assert!(verify(&p).iter().any(|e| e.message.contains("exceeds declared")));
        }
    }
}

#[test]
fn http_stack_layout_includes_response_scratch_without_numeric_expressions() {
    let source = SOURCE.replace("view.count * 2", "view.count")
        .replace("out = concat(view.path, \":\", view.count)", "out = view.path")
        .replace("reads: [view.path, view.count]", "reads: [view.path]")
        .replace("count: length(req.body)", "count: 0")
        .replace("reads: [req.path, req.body]", "reads: [req.path]")
        .replace("max_request: 4096", "max_request: 1048576")
        .replace("native_stack: 8192", "native_stack: 2097152");
    for response in ["body", "if length(saved) > 1 then body else \"\"", "\"é\""] {
        let p = parse(&source.replace("status: 200, body: body", &format!("status: 200, body: {response}")));
        verified(&p);
        let report = native::service_stack_report(&p, "bounded_http").unwrap();
        assert_eq!(report.request_metadata_bytes, 48);
        assert_eq!(report.expression_stack_bytes, 0);
        assert_eq!(report.response_stack_bytes, 24);
        assert_eq!(report.stack_bound_bytes(), service_machine_peak(&binary(&p)[120..]));
    }
}

#[test]
fn http_stack_syntax_and_direct_context_refusals_preserve_artifacts() {
    for replacement in ["native_stack: 0", "native_stack: -1", "native_stack: 2097153",
        "native_stack: 1 + 1", "native_stack: \"8192\"", "native_stack: 8192\n  native_stack: 8192"] {
        assert!(Parser::new(Lexer::new(&SOURCE.replace("native_stack: 8192", replacement)).tokenize().unwrap())
            .parse_program().is_err(), "{replacement}");
    }
    let path = format!("/tmp/verbose-http-stack-refusal-{}", std::process::id());
    fs::write(&path, b"preserve").unwrap();
    let mutations: Vec<(&str, Box<dyn Fn(&mut Service)>)> = vec![
        ("[1, 2097152]", Box::new(|s| s.native_stack = Some(0))),
        ("both request_timeout", Box::new(|s| s.request_timeout = None)),
        ("http_1_0", Box::new(|s| s.protocol = Protocol::RawTcp)),
        ("shutdown_timeout", Box::new(|s| s.shutdown_timeout = Some(1))),
        ("require append_file", Box::new(|s| s.logs.push(LogBlock { effect: Effect::Print(vec![Expr::Text("x".into())]), on_error: ErrorPolicy::Drop }))),
        ("bounded-text call graph", Box::new(|s| s.handler = "missing".into())),
    ];
    for (expected, mutate) in mutations {
        let mut p = parse(SOURCE);
        mutate(service(&mut p));
        assert!(verify(&p).iter().any(|e| e.message.contains(expected)), "{:?}", verify(&p));
        let error = native::compile_service(&p, "bounded_http", &path).unwrap_err();
        assert!(error.message.contains(expected), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
    }
    let p = parse(SOURCE);
    let error = crate::wasm::compile_wasm(&p, "describe", &path).unwrap_err();
    assert!(error.message.contains("service native_stack"));
    assert_eq!(fs::read(&path).unwrap(), b"preserve");
    fs::remove_file(path).unwrap();
}

#[test]
fn http_stack_checks_unselected_declarations_and_keeps_argv_proof_meaning() {
    let mut p = parse(SOURCE);
    let mut bad = service(&mut p).clone();
    bad.name = "unselected".into();
    bad.native_stack = Some(1);
    p.items.push(Item::Service(bad));
    assert!(verify(&p).iter().any(|e| e.context.contains("unselected") && e.message.contains("exceeds declared")));
    let mut p = parse(SOURCE);
    rule(&mut p, "describe").proofs.native_stack = Some(8192);
    assert!(verify(&p).iter().any(|e| e.message.contains("not service or reaction contexts")));
}
