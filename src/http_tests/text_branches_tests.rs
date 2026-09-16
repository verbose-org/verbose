use super::*;

fn source() -> String {
    include_str!("../../examples/http_bounded_text.verbose")
        .replace("port: 18964", "port: 18960")
        .replace("service bounded_text_http", "service bounded_http")
        .replace("out : text [..262]", "out : text [..4096]")
        .replace("out = concat(prefix, request.path)", "out = concat(\"\", request.body)")
        .replace("reads : [request.path]", "reads : [request.body]")
        .replace("reads : [req]", "reads : [req, req.method, req.path]")
        .replace("bound : 8", "bound : 64")
        .replace("resp = HttpResponse { status: 200, body: concat(\"\", response_text(req)) }", r#"let selected = if req.method == "POST" then HttpResponse { status: 201, body: response_text(req) } else if req.path == "/empty" then HttpResponse { body: "", status: 200 } else HttpResponse { body: concat("path:", req.path), status: 404 }
    let saved = selected
    let selected = HttpResponse { status: 200, body: "shadow" }
    let unused = response_text(req)
    resp = saved"#)
}

fn response(status: u16, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.0 {status} OK\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

#[test]
fn text_branches_http_selected_records_survive_aliases_and_later_calls() {
    for concurrency in [
        "",
        "  concurrency: forked\n",
        "  concurrency: pooled\n  workers: 2\n",
    ] {
        let server = Server::start(&format!("{}{concurrency}", source()));
        for i in 0..30 {
            let body = if i % 2 == 0 {
                vec![b'x'; 3900]
            } else {
                vec![0, 0xff, 0xc3, 0xa9, 0]
            };
            let mut request =
                format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            request.extend_from_slice(&body);
            assert_eq!(server.request(&request), response(201, &body));
            assert_eq!(
                server.request(b"GET /empty HTTP/1.0\r\n\r\n"),
                response(200, b"")
            );
            assert_eq!(
                server.request(b"GET /other HTTP/1.0\r\n\r\n"),
                response(404, b"path:/other")
            );
            assert!(server.request(b"invalid\r\n\r\n").is_empty());
        }
    }
}

#[test]
fn text_branches_http_literal_status_refusals_survive_joins() {
    for (from, to) in [
        ("status: 201", "status: 99"),
        ("status: 404", "status: 1000"),
    ] {
        let p = parse(&source().replace(from, to));
        let file = format!("/tmp/verbose-text-branches-status-{}", std::process::id());
        fs::write(&file, b"preserve").unwrap();
        let error = crate::native::compile_service(&p, "bounded_http", &file).unwrap_err();
        assert!(
            error.message.contains("outside HTTP valid range"),
            "{error}"
        );
        assert_eq!(fs::read(&file).unwrap(), b"preserve");
        fs::remove_file(&file).unwrap();
    }
}

#[test]
fn text_branches_state_formatter_copies_the_selected_record_field() {
    let source = include_str!("../../examples/bounded_text_state.verbose")
        .replace("port: 18965", "port: 18960")
        .replace("service remember_http", "service bounded_http")
        .replace("rule wrap_body", "concept BodyInput\n  @intention: \"Bound the selected body\"\n  @source: bounded_text_state.intent:2\n  fields:\n    body : text [..4096]\n\nrule wrap_body")
        .replace("item : HttpRequest", "item : BodyInput")
        .replace("let message = wrap_body(input)", "let selected = if input.method == \"POST\" then BodyInput { body: concat(\"\", input.body) } else BodyInput { body: input.path }\n    let message = wrap_body(selected)")
        .replace("out = if input.method == \"POST\" then saved else concat(\"<\", input.path, \">\")", "out = saved")
        .replace("reads : [input, input.method, input.path]", "reads : [input.body, input.method, input.path]");
    let server = Server::start(&source);
    assert_eq!(
        server.request(b"POST / HTTP/1.0\r\nContent-Length: 4\r\n\r\nx\0\xffz"),
        wire(b"prev:none")
    );
    assert_eq!(
        server.request(b"GET /next HTTP/1.0\r\n\r\n"),
        wire(b"prev:[x\0\xffz]")
    );
    assert_eq!(
        server.request(b"GET /last HTTP/1.0\r\n\r\n"),
        wire(b"prev:[/next]")
    );
}
