"""Materialize an unbatched reading workload and independent experiment batches."""
import argparse
import json
from pathlib import Path

from benchmark_numeric_contract import ROOT


def create(output):
    output.mkdir(parents=True, exist_ok=False)
    for name in ['workload_profile.verbose', 'workload_profile.intent', 'sequential_stack.verbose', 'sequential_stack.intent']:
        (output / name).write_bytes((ROOT / 'examples' / name).read_bytes())
    source = output / 'workload_profile.verbose'
    # An explicit unbatched baseline, not a claim to improve the already-batched
    # repository example. Preserve the source's existing 20 KiB ceiling.
    source.write_text(source.read_text().replace('result_batch: 32', 'result_batch: 1'))
    cases = {}
    for name, count in [('interactive', 1), ('bulk', 4096)]:
        cases[name] = {}
        for stage in ['selection', 'validation']:
            filename = f'{name}-{stage}.json'
            cases[name][stage] = filename
            rows = [dict(title=('a' if i % 2 == 0 else 'é') if stage == 'selection' else ('b' if i % 2 == 0 else 'ø'),
                         value=i % 1001 if stage == 'selection' else 1000 - i % 1001) for i in range(count)]
            (output / filename).write_text(json.dumps(rows, ensure_ascii=False) + '\n')
    checks = [[], [dict(title='edge', value=n) for n in [0, 100, 2**63 - 1]],
              [dict(title='é', value=n) for n in [1, -(2**63), 3]],
              [dict(title='123456789', value=1)]]
    for i, rows in enumerate(checks):
        (output / f'check-{i}.json').write_text(json.dumps(rows) + '\n')
    manifest = dict(schema_version=1, source=source.name, execution='inspect_usage', budgets=dict(native_stack=192),
        variants=[dict(name='sequential', mode='sequential')] +
                 [dict(name=f'batch{b}', mode='concurrent', max_in_flight=2, result_batch=b) for b in [8, 32, 64, 128]],
        cases=cases, checks=[f'check-{i}.json' for i in range(len(checks))],
        argv_checks=[[], ['a'], ['a', '1', 'b', 'bad', 'c', '3'], ['a', str(2**63)], ['a', '1', 'b']])
    (output / 'experiment.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    create(parser.parse_args().output)
