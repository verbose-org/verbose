"""Exercise the benchmark oracle against controlled correct/broken peers.

python3 tools/test_http_load.py -v (requires rustc and loopback sockets).
"""
from contextlib import contextmanager
from pathlib import Path
import socketserver
import subprocess
import tempfile
import threading
import time
import unittest

from benchmark_http import ROOT, check_load, load


@contextmanager
def peer(behavior):
    class Handler(socketserver.StreamRequestHandler):
        def handle(self):
            self.connection.settimeout(2)
            self.rfile.readline()
            size = 0
            while (line := self.rfile.readline()) != b'\r\n':
                if not line:
                    return
                if line.startswith(b'Content-Length: '):
                    size = int(line.split(b':')[1])
            body = self.rfile.read(size)
            reply = f'HTTP/1.0 200 OK\r\nContent-Length: {size}\r\n\r\n'.encode() + body
            if behavior == 'wrong':
                reply = reply[:-1] + bytes([reply[-1] ^ 1])
            elif behavior == 'extra':
                reply += b'x'
            elif behavior == 'short':
                reply = reply[:-1]
            elif behavior == 'silent':
                time.sleep(.2)
                return
            self.wfile.write(reply)
            self.wfile.flush()
            if behavior == 'no-eof':
                time.sleep(.2)

    with socketserver.ThreadingTCPServer(('127.0.0.1', 0), Handler) as server:
        server.daemon_threads = True
        thread = threading.Thread(target=server.serve_forever, kwargs={'poll_interval': .01})
        thread.start()
        try:
            yield server.server_address[1]
        finally:
            server.shutdown()
            thread.join()


class LoadOracle(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.work = tempfile.TemporaryDirectory(prefix='verbose-load-test-')
        cls.client = Path(cls.work.name) / 'client'
        subprocess.run(['rustc', '--edition=2021', '-O', str(ROOT / 'tools/http_load.rs'), '-o', str(cls.client)], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.work.cleanup()

    def test_binary_echo_and_uneven_request_distribution(self):
        with peer('correct') as port:
            for size in (0, 1024, 3900):
                report = load(self.client, port, 3, 11, size, None)
                check_load(report)
                self.assertEqual(report['attempted'], 11)
                self.assertEqual(report['successful'], 11)
                self.assertGreater(report['success_latency_us']['p99'], 0)

    def test_wrong_truncated_and_trailing_bytes_are_failures(self):
        for behavior in ('wrong', 'extra', 'short'):
            with self.subTest(behavior=behavior), peer(behavior) as port:
                report = load(self.client, port, 1, 3, 1024, None)
                self.assertEqual(report['client_exit_status'], 1)
                self.assertEqual(report['errors']['response'], 3)
                self.assertEqual(report['goodput_per_second'], 0)
                self.assertIsNone(report['success_latency_us']['p50'])
                with self.assertRaises(RuntimeError):
                    check_load(report)

    def test_timeout_and_missing_eof_are_failures(self):
        for behavior in ('silent', 'no-eof'):
            with self.subTest(behavior=behavior), peer(behavior) as port:
                report = load(self.client, port, 1, 1, 0, None, timeout_ms=50)
                self.assertEqual(report['errors']['read'], 1)
                with self.assertRaises(RuntimeError):
                    check_load(report)

    def test_invalid_arguments_do_not_start_a_load(self):
        result = subprocess.run([str(self.client), '1', '0', '10', '0', '50'], capture_output=True)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, b'')

    def test_overall_deadline_kills_a_stuck_client(self):
        with peer('silent') as port:
            with self.assertRaises(subprocess.TimeoutExpired):
                load(self.client, port, 1, 1, 0, None, wall_timeout=.05)


if __name__ == '__main__':
    unittest.main()
