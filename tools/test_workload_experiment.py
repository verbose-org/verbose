"""Decision policy, constrained edits, and real compiler/interpreter experiments.

Synthetic timings test decisions; real timings never assert a performance win.
Run after cargo build; VERBOSEC selects another compiler build.
"""
import json
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
import subprocess
from unittest.mock import patch

import experiment_workload as experiment
from workload_experiment_decision import decision, summarize
from workload_experiment_source import configuration, snapshot, variant_source
from test_stack_budget import ROOT, COMPILER


def profile(objective='elapsed'):
    return dict(objective=objective, cases=[dict(name='small', weight=99, records=1, target_us=None),
                                          dict(name='large', weight=1, records=1000, target_us=None)])


def samples(base=(1, 1000), candidate=(2, 10), count=8):
    return [dict(samples={name: {case: dict(wall_ms=value, cpu_ms=value) for case, value in zip(('small', 'large'), values)}
                         for name, values in [('baseline', base), ('candidate', candidate)]}) for _ in range(count)]


class Decisions(unittest.TestCase):
    def test_invocation_weighted_mean_not_median_or_volume(self):
        runs = samples()
        self.assertAlmostEqual(summarize(profile(), runs, 'baseline')['score_ms'], 10.99)
        self.assertAlmostEqual(summarize(profile(), runs, 'candidate')['score_ms'], 2.08)
        self.assertEqual(decision(profile(), runs, ['baseline', 'candidate'], 5)['chosen'], 'candidate')
        runs[-1]['samples']['candidate']['large']['wall_ms'] = 10000
        self.assertIsNone(decision(profile(), runs, ['baseline', 'candidate'], 5)['chosen'])

    def test_cpu_objective_still_obeys_elapsed_targets(self):
        p = profile('cpu')
        runs = samples((10, 10), (1, 1))
        for run in runs:
            run['samples']['candidate']['small']['wall_ms'] = 100
        self.assertEqual(decision(p, runs, ['candidate'], 5)['chosen'], 'candidate')
        p['cases'][0]['target_us'] = 2000
        self.assertIsNone(decision(p, runs, ['candidate'], 5)['chosen'])
        p['objective'] = 'elapsed'
        p['cases'][0]['target_us'] = None
        self.assertIsNone(decision(p, runs, ['candidate'], 5)['chosen'])

    def test_unstable_halves_zero_resolution_and_nonfinite_samples(self):
        runs = samples((10, 10), (1, 1), 4) + samples((10, 10), (11, 11), 4)
        self.assertIsNone(decision(profile(), runs, ['candidate'], 5)['chosen'])
        self.assertIsNone(decision(profile('cpu'), samples((0, 0), (0, 0)), ['candidate'], 5)['chosen'])
        for bad in [float('nan'), float('inf'), -1]:
            runs[0]['samples']['candidate']['small']['wall_ms'] = bad
            with self.assertRaisesRegex(ValueError, 'timing sample'):
                decision(profile(), runs, ['candidate'], 5)

    def test_each_half_balances_candidate_positions_without_discarding_samples(self):
        instance = object.__new__(experiment.Experiment)
        instance.args = SimpleNamespace(repeats=7)
        instance.report = dict(samples={})
        variants = {name: dict(binary=Path('/tmp') / name, threads=2) for name in ['baseline', 'a', 'b']}
        data = {f'selection-{case}': dict(argv=['1']) for case in ['small', 'large']}
        with patch.object(experiment, 'execute', return_value=subprocess.CompletedProcess([], 0, b'', b'')), \
             patch.object(experiment, 'measured', return_value=dict(wall_ms=1, cpu_ms=1, cpu_exceeds_capacity_tolerance=False)) as measure:
            runs = instance.measure('selection', variants, data, profile())
        self.assertEqual(len(runs), 12)
        self.assertEqual(measure.call_count, 12 * 3 * 2)
        for half in [runs[:6], runs[6:]]:
            for name in variants:
                for position in range(3):
                    self.assertEqual(sum(r['order'][position] == name for r in half), 2)


class SourceEditing(unittest.TestCase):
    def setUp(self):
        self.source = (ROOT / 'examples/workload_profile.verbose').read_text()

    def test_only_selected_configuration_changes_strings_comments_and_following_execution_survive(self):
        text = self.source.replace('Describe likely reading batches', 'mode: sequential -- Describe likely reading batches')
        text += self.source[self.source.index('execution '):].replace('inspect_usage', 'second')
        text += '\n-- execution inspect_usage\n--   mode: concurrent\n'
        updated = variant_source(text, 'inspect_usage', dict(name='seq', mode='sequential'), dict(native_stack=192))
        self.assertIn('mode: sequential -- Describe', updated)
        self.assertIn('  native_stack: 192\n', updated)
        self.assertEqual(updated.split('execution second')[1], text.split('execution second')[1])
        self.assertEqual(updated.split('  workload:')[1], text.split('  workload:')[1])
        self.assertEqual(configuration(updated, 'second')[0]['native_memory'], 20480)

    def test_missing_other_scope_overrides_and_unrecognized_layout_refuse(self):
        v = dict(name='seq', mode='sequential')
        for budgets in [{}, dict(native_memory=40000, native_stack=192)]:
            with self.assertRaises(ValueError):
                variant_source(self.source, 'inspect_usage', v, budgets)
        for text in [self.source.replace('\n', '\r\n'), self.source.rstrip(), self.source.replace('execution inspect_usage', 'execution other')]:
            with self.assertRaises(ValueError):
                configuration(text, 'inspect_usage')

    def test_same_mode_proposal_changes_only_the_value_and_can_add_default_batch(self):
        variant = dict(name='b64', mode='concurrent', max_in_flight=2, result_batch=64)
        text = self.source.replace('result_batch: 32', 'result_batch: 32 -- keep this explanation')
        updated = variant_source(text, 'inspect_usage', variant, {})
        self.assertEqual(updated, text.replace('result_batch: 32', 'result_batch: 64'))
        text = self.source.replace('  result_batch: 32\n', '')
        updated = variant_source(text, 'inspect_usage', variant, {})
        self.assertEqual(configuration(updated, 'inspect_usage')[0]['result_batch'], 64)
        self.assertEqual(updated.replace('  result_batch: 64\n', ''), text)

    def test_literal_contents_do_not_become_declarations(self):
        # A masking unit test, not an expansion of the language's string grammar.
        text = 'rule example\n  value: "one\nexecution inspect_usage\n  mode: concurrent\n"\n' + self.source
        self.assertEqual(configuration(text, 'inspect_usage')[0]['native_memory'], 20480)

    def test_snapshot_tracks_import_intentions_and_refuses_escape_or_symlinks(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'sub').mkdir()
            entry = root / 'main.verbose'
            entry.write_text('use "sub/a.verbose"\n  @source: main.intent:1\n-- use "absent"\n')
            (root / 'sub/a.verbose').write_text('use "leaf.verbose"\n  @source: "a.intent":1\n')
            (root / 'leaf.verbose').write_text('-- no dependencies\n')
            (root / 'sub/a.intent').write_text('1. imported\n')
            (root / 'main.intent').write_text('1. entry\n')
            self.assertEqual(set(map(str, snapshot(entry))), {'main.verbose', 'sub/a.verbose', 'leaf.verbose', 'main.intent', 'sub/a.intent'})
            for path in ['../outside.verbose', '/tmp/outside.verbose']:
                entry.write_text(f'use "{path}"\n')
                with self.assertRaisesRegex(ValueError, 'escapes'):
                    snapshot(entry)
            entry.write_text('use "link.verbose"\n')
            (root / 'link.verbose').symlink_to(root / 'leaf.verbose')
            with self.assertRaisesRegex(ValueError, 'symlink'):
                snapshot(entry)


class Experiments(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='verbose-experiment-test-')
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for name in ['workload_profile.verbose', 'workload_profile.intent', 'sequential_stack.verbose', 'sequential_stack.intent']:
            (self.root / name).write_bytes((ROOT / 'examples' / name).read_bytes())
        source = self.root / 'workload_profile.verbose'
        source.write_text(source.read_text().replace('records: 4096', 'records: 3').replace('result_batch: 32', 'result_batch: 1'))
        for name, values in [('small-a', [1]), ('small-b', [2]), ('bulk-a', [0, 100, 1000]),
                             ('bulk-b', [3, 99, 2**63 - 1]), ('false', [2, -1, 3])]:
            self.write(name, [dict(value=v, title='é') for v in values])
        self.write('empty', [])
        self.write('long', [dict(title='too long title', value=1)])
        self.manifest = dict(schema_version=1, source=source.name, execution='inspect_usage', budgets=dict(native_stack=192),
            variants=[dict(name='seq', mode='sequential'), dict(name='batch32', mode='concurrent', max_in_flight=2, result_batch=32),
                      dict(name='serial_lane', mode='concurrent', max_in_flight=1, result_batch=32),
                      dict(name='too_big', mode='concurrent', max_in_flight=2, result_batch=128),
                      dict(name='unchanged', mode='concurrent', max_in_flight=2, result_batch=1)],
            cases=dict(interactive=dict(selection='small-a.json', validation='small-b.json'),
                       bulk=dict(selection='bulk-a.json', validation='bulk-b.json')),
            checks=['false.json', 'empty.json', 'long.json'], argv_checks=[['a', '1', 'b', 'bad', 'c', '3'], []])
        self.args = SimpleNamespace(manifest=self.root / 'experiment.json', compiler=COMPILER,
            output=self.root / 'output', repeats=4, min_gain_pct=5, host_note='Test; no performance claim')
        self.write('experiment', self.manifest)

    def write(self, name, value):
        (self.root / f'{name}.json').write_text(json.dumps(value))

    @staticmethod
    def measured(command, threads):
        v = 10 if Path(command[0]).parent.name == 'baseline' else 1
        return dict(wall_ms=v, cpu_ms=v, cpu_exceeds_capacity_tolerance=False)

    def run_fake(self):
        probe = dict(cpu_exceeds_wall_tolerance=False, inner=dict(cpu_exceeds_wall_tolerance=False))
        with patch.object(experiment, 'measured', side_effect=self.measured), patch.object(experiment, 'clock_probe', return_value=probe):
            return experiment.run_experiment(self.args)

    def test_end_to_end_proposal_keeps_refusals_inputs_identities_and_exact_artifacts(self):
        before = {p: p.read_bytes() for p in self.root.iterdir()}
        code, report = self.run_fake()
        self.assertEqual((code, report['status']), (0, 'proposed'), report.get('reason'))
        self.assertEqual(report['candidates']['too_big']['status'], 'refused')
        self.assertIn('exceeds declared', report['candidates']['too_big']['reason'])
        self.assertEqual(report['candidates']['unchanged']['status'], 'identical_binary')
        self.assertEqual(report['candidates']['seq']['status'], 'functional_refused')
        self.assertEqual(report['candidates']['serial_lane']['status'], 'admitted')
        self.assertEqual(report['frozen_selection'], 'batch32')
        self.assertEqual(set(report['samples']['validation'][0]['samples']), {'baseline', 'batch32'})
        self.assertEqual(report['proposal']['binary_sha256'], report['candidates']['batch32']['binary_sha256'])
        self.assertIn('result_batch: 32', (self.args.output / report['proposal']['source']).read_text())
        self.assertTrue((self.args.output / 'proposal.patch').is_file())
        for path, data in before.items():
            self.assertEqual(path.read_bytes(), data)
        # Compiler schema order, not JSON key order (the fixtures put value first).
        native = next(r for r in report['functional'] if r['dataset'] == 'selection-interactive' and r['backend'] == 'native')
        self.assertEqual(native['status'], 0)
        with self.assertRaises(FileExistsError):
            self.run_fake()

    def test_validation_reversal_never_proposes_or_tries_a_second_choice(self):
        self.manifest['variants'].append(dict(name='batch8', mode='concurrent', max_in_flight=2, result_batch=8))
        self.write('experiment', self.manifest)
        original = experiment.Experiment.measure
        def reversal(instance, stage, variants, datasets, profile):
            runs = original(instance, stage, variants, datasets, profile)
            if stage == 'validation':
                for run in runs:
                    for sample in run['samples']['batch32'].values():
                        sample.update(wall_ms=20, cpu_ms=20)
            return runs
        with patch.object(experiment.Experiment, 'measure', reversal):
            code, report = self.run_fake()
        self.assertEqual((code, report['status']), (0, 'no_proposal'))
        self.assertEqual(report['frozen_selection'], 'batch32')
        self.assertIsNone(report['validation']['chosen'])
        self.assertTrue(report['selection']['candidates']['batch8']['eligible'])
        self.assertNotIn('batch8', report['samples']['validation'][0]['samples'])
        self.assertFalse((self.args.output / 'proposal.patch').exists())

    def test_empty_argv_is_mandatory_and_selection_failure_skips_validation(self):
        self.manifest.update(checks=[], argv_checks=[])
        self.write('experiment', self.manifest)
        self.args.min_gain_pct = 99
        code, report = self.run_fake()
        self.assertEqual((code, report['status']), (0, 'no_proposal'))
        self.assertEqual(report['candidates']['seq']['status'], 'functional_refused')
        self.assertIn('raw argv check 0', report['candidates']['seq']['reason'])
        self.assertNotIn('validation', report['samples'])

    def test_changed_import_or_accounting_anomaly_prevents_proposal(self):
        original = experiment.Experiment.measure
        def mutate(instance, stage, *args):
            runs = original(instance, stage, *args)
            if stage == 'validation':
                path = self.root / 'sequential_stack.intent'
                path.write_text(path.read_text() + '\nchanged\n')
            return runs
        with patch.object(experiment.Experiment, 'measure', mutate):
            code, report = self.run_fake()
        self.assertEqual((code, report['status']), (2, 'inconclusive'))
        self.assertIn(str(self.root / 'sequential_stack.intent'), report['changed_identities'])
        self.assertIsNone(report['proposal'])
        self.args.output = self.root / 'anomaly'
        with patch.object(self, 'measured', return_value=dict(wall_ms=1, cpu_ms=100, cpu_exceeds_capacity_tolerance=True)):
            code, report = self.run_fake()
        self.assertEqual((code, report['status']), (2, 'inconclusive'))
        self.assertTrue(report['accounting_anomaly'])

    def test_closed_manifest_and_data_refusals(self):
        for change in [dict(surprise=1), dict(schema_version=True), dict(budgets=dict(native_memory=True)),
                       dict(variants=[dict(name='baseline', mode='sequential')]), dict(checks='not an array')]:
            m = dict(self.manifest, **change)
            with self.assertRaises(ValueError):
                experiment.validate_manifest(m)
        fields = [dict(name='v', type='number'), dict(name='t', type='text')]
        for rows in [[dict(v=True, t='a')], [dict(v=2**63, t='a')], [dict(v=1, t='a\0')], [dict(v=1)]]:
            with self.assertRaises(ValueError):
                experiment.argv_for(rows, fields)
        (self.root / 'small-b.json').write_text('[ { "title": "é", "value": 1 } ]')
        code, report = self.run_fake()
        self.assertEqual((code, report['status']), (1, 'failed'))
        self.assertIn('differ in content', report['reason'])
        self.assertFalse(report['samples'])

    def test_duplicate_json_keys_and_wrong_record_counts_refuse_before_timing(self):
        path = self.root / 'duplicate.json'
        path.write_text('{"value": 1, "value": 2}')
        with self.assertRaisesRegex(ValueError, 'duplicate JSON key'):
            experiment.read_json(path)
        self.write('small-a', [])
        code, report = self.run_fake()
        self.assertEqual((code, report['status']), (1, 'failed'))
        self.assertIn('expected 1 records', report['reason'])
        self.assertFalse(report['samples'])

    def test_failed_timed_invocation_is_retained_and_cannot_propose(self):
        probe = dict(cpu_exceeds_wall_tolerance=False, inner=dict(cpu_exceeds_wall_tolerance=False))
        with patch.object(experiment, 'measured', side_effect=subprocess.CalledProcessError(1, ['binary'], stderr=b'failure')), \
             patch.object(experiment, 'clock_probe', return_value=probe):
            code, report = experiment.run_experiment(self.args)
        self.assertEqual((code, report['status']), (1, 'failed'))
        failure = report['samples']['selection'][0]['failure']
        self.assertEqual((failure['status'], bytes.fromhex(failure['stderr_hex'])), (1, b'failure'))
        self.assertIsNone(report['proposal'])

    def test_real_measurement_smoke_without_a_speed_threshold(self):
        # Exercise child CPU accounting and clock probes. The result may be
        # inconclusive or have no winner; neither indicates a functional defect.
        code, report = experiment.run_experiment(self.args)
        self.assertIn(code, (0, 2), report.get('reason'))
        self.assertIn(report['status'], ('proposed', 'no_proposal', 'inconclusive'))
        self.assertTrue(report['samples']['selection'])


if __name__ == '__main__':
    unittest.main()
