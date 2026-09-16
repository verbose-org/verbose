//! Dependency-free load client for benchmark_http.py. One connection/request;
//! byte-exact binary echo oracle. Failures produce JSON AND a nonzero exit code.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn number(args: &[String], i: usize, low: usize, high: usize) -> usize {
    args.get(i)
        .and_then(|s| s.parse().ok())
        .filter(|n| (low..=high).contains(n))
        .unwrap_or_else(|| {
            eprintln!("invalid argument {i}: expected {low}..{high}");
            std::process::exit(2)
        })
}

fn exchange(
    addr: SocketAddr,
    request: &[u8],
    expected: &[u8],
    timeout: Duration,
) -> Result<(), usize> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|_| 0usize)?;
    stream.set_read_timeout(Some(timeout)).map_err(|_| 2usize)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| 1usize)?;
    stream.write_all(request).map_err(|_| 1usize)?;
    // Reading one extra byte detects trailing data while bounding client memory.
    // A correct reply must also end in EOF, not just have the expected prefix.
    let mut reply = Vec::with_capacity(expected.len() + 1);
    stream
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut reply)
        .map_err(|_| 2usize)?;
    if reply == expected {
        Ok(())
    } else {
        Err(3)
    }
}

fn percentile(sorted: &[u128], p: usize) -> String {
    if sorted.is_empty() {
        "null".into()
    } else {
        // Nearest rank; all reported latencies concern successful requests.
        format!(
            "{:.3}",
            sorted[(sorted.len() * p).div_ceil(100) - 1] as f64 / 1000.0
        )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let alternate_paths = args.len() == 7 && args[6] == "--alternate-paths";
    if args.len() != 6 && !alternate_paths {
        eprintln!(
            "usage: http_load PORT CLIENTS REQUESTS BODY_BYTES TIMEOUT_MS [--alternate-paths]"
        );
        std::process::exit(2);
    }
    let port = number(&args, 1, 1, 65535);
    let clients = number(&args, 2, 1, 64);
    let requests = number(&args, 3, clients, 10_000_000);
    let size = number(&args, 4, 0, 3900);
    let timeout = Duration::from_millis(number(&args, 5, 1, 60_000) as u64);
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let barrier = Arc::new(Barrier::new(clients + 1));
    let mut threads = Vec::new();
    for client in 0..clients {
        let barrier = barrier.clone();
        let count = requests / clients + usize::from(client < requests % clients);
        threads.push(std::thread::spawn(move || {
            let path = if alternate_paths { "/a" } else { "/" };
            let header = format!("POST {path} HTTP/1.0\r\nContent-Length: {size}\r\n\r\n");
            let response_header = format!("HTTP/1.0 200 OK\r\nContent-Length: {size}\r\n\r\n");
            let mut request = header.as_bytes().to_vec();
            request.resize(header.len() + size, 0);
            let mut expected = response_header.as_bytes().to_vec();
            expected.resize(response_header.len() + size, 0);
            let mut latencies = Vec::with_capacity(count);
            let mut errors = [0usize; 4];
            barrier.wait();
            for sequence in 0..count {
                if alternate_paths {
                    request[6] = if (sequence + client) % 2 == 0 {
                        b'a'
                    } else {
                        b'b'
                    };
                }
                // Vary bytes across requests/clients, including NUL and 0xff.
                // For bodies >= 8 bytes the prefix uniquely identifies a request.
                let id = (sequence * clients + client) as u64;
                for j in 0..size {
                    let byte = if j < 8 {
                        id.to_le_bytes()[j]
                    } else {
                        (j as u64 + id) as u8
                    };
                    request[header.len() + j] = byte;
                    expected[response_header.len() + j] = byte;
                }
                let start = Instant::now();
                match exchange(addr, &request, &expected, timeout) {
                    Ok(()) => latencies.push(start.elapsed().as_nanos()),
                    Err(kind) => errors[kind] += 1,
                }
            }
            (latencies, errors)
        }));
    }
    let start = Instant::now();
    barrier.wait();
    let mut latencies = Vec::with_capacity(requests);
    let mut errors = [0usize; 4];
    for thread in threads {
        let (mut samples, counts) = thread.join().unwrap();
        latencies.append(&mut samples);
        for i in 0..4 {
            errors[i] += counts[i];
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    latencies.sort_unstable();
    let failures: usize = errors.iter().sum();
    println!(
        "{{\"attempted\":{requests},\"successful\":{},\"failed\":{failures},\"elapsed_seconds\":{seconds:.6},\"goodput_per_second\":{:.3},\"success_latency_us\":{{\"p50\":{},\"p95\":{},\"p99\":{},\"max\":{}}},\"errors\":{{\"connect\":{},\"write\":{},\"read\":{},\"response\":{}}}}}",
        latencies.len(), latencies.len() as f64 / seconds,
        percentile(&latencies, 50), percentile(&latencies, 95), percentile(&latencies, 99), percentile(&latencies, 100),
        errors[0], errors[1], errors[2], errors[3]
    );
    if failures != 0 {
        std::process::exit(1);
    }
}
