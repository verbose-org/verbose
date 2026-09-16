use super::admission_tests::{children, kill, wait_count};
use super::*;

fn source(n: u32, seconds: u32) -> String {
    format!("{SOURCE}  concurrency: pooled\n  workers: {n}\n  shutdown_timeout: {seconds}\n")
        .replace("request_timeout: 2", "request_timeout: 20")
        .replace("response_timeout: 2", "response_timeout: 20")
}
fn until(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    while !f() {
        assert!(Instant::now() < end, "shutdown condition timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn syscall(pid: u32) -> Vec<String> {
    fs::read_to_string(format!("/proc/{pid}/syscall"))
        .unwrap()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}
fn listening(port: u16) -> bool {
    fs::read_to_string("/proc/net/tcp")
        .unwrap()
        .lines()
        .skip(1)
        .any(|line| {
            let f: Vec<_> = line.split_whitespace().collect();
            f[1].ends_with(&format!(":{port:04X}")) && f[3] == "0A"
        })
}
fn finished(server: &mut Server, pids: &[u32], expected: i32) {
    let mut status = None;
    until(|| {
        status = server.child.try_wait().unwrap();
        status.is_some()
    });
    assert_eq!(status.unwrap().code(), Some(expected));
    for pid in pids {
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "worker not reaped"
        );
    }
    assert!(!listening(server.port));
}
fn partial(server: &Server, pids: &[u32]) -> Vec<TcpStream> {
    for pid in pids {
        until(|| syscall(*pid)[0] == "43");
    }
    let clients = pids
        .iter()
        .map(|_| {
            let mut s = server.connect();
            s.write_all(b"POST / HTTP/1.0\r\nContent-Length: 5\r\n\r\na")
                .unwrap();
            s
        })
        .collect();
    until(|| pids.iter().all(|p| syscall(*p)[0] == "7"));
    clients
}

#[test]
fn shutdown_idle_group_and_inherited_ignored_signals() {
    for workers in [1, 4] {
        let mut server = Server::start_with_ignored_signals(&source(workers, 3), &[15, 17]);
        let pids = wait_count(&server, workers as usize);
        for p in &pids {
            until(|| syscall(*p)[0] == "43");
        }
        let stacks: Vec<_> = pids.iter().map(|p| syscall(*p)[7].clone()).collect();
        let fds: Vec<_> = pids
            .iter()
            .map(|p| fs::read_dir(format!("/proc/{p}/fd")).unwrap().count())
            .collect();
        for i in 0..50 {
            let body = vec![i as u8; if i % 2 == 0 { 3900 } else { 0 }];
            let mut req =
                format!("POST / HTTP/1.0\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
            req.extend_from_slice(&body);
            assert_eq!(server.request(&req), wire(&body));
        }
        for (i, p) in pids.iter().enumerate() {
            until(|| syscall(*p)[0] == "43");
            assert_eq!(syscall(*p)[7], stacks[i]);
            assert_eq!(
                fs::read_dir(format!("/proc/{p}/fd")).unwrap().count(),
                fds[i]
            );
        }
        // Direct TERM to one worker remains blocked; the supervisor owns the policy.
        assert_eq!(unsafe { kill(pids[0] as i32, 15) }, 0);
        assert_eq!(server.request(b"GET / HTTP/1.0\r\n\r\n"), wire(b""));
        assert_eq!(children(&server), pids);
        assert_eq!(unsafe { kill(-(server.child.id() as i32), 15) }, 0);
        finished(&mut server, &pids, 0);
    }
}

#[test]
fn shutdown_finishes_accepted_binary_requests_and_drops_queue() {
    let mut server = Server::start(&source(2, 5));
    let pids = wait_count(&server, 2);
    let mut active = partial(&server, &pids);
    let mut queued = server.connect();
    queued.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
    until(|| !listening(server.port));
    assert!(server.child.try_wait().unwrap().is_none());
    for s in &mut active {
        s.write_all(b"b\0\xffz").unwrap();
        let mut output = vec![];
        s.read_to_end(&mut output).unwrap();
        assert_eq!(output, wire(b"ab\0\xffz"));
    }
    let mut reply = vec![];
    let _ = queued.read_to_end(&mut reply);
    assert!(
        reply.is_empty(),
        "queued connection ran a handler after the cutoff"
    );
    finished(&mut server, &pids, 0);
}

#[test]
fn shutdown_repeated_term_keeps_deadline_and_reaps_stopped_worker() {
    let mut server = Server::start(&source(2, 1));
    let pids = wait_count(&server, 2);
    let _active = partial(&server, &pids);
    assert_eq!(unsafe { kill(pids[0] as i32, 19) }, 0);
    let start = Instant::now();
    assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
    until(|| !listening(server.port));
    for _ in 0..20 {
        if server.child.try_wait().unwrap().is_some() {
            break;
        }
        unsafe {
            kill(server.child.id() as i32, 15);
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    finished(&mut server, &pids, 1);
    assert!(start.elapsed() >= Duration::from_millis(800));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "repeated TERM extended the deadline"
    );
}

#[test]
fn shutdown_client_timeout_is_clean_but_worker_death_is_failure() {
    for killed in [false, true] {
        let src = source(1, 4).replace("request_timeout: 20", "request_timeout: 1");
        let mut server = Server::start(&src);
        let pids = wait_count(&server, 1);
        let _active = partial(&server, &pids);
        assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
        until(|| !listening(server.port));
        if killed {
            assert_eq!(unsafe { kill(pids[0] as i32, 9) }, 0);
        }
        finished(&mut server, &pids, if killed { 1 } else { 0 });
    }
}

#[test]
fn shutdown_completes_a_blocked_large_response() {
    let payload = "x".repeat(4 * 1024 * 1024);
    let src = source(1, 5).replace(
        "concat(\"\", req.body)",
        &format!("concat(\"{payload}\", req.body)"),
    );
    let mut server = Server::start(&src);
    let pids = wait_count(&server, 1);
    for p in &pids {
        until(|| syscall(*p)[0] == "43");
    }
    let mut slow = server.connect();
    slow.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    until(|| syscall(pids[0])[0] == "7");
    assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
    until(|| !listening(server.port));
    let mut output = vec![];
    slow.read_to_end(&mut output).unwrap();
    assert_eq!(output, wire(payload.as_bytes()));
    finished(&mut server, &pids, 0);
}

#[test]
fn shutdown_deadline_terminates_a_blocked_response() {
    let payload = "x".repeat(4 * 1024 * 1024);
    let src = source(1, 1).replace(
        "concat(\"\", req.body)",
        &format!("concat(\"{payload}\", req.body)"),
    );
    let mut server = Server::start(&src);
    let pids = wait_count(&server, 1);
    until(|| syscall(pids[0])[0] == "43");
    let mut slow = server.connect();
    slow.write_all(b"GET / HTTP/1.0\r\n\r\n").unwrap();
    until(|| syscall(pids[0])[0] == "7");
    assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
    finished(&mut server, &pids, 1);
}

#[test]
fn shutdown_preserves_worker_failure_during_request_effects() {
    let asset = format!("/tmp/verbose-shutdown-asset-{}", std::process::id());
    fs::write(&asset, b"ok").unwrap();
    let resource = format!("resource asset\n  @intention: \"asset\"\n  @source: http_bounded.intent:1\n  path: \"{asset}\"\n  max: 8\n  on_read_error: abort\n\n");
    let src = source(1, 5)
        .replace("rule echo_body", &format!("{resource}rule echo_body"))
        .replace("concat(\"\", req.body)", "concat(read(asset), req.body)")
        .replace("[req.body]", "[asset, req.body]")
        .replace("bound : 4", "bound : 8");
    let mut server = Server::start(&src);
    let pids = wait_count(&server, 1);
    let mut active = partial(&server, &pids);
    assert_eq!(unsafe { kill(server.child.id() as i32, 15) }, 0);
    until(|| !listening(server.port));
    fs::remove_file(&asset).unwrap();
    active[0].write_all(b"bcde").unwrap();
    finished(&mut server, &pids, 1);
}

#[test]
fn shutdown_declarations_and_unsupported_backends_refuse_explicitly() {
    for seconds in [1, 3600] {
        assert!(crate::verifier::verify_program(
            &parse(&source(1, seconds)),
            Path::new("examples")
        )
        .is_empty());
    }
    for src in [
        source(1, 0),
        source(1, 3601),
        format!("{}  shutdown_timeout: 2\n", source(1, 1)),
    ] {
        assert!(Parser::new(Lexer::new(&src).tokenize().unwrap())
            .parse_program()
            .unwrap_err()
            .to_string()
            .contains("shutdown_timeout"));
    }
    for src in [
        source(1, 1).replace("pooled", "sequential"),
        source(1, 1).replace("pooled", "forked"),
        source(1, 1).replace("http_1_0", "raw_tcp"),
    ] {
        let p = parse(&src);
        assert!(crate::verifier::verify_program(&p, Path::new("examples"))
            .iter()
            .any(|e| e.message.contains("shutdown_timeout requires")));
        assert!(crate::native::compile_service(
            &p,
            "bounded_http",
            "/tmp/verbose-shutdown-refused"
        )
        .unwrap_err()
        .message
        .contains("shutdown_timeout requires"));
    }
    let mut p = parse(&source(1, 1));
    if let Item::Service(s) = p.items.last_mut().unwrap() {
        s.shutdown_timeout = Some(u32::MAX);
    }
    assert!(
        crate::native::compile_service(&p, "bounded_http", "/tmp/verbose-shutdown-refused")
            .unwrap_err()
            .message
            .contains("[1, 3600]")
    );
    assert!(
        crate::wasm::compile_wasm(&p, "bounded_http", "/tmp/verbose-shutdown-refused.wasm")
            .unwrap_err()
            .message
            .contains("shutdown_timeout")
    );
    assert!(!Path::new("/tmp/verbose-shutdown-refused").exists());
    assert!(!Path::new("/tmp/verbose-shutdown-refused.wasm").exists());
}
