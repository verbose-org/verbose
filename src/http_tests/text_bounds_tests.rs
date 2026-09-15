use super::*;

#[test]
fn bounded_text_http_composes_pure_rules() {
    let source = include_str!("../../examples/http_bounded_text.verbose")
        .replace("port: 18964", "port: 18960")
        .replace("service bounded_text_http", "service bounded_http");
    let server = Server::start(&source);
    for path in [
        "/".to_string(),
        format!("/{}", "x".repeat(255)),
        "/caf%C3%A9".to_string(),
    ] {
        let request = format!("GET {path} HTTP/1.0\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(
            server.request(request.as_bytes()),
            wire(format!("path: {path}").as_bytes())
        );
    }
    let request = format!("GET /{} HTTP/1.0\r\n\r\n", "x".repeat(256));
    assert!(server.request(request.as_bytes()).is_empty());
    assert_eq!(
        server.request(b"GET /again HTTP/1.0\r\n\r\n"),
        wire(b"path: /again")
    );
}

#[test]
fn bounded_text_http_uses_every_service_input_bound() {
    let source = include_str!("../../examples/http_bounded_text.verbose")
        .replace("request.path", "request.body")
        .replace("[..262]", "[..4102]");
    let p = parse(&source);
    assert!(crate::text_bounds::verify(&p).is_empty());
    let mut p = p.clone();
    let mut larger = p
        .items
        .iter()
        .find_map(|i| match i {
            Item::Service(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    larger.name = "larger".into();
    larger.max_request = 8192;
    p.items.push(Item::Service(larger));
    assert!(crate::text_bounds::verify(&p)
        .iter()
        .any(|e| e.message.contains("exceeds declared")));
}

fn storage_accept_stack(pid: u32) -> String {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let s = fs::read_to_string(format!("/proc/{pid}/syscall")).unwrap();
        let fields: Vec<_> = s.split_whitespace().collect();
        if fields.first() == Some(&"43") {
            return fields[7].to_string();
        }
        assert!(Instant::now() < deadline, "worker did not return to accept");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn text_storage_http_reuses_frames_with_binary_bodies_and_shadowing() {
    use super::admission_tests::{children, wait_count};
    let source = include_str!("../../examples/http_bounded_text.verbose")
        .replace("port: 18964", "port: 18960")
        .replace("service bounded_text_http", "service bounded_http")
        .replace("[..262]", "[..4098]")
        .replace("[request.path]", "[request.path, request.body, request.method]")
        .replace("bound : 8", "bound : 128")
        .replace("out = concat(prefix, request.path)", "out = if request.method == \"POST\" then concat(\"[\", if length(request.body) > 0 then concat(\"\", request.body) else \"\", \"]\") else concat(\"<\", request.path, \">\")")
        .replace("resp = HttpResponse { status: 200, body: concat(\"\", response_text(req)) }", "let message = response_text(req)\n    let saved = message\n    let message = \"shadow\"\n    resp = HttpResponse { status: 200, body: concat(saved, \":\", message) }");
    for pooled in [false, true] {
        let source = if pooled {
            format!("{source}  concurrency: pooled\n  workers: 2\n")
        } else {
            source.clone()
        };
        let server = Server::start(&source);
        let pids = if pooled {
            wait_count(&server, 2)
        } else {
            vec![server.child.id()]
        };
        let stacks: Vec<_> = pids.iter().map(|p| storage_accept_stack(*p)).collect();
        let descriptors: Vec<_> = pids
            .iter()
            .map(|p| fs::read_dir(format!("/proc/{p}/fd")).unwrap().count())
            .collect();
        for i in 0..150 {
            let body = match i % 4 {
                0 => vec![b'x'; 3900],
                1 => vec![],
                2 => vec![0, 0xc3, 0xa9, 0xff, 0, b'z'],
                _ => vec![b'y'],
            };
            let mut request =
                format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            request.extend_from_slice(&body);
            let mut expected = b"[".to_vec();
            expected.extend_from_slice(&body);
            expected.extend_from_slice(b"]:shadow");
            assert_eq!(server.request(&request), wire(&expected));
            if i % 25 == 0 {
                assert!(server.request(b"invalid\r\n\r\n").is_empty());
                assert_eq!(
                    server.request(b"GET /after HTTP/1.0\r\n\r\n"),
                    wire(b"</after>:shadow")
                );
            }
        }
        if pooled {
            assert_eq!(children(&server), pids);
        }
        for (i, p) in pids.iter().enumerate() {
            assert_eq!(
                storage_accept_stack(*p),
                stacks[i],
                "bounded text frame leaked between requests"
            );
            assert_eq!(
                fs::read_dir(format!("/proc/{p}/fd")).unwrap().count(),
                descriptors[i]
            );
        }
    }
}

#[test]
fn text_storage_http_preserves_literal_status_guard() {
    let p = parse(
        &include_str!("../../examples/http_bounded_text.verbose")
            .replace("status: 200", "status: 1000"),
    );
    let out = format!("/tmp/verbose-storage-http-refused-{}", std::process::id());
    assert!(!Path::new(&out).exists());
    let error = crate::native::compile_service(&p, "bounded_text_http", &out).unwrap_err();
    assert!(
        error.message.contains("outside HTTP valid range"),
        "{error}"
    );
    assert!(!Path::new(&out).exists());
}

#[test]
fn text_destinations_http_forwards_tail_calls_in_every_worker_mode() {
    let source = include_str!("../../examples/http_bounded_text.verbose")
        .replace("port: 18964", "port: 18960")
        .replace("service bounded_text_http", "service bounded_http")
        .replace("[..262]", "[..4098]")
        .replace("[request.path]", "[request.path, request.body, request.method]")
        .replace("bound : 8", "bound : 64")
        .replace("out = concat(prefix, request.path)", "out = if request.method == \"POST\" then concat(\"[\", request.body, \"]\") else concat(\"<\", request.path, \">\")")
        .replace("body: concat(\"\", response_text(req))", "body: relay(req)")
        .replace("calls : [response_text]", "calls : [relay]");
    // An inferred-capacity relay must forward the HTTP counted body just as
    // an annotated rule does. Only the selected output branch may fill it.
    let relay = r#"
rule relay
  @intention: "Forward the counted response body"
  @source: http_bounded_text.intent:2
  input:
    other : HttpRequest
  output:
    out : text
  logic:
    out = response_text(other)
  proofs:
    purity:
      reads : [other]
      calls : [response_text]
    termination:
      bound : 8
"#;
    for mode in [
        "",
        "  concurrency: forked\n  max_connections: 2\n",
        "  concurrency: pooled\n  workers: 2\n",
    ] {
        let server = Server::start(&format!("{source}{mode}{relay}"));
        for body in [vec![], b"\0\xc3\xa9\xff\0z".to_vec(), vec![b'x'; 3900]] {
            let mut request =
                format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            request.extend_from_slice(&body);
            let mut expected = b"[".to_vec();
            expected.extend_from_slice(&body);
            expected.push(b']');
            assert_eq!(server.request(&request), wire(&expected));
            assert!(server.request(b"invalid\r\n\r\n").is_empty());
            assert_eq!(
                server.request(b"GET /after HTTP/1.0\r\n\r\n"),
                wire(b"</after>")
            );
        }
    }
}
