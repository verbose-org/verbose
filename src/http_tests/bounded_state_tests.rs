use super::*;
use crate::interpreter::{self, Value};
use std::collections::HashMap;

const SOURCE: &str = include_str!("../../examples/bounded_text_state.verbose");
fn source() -> String {
    SOURCE
        .replace("port: 18965", "port: 18960")
        .replace("service remember_http", "service bounded_http")
}
fn expected(p: &Program, method: &str, path: &str, body: &str) -> Vec<u8> {
    let rules: Vec<_> = p
        .items
        .iter()
        .filter_map(|i| if let Item::Rule(r) = i { Some(r) } else { None })
        .collect();
    let input = crate::verifier::builtin_http_request(4096);
    let output = crate::verifier::builtin_http_response();
    let value = interpreter::eval_rule(
        rules.iter().find(|r| r.name == "remember_text").unwrap(),
        &rules,
        &[&input, &output],
        &[],
        &HashMap::from([
            ("method".into(), Value::Text(method.into())),
            ("path".into(), Value::Text(path.into())),
            ("body".into(), Value::Text(body.into())),
        ]),
    )
    .unwrap();
    let Value::Text(text) = value else {
        panic!("expected text");
    };
    text.into_bytes()
}

#[test]
fn bounded_state_copies_results_before_request_storage_is_reclaimed() {
    let src = source();
    let p = parse(&src);
    let server = Server::start(&src);
    let pid = server.child.id();
    let stack = super::text_bounds_tests::storage_accept_stack(pid);
    let descriptors = fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count();
    let mut previous = b"none".to_vec();
    for i in 0..150 {
        let body = match i % 5 {
            0 => vec![b'x'; 3900],
            1 => vec![],
            2 => "a\0é\0z".as_bytes().to_vec(),
            3 => vec![0xff, 0, b'y'],
            _ => b"short".to_vec(),
        };
        let mut request =
            format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        request.extend_from_slice(&body);
        let mut response = b"prev:".to_vec();
        response.extend_from_slice(&previous);
        assert_eq!(server.request(&request), wire(&response));
        previous = match std::str::from_utf8(&body) {
            Ok(text) => expected(&p, "POST", "/", text),
            Err(_) => [b"[".as_slice(), body.as_slice(), b"]"].concat(),
        };
        if i % 25 == 0 {
            assert!(server.request(b"invalid\r\n\r\n").is_empty());
            let response = [b"prev:".as_slice(), previous.as_slice()].concat();
            assert_eq!(
                server.request(b"GET /after HTTP/1.0\r\n\r\n"),
                wire(&response)
            );
            previous = expected(&p, "GET", "/after", "");
        }
    }
    assert_eq!(super::text_bounds_tests::storage_accept_stack(pid), stack);
    assert_eq!(
        fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count(),
        descriptors
    );
}

#[test]
fn bounded_state_calls_use_parser_inputs_and_preserve_set_order() {
    let src = source()
        .replace("req : HttpRequest", "original : HttpRequest")
        .replace("remember_text(req)", "remember_text(original)")
        .replace("resp = HttpResponse", "let body = \"shadow-body\"\n    let path = \"shadow-path\"\n    let method = \"shadow-method\"\n    let req = \"shadow-req\"\n    let original = \"shadow-input\"\n    resp = HttpResponse")
        .replace("concat(\"prev:\", state.last)", "concat(state.last, \"|\", state.copy, \"|\", state.second)")
        .replace("reads : [state.last]", "reads : [state.last, state.copy, state.second]")
        .replace("bound : 8", "bound : 64")
        .replace("last : text [..4098] = \"none\"", "last : text [..4098] = \"none\"\n    copy : text [..4098] = \"copy\"\n    second : text [..4098] = \"second\"")
        .replace("set last = remember_text(original)", "set last = remember_text(original)\n    set copy = state.last\n    set second = wrap_body(original)");
    for src in [
        src.clone(),
        src.replace("  request_timeout: 2\n  response_timeout: 2\n", ""),
    ] {
        let server = Server::start(&src);
        assert_eq!(
            server.request(b"POST /a HTTP/1.0\r\nContent-Length: 3\r\n\r\nx\0z"),
            wire(b"none|copy|second")
        );
        assert_eq!(
            server.request(b"GET /b HTTP/1.0\r\n\r\n"),
            wire(b"[x\0z]|[x\0z]|[x\0z]")
        );
        assert_eq!(
            server.request(b"GET /c HTTP/1.0\r\n\r\n"),
            wire(b"</b>|</b>|[]")
        );
    }
}

#[test]
fn bounded_state_accepts_empty_and_exact_capacity_results() {
    for size in [0, 1, 65536] {
        let value = "x".repeat(size);
        let src = source()
            .replace("set last = remember_text(req)", "set last = wrap_body(req)")
            .replace("reads : [item.body]", "reads : []")
            .replace(
                "out : text [..4098]",
                &format!("out : text [..{}]", size.max(4098)),
            )
            .replace(
                "out = concat(\"[\", item.body, \"]\")",
                &format!("out = \"{value}\""),
            )
            .replace("last : text [..4098]", "last : text [..65536]");
        // Only the entry used by after has the exact public result capacity;
        // the unused formatter still has to satisfy its own annotation.
        let src = src.replacen(
            &format!("out : text [..{}]", size.max(4098)),
            &format!("out : text [..{size}]"),
            1,
        );
        let server = Server::start(&src);
        assert_eq!(
            server.request(b"GET / HTTP/1.0\r\n\r\n"),
            wire(b"prev:none")
        );
        assert_eq!(
            server.request(b"GET / HTTP/1.0\r\n\r\n"),
            wire(format!("prev:{value}").as_bytes())
        );
    }
}

#[test]
fn bounded_state_rejects_unsupported_escapes_before_artifact_emission() {
    let cases = [
        (
            source().replace(
                "out : text [..4098]\n  logic:\n    let message",
                "out : text\n  logic:\n    let message",
            ),
            "explicit text [..N]",
        ),
        (
            source().replace("last : text [..4098] = \"none\"", "last : number = 0"),
            "text state field",
        ),
        (
            source().replace(
                "set last = remember_text(req)",
                "set missing = remember_text(req)",
            ),
            "declared state field",
        ),
        (
            source().replace("protocol: http_1_0", "protocol: raw_tcp")
                .replace("rule recall_text", "concept HttpRequest\n  @intention: \"Explicit request concept outside HTTP\"\n  @source: bounded_text_state.intent:1\n  fields:\n    method : text [..16]\n    path : text [..256]\n    body : text [..4096]\n\nrule recall_text"),
            "sequential HTTP",
        ),
        (
            source().replace("last : text [..4098]", "last : text [..4097]"),
            "exceeds state field",
        ),
        (
            source().replace(
                "handler: recall_text",
                "handler: recall_text\n  concurrency: forked",
            ),
            "sequential HTTP",
        ),
        (
            source().replace(
                "handler: recall_text",
                "handler: recall_text\n  concurrency: pooled\n  workers: 2",
            ),
            "sequential HTTP",
        ),
        (
            source().replace(
                "set last = remember_text(req)",
                "set last = concat(remember_text(req))",
            ),
            "complete call",
        ),
        (
            source().replace(
                "set last = remember_text(req)",
                "set last = remember_text(state)",
            ),
            "original HTTP input",
        ),
        (
            source().replace(
                "set last = remember_text(req)",
                "set last = remember_text()",
            ),
            "original HTTP input",
        ),
        (
            source().replace(
                "let message = wrap_body(input)",
                "let message = remember_text(input)",
            ),
            "recursion",
        ),
        (
            source().replace(
                "let message = wrap_body(input)",
                "let now = now_unix()\n    let message = wrap_body(input)",
            ),
            "effect",
        ),
    ];
    let bin = format!("/tmp/verbose-bounded-state-refused-{}", std::process::id());
    for (src, message) in cases {
        let p = parse(&src);
        assert!(
            crate::verifier::verify_program(&p, Path::new("examples"))
                .iter()
                .any(|e| e.message.contains(message)),
            "{message}"
        );
        fs::write(&bin, b"existing artifact").unwrap();
        let error = crate::native::compile_service(&p, "bounded_http", &bin).unwrap_err();
        assert!(error.message.contains(message), "{error}");
        assert_eq!(fs::read(&bin).unwrap(), b"existing artifact");
        fs::remove_file(&bin).unwrap();
        assert!(crate::native::compile_service(&p, "bounded_http", &bin).is_err());
        assert!(!Path::new(&bin).exists());
    }
    // Without transport options the rejection must still name the text
    // contract, even though only an after call uses it.
    let p = parse(&source().replace("  request_timeout: 2\n  response_timeout: 2\n", ""));
    let error = crate::wasm::compile_wasm(&p, "bounded_http", &bin).unwrap_err();
    assert!(error.message.contains("bounded text"), "{error}");
    assert!(!Path::new(&bin).exists());
}
