"""Linux/strace acceptance for bounded text transferred into persistent state.

Run cargo build, then python3 tools/check_bounded_text_state.py. Requires loopback
sockets, strace and ptrace permission. Uses Python's standard library. Retains
sources, binaries, traces and a report in the printed temporary directory.
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
WORK = Path(tempfile.mkdtemp(prefix='verbose-text-state-faults-'))
print(f'Traces: {WORK}', flush=True)
source = (ROOT / 'examples/bounded_text_state.verbose').read_text()
(WORK / 'bounded_text_state.intent').write_bytes(
    (ROOT / 'examples/bounded_text_state.intent').read_bytes())
with socket.socket() as reserve:
    reserve.bind(('127.0.0.1', 0))
    port = reserve.getsockname()[1]
(WORK / 'server.verbose').write_text(source.replace('port: 18965', f'port: {port}'))
binary = WORK / 'server'
subprocess.run([str(ROOT / 'target/debug/verbosec'), str(WORK / 'server.verbose'),
                '--native', str(binary), '--run', 'remember_http'],
               check=True, capture_output=True)

# A verified source cannot exceed this bound. Lower the emitted copy guard to
# zero to exercise its defensive exit without permitting an out-of-bounds copy.
guard = b'\x48\x81\xfa' + (4098).to_bytes(4, 'little') + b'\x0f\x87'
blob = binary.read_bytes()
assert blob.count(guard) == 1, 'expected one persistent-copy capacity guard'
backstop = WORK / 'server-capacity-backstop'
backstop.write_bytes(blob.replace(guard, b'\x48\x81\xfa' + bytes(4) + b'\x0f\x87'))
backstop.chmod(0o755)


def wait_for(test):
    deadline = time.monotonic() + 5
    while not (result := test()):
        assert time.monotonic() < deadline, 'timed out waiting for trace/runtime'
        time.sleep(.01)
    return result


def request(path, body=None):
    method = b'GET' if body is None else b'POST'
    header = b'' if body is None else f'Content-Length: {len(body)}\r\n'.encode()
    with socket.create_connection(('127.0.0.1', port), timeout=4) as client:
        client.sendall(method + b' ' + path + b' HTTP/1.0\r\n' + header
                       + b'\r\n' + (body or b''))
        client.shutdown(socket.SHUT_WR)
        result = b''
        try:
            while data := client.recv(8192):
                result += data
        except ConnectionResetError:
            pass
        return result


def wire(value):
    body = b'prev:' + value
    return f'HTTP/1.0 200 OK\r\nContent-Length: {len(body)}\r\n\r\n'.encode() + body


def idle(pid):
    words = Path(f'/proc/{pid}/syscall').read_text().split()
    if words and words[0] == '43':  # accept: iteration temporaries released
        return dict(stack=words[-2], descriptors=len(list(Path(f'/proc/{pid}/fd').iterdir())))
    return None


# This fixture sends six small segments per complete response. Target the next
# response's first segment and final body separately; pin the injection location
# against the trace so a transport change cannot silently test another request.
cases = [
    ('success', None),
    ('header-failure', 'sendto:error=EPIPE:when=7'),
    ('body-failure', 'sendto:error=EPIPE:when=12'),
    ('receive-failure', 'recvfrom:error=EIO:when=2'),
    ('capacity-backstop', None),
]
reports = []
for name, injection in cases:
    trace = WORK / f'{name}.trace'
    command = ['strace', '-f', '-qq', '-o', str(trace)]
    if injection:
        command.append(f'-einject={injection}')
    executable = backstop if name == 'capacity-backstop' else binary
    proc = subprocess.Popen([*command, str(executable)], stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, start_new_session=True)
    trace_text = lambda: trace.read_text() if trace.exists() else ''
    try:
        listen = wait_for(lambda: re.search(r'^(\d+)\s+listen\(.*\)\s+= 0', trace_text(), re.M))
        pid = int(listen[1])
        initial = wait_for(lambda: idle(pid))
        assert request(b'/seed', b'seed\x00\xc3\xa9') == wire(b'none'), name
        if name == 'capacity-backstop':
            stdout, stderr = proc.communicate(timeout=5)
            assert proc.returncode == 1, proc.returncode
            assert stdout == stderr == b'', (stdout, stderr)
            assert not re.search(r'\b(mmap|brk|mremap)\(', trace_text()), name
            reports.append(dict(case=name, exit_status=1, allocation_syscalls=0, trace=str(trace)))
            print(f'{name}: passed', flush=True)
            continue
        wait_for(lambda: idle(pid))
        assert '(INJECTED)' not in trace_text(), 'failure preceded the intended request'
        seed = b'[seed\x00\xc3\xa9]'
        result = request(b'/discard', b'changed\xff\x00')
        if injection:
            assert result != wire(seed) and wire(seed).startswith(result), (name, result)
            assert '(INJECTED)' in trace_text(), 'failure missed the intended request'
            previous = seed
        else:
            assert result == wire(seed), (name, result)
            previous = b'[changed\xff\x00]'
        assert request(b'/recovered') == wire(previous), name
        assert request(b'/last') == wire(b'</recovered>'), name
        final = wait_for(lambda: idle(pid))
        assert final == initial, (name, initial, final)
        os.killpg(proc.pid, signal.SIGTERM)
        stdout, stderr = proc.communicate(timeout=5)
        assert proc.returncode == -signal.SIGTERM, (name, proc.returncode)
        assert stdout == stderr == b'', (name, stdout, stderr)
        assert not re.search(r'\b(mmap|brk|mremap)\(', trace_text()), name
        reports.append(dict(case=name, allocation_syscalls=0,
                            stable_stack=True, stable_descriptors=True, trace=str(trace)))
        print(f'{name}: passed', flush=True)
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate(timeout=5)

(WORK / 'report.json').write_text(json.dumps(reports, indent=2) + '\n')
