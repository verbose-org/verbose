use super::*;
use super::http::{SOURCE, service, binary, service_machine_peak_with_logs};

fn log(content: &str, policy: &str) -> String {
    format!("  log:\n    append_file \"/tmp/unused-http-stack.log\" {content}\n    on_error: {policy}\n")
}

#[test]
fn http_log_stack_matches_static_and_dynamic_emission_in_every_dispatch_mode() {
    // Independent bounds: a path is <=256, method <=8, request body <=4096,
    // and describe's public output is <=277. Native numbers reserve 21 bytes
    // but produce <=20; dynamic method/path lengths add to their static bounds.
    for mode in ["", "  concurrency: forked\n", "  concurrency: forked\n  max_connections: 4\n",
        "  concurrency: pooled\n  workers: 2\n", "  concurrency: pooled\n  workers: 64\n"] {
        let source = SOURCE.replace("  concurrency: pooled\n  workers: 2\n", mode)
            .replace("native_stack: 8192", "native_stack: 2097152");
        for (content, strategy, capacity, buffer, sizing, formatting, timestamp) in [
            ("\"\"", "literal", 0, 0, 0, 0, false),
            ("\"ab\"", "literal", 2, 0, 0, 0, false),
            ("concat(\"ab\", resp.status)", "static", 22, 24, 0, 24, false),
            ("concat(req.method, req.path)", "static", 264, 264, 0, 0, false),
            ("concat(resp.body)", "dynamic", 277, 280, 0, 0, false),
            ("concat(req.method, req.path, resp.body, resp.status)", "dynamic", 561, 832, 8, 24, false),
            ("concat(req.path, req.path, resp.body)", "dynamic", 789, 1304, 8, 0, false),
            ("concat(req.timestamp, req.body, resp.body)", "dynamic", 4393, 4400, 0, 24, true),
        ] {
            for policy in ["drop", "abort"] {
                let mut p = parse(&(source.clone() + &log(content, policy)));
                verified(&p);
                let report = native::service_stack_report(&p, "bounded_http").unwrap();
                assert_eq!(report.request_metadata_bytes, if timestamp { 72 } else { 64 });
                assert_eq!(report.logs.len(), 1);
                let l = &report.logs[0];
                assert_eq!((l.strategy, l.content_capacity_bytes, l.buffer_bytes,
                    l.sizing_stack_bytes, l.formatting_stack_bytes, l.on_error),
                    (strategy, capacity, buffer, sizing, formatting, policy), "{content}");
                let bytes = binary(&p);
                let dynamic = if strategy == "dynamic" { vec![buffer] } else { vec![] };
                assert_eq!(report.stack_bound_bytes(), service_machine_peak_with_logs(&bytes[120..], &dynamic), "{mode}: {content}");
                service(&mut p).native_stack = Some(report.stack_bound_bytes() as u32);
                verified(&p);
                assert_eq!(binary(&p), bytes);
                service(&mut p).native_stack = None;
                assert_eq!(binary(&p), bytes);
                service(&mut p).native_stack = Some(report.stack_bound_bytes() as u32 - 1);
                assert!(verify(&p).iter().any(|e| e.message.contains("exceeds declared")));
            }
        }
    }
}

#[test]
fn http_log_stack_combines_sequential_consumers_by_maximum_and_counts_parser_only_fields() {
    let source = SOURCE.replace("native_stack: 8192", "native_stack: 32768")
        .replace("count: length(req.body)", "count: 0").replace("reads: [req.path, req.body]", "reads: [req.path]");
    let plain = native::service_stack_report(&parse(&source), "bounded_http").unwrap();
    assert_eq!(plain.request_metadata_bytes, 48);
    let mut p = parse(&(source + &log("concat(req.body, resp.body, req.timestamp)", "abort")
        + &log("concat(req.timestamp, resp.status)", "drop") + &log("\"done\"", "abort")));
    verified(&p);
    let report = native::service_stack_report(&p, "bounded_http").unwrap();
    assert_eq!(report.request_metadata_bytes, 72); // counted body + one shared timestamp
    assert_eq!(report.logs.iter().map(|l| l.stack_bound_bytes()).collect::<Vec<_>>(), [4424, 72, 0]);
    assert_eq!(report.log_stack_bytes(), 4424);
    assert_eq!(report.stack_bound_bytes(), 8 + report.frame_bytes + report.handler_frame.frame_bytes() + 16 + 4424);
    assert_eq!(report.stack_bound_bytes(), service_machine_peak_with_logs(&binary(&p)[120..], &[4400]));
    service(&mut p).logs.remove(1);
    assert_eq!(native::service_stack_report(&p, "bounded_http").unwrap().stack_bound_bytes(), report.stack_bound_bytes());
    // A counted empty response allocates zero in the dynamic concat path.
    let source = SOURCE.replace("body: body }", "body: \"\" }") + &log("concat(resp.body)", "drop");
    let p = parse(&source);
    verified(&p);
    let r = native::service_stack_report(&p, "bounded_http").unwrap();
    assert_eq!(r.log_stack_bytes(), 0);
    assert_eq!(r.stack_bound_bytes(), service_machine_peak_with_logs(&binary(&p)[120..], &[0]));
}

#[test]
fn http_log_stack_unknown_or_excessive_effects_preserve_existing_artifacts() {
    let path = format!("/tmp/verbose-http-log-stack-refusal-{}", std::process::id());
    fs::write(&path, b"preserve").unwrap();
    for (content, expected) in [("resp.body", "text literal or concat"),
        ("concat(concat(resp.body))", "flat concat"), ("concat(length(resp.body))", "concat only"),
        ("concat(describe(req))", "concat only"), ("concat(resp.missing)", "unknown record field")] {
        let p = parse(&(SOURCE.to_string() + &log(content, "drop")));
        let error = native::compile_service(&p, "bounded_http", &path).unwrap_err();
        assert!(error.message.contains(expected), "{error}");
        assert_eq!(fs::read(&path).unwrap(), b"preserve");
    }
    let source = SOURCE.replace("[..277]", "[..1048576]") + &log("concat(resp.body, resp.body)", "drop");
    let error = native::compile_service(&parse(&source), "bounded_http", &path).unwrap_err();
    assert!(error.message.contains("1048576"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), b"preserve");
    fs::remove_file(path).unwrap();
}

#[test]
fn http_log_stack_counts_typed_literal_bytes_and_signed_number_scratch() {
    // Exercise the emitter boundary directly; the legacy source lexer expands
    // non-ASCII literal bytes when constructing its String (a separate gap).
    let mut p = parse(SOURCE);
    service(&mut p).logs = vec![LogBlock {
        effect: Effect::AppendFile { path: "/tmp/unused-http-stack.log".into(),
            content: Expr::Concat(vec![Expr::Text("é\0".into()), Expr::Number(i64::MIN)]) },
        on_error: ErrorPolicy::Abort,
    }];
    verified(&p);
    let report = native::service_stack_report(&p, "bounded_http").unwrap();
    assert_eq!(report.logs[0].content_capacity_bytes, 23);
    assert_eq!(report.logs[0].buffer_bytes, 24);
    assert_eq!(report.log_stack_bytes(), 48);
    assert_eq!(report.stack_bound_bytes(), service_machine_peak_with_logs(&binary(&p)[120..], &[]));
}
