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


class SourceExecutionCLI(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="verbose-execution-cli-")
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        self.source = self.base / "execution_stack.verbose"
        for name in ["execution_stack.verbose", "execution_stack.intent",
                     "sequential_stack.verbose", "sequential_stack.intent"]:
            (self.base / name).write_bytes((ROOT / "examples" / name).read_bytes())
        self.original = self.source.read_text()

    def run_compiler(self, *args):
        return subprocess.run([str(COMPILER), str(self.source), *map(str, args)],
                              capture_output=True, timeout=30)

    def test_default_selection_report_and_binary_match_explicit_order(self):
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        report = json.loads(out.stdout)
        self.assertEqual(report["execution"], "inspect_readings")
        self.assertEqual(report["input_concept"], "Reading")
        self.assertEqual(report["declared_bytes"], 192)
        self.assertEqual(report["stack_bound_bytes"], 192)
        self.assertEqual(report["composition"], "sequential")
        self.assertEqual(report["on_phase_failure"], "stop")
        self.assertEqual([p["rule"] for p in report["phases"]], ["clamp", "nonnegative", "label"])
        self.assertEqual(out.stdout, self.run_compiler("--stack-report", "--json", "--run", "inspect_readings").stdout)
        paths = [self.base / n for n in ["default", "named", "phases"]]
        for path, selection in zip(paths, [[], ["--run", "inspect_readings"], ["--run", "clamp,nonnegative,label"]]):
            out = self.run_compiler("--native", path, *selection)
            self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(paths[0].read_bytes(), paths[1].read_bytes())
        self.assertEqual(paths[0].read_bytes(), paths[2].read_bytes())
        for args, status, expected in [
            (["a", "2", "b", "1000"], 0, b"2\n100\ntrue\ntrue\na:2\nb:1000\n"),
            (["x", "2", "y", "-1", "z", "3"], 1, b"2\n-1\n3\ntrue\nfalse\ntrue\n"),
        ]:
            out = subprocess.run([str(paths[0]), *args], capture_output=True, timeout=5)
            self.assertEqual((out.returncode, out.stdout, out.stderr), (status, expected, b""))

    def test_every_execution_budget_is_checked_before_success_or_artifact(self):
        bad = self.original.split("execution inspect_readings", 1)[1]
        self.source.write_text(self.original + "\nexecution unused" + bad.replace("native_stack: 192", "native_stack: 191"))
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve")
        for args in [[], ["--stack-report", "--json"], ["--native", artifact],
                     ["--native", artifact, "--run", "clamp"], ["--wasm", artifact]]:
            out = self.run_compiler(*args)
            self.assertEqual(out.returncode, 1)
            self.assertEqual(out.stdout, b"")
            self.assertIn(b"execution 'unused'", out.stderr)
            self.assertIn(b"192 bytes exceeds declared 191", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve")

    def test_unsupported_execution_modes_refuse_and_preserve_artifact(self):
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve")
        for flag in ["--stdin", "--stdin-raw", "--stream"]:
            out = self.run_compiler("--native", artifact, flag)
            self.assertEqual(out.returncode, 1)
            self.assertIn(b"execution entries support native argv", out.stderr)
        out = self.run_compiler("--wasm", artifact, "--run", "inspect_readings")
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"WASM does not support source execution", out.stderr)
        out = self.run_compiler("--run", "inspect_readings", "--input", "missing.json")
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"cannot read execution input", out.stderr)
        self.assertEqual(artifact.read_bytes(), b"preserve")

    def interpret(self, records, *flags):
        data = self.base / "input.json"
        data.write_text(json.dumps(records))
        return self.run_compiler("--run", "inspect_readings", "--input", data, *flags)

    def test_interpreted_execution_matches_native_output_and_failure_policy(self):
        binary = self.base / "native"
        self.assertEqual(self.run_compiler("--native", binary).returncode, 0)
        for rows in [[("a", 2), ("b", 1000)], [("x", 2), ("y", -1), ("z", 3)],
                     [("éééé", -2**63), ("🚀", 2**63 - 1)], [("a,{\"}\\\n", 0)], []]:
            records = [dict(title=s, value=n) for s, n in rows]
            interpreted = self.interpret(records)
            native = subprocess.run([str(binary), *[v for s, n in rows for v in [s, str(n)]]],
                                    capture_output=True, timeout=5)
            self.assertEqual((interpreted.returncode, interpreted.stdout), (native.returncode, native.stdout))
            if rows:
                self.assertEqual(interpreted.stderr, native.stderr)
            else:
                self.assertIn(b"at least one input record", interpreted.stderr)
        # Repeated phases retain separate indices and do not consume prior output.
        self.source.write_text(self.original.replace("[clamp, nonnegative, label]", "[label, clamp, label]"))
        out = self.interpret([dict(title="x", value=1000)], "--json")
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(json.loads(out.stdout), [
            dict(phase=1, rule="label", record=0, value="x:1000"),
            dict(phase=2, rule="clamp", record=0, value=100),
            dict(phase=3, rule="label", record=0, value="x:1000"),
        ])

    def test_interpreted_execution_json_preserves_phase_record_order_and_escapes(self):
        rows = [dict(title='a,{"}\\\n', value=0), dict(title="🚀", value=2**63 - 1)]
        out = self.interpret(rows, "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        expected = []
        for phase, rule, values in [(1, "clamp", [0, 100]), (2, "nonnegative", [True, True]),
                                    (3, "label", [f'{r["title"]}:{r["value"]}' for r in rows])]:
            expected.extend(dict(phase=phase, rule=rule, record=i, value=v) for i, v in enumerate(values))
        self.assertEqual(json.loads(out.stdout), expected)
        out = self.interpret([dict(title="x", value=-1), dict(title="y", value=2)], "--json")
        self.assertEqual((out.returncode, out.stderr), (1, b""))
        self.assertEqual([e["value"] for e in json.loads(out.stdout)], [-1, 2, False, True])
        self.assertEqual([e["phase"] for e in json.loads(out.stdout)], [1, 1, 2, 2])

    def test_interpreted_execution_input_errors_keep_completed_prefix(self):
        for bad in [dict(title="ééééé", value=0), dict(title="x", value="0"), dict(title="x")]:
            rows = [dict(title="ok", value=2), bad, dict(title="later", value=3)]
            out = self.interpret(rows)
            self.assertEqual((out.returncode, out.stdout), (1, b"2\n"))
            self.assertIn(b"phase 1 ('clamp'), record 1", out.stderr)
            out = self.interpret(rows, "--json")
            self.assertEqual(out.returncode, 1)
            self.assertEqual(json.loads(out.stdout), [dict(phase=1, rule="clamp", record=0, value=2)])
        # Numeric bounds are checked even before a phase that ignores the field.
        phase_source = self.base / "sequential_stack.verbose"
        phase_source.write_text(phase_source.read_text().replace("value : number\n", "value : number [-10, 10]\n"))
        out = self.interpret([dict(title="ok", value=2), dict(title="bad", value=11)])
        self.assertEqual((out.returncode, out.stdout), (1, b"2\n"))
        self.assertIn(b"record 1", out.stderr)
        binary = self.base / "native"
        self.assertEqual(self.run_compiler("--native", binary).returncode, 0)
        native = subprocess.run([str(binary), "ok", "2", "bad", "11"], capture_output=True, timeout=5)
        self.assertEqual((native.returncode, native.stdout), (out.returncode, out.stdout))
        out = self.interpret([dict(title="ok", value=2), dict(title="ééééé", value=0)])
        native = subprocess.run([str(binary), "ok", "2", "ééééé", "0"], capture_output=True, timeout=5)
        self.assertEqual((native.returncode, native.stdout), (out.returncode, out.stdout))

    def test_interpreted_execution_records_keep_typed_values_in_json(self):
        for name in ["retained_stack.verbose", "retained_stack.intent"]:
            (self.base / name).write_bytes((ROOT / "examples" / name).read_bytes())
        self.source.write_text(self.original.replace('"sequential_stack.verbose"', '"retained_stack.verbose"')
                               .replace("[clamp, nonnegative, label]", "[prepare, analyze]")
                               .replace("native_stack: 192", "native_stack: 4096"))
        out = self.interpret([dict(title="café", code=0), dict(title='"\\\n', code=2**63-1)], "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        events = json.loads(out.stdout)
        self.assertEqual(events[0], dict(phase=1, rule="prepare", record=0, value=dict(title="[café]", code=-1)))
        self.assertEqual(events[1]["value"], dict(title='["\\\n]', code=1))
        self.assertEqual(events[2]["value"], "[café]:-1 | [café]")
        self.assertEqual(events[3]["value"], '["\\\n]:1 | ["\\\n]')

    def test_interpreted_execution_rejects_malformed_json_before_output(self):
        data = self.base / "input.json"
        for text in ['[{"title":"x","value":1,}]', '[{"title":"x","value":1,"value":2}]',
                     '[{"title":"x","value":01}]', '[{"title":"x","value":9223372036854775808}]',
                     '[{"title":"x","value":1}] garbage', '[{"title":"\\ud800","value":1}]']:
            data.write_text(text)
            out = self.run_compiler("--run", "inspect_readings", "--input", data, "--json")
            self.assertEqual((out.returncode, out.stdout), (1, b""))
            self.assertIn(b"execution input JSON", out.stderr)

    def test_interpreted_execution_stdin_flags_and_legacy_rule_output(self):
        data = json.dumps([dict(title="café", value=2)]).encode()
        out = subprocess.run([str(COMPILER), str(self.source), "--run", "inspect_readings", "--stdin"],
                             input=data, capture_output=True, timeout=30)
        self.assertEqual((out.returncode, out.stdout, out.stderr), (0, "2\ntrue\ncafé:2\n".encode(), b""))
        for flags in [[], ["--stdin-raw"], ["--stream"], ["--benchmark"], ["--stats"],
                      ["--disasm"], ["--input", "missing.json", "--stdin"]]:
            out = self.run_compiler("--run", "inspect_readings", *flags)
            self.assertEqual((out.returncode, out.stdout), (2, b""), flags)
        path = self.base / "legacy.json"
        path.write_text('[{"title":"x","value":-1}]')
        out = self.run_compiler("--run", "nonnegative", "--input", path, "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        self.assertEqual(json.loads(out.stdout), [{"out": False}])

    def test_interpreted_execution_rejects_invalid_unselected_budget_before_input(self):
        bad = self.original.split("execution inspect_readings", 1)[1]
        self.source.write_text(self.original + "\nexecution unused" + bad.replace("native_stack: 192", "native_stack: 191"))
        out = self.run_compiler("--run", "inspect_readings", "--input", "missing.json", "--json")
        self.assertEqual((out.returncode, out.stdout), (1, b""))
        self.assertIn(b"execution 'unused'", out.stderr)
        self.assertNotIn(b"cannot read", out.stderr)

    def test_imported_execution_source_reference_is_resolved_and_checked(self):
        module = self.base / "module"
        module.mkdir()
        # Keep imported phases at the root: the existing resolver resolves use
        # paths from the entry module's base, not the nested module's location.
        (module / "entry.verbose").write_text(self.original)
        (module / "execution_stack.intent").write_text("One explicit source execution.\n")
        self.source.write_text('@verbose 0.1.0\nuse "module/entry.verbose"\n')
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        self.assertEqual(json.loads(out.stdout)["execution"], "inspect_readings")
        (module / "execution_stack.intent").unlink()
        out = self.run_compiler("--stack-report", "--json")
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"execution 'inspect_readings' / @source", out.stderr)


class ConcurrentExecutionCLI(unittest.TestCase):
    def setUp(self):
        SourceExecutionCLI.setUp(self)
        self.sequential = self.original
        self.original = self.original.replace("mode: sequential", "mode: concurrent").replace(
            "native_stack: 192", "max_in_flight: 2")
        self.source.write_text(self.original)

    run_compiler = SourceExecutionCLI.run_compiler
    interpret = SourceExecutionCLI.interpret

    def test_concurrent_values_status_and_json_match_sequential_reference(self):
        binary = self.base / "sequential"
        self.source.write_text(self.sequential)
        self.assertEqual(self.run_compiler("--native", binary).returncode, 0)
        rows = [dict(title="café", value=2), dict(title="x", value=1000)]
        expected = self.interpret(rows, "--json")
        for limit in [1, 2, 64]:
            self.source.write_text(self.original.replace("max_in_flight: 2", f"max_in_flight: {limit}"))
            for _ in range(3):
                out = self.interpret(rows, "--json")
                self.assertEqual((out.returncode, out.stdout, out.stderr),
                                 (expected.returncode, expected.stdout, expected.stderr))
            for batch in [rows, [dict(title="éééé", value=-2**63), dict(title="🚀", value=2**63-1)],
                          [dict(title="x", value=2), dict(title="y", value=-1), dict(title="z", value=3)]]:
                out = self.interpret(batch)
                native = subprocess.run([str(binary), *[arg for r in batch for arg in [r["title"], str(r["value"])]]],
                                        capture_output=True, timeout=5)
                self.assertEqual((out.returncode, out.stdout, out.stderr),
                                 (native.returncode, native.stdout, native.stderr))
        data = json.dumps(rows).encode()
        out = subprocess.run([str(COMPILER), str(self.source), "--run", "inspect_readings", "--stdin", "--json"],
                             input=data, capture_output=True, timeout=30)
        self.assertEqual((out.returncode, out.stdout, out.stderr), (0, expected.stdout, b""))

    def test_concurrent_errors_publish_only_the_ordered_prefix(self):
        for rows in [[], [dict(title="ok", value=2), dict(title="too long!", value=1), dict(title="later", value=3)],
                     [dict(title="ok", value=2), dict(title="bad", value=False)]]:
            self.source.write_text(self.sequential)
            before = self.interpret(rows, "--json")
            self.source.write_text(self.original)
            after = self.interpret(rows, "--json")
            self.assertEqual((after.returncode, after.stdout, after.stderr),
                             (before.returncode, before.stdout, before.stderr))
            self.assertEqual(after.returncode, 1)
            json.loads(after.stdout)

    def test_concurrent_native_reports_and_artifacts_refuse_explicitly(self):
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve")
        for flags, expected in [(["--native", artifact], b"native_memory is required"),
                                (["--native", artifact, "--run", "inspect_readings"], b"native_memory is required"),
                                (["--stack-report", "--json"], b"no native stack report"),
                                (["--wasm", artifact], b"source execution")]:
            out = self.run_compiler(*flags)
            self.assertEqual(out.returncode, 1)
            self.assertIn(expected, out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve")
            if "--stack-report" in flags:
                self.assertEqual(out.stdout, b"")
        out = self.run_compiler()
        self.assertEqual(out.returncode, 0, out.stderr)
        # Selecting an unrelated native rule still has its own entry semantics.
        before, after = self.base / "before", self.base / "after"
        self.source.write_text(self.sequential)
        self.assertEqual(self.run_compiler("--native", before, "--run", "label").returncode, 0)
        self.source.write_text(self.original)
        self.assertEqual(self.run_compiler("--native", after, "--run", "label").returncode, 0)
        self.assertEqual(before.read_bytes(), after.read_bytes())

    def test_concurrent_unselected_invalid_admission_fails_before_input_or_artifact(self):
        bad = self.original.split("execution inspect_readings", 1)[1].replace("max_in_flight: 2", "max_in_flight: 0")
        self.source.write_text(self.original + "\nexecution unused" + bad)
        artifact = self.base / "existing"
        artifact.write_bytes(b"preserve")
        for flags in [["--run", "inspect_readings", "--input", "missing.json", "--json"],
                      ["--native", artifact, "--run", "label"], ["--stack-report", "--json", "--run", "label"]]:
            out = self.run_compiler(*flags)
            self.assertEqual((out.returncode, out.stdout), (1, b""))
            self.assertIn(b"max_in_flight must be in [1, 64]", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve")

    def test_concurrent_records_retain_typed_json_values(self):
        SourceExecutionCLI.test_interpreted_execution_records_keep_typed_values_in_json(self)


class NativeConcurrentExecutionCLI(unittest.TestCase):
    setUp = ConcurrentExecutionCLI.setUp
    run_compiler = SourceExecutionCLI.run_compiler
    interpret = SourceExecutionCLI.interpret

    def test_memory_report_exact_budget_and_unchanged_generous_budget(self):
        out = self.run_compiler("--memory-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        report = json.loads(out.stdout)
        self.assertEqual(report["scope"], "concurrent_execution_reservation")
        self.assertEqual(report["reserved_bytes"], 20480)
        self.assertIsNone(report["declared_bytes"])
        self.assertEqual(report["coordinator_stack_bytes"], 0)
        self.assertEqual(len(report["lanes"]), 2)
        self.assertEqual(report["lanes"][0]["stack_bound_bytes"], report["phases"][2]["stack_bound_bytes"])
        paths = [self.base / "exact", self.base / "generous"]
        for budget, path in zip([20480, 20481], paths):
            self.source.write_text(self.original + f"  native_memory: {budget}\n")
            out = self.run_compiler("--native", path)
            self.assertEqual((out.returncode, out.stderr), (0, b""))
            text = self.run_compiler("--memory-report")
            self.assertIn(b"reserved virtual address bytes", text.stdout)
        self.assertEqual(paths[0].read_bytes(), paths[1].read_bytes())
        artifact = self.base / "preserve"
        artifact.write_bytes(b"preserve")
        invalid = self.original.split("execution inspect_readings", 1)[1] + "  native_memory: 20479\n"
        self.source.write_text(self.original + "\nexecution unused" + invalid)
        for flags in [["--native", artifact, "--run", "clamp"], ["--memory-report", "--json"],
                      ["--run", "inspect_readings", "--input", "missing.json"]]:
            out = self.run_compiler(*flags)
            self.assertEqual((out.returncode, out.stdout), (1, b""))
            self.assertIn(b"20480 bytes exceeds declared 20479", out.stderr)
            self.assertNotIn(b"cannot read", out.stderr)
            self.assertEqual(artifact.read_bytes(), b"preserve")

    def test_native_concurrent_matches_original_interpretation(self):
        path = self.base / "native"
        for limit in [1, 2, 64]:
            self.source.write_text(self.original.replace("max_in_flight: 2", f"max_in_flight: {limit}") + "  native_memory: 1000000\n")
            out = self.run_compiler("--native", path)
            self.assertEqual((out.returncode, out.stderr), (0, b""))
            for rows in [[dict(title="café", value=2), dict(title="x", value=1000)],
                         [dict(title="", value=-2**63), dict(title="🚀", value=2**63-1)],
                         [dict(title="x", value=2), dict(title="y", value=-1), dict(title="z", value=3)]]:
                interpreted = self.interpret(rows)
                native = subprocess.run([str(path), *[v for r in rows for v in [r["title"], str(r["value"])]]],
                                        capture_output=True, timeout=5)
                self.assertEqual((native.returncode, native.stdout, native.stderr),
                                 (interpreted.returncode, interpreted.stdout, interpreted.stderr))
        self.source.write_text(self.original + "  native_memory: 20480\n")
        for flag in ["--stdin", "--stdin-raw", "--stream"]:
            before = path.read_bytes()
            out = self.run_compiler("--native", path, flag)
            self.assertEqual(out.returncode, 1)
            self.assertEqual(path.read_bytes(), before)

    def test_memory_field_and_analysis_flags_are_closed(self):
        for line in ["native_memory: 0", "native_memory: -1", "native_memory: 268435457",
                     "native_memory: 20480\n  native_memory: 20480"]:
            self.source.write_text(self.original + f"  {line}\n")
            out = self.run_compiler("--memory-report", "--json")
            self.assertNotEqual(out.returncode, 0)
            self.assertEqual(out.stdout, b"")
        self.source.write_text(self.sequential + "  native_memory: 20480\n")
        out = self.run_compiler()
        self.assertIn(b"sequential execution does not accept native_memory", out.stderr)
        self.source.write_text(self.original)
        for flags in [["--stack-report"], ["--input", "missing.json"], ["--native", self.base / "absent"],
                      ["--wasm", self.base / "absent"], ["--stdin"], ["--stream"], ["--benchmark"]]:
            out = self.run_compiler("--memory-report", *flags)
            self.assertEqual((out.returncode, out.stdout), (2, b""))
        self.assertFalse((self.base / "absent").exists())
        self.source.write_text(self.sequential)
        out = self.run_compiler("--memory-report", "--json")
        self.assertIn(b"requires a concurrent execution", out.stderr)


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
