"""Linux/strace acceptance: fixed worker count and pool failure cleanup.

Run cargo build, then python3 tools/check_http_pool.py. Requires loopback sockets
and ptrace permission. Traces remain in the printed temporary directory.
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
WORK = Path(tempfile.mkdtemp(prefix='verbose-pool-trace-'))
print(f'Traces: {WORK}', flush=True)
with socket.socket() as reserve:
    reserve.bind(('127.0.0.1', 0))
    port = reserve.getsockname()[1]
source = (ROOT / 'examples/http_pooled.verbose').read_text().replace('port: 18962', f'port: {port}')
(WORK / 'server.verbose').write_text(source)
(WORK / 'http_pooled.intent').write_bytes((ROOT / 'examples/http_pooled.intent').read_bytes())
binary = WORK / 'server'
subprocess.run([str(ROOT / 'target/debug/verbosec'), str(WORK / 'server.verbose'), '--native', str(binary), '--run', 'bounded_http'], check=True, capture_output=True)

def until(test):
    end = time.monotonic() + 4
    while not test():
        assert time.monotonic() < end, 'timed out waiting for pool'
        time.sleep(.01)

def request():
    with socket.create_connection(('127.0.0.1', port), timeout=3) as client:
        client.sendall(b'GET / HTTP/1.0\r\n\r\n')
        output = b''
        try:
            while chunk := client.recv(4096):
                output += chunk
        except ConnectionResetError:
            pass
        return output

expected = b'HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n'
reports = []
cases = [
    ('reuse', None, 'healthy'),
    ('fork-first', 'fork:error=EAGAIN:when=1', 'fatal'),
    ('fork-partial', 'fork:error=EAGAIN:when=2', 'fatal'),
    ('signal-setup', 'rt_sigaction:error=EINVAL:when=1', 'fatal'),
    ('socket', 'socket:error=EMFILE:when=1', 'fatal'),
    ('listen', 'listen:error=EOPNOTSUPP:when=1', 'fatal'),
    ('parent-death-setup', 'prctl:error=EINVAL:when=1', 'fatal'),
    ('wait-error', 'wait4:error=EINVAL:when=1', 'fatal'),
    ('wait-eintr', 'wait4:error=EINTR:when=1', 'healthy'),
    ('accept-eintr', 'accept:error=EINTR:when=1', 'healthy'),
    ('accept-network', 'accept:error=ENETDOWN:when=1', 'healthy'),
    ('accept-permanent', 'accept:error=EBADF:when=1', 'fatal'),
    ('parent-death-race', 'prctl:delay_enter=1s:when=1', 'kill-parent'),
]
for name, injection, mode in cases:
    trace = WORK / f'{name}.trace'
    cmd = ['strace', '-f', '-qq', '-o', str(trace)]
    if injection:
        cmd.append(f'-einject={injection}')
    proc = subprocess.Popen([*cmd, str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    trace_text = lambda: trace.read_text() if trace.exists() else ''
    try:
        if mode == 'healthy':
            until(lambda: trace_text().count('accept(') >= 2)
            for _ in range(30):
                assert request() == expected, name
            assert trace_text().count('fork(') == 2, 'forked after startup'
            os.killpg(proc.pid, signal.SIGTERM)
        elif mode == 'kill-parent':
            until(lambda: trace_text().count('fork(') == 2)
            assert 'getppid(' not in trace_text(), 'missed the parent-death setup race'
            os.kill(int(trace_text().split()[0]), signal.SIGKILL)
        stdout, stderr = proc.communicate(timeout=5)
        if mode == 'fatal':
            assert proc.returncode == 1, (name, proc.returncode)
        assert stdout == stderr == b'', (name, stdout, stderr)
        text = trace_text()
        if injection and mode != 'kill-parent':
            assert '(INJECTED)' in text, name
        assert not re.search(r'\b(mmap|brk|mremap)\(', text), name
        if name == 'fork-partial':
            assert re.search(r'kill\(\d+, SIGKILL(?:\)| <unfinished)', text), 'partial startup did not kill its child'
        # strace -f has waited for every tracee. No surviving worker can retain
        # the listener after supervisor error/death, even in the prctl race.
        with socket.socket() as probe:
            assert probe.connect_ex(('127.0.0.1', port)) != 0, name
        reports.append({'case': name, 'allocation_syscalls': 0, 'trace': str(trace)})
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate(timeout=5)

print(json.dumps({'passed': len(reports), 'cases': reports}, indent=2))
