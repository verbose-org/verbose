"""Compare verified native execution variants and propose a measured source diff.

Linux x86-64, Python standard library only. See docs/workload-experiments.md.
The original project is never written; a fresh output directory keeps evidence.
"""
import argparse
from datetime import datetime, timezone
import difflib
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys

from benchmark_concurrent import measured
from benchmark_numeric_contract import ROOT, clock_probe, digest, signature
from workload_experiment_decision import decision
from workload_experiment_source import IDENT, configuration, relative, snapshot, variant_source


class Inconclusive(RuntimeError):
    pass


class FunctionalMismatch(ValueError):
    pass


def sha(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, ensure_ascii=True, separators=(',', ':'), allow_nan=False).encode()


def closed(value, required, optional=()):
    if not isinstance(value, dict) or not set(required) <= value.keys() or value.keys() - set(required) - set(optional):
        raise ValueError(f'expected keys {list(required)}, optional {list(optional)}')


def integer(value, maximum):
    if type(value) is not int or not 1 <= value <= maximum:
        raise ValueError(f'expected integer in 1..{maximum}')


def read_json(path):
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError(f'duplicate JSON key: {key}')
            result[key] = value
        return result
    return json.loads(path.read_bytes(), object_pairs_hook=pairs,
                      parse_constant=lambda v: (_ for _ in ()).throw(ValueError(f'non-finite JSON value: {v}')))


def validate_manifest(m):
    closed(m, ('schema_version', 'source', 'execution', 'variants', 'cases', 'checks', 'argv_checks'), ('budgets',))
    if type(m['schema_version']) is not int or m['schema_version'] != 1:
        raise ValueError('unsupported experiment schema_version')
    relative(m['source'])
    if not isinstance(m['execution'], str) or not re.fullmatch(IDENT, m['execution']):
        raise ValueError('execution must be an identifier')
    budgets = m.get('budgets', {})
    closed(budgets, (), ('native_stack', 'native_memory'))
    for k, v in budgets.items():
        integer(v, 2097152 if k == 'native_stack' else 268435456)
    if not isinstance(m['variants'], list) or not 1 <= len(m['variants']) <= 8:
        raise ValueError('experiment requires 1..8 variants')
    names = {'baseline', 'proposal'}
    for v in m['variants']:
        closed(v, ('name', 'mode'), ('max_in_flight', 'result_batch'))
        if not isinstance(v['name'], str) or not re.fullmatch(IDENT, v['name']) or v['name'] in names:
            raise ValueError('variant names must be unique identifiers; baseline/proposal are reserved')
        names.add(v['name'])
        if v['mode'] == 'concurrent':
            closed(v, ('name', 'mode', 'max_in_flight', 'result_batch'))
            integer(v['max_in_flight'], 64)
            integer(v['result_batch'], 1024)
        elif v['mode'] == 'sequential':
            closed(v, ('name', 'mode'))
        else:
            raise ValueError('variant mode must be sequential or concurrent')
    if not isinstance(m['cases'], dict) or not m['cases']:
        raise ValueError('cases must map profile names to selection/validation files')
    for case in m['cases'].values():
        closed(case, ('selection', 'validation'))
        for path in case.values():
            relative(path)
    if not isinstance(m['checks'], list) or not isinstance(m['argv_checks'], list):
        raise ValueError('checks and argv_checks must be arrays')
    for path in m['checks']:
        relative(path)
    for argv in m['argv_checks']:
        if not isinstance(argv, list) or any(not isinstance(s, str) or '\x00' in s for s in argv):
            raise ValueError('argv checks must be arrays of NUL-free strings')
        for value in argv:
            value.encode('utf-8')


def argv_for(rows, fields):
    if not isinstance(rows, list):
        raise ValueError('input must be a JSON array of flat records')
    argv = []
    for row in rows:
        closed(row, [f['name'] for f in fields])
        for field in fields:
            value = row[field['name']]
            if field['type'] == 'number':
                if type(value) is not int or not -(2**63) <= value < 2**63:
                    raise ValueError('numeric input must be an i64 JSON integer')
                argv.append(str(value))
            elif field['type'] == 'text':
                if not isinstance(value, str) or '\x00' in value:
                    raise ValueError('native argv text must be a NUL-free string')
                value.encode('utf-8')
                argv.append(value)
            else:
                raise ValueError('experiment supports flat number/text inputs only')
    return argv


def execute(command):
    return subprocess.run([str(a) for a in command], capture_output=True, timeout=120)


class Experiment:
    def __init__(self, args):
        self.args = args
        self.output = args.output.resolve()
        self.output.mkdir(parents=True, exist_ok=False)
        self.report = dict(schema_version=1, status='running', started_utc=datetime.now(timezone.utc).isoformat(),
                           host_note=args.host_note, platform=platform.platform(), machine=platform.machine(),
                           python=sys.version, cpuinfo=Path('/proc/cpuinfo').read_text(),
                           cpus=sorted(os.sched_getaffinity(0)), repeats_requested=args.repeats,
                           warmups_per_case=2, timed_stdout='/dev/null',
                           order_policy='balanced_rotating_candidates_and_cases',
                           min_gain_pct=args.min_gain_pct, candidates={}, samples={}, functional=[],
                           identities={}, proposal=None)
        self.identities = self.report['identities']

    def remember(self, path, expected=None):
        path = path.absolute()
        value = digest(path)
        if (expected is not None and value != expected) or (str(path) in self.identities and self.identities[str(path)] != value):
            raise Inconclusive(f'identity changed during experiment: {path}')
        self.identities[str(path)] = value
        return value

    def save(self):
        (self.output / 'report.json').write_text(json.dumps(self.report, indent=2, allow_nan=False) + '\n')

    def transcript(self, label, command):
        result = execute(command)
        directory = self.output / 'transcripts'
        directory.mkdir(exist_ok=True)
        (directory / f'{label}.stdout').write_bytes(result.stdout)
        (directory / f'{label}.stderr').write_bytes(result.stderr)
        (directory / f'{label}.json').write_text(json.dumps(dict(command=list(map(str, command)), status=result.returncode)))
        return result

    def build(self, name, text, files):
        project = self.output / 'variants' / name / 'project'
        project.mkdir(parents=True)
        for path, data in files.items():
            target = project / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
        source = project / self.entry.name
        source.write_text(text, encoding='utf-8')
        candidate = dict(status='checking', source_sha256=digest(source), source=str(source.relative_to(self.output)))
        self.report['candidates'][name] = candidate
        for path in files:
            self.remember(project / path)
        command = [self.compiler, source, '--run', self.manifest['execution']]
        result = self.transcript(f'{name}-report', [*command, '--workload-report', '--json'])
        if result.returncode:
            candidate.update(status='refused', reason=result.stderr.decode(errors='replace'))
            return None
        profile = json.loads(result.stdout)
        candidate['profile'] = profile
        binary = project.parent / 'native'
        result = self.transcript(f'{name}-build', [*command, '--native', binary])
        if result.returncode:
            candidate.update(status='refused', reason=result.stderr.decode(errors='replace'))
            return None
        second = project.parent / 'native-repeat'
        result = self.transcript(f'{name}-repeat', [*command, '--native', second])
        if result.returncode or binary.read_bytes() != second.read_bytes():
            raise ValueError(f'{name}: emission is not reproducible')
        for path in [binary, second]:
            self.remember(path)
        candidate.update(status='admitted', binary_sha256=digest(binary), binary_bytes=binary.stat().st_size)
        resource = profile['native_resource']
        threads = 1 + len(resource['lanes']) if resource['composition'] == 'concurrent' else 1
        return dict(name=name, source=source, binary=binary, profile=profile, threads=threads)

    def load_data(self, profile):
        m = self.manifest
        if m['cases'].keys() != {c['name'] for c in profile['cases']}:
            raise ValueError('manifest must cover exactly the declared workload cases')
        if not profile.get('input_fields') or any(f['type'] not in ('number', 'text') for f in profile['input_fields']):
            raise ValueError('compiler report must expose ordered flat number/text input_fields')
        datasets, hashes = {}, {'selection': set(), 'validation': set()}
        inputs = self.output / 'inputs'
        inputs.mkdir()
        entries = [(f'{stage}-{c["name"]}', m['cases'][c['name']][stage], c['records'], stage)
                   for stage in ('selection', 'validation') for c in profile['cases']]
        entries += [(f'check-{i}', path, None, None) for i, path in enumerate(m['checks'])]
        for label, name, records, stage in entries:
            path = self.args.manifest.resolve().parent / name
            self.remember(path)
            rows = read_json(path)
            argv = argv_for(rows, profile['input_fields'])
            if records is not None and len(rows) != records:
                raise ValueError(f'{label}: expected {records} records, got {len(rows)}')
            if stage:
                hashes[stage].add(sha(canonical(rows)))
            target = inputs / f'{label}.json'
            target.write_bytes(path.read_bytes())
            self.remember(target)
            datasets[label] = dict(path=target, argv=argv, timed=stage is not None)
        if hashes['selection'] & hashes['validation']:
            raise ValueError('selection and validation batches must differ in content, not only filename/format')
        return datasets

    def functional(self, variants, datasets, phase):
        for label, data in datasets.items():
            references = {}
            for name, variant in variants.items():
                results = {}
                for backend, command in [('native', [variant['binary'], *data['argv']]),
                                         ('interpreter', [self.compiler, variant['source'], '--run', self.manifest['execution'], '--input', data['path']])]:
                    out = self.transcript(f'{phase}-{label}-{name}-{backend}', command)
                    sig = signature(out)
                    results[backend] = sig
                    if name == 'baseline':
                        references[backend] = sig
                    elif sig != references[backend]:
                        raise FunctionalMismatch(f'{label}: {name} {backend} differs from baseline')
                    if data['timed'] and (out.returncode != 0 or out.stderr):
                        raise ValueError(f'{label}: timed cases must succeed without stderr')
                    self.report['functional'].append(dict(phase=phase, dataset=label, variant=name, backend=backend,
                        status=out.returncode, stdout_sha256=sha(out.stdout), stderr_sha256=sha(out.stderr)))
                # Runtime input errors use contextual interpreter diagnostics.
                # For evaluated batches (including boolean false), channels agree.
                if not results['interpreter'][2] and results['native'] != results['interpreter']:
                    raise FunctionalMismatch(f'{label}: interpreter/native disagree for {name}')
        # Empty argv exposes a currently observable sequential/concurrent
        # diagnostic difference. It must never be hidden by omitted fixtures.
        for i, argv in enumerate([[], *self.manifest['argv_checks']]):
            expected = None
            for name, variant in variants.items():
                out = self.transcript(f'{phase}-argv-{i}-{name}', [variant['binary'], *argv])
                if name == 'baseline':
                    expected = signature(out)
                elif signature(out) != expected:
                    raise FunctionalMismatch(f'raw argv check {i}: {name} differs from baseline')

    def admit_functional(self, variants, datasets):
        self.functional({'baseline': variants['baseline']}, datasets, 'before-baseline')
        admitted = {'baseline': variants['baseline']}
        for name, variant in variants.items():
            if name == 'baseline':
                continue
            try:
                self.functional({'baseline': variants['baseline'], name: variant}, datasets, 'before-' + name)
            except FunctionalMismatch as error:
                self.report['candidates'][name].update(status='functional_refused', reason=str(error))
            else:
                admitted[name] = variant
        return admitted

    def measure(self, stage, variants, datasets, profile):
        names = list(variants)
        repeats = math.ceil(self.args.repeats / (2 * len(names))) * 2 * len(names)
        runs = self.report['samples'][stage] = []
        for _ in range(2):
            for name in names:
                for case in profile['cases']:
                    command = [variants[name]['binary'], *datasets[f'{stage}-{case["name"]}']['argv']]
                    out = execute(command)
                    if out.returncode or out.stderr:
                        raise ValueError('warmup failed')
        for round_index in range(repeats):
            offset = round_index % len(names)
            order = names[offset:] + names[:offset]
            run = dict(round=round_index, order=order, samples={})
            runs.append(run)
            cases = profile['cases']
            offset = round_index % len(cases)
            for name in order:
                sample = run['samples'][name] = {}
                for case in cases[offset:] + cases[:offset]:
                    v = variants[name]
                    try:
                        sample[case['name']] = measured([str(v['binary']), *datasets[f'{stage}-{case["name"]}']['argv']], v['threads'])
                    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                        run['failure'] = dict(variant=name, case=case['name'], reason=str(error),
                            status=getattr(error, 'returncode', None), stderr_hex=(getattr(error, 'stderr', None) or b'').hex())
                        raise
        return runs

    def stable_identities(self):
        changed = [path for path, value in self.identities.items() if not Path(path).is_file() or digest(Path(path)) != value]
        self.report['changed_identities'] = changed
        return not changed

    def run(self):
        self.remember(self.args.manifest)
        m = self.manifest = read_json(self.args.manifest)
        validate_manifest(m)
        self.report['manifest'] = m
        self.report['tool_repository_revision'] = execute(['git', '-C', ROOT, 'rev-parse', 'HEAD']).stdout.decode().strip()
        self.report['tool_repository_status'] = execute(['git', '-C', ROOT, 'status', '--porcelain']).stdout.decode()
        self.report['tool_repository_diff_sha256'] = sha(execute(['git', '-C', ROOT, 'diff', 'HEAD']).stdout)
        (self.output / 'manifest.json').write_bytes(self.args.manifest.read_bytes())
        self.entry = (self.args.manifest.resolve().parent / m['source']).absolute()
        path = self.args.manifest.resolve().parent
        for part in Path(m['source']).parts:
            path /= part
            if path.is_symlink():
                raise ValueError(f'snapshot refuses symlink: {path}')
        files = snapshot(self.entry)
        self.report['source_files'] = {str(p): sha(data) for p, data in files.items()}
        for path in files:
            self.remember(self.entry.parent / path, sha(files[path]))
        for name in ['experiment_workload.py', 'workload_experiment_source.py', 'workload_experiment_decision.py',
                     'benchmark_concurrent.py', 'benchmark_numeric_contract.py']:
            self.remember(Path(__file__).parent / name)
        compiler_hash = self.remember(self.args.compiler)
        self.compiler = self.output / 'verbosec'
        shutil.copy2(self.args.compiler, self.compiler)
        self.remember(self.compiler, compiler_hash)
        original = files[Path(self.entry.name)].decode('utf-8')
        baseline = self.build('baseline', original, files)
        if baseline is None:
            raise ValueError('baseline refused; see compiler transcripts')
        profile = baseline['profile']
        self.report['profile_sha256'] = sha(canonical(profile))
        configuration(original, m['execution'])
        variants = {'baseline': baseline}
        binary_owners = {digest(baseline['binary']): 'baseline'}
        for v in m['variants']:
            text = variant_source(original, m['execution'], v, m.get('budgets', {}))
            candidate = self.build(v['name'], text, files)
            if candidate:
                binary_hash = digest(candidate['binary'])
                if binary_hash in binary_owners:
                    self.report['candidates'][v['name']].update(status='identical_binary', duplicate_of=binary_owners[binary_hash])
                else:
                    binary_owners[binary_hash] = v['name']
                    variants[v['name']] = candidate
        datasets = self.load_data(profile)
        variants = self.admit_functional(variants, datasets)
        self.report['clock_before'] = clock_probe()
        selection = self.measure('selection', variants, datasets, profile)
        self.report['selection'] = decision(profile, selection, variants, self.args.min_gain_pct)
        chosen = self.report['selection']['chosen']
        # This selection is persisted before any validation measurement.
        self.report['frozen_selection'] = chosen
        self.save()
        if chosen:
            finalists = {n: variants[n] for n in ('baseline', chosen)}
            validation = self.measure('validation', finalists, datasets, profile)
            self.report['validation'] = decision(profile, validation, finalists, self.args.min_gain_pct)
        self.report['clock_after'] = clock_probe()
        self.functional(variants, datasets, 'after')
        anomalies = any(s['cpu_exceeds_capacity_tolerance'] for runs in self.report['samples'].values()
                        for r in runs for v in r['samples'].values() for s in v.values())
        anomalies |= any(self.report[k]['cpu_exceeds_wall_tolerance'] or self.report[k]['inner']['cpu_exceeds_wall_tolerance']
                         for k in ('clock_before', 'clock_after'))
        self.report['accounting_anomaly'] = anomalies
        if not self.stable_identities() or anomalies:
            self.report.update(status='inconclusive', reason='identity changed or accounting anomaly; no proposal')
            return 2
        if not chosen or self.report['validation']['chosen'] != chosen:
            self.report.update(status='no_proposal', reason='no stable qualifying gain on selection and validation')
            return 0
        text = variants[chosen]['source'].read_text()
        proposed = self.build('proposal', text, files)
        if proposed is None or proposed['binary'].read_bytes() != variants[chosen]['binary'].read_bytes():
            raise ValueError('exact proposed source did not reproduce the measured binary')
        if not self.stable_identities():
            self.report.update(status='inconclusive', reason='identity changed before proposal; no proposal')
            return 2
        diff = ''.join(difflib.unified_diff(original.splitlines(keepends=True), text.splitlines(keepends=True),
                                          fromfile='a/' + self.entry.name, tofile='b/' + self.entry.name))
        (self.output / 'proposal.patch').write_text(diff)
        self.report.update(status='proposed', proposal=dict(variant=chosen, patch='proposal.patch',
            source=str(proposed['source'].relative_to(self.output)), source_sha256=digest(proposed['source']),
            binary_sha256=digest(proposed['binary']), requires_review=True))
        return 0


def run_experiment(args):
    experiment = Experiment(args)
    try:
        code = experiment.run()
    except Inconclusive as error:
        experiment.report.update(status='inconclusive', reason=str(error))
        code = 2
    except (ValueError, OSError, RuntimeError, subprocess.SubprocessError) as error:
        experiment.report.update(status='failed', reason=str(error))
        code = 1
    finally:
        experiment.report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        experiment.save()
    return code, experiment.report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('manifest', type=Path)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cpus', type=int, nargs='+', required=True)
    parser.add_argument('--repeats', type=int, default=32)
    parser.add_argument('--min-gain-pct', type=float, default=5)
    parser.add_argument('--host-note', required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if not 4 <= args.repeats <= 256 or not 0 < args.min_gain_pct < 100:
        parser.error('repeats must be 4..256; min-gain-pct must be finite and in (0,100)')
    if not args.host_note.strip():
        parser.error('host-note must describe the current load or state that it is unknown')
    if not args.cpus or not set(args.cpus) <= os.sched_getaffinity(0):
        parser.error('CPUs must be within allowed affinity')
    os.sched_setaffinity(0, args.cpus)
    try:
        code, report = run_experiment(args)
    except (OSError, ValueError) as error:
        parser.exit(1, str(error) + '\n')
    print(f'{report["status"]}: {args.output / "report.json"}')
    return code


if __name__ == '__main__':
    sys.exit(main())
