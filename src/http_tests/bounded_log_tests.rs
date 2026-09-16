use super::admission_tests::{children, wait_count};
use super::text_bounds_tests::storage_accept_stack;
use super::*;

const SOURCE: &str = include_str!("../../examples/http_bounded_log.verbose");
const CONTENT: &str = r#"concat(req.timestamp, " ", req.method, " ", req.path, " ", resp.status, " ", resp.body, "\n")"#;

fn source(path: &str, content: &str) -> String {
    SOURCE
        .replace("port: 18966", "port: 18960")
        .replace("service bounded_log_http", "service bounded_http")
        .replace("/tmp/verbose_bounded_response.log", path)
        .replace(CONTENT, content)
}

#[test]
fn bounded_logs_borrow_binary_responses_and_reclaim_across_worker_modes() {
    let path = format!("/tmp/verbose-bounded-log-{}", std::process::id());
    for deadlines in [false, true] {
        for mode in [
            "",
            "  concurrency: forked\n",
            "  concurrency: pooled\n  workers: 2\n",
        ] {
            if !deadlines && mode.contains("pooled") {
                continue;
            }
            let _ = fs::remove_file(&path);
            let mut src = source(
                &path,
                r#"concat("A:", req.method, ":", req.path, ":", req.body, ":", resp.status, ":", resp.body, "\n")"#,
            );
            src.push_str(&format!("  log:\n    append_file \"{path}\" concat(\"B:\", resp.body, \"\\n\")\n    on_error: abort\n{mode}"));
            if !deadlines {
                src = src.replace("  request_timeout: 2\n  response_timeout: 2\n", "");
            }
            let server = Server::start(&src);
            let pids = if mode.contains("pooled") {
                wait_count(&server, 2)
            } else if mode.is_empty() {
                vec![server.child.id()]
            } else {
                vec![]
            };
            let before: Vec<_> = pids
                .iter()
                .map(|p| {
                    (
                        storage_accept_stack(*p),
                        fs::read_dir(format!("/proc/{p}/fd")).unwrap().count(),
                    )
                })
                .collect();
            let mut expected = Vec::new();
            for i in 0..24 {
                // Legacy reception is a single read; keep its fixture within
                // one small write. Deadlined reception also covers large bodies.
                let body = match i % 4 {
                    0 if deadlines => vec![b'x'; 3900],
                    0 => vec![b'x'; 64],
                    1 => vec![],
                    2 => vec![0, 0xff, 0xc3, 0xa9, b'\n', 0],
                    _ => b"small".to_vec(),
                };
                let mut request = format!(
                    "POST /entry HTTP/1.0\r\nContent-Length: {}\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                request.extend_from_slice(&body);
                let mut result = b"[".to_vec();
                result.extend_from_slice(&body);
                result.push(b']');
                assert_eq!(server.request(&request), wire(&result));
                expected.extend_from_slice(b"A:POST:/entry:");
                expected.extend_from_slice(&body);
                expected.extend_from_slice(b":200:");
                expected.extend_from_slice(&result);
                expected.extend_from_slice(b"\nB:");
                expected.extend_from_slice(&result);
                expected.push(b'\n');
                assert_eq!(fs::read(&path).unwrap(), expected);
                assert!(server.request(b"invalid\r\n\r\n").is_empty());
                assert_eq!(
                    fs::read(&path).unwrap(),
                    expected,
                    "malformed client was logged"
                );
            }
            assert_eq!(
                server.request(b"GET /after HTTP/1.0\r\n\r\n"),
                wire(b"</after>")
            );
            expected.extend_from_slice(b"A:GET:/after::200:</after>\nB:</after>\n");
            assert_eq!(fs::read(&path).unwrap(), expected);
            if mode.contains("pooled") {
                assert_eq!(children(&server), pids);
            }
            for (p, expected) in pids.iter().zip(before) {
                assert_eq!(
                    (
                        storage_accept_stack(*p),
                        fs::read_dir(format!("/proc/{p}/fd")).unwrap().count()
                    ),
                    expected
                );
            }
            drop(server);
        }
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn bounded_logs_read_parser_fields_even_when_only_the_log_uses_body() {
    let path = format!("/tmp/verbose-bounded-log-scope-{}", std::process::id());
    let _ = fs::remove_file(&path);
    let src = source(&path, CONTENT)
        .replace("out = if request.method == \"POST\" then concat(\"[\", request.body, \"]\") else concat(\"<\", request.path, \">\")", "out = concat(\"<\", request.path, \">\")")
        .replace("[request.method, request.body, request.path]", "[request.path]")
        .replace("resp.status, \" \", resp.body", "resp.status, \" \", req.body, \" \", resp.body")
        .replace("let message = \"shadow\"", "let message = \"shadow\"\n    let body = \"shadow\"\n    let req = \"shadow\"\n    let resp = \"shadow\"");
    let server = Server::start(&src);
    assert_eq!(
        server.request(b"POST /only-log HTTP/1.0\r\nContent-Length: 4\r\n\r\nx\0\xffz"),
        wire(b"</only-log>")
    );
    let logged = fs::read(&path).unwrap();
    let split = logged.iter().position(|b| *b == b' ').unwrap();
    assert!(
        std::str::from_utf8(&logged[..split])
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert_eq!(
        &logged[split..],
        b" POST /only-log 200 x\0\xffz </only-log>\n"
    );
    drop(server);
    fs::remove_file(path).unwrap();
}

#[test]
fn bounded_logs_keep_open_and_write_error_policies() {
    for target in [
        "/dev/full".to_string(),
        format!("/tmp/verbose-no-directory-{}/log", std::process::id()),
    ] {
        for policy in ["drop", "abort"] {
            let path = format!("/tmp/verbose-bounded-log-later-{}", std::process::id());
            let _ = fs::remove_file(&path);
            let mut src = source(&target, r#"concat(resp.body)"#)
                .replace("on_error: abort", &format!("on_error: {policy}"));
            src.push_str(&format!("  log:\n    append_file \"{path}\" \"later\"\n"));
            let mut server = Server::start(&src);
            let response = server.request(b"GET /failure HTTP/1.0\r\n\r\n");
            if policy == "drop" {
                assert_eq!(response, wire(b"</failure>"));
                assert_eq!(fs::read(&path).unwrap(), b"later");
                assert_eq!(
                    server.request(b"GET /again HTTP/1.0\r\n\r\n"),
                    wire(b"</again>")
                );
                fs::remove_file(&path).unwrap();
            } else {
                assert!(response.is_empty());
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    if let Some(status) = server.child.try_wait().unwrap() {
                        assert_eq!(status.code(), Some(1));
                        break;
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(!Path::new(&path).exists(), "later log ran after abort");
            }
        }
    }
}

#[test]
fn bounded_logs_abort_terminates_pool_after_worker_failure() {
    let src = format!(
        "{}  concurrency: pooled\n  workers: 2\n",
        source("/dev/full", "concat(resp.body)")
    );
    let mut server = Server::start(&src);
    wait_count(&server, 2);
    assert!(server.request(b"GET /failure HTTP/1.0\r\n\r\n").is_empty());
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(1));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pool did not terminate after log failure"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn bounded_logs_reject_unknown_or_excessive_content_before_artifact_emission() {
    let file = format!("/tmp/verbose-bounded-log-refused-{}", std::process::id());
    let cases = [
        (
            source("/tmp/log", "concat(concat(resp.body))"),
            "flat concat",
        ),
        (source("/tmp/log", "resp.body"), "text literal or concat"),
        (
            source("/tmp/log", "concat(length(resp.body))"),
            "concat only",
        ),
        (
            source("/tmp/log", "concat(json_escape(resp.body))"),
            "concat only",
        ),
        (
            source("/tmp/log", "concat(parse_int(req.body))"),
            "concat only",
        ),
        (
            source("/tmp/log", "concat(response_text(req))"),
            "concat only",
        ),
        (source("/tmp/log", "concat(saved)"), "concat only"),
        (
            source("/tmp/log", "concat(resp.missing)"),
            "unknown record field",
        ),
        (
            source("/tmp/log", "concat(resp.body, resp.body)").replace("[..4098]", "[..1048576]"),
            "1048576",
        ),
        (
            format!(
                "{}  state:\n    count : number = 0\n",
                source("/tmp/log", "concat(resp.body)")
            ),
            "without state",
        ),
    ];
    for (src, diagnostic) in cases {
        let p = parse(&src);
        let errors = crate::verifier::verify_program(&p, Path::new("examples"));
        assert!(
            errors.iter().any(|e| e.message.contains(diagnostic)),
            "{diagnostic}: {errors:?}"
        );
        fs::write(&file, b"preserve").unwrap();
        let error = crate::native::compile_service(&p, "bounded_http", &file).unwrap_err();
        assert!(error.message.contains(diagnostic), "{error}");
        assert_eq!(fs::read(&file).unwrap(), b"preserve");
        fs::remove_file(&file).unwrap();
        assert!(crate::native::compile_service(&p, "bounded_http", &file).is_err());
        assert!(!Path::new(&file).exists());
    }
    let p = parse(
        &source("/tmp/log", "concat(resp.body)")
            .replace("  request_timeout: 2\n  response_timeout: 2\n", ""),
    );
    let error = crate::wasm::compile_wasm(&p, "bounded_http", &file).unwrap_err();
    assert!(error.message.contains("bounded text"), "{error}");
    assert!(!Path::new(&file).exists());
}

#[test]
fn bounded_logs_content_capacity_boundary_uses_public_result_contract() {
    let path = format!("/tmp/verbose-bounded-log-cap-{}", std::process::id());
    let _ = fs::remove_file(&path);
    let src = source(&path, "concat(resp.body)")
        .replace("[..4098]", "[..1048576]")
        .replace("    let unused = response_text(incoming)\n", "");
    let server = Server::start(&src);
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b"</>"));
    assert_eq!(fs::read(&path).unwrap(), b"</>");
    drop(server);
    fs::remove_file(path).unwrap();
    let p = parse(&src.replace("concat(resp.body)", "concat(resp.body, \"x\")"));
    assert!(crate::text_bounds::verify(&p)
        .iter()
        .any(|e| e.message.contains("1048577 bytes")));
}
