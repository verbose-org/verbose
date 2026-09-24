"""Check measurement logic without performance thresholds.

Run: python3 tools/test_concurrent_benchmark.py -v
The full harness also checks every generated native fixture before timing.
"""
import unittest
from unittest.mock import patch

from benchmark_concurrent import batch, measured, parse_smaps, summarize, MODES
import benchmark_result_batches as batches


class ConcurrentBenchmark(unittest.TestCase):
    def test_batch_comparison_retains_original_controls_and_paired_ratios(self):
        from benchmark_concurrent import source
        for kind in ['light', 'compute', 'readings']:
            self.assertEqual(batches.fixture(kind, 'sequential'), source(kind, 'sequential'))
            self.assertEqual(batches.fixture(kind, 'batch_1'), source(kind, 'concurrent_4'))
        metrics = ['wall_ms', 'cpu_ms', 'user_ms', 'system_ms', 'voluntary_switches',
                   'involuntary_switches', 'minor_faults', 'major_faults']
        runs = []
        for value in [1, 10]:
            runs.append({'samples': {m: {k: value * (i + 1) for k in metrics}
                                     for i, m in enumerate(batches.MODES)}})
        summary = batches.summary(runs)
        self.assertEqual(summary['batch_128']['paired_wall_ratio_to_batch_1']['median'], 2.5)
        self.assertEqual(summary['batch_128']['paired_wall_ratio_to_sequential']['median'], 5)

    def test_oracle_retains_phase_order_and_signed_truncation(self):
        _, argv, output = batch('light', 201)
        values = list(map(int, output.splitlines()))
        self.assertEqual(argv[0], '-100')
        self.assertEqual(argv[-1], '100')
        self.assertEqual(values[:201], list(range(-100, 101)))
        self.assertEqual(values[201:402], list(range(-99, 102)))
        _, _, output = batch('compute', 201)
        values = list(map(int, output.splitlines()))
        # Division truncates towards zero; starting above a negative fixed
        # point converges to x+1, while starting below a positive one reaches x-1.
        self.assertEqual(values[0], -99 * 96)
        self.assertEqual(values[100], 0)
        self.assertEqual(values[200], 99 * 96)
        self.assertEqual(values[201], values[0] + 1)

    def test_text_oracle_counts_utf8_and_clamps(self):
        _, argv, output = batch('readings', 102)
        lines = output.splitlines()
        self.assertEqual(argv[2], 'é')
        self.assertEqual(lines[101], b'100')
        self.assertEqual(lines[102:204], [b'true'] * 102)
        self.assertEqual(lines[-1], 'é:101'.encode())

    def test_cpu_capacity_allows_parallelism_but_flags_impossible_accounting(self):
        def sample(cpu):
            return {'cpu_ms': cpu, 'accounting_wall_ms': 100,
                    'cpu_exceeds_wall_tolerance': True}, None
        with patch('benchmark_concurrent.os.sched_getaffinity', return_value={2, 4, 6, 8}):
            with patch('benchmark_concurrent.run_measured', return_value=sample(300)):
                self.assertFalse(measured([], 5)['cpu_exceeds_capacity_tolerance'])
                self.assertTrue(measured([], 1)['cpu_exceeds_capacity_tolerance'])
            with patch('benchmark_concurrent.run_measured', return_value=sample(430)):
                self.assertTrue(measured([], 5)['cpu_exceeds_capacity_tolerance'])

    def test_smaps_keeps_reservation_residency_and_guards_distinct(self):
        maps = parse_smaps('''00400000-00401000 r-xp 00000000 08:10 1 /tmp/program
Size:                  4 kB
Rss:                   4 kB
00700000-00701000 ---p 00000000 00:00 0
Size:                  4 kB
Rss:                   0 kB
00701000-00702000 rw-p 00000000 00:00 0
Size:                  4 kB
Rss:                   4 kB
''')
        self.assertEqual(len(maps), 3)
        self.assertEqual(maps[0]['path'], '/tmp/program')
        self.assertEqual(maps[1]['path'], '')
        self.assertEqual(maps[1]['Size_kib'], 4)
        self.assertEqual(maps[1]['Rss_kib'], 0)
        with self.assertRaisesRegex(RuntimeError, 'incomplete'):
            parse_smaps('')

    def test_ratios_are_paired_and_no_samples_are_discarded(self):
        def run(base, candidate):
            metrics = ['wall_ms', 'cpu_ms', 'user_ms', 'system_ms', 'voluntary_switches',
                       'involuntary_switches', 'minor_faults', 'major_faults']
            return {'samples': {m: {k: base if m == 'sequential' else candidate for k in metrics}
                                for m in MODES}}
        summary = summarize([run(10, 20), run(100, 50)])
        self.assertEqual(summary['concurrent_4']['paired_wall_ratio_to_sequential']['median'], 1.25)
        self.assertEqual(summary['sequential']['wall_ms']['min'], 10)
        self.assertEqual(summary['sequential']['wall_ms']['max'], 100)


if __name__ == '__main__':
    unittest.main()
