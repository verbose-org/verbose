use super::*;
use super::admission_tests::{children, wait_count};
use super::text_bounds_tests::storage_accept_stack;

const SOURCE: &str = include_str!("../../examples/http_stack.verbose");

#[test]
fn http_stack_exact_budget_retains_responses_and_reclaims_repeated_requests() {
    for mode in ["", "  concurrency: forked\n  max_connections: 4\n",
        "  concurrency: pooled\n  workers: 2\n"] {
        let source = SOURCE.replace("  concurrency: pooled\n  workers: 2\n", mode);
        let bound = crate::native::service_stack_report(&parse(&source), "bounded_http").unwrap().stack_bound_bytes();
        let server = Server::start(&source.replace("native_stack: 8192", &format!("native_stack: {bound}")));
        let pids = if mode.contains("pooled") { wait_count(&server, 2) }
            else if mode.is_empty() { vec![server.child.id()] } else { vec![] };
        let stacks: Vec<_> = pids.iter().map(|p| storage_accept_stack(*p)).collect();
        for i in 0..40 {
            let path = if i % 2 == 0 { "/%C3%A9".to_string() } else { format!("/{}", "x".repeat(255)) };
            let body = match i % 3 { 0 => vec![], 1 => vec![0, 0xff, 0xc3, 0xa9], _ => vec![b'x'; 3700] };
            let request = [format!("POST {path} HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(), body.clone()].concat();
            assert_eq!(server.request(&request), wire(format!("{path}:{}", body.len() * 2).as_bytes()));
            if i % 10 == 0 {
                assert!(server.request(b"POST / HTTP/1.0\r\nContent-Length: 4096\r\n\r\n").is_empty());
                assert!(server.request(b"invalid\r\n\r\n").is_empty());
            }
        }
        // An incomplete request must expire without retaining a handler frame.
        let mut partial = server.connect();
        partial.write_all(b"POST / HTTP/1.0\r\nContent-Length: 1\r\n\r\n").unwrap();
        let mut out = vec![];
        partial.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
        assert_eq!(server.request(b"GET /fresh HTTP/1.0\r\n\r\n"), wire(b"/fresh:0"));
        for (pid, stack) in pids.iter().zip(stacks) {
            assert_eq!(storage_accept_stack(*pid), stack, "stack accumulated across requests");
        }
        if mode.contains("pooled") { assert_eq!(children(&server), pids); }
    }
}

#[test]
fn http_stack_branch_and_alias_results_keep_counted_binary_storage_alive() {
    let source = SOURCE
        .replace("[..277]", "[..4098]")
        .replace("    path : text [..256]", "    path : text [..4096]")
        .replace("concat(view.path, \":\", view.count * 2)", "if view.count == 0 then \"\" else concat(\"[\", view.path, \"]\")")
        .replace("    let saved = req.path", "    let saved = req.body")
        .replace("    resp = HttpResponse { status: 200, body: body }", "    let retained = body\n    let body = \"shadow\"\n    let unused = describe(view)\n    resp = HttpResponse { status: 200, body: retained }")
        .replace("reads: [req.path, req.body]", "reads: [req.body]")
        .replace("bound: 8", "bound: 32")
        .replace("bound: 16", "bound: 64")
        .replace("native_stack: 8192", "native_stack: 32768");
    let bound = crate::native::service_stack_report(&parse(&source), "bounded_http").unwrap().stack_bound_bytes();
    let server = Server::start(&source.replace("native_stack: 32768", &format!("native_stack: {bound}")));
    for body in [vec![], vec![0, 0xff, 0xc3, 0xa9], vec![b'x'; 4054]] {
        let request = [format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(), body.clone()].concat();
        let expected = if body.is_empty() { vec![] } else { [b"[".as_slice(), &body, b"]"].concat() };
        assert_eq!(server.request(&request), wire(&expected));
    }
}
