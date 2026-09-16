"""Linux x86-64 HTTP forked/pool baseline; stdlib Python + rustc, no packages.

Build verbosec first. See docs/http-worker-benchmarks.md for scope and commands.
Artifacts and partial results survive failure. Loopback and /proc access required.
"""
import argparse
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import signal
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command(argv):
    return subprocess.check_output(argv, cwd=ROOT, text=True).strip()


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError('must be positive')
    return number


def integers(value):
    try:
        values = [int(v) for v in value.split(',')]
    except ValueError as error:
        raise argparse.ArgumentTypeError('expected comma-separated integers') from error
    if len(values) != len(set(values)):
        raise argparse.ArgumentTypeError('duplicate values')
    return values


def pinned(argv, cpus):
    return ['taskset', '-c', ','.join(map(str, cpus)), *argv] if cpus else argv


def children(pid):
    return sorted(map(int, Path(f'/proc/{pid}/task/{pid}/children').read_text().split()))


def until(test, description):
    end = time.monotonic() + 5
    while True:
        result = test()
        if result:
            return result
        if time.monotonic() >= end:
            raise RuntimeError(f'timed out waiting for {description}')
        time.sleep(.01)


def pool_snapshot(parent, count):
    pids = until(lambda: children(parent) if len(children(parent)) == count else None, 'workers')
    rows = []
    for pid in [parent, *pids]:
        stack = None
        if pid != parent:
            def accepting():
                fields = Path(f'/proc/{pid}/syscall').read_text().split()
                return fields[7] if fields[0] == '43' else None  # x86-64 accept
            stack = until(accepting, f'worker {pid} returning to accept')
        fields = {}
        for line in Path(f'/proc/{pid}/smaps_rollup').read_text().splitlines()[1:]:
            key, value = line.split(':', 1)
            fields[key] = int(value.split()[0])
        rows.append({
            'pid': pid, 'role': 'supervisor' if pid == parent else 'worker',
            'rss_kib': fields['Rss'], 'pss_kib': fields['Pss'],
            'private_kib': fields['Private_Clean'] + fields['Private_Dirty'],
            'fd_count': len(list(Path(f'/proc/{pid}/fd').iterdir())),
            'idle_stack_pointer': stack,
        })
    return {'processes': rows, **{f'total_{key}': sum(row[key] for row in rows)
                                for key in ('rss_kib', 'pss_kib', 'private_kib')}}


def identity(snapshot):
    return [(r['pid'], r['fd_count'], r['idle_stack_pointer']) for r in snapshot['processes']]


@contextmanager
def server(binary, port, cpus, work, label):
    # Files prevent a verbose/broken server from blocking on undrained pipes.
    with (work / f'{label}.stdout').open('wb') as out, (work / f'{label}.stderr').open('wb') as err:
        process = subprocess.Popen(pinned([str(binary)], cpus), stdout=out, stderr=err,
                                   start_new_session=True)
        try:
            def listening():
                if process.poll() is not None:
                    raise RuntimeError(f'server exited {process.returncode}: {label}')
                try:
                    with socket.create_connection(('127.0.0.1', port), timeout=.1):
                        return True
                except OSError:
                    return False
            until(listening, 'listener')
            yield process
            if process.poll() is not None:
                raise RuntimeError(f'server exited during load: {label}')
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
    if Path(out.name).stat().st_size or Path(err.name).stat().st_size:
        raise RuntimeError(f'unexpected server stdout/stderr: {label}')


def load(client, port, clients, requests, payload, cpus, timeout_ms=5000, wall_timeout=120,
         alternate_paths=False):
    argv = [str(client), str(port), str(clients), str(requests), str(payload), str(timeout_ms)]
    if alternate_paths:
        argv.append('--alternate-paths')
    argv = pinned(argv, cpus)
    # Per-operation timeouts are in the client. This wall bound also catches a
    # peer that drips bytes forever without triggering an individual read timeout.
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = subprocess.run(argv, capture_output=True, text=True, timeout=wall_timeout)
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    report = json.loads(result.stdout)
    # The long-lived server is not waited for here; these deltas cover the client.
    report['client_cpu_seconds'] = {'user': after.ru_utime - before.ru_utime,
                                    'system': after.ru_stime - before.ru_stime}
    report['client_exit_status'] = result.returncode
    if result.stderr:
        report['client_stderr'] = result.stderr
    return report


def check_load(report):
    if report['client_exit_status'] or report['failed'] or report.get('client_stderr'):
        raise RuntimeError(f'load correctness failed: {report}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/debug/verbosec')
    parser.add_argument('--workers', type=integers, default=[1, 4])
    parser.add_argument('--payloads', type=integers, default=[0, 1024, 3900])
    parser.add_argument('--requests', type=positive, default=30000)
    parser.add_argument('--repeats', type=positive, default=3)
    parser.add_argument('--warmup', type=positive, default=500)
    parser.add_argument('--memory-batches', type=positive, default=4)
    parser.add_argument('--memory-requests', type=positive, default=10000)
    parser.add_argument('--load-timeout', type=positive, default=120, help='wall seconds allowed per client invocation')
    parser.add_argument('--server-cpus', type=integers)
    parser.add_argument('--client-cpus', type=integers)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if any(n < 1 or n > 64 for n in args.workers):
        parser.error('workers must be in 1..64')
    if any(n < 0 or n > 3900 for n in args.payloads):
        parser.error('payloads must be in 0..3900')
    if min(args.requests, args.warmup, args.memory_requests) < max(args.workers):
        parser.error('request counts must be at least the largest worker count')
    if max(args.requests, args.warmup, args.memory_requests) > 10000000:
        parser.error('request counts must be at most 10000000')
    for cpus in (args.server_cpus, args.client_cpus):
        if cpus and not set(cpus) <= os.sched_getaffinity(0):
            parser.error('CPU set is outside current affinity')
    args.compiler = args.compiler.resolve()
    work = Path(tempfile.mkdtemp(prefix='verbose-http-bench-'))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    print(f'Artifacts: {work}', flush=True)
    cpuinfo = Path('/proc/cpuinfo').read_text()
    cpu = next(line.split(':', 1)[1].strip() for line in cpuinfo.splitlines() if line.startswith('model name'))
    report = {
        'schema_version': 1, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
        'revision': command(['git', 'rev-parse', 'HEAD']),
        'working_tree_status': command(['git', 'status', '--short']),
        'compiler_sha256': digest(args.compiler), 'rustc': command(['rustc', '--version']),
        'harness_sha256': digest(__file__), 'client_source_sha256': digest(ROOT / 'tools/http_load.rs'),
        'host': {'kernel': platform.release(), 'cpu': cpu, 'logical_cpus': os.cpu_count(),
                 'affinity': sorted(os.sched_getaffinity(0)), 'loadavg_before': os.getloadavg(),
                 'thread_siblings': {str(i): Path(f'/sys/devices/system/cpu/cpu{i}/topology/thread_siblings_list').read_text().strip()
                                     for i in sorted(os.sched_getaffinity(0))},
                 'network_sysctls': {key: Path('/proc/sys/net', key).read_text().strip() for key in
                                     ('ipv4/ip_local_port_range', 'ipv4/tcp_tw_reuse', 'core/somaxconn')}},
        'config': {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
        'artifacts': str(work), 'builds': [], 'runs': [], 'memory': [],
    }

    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')

    def measure(port, workers, requests, payload):
        return load(client, port, workers, requests, payload, args.client_cpus, wall_timeout=args.load_timeout)

    try:
        client = work / 'http_load'
        subprocess.run(['rustc', '--edition=2021', '-O', str(ROOT / 'tools/http_load.rs'), '-o', str(client)], check=True)
        report['client_sha256'] = digest(client)
        template = (ROOT / 'examples/http_pooled.verbose').read_text()
        (work / 'http_pooled.intent').write_bytes((ROOT / 'examples/http_pooled.intent').read_bytes())
        builds = {}
        for workers in args.workers:
            for mode in ('forked', 'pooled'):
                with socket.socket() as reserve:
                    reserve.bind(('127.0.0.1', 0))
                    port = reserve.getsockname()[1]
                source = template.replace('port: 18962', f'port: {port}')
                source = source.replace('  concurrency: pooled\n  workers: 2\n',
                                        f'  concurrency: {mode}\n' + (f'  workers: {workers}\n' if mode == 'pooled' else ''))
                path = work / f'{mode}-{workers}.verbose'
                path.write_text(source)
                binary = path.with_suffix('')
                compiled = subprocess.run([str(args.compiler), str(path), '--native', str(binary), '--run', 'bounded_http'],
                                          capture_output=True, text=True)
                if compiled.returncode:
                    raise RuntimeError(f'compile failed: {compiled.stdout}\n{compiled.stderr}')
                builds[mode, workers] = (binary, port)
                report['builds'].append({'mode': mode, 'clients': workers, 'port': port,
                                         'source': source, 'binary_sha256': digest(binary), 'binary_bytes': binary.stat().st_size})
        save()
        for workers in args.workers:
            for payload in args.payloads:
                for repeat in range(args.repeats):
                    modes = ('forked', 'pooled') if repeat % 2 == 0 else ('pooled', 'forked')
                    for mode in modes:
                        label = f'{mode}-c{workers}-b{payload}-r{repeat}'
                        binary, port = builds[mode, workers]
                        with server(binary, port, args.server_cpus, work, label):
                            warmup = measure(port, workers, args.warmup, payload)
                            row = {'mode': mode, 'clients': workers, 'body_bytes': payload, 'repeat': repeat, 'warmup': warmup}
                            report['runs'].append(row)
                            save()
                            check_load(warmup)
                            row['measurement'] = measure(port, workers, args.requests, payload)
                            save()
                            check_load(row['measurement'])
                        print(f"{label}: {row['measurement']['goodput_per_second']:.0f} responses/s", flush=True)
        for workers in args.workers:
            binary, port = builds['pooled', workers]
            row = {'workers': workers, 'snapshots': [], 'loads': []}
            report['memory'].append(row)
            with server(binary, port, args.server_cpus, work, f'memory-{workers}') as process:
                for batch in range(args.memory_batches + 1):
                    count = args.warmup if batch == 0 else args.memory_requests
                    payload = 3900 if batch % 2 == 0 else 0
                    measured = measure(port, workers, count, payload)
                    row['loads'].append({'body_bytes': payload, **measured})
                    save()
                    check_load(measured)
                    snapshot = pool_snapshot(process.pid, workers)
                    snapshot['completed_requests'] = args.warmup + batch * args.memory_requests
                    row['snapshots'].append(snapshot)
                    save()
                    if identity(snapshot) != identity(row['snapshots'][0]):
                        raise RuntimeError('pool changed PIDs, descriptors, or idle stack positions')
                row['stable_pids_fds_and_stack'] = True
                print(f"pool-{workers} PSS KiB: {[s['total_pss_kib'] for s in row['snapshots']]}", flush=True)
        report['status'] = 'passed'
    except BaseException as error:
        report['status'] = 'failed'
        report['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        report['host']['loadavg_after'] = os.getloadavg()
        save()
        print(f'Report: {args.output} ({report["status"]})', flush=True)


if __name__ == '__main__':
    main()
