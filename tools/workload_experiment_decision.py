"""Deterministic policy over retained samples, separate from measurement."""
import math
import statistics


def summarize(profile, runs, name):
    cases = {}
    for case in profile['cases']:
        metrics = {}
        for metric in ('wall_ms', 'cpu_ms'):
            values = [r['samples'][name][case['name']][metric] for r in runs]
            if any(not math.isfinite(v) or v < 0 for v in values):
                raise ValueError('non-finite or negative timing sample')
            metrics[metric] = dict(mean=statistics.mean(values), min=min(values), max=max(values))
        cases[case['name']] = metrics
    metric = 'wall_ms' if profile['objective'] == 'elapsed' else 'cpu_ms'
    score = sum(c['weight'] * cases[c['name']][metric]['mean'] for c in profile['cases']) / sum(c['weight'] for c in profile['cases'])
    targets = all(c['target_us'] is None or cases[c['name']]['wall_ms']['mean'] * 1000 <= c['target_us'] for c in profile['cases'])
    return dict(score_ms=score, cases=cases, elapsed_mean_targets_met=targets)


def decision(profile, runs, names, minimum_gain):
    """Select once; callers pass only the frozen selection to validation."""
    halves = [runs[:len(runs) // 2], runs[len(runs) // 2:]]
    if len(runs) < 4 or len(runs) % 2:
        raise ValueError('decision requires two complete, nonempty sample halves')
    baseline = summarize(profile, runs, 'baseline')
    base_scores = [baseline['score_ms']] + [summarize(profile, half, 'baseline')['score_ms'] for half in halves]
    result = dict(baseline=baseline, candidates={}, chosen=None)
    for name in names:
        if name == 'baseline':
            continue
        candidate = summarize(profile, runs, name)
        scores = [candidate['score_ms']] + [summarize(profile, half, name)['score_ms'] for half in halves]
        gains = [(b - c) * 100 / b if b > 0 else None for b, c in zip(base_scores, scores)]
        candidate['gain_pct_all_first_second_half'] = gains
        candidate['eligible'] = candidate['elapsed_mean_targets_met'] and all(g is not None and g >= minimum_gain for g in gains)
        result['candidates'][name] = candidate
    eligible = [name for name, c in result['candidates'].items() if c['eligible']]
    if eligible:
        result['chosen'] = min(eligible, key=lambda n: (result['candidates'][n]['score_ms'], n))
    return result
