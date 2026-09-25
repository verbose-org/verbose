"""HTTP service storage contracts; run after cargo build (or set VERBOSEC)."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPILER = Path(os.environ.get("VERBOSEC", ROOT / "target/debug/verbosec")).resolve()


class HTTPStackCLI(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="verbose-http-stack-cli-")
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        self.source = self.base / "http_stack.verbose"
        self.original = (ROOT / "examples/http_stack.verbose").read_text()
        self.source.write_text(self.original)
        (self.base / "http_stack.intent").write_bytes((ROOT / "examples/http_stack.intent").read_bytes())

    def compile(self, *args):
        return subprocess.run([str(COMPILER), str(self.source), *map(str, args)],
                              capture_output=True, timeout=30)

    def report(self):
        out = self.compile("--stack-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        return json.loads(out.stdout)

    def test_report_scope_formula_and_default_selection(self):
        report = self.report()
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(report["entry_mode"], "http_1_0")
        self.assertEqual(report["scope"], "additional_service_stack_per_process")
        self.assertEqual(report["service"], "bounded_http")
        self.assertEqual(report["declared_bytes"], 8192)
        self.assertEqual(report["stack_bound_bytes"], 4720)
        self.assertEqual(report["frame_bytes"], 4096 + 64 + 128 + 40)
        handler = report["handler_frame"]
        self.assertEqual(handler["frame_bytes"], handler["slot_bytes"] + handler["buffer_bytes"])
        self.assertEqual(report["stack_bound_bytes"], 8 + report["frame_bytes"] + max(
            report["startup_stack_bytes"], handler["frame_bytes"] + handler["saved_register_bytes"]
            + max(report["expression_stack_bytes"], report["response_stack_bytes"])))
        self.assertEqual(report, json.loads(self.compile("--stack-report", "--run", "bounded_http", "--json").stdout))
        out = self.compile("--stack-report")
        self.assertIn(b"per process: 4720 bytes", out.stdout)
        self.assertIn(b"not total RSS", out.stdout)
        # Selecting an ordinary reusable rule retains its standalone argv scope.
        out = self.compile("--stack-report", "--run", "describe", "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(json.loads(out.stdout)["entry_mode"], "argv")

    def test_dispatch_bookkeeping_and_sufficient_declaration_are_byte_neutral(self):
        original_mode = "  concurrency: pooled\n  workers: 2\n"
        for mode, dispatch in [("", 0), ("  concurrency: forked\n", 0),
                               ("  concurrency: forked\n  max_connections: 4\n", 24),
                               (original_mode, 40)]:
            with self.subTest(mode=mode):
                source = self.original.replace(original_mode, mode)
                self.source.write_text(source)
                report = self.report()
                self.assertEqual(report["dispatch_bookkeeping_bytes"], dispatch)
                self.assertEqual(report["stack_bound_bytes"], 4680 + dispatch)
                artifacts = []
                for declaration in [f"  native_stack: {report['stack_bound_bytes']}\n", "", "  native_stack: 8192\n"]:
                    self.source.write_text(source.replace("  native_stack: 8192\n", declaration))
                    path = self.base / "server"
                    out = self.compile("--native", path)
                    self.assertEqual(out.returncode, 0, out.stderr)
                    artifacts.append(path.read_bytes())
                self.assertEqual(artifacts[0], artifacts[1])
                self.assertEqual(artifacts[0], artifacts[2])
                self.source.write_text(source.replace("  native_stack: 8192\n", ""))
                self.assertIsNone(self.report()["declared_bytes"])

    def test_unselected_ceiling_fails_before_artifact_input_and_report(self):
        source = self.original + self.original[self.original.index("service bounded_http"):].replace(
            "service bounded_http", "service unused").replace("native_stack: 8192", "native_stack: 4719")
        self.source.write_text(source)
        target = self.base / "artifact"
        target.write_bytes(b"preserve")
        for args in [("--native", target), ("--stack-report", "--json", "--run", "describe"),
                     ("--run", "describe", "--input", self.base / "missing.json")]:
            out = self.compile(*args)
            self.assertNotEqual(out.returncode, 0)
            self.assertIn(b"service 'unused' / native_stack", out.stderr)
            self.assertIn(b"4720 bytes per process exceeds declared 4719", out.stderr)
            self.assertEqual(out.stdout, b"")
            self.assertEqual(target.read_bytes(), b"preserve")

    def test_excluded_contexts_and_unsafe_computation_refuse(self):
        cases = [
            (self.original.replace("  response_timeout: 1\n", ""), b"both request_timeout"),
            (self.original + "  shutdown_timeout: 1\n", b"shutdown_timeout"),
            (self.original + '  state:\n    count : number = 0\n', b"without state or after mutations"),
            (self.original + '  log:\n    append_file "/tmp/unused-stack.log" concat(length(resp.body))\n    on_error: drop\n', b"concat only"),
            (self.original.replace("    purity:\n", "    native_stack: 8192\n    purity:\n", 1), b"not service or reaction contexts"),
            (self.original.replace("view.count * 2", "view.count / 0"), b"divisor range [0, 0] includes zero"),
        ]
        target = self.base / "artifact"
        target.write_bytes(b"preserve")
        for source, message in cases:
            with self.subTest(message=message):
                self.source.write_text(source)
                out = self.compile("--native", target)
                self.assertNotEqual(out.returncode, 0)
                self.assertIn(message, out.stderr)
                self.assertEqual(target.read_bytes(), b"preserve")

    def test_legacy_handler_report_refuses_instead_of_assuming_argv_layout(self):
        for name in ["hello_http.verbose", "hello_http.intent"]:
            (self.base / name).write_bytes((ROOT / "examples" / name).read_bytes())
        self.source = self.base / "hello_http.verbose"
        out = self.compile("--stack-report", "--json")
        self.assertNotEqual(out.returncode, 0)
        self.assertEqual(out.stdout, b"")
        self.assertIn(b"service native_stack requires", out.stderr)

    def test_wasm_refuses_service_contract_before_artifact(self):
        target = self.base / "output.wasm"
        target.write_bytes(b"preserve")
        out = self.compile("--wasm", target, "--run", "describe")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn(b"WASM does not support service native_stack", out.stderr)
        self.assertEqual(target.read_bytes(), b"preserve")

    def test_log_report_counts_emitter_reservation_and_composes_by_maximum(self):
        for name in ["http_log_stack.verbose", "http_log_stack.intent"]:
            (self.base / name).write_bytes((ROOT / "examples" / name).read_bytes())
        self.source = self.base / "http_log_stack.verbose"
        original = self.source.read_text()
        report = self.report()
        self.assertEqual(report["stack_bound_bytes"], 5584)
        self.assertEqual(report["request_metadata_bytes"], 72)
        self.assertEqual(report["log_stack_bytes"], 880)
        logs = report["logs"]
        self.assertEqual([entry["index"] for entry in logs], [0, 1])
        self.assertEqual([entry["on_error"] for entry in logs], ["abort", "drop"])
        self.assertEqual([entry["content_capacity_bytes"] for entry in logs], [586, 287])
        self.assertEqual([entry["buffer_bytes"] for entry in logs], [856, 288])
        self.assertEqual([entry["stack_bound_bytes"] for entry in logs], [880, 288])
        self.assertIn(b"sequential logs: 880 bytes maximum", self.compile("--stack-report").stdout)
        artifacts = []
        for limit in ["native_stack: 5584", "native_stack: 8192", ""]:
            self.source.write_text(original.replace("native_stack: 8192", limit))
            target = self.base / "logged-server"
            out = self.compile("--native", target)
            self.assertEqual(out.returncode, 0, out.stderr)
            artifacts.append(target.read_bytes())
        self.assertEqual(artifacts[0], artifacts[1])
        self.assertEqual(artifacts[0], artifacts[2])
        self.source.write_text(original.replace("native_stack: 8192", "native_stack: 5583"))
        out = self.compile("--native", target)
        self.assertNotEqual(out.returncode, 0)
        self.assertIn(b"5584 bytes per process exceeds declared 5583", out.stderr)
        self.assertEqual(target.read_bytes(), artifacts[0])

    def test_log_ceiling_checks_unselected_services_and_refuses_wasm(self):
        logs = '\n  log:\n    append_file "/tmp/unused-stack.log" concat(resp.body)\n    on_error: drop\n'
        unselected = self.original[self.original.index("service bounded_http"):].replace(
            "service bounded_http", "service unused").replace("native_stack: 8192", "native_stack: 4720")
        self.source.write_text(self.original + unselected + logs)
        out = self.compile("--stack-report", "--run", "describe", "--json")
        self.assertNotEqual(out.returncode, 0)
        self.assertEqual(out.stdout, b"")
        self.assertIn(b"service 'unused' / native_stack", out.stderr)
        self.assertIn(b"exceeds declared 4720", out.stderr)
        self.source.write_text(self.original + logs)
        target = self.base / "logged.wasm"
        target.write_bytes(b"preserve")
        out = self.compile("--wasm", target, "--run", "describe")
        self.assertNotEqual(out.returncode, 0)
        self.assertIn(b"WASM does not support service native_stack", out.stderr)
        self.assertEqual(target.read_bytes(), b"preserve")


if __name__ == "__main__":
    unittest.main()
