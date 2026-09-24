"""Measure native ordered phases, sequentially and with 1/2/4 worker lanes.

Linux x86-64, standard library only. Builds and correctness checks precede timed
execution; memory snapshots and optional syscall tracing are separate runs.
See docs/concurrent-benchmark.md for the scope and interpretation.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import subprocess
import sys
import tempfile
import time

from benchmark_numeric_contract import (ROOT, clock_probe, digest, distribution,
                                        rule, run_measured)

MODES = ['sequential', 'concurrent_1', 'concurrent_2', 'concurrent_4']


def source(kind, mode):
    if kind == 'readings':
        body = '@verbose 0.1.0\nuse "sequential_stack.verbose"\n'
        concept, phases = 'Reading', ['clamp', 'nonnegative', 'label']
    else:
        body = '''@verbose 0.1.0
concept Input
  @intention: "Bound the benchmark input"
  @source: numeric.intent:1
  fields:
    x : number [-100, 100]
'''
        concept, phases = 'Input', [f'p{i}' for i in range(4)]
        if kind == 'compute':
            bindings = '    let v0 = 17\n' + ''.join(
                f'    let v{i} = (v{i-1} + i.x) / 2\n' for i in range(1, 128))
            body += rule('leaf', 'v127', bindings, hint=False)
            body += '  hints:\n    overflow: [-100, 100]\n'
        for i, name in enumerate(phases):
            expression = (' + '.join(['leaf(i)'] * 96) if kind == 'compute' else 'i.x')
            body += rule(name, expression + f' + {i}',
                         calls='[leaf]' if kind == 'compute' else '[]',
                         reads='[i]' if kind == 'compute' else '[i.x]')
    body += f'''execution bench
  @intention: "Compare identical phases with ordered output and bounded storage"
  @source: numeric.intent:1
  input: {concept}
  mode: {'sequential' if mode == 'sequential' else 'concurrent'}
  phases: [{', '.join(phases)}]
  on_failure: stop
'''
    body += ('  native_stack: 1048576\n' if mode == 'sequential' else
             f'  max_in_flight: {mode.rsplit("_", 1)[1]}\n  native_memory: 1048576\n')
    return body, phases


def batch(kind, count):
    if kind == 'readings':
        rows = [{'title': 'a' if i % 2 == 0 else 'é', 'value': i % 1001}
                for i in range(count)]
        args = [v for r in rows for v in [r['title'], str(r['value'])]]
        lines = [str(min(r['value'], 100)) for r in rows]
        lines += ['true'] * count
        lines += [f'{r["title"]}:{r["value"]}' for r in rows]
    else:
        rows = [{'x': i % 201 - 100} for i in range(count)]
        args = [str(r['x']) for r in rows]
        values = []
        for row in rows:
            value = row['x']
            if kind == 'compute':
                value = 17
                for _ in range(127):
                    n = value + row['x']
                    value = (abs(n) // 2) * (-1 if n < 0 else 1)
                value *= 96
            values.append(value)
        lines = [str(value + phase) for phase in range(4) for value in values]
    return rows, args, ('\n'.join(lines) + '\n').encode()


def checked(command):
    result = subprocess.run([str(a) for a in command], capture_output=True, timeout=120, cwd=ROOT)
    if result.returncode or result.stderr:
        raise RuntimeError(f'{command[:3]}: status {result.returncode}: {result.stderr.decode()}')
    return result.stdout


def measured(command, threads):
    sample, _ = run_measured(command)
    sample = dict(sample)
    # Aggregate CPU is allowed to exceed wall time for multiple runnable threads.
    del sample['cpu_exceeds_wall_tolerance']
    capacity = min(threads, len(os.sched_getaffinity(0)))
    bound = sample['accounting_wall_ms'] * capacity
    sample['cpu_exceeds_capacity_tolerance'] = sample['cpu_ms'] > bound + max(20, bound * .05)
    return sample


def summarize(runs):
    metrics = ['wall_ms', 'cpu_ms', 'user_ms', 'system_ms', 'voluntary_switches',
               'involuntary_switches', 'minor_faults', 'major_faults']
    result = {mode: {metric: distribution([r['samples'][mode][metric] for r in runs])
                     for metric in metrics} for mode in MODES}
    for mode in MODES[1:]:
        result[mode]['paired_wall_ratio_to_sequential'] = distribution([
            r['samples'][mode]['wall_ms'] / r['samples']['sequential']['wall_ms'] for r in runs])
    return result


def parse_smaps(text):
    mappings = []
    for line in text.splitlines():
        match = re.match(r'^([0-9a-f]+)-([0-9a-f]+)\s+(\S+)\s+\S+\s+\S+\s+\d+\s*(.*)$', line)
        if match:
            mappings.append({'permissions': match[3], 'path': match[4]})
        elif mappings and ':' in line:
            key, value = line.split(':', 1)
            if key in ['Size', 'Rss', 'Pss', 'Private_Clean', 'Private_Dirty', 'Swap']:
                mappings[-1][key + '_kib'] = int(value.split()[0])
    if not mappings or any('Rss_kib' not in m for m in mappings):
        raise RuntimeError('incomplete smaps snapshot')
    return mappings


def memory_snapshot(command, expected, threads):
    # Fill an intentionally undrained pipe. Snapshot only once the coordinator
    # is blocked in pipe_write, then drain and check the entire result.
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               pipesize=4096)
    try:
        deadline = time.monotonic() + 10
        while True:
            if process.poll() is not None:
                raise RuntimeError('memory workload exited before output backpressure')
            root = Path(f'/proc/{process.pid}')
            if 'pipe_write' in (root / 'wchan').read_text():
                tasks = sorted((root / 'task').iterdir())
                if len(tasks) == threads:
                    break
            if time.monotonic() > deadline:
                raise RuntimeError('could not observe expected threads under output backpressure')
            time.sleep(.005)
        maps = parse_smaps((root / 'smaps').read_text())
        snapshot = {'threads': len(tasks), 'mappings': maps,
                    'rss_kib': sum(m['Rss_kib'] for m in maps),
                    'anonymous_size_kib': sum(m['Size_kib'] for m in maps if not m['path']),
                    'anonymous_rss_kib': sum(m['Rss_kib'] for m in maps if not m['path'])}
        output, error = process.communicate(timeout=120)
        if (process.returncode, output, error) != (0, expected, b''):
            raise RuntimeError('memory run output/status mismatch')
        return snapshot
    finally:
        if process.poll() is None:
            process.kill()
        process.communicate()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--cpus', type=int, nargs='+', required=True)
    parser.add_argument('--repeats', type=int, default=32)
    parser.add_argument('--light-records', type=int, default=4096)
    parser.add_argument('--compute-records', type=int, default=256)
    parser.add_argument('--memory-repeats', type=int, default=3)
    parser.add_argument('--host-note', default='External host load unknown')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if not set(args.cpus) <= os.sched_getaffinity(0):
        parser.error('requested CPUs outside allowed affinity')
    if not 4 <= args.repeats <= 128 or args.repeats % 4:
        parser.error('repeats must be a multiple of 4 in 4..128')
    if not 1 <= args.light_records <= 16000 or not 1 <= args.compute_records <= 4096:
        parser.error('record counts outside supported range')
    if not 1 <= args.memory_repeats <= 10:
        parser.error('memory repeats must be in 1..10')
    os.sched_setaffinity(0, args.cpus)
    work = Path(tempfile.mkdtemp(prefix='verbose-concurrent-bench-'))
    print(f'Artifacts: {work}', flush=True)
    (work / 'numeric.intent').write_text('Measure equivalent pure phases under fixed storage ceilings.\n')
    for name in ['sequential_stack.verbose', 'sequential_stack.intent']:
        (work / name).write_bytes((ROOT / 'examples' / name).read_bytes())
    compiler = args.compiler.resolve()
    report = {'schema_version': 1, 'status': 'running',
              'started_utc': datetime.now(timezone.utc).isoformat(),
              'revision': checked(['git', 'rev-parse', 'HEAD']).decode().strip(),
              'working_tree_status': checked(['git', 'status', '--short']).decode(),
              'compiler_sha256': digest(compiler), 'harness_sha256': digest(__file__),
              'helper_sha256': digest(ROOT / 'tools/benchmark_numeric_contract.py'),
              'import_sha256': {n: digest(work / n) for n in ['sequential_stack.verbose', 'sequential_stack.intent']},
              'artifacts': str(work),
              'config': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
              'host': {'kernel': platform.release(), 'affinity': sorted(os.sched_getaffinity(0)),
                       'cpu': next(l.split(':', 1)[1].strip() for l in Path('/proc/cpuinfo').read_text().splitlines()
                                   if l.startswith('model name')), 'loadavg_before': os.getloadavg(),
                       'thread_siblings': {str(c): Path(f'/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list').read_text().strip()
                                           for c in args.cpus}},
              'protocol': {'warmups': 2, 'order': 'rotated mode order, equal positions per block of four',
                           'python': platform.python_version(),
                           'perf_counter_resolution_seconds': time.get_clock_info('perf_counter').resolution,
                           'wall': 'launch + native execution + wait; prebuilt argv; stdout /dev/null',
                           'cpu': 'RUSAGE_CHILDREN delta; sum over coordinator and workers',
                           'memory': 'separate smaps snapshot while coordinator blocks on stdout; not peak RSS',
                           'hardware_cache_counters': 'not measured'},
              'builds': {}, 'cases': [], 'memory': [], 'clock_probes': []}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    try:
        # Complete all compilation and oracle checks before measurements.
        for kind in ['light', 'compute', 'readings']:
            builds = report['builds'][kind] = {}
            for mode in MODES:
                src, phases = source(kind, mode)
                path, binary = work / f'{kind}-{mode}.verbose', work / f'{kind}-{mode}'
                path.write_text(src)
                compiled = subprocess.run([compiler, path, '--native', binary],
                                          capture_output=True, timeout=120)
                if compiled.returncode:
                    raise RuntimeError(compiled.stderr.decode())
                layout = json.loads(checked([compiler, path, '--json',
                                            '--stack-report' if mode == 'sequential' else '--memory-report']))
                builds[mode] = {'source_sha256': digest(path), 'sha256': digest(binary),
                                'binary_bytes': binary.stat().st_size, 'layout': layout,
                                'compiler_stdout': compiled.stdout.decode(),
                                'compiler_stderr': compiled.stderr.decode(),
                                'threads': 1 if mode == 'sequential' else 1 + min(len(phases), int(mode.rsplit('_', 1)[1]))}
                rows, argv, expected = batch(kind, 201)
                if checked([binary, *argv]) != expected:
                    raise RuntimeError(f'native oracle mismatch: {kind}/{mode}')
                # Original-AST interpreter and independent arithmetic/output oracle.
                indices = [0, 1, 99, 100, 101, 199, 200]
                lines = expected.splitlines(keepends=True)
                selected = b''.join(lines[phase * len(rows) + i]
                                    for phase in range(len(phases)) for i in indices)
                inp = work / f'{kind}.json'
                inp.write_text(json.dumps([rows[i] for i in indices]))
                if checked([compiler, path, '--run', 'bench', '--input', inp]) != selected:
                    raise RuntimeError(f'interpreter oracle mismatch: {kind}/{mode}')
                builds[mode]['checked_records_against_oracle'] = len(rows)
                builds[mode]['checked_records_against_interpreter'] = len(indices)
            print(f'Built and checked {kind}', flush=True)
        for kind, count in [('light', 1), ('light', args.light_records),
                            ('compute', 1), ('compute', args.compute_records), ('readings', args.light_records)]:
            _, argv, expected = batch(kind, count)
            commands = {mode: [str(work / f'{kind}-{mode}'), *argv] for mode in MODES}
            for command in commands.values():
                if checked(command) != expected:
                    raise RuntimeError('full timed input oracle mismatch')
            report['cases'].append({'kind': kind, 'records': count,
                                    'expected_stdout_sha256': hashlib.sha256(expected).hexdigest(),
                                    'expected_stdout_bytes': len(expected), 'commands': commands,
                                    'warmups': {}, 'runs': []})
        report['clock_probes'].append(clock_probe())
        for case in report['cases']:
            builds = report['builds'][case['kind']]
            commands = case.pop('commands')
            for mode in MODES:
                case['warmups'][mode] = [measured(commands[mode], builds[mode]['threads']) for _ in range(2)]
            for index in range(args.repeats):
                offset = index % len(MODES)
                order = MODES[offset:] + MODES[:offset]
                case['runs'].append({'order': order, 'samples': {
                    mode: measured(commands[mode], builds[mode]['threads']) for mode in order}})
            case['summary'] = summarize(case['runs'])
            print(f'{case["kind"]}/{case["records"]}: ' + ', '.join(
                f'{mode} {case["summary"][mode]["wall_ms"]["median"]:.3f} ms' for mode in MODES), flush=True)
            save()
        report['clock_probes'].append(clock_probe())
        # Backpressure makes these snapshots repeatable; do not mix their cost
        # into timing or infer peak residency/cache hits from them.
        for kind in ['light', 'compute', 'readings']:
            _, argv, expected = batch(kind, 4096)
            for mode in MODES:
                builds = report['builds'][kind][mode]
                snapshots = [memory_snapshot([str(work / f'{kind}-{mode}'), *argv], expected,
                                             builds['threads']) for _ in range(args.memory_repeats)]
                if mode != 'sequential' and any(s['anonymous_size_kib'] * 1024 != builds['layout']['reserved_bytes'] for s in snapshots):
                    raise RuntimeError('observed anonymous reservation differs from compiler report')
                for snapshot in snapshots:
                    guards = [m for m in snapshot['mappings'] if not m['path'] and m['permissions'] == '---p']
                    if sum(m['Size_kib'] for m in guards) != (builds['threads'] - 1) * 4 or any(m['Rss_kib'] for m in guards):
                        raise RuntimeError('guard pages differ from planned size or have resident bytes')
                report['memory'].append({'kind': kind, 'mode': mode, 'records': 4096, 'snapshots': snapshots})
        inconsistent = sum(p['cpu_exceeds_wall_tolerance'] or p['inner']['cpu_exceeds_wall_tolerance']
                           for p in report['clock_probes'])
        inconsistent += sum(s['cpu_exceeds_capacity_tolerance'] for c in report['cases']
                            for values in c['warmups'].values() for s in values)
        inconsistent += sum(s['cpu_exceeds_capacity_tolerance'] for c in report['cases']
                            for r in c['runs'] for s in r['samples'].values())
        report['inconsistent_cpu_observations'] = inconsistent
        report['status'] = 'inconclusive' if inconsistent else 'ok'
    except Exception as error:
        report['status'], report['error'] = 'failed', str(error)
        raise
    finally:
        report['host']['loadavg_after'] = os.getloadavg()
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        save()
    return 2 if report['status'] == 'inconclusive' else 0


if __name__ == '__main__':
    sys.exit(main())
