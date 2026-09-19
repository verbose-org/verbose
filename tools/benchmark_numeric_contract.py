"""Compare strict numeric native emission on Linux x86-64.

Checks output/exit parity before timing identical argv records. Reports elapsed
time and child CPU time separately, including process startup, decimal parsing
and writes to /dev/null. Argument strings are prepared before timing. Frame sizes
describe reserved stack storage, not RSS or measured hardware cache residency.
Only the Python standard library and two supplied compiler binaries are needed.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import struct
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def rule(name, expression, bindings='', calls='[]', reads='[i.x]', hint=True):
    return f'''rule {name}
  @intention: "Measure verified scalar arithmetic"
  @source: numeric.intent:1
  input:
    i : Input
  output:
    out : number
  logic:
{bindings}    out = {expression}
  proofs:
    purity:
      reads : {reads}
      calls : {calls}
    termination:
      bound : 10000
''' + ('  hints:\n    overflow : [-1000000000, 1000000000]\n' if hint else '')


def fixtures():
    header = '''@verbose 0.1.0
concept Input
  @intention: "Bound the benchmark input"
  @source: numeric.intent:1
  fields:
    x : number [-100, 100]
'''
    lets = '    let k0 = 12 / -5\n' + ''.join(
        f'    let k{i} = (k{i - 1} * 3 + 17) / 2\n' for i in range(1, 24))
    constants = rule('entry', 'if i.x > 100 then 0 else i.x + k23', lets)
    expression = 'i.x' + ' + i.x' * 23
    arithmetic = rule('entry', expression)
    leaf = rule('leaf', expression)
    expression = ' + '.join(['leaf(i)'] * 48)
    calls = leaf + rule('entry', f'if i.x < 0 then {expression} else -({expression})',
                       calls='[leaf]', reads='[i, i.x]', hint=False)
    lets = '    let v0 = i.x\n' + ''.join(
        f'    let v{i} = v{i - 1} + i.x\n' for i in range(1, 128))
    locals_case = rule('entry', 'v127', lets)
    local_leaf = rule('leaf', 'v127', lets)
    local_calls = local_leaf + rule('entry', expression, calls='[leaf]', reads='[i]', hint=False)
    live_lets = ''.join(f'    let v{i} = i.x + {i}\n' for i in range(128))
    live_locals = rule('entry', ' + '.join(f'v{i}' for i in range(128)), live_lets)
    return {'constants': header + constants, 'arithmetic': header + arithmetic,
            'calls': header + calls, 'locals': header + locals_case,
            'local_calls': header + local_calls, 'live_locals': header + live_locals}


def frame_bytes(path):
    blob = path.read_bytes()
    # First numeric _start frame: push rbp; mov rbp,rsp; sub rsp,imm32.
    prologue = bytes.fromhex('554889e54881ec')
    at = blob.find(prologue)
    if at < 0:
        raise RuntimeError(f'no expected argv frame in {path}')
    return struct.unpack_from('<I', blob, at + len(prologue))[0]


def execute(args, **kwargs):
    return subprocess.run([str(a) for a in args], timeout=120, **kwargs)


def signature(result):
    return result.returncode, result.stdout, result.stderr


def cpu_exceeds_wall(cpu_ms, wall_ms):
    # A deliberately generous diagnostic tolerance, not an accuracy guarantee:
    # allow 20 ms for short samples or 5% for longer ones.
    return cpu_ms > wall_ms + max(20, wall_ms * .05)


def run_measured(command, *, timeout=120, stdout=subprocess.DEVNULL):
    """One serial, waited-for child; no other children run in this interval."""
    accounting_start = time.perf_counter_ns()
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.perf_counter_ns()
    result = subprocess.run(command, stdout=stdout, stderr=subprocess.PIPE,
                            check=True, timeout=timeout)
    wall_ms = (time.perf_counter_ns() - start) / 1e6
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    accounting_wall_ms = (time.perf_counter_ns() - accounting_start) / 1e6
    if result.stderr:
        raise RuntimeError(f'timed native execution wrote stderr: {result.stderr!r}')
    user_ms = (after.ru_utime - before.ru_utime) * 1000
    system_ms = (after.ru_stime - before.ru_stime) * 1000
    sample = {'wall_ms': wall_ms, 'accounting_wall_ms': accounting_wall_ms,
              'user_ms': user_ms, 'system_ms': system_ms,
              'cpu_ms': user_ms + system_ms}
    for key, attr in [('voluntary_switches', 'ru_nvcsw'), ('involuntary_switches', 'ru_nivcsw'),
                      ('minor_faults', 'ru_minflt'), ('major_faults', 'ru_majflt')]:
        sample[key] = getattr(after, attr) - getattr(before, attr)
    if any(v < 0 for v in sample.values()):
        raise RuntimeError(f'non-monotonic child accounting: {sample}')
    sample['cpu_exceeds_wall_tolerance'] = cpu_exceeds_wall(sample['cpu_ms'], accounting_wall_ms)
    return sample, result.stdout


def measure(command, *, timeout=120):
    return run_measured(command, timeout=timeout)[0]


def clock_probe():
    """A single-thread probe independent of the compiler and its native output."""
    code = '''import json,time
wall = time.perf_counter()
cpu = time.process_time()
while time.perf_counter() - wall < .3:
    pass
print(json.dumps({"wall_ms": (time.perf_counter()-wall)*1000,
                  "cpu_ms": (time.process_time()-cpu)*1000}))
'''
    sample, output = run_measured([sys.executable, '-I', '-S', '-c', code], stdout=subprocess.PIPE)
    sample['inner'] = json.loads(output)
    sample['inner']['cpu_exceeds_wall_tolerance'] = cpu_exceeds_wall(
        sample['inner']['cpu_ms'], sample['inner']['wall_ms'])
    return sample


def distribution(values):
    median = statistics.median(values)
    return {'median': median, 'min': min(values), 'max': max(values),
            'mad': statistics.median(abs(v - median) for v in values)}


def summarize(runs):
    summary = {}
    for metric in ['wall_ms', 'cpu_ms', 'user_ms', 'system_ms']:
        summary[metric] = {
            label: distribution([run[label][metric] for run in runs])
            for label in ['before', 'after']
        }
        # Keep paired comparisons. A zero CPU reading has no defined ratio;
        # never silently drop it or fabricate a percentage.
        summary[metric]['paired_change_pct'] = (
            distribution([(run['after'][metric] / run['before'][metric] - 1) * 100
                          for run in runs])
            if all(run['before'][metric] > 0 for run in runs) else None
        )
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--reference-compiler', type=Path, required=True)
    parser.add_argument('--reference-revision', required=True)
    parser.add_argument('--records', type=int, default=16000)
    parser.add_argument('--repeats', type=int, default=32,
                        help='Paired samples; use an even count to balance AB/BA order (default: 32)')
    parser.add_argument('--cases', nargs='+', choices=fixtures().keys(),
                        help='Selected workloads; default: all')
    parser.add_argument('--cpu', type=int)
    parser.add_argument('--host-note', default='External host activity not independently measured')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != 'Linux' or platform.machine() != 'x86_64':
        parser.error('Linux x86-64 required')
    if not 1 <= args.records <= 32000 or not 3 <= args.repeats <= 101:
        parser.error('records must be in 1..32000; repeats in 3..101')
    if args.cpu is not None:
        if args.cpu not in os.sched_getaffinity(0):
            parser.error('CPU outside current affinity')
        os.sched_setaffinity(0, {args.cpu})
    work = Path(tempfile.mkdtemp(prefix='verbose-numeric-bench-'))
    print(f'Artifacts: {work}', flush=True)
    (work / 'numeric.intent').write_text('Measure pure arithmetic under enforced input bounds.\n')
    compilers = {'before': args.reference_compiler.resolve(), 'after': args.compiler.resolve()}
    report = {
        'schema_version': 2, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
        'revision': execute(['git', 'rev-parse', 'HEAD'], cwd=ROOT, capture_output=True, text=True).stdout.strip(),
        'working_tree_status': execute(['git', 'status', '--short'], cwd=ROOT, capture_output=True, text=True).stdout,
        'compiler_sha256': {label: digest(path) for label, path in compilers.items()},
        'harness_sha256': digest(__file__), 'artifacts': str(work),
        'timing': {'wall': 'process launch, execution and wait; argument strings prepared beforehand',
                   'cpu': 'RUSAGE_CHILDREN delta after one serial child has terminated and been waited for',
                   'order': 'alternating AB/BA pairs', 'balanced_order': args.repeats % 2 == 0,
                   'cpu_consistency_tolerance': 'max(20 ms, 5% of enclosing wall interval); diagnostic only',
                   'python': platform.python_version(),
                   'perf_counter_resolution_seconds': time.get_clock_info('perf_counter').resolution},
        'config': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        'host': {'kernel': platform.release(), 'affinity': sorted(os.sched_getaffinity(0)),
                 'cpu': next(line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
                             if line.startswith('model name')), 'loadavg_before': os.getloadavg()},
        'cache_counters': 'not measured', 'clock_probes': [], 'cases': [],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')

    values = list(range(-100, 101))
    records = [str(values[i % len(values)]) for i in range(args.records)]
    try:
        report['clock_probes'].append(clock_probe())
        for name, source in fixtures().items():
            if args.cases and name not in args.cases:
                continue
            path = work / f'{name}.verbose'
            path.write_text(source)
            row = {'name': name, 'source_sha256': digest(path), 'builds': {}, 'runs': []}
            report['cases'].append(row)
            binaries = {}
            for label, compiler in compilers.items():
                binary = work / f'{name}-{label}'
                built = execute([compiler, path, '--run', 'entry', '--native', binary], capture_output=True)
                (work / f'{name}-{label}.compile.stdout').write_bytes(built.stdout)
                (work / f'{name}-{label}.compile.stderr').write_bytes(built.stderr)
                if built.returncode:
                    raise RuntimeError(f'{name}/{label}: {built.stderr.decode()}')
                binaries[label] = binary
                row['builds'][label] = {'frame_bytes': frame_bytes(binary), 'binary_bytes': binary.stat().st_size,
                                        'sha256': digest(binary)}
            reference = execute([binaries['before'], *values], capture_output=True)
            candidate = execute([binaries['after'], *values], capture_output=True)
            if signature(reference) != signature(candidate) or reference.returncode != 0:
                raise RuntimeError(f'{name}: native value/status mismatch')
            inputs = work / f'{name}.json'
            inputs.write_text(json.dumps([{'x': n} for n in values]))
            interpreted = execute([compilers['after'], path, '--run', 'entry', '--input', inputs, '--json'],
                                  capture_output=True)
            if interpreted.returncode or [r['out'] for r in json.loads(interpreted.stdout)] != [int(v) for v in candidate.stdout.splitlines()]:
                raise RuntimeError(f'{name}: interpreter mismatch')
            for bad in [[], [101], [-101], ['bad'], ['9223372036854775808'], [1, 'bad']]:
                a = execute([binaries['before'], *bad], capture_output=True)
                b = execute([binaries['after'], *bad], capture_output=True)
                if signature(a) != signature(b) or b.returncode != 1:
                    raise RuntimeError(f'{name}: entry failure mismatch for {bad}')
            row['checked_values'] = len(values)
            row['entry_failures_agree'] = True
            row['identical_binary'] = row['builds']['before']['sha256'] == row['builds']['after']['sha256']
            commands = {label: [str(binary), *records] for label, binary in binaries.items()}
            row['warmups'] = {label: [measure(command) for _ in range(2)]
                              for label, command in commands.items()}
            for repeat in range(args.repeats):
                order = ('before', 'after') if repeat % 2 == 0 else ('after', 'before')
                sample = {'order': list(order)}
                for label in order:
                    sample[label] = measure(commands[label])
                row['runs'].append(sample)
            row['summary'] = summarize(row['runs'])
            save()
            print(name, row['builds'], {k: row['summary'][k] for k in ['wall_ms', 'cpu_ms']}, flush=True)
        report['clock_probes'].append(clock_probe())
        inconsistent_probes = sum(p['cpu_exceeds_wall_tolerance'] or
                                  p['inner']['cpu_exceeds_wall_tolerance'] for p in report['clock_probes'])
        inconsistent_samples = sum(run[label]['cpu_exceeds_wall_tolerance']
                                   for row in report['cases'] for run in row['runs']
                                   for label in ['before', 'after'])
        inconsistent_warmups = sum(sample['cpu_exceeds_wall_tolerance']
                                   for row in report['cases'] for samples in row['warmups'].values()
                                   for sample in samples)
        report['cpu_consistency'] = {'inconsistent_probes': inconsistent_probes,
                                     'inconsistent_samples': inconsistent_samples,
                                     'inconsistent_warmups': inconsistent_warmups}
        report['status'] = 'inconclusive' if any(report['cpu_consistency'].values()) else 'ok'
    except Exception as error:
        report['status'] = 'failed'
        report['error'] = str(error)
        raise
    finally:
        report['host']['loadavg_after'] = os.getloadavg()
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        save()
    if report['status'] == 'inconclusive':
        print('CPU accounting exceeds the single-thread wall-time tolerance. '
              'Raw data retained; CPU comparisons are inconclusive.', file=sys.stderr)
        raise SystemExit(2)


if __name__ == '__main__':
    main()
