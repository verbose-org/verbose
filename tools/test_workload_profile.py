"""Predicted workload CLI checks; no performance thresholds or timed runs."""
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

from test_stack_budget import ROOT, COMPILER


class WorkloadCLI(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='verbose-workload-cli-')
        self.addCleanup(self.directory.cleanup)
        self.base = Path(self.directory.name)
        for name in ['workload_profile.verbose', 'workload_profile.intent',
                     'sequential_stack.verbose', 'sequential_stack.intent']:
            (self.base / name).write_bytes((ROOT / 'examples' / name).read_bytes())
        self.source = self.base / 'workload_profile.verbose'
        self.original = self.source.read_text()
        self.control, tail = self.original.split('  workload:\n', 1)
        self.profile = '  workload:\n' + tail

    def compiler(self, *args, source=None):
        return subprocess.run([str(COMPILER), str(source or self.source), *map(str, args)],
                              capture_output=True, timeout=30)

    def report(self):
        out = self.compiler('--workload-report', '--json')
        self.assertEqual((out.returncode, out.stderr), (0, b''), out)
        return json.loads(out.stdout)

    def test_report_distinguishes_frequency_volume_targets_and_verified_storage(self):
        report = self.report()
        self.assertEqual(report['profile_kind'], 'prediction')
        self.assertFalse(report['measurements_available'])
        self.assertEqual(report['measurement_metric'], 'wall_us')
        self.assertEqual(report['aggregation'], 'weighted_arithmetic_mean_per_invocation')
        self.assertEqual(report['target_kind'], 'unverified_elapsed_goal')
        self.assertEqual(report['expected_records_per_invocation'], dict(numerator=4195, denominator=100))
        self.assertEqual(report['expected_full_success_phase_evaluations'], dict(numerator=12585, denominator=100))
        self.assertEqual(report['expected_full_success_native_result_batches'], dict(numerator=681, denominator=100))
        common, bulk = report['cases']
        self.assertEqual(common['invocation_share'], dict(numerator=99, denominator=100))
        self.assertEqual(bulk['record_volume_share'], dict(numerator=4096, denominator=4195))
        self.assertEqual((common['target_us'], bulk['target_us']), (2000, 100000))
        self.assertEqual((common['full_success_native_result_batches'], bulk['full_success_native_result_batches']), (3, 384))
        self.assertTrue(report['native_emission_budget_present'])
        self.assertEqual(report['native_resource'], json.loads(self.compiler('--memory-report', '--json').stdout))
        self.assertEqual(report['native_resource']['reserved_bytes'], 20480)
        out = self.compiler('--workload-report', '--json', '--run', 'inspect_usage')
        self.assertEqual(out.stdout, self.compiler('--workload-report', '--json').stdout)
        text = self.compiler('--workload-report')
        self.assertEqual((text.returncode, text.stderr), (0, b''))
        self.assertIn(b'99/4195 record volume', text.stdout)
        self.assertIn(b'not verified deadlines', text.stdout)

    def test_weights_are_relative_cpu_objective_and_latency_goal_are_independent(self):
        self.source.write_text(self.original.replace('objective: elapsed', 'objective: cpu')
                               .replace('weight: 99', 'weight: 198').replace('weight: 1\n', 'weight: 2\n')
                               .replace('      target_us: 2000\n', ''))
        report = self.report()
        self.assertEqual((report['objective'], report['measurement_metric']), ('cpu', 'cpu_us'))
        self.assertIsNone(report['cases'][0]['target_us'])
        self.assertEqual(report['cases'][1]['target_us'], 100000)
        self.assertEqual(report['expected_records_per_invocation'], dict(numerator=8390, denominator=200))

    def test_profile_never_changes_native_bytes_reports_or_interpreted_inputs(self):
        data = self.base / 'input.json'
        for sequential in [False, True]:
            control = self.control
            if sequential:
                control = control.replace('mode: concurrent', 'mode: sequential')
                control = control.replace('  max_in_flight: 2\n  result_batch: 32\n  native_memory: 20480',
                                          '  native_stack: 192')
            binaries = []
            outcomes = []
            layouts = []
            for suffix in ['', self.profile, self.profile.replace('records: 4096', 'records: 1').replace('target_us: 2000', 'target_us: 1')]:
                self.source.write_text(control + suffix)
                path = self.base / 'binary'
                built = self.compiler('--native', path)
                self.assertEqual(built.returncode, 0, built.stderr)
                binaries.append(path.read_bytes())
                layouts.append(self.compiler('--stack-report' if sequential else '--memory-report', '--json').stdout)
                observed = []
                # 3 records are outside both predicted sizes. False and invalid
                # inputs keep their complete output prefixes and status.
                for values in [[1, 2, 3], [1, -1, 3], [1, 'bad', 3], []]:
                    data.write_text(json.dumps([dict(title='é', value=v) for v in values]))
                    out = self.compiler('--run', 'inspect_usage', '--input', data)
                    observed.append((out.returncode, out.stdout, out.stderr))
                    native = subprocess.run([str(path), *[x for v in values for x in ['é', str(v)]]],
                                            capture_output=True, timeout=5)
                    observed.append((native.returncode, native.stdout, native.stderr))
                outcomes.append(observed)
            self.assertEqual(binaries[0], binaries[1])
            self.assertEqual(binaries[0], binaries[2])
            self.assertEqual(outcomes[0], outcomes[1])
            self.assertEqual(outcomes[0], outcomes[2])
            self.assertEqual(layouts[0], layouts[1])
            self.assertEqual(layouts[0], layouts[2])

    def test_scope_and_missing_native_ceiling_are_explicit(self):
        self.source.write_text(self.original.replace('  native_memory: 20480\n', ''))
        report = self.report()
        self.assertFalse(report['native_emission_budget_present'])
        self.assertIsNone(report['native_resource']['declared_bytes'])
        path = self.base / 'artifact'
        path.write_bytes(b'preserve')
        out = self.compiler('--native', path)
        self.assertEqual(out.returncode, 1)
        self.assertIn(b'native_memory is required', out.stderr)
        self.assertEqual(path.read_bytes(), b'preserve')
        sequential = self.original.replace('mode: concurrent', 'mode: sequential')
        sequential = sequential.replace('  max_in_flight: 2\n  result_batch: 32\n  native_memory: 20480', '  native_stack: 192')
        self.source.write_text(sequential)
        report = self.report()
        self.assertIsNone(report['expected_full_success_native_result_batches'])
        self.assertTrue(all(c['full_success_native_result_batches'] is None for c in report['cases']))
        self.assertEqual(report['native_resource']['scope'], 'additional_entry_stack')
        self.assertEqual(report['native_resource']['stack_bound_bytes'], 192)

    def test_report_selection_modes_and_backend_refusals_preserve_artifacts(self):
        artifact = self.base / 'artifact'
        artifact.write_bytes(b'preserve')
        for flags in [['--native', artifact], ['--wasm', artifact], ['--memory-report'], ['--stack-report'],
                      ['--input', 'missing.json'], ['--stdin'], ['--stream'], ['--benchmark'], ['--disasm']]:
            out = self.compiler('--workload-report', *flags)
            self.assertEqual((out.returncode, out.stdout), (2, b''), out)
            self.assertIn(b'--workload-report', out.stderr)
            self.assertEqual(artifact.read_bytes(), b'preserve')
        for name in ['clamp', 'missing', 'inspect_usage,clamp']:
            out = self.compiler('--workload-report', '--run', name)
            self.assertEqual((out.returncode, out.stdout), (1, b''))
            self.assertIn(b'requires one source execution', out.stderr)
        out = self.compiler('--wasm', artifact)
        self.assertEqual(out.returncode, 1)
        self.assertIn(b'WASM does not support source execution', out.stderr)
        self.assertEqual(artifact.read_bytes(), b'preserve')
        self.source.write_text(self.control)
        out = self.compiler('--workload-report')
        self.assertEqual((out.returncode, out.stdout), (1, b''))
        self.assertIn(b'requires a declared workload', out.stderr)
        # Default selection does not search backwards for a profiled execution.
        declaration = self.control.split('execution inspect_usage', 1)[1]
        self.source.write_text(self.original + '\nexecution last' + declaration)
        out = self.compiler('--workload-report')
        self.assertEqual((out.returncode, out.stdout), (1, b''))
        self.assertIn(b"execution 'last'", out.stderr)

    def test_imports_and_unselected_invalid_profiles_or_budgets(self):
        wrapper = self.base / 'wrapper.verbose'
        wrapper.write_text('@verbose 0.1.0\nuse "workload_profile.verbose"\n')
        imported = self.compiler('--workload-report', '--json', source=wrapper)
        self.assertEqual((imported.returncode, imported.stderr), (0, b''))
        self.assertEqual(json.loads(imported.stdout), self.report())
        path = self.base / 'artifact'
        path.write_bytes(b'preserve')
        for old, new, message in [('weight: 99', 'weight: 0', b'weight'),
                                  ('native_memory: 20480', 'native_memory: 20479', b'exceeds declared')]:
            self.source.write_text(self.original.replace(old, new))
            for args in [[], ['--workload-report'], ['--native', path, '--run', 'clamp']]:
                out = self.compiler(*args)
                self.assertEqual((out.returncode, out.stdout), (1, b''))
                self.assertIn(message, out.stderr)
                self.assertEqual(path.read_bytes(), b'preserve')


if __name__ == '__main__':
    unittest.main()
