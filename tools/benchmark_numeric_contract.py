"""Compare strict numeric native emission on Linux x86-64.

Checks output/exit parity before timing identical argv records. Timings include
process startup, decimal parsing and writes to /dev/null. Frame sizes describe
reserved stack storage, not process RSS or measured hardware cache residency.
Only the Python standard library and two supplied compiler binaries are needed.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import struct
import subprocess
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
    return {'constants': header + constants, 'arithmetic': header + arithmetic,
            'calls': header + calls}


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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--compiler', type=Path, default=ROOT / 'target/release/verbosec')
    parser.add_argument('--reference-compiler', type=Path, required=True)
    parser.add_argument('--reference-revision', required=True)
    parser.add_argument('--records', type=int, default=16000)
    parser.add_argument('--repeats', type=int, default=11)
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
        'schema_version': 1, 'status': 'running', 'started_utc': datetime.now(timezone.utc).isoformat(),
        'revision': execute(['git', 'rev-parse', 'HEAD'], capture_output=True, text=True).stdout.strip(),
        'working_tree_status': execute(['git', 'status', '--short'], capture_output=True, text=True).stdout,
        'compiler_sha256': {label: digest(path) for label, path in compilers.items()},
        'harness_sha256': digest(__file__), 'artifacts': str(work),
        'config': {k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()},
        'host': {'kernel': platform.release(), 'affinity': sorted(os.sched_getaffinity(0)),
                 'cpu': next(line.split(':', 1)[1].strip() for line in Path('/proc/cpuinfo').read_text().splitlines()
                             if line.startswith('model name')), 'loadavg_before': os.getloadavg()},
        'cache_counters': 'not measured', 'cases': [],
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)

    def save():
        args.output.write_text(json.dumps(report, indent=2) + '\n')

    values = list(range(-100, 101))
    records = [values[i % len(values)] for i in range(args.records)]
    try:
        for name, source in fixtures().items():
            path = work / f'{name}.verbose'
            path.write_text(source)
            row = {'name': name, 'source_sha256': digest(path), 'builds': {}, 'runs_ms': []}
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
            for binary in binaries.values():
                for _ in range(2):
                    execute([binary, *records], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True)
            for repeat in range(args.repeats):
                sample = {}
                order = ('before', 'after') if repeat % 2 == 0 else ('after', 'before')
                for label in order:
                    start = time.perf_counter_ns()
                    execute([binaries[label], *records], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True)
                    sample[label] = (time.perf_counter_ns() - start) / 1e6
                row['runs_ms'].append(sample)
            row['summary'] = {}
            for label in compilers:
                times = [run[label] for run in row['runs_ms']]
                median = statistics.median(times)
                row['summary'][label] = {'median_ms': median, 'min_ms': min(times), 'max_ms': max(times),
                                         'mad_ms': statistics.median(abs(t - median) for t in times)}
            save()
            print(name, row['builds'], row['summary'], flush=True)
        report['status'] = 'ok'
    except Exception as error:
        report['status'] = 'failed'
        report['error'] = str(error)
        raise
    finally:
        report['host']['loadavg_after'] = os.getloadavg()
        report['finished_utc'] = datetime.now(timezone.utc).isoformat()
        save()


if __name__ == '__main__':
    main()
