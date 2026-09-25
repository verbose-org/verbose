//! Socket-level acceptance tests, independent of the native framing machine.
mod admission_tests;
mod pool_tests;
mod shutdown_tests;
mod text_bounds_tests;
mod bounded_state_tests;
mod text_inputs_tests;
mod text_branches_tests;
mod bounded_log_tests;
mod stack_budget_tests;
use crate::{
    ast::*,
    http_framing::{reference, Frame},
    lexer::Lexer,
    parser::Parser,
};
use std::os::unix::process::CommandExt;
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const SOURCE: &str = include_str!("../examples/http_bounded.verbose");
fn parse(s: &str) -> Program {
    Parser::new(Lexer::new(s).tokenize().unwrap())
        .parse_program()
        .unwrap()
}
fn wire(body: &[u8]) -> Vec<u8> {
    let mut b = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    b.extend_from_slice(body);
    b
}
struct Server {
    child: Child,
    port: u16,
    bin: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        // Dedicated test process group includes forked request children, even
        // if a test panics while one is stopped or has a long deadline.
        unsafe {
            admission_tests::kill(-(self.child.id() as i32), 9);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.bin);
    }
}
impl Server {
    fn start(source: &str) -> Self {
        Self::start_with_sigchld(source, false)
    }
    fn start_with_sigchld(source: &str, ignore: bool) -> Self {
        Self::start_with_ignored_signals(source, if ignore { &[17] } else { &[] })
    }
    fn start_with_ignored_signals(source: &str, ignored: &[i32]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let p = parse(&source.replace("port: 18960", &format!("port: {port}")));
        let errors = crate::verifier::verify_program(&p, Path::new("examples"));
        assert!(errors.is_empty(), "{errors:?}");
        let bin = format!("/tmp/verbose-http-{}-{port}", std::process::id());
        crate::native::compile_service(&p, "bounded_http", &bin).unwrap();
        let first = fs::read(&bin).unwrap();
        crate::native::compile_service(&p, "bounded_http", &bin).unwrap();
        assert_eq!(
            first,
            fs::read(&bin).unwrap(),
            "non-deterministic transport emission"
        );
        let mut command = Command::new(&bin);
        command
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if !ignored.is_empty() {
            let ignored = ignored.to_vec();
            unsafe {
                command.pre_exec(move || {
                    for sig in &ignored {
                        if admission_tests::signal(*sig, 1) == usize::MAX {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().unwrap();
        let mut s = Self { child, port, bin };
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                s.child.try_wait().unwrap().is_none(),
                "server exited on startup"
            );
            assert!(Instant::now() < deadline, "server did not listen");
            std::thread::sleep(Duration::from_millis(10));
        }
        s
    }
    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
        s.set_write_timeout(Some(Duration::from_secs(4))).unwrap();
        s
    }
    fn request(&self, bytes: &[u8]) -> Vec<u8> {
        let mut s = self.connect();
        let _ = s.write_all(bytes);
        let _ = s.shutdown(Shutdown::Write);
        let mut out = vec![];
        if let Err(e) = s.read_to_end(&mut out) {
            assert_eq!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset,
                "unexpected read failure"
            );
        }
        out
    }
}

#[test]
fn bounded_http_fragments_binary_and_exact_capacity() {
    let server = Server::start(SOURCE);
    let request = b"POST /body HTTP/1.0\r\ncOnTeNt-LeNgTh: 5\r\n\r\na\0\xff\r\n";
    assert_eq!(reference(request, 4096), Frame::Complete(request.len()));
    for split in 1..request.len() {
        let mut client = server.connect();
        client.write_all(&request[..split]).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        let mut byte = [0];
        let e = client
            .read(&mut byte)
            .expect_err("handler ran before complete request");
        assert!(matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client.write_all(&request[split..]).unwrap();
        let mut output = vec![];
        client.read_to_end(&mut output).unwrap();
        assert_eq!(output, wire(b"a\0\xff\r\n"), "split {split}");
    }
    assert_eq!(
        server.request(b"GET / HTTP/1.1\r\nX-Empty:\r\n\r\n"),
        wire(b"")
    );
    let header = b"POST / HTTP/1.0\r\nContent-Length: 4054\r\n\r\n";
    let body = vec![b'x'; 4096 - header.len()];
    let request = [
        format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(),
        body.clone(),
    ]
    .concat();
    assert_eq!(request.len(), 4096);
    assert_eq!(server.request(&request), wire(&body));
    assert_eq!(
        server.request(b"GET / HTTP/1.0\r\n\r\nignored second request"),
        wire(b"")
    );
}

#[test]
fn bounded_http_invalid_requests_close_only_the_client() {
    let server = Server::start(SOURCE);
    let cases: Vec<Vec<u8>> = [
        "POST / HTTP/1.0\r\nContent-Length: -1\r\n\r\n",
        "POST / HTTP/1.0\r\nContent-Length: 1x\r\n\r\nx",
        "POST / HTTP/1.0\r\nContent-Length: \r\n\r\n",
        "POST / HTTP/1.0\r\nContent-Length: 1, 1\r\n\r\nx",
        "POST / HTTP/1.0\r\nContent-Length: 1\r\ncontent-length: 1\r\n\r\nx",
        "POST / HTTP/1.0\r\nContent-Length: 18446744073709551616\r\n\r\n",
        "POST / HTTP/1.0\r\nContent-Length: 4096\r\n\r\n",
        "POST / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        "POST / HTTP/1.0\r\nContent-Length: 0\r\nTRANSFER-ENCODING: identity\r\n\r\n",
        "POST / HTTP/1.0\r\nExpect: 100-continue\r\n\r\n",
        "GET / HTTP/1.0\r\nX : y\r\n\r\n",
        "GET / HTTP/1.0\r\nX: y\r\n folded\r\n\r\n",
        "GET / HTTP/1.0\r\nX: \0\r\n\r\n",
        "GET / HTTP/1.0\n\n",
        "GET / HTTP/1.0\r\n\nX: y\r\n\r\n",
        "GET / HTTP/2.0\r\n\r\n",
        "GET http://host/ HTTP/1.0\r\n\r\n",
        "GET /#fragment HTTP/1.0\r\n\r\n",
        "TOOLONGMETHOD / HTTP/1.0\r\n\r\n",
        "POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .chain([
        format!("GET /{} HTTP/1.0\r\n\r\n", "x".repeat(256)).into_bytes(),
        [b"GET / HTTP/1.0\r\nX: ".as_slice(), &vec![b'x'; 4096]].concat(),
    ])
    .collect();
    for request in cases {
        assert!(!matches!(reference(&request, 4096), Frame::Complete(_)));
        assert!(
            server.request(&request).is_empty(),
            "answered malformed input {request:?}"
        );
        assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    }
    // The constant-response optimization must not bypass framing either.
    let constant = SOURCE
        .replace("body: concat(\"\", req.body)", "body: \"ok\"")
        .replace("[req.body]", "[]");
    let server = Server::start(&constant);
    assert!(server.request(b"garbage").is_empty());
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b"ok"));
}

#[test]
fn bounded_http_receive_deadline_is_absolute() {
    let server = Server::start(&SOURCE.replace("request_timeout: 2", "request_timeout: 1"));
    let mut slow = server.connect();
    let began = Instant::now();
    for byte in b"GET" {
        let _ = slow.write_all(&[*byte]);
        std::thread::sleep(Duration::from_millis(400));
    }
    let mut out = vec![];
    let _ = slow.read_to_end(&mut out);
    assert!(out.is_empty());
    assert!(began.elapsed() < Duration::from_millis(1900));
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    // A header without its promised body has the same absolute deadline.
    let mut slow = server.connect();
    slow.write_all(b"POST / HTTP/1.0\r\nContent-Length: 10\r\n\r\nx")
        .unwrap();
    let began = Instant::now();
    let mut out = vec![];
    let _ = slow.read_to_end(&mut out);
    assert!(out.is_empty());
    assert!(began.elapsed() < Duration::from_millis(1800));
}

#[test]
fn bounded_http_declaration_and_backend_refusals() {
    for key in ["request_timeout", "response_timeout"] {
        for n in [0, 3601] {
            let s = SOURCE.replace(&format!("{key}: 2"), &format!("{key}: {n}"));
            assert!(Parser::new(Lexer::new(&s).tokenize().unwrap())
                .parse_program()
                .unwrap_err()
                .to_string()
                .contains("out of range"));
        }
        let s = format!("{SOURCE}  {key}: 2\n");
        assert!(Parser::new(Lexer::new(&s).tokenize().unwrap())
            .parse_program()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }
    for (source, diagnostic) in [
        (
            SOURCE.replace("  request_timeout: 2\n", ""),
            "requires both",
        ),
        (
            SOURCE.replace("  response_timeout: 2\n", ""),
            "requires both",
        ),
        (SOURCE.replace("http_1_0", "raw_tcp"), "require protocol"),
        (
            SOURCE.replace("max_request: 4096", "max_request: 1048577"),
            "max_request",
        ),
        (format!("{SOURCE}  read_timeout: 1\n"), "cannot use raw_tcp"),
    ] {
        let p = parse(&source);
        assert!(crate::verifier::verify_program(&p, Path::new("examples"))
            .iter()
            .any(|e| e.message.contains(diagnostic)));
        let file = format!("/tmp/verbose-http-refusal-{}", std::process::id());
        assert!(crate::native::compile_service(&p, "bounded_http", &file)
            .unwrap_err()
            .message
            .contains(diagnostic));
        assert!(!Path::new(&file).exists());
    }
    for seconds in [1, 3600] {
        let source = SOURCE.replace("timeout: 2", &format!("timeout: {seconds}"));
        assert!(crate::verifier::verify_program(&parse(&source), Path::new("examples")).is_empty());
    }
    // Direct AST callers do not get to bypass the range check.
    let mut invalid = parse(SOURCE);
    if let Item::Service(service) = invalid.items.last_mut().unwrap() {
        service.response_timeout = Some(0);
    }
    assert!(crate::native::compile_service(
        &invalid,
        "bounded_http",
        "/tmp/verbose-http-invalid-range"
    )
    .unwrap_err()
    .message
    .contains("[1, 3600]"));
    let too_large = SOURCE
        .replace(
            "body: concat(\"\", req.body)",
            &format!("body: \"{}\"", "x".repeat(4097)),
        )
        .replace("[req.body]", "[]");
    assert!(crate::native::compile_service(
        &parse(&too_large),
        "bounded_http",
        "/tmp/verbose-http-invalid-literal"
    )
    .unwrap_err()
    .message
    .contains("[..4096]"));
    let file = format!("/tmp/verbose-http-refusal-{}.wasm", std::process::id());
    assert!(
        crate::wasm::compile_wasm(&parse(SOURCE), "bounded_http", &file)
            .unwrap_err()
            .message
            .contains("bounded HTTP")
    );
    assert!(!Path::new(&file).exists());
}

#[test]
fn bounded_http_partial_sends_deadline_and_after_order() {
    // An existing dynamic concat can stream a response larger than socket send
    // buffers. Its source-level text-range limitations are unchanged by this
    // transport slice; use it here to force real short sendto calls/backpressure.
    let payload = "x".repeat(4 * 1024 * 1024);
    let source = SOURCE
        .replace(
            "body: concat(\"\", req.body)",
            &format!("body: concat(\"{payload}\", state.count)"),
        )
        .replace("[req.body]", "[state.count]")
        .replace("response_timeout: 2", "response_timeout: 1");
    let source = format!(
        "{source}  state:\n    count : number = 0\n  after:\n    set count = state.count + 1\n"
    );
    let server = Server::start(&source);
    let mut blocked = server.connect();
    blocked.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(1400));
    let mut partial = vec![];
    blocked.read_to_end(&mut partial).unwrap();
    assert!(partial.starts_with(b"HTTP/1.0 200 OK\r\n"));
    assert!(
        partial.len() < payload.len(),
        "test did not force a blocked partial send"
    );
    let mut body = payload.into_bytes();
    body.push(b'0');
    assert!(
        server.request(b"GET / HTTP/1.0\r\n\r\n") == wire(&body),
        "after ran on an incomplete send"
    );
    *body.last_mut().unwrap() = b'1';
    assert!(
        server.request(b"GET / HTTP/1.0\r\n\r\n") == wire(&body),
        "after failed to run after complete send"
    );
}

#[test]
fn bounded_http_peer_reset_and_forked_recovery() {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct Linger {
        on: i32,
        seconds: i32,
    }
    extern "C" {
        fn setsockopt(fd: i32, level: i32, option: i32, value: *const Linger, len: u32) -> i32;
    }
    for concurrency in ["", "  concurrency: forked\n"] {
        let server = Server::start(&format!("{SOURCE}{concurrency}"));
        for _ in 0..12 {
            let mut reset = server.connect();
            let linger = Linger { on: 1, seconds: 0 };
            // Test-client-only SO_LINGER, to exercise reset rather than FIN.
            assert_eq!(
                unsafe { setsockopt(reset.as_raw_fd(), 1, 13, &linger, 8) },
                0
            );
            reset.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
            drop(reset);
        }
        assert_eq!(
            server.request(b"POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nok"),
            wire(b"ok")
        );
    }
}

#[test]
fn bounded_http_seeded_reference_differential() {
    let server = Server::start(SOURCE);
    let seed_request = b"POST /target HTTP/1.0\r\nX-Header: value\r\nContent-Length: 4\r\n\r\nbody";
    let mut seed = 0x239acd71u32;
    for _ in 0..256 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let mut req = seed_request.to_vec();
        let pos = seed as usize % req.len();
        req[pos] = (seed >> 24) as u8;
        let expected = match reference(&req, 4096) {
            Frame::Complete(total) => {
                let start = req.windows(4).position(|x| x == b"\r\n\r\n").unwrap() + 4;
                wire(&req[start..total])
            }
            _ => vec![], // shutdown(Write) makes an incomplete message fail too
        };
        assert_eq!(server.request(&req), expected, "mutation at {pos}: {req:?}");
    }
    for req in [
        "EIGHTCHR / HTTP/1.0\r\nContent-Length: 0\r\n\r\n".to_string(),
        format!("GET /{} HTTP/1.0\r\n\r\n", "x".repeat(255)),
    ] {
        assert_eq!(reference(req.as_bytes(), 4096), Frame::Complete(req.len()));
        assert_eq!(server.request(req.as_bytes()), wire(b""));
    }
    let req = b"POST / HTTP/1.0\r\nContent-Length: \t0002 \t\r\n\r\nok";
    assert_eq!(reference(req, 4096), Frame::Complete(req.len()));
    assert_eq!(server.request(req), wire(b"ok"));
    for name in [
        "Content-Length-X",
        "Transfer-Encoding-X",
        "Expectant",
        "content",
        "c",
        "t",
        "e",
    ] {
        let req = format!("GET / HTTP/1.0\r\n{name}: ignored\r\n\r\n");
        assert_eq!(server.request(req.as_bytes()), wire(b""));
    }
}

#[test]
fn bounded_http_self_hosted_refuses_before_output() {
    let compiler = parse(include_str!("../examples/vexprparse.verbose"));
    // Both raw machine-code and ELF entry points must gate before their first
    // emitted byte. verify_errors has the same scoped scanner wired in.
    for entry in ["elf_program_src", "x86_program_src"] {
        let bin = format!("/tmp/verbose-http-self-{}-{entry}", std::process::id());
        crate::native::compile_native_stdin_raw(&compiler, entry, &bin).unwrap();
        let run = |source: &str| {
            let mut child = Command::new(&bin)
                .arg("0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(source.as_bytes())
                .unwrap();
            child.wait_with_output().unwrap()
        };
        let hello = include_str!("../examples/hello_http.verbose");
        for attrs in [
            "  request_timeout: 2\n",
            "  response_timeout: 2\n",
            "  max_connections: 2\n",
            "  workers: 2\n",
            "  concurrency: pooled\n",
            "  shutdown_timeout: 2\n",
            "  request_timeout: 2\n  response_timeout: 2\n",
        ] {
            for source in [
                format!("{hello}\n{attrs}"),
                hello.replace(
                    "  handler: hello_handler",
                    &format!("{attrs}  handler: hello_handler"),
                ),
            ] {
                let output = run(&source);
                assert_eq!(
                    output.status.code(),
                    Some(1),
                    "{entry} accepted unsupported HTTP contract"
                );
                assert!(output.stdout.is_empty(), "{entry} emitted partial artifact");
            }
        }
        // The new names are not globally reserved; fields and let identifiers
        // outside services, comments, and strings must remain accepted.
        let source = "@verbose 0.1.0\nconcept Input\n  @intention: \"request_timeout: response_timeout:\"\n  @source: http_bounded.intent:1\n  fields:\n    request_timeout : number\nrule main\n  @intention: \"x\"\n  @source: http_bounded.intent:1\n  input:\n    i : Input\n  output:\n    out : number\n  logic:\n    let response_timeout = i.request_timeout\n    out = response_timeout\n  proofs:\n    purity:\n      reads : [i.request_timeout]\n      calls : []\n    termination:\n      bound : 8\n-- request_timeout: 2\n";
        let output = run(source);
        assert!(
            output.status.success(),
            "{entry} over-reserved identifiers: {output:?}"
        );
        assert!(!output.stdout.is_empty());
        for name in ["max_connections", "workers", "pooled", "shutdown_timeout"] {
            let ordinary_name = source.replace("request_timeout", name);
            let output = run(&ordinary_name);
            assert!(
                output.status.success(),
                "{entry} reserved {name} outside services"
            );
            assert!(!output.stdout.is_empty());
        }
        fs::remove_file(bin).unwrap();
    }
}

#[test]
fn bounded_http_resources_and_logs_follow_complete_framing() {
    let root = format!("/tmp/verbose-http-effects-{}", std::process::id());
    let asset = format!("{root}.asset");
    let log = format!("{root}.log");
    fs::write(&asset, b"first").unwrap();
    let _ = fs::remove_file(&log);
    let resource = format!("resource asset\n  @intention: \"asset\"\n  @source: http_bounded.intent:1\n  path: \"{asset}\"\n  max: 64\n  on_read_error: abort\n\n");
    let source = SOURCE
        .replace("rule echo_body", &format!("{resource}rule echo_body"))
        .replace(
            "body: concat(\"\", req.body)",
            "body: concat(read(asset), req.body)",
        )
        .replace("[req.body]", "[asset, req.body]")
        .replace("bound : 4", "bound : 8");
    let source = format!("{source}  log:\n    append_file \"{log}\" concat(req.timestamp, \":\", req.path, \"\\n\")\n    on_error: abort\n");
    let server = Server::start(&source);
    let mut client = server.connect();
    client
        .write_all(b"POST / HTTP/1.0\r\nContent-Length: 1\r\n\r\n")
        .unwrap();
    std::thread::sleep(Duration::from_millis(80));
    assert!(!Path::new(&log).exists());
    fs::write(&asset, b"second").unwrap();
    client.write_all(b"!").unwrap();
    let mut out = vec![];
    client.read_to_end(&mut out).unwrap();
    assert_eq!(
        out,
        wire(b"second!"),
        "resource was read before complete framing"
    );
    let logged = fs::read(&log).unwrap();
    assert!(logged.ends_with(b":/\n"));
    assert!(!logged.starts_with(b"0:"), "timestamp scratch corrupted");
    fs::remove_file(&asset).unwrap();
    assert!(server.request(b"invalid\r\n\r\n").is_empty());
    fs::write(&asset, b"third").unwrap();
    assert_eq!(
        server.request(b"GET /next HTTP/1.0\r\n\r\n"),
        wire(b"third"),
        "malformed input attempted missing resource and killed listener"
    );
    assert_eq!(
        fs::read(&log)
            .unwrap()
            .iter()
            .filter(|b| **b == b'\n')
            .count(),
        2
    );
    fs::remove_file(asset).unwrap();
    fs::remove_file(log).unwrap();
}
