"""Compare native result batches against fresh sequential/default controls.

Requires Linux x86-64, Python standard library, candidate and reference compilers.
No compilation, correctness checks or memory snapshots overlap timed execution.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import sys
import tempfile

from benchmark_concurrent import batch, checked, measured, memory_snapshot, source
from benchmark_numeric_contract import ROOT, clock_probe, digest, distribution

MODES = ['sequential', 'batch_1', 'batch_8', 'batch_32', 'batch_128']


def fixture(kind, mode):
    text, phases = source(kind, 'sequential' if mode == 'sequential' else 'concurrent_4')
    if mode not in ['sequential', 'batch_1']:
        text += f'  result_batch: {mode.split("_")[1]}\n'
    return text, phases


def summary(runs):
    result = {m: {k: distribution([r['samples'][m][k] for r in runs])
                  for k in ['wall_ms', 'cpu_ms', 'user_ms', 'system_ms',
                            'voluntary_switches', 'involuntary_switches', 'minor_faults', 'major_faults']}
              for m in MODES}
    for mode in MODES:
        for reference in ['sequential', 'batch_1']:
            result[mode]['paired_wall_ratio_to_' + reference] = distribution([
                r['samples'][mode]['wall_ms'] / r['samples'][reference]['wall_ms'] for r in runs])
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--reference-compiler', type=Path, required=True)
    parser.add_argument('--reference-revision', required=True)
    parser.add_argument('--cpus', type=int, nargs='+', required=True)
    parser.add_argument('--repeats', type=int, default=30)
    parser.add_argument('--light-records', type=int, default=4096)
    parser.add_argument('--compute-records', type=int, default=256)
    parser.add_argument('--memory-records', type=int, default=4096)
    parser.add_argument('--memory-repeats', type=int, default=3)
    parser.add_argument('--host-note', default='External host load unknown')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if not set(args.cpus) <= os.sched_getaffinity(0):
        parser.error('CPUs outside current affinity')
    if not 5 <= args.repeats <= 100 or args.repeats % 5:
        parser.error('repeats must be a multiple of 5 in 5..100')
    if not 1 <= args.light_records <= 16000 or not 1 <= args.compute_records <= 4096:
        parser.error('unsupported record counts')
    if not 4096 <= args.memory_records <= 16000 or not 1 <= args.memory_repeats <= 10:
        parser.error('memory records must be in 4096..16000, repeats in 1..10')
    os.sched_setaffinity(0, args.cpus)
    compiler, reference = args.compiler.resolve(), args.reference_compiler.resolve()
    work = Path(tempfile.mkdtemp(prefix='verbose-result-batches-bench-'))
    print(f'Artifacts: {work}', flush=True)
    (work / 'numeric.intent').write_text('Compare bounded pure phase result batches.\n')
    for name in ['sequential_stack.verbose', 'sequential_stack.intent']:
        (work / name).write_bytes((ROOT / 'examples' / name).read_bytes())
    report = {'schema_version': 1, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
              'revision': checked(['git', 'rev-parse', 'HEAD']).decode().strip(),
              'working_tree_status': checked(['git', 'status', '--short']).decode(),
              'compiler_sha256': digest(compiler), 'reference_compiler_sha256': digest(reference),
              'source_diff_sha256': hashlib.sha256(checked(['git', 'diff', '--', 'src'])).hexdigest(),
              'harness_sha256': digest(__file__),
              'helper_sha256': {n: digest(ROOT / 'tools' / n) for n in ['benchmark_concurrent.py', 'benchmark_numeric_contract.py']},
              'import_sha256': {n: digest(work / n) for n in ['sequential_stack.verbose', 'sequential_stack.intent']},
              'config': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              'artifacts': str(work), 'builds': {}, 'cases': [], 'memory': [], 'clock_probes': [],
              'host': {'kernel': platform.release(), 'affinity': sorted(os.sched_getaffinity(0)),
                       'python': platform.python_version(), 'loadavg_before': os.getloadavg(),
                       'cpu': next(l.split(':', 1)[1].strip() for l in Path('/proc/cpuinfo').read_text().splitlines()
                                   if l.startswith('model name')),
                       'thread_siblings': {str(c): Path(f'/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list').read_text().strip()
                                           for c in args.cpus}},
              'protocol': {'warmups': 2, 'order': 'rotating order over five modes, equal positions',
                           'wall': 'launch + execution + wait; prebuilt argv; stdout /dev/null',
                           'cpu': 'RUSAGE_CHILDREN delta, coordinator plus workers; capacity consistency check',
                           'memory': 'separate smaps snapshots under stdout backpressure, not peak RSS',
                           'hardware_cache_counters': 'not measured'}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    try:
        for kind in ['light', 'compute', 'readings']:
            builds = report['builds'][kind] = {}
            for mode in MODES:
                text, phases = fixture(kind, mode)
                path, binary = work / f'{kind}-{mode}.verbose', work / f'{kind}-{mode}'
                path.write_text(text)
                compiled = subprocess.run([compiler, path, '--native', binary], capture_output=True, timeout=120)
                if compiled.returncode:
                    raise RuntimeError(compiled.stderr.decode())
                layout = json.loads(checked([compiler, path, '--json',
                                            '--stack-report' if mode == 'sequential' else '--memory-report']))
                build = builds[mode] = {'source_sha256': digest(path), 'sha256': digest(binary),
                                       'binary_bytes': binary.stat().st_size, 'layout': layout,
                                       'compiler_stdout': compiled.stdout.decode(), 'compiler_stderr': compiled.stderr.decode(),
                                       'threads': 1 if mode == 'sequential' else 1 + min(4, len(phases))}
                if mode in ['sequential', 'batch_1']:
                    old = work / f'{kind}-{mode}-reference'
                    built = subprocess.run([reference, path, '--native', old], capture_output=True, timeout=120)
                    if built.returncode or old.read_bytes() != binary.read_bytes():
                        raise RuntimeError(f'{kind}/{mode}: default bytes changed: {built.stderr.decode()}')
                    build['reference_sha256'] = digest(old)
                rows, argv, expected = batch(kind, 201)
                if checked([binary, *argv]) != expected:
                    raise RuntimeError('independent oracle mismatch')
                indices = [0, 1, 99, 100, 101, 199, 200]
                lines = expected.splitlines(keepends=True)
                expected = b''.join(lines[p * len(rows) + i] for p in range(len(phases)) for i in indices)
                inp = work / f'{kind}.json'
                inp.write_text(json.dumps([rows[i] for i in indices]))
                if checked([compiler, path, '--run', 'bench', '--input', inp]) != expected:
                    raise RuntimeError('original interpreter mismatch')
                build['checked_oracle_records'], build['checked_interpreter_records'] = 201, 7
            print(f'Built and checked {kind}', flush=True)
        commands = []
        for kind, count in [('light', 1), ('light', args.light_records), ('compute', 1),
                            ('compute', args.compute_records), ('readings', args.light_records)]:
            _, argv, expected = batch(kind, count)
            commands.append({m: [str(work / f'{kind}-{m}'), *argv] for m in MODES})
            for command in commands[-1].values():
                if checked(command) != expected:
                    raise RuntimeError('full timed input oracle mismatch')
            report['cases'].append({'kind': kind, 'records': count, 'runs': [], 'warmups': {},
                                    'expected_stdout_sha256': hashlib.sha256(expected).hexdigest(),
                                    'expected_stdout_bytes': len(expected)})
        report['clock_probes'].append(clock_probe())
        for case, command in zip(report['cases'], commands):
            for mode in MODES:
                threads = report['builds'][case['kind']][mode]['threads']
                case['warmups'][mode] = [measured(command[mode], threads) for _ in range(2)]
            for i in range(args.repeats):
                offset = i % len(MODES)
                order = MODES[offset:] + MODES[:offset]
                case['runs'].append({'order': order, 'samples': {
                    m: measured(command[m], report['builds'][case['kind']][m]['threads']) for m in order}})
            case['summary'] = summary(case['runs'])
            print(f'{case["kind"]}/{case["records"]}: ' + ', '.join(
                f'{m} {case["summary"][m]["wall_ms"]["median"]:.3f} ms' for m in MODES), flush=True)
            save()
        report['clock_probes'].append(clock_probe())
        for kind in ['light', 'compute', 'readings']:
            _, argv, expected = batch(kind, args.memory_records)
            for mode in MODES:
                build = report['builds'][kind][mode]
                samples = [memory_snapshot([str(work / f'{kind}-{mode}'), *argv], expected, build['threads'])
                           for _ in range(args.memory_repeats)]
                for snapshot in samples:
                    if mode != 'sequential' and snapshot['anonymous_size_kib'] * 1024 != build['layout']['reserved_bytes']:
                        raise RuntimeError('memory reservation differs from compiler report')
                    guards = [m for m in snapshot['mappings'] if not m['path'] and m['permissions'] == '---p']
                    if sum(m['Size_kib'] for m in guards) != (build['threads'] - 1) * 4 or any(m['Rss_kib'] for m in guards):
                        raise RuntimeError('guard size/residency mismatch')
                report['memory'].append({'kind': kind, 'mode': mode, 'records': args.memory_records, 'snapshots': samples})
        anomalies = sum(p['cpu_exceeds_wall_tolerance'] or p['inner']['cpu_exceeds_wall_tolerance'] for p in report['clock_probes'])
        anomalies += sum(s['cpu_exceeds_capacity_tolerance'] for c in report['cases'] for v in c['warmups'].values() for s in v)
        anomalies += sum(s['cpu_exceeds_capacity_tolerance'] for c in report['cases'] for r in c['runs'] for s in r['samples'].values())
        report['inconsistent_cpu_observations'] = anomalies
        report['status'] = 'inconclusive' if anomalies else 'ok'
    except Exception as e:
        report['status'], report['error'] = 'failed', str(e)
        raise
    finally:
        report['host']['loadavg_after'] = os.getloadavg()
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        save()
    return 2 if report['status'] == 'inconclusive' else 0


if __name__ == '__main__':
    sys.exit(main())
