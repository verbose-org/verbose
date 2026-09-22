"""CLI/resource-contract checks. Run after cargo build; VERBOSEC can override it."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPILER = Path(os.environ.get("VERBOSEC", ROOT / "target/debug/verbosec")).resolve()


class NativeStackCLI(unittest.TestCase):
    def setUp(self):
        self.assertTrue(COMPILER.is_file(), "run cargo build first")
        self.directory = tempfile.TemporaryDirectory(prefix="verbose-stack-cli-")
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        self.source = self.base / "native_stack.verbose"
        self.original = (ROOT / "examples/native_stack.verbose").read_text()
        self.source.write_text(self.original)
        (self.base / "native_stack.intent").write_bytes(
            (ROOT / "examples/native_stack.intent").read_bytes())

    def run_compiler(self, *args):
        return subprocess.run([str(COMPILER), str(self.source), *map(str, args)],
                              capture_output=True, timeout=30)

    def test_report_json_is_standalone_scoped_and_deterministic(self):
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stderr, b"")
        report = json.loads(out.stdout)
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(report["rule"], "magnitude")
        self.assertEqual(report["target"], "x86_64-linux")
        self.assertEqual(report["entry_mode"], "argv")
        self.assertEqual(report["scope"], "additional_entry_stack")
        self.assertEqual(report["declared_bytes"], 112)
        self.assertEqual(report["stack_bound_bytes"], 96)
        self.assertEqual(report["frame_bytes"], 64)
        self.assertEqual(out.stdout, self.run_compiler("--stack-report", "--json").stdout)
        human = self.run_compiler("--stack-report", "--run", "clamp")
        self.assertEqual(human.returncode, 0, human.stderr)
        self.assertIn(b"declared limit: 96 bytes (verified)", human.stdout)
        self.assertIn(b"excludes initial argv/environment", human.stdout)

    def test_annotation_does_not_change_code_or_extreme_values(self):
        binary = self.base / "annotated"
        out = self.run_compiler("--native", binary, "--run", "magnitude")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.source.write_text(self.original.replace("    native_stack: 96\n", "")
                               .replace("    native_stack: 112\n", ""))
        control = self.base / "control"
        self.assertEqual(self.run_compiler("--native", control, "--run", "magnitude").returncode, 0)
        self.assertEqual(binary.read_bytes(), control.read_bytes())
        report = json.loads(self.run_compiler("--stack-report", "--json").stdout)
        self.assertIsNone(report["declared_bytes"])
        values = [-2**63, -2**63 + 1, -101, -100, -1, 0, 1, 37, 100, 101, 2**63 - 1]
        expected = [abs(min(max(value, -100), 100)) for value in values]
        output = subprocess.run([str(binary), *map(str, values)], capture_output=True, timeout=10)
        self.assertEqual(output.returncode, 0)
        self.assertEqual(output.stderr, b"")
        self.assertEqual(output.stdout.decode(), "".join(f"{n}\n" for n in expected))
        data = self.base / "input.json"
        data.write_text(json.dumps([{"value": value} for value in values]))
        self.source.write_text(self.original)
        interpreted = self.run_compiler("--run", "magnitude", "--input", data, "--json")
        self.assertEqual(interpreted.returncode, 0, interpreted.stderr)
        self.assertEqual(json.loads(interpreted.stdout), [{"out": n} for n in expected])

    def test_too_small_budget_fails_verification_report_and_emission(self):
        self.source.write_text(self.original.replace("native_stack: 112", "native_stack: 95"))
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve this artifact")
        for args in [[], ["--stack-report", "--json"], ["--native", artifact],
                     ["--wasm", artifact]]:
            out = self.run_compiler(*args)
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"96 bytes exceeds declared 95 bytes", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve this artifact")
        # Equal is accepted; declarations are independently enforced even when
        # another rule is selected as entry.
        self.source.write_text(self.original.replace("native_stack: 112", "native_stack: 96"))
        self.assertEqual(self.run_compiler("--stack-report", "--json").returncode, 0)
        self.source.write_text(self.original.replace("native_stack: 96", "native_stack: 1"))
        self.assertNotEqual(self.run_compiler("--stack-report", "--run", "magnitude").returncode, 0)

    def test_unknown_entries_and_unsupported_modes_never_report_success(self):
        for name in ["missing", "magnitude,clamp"]:
            out = self.run_compiler("--stack-report", "--run", name, "--json")
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"no rule named", out.stderr)
        for flag in ["--native", "--wasm", "--input", "--stdin", "--stdin-raw",
                     "--stream", "--benchmark", "--stats", "--disasm", "--http-server",
                     "--echo-server", "--demo-http"]:
            out = self.run_compiler("--stack-report", "--json", flag)
            self.assertEqual(out.returncode, 2, flag)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"cannot combine", out.stderr)
        artifact = self.base / "wasm"
        artifact.write_bytes(b"old wasm")
        out = self.run_compiler("--wasm", artifact)
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"WASM does not support proofs.native_stack", out.stderr)
        self.assertEqual(artifact.read_bytes(), b"old wasm")


if __name__ == "__main__":
    unittest.main()
