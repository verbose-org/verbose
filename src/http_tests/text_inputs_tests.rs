use super::*;

const BODY_CONCEPT: &str = "concept BodyInput\n  @intention: \"Bound the reusable formatter input\"\n  @source: bounded_text_state.intent:2\n  fields:\n    body : text [..4096]\n\n";

#[test]
fn text_inputs_http_state_formatter_projects_to_a_different_concept() {
    let source = include_str!("../../examples/bounded_text_state.verbose")
        .replace("port: 18965", "port: 18960")
        .replace("service remember_http", "service bounded_http")
        .replace("rule wrap_body", &format!("{BODY_CONCEPT}rule wrap_body"))
        .replace("item : HttpRequest", "item : BodyInput")
        .replace(
            "wrap_body(input)",
            "wrap_body(BodyInput { body: input.body })",
        )
        .replace(
            "reads : [input, input.method, input.path]",
            "reads : [input.body, input.method, input.path]",
        );
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
        wire(b"prev:</next>")
    );
}

#[test]
fn text_inputs_http_handlers_keep_projected_binary_bodies_alive() {
    let source = format!(
        r#"@verbose 0.1.0
{BODY_CONCEPT}
rule project
  @intention: "Construct a reusable input from an HTTP request"
  @source: bounded_text_state.intent:2
  input:
    req : HttpRequest
  output:
    out : BodyInput
  logic:
    out = BodyInput {{ body: concat("", req.body) }}
  proofs:
    purity:
      reads : [req.body]
      calls : []
    termination:
      bound : 16
rule wrap_body
  @intention: "Format counted bytes through a different input concept"
  @source: bounded_text_state.intent:2
  input:
    item : BodyInput
  output:
    out : text [..4098]
  logic:
    let unused = concat("scratch:", item.body)
    out = concat("[", item.body, "]")
  proofs:
    purity:
      reads : [item.body]
      calls : []
    termination:
      bound : 32
rule respond
  @intention: "Retain a projected input across aliasing and shadowing"
  @source: bounded_text_state.intent:1
  input:
    request : HttpRequest
  output:
    out : HttpResponse
  logic:
    let input = project(request)
    let saved = input
    let input = BodyInput {{ body: "shadow" }}
    out = HttpResponse {{ status: 200, body: wrap_body(saved) }}
  proofs:
    purity:
      reads : [request]
      calls : [project, wrap_body]
    termination:
      bound : 32
service bounded_http
  @intention: "Serve a bounded formatter using explicit input construction"
  @source: bounded_text_state.intent:4
  listen:
    protocol: http_1_0
    port: 18960
    max_request: 4096
  handler: respond
  request_timeout: 2
  response_timeout: 2
"#
    );
    for concurrency in [
        "",
        "  concurrency: forked\n",
        "  concurrency: pooled\n  workers: 2\n",
    ] {
        let server = Server::start(&format!("{source}{concurrency}"));
        for i in 0..30 {
            let body = match i % 3 {
                0 => vec![b'x'; 3900],
                1 => vec![0, 0xc3, 0xa9, 0xff, 0],
                _ => vec![],
            };
            let mut request =
                format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            request.extend_from_slice(&body);
            assert_eq!(
                server.request(&request),
                wire(&[b"[".as_slice(), body.as_slice(), b"]"].concat())
            );
            assert!(server.request(b"invalid\r\n\r\n").is_empty());
        }
    }
}
