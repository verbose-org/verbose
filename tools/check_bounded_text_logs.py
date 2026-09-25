"""Linux/strace checks for logs borrowing a bounded HTTP response.

Run cargo build, then python3 tools/check_bounded_text_logs.py. Standard library
only; requires strace, ptrace permission and loopback sockets. Keeps artifacts.
Checks the exact service stack ceiling, log/send failure ordering, frame/fd reclamation, allocation syscalls and
a negative control that deliberately releases response storage before logging.
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
WORK = Path(tempfile.mkdtemp(prefix='verbose-text-log-faults-'))
print(f'Traces: {WORK}', flush=True)
source = (ROOT / 'examples/http_bounded_log.verbose').read_text()
(WORK / 'http_bounded_log.intent').write_bytes(
    (ROOT / 'examples/http_bounded_log.intent').read_bytes())
with socket.socket() as reserve:
    reserve.bind(('127.0.0.1', 0))
    port = reserve.getsockname()[1]
logfile = WORK / 'response.log'
source = source[:source.index('  log:\n')]
source += f'''  log:
    append_file "{logfile}" concat("A:", resp.body, "\\n")
    on_error: POLICY
  log:
    append_file "{logfile}" concat("B:", resp.body, "\\n")
    on_error: POLICY
'''
for policy in ['abort', 'drop']:
    path = WORK / f'{policy}.verbose'
    path.write_text(source.replace('port: 18966', f'port: {port}').replace('POLICY', policy))
    path.write_text(path.read_text() + '  native_stack: 65536\n')
    report = json.loads(subprocess.run([
        str(ROOT / 'target/debug/verbosec'), str(path), '--stack-report', '--json'],
        check=True, capture_output=True).stdout)
    peaks = [entry['stack_bound_bytes'] for entry in report['logs']]
    assert report['log_stack_bytes'] == max(peaks) < sum(peaks)
    path.write_text(path.read_text().replace('native_stack: 65536',
        f"native_stack: {report['stack_bound_bytes']}"))
    subprocess.run([str(ROOT / 'target/debug/verbosec'), str(path), '--native',
                    str(WORK / policy), '--run', 'bounded_log_http'],
                   check=True, capture_output=True)

# Keep instruction offsets unchanged. Instead of restoring rbx from [rbp],
# discard the bounded frame too early, while restoring the transport rbp as
# usual. A large log now overwrites still-borrowed response storage.
blob = (WORK / 'abort').read_bytes()
restore = bytes.fromhex('488b9d000000004c89d5')
assert blob.count(restore) == 1, 'expected one bounded HTTP frame publication'
broken = WORK / 'released-before-log'
broken.write_bytes(blob.replace(restore, bytes.fromhex('488d65089090904c89d5')))
broken.chmod(0o755)


def wait_for(test):
    deadline = time.monotonic() + 5
    while not (result := test()):
        assert time.monotonic() < deadline, 'timed out waiting for trace/runtime'
        time.sleep(.01)
    return result


def request(body):
    with socket.create_connection(('127.0.0.1', port), timeout=4) as client:
        client.sendall(f'POST / HTTP/1.0\r\nContent-Length: {len(body)}\r\n\r\n'.encode() + body)
        client.shutdown(socket.SHUT_WR)
        out = b''
        try:
            while data := client.recv(8192):
                out += data
        except ConnectionResetError:
            pass
        return out


def response(body):
    value = b'[' + body + b']'
    return f'HTTP/1.0 200 OK\r\nContent-Length: {len(value)}\r\n\r\n'.encode() + value


def logged(body, prefix=b'A:'):
    return prefix + b'[' + body + b']\n'


def idle(pid):
    words = Path(f'/proc/{pid}/syscall').read_text().split()
    if words and words[0] == '43':
        return dict(stack=words[-2], descriptors=len(list(Path(f'/proc/{pid}/fd').iterdir())))
    return None


def run(name, injection=None, policy='abort', executable=None):
    logfile.unlink(missing_ok=True)
    trace = WORK / f'{name}.trace'
    command = ['strace', '-f', '-qq', '-o', str(trace)]
    if injection:
        command.append(f'-einject={injection}')
    proc = subprocess.Popen([*command, str(executable or WORK / policy)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            start_new_session=True)
    trace_text = lambda: trace.read_text() if trace.exists() else ''
    try:
        listen = wait_for(lambda: re.search(r'^(\d+)\s+listen\(.*\)\s+= 0', trace_text(), re.M))
        pid = int(listen[1])
        before = wait_for(lambda: idle(pid))
        seed = b'seed\x00\xc3\xa9'
        assert request(seed) == response(seed), 'seed response mismatch'
        expected = logged(seed) + logged(seed, b'B:')
        assert logfile.read_bytes() == expected, 'seed log mismatch'
        wait_for(lambda: idle(pid))
        assert '(INJECTED)' not in trace_text(), 'failure preceded intended request'
        body = (b'\xff\x00x' * 1300)
        actual = request(body)
        if injection:
            assert '(INJECTED)' in trace_text(), 'failure missed intended request'
        if name.startswith('abort-'):
            stdout, stderr = proc.communicate(timeout=5)
            assert proc.returncode == 1 and stdout == stderr == b''
            assert actual == b'' and logfile.read_bytes() == expected
            result = dict(case=name, exit_status=1)
        else:
            if name.startswith('drop-'):
                expected += logged(body, b'B:')
            elif name != 'receive-failure':
                expected += logged(body) + logged(body, b'B:')
            if name in ['header-failure', 'body-failure', 'receive-failure']:
                assert actual != response(body) and response(body).startswith(actual)
            else:
                assert actual == response(body), 'response changed after log consumption'
            assert logfile.read_bytes() == expected, 'log bytes/order mismatch'
            wait_for(lambda: idle(pid))
            for body in [b'', b'again\x00\xff']:
                assert request(body) == response(body)
                expected += logged(body) + logged(body, b'B:')
                assert logfile.read_bytes() == expected
            after = wait_for(lambda: idle(pid))
            assert before == after, (before, after)
            os.killpg(proc.pid, signal.SIGTERM)
            stdout, stderr = proc.communicate(timeout=5)
            assert proc.returncode == -signal.SIGTERM and stdout == stderr == b''
            result = dict(case=name, stable_stack=True, stable_descriptors=True)
        assert not re.search(r'\b(mmap|brk|mremap)\(', trace_text()), 'allocation syscall'
        return dict(**result, allocation_syscalls=0, trace=str(trace))
    finally:
        if proc.poll() is None:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate(timeout=5)


cases = [
    ('success', None, 'abort'),
    ('drop-open', 'open:error=EACCES:when=3', 'drop'),
    ('abort-open', 'open:error=EACCES:when=3', 'abort'),
    ('drop-write', 'write:error=EIO:when=3', 'drop'),
    ('abort-write', 'write:error=EIO:when=3', 'abort'),
    ('header-failure', 'sendto:error=EPIPE:when=7', 'abort'),
    ('body-failure', 'sendto:error=EPIPE:when=12', 'abort'),
    ('receive-failure', 'recvfrom:error=EIO:when=2', 'abort'),
]
reports = []
for case in cases:
    reports.append(run(*case))
    print(f'{case[0]}: passed', flush=True)
try:
    run('negative-control', executable=broken)
except AssertionError as error:
    reports.append(dict(case='negative-control', caught=str(error)))
    print(f'negative control caught: {error}', flush=True)
else:
    raise AssertionError('early frame release was not detected')
(WORK / 'report.json').write_text(json.dumps(reports, indent=2) + '\n')
