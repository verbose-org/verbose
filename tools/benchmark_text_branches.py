"""Compare pre/post-overlay compilers on the same bounded-text HTTP program.

Linux x86-64, Python standard library + rustc. Alternating /a and /b requests
exercise exclusive owned buffers; every binary response must match byte-for-byte.
This measures closed-loop HTTP latency/goodput and sampled pool residency, not
hardware cache hits. See docs/bounded-text-http-benchmark.md.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import socket
import struct
import subprocess
import tempfile

from benchmark_http import (ROOT, check_load, children, command, digest, identity,
                            integers, load, pool_snapshot, positive, server)

SOURCE = '''@verbose 0.1.0
rule piece
  @intention: "Own a bounded copy of the counted binary request body"
  @source: branches.intent:1
  input:
    input : HttpRequest
  output:
    out : text [..4096]
  logic:
    out = concat("", input.body)
  proofs:
    purity:
      reads : [input.body]
      calls : []
    termination:
      bound : 8
rule handle
  @intention: "Select an owned body from exactly one branch"
  @source: branches.intent:2
  input:
    req : HttpRequest
  output:
    resp : HttpResponse
  logic:
    let selected = if req.path == "/a" then HttpResponse { status: 200, body: piece(req) } else HttpResponse { body: piece(req), status: 200 }
    resp = selected
  proofs:
    purity:
      reads : [req, req.path]
      calls : [piece]
    termination:
      bound : 64
service bounded_http
  @intention: "Compare placement with identical pooled transport"
  @source: branches.intent:3
  listen:
    protocol: http_1_0
    port: PORT
    max_request: 4096
  handler: handle
  request_timeout: 2
  response_timeout: 2
  concurrency: pooled
  workers: WORKERS
'''


def layout_pair(before, after):
    """Fail closed unless only frame/address immediates change in this fixture."""
    prologue = bytes.fromhex('55534989ea4889e54881ec')
    if len(before) != len(after) or before.count(prologue) != 1 or after.count(prologue) != 1:
        raise RuntimeError('expected equal-size binaries with one bounded invocation frame')
    at = before.index(prologue) + len(prologue)
    if at != after.index(prologue) + len(prologue):
        raise RuntimeError('invocation instructions moved')
    sizes = [struct.unpack_from('<I', blob, at)[0] for blob in [before, after]]
    if sizes[0] - sizes[1] != 4096:
        raise RuntimeError(f'expected one 4096-byte branch buffer saved, got frames {sizes}')
    allowed = set(range(at, at + 4))
    start = 0
    while (at := before.find(bytes.fromhex('488d85'), start)) >= 0:
        allowed.update(range(at + 3, at + 7))  # lea rax, [rbp + disp32]
        start = at + 7
    differences = {i for i, (a, b) in enumerate(zip(before, after)) if a != b}
    if not differences <= allowed:
        raise RuntimeError('binaries differ beyond frame/address immediates')
    return {'before_frame_bytes': sizes[0], 'after_frame_bytes': sizes[1],
            'saved_frame_bytes': sizes[0] - sizes[1], 'binary_bytes': len(before),
            'only_layout_immediates_differ': True}


def cpu_ticks(parent):
    rows = {}
    for pid in [parent, *children(parent)]:
        fields = Path(f'/proc/{pid}/stat').read_text().rsplit(')', 1)[1].split()
        rows[str(pid)] = (int(fields[11]), int(fields[12]))
    return rows


def cpu_delta(before, after):
    if before.keys() != after.keys():
        raise RuntimeError('pool PIDs changed during measurement')
    ticks = os.sysconf('SC_CLK_TCK')
    return {name: sum(after[p][i] - before[p][i] for p in before) / ticks
            for i, name in enumerate(['user', 'system'])}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--reference-compiler', type=Path, required=True)
    parser.add_argument('--reference-revision', required=True, help='recorded source revision of the supplied reference')
    parser.add_argument('--workers', type=integers, default=[1, 4])
    parser.add_argument('--payloads', type=integers, default=[0, 1024, 3900])
    parser.add_argument('--requests', type=positive, default=30000)
    parser.add_argument('--warmup', type=positive, default=500)
    parser.add_argument('--repeats', type=positive, default=3)
    parser.add_argument('--memory-requests', type=positive, default=2000)
    parser.add_argument('--load-timeout', type=positive, default=120)
    parser.add_argument('--server-cpus', type=integers)
    parser.add_argument('--client-cpus', type=integers)
    parser.add_argument('--host-note', default='Host activity outside Linux not reported')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if any(w < 1 or w > 64 for w in args.workers) or any(p < 0 or p > 3900 for p in args.payloads):
        parser.error('workers must be in 1..64; payloads in 0..3900')
    if any(n < max(args.workers) or n > 10000000 for n in [args.requests, args.warmup, args.memory_requests]):
        parser.error('request counts must be between largest worker count and 10000000')
    for cpus in [args.server_cpus, args.client_cpus]:
        if cpus and not set(cpus) <= os.sched_getaffinity(0):
            parser.error('CPU set outside current affinity')
    work = Path(tempfile.mkdtemp(prefix='verbose-branch-http-'))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    print(f'Artifacts: {work}', flush=True)
    compilers = {'before': args.reference_compiler.resolve(), 'after': args.compiler.resolve()}
    report = {
        'schema_version': 1, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
        'revision': command(['git', 'rev-parse', 'HEAD']),
        'working_tree_status': command(['git', 'status', '--short']),
        'compiler_sha256': {label: digest(path) for label, path in compilers.items()},
        'harness_sha256': digest(__file__), 'load_harness_sha256': digest(ROOT / 'tools/benchmark_http.py'),
        'client_source_sha256': digest(ROOT / 'tools/http_load.rs'),
        'rustc': command(['rustc', '--version']), 'artifacts': str(work),
        'config': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        'host': {'kernel': platform.release(), 'cpu': next(line.split(':', 1)[1].strip()
                  for line in Path('/proc/cpuinfo').read_text().splitlines() if line.startswith('model name')),
                 'affinity': sorted(os.sched_getaffinity(0)), 'loadavg_before': os.getloadavg(),
                 'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
                 'thread_siblings': {str(i): Path(f'/sys/devices/system/cpu/cpu{i}/topology/thread_siblings_list').read_text().strip()
                                     for i in sorted(os.sched_getaffinity(0))}},
        'cache_counters': 'not measured', 'builds': [], 'runs': [], 'memory': [],
    }

    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')

    def measure(port, workers, requests, payload):
        return load(client, port, workers, requests, payload, args.client_cpus,
                    wall_timeout=args.load_timeout, alternate_paths=True)

    try:
        client = work / 'client'
        subprocess.run(['rustc', '--edition=2021', '-O', str(ROOT / 'tools/http_load.rs'), '-o', str(client)], check=True)
        report['client_sha256'] = digest(client)
        (work / 'branches.intent').write_text('Bound a copy.\nSelect the body.\nServe the comparison.\n')
        builds = {}
        for workers in args.workers:
            with socket.socket() as reserve:
                reserve.bind(('127.0.0.1', 0))
                port = reserve.getsockname()[1]
            source = SOURCE.replace('PORT', str(port)).replace('WORKERS', str(workers))
            path = work / f'workers-{workers}.verbose'
            path.write_text(source)
            binaries = {}
            for label, compiler in compilers.items():
                binary = work / f'{label}-{workers}'
                compiled = subprocess.run([str(compiler), str(path), '--run', 'bounded_http', '--native', str(binary)],
                                          capture_output=True, text=True)
                (work / f'{label}-{workers}.compile.stdout').write_text(compiled.stdout)
                (work / f'{label}-{workers}.compile.stderr').write_text(compiled.stderr)
                if compiled.returncode:
                    raise RuntimeError(f'{label} compilation failed: {compiled.stderr}')
                binaries[label] = binary
                builds[label, workers] = (binary, port)
            report['builds'].append({'workers': workers, 'source': source, 'port': port,
                'binary_sha256': {k: digest(v) for k, v in binaries.items()},
                **layout_pair(binaries['before'].read_bytes(), binaries['after'].read_bytes())})
        save()
        for workers in args.workers:
            for payload in args.payloads:
                for repeat in range(args.repeats):
                    order = ('before', 'after') if repeat % 2 == 0 else ('after', 'before')
                    for label in order:
                        name = f'{label}-w{workers}-b{payload}-r{repeat}'
                        binary, port = builds[label, workers]
                        row = {'variant': label, 'workers': workers, 'body_bytes': payload, 'repeat': repeat}
                        report['runs'].append(row)
                        with server(binary, port, args.server_cpus, work, name) as process:
                            row['warmup'] = measure(port, workers, args.warmup, payload)
                            save()
                            check_load(row['warmup'])
                            row['memory_before'] = pool_snapshot(process.pid, workers)
                            ticks = cpu_ticks(process.pid)
                            row['measurement'] = measure(port, workers, args.requests, payload)
                            row['server_cpu_seconds'] = cpu_delta(ticks, cpu_ticks(process.pid))
                            save()
                            check_load(row['measurement'])
                            row['memory_after'] = pool_snapshot(process.pid, workers)
                            if identity(row['memory_before']) != identity(row['memory_after']):
                                raise RuntimeError('worker PIDs, descriptors or idle stacks changed')
                        save()
                        print(f"{name}: {row['measurement']['goodput_per_second']:.0f} responses/s, p99 {row['measurement']['success_latency_us']['p99']:.1f} us", flush=True)
        for workers in args.workers:
            for label in ('before', 'after'):
                binary, port = builds[label, workers]
                row = {'variant': label, 'workers': workers, 'snapshots': [], 'loads': []}
                report['memory'].append(row)
                with server(binary, port, args.server_cpus, work, f'memory-{label}-{workers}') as process:
                    for batch, payload in enumerate([3900, 0, 3900, 0, 3900]):
                        measured = measure(port, workers, args.warmup if batch == 0 else args.memory_requests, payload)
                        row['loads'].append({'body_bytes': payload, **measured})
                        save()
                        check_load(measured)
                        snapshot = pool_snapshot(process.pid, workers)
                        row['snapshots'].append(snapshot)
                        save()
                        if identity(snapshot) != identity(row['snapshots'][0]):
                            raise RuntimeError('worker PIDs, descriptors or idle stacks changed')
                    row['stable_pids_fds_and_stack'] = True
        report['status'] = 'passed'
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        report['host']['loadavg_after'] = os.getloadavg()
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        save()
        print(f'Report: {args.output} ({report["status"]})', flush=True)


if __name__ == '__main__':
    main()
