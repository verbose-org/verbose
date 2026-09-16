"""Linux/strace acceptance for bounded SIGTERM shutdown, including failures.

Run cargo build, then python3 tools/check_http_shutdown.py. Requires loopback and
ptrace access. The printed temporary directory retains traces and a JSON report.
"""
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
WORK = Path(tempfile.mkdtemp(prefix='verbose-shutdown-trace-'))
print(f'Traces: {WORK}', flush=True)
with socket.socket() as reserve:
    reserve.bind(('127.0.0.1', 0))
    port = reserve.getsockname()[1]
source = (ROOT / 'examples/http_shutdown.verbose').read_text().replace('port: 18963', f'port: {port}')
source = source.replace('shutdown_timeout: 5', 'shutdown_timeout: 1')
(WORK / 'server.verbose').write_text(source)
(WORK / 'http_shutdown.intent').write_bytes((ROOT / 'examples/http_shutdown.intent').read_bytes())
binary = WORK / 'server'
subprocess.run([str(ROOT / 'target/debug/verbosec'), str(WORK / 'server.verbose'), '--native', str(binary), '--run', 'bounded_http'], check=True, capture_output=True)


def until(test):
    end = time.monotonic() + 6
    while not test():
        assert time.monotonic() < end, 'timed out waiting for pool'
        time.sleep(.01)


def request():
    with socket.create_connection(('127.0.0.1', port), timeout=3) as client:
        client.sendall(b'GET / HTTP/1.0\r\n\r\n')
        reply = b''
        while chunk := client.recv(4096):
            reply += chunk
        assert reply == b'HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n'


cases = [
    ('reuse-and-grace', None, 'requests', 0),
    ('force-stopped-worker', None, 'force', 1),
    ('mask-failure', 'rt_sigprocmask:error=EINVAL:when=1', 'fatal', 1),
    ('term-disposition-failure', 'rt_sigaction:error=EINVAL:when=2', 'fatal', 1),
    ('fork-first', 'fork:error=EAGAIN:when=1', 'fatal', 1),
    ('fork-partial', 'fork:error=EAGAIN:when=2', 'fatal', 1),
    ('child-setup-failure', 'prctl:error=EINVAL:when=1', 'fatal', 1),
    ('signal-wait-failure', 'rt_sigtimedwait:error=EINVAL:when=1', 'fatal', 1),
    ('signal-wait-eintr', 'rt_sigtimedwait:error=EINTR:when=1', 'requests', 0),
    ('signal-wait-eintr-deadline', 'rt_sigtimedwait:error=EINTR:when=2+', 'force', 1),
    ('wait-failure', 'wait4:error=EINVAL:when=1', 'fatal', 1),
    ('wait-echild', 'wait4:error=ECHILD:when=1', 'fatal', 1),
    ('wait-eintr', 'wait4:error=EINTR:when=1', 'requests', 0),
    ('clock-failure', 'clock_gettime:error=EINVAL:when=1', 'term', 1),
    ('shutdown-failure', 'shutdown:error=EINVAL:when=1', 'term', 1),
    ('accept-live-einval', 'accept:error=EINVAL:when=1', 'fatal', 1),
    ('socket-query-failure', 'getsockopt:error=EIO:when=1', 'term', 1),
    ('parent-death-race', 'prctl:delay_enter=1s:when=1', 'kill-parent', -9),
]
reports = []
for name, injection, mode, expected_status in cases:
    trace = WORK / f'{name}.trace'
    argv = ['strace', '-f', '-qq', '-o', str(trace)]
    if injection:
        argv.append(f'-einject={injection}')
    proc = subprocess.Popen([*argv, str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    trace_text = lambda: trace.read_text() if trace.exists() else ''
    clients = []
    try:
        until(lambda: bool(trace_text().strip()))
        parent = int(trace_text().split()[0])
        if mode == 'kill-parent':
            until(lambda: trace_text().count('fork(') == 2)
            assert 'getppid(' not in trace_text(), 'missed parent-death setup race'
            os.kill(parent, signal.SIGKILL)
        elif mode != 'fatal':
            until(lambda: trace_text().count('accept(') >= 2 and 'rt_sigtimedwait(' in trace_text())
            if mode == 'requests':
                for _ in range(20):
                    request()
                assert trace_text().count('fork(') == 2
            if mode == 'force':
                pids = list(map(int, Path(f'/proc/{parent}/task/{parent}/children').read_text().split()))
                assert len(pids) == 2
                for _ in pids:
                    client = socket.create_connection(('127.0.0.1', port), timeout=3)
                    client.sendall(b'POST / HTTP/1.0\r\nContent-Length: 8\r\n\r\nx')
                    clients.append(client)
                until(lambda: trace_text().count('poll(') >= 2)
                # Keep the first signal-wait return reserved for SIGTERM in
                # the persistent-EINTR case. A stop would queue SIGCHLD first.
                if name != 'signal-wait-eintr-deadline':
                    os.kill(pids[0], signal.SIGSTOP)
            os.kill(parent, signal.SIGTERM)
        stdout, stderr = proc.communicate(timeout=6)
        assert proc.returncode == expected_status, (name, proc.returncode, expected_status)
        assert stdout == stderr == b'', (name, stdout, stderr)
        text = trace_text()
        if injection and mode != 'kill-parent':
            assert '(INJECTED)' in text, name
        assert not re.search(r'\b(mmap|brk|mremap)\(', text), name
        kills = re.findall(r'(?m)^(\d+)\s+kill\((\d+), SIGKILL', text)
        assert all(int(caller) == parent for caller, _ in kills), 'worker ran supervisor cleanup'
        if name == 'fork-partial' or mode == 'force':
            assert kills, 'did not kill owned workers'
        if expected_status == 0:
            assert not kills, 'clean drain force-killed a worker'
            assert re.search(r'shutdown\(\d+, SHUT_RD\)\s+= 0', text), 'missing listener cutoff'
        if name == 'accept-live-einval':
            assert 'SO_ACCEPTCONN' in text and '[1]' in text, 'live-listener failure was not checked'
        with socket.socket() as probe:
            assert probe.connect_ex(('127.0.0.1', port)) != 0, 'listener survived'
        reports.append({'case': name, 'exit_status': proc.returncode, 'allocation_syscalls': 0, 'trace': str(trace)})
        print(f'{name}: passed', flush=True)
    finally:
        for client in clients:
            client.close()
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate(timeout=6)

(WORK / 'report.json').write_text(json.dumps({'passed': len(reports), 'cases': reports}, indent=2) + '\n')
print(f'{len(reports)} cases passed')
