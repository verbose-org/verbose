use super::*;
extern "C" {
    pub(super) fn kill(pid: i32, signal: i32) -> i32;
    pub(super) fn signal(signal: i32, handler: usize) -> usize;
}
fn children(server: &Server) -> Vec<u32> {
    let pid = server.child.id();
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap()
        .split_whitespace()
        .map(|s| s.parse().unwrap())
        .collect()
}
fn wait_count(server: &Server, expected: usize) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let pids = children(server);
        if pids.len() == expected {
            return pids;
        }
        assert!(
            Instant::now() < deadline,
            "expected {expected} children, got {pids:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn capped_source(n: u32) -> String {
    format!("{SOURCE}  concurrency: forked\n  max_connections: {n}\n")
}
fn warm(server: &Server) {
    // The listen probe may still occupy a child; require a completed request
    // followed by reaping, so saturation tests start without readiness races.
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.request(b"GET / HTTP/1.0\r\n\r\n").is_empty() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    wait_count(server, 0);
}
fn start(n: u32) -> Server {
    let server = Server::start(&capped_source(n));
    warm(&server);
    server
}
fn incomplete(server: &Server) -> TcpStream {
    let mut s = server.connect();
    s.write_all(b"POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx")
        .unwrap();
    s
}
fn closed(mut s: TcpStream) {
    let mut b = [0];
    match s.read(&mut b) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("expected close, got {result:?}"),
    }
}

#[test]
fn bounded_admission_saturation_and_recovery() {
    for n in [1, 3] {
        let server = start(n);
        let mut held: Vec<_> = (0..n).map(|_| incomplete(&server)).collect();
        wait_count(&server, n as usize);
        let begin = Instant::now();
        assert!(server.request(b"GET /overload HTTP/1.0\r\n\r\n").is_empty());
        assert!(
            begin.elapsed() < Duration::from_secs(1),
            "overload was queued for a child slot"
        );
        assert_eq!(children(&server).len(), n as usize);
        let mut first = held.pop().unwrap();
        first.write_all(b"y").unwrap();
        let mut output = vec![];
        first.read_to_end(&mut output).unwrap();
        assert_eq!(output, wire(b"xy"));
        wait_count(&server, n as usize - 1);
        assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
        drop(held);
        wait_count(&server, 0);
    }
}

#[test]
fn bounded_admission_timeout_signal_and_idle_reaping() {
    let source = capped_source(1).replace("request_timeout: 2", "request_timeout: 1");
    let server = Server::start(&source);
    warm(&server);
    let client = incomplete(&server);
    wait_count(&server, 1);
    closed(client);
    wait_count(&server, 0); // no new request needed to reap timeout
    let client = incomplete(&server);
    let pid = wait_count(&server, 1)[0];
    assert_eq!(unsafe { kill(pid as i32, 9) }, 0);
    closed(client);
    wait_count(&server, 0);
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    wait_count(&server, 0);
    let client = incomplete(&server);
    let pid = wait_count(&server, 1)[0];
    assert_eq!(unsafe { kill(pid as i32, 19) }, 0); // SIGSTOP retains the slot
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(children(&server), vec![pid]);
    assert!(server.request(b"GET /full HTTP/1.0\r\n\r\n").is_empty());
    assert_eq!(unsafe { kill(pid as i32, 18) }, 0);
    closed(client);
    wait_count(&server, 0);
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
}

#[test]
fn bounded_admission_burst_and_descriptor_ownership() {
    let server = start(3);
    let fds = |pid: u32| {
        fs::read_dir(format!("/proc/{pid}/fd"))
            .unwrap()
            .filter_map(|e| fs::read_link(e.unwrap().path()).ok())
            .collect::<Vec<_>>()
    };
    let parent_before = fds(server.child.id());
    let listener = parent_before
        .iter()
        .find(|p| p.to_string_lossy().starts_with("socket:"))
        .unwrap()
        .clone();
    let mut clients = vec![];
    for _ in 0..20 {
        clients.push(server.connect());
        assert!(children(&server).len() <= 3);
    }
    let pids = wait_count(&server, 3);
    let mut live = 0;
    let mut survivors = vec![];
    for mut s in clients {
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let mut b = [0];
        match s.read(&mut b) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                live += 1;
                // Check while at least this child still owns a live client.
                assert!(children(&server).len() <= 3);
                survivors.push(s);
            }
            Ok(0) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }
    assert_eq!(live, 3);
    drop(survivors);
    wait_count(&server, 0);
    assert_eq!(
        fds(server.child.id()).len(),
        parent_before.len(),
        "parent leaked accepted fds"
    );
    let _ = pids;
    let held = incomplete(&server);
    let pid = wait_count(&server, 1)[0];
    assert!(
        !fds(pid).contains(&listener),
        "child retained listening socket"
    );
    drop(held);
    wait_count(&server, 0);
}

#[test]
fn bounded_admission_overload_has_no_request_effects() {
    let log = format!("/tmp/verbose-admission-{}.log", std::process::id());
    let source = format!(
        "{}  log:\n    append_file \"{log}\" \"handled\\n\"\n    on_error: abort\n",
        capped_source(1)
    );
    let server = Server::start(&source);
    warm(&server);
    fs::remove_file(&log).unwrap();
    let held = incomplete(&server);
    wait_count(&server, 1);
    for _ in 0..10 {
        assert!(server.request(b"GET / HTTP/1.0\r\n\r\n").is_empty());
    }
    assert!(!Path::new(&log).exists());
    drop(held);
    wait_count(&server, 0);
    assert!(server.request(b"invalid\r\n\r\n").is_empty());
    wait_count(&server, 0);
    assert!(!Path::new(&log).exists());
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    assert_eq!(fs::read(&log).unwrap(), b"handled\n");
    fs::remove_file(log).unwrap();
}

#[test]
fn bounded_admission_resets_inherited_sigchld_ignore() {
    let server = Server::start_with_sigchld(&capped_source(1), true);
    warm(&server);
    for _ in 0..6 {
        let held = incomplete(&server);
        wait_count(&server, 1);
        assert!(server.request(b"GET /full HTTP/1.0\r\n\r\n").is_empty());
        drop(held);
        wait_count(&server, 0);
    }
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
}

#[test]
fn bounded_admission_declarations_and_direct_backend_refusals() {
    for n in [0, 65536] {
        let source = capped_source(n);
        assert!(Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .unwrap_err()
            .to_string()
            .contains("max_connections out of range"));
    }
    let source = format!("{}  max_connections: 2\n", capped_source(1));
    assert!(Parser::new(Lexer::new(&source).tokenize().unwrap())
        .parse_program()
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
    for n in [1, 65535] {
        assert!(
            crate::verifier::verify_program(&parse(&capped_source(n)), Path::new("examples"))
                .is_empty()
        );
    }
    for (source, message) in [
        (
            capped_source(2).replace("  concurrency: forked\n", ""),
            "concurrency: forked",
        ),
        (capped_source(2).replace("http_1_0", "raw_tcp"), "http_1_0"),
        (
            capped_source(2).replace("  request_timeout: 2\n", ""),
            "requires both",
        ),
        (
            capped_source(2).replace("  response_timeout: 2\n", ""),
            "requires both",
        ),
        (
            format!(
                "{}  state:\n    count : number = 0\n  after:\n    set count = state.count + 1\n",
                capped_source(2)
            ),
            "after mutations",
        ),
    ] {
        let p = parse(&source);
        assert!(crate::verifier::verify_program(&p, Path::new("examples"))
            .iter()
            .any(|e| e.message.contains(message)));
        assert!(crate::native::compile_service(
            &p,
            "bounded_http",
            "/tmp/verbose-admission-refused"
        )
        .unwrap_err()
        .message
        .contains(message));
    }
    let mut p = parse(&capped_source(2));
    if let Item::Service(s) = p.items.last_mut().unwrap() {
        s.max_connections = Some(u32::MAX);
    }
    assert!(
        crate::native::compile_service(&p, "bounded_http", "/tmp/verbose-admission-refused")
            .unwrap_err()
            .message
            .contains("[1, 65535]")
    );
    let source = format!("{SOURCE}  max_connections: 2\n");
    let path = format!("/tmp/verbose-admission-{}.wasm", std::process::id());
    assert!(
        crate::wasm::compile_wasm(&parse(&source), "bounded_http", &path)
            .unwrap_err()
            .message
            .contains("max_connections")
    );
    assert!(!Path::new(&path).exists());
    assert!(!Path::new("/tmp/verbose-admission-refused").exists());
}

#[test]
fn bounded_admission_bind_failure_exits_parent() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let source = capped_source(1).replace("port: 18960", &format!("port: {port}"));
    let bin = format!("/tmp/verbose-admission-bind-{}", std::process::id());
    crate::native::compile_service(&parse(&source), "bounded_http", &bin).unwrap();
    let output = Command::new(&bin).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    fs::remove_file(bin).unwrap();
}
