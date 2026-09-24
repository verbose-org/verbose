"""Original-AST/native differential and source-boundary checks for pipelines."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
COMPILER = Path(os.environ.get("VERBOSEC", ROOT / "target/debug/verbosec")).resolve()


class PipelineCLI(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="verbose-pipeline-cli-")
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        for name in ["pipeline_stack.verbose", "pipeline_stack.intent", "retained_stack.verbose", "retained_stack.intent"]:
            (self.base / name).write_bytes((ROOT / "examples" / name).read_bytes())
        self.source = self.base / "pipeline_stack.verbose"
        self.rules = self.base / "retained_stack.verbose"
        self.original = self.source.read_text()
        self.original_rules = self.rules.read_text()
        self.binary = self.base / "native"

    def compiler(self, *args, input=None):
        return subprocess.run([str(COMPILER), str(self.source), *map(str, args)],
                              input=input, capture_output=True, timeout=30)

    def build(self, *args):
        out = self.compiler("--native", self.binary, *args)
        self.assertEqual(out.returncode, 0, out.stderr)
        return self.binary.read_bytes()

    def native(self, rows):
        argv = [v for r in rows for v in [r["title"], str(r["code"])]]
        return subprocess.run([str(self.binary), *argv], capture_output=True, timeout=5)

    def interpret(self, rows, *flags):
        return self.compiler("--run", "prepare_readings", "--stdin", *flags,
                             input=json.dumps(rows).encode())

    def final_rule(self, ty, expr, reads):
        # No bounded text output or stack annotation in any source rule: the
        # pipeline declaration itself must activate strict composition checks.
        self.rules.write_text(self.original_rules.split("rule render")[0] + f'''rule render
  @intention: "Consume the prepared record"
  @source: retained_stack.intent:5
  input:
    record : Prepared
  output:
    out : {ty}
  logic:
    out = {expr}
  proofs:
    purity:
      reads: [{reads}]
      calls: []
    termination:
      bound: 100
''')

    def test_default_selection_report_and_exact_budget(self):
        out = self.compiler("--stack-report", "--json")
        self.assertEqual((out.returncode, out.stderr), (0, b""))
        report = json.loads(out.stdout)
        self.assertEqual(report["composition"], "pipeline")
        self.assertEqual(report["publication"], "final_phase")
        self.assertEqual(report["phases"], ["prepare", "forward", "render"])
        self.assertEqual(report["stack_bound_bytes"], report["invocation"]["stack_bound_bytes"])
        self.assertEqual(report["retained_storage"], "included_in_invocation")
        self.assertEqual(out.stdout, self.compiler("--stack-report", "--json", "--run", "prepare_readings").stdout)
        original = self.build()
        self.assertEqual(original, self.build("--run", "prepare_readings"))
        bound = report["stack_bound_bytes"]
        self.source.write_text(self.original.replace("native_stack: 512", f"native_stack: {bound}"))
        self.assertEqual(original, self.build())
        self.source.write_text(self.original.replace("native_stack: 512", f"native_stack: {bound - 1}"))
        out = self.compiler("--native", self.binary)
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"exceeds declared", out.stderr)
        self.assertEqual(original, self.binary.read_bytes())

    def test_final_only_values_utf8_i64_and_native_parity(self):
        self.build()
        for rows in [
            [dict(title="café", code=42), dict(title="sample", code=-42)],
            [dict(title="", code=-2**63), dict(title="éééé", code=2**63 - 1)],
            [dict(title='a"\\\n', code=0), dict(title="🚀", code=1)],
        ]:
            interpreted = self.interpret(rows)
            native = self.native(rows)
            expected = "".join(f'[{r["title"]}]:{1 if r["code"] > 0 else -1}\n' for r in rows).encode()
            self.assertEqual((interpreted.returncode, interpreted.stdout, interpreted.stderr), (0, expected, b""))
            self.assertEqual((native.returncode, native.stdout, native.stderr), (0, expected, b""))
            events = self.interpret(rows, "--json")
            self.assertEqual(json.loads(events.stdout), [dict(phase=3, rule="render", record=i,
                value=f'[{r["title"]}]:{1 if r["code"] > 0 else -1}') for i, r in enumerate(rows)])

    def test_scalar_record_and_boolean_final_values_without_text_annotations(self):
        rows = [dict(title="one", code=1), dict(title="two", code=-1), dict(title="three", code=1)]
        for ty, expr, reads, values, status in [
            ("number", "record.code", "record.code", [1, -1, 1], 0),
            ("bool", "record.code > 0", "record.code", [True, False, True], 1),
            ("Prepared", "record", "record", [dict(title=f'[{r["title"]}]', code=r["code"]) for r in rows], 0),
        ]:
            self.final_rule(ty, expr, reads)
            self.build()
            a, b = self.interpret(rows), self.native(rows)
            self.assertEqual((a.returncode, a.stdout, a.stderr), (b.returncode, b.stdout, b.stderr))
            events = self.interpret(rows, "--json")
            self.assertEqual((events.returncode, events.stderr), (status, b""))
            self.assertEqual([e["value"] for e in json.loads(events.stdout)], values)

    def test_alias_shadowing_branches_and_repeated_phases_keep_values(self):
        self.rules.write_text(self.original_rules.replace(
            "    out = item", '''    let saved = item
    let item = Prepared { title: "shadow", code: 0 }
    out = if saved.code > 0 then saved else Prepared { title: concat(saved.title, ""), code: saved.code }''')
            .replace("reads: [item]", "reads: []").replace("      bound: 1\n", "      bound: 100\n"))
        self.source.write_text(self.original.replace("prepare, forward, render", "prepare, forward, forward, render"))
        self.build()
        rows = [dict(title="original", code=1), dict(title="other", code=-1)]
        a, b = self.interpret(rows), self.native(rows)
        self.assertEqual((a.returncode, a.stdout, a.stderr), (0, b"[original]:1\n[other]:-1\n", b""))
        self.assertEqual((a.returncode, a.stdout, a.stderr), (b.returncode, b.stdout, b.stderr))
        self.assertTrue(all(e["phase"] == 4 for e in json.loads(self.interpret(rows, "--json").stdout)))

    def test_invalid_later_input_preserves_only_completed_final_results(self):
        self.build()
        for bad in [dict(title="ééééé", code=1), dict(title="x", code="bad"), dict(title="x")]:
            rows = [dict(title="ok", code=1), bad, dict(title="later", code=2)]
            a = self.interpret(rows)
            self.assertEqual((a.returncode, a.stdout), (1, b"[ok]:1\n"))
            self.assertIn(b"phase 1 ('prepare'), record 1", a.stderr)
            if "code" in bad:
                b = self.native(rows)
                self.assertEqual((b.returncode, b.stdout), (1, a.stdout))
            events = self.interpret(rows, "--json")
            self.assertEqual(json.loads(events.stdout), [dict(phase=3, rule="render", record=0, value="[ok]:1")])
        a, b = self.interpret([]), self.native([])
        self.assertEqual((a.returncode, a.stdout), (1, b""))
        self.assertEqual((b.returncode, b.stdout), (1, b""))
        self.assertIn(b"at least one input record", a.stderr)
        self.assertEqual(json.loads(self.interpret([], "--json").stdout), [])

    def test_pipeline_checks_even_unused_input_fields(self):
        self.rules.write_text(self.original_rules.replace("    code : number\n", "    code : number\n    unused : text [..1]\n", 1))
        self.build()
        rows = [dict(title="ok", code=1, unused="xx")]
        a = self.interpret(rows)
        b = subprocess.run([str(self.binary), "ok", "1", "xx"], capture_output=True, timeout=5)
        self.assertEqual((a.returncode, a.stdout), (1, b""))
        self.assertEqual((b.returncode, b.stdout), (1, b""))
        self.assertIn(b"unused", a.stderr)

    def test_native_rejects_invalid_integer_spellings_and_partial_final_record(self):
        self.build()
        for args in [["bad", value] for value in ["", "-", "+1", "1x", "1.0", " 1", "9223372036854775808", "-9223372036854775809"]] + [["partial"]]:
            out = subprocess.run([str(self.binary), "ok", "1", *args], capture_output=True, timeout=5)
            self.assertEqual((out.returncode, out.stdout, out.stderr), (1, b"[ok]:1\n", b""), args)

    def test_invalid_unselected_declarations_refuse_before_input_and_artifacts(self):
        declaration = self.original[self.original.index("execution "):]
        self.source.write_text(self.original + "\n" + declaration.replace("prepare_readings", "unused")
                               .replace("native_stack: 512", "native_stack: 1"))
        self.binary.write_bytes(b"preserve")
        for args in [[], ["--stack-report", "--json"], ["--native", self.binary, "--run", "render"],
                     ["--run", "prepare_readings", "--input", "missing.json"], ["--wasm", self.binary]]:
            out = self.compiler(*args)
            self.assertEqual((out.returncode, out.stdout), (1, b""))
            self.assertIn(b"execution 'unused'", out.stderr)
            self.assertNotIn(b"cannot read execution input", out.stderr)
            self.assertEqual(self.binary.read_bytes(), b"preserve")

    def test_unsupported_modes_and_profiles_preserve_existing_artifact(self):
        self.binary.write_bytes(b"preserve")
        for args in [["--native", self.binary, flag] for flag in ["--stdin", "--stdin-raw", "--stream"]] + [
            ["--wasm", self.binary], ["--native", self.binary, "--run", "prepare_readings,render"],
            ["--memory-report"], ["--workload-report"],
        ]:
            out = self.compiler(*args)
            self.assertEqual(out.returncode, 1, (args, out))
            self.assertEqual(self.binary.read_bytes(), b"preserve")
        self.source.write_text(self.original + "  workload:\n    objective: cpu\n    case common:\n      weight: 1\n      records: 1\n")
        out = self.compiler("--native", self.binary)
        self.assertEqual(out.returncode, 1)
        self.assertIn(b"pipeline execution does not accept workload", out.stderr)
        self.assertEqual(self.binary.read_bytes(), b"preserve")

    def test_unknown_analysis_is_a_source_error_before_optimization(self):
        # Dead arithmetic would be foldable; the source contract must refuse it.
        self.final_rule("number", "1 + 2", "")
        self.binary.write_bytes(b"preserve")
        out = self.compiler("--native", self.binary)
        self.assertEqual((out.returncode, out.stdout), (1, b""))
        self.assertIn(b"numeric arithmetic is outside", out.stderr)
        self.assertEqual(self.binary.read_bytes(), b"preserve")


if __name__ == "__main__":
    unittest.main()
