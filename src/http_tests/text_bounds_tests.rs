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
