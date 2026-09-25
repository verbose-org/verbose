use super::admission_tests::{children, kill, wait_count};
use super::*;

fn source(n: u32) -> String {
    format!("{SOURCE}  concurrency: pooled\n  workers: {n}\n")
}
#[track_caller]
fn until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "pool did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn fds(pid: u32) -> usize {
    fs::read_dir(format!("/proc/{pid}/fd")).unwrap().count()
}
fn idle_stack(pid: u32) -> String {
    let mut stack = String::new();
    until(|| {
        let s = fs::read_to_string(format!("/proc/{pid}/syscall")).unwrap();
        let fields: Vec<_> = s.split_whitespace().collect();
        if fields.first() == Some(&"43") {
            stack = fields[7].to_owned();
            true
        } else {
            false
        }
    });
    stack
}
fn body_request(body: &[u8]) -> Vec<u8> {
    let mut req = format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
    req.extend_from_slice(body);
    req
}

#[test]
fn pooled_workers_reuse_pids_stack_and_descriptors() {
    for n in [1, 3] {
        let server = Server::start(&source(n));
        let pids = wait_count(&server, n as usize);
        let stacks: Vec<_> = pids.iter().map(|p| idle_stack(*p)).collect();
        let descriptors: Vec<_> = pids.iter().map(|p| fds(*p)).collect();
        let parent_fds = fds(server.child.id());
        for i in 0..100 {
            let body = match i % 4 {
                0 => vec![b'x'; 3900],
                1 => vec![],
                2 => vec![0, 0xc3, 0xa9, 0xff],
                _ => vec![b'y'],
            };
            assert_eq!(server.request(&body_request(&body)), wire(&body));
        }
        assert_eq!(children(&server), pids);
        for (i, p) in pids.iter().enumerate() {
            assert_eq!(idle_stack(*p), stacks[i], "request stack was not reclaimed");
            assert_eq!(fds(*p), descriptors[i], "worker leaked a descriptor");
        }
        assert_eq!(fds(server.child.id()), parent_fds);
    }
}

#[test]
fn pooled_workers_overlap_and_queue_without_spawning() {
    let server = Server::start(&source(2));
    let pids = wait_count(&server, 2);
    let idle_fds: Vec<_> = pids
        .iter()
        .map(|p| {
            idle_stack(*p);
            fds(*p)
        })
        .collect();
    let mut held: Vec<_> = (0..2)
        .map(|_| {
            let mut s = server.connect();
            s.write_all(b"POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx")
                .unwrap();
            s
        })
        .collect();
    // The test runner may pass through additional inheritable descriptors.
    // Observe one client above EACH worker's idle baseline, not an absolute
    // count that could mistake an idle worker for a busy one after a timeout.
    until(|| {
        pids.iter()
            .zip(&idle_fds)
            .all(|(p, idle)| fds(*p) == idle + 1)
    });
    let mut queued = server.connect();
    queued.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    queued
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    assert!(
        matches!(queued.read(&mut [0]), Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut))
    );
    assert_eq!(children(&server), pids);
    held[0].write_all(b"y").unwrap();
    let mut out = vec![];
    held[0].read_to_end(&mut out).unwrap();
    assert_eq!(out, wire(b"xy"));
    queued
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    out.clear();
    queued.read_to_end(&mut out).unwrap();
    assert_eq!(out, wire(b""));
    assert_eq!(children(&server), pids);
}

#[test]
fn pooled_workers_client_failures_reuse_the_same_worker() {
    let server = Server::start(&source(1).replace("request_timeout: 2", "request_timeout: 1"));
    let pid = wait_count(&server, 1)[0];
    for invalid in [
        b"invalid\r\n\r\n".as_slice(),
        b"POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx",
    ] {
        assert!(server.request(invalid).is_empty());
        assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
        assert_eq!(children(&server), vec![pid]);
    }
    let before = idle_stack(pid);
    let mut slow = server.connect();
    slow.write_all(b"POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx")
        .unwrap();
    let began = Instant::now();
    let mut out = vec![];
    slow.read_to_end(&mut out).unwrap();
    assert!(out.is_empty());
    assert!(
        began.elapsed() >= Duration::from_millis(750),
        "tested EOF instead of deadline"
    );
    assert!(began.elapsed() < Duration::from_secs(3));
    assert_eq!(children(&server), vec![pid]);
    let mut partial = server.connect();
    partial.write_all(b"GET / HTTP/1.0\r\n").unwrap();
    drop(partial);
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    assert_eq!(idle_stack(pid), before);
}

#[test]
fn pooled_workers_response_timeout_reclaims_large_stack_buffer() {
    let payload = "x".repeat(4 * 1024 * 1024);
    let source = source(1)
        .replace("resp = HttpResponse { status: 200, body: concat(\"\", req.body) }", &format!("resp = if req.path == \"/large\" then HttpResponse {{ status: 200, body: concat(\"{payload}\", req.body) }} else HttpResponse {{ status: 200, body: concat(\"\", req.body) }}"))
        .replace("[req.body]", "[req.path, req.body]")
        .replace("bound : 4", "bound : 16")
        .replace("response_timeout: 2", "response_timeout: 1");
    let server = Server::start(&source);
    let pid = wait_count(&server, 1)[0];
    let stack = idle_stack(pid);
    let mut slow = server.connect();
    slow.write_all(b"GET /large HTTP/1.0\r\n\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(1300));
    let mut prefix = vec![];
    slow.read_to_end(&mut prefix).unwrap();
    assert!(
        !prefix.is_empty() && prefix.len() < payload.len(),
        "backpressure did not truncate response"
    );
    assert_eq!(children(&server), vec![pid]);
    assert_eq!(idle_stack(pid), stack);
    assert_eq!(server.request(&body_request(b"fresh")), wire(b"fresh"));
    assert_eq!(idle_stack(pid), stack);
}

#[test]
fn pooled_workers_resource_snapshots_and_request_effect_lifetimes() {
    let root = format!("/tmp/verbose-pool-resources-{}", std::process::id());
    let asset = format!("{root}.asset");
    let log = format!("{root}.log");
    for cached in [false, true] {
        fs::write(&asset, b"first").unwrap();
        let resource = format!("resource asset\n  @intention: \"asset\"\n  @source: http_bounded.intent:1\n  path: \"{asset}\"\n  max: 64\n  on_read_error: abort\n{}\n", if cached { "  cache: true\n" } else { "" });
        let src = source(1)
            .replace("rule echo_body", &format!("{resource}rule echo_body"))
            .replace(
                "body: concat(\"\", req.body)",
                "body: concat(read(asset), state.suffix, req.body)",
            )
            .replace("[req.body]", "[asset, state.suffix, req.body]")
            .replace("bound : 4", "bound : 12");
        let src = format!("{src}  state:\n    suffix : text [..1] = \"!\"\n  log:\n    append_file \"{log}\" concat(req.path, \"\\n\")\n    on_error: abort\n");
        let mut server = Server::start(&src);
        let pid = wait_count(&server, 1)[0];
        assert_eq!(server.request(&body_request(b"a")), wire(b"first!a"));
        fs::write(&asset, b"second").unwrap();
        assert!(server.request(b"invalid\r\n\r\n").is_empty());
        assert_eq!(fs::read(&log).unwrap(), b"/\n");
        assert_eq!(
            server.request(&body_request(b"b")),
            wire(if cached { b"first!b" } else { b"second!b" })
        );
        assert_eq!(children(&server), vec![pid]);
        fs::remove_file(&asset).unwrap();
        if !cached {
            assert!(server.request(b"GET / HTTP/1.0\r\n\r\n").is_empty());
            let mut status = None;
            until(|| {
                status = server.child.try_wait().unwrap();
                status.is_some()
            });
            assert_eq!(status.unwrap().code(), Some(1));
        } else {
            assert_eq!(server.request(&body_request(b"c")), wire(b"first!c"));
        }
        fs::remove_file(&log).unwrap();
    }
}

#[test]
fn pooled_workers_worker_death_terminates_and_reaps_pool() {
    let mut server = Server::start_with_sigchld(&source(3), true);
    let pids = wait_count(&server, 3);
    for p in &pids {
        idle_stack(*p);
    }
    let stopped = || {
        fs::read_to_string(format!("/proc/{}/stat", pids[0])).unwrap()
            .split_whitespace().nth(2) == Some("T")
    };
    assert_eq!(unsafe { kill(pids[0] as i32, 19) }, 0); // stopped != terminated
    until(stopped);
    assert!(server.child.try_wait().unwrap().is_none());
    assert_eq!(children(&server), pids);
    // Signal delivery is asynchronous. A connection racing SIGSTOP can be
    // accepted by this worker and then wait for its resumption. Check service
    // health after SIGCONT instead of assuming another acceptor handles it.
    assert_eq!(unsafe { kill(pids[0] as i32, 18) }, 0);
    assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
    assert_eq!(children(&server), pids);
    // Keep the cleanup obligation: a peer death must kill/reap even a stopped
    // worker, not just the workers currently running or blocked in accept.
    assert_eq!(unsafe { kill(pids[0] as i32, 19) }, 0);
    until(stopped);
    assert_eq!(unsafe { kill(pids[1] as i32, 9) }, 0);
    let mut status = None;
    until(|| {
        status = server.child.try_wait().unwrap();
        status.is_some()
    });
    assert_eq!(status.unwrap().code(), Some(1));
    for p in pids {
        assert!(
            !Path::new(&format!("/proc/{p}")).exists(),
            "worker not reaped"
        );
    }
    assert!(TcpStream::connect(("127.0.0.1", server.port)).is_err());
}

#[test]
fn pooled_workers_supervisor_death_closes_worker_listeners() {
    let mut server = Server::start(&source(2));
    let pids = wait_count(&server, 2);
    for p in &pids {
        idle_stack(*p);
    }
    assert_eq!(unsafe { kill(server.child.id() as i32, 9) }, 0);
    server.child.wait().unwrap();
    until(|| {
        pids.iter().all(|p| {
            fs::read_to_string(format!("/proc/{p}/stat"))
                .map(|s| s.split_whitespace().nth(2) == Some("Z"))
                .unwrap_or(true)
        })
    });
    assert!(TcpStream::connect(("127.0.0.1", server.port)).is_err());
}

#[test]
fn pooled_workers_contexts_and_backend_refusals() {
    for n in [1, 64] {
        assert!(
            crate::verifier::verify_program(&parse(&source(n)), Path::new("examples")).is_empty()
        );
    }
    for n in [0, 65] {
        assert!(Parser::new(Lexer::new(&source(n)).tokenize().unwrap())
            .parse_program()
            .unwrap_err()
            .to_string()
            .contains("workers out of range"));
    }
    for source in [
        format!("{}  workers: 1\n", source(1)),
        format!("{}  concurrency: sequential\n", source(1)),
    ] {
        assert!(Parser::new(Lexer::new(&source).tokenize().unwrap())
            .parse_program()
            .unwrap_err()
            .to_string()
            .contains("duplicate"));
    }
    for (source, message) in [
        (source(1).replace("  workers: 1\n", ""), "requires workers"),
        (source(1).replace("pooled", "forked"), "concurrency: pooled"),
        (source(1).replace("http_1_0", "raw_tcp"), "http_1_0"),
        (
            source(1).replace("  request_timeout: 2\n", ""),
            "requires both",
        ),
        (
            source(1).replace("  response_timeout: 2\n", ""),
            "requires both",
        ),
        (
            format!("{}  max_connections: 1\n", source(1)),
            "cannot use max_connections",
        ),
        (
            format!(
                "{}  state:\n    count : number = 0\n  after:\n    set count = state.count + 1\n",
                source(1)
            ),
            "after mutations",
        ),
    ] {
        let program = parse(&source);
        assert!(
            crate::verifier::verify_program(&program, Path::new("examples"))
                .iter()
                .any(|e| e.message.contains(message)),
            "{message}"
        );
        assert!(crate::native::compile_service(
            &program,
            "bounded_http",
            "/tmp/verbose-pool-refused"
        )
        .unwrap_err()
        .message
        .contains(message));
    }
    let mut program = parse(&source(1));
    if let Item::Service(s) = program.items.last_mut().unwrap() {
        s.workers = Some(u32::MAX);
    }
    assert!(
        crate::native::compile_service(&program, "bounded_http", "/tmp/verbose-pool-refused")
            .unwrap_err()
            .message
            .contains("[1, 64]")
    );
    for src in [source(1), source(1).replace("  workers: 1\n", "")] {
        assert!(crate::wasm::compile_wasm(
            &parse(&src),
            "bounded_http",
            "/tmp/verbose-pool-refused.wasm"
        )
        .unwrap_err()
        .message
        .contains("pooled"));
    }
    assert!(!Path::new("/tmp/verbose-pool-refused").exists());
    assert!(!Path::new("/tmp/verbose-pool-refused.wasm").exists());
}
