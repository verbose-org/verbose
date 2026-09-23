"""CLI/resource-contract checks. Run after cargo build; VERBOSEC can override it."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPILER = Path(os.environ.get("VERBOSEC", ROOT / "target/debug/verbosec")).resolve()


class StackCLIBase(unittest.TestCase):
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


class NativeStackCLI(StackCLIBase):
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
        for name in ["missing", "magnitude,missing"]:
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


class TextStackCLI(StackCLIBase):
    def setUp(self):
        super().setUp()
        self.source = self.base / "text_stack.verbose"
        self.original = (ROOT / "examples/text_stack.verbose").read_text()
        self.source.write_text(self.original)
        (self.base / "text_stack.intent").write_bytes(
            (ROOT / "examples/text_stack.intent").read_bytes())

    def test_report_json_is_standalone_scoped_and_deterministic(self):
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stderr, b"")
        report = json.loads(out.stdout)
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(report["scope"], "additional_entry_stack")
        self.assertEqual(report["rule"], "repeat_reading")
        self.assertEqual(report["declared_bytes"], 384)
        self.assertEqual(report["stack_bound_bytes"], 336)
        self.assertEqual(report["frame_bytes"], 56)
        self.assertEqual(report["text_frame"], dict(frame_bytes=232, slot_bytes=104,
                                                  buffer_bytes=128, saved_register_bytes=16,
                                                  calls=[dict(call=n, callee="format_reading", parent_call=None,
                                                              live_caller_buffer_capacity_bytes=capacity,
                                                              retained_caller_buffer_capacity_bytes=capacity)
                                                         for n, capacity in [(1, 64), (2, 96)]]))
        self.assertEqual(report["expression_stack_bytes"], 24)
        self.assertEqual(report["input_stack_bytes"], 8)
        self.assertEqual(report["output_stack_bytes"], 0)
        self.assertEqual(out.stdout, self.run_compiler("--stack-report", "--json").stdout)
        human = self.run_compiler("--stack-report")
        self.assertEqual(human.returncode, 0, human.stderr)
        self.assertIn(b"nested text frame: 232 bytes", human.stdout)
        self.assertIn(b"declared limit: 384 bytes (verified)", human.stdout)

    def test_annotation_does_not_change_code_or_extreme_values(self):
        binary = self.base / "annotated"
        out = self.run_compiler("--native", binary)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.source.write_text(self.original.replace("    native_stack: 208\n", "")
                               .replace("    native_stack: 384\n", ""))
        control = self.base / "control"
        self.assertEqual(self.run_compiler("--native", control).returncode, 0)
        self.assertEqual(binary.read_bytes(), control.read_bytes())
        report = json.loads(self.run_compiler("--stack-report", "--json").stdout)
        self.assertIsNone(report["declared_bytes"])
        values = [{"title": title, "code": code} for title in ["", "abcdefgh", "éééé"]
                  for code in [-2**63, -1, 0, 1, 2**63 - 1]]
        expected = [f"[{v['title']}]{v['code']} | [{v['title']}]{v['code']}" for v in values]
        args = [str(x) for v in values for x in [v["title"], v["code"]]]
        output = subprocess.run([str(binary), *args], capture_output=True, timeout=10)
        self.assertEqual(output.returncode, 0)
        self.assertEqual(output.stderr, b"")
        self.assertEqual(output.stdout.decode(), "".join(f"{s}\n" for s in expected))
        data = self.base / "input.json"
        data.write_text(json.dumps(values, ensure_ascii=False))
        self.source.write_text(self.original)
        interpreted = self.run_compiler("--run", "repeat_reading", "--input", data, "--json")
        self.assertEqual(interpreted.returncode, 0, interpreted.stderr)
        self.assertEqual(json.loads(interpreted.stdout), [{"out": s} for s in expected])
        for args in [["ééééé", "0"], ["x"], ["ok", "1", "unfinished"]]:
            a = subprocess.run([str(binary), *args], capture_output=True, timeout=10)
            b = subprocess.run([str(control), *args], capture_output=True, timeout=10)
            self.assertEqual((a.returncode, a.stdout, a.stderr), (b.returncode, b.stdout, b.stderr))
            self.assertNotEqual(a.returncode, 0)

    def test_too_small_budget_fails_verification_report_and_emission(self):
        self.source.write_text(self.original.replace("native_stack: 384", "native_stack: 335"))
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve this artifact")
        for args in [[], ["--stack-report", "--json"], ["--native", artifact], ["--wasm", artifact]]:
            out = self.run_compiler(*args)
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"336 bytes exceeds declared 335 bytes", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve this artifact")
        self.source.write_text(self.original.replace("native_stack: 208", "native_stack: 207"))
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"208 bytes exceeds declared 207 bytes", out.stderr)

    def test_unknown_entries_and_unsupported_modes_never_report_success(self):
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve this artifact")
        # Only the helper declares a budget; its unannotated caller must still
        # refuse wrappers whose storage is not covered by that argv contract.
        self.source.write_text(self.original.replace("    native_stack: 384\n", ""))
        for args in [["--native", artifact, "--stdin"], ["--native", artifact, "--stream"],
                     ["--native", artifact, "--stdin-raw"],
                     ["--native", artifact, "--run", "repeat_reading,format_reading", "--stdin"],
                     ["--wasm", artifact]]:
            out = self.run_compiler(*args)
            self.assertNotEqual(out.returncode, 0, args)
            self.assertIn(b"native_stack", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve this artifact")


class SequentialStackCLI(StackCLIBase):
    def setUp(self):
        super().setUp()
        self.source = self.base / "sequential_stack.verbose"
        self.original = (ROOT / "examples/sequential_stack.verbose").read_text()
        self.source.write_text(self.original)
        (self.base / "sequential_stack.intent").write_bytes(
            (ROOT / "examples/sequential_stack.intent").read_bytes())

    def test_composed_report_and_ordered_outputs(self):
        names = "clamp,nonnegative,label"
        out = self.run_compiler("--stack-report", "--run", names, "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(out.stderr, b"")
        report = json.loads(out.stdout)
        self.assertEqual(report["composition"], "sequential")
        self.assertEqual(report["on_phase_failure"], "stop")
        self.assertEqual(report["retained_stack_bytes"], 0)
        self.assertEqual(report["stack_bound_bytes"], 192)
        self.assertEqual([p["rule"] for p in report["phases"]], names.split(","))
        self.assertEqual([p["stack_bound_bytes"] for p in report["phases"]], [104, 88, 192])
        for phase in report["phases"]:
            single = self.run_compiler("--stack-report", "--run", phase["rule"], "--json")
            self.assertEqual(phase, json.loads(single.stdout))
        self.assertEqual(out.stdout, self.run_compiler("--stack-report", "--run", names, "--json").stdout)
        human = self.run_compiler("--stack-report", "--run", names)
        self.assertIn(b"192 bytes (maximum of phases)", human.stdout)
        binary = self.base / "sequence"
        out = self.run_compiler("--native", binary, "--run", names)
        self.assertEqual(out.returncode, 0, out.stderr)
        values = [{"title": "a", "value": 2}, {"title": "éééé", "value": 2**63-1}]
        data = self.base / "input.json"
        data.write_text(json.dumps(values, ensure_ascii=False))
        expected = []
        for name in names.split(","):
            result = self.run_compiler("--run", name, "--input", data, "--json")
            self.assertEqual(result.returncode, 0, result.stderr)
            for v in json.loads(result.stdout):
                value = v["out"]
                expected.append(str(value).lower() if isinstance(value, bool) else str(value))
        args = [str(x) for v in values for x in [v["title"], v["value"]]]
        actual = subprocess.run([str(binary), *args], capture_output=True, timeout=10)
        self.assertEqual(actual.returncode, 0)
        self.assertEqual(actual.stderr, b"")
        self.assertEqual(actual.stdout.decode(), "".join(x + "\n" for x in expected))
        failed = subprocess.run([str(binary), "x", "2", "y", "-1", "z", "3"], capture_output=True, timeout=10)
        self.assertEqual((failed.returncode, failed.stdout, failed.stderr), (1, b"2\n-1\n3\ntrue\nfalse\ntrue\n", b""))
        for n in [104, 88, 192]:
            self.original = self.original.replace(f"    native_stack: {n}\n", "")
        self.source.write_text(self.original)
        control = self.base / "control"
        self.assertEqual(self.run_compiler("--native", control, "--run", names).returncode, 0)
        self.assertEqual(binary.read_bytes(), control.read_bytes())

    def test_unknown_phase_and_late_budget_failure_preserve_artifacts(self):
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve this artifact")
        for names in ["clamp,missing", "clamp,", "label," * 64 + "label"]:
            for args in [["--stack-report", "--json"], ["--native", artifact]]:
                out = self.run_compiler(*args, "--run", names)
                self.assertNotEqual(out.returncode, 0)
                if "--stack-report" in args:
                    self.assertEqual(out.stdout, b"")
                self.assertEqual(artifact.read_bytes(), b"preserve this artifact")
        self.source.write_text(self.original.replace("native_stack: 192", "native_stack: 191"))
        for args in [["--stack-report", "--json"], ["--native", artifact]]:
            out = self.run_compiler(*args, "--run", "clamp,label")
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"192 bytes exceeds declared 191 bytes", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve this artifact")


class RetainedStackCLI(StackCLIBase):
    def setUp(self):
        super().setUp()
        self.source = self.base / "retained_stack.verbose"
        self.original = (ROOT / "examples/retained_stack.verbose").read_text()
        self.source.write_text(self.original)
        (self.base / "retained_stack.intent").write_bytes(
            (ROOT / "examples/retained_stack.intent").read_bytes())

    def test_call_report_and_retained_values_match_the_interpreter(self):
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        report = json.loads(out.stdout)
        self.assertEqual(report["stack_bound_bytes"], 288)
        self.assertEqual(report["declared_bytes"], 408)
        self.assertEqual(report["text_frame"]["buffer_bytes"], 96)
        calls = report["text_frame"]["calls"]
        self.assertEqual([c["call"] for c in calls], [1, 2, 3])
        self.assertEqual([c["callee"] for c in calls], ["prepare", "forward", "render"])
        self.assertEqual([c["parent_call"] for c in calls], [None] * 3)
        self.assertEqual([c["live_caller_buffer_capacity_bytes"] for c in calls], [48, 64, 64])
        self.assertEqual([c["retained_caller_buffer_capacity_bytes"] for c in calls], [48, 64, 64])
        self.assertEqual(out.stdout, self.run_compiler("--stack-report", "--json").stdout)
        human = self.run_compiler("--stack-report")
        self.assertIn(b"64 bytes live at entry, 64 retained through return", human.stdout)
        self.assertIn(b"included in the frame, not additive", human.stdout)
        binary = self.base / "analyze"
        out = self.run_compiler("--native", binary)
        self.assertEqual(out.returncode, 0, out.stderr)
        values = [{"title": title, "code": code} for title in ["", "abcdefgh", "éééé"]
                  for code in [-2**63, -1, 0, 1, 2**63 - 1]]
        expected = [f"[{v['title']}]:{1 if v['code'] > 0 else -1} | [{v['title']}]" for v in values]
        data = self.base / "input.json"
        data.write_text(json.dumps(values, ensure_ascii=False))
        interpreted = self.run_compiler("--run", "analyze", "--input", data, "--json")
        self.assertEqual(interpreted.returncode, 0, interpreted.stderr)
        self.assertEqual(json.loads(interpreted.stdout), [{"out": v} for v in expected])
        args = [str(x) for v in values for x in [v["title"], v["code"]]]
        actual = subprocess.run([str(binary), *args], capture_output=True, timeout=10)
        self.assertEqual((actual.returncode, actual.stderr), (0, b""))
        self.assertEqual(actual.stdout.decode(), "".join(v + "\n" for v in expected))
        self.source.write_text(self.original.replace("    native_stack: 408\n", ""))
        control = self.base / "control"
        self.assertEqual(self.run_compiler("--native", control).returncode, 0)
        self.assertEqual(binary.read_bytes(), control.read_bytes())

    def test_budget_and_transferred_field_bounds_refuse_before_artifact_creation(self):
        artifact = self.base / "existing"
        artifact.write_bytes(b"existing")
        for source, message in [
            (self.original.replace("native_stack: 408", "native_stack: 287"), b"288 bytes exceeds declared 287"),
            (self.original.replace("title : text [..10]", "title : text [..9]"), b"call 'forward' input field 'title'"),
            (self.original.replace("code : number [-1, 1]", "code : number [0, 1]"), b"call 'forward' input field 'code'"),
        ]:
            self.source.write_text(source)
            for args in [[], ["--stack-report", "--json"], ["--native", artifact]]:
                out = self.run_compiler(*args)
                self.assertEqual(out.returncode, 1, out.stderr)
                self.assertEqual(out.stdout, b"")
                self.assertIn(message, out.stderr)
                self.assertEqual(artifact.read_bytes(), b"existing")
        self.source.write_text(self.original)
        for args in [["--native", artifact, "--stdin"], ["--wasm", artifact]]:
            out = self.run_compiler(*args)
            self.assertEqual(out.returncode, 1)
            self.assertIn(b"native_stack", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"existing")


if __name__ == "__main__":
    unittest.main()
