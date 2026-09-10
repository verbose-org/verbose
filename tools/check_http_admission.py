"""Optional Linux/strace acceptance checks for bounded HTTP admission.

Build verbosec with `cargo build`, then run `python3 tools/check_http_admission.py`.
Uses loopback sockets and ptrace; retains traces in the printed temporary path.
Only Python's standard library and strace are required.
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
WORK = Path(tempfile.mkdtemp(prefix='verbose-admission-faults-'))
source = (ROOT / 'examples/http_capped.verbose').read_text()
(WORK / 'http_capped.intent').write_bytes((ROOT / 'examples/http_capped.intent').read_bytes())
with socket.socket() as reserve:
    reserve.bind(('127.0.0.1', 0))
    port = reserve.getsockname()[1]
(WORK / 'server.verbose').write_text(source.replace('port: 18961', f'port: {port}').replace('max_connections: 8', 'max_connections: 2'))
binary = WORK / 'server'
subprocess.run([str(ROOT / 'target/debug/verbosec'), str(WORK / 'server.verbose'), '--native', str(binary), '--run', 'bounded_http'], check=True, capture_output=True)

def wait_for(test):
    end = time.monotonic() + 4
    while not test():
        assert time.monotonic() < end, 'timed out waiting for trace/runtime'
        time.sleep(.01)

def connect():
    s = socket.create_connection(('127.0.0.1', port), timeout=3)
    s.settimeout(3)
    return s

def read_all(s):
    result = b''
    try:
        while data := s.recv(4096):
            result += data
    except ConnectionResetError:
        pass
    return result

expected = b'HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n'
reports = []
cases = [
    ('fork', 'EAGAIN', 'recover'),
    ('accept', 'EAGAIN', 'retry'),
    ('accept', 'EINTR', 'retry'),
    ('accept', 'ENETDOWN', 'retry'),
    ('accept', 'EBADF', 'fatal_accept'),
    ('socket', 'EMFILE', 'startup'),
    ('bind', 'EADDRINUSE', 'startup'),
    ('listen', 'EOPNOTSUPP', 'startup'),
    ('fcntl', 'EBADF', 'startup'),
    ('rt_sigaction', 'EINVAL', 'startup'),
    ('wait4', 'ENOMEM', 'startup'),
    ('poll', 'EIO', 'startup'),
]
for syscall, errno, mode in cases:
    name = f'{syscall}-{errno}'
    trace = WORK / f'{name}.trace'
    proc = subprocess.Popen(['strace', '-f', '-qq', '-o', str(trace), f'-einject={syscall}:error={errno}:when=1', str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    trace_text = lambda: trace.read_text() if trace.exists() else ''
    try:
        if mode == 'startup':
            stdout, stderr = proc.communicate(timeout=4)
            assert proc.returncode == 1, (name, proc.returncode)
        else:
            wait_for(lambda: re.search(r'listen\(.*\)\s+= 0', trace_text()))
            with connect() as s:
                s.sendall(b'GET / HTTP/1.0\r\n\r\n')
                output = read_all(s)
                assert output == (expected if mode == 'retry' else b''), (name, output)
            if mode == 'fatal_accept':
                stdout, stderr = proc.communicate(timeout=4)
                assert proc.returncode == 1
            else:
                time.sleep(.15)
                with connect() as s:
                    s.sendall(b'GET / HTTP/1.0\r\n\r\n')
                    assert read_all(s) == expected, name
                time.sleep(.15)
                os.killpg(proc.pid, signal.SIGTERM)
                stdout, stderr = proc.communicate(timeout=4)
        assert stdout == stderr == b'', (name, stdout, stderr)
        trace_data = trace_text()
        assert '(INJECTED)' in trace_data, name
        assert not re.search(r'\b(mmap|brk|mremap)\(', trace_data), name
        if mode in ('startup', 'fatal_accept'):
            assert not re.search(r'\bfork\(', trace_data), name
        if mode == 'retry':
            failed = trace_data.index('(INJECTED)')
            next_accept = trace_data.index('accept(', failed)
            assert 'fork(' not in trace_data[failed:next_accept], name
        reports.append({'case': name, 'outcome': mode, 'allocation_syscalls': 0, 'trace': str(trace)})
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate(timeout=4)

# Keep two clients live while rejecting a burst; then free and reuse a slot.
trace = WORK / 'overload.trace'
proc = subprocess.Popen(['strace', '-f', '-qq', '-o', str(trace), str(binary)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
trace_text = lambda: trace.read_text() if trace.exists() else ''
held = []
try:
    wait_for(lambda: re.search(r'listen\(.*\)\s+= 0', trace_text()))
    server_pid = int(trace_text().split()[0])
    child_file = Path(f'/proc/{server_pid}/task/{server_pid}/children')
    children = lambda: child_file.read_text().split()
    for _ in range(2):
        client = connect()
        client.sendall(b'POST / HTTP/1.0\r\nContent-Length: 2\r\n\r\nx')
        held.append(client)
    wait_for(lambda: len(children()) == 2)
    for _ in range(10):
        with connect() as client:
            client.sendall(b'GET /full HTTP/1.0\r\n\r\n')
            assert read_all(client) == b''
        assert len(children()) == 2
    assert trace_text().count('fork(') == 2, 'overload forked a child'
    held[0].sendall(b'y')
    assert read_all(held[0]) == expected.replace(b'Length: 0', b'Length: 2') + b'xy'
    held[0].close()
    wait_for(lambda: len(children()) == 1)
    with connect() as client:
        client.sendall(b'GET / HTTP/1.0\r\n\r\n')
        assert read_all(client) == expected
    held[1].close()
    wait_for(lambda: len(children()) == 0)
    os.killpg(proc.pid, signal.SIGTERM)
    stdout, stderr = proc.communicate(timeout=4)
    assert stdout == stderr == b''
    assert not re.search(r'\b(mmap|brk|mremap)\(', trace_text())
    reports.append({'case': 'overload-and-recovery', 'allocation_syscalls': 0, 'trace': str(trace)})
finally:
    for client in held:
        client.close()
    if proc.poll() is None:
        os.killpg(proc.pid, signal.SIGKILL)
        proc.communicate(timeout=4)

print(json.dumps({'passed': len(reports), 'cases': reports}, indent=2))
