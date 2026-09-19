"""Measurement/oracle checks, without performance thresholds.

Run after cargo build: python3 tools/test_numeric_benchmark.py -v
"""
import json
from pathlib import Path
import resource
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from benchmark_numeric_contract import ROOT, cpu_exceeds_wall, measure, summarize


class NumericBenchmark(unittest.TestCase):
    def test_impossible_cpu_accounting_is_flagged_with_generous_tolerance(self):
        self.assertTrue(cpu_exceeds_wall(600, 550))
        self.assertTrue(cpu_exceeds_wall(330, 300))
        self.assertFalse(cpu_exceeds_wall(2, 1))
        self.assertFalse(cpu_exceeds_wall(1001, 1000))

    def test_failed_stderr_and_timed_out_children_are_not_samples(self):
        with self.assertRaises(subprocess.CalledProcessError):
            measure([sys.executable, '-c', 'raise SystemExit(7)'])
        with self.assertRaisesRegex(RuntimeError, 'stderr'):
            measure([sys.executable, '-c', 'import sys; sys.stderr.write("failure")'])
        with self.assertRaises(subprocess.TimeoutExpired):
            measure([sys.executable, '-c', 'import time; time.sleep(10)'], timeout=.05)

    def test_previous_child_work_is_excluded_from_accounting(self):
        before = SimpleNamespace(ru_utime=10, ru_stime=4, ru_nvcsw=40,
                                 ru_nivcsw=30, ru_minflt=200, ru_majflt=8)
        after = SimpleNamespace(ru_utime=10.002, ru_stime=4.003, ru_nvcsw=41,
                                ru_nivcsw=32, ru_minflt=205, ru_majflt=8)
        # Large completed-child totals must not be charged to this single run.
        with patch('benchmark_numeric_contract.resource.getrusage', side_effect=[before, after]) as get:
            sample = measure([sys.executable, '-c', 'pass'])
        self.assertEqual([c.args for c in get.call_args_list], [(resource.RUSAGE_CHILDREN,)] * 2)
        self.assertAlmostEqual(sample['cpu_ms'], 5)
        self.assertEqual(sample['involuntary_switches'], 2)
        self.assertEqual(sample['minor_faults'], 5)
        self.assertEqual(sample['major_faults'], 0)

    def test_paired_comparisons_do_not_hide_zero_cpu_readings(self):
        def value(n):
            return dict(wall_ms=n, cpu_ms=n, user_ms=n, system_ms=n)
        summary = summarize([
            {'before': value(10), 'after': value(20)},
            {'before': value(100), 'after': value(50)},
        ])
        # Pairwise +100% and -50%, not a ratio of the unpaired medians.
        self.assertEqual(summary['cpu_ms']['paired_change_pct']['median'], 25)
        summary = summarize([{'before': value(0), 'after': value(2)}])
        self.assertIsNone(summary['cpu_ms']['paired_change_pct'])
        self.assertEqual(summary['cpu_ms']['after']['median'], 2)

    def test_same_compiler_keeps_oracle_order_and_complete_report(self):
        compiler = ROOT / 'target/debug/verbosec'
        self.assertTrue(compiler.is_file(), 'run cargo build first')
        with tempfile.TemporaryDirectory(prefix='verbose-numeric-harness-test-') as directory:
            output = Path(directory) / 'report.json'
            result = subprocess.run([
                sys.executable, str(ROOT / 'tools/benchmark_numeric_contract.py'),
                '--compiler', str(compiler), '--reference-compiler', str(compiler),
                '--reference-revision', 'same-binary-control', '--cases', 'locals', 'live_locals',
                '--records', '8', '--repeats', '4', '--output', str(output),
            ], capture_output=True, text=True, timeout=120)
            self.assertIn(result.returncode, (0, 2), result.stderr)
            report = json.loads(output.read_text())
            try:
                bad_cpu = any(report['cpu_consistency'].values())
                self.assertEqual(report['status'], 'inconclusive' if bad_cpu else 'ok')
                self.assertEqual(result.returncode, 2 if bad_cpu else 0)
                self.assertEqual(len(report['clock_probes']), 2)
                self.assertEqual(report['schema_version'], 2)
                self.assertTrue(report['timing']['balanced_order'])
                self.assertEqual(len(report['cases']), 2)
                for case in report['cases']:
                    self.assertTrue(case['identical_binary'])
                    self.assertTrue(case['entry_failures_agree'])
                    self.assertEqual(case['checked_values'], 201)
                    self.assertEqual([r['order'] for r in case['runs']],
                                     [['before', 'after'], ['after', 'before']] * 2)
                    for run in case['runs']:
                        for label in ['before', 'after']:
                            self.assertGreater(run[label]['wall_ms'], 0)
                            self.assertGreaterEqual(run[label]['cpu_ms'], 0)
                            self.assertAlmostEqual(run[label]['cpu_ms'],
                                                   run[label]['user_ms'] + run[label]['system_ms'])
            finally:
                shutil.rmtree(report['artifacts'])


if __name__ == '__main__':
    unittest.main()
