#!/usr/bin/env python3
"""Independent read-only recomputation of the completed metadata evaluation."""
import collections
import csv
from datetime import datetime, timezone
import hashlib
import json
import math
from pathlib import Path
import statistics
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path('/tmp/scion-metadata-compression-work')
DATA = Path('/tmp/scion-final-measurements')
COMMIT = 'bd393c7cd064d03d231d6b0af8a05afb181f13c0'
CONTROLS = {'noop', 'process_startup', 'repository_open'}
SCOPES = {'ancestry_negative': 'fresh_process', 'ancestry_positive': 'fresh_process',
          'file_read': 'fresh_process', 'file_read_spread': 'resident',
          'merge_base_ancestor': 'fresh_process', 'merge_base_divergent': 'fresh_process',
          'noop': 'resident', 'prepared_commit': 'resident',
          'process_startup': 'fresh_process', 'repository_open': 'fresh_process',
          'small_diff': 'fresh_process'}
IMPLEMENTATIONS = ('git', 'scion', 'scion_baseline')
CHECKS = collections.Counter()


def check(condition, category, message):
    if not condition:
        raise AssertionError(f'{category}: {message}')
    CHECKS[category] += 1


def digest(data):
    return hashlib.sha256(data).hexdigest()


def sha(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def load(path):
    return json.loads(Path(path).read_text())


def verify_file(item):
    p = Path(item['path'])
    check(p.stat().st_size == item['bytes'] and sha(p) == item['sha256'],
          'recorded_file_hashes', str(p))


def git(*args):
    return subprocess.run(['git', *args], cwd=ROOT, check=True,
                          capture_output=True).stdout


def git_file_sha(commit, path):
    return digest(git('show', f'{commit}:{path}'))


def computed_summary(samples, replay=False):
    operations = set(SCOPES) - ({'noop', 'prepared_commit'} if replay else set())
    expected = {(operation, SCOPES[operation], impl)
                for operation in operations for impl in IMPLEMENTATIONS}
    groups = collections.defaultdict(list)
    indices = collections.defaultdict(set)
    for row in samples:
        key = (row['operation'], row['scope'], row['implementation'])
        check(key in expected, 'raw_sample_groups', str(key))
        check(type(row['wall_ns']) is int and row['wall_ns'] > 0,
              'raw_sample_values', str(key))
        check(type(row['sample']) is int and 0 <= row['sample'] < 31
              and row['sample'] not in indices[key], 'raw_sample_indices', str(key))
        indices[key].add(row['sample'])
        groups[key].append(row['wall_ns'])
    check(set(groups) == expected and all(v == set(range(31)) for v in indices.values()),
          'complete_sample_groups', str(len(groups)))
    result = []
    for key, observations in sorted(groups.items()):
        values = sorted(observations)
        result.append(dict(operation=key[0], scope=key[1], implementation=key[2],
                           samples=len(values), p50_us=statistics.median(values) / 1000,
                           p95_us=values[math.ceil(len(values) * .95) - 1] / 1000,
                           min_us=values[0] / 1000, max_us=values[-1] / 1000))
    return result


def computed_ratios(summary):
    groups = {(r['operation'], r['scope'], r['implementation']): r for r in summary}
    result = []
    for operation, scope in sorted({(r['operation'], r['scope']) for r in summary}):
        denominator = groups[operation, scope, 'scion']['p50_us']
        result.append(dict(operation=operation, scope=scope,
                           git_over_scion=groups[operation, scope, 'git']['p50_us'] / denominator,
                           baseline_over_scion=groups[operation, scope, 'scion_baseline']['p50_us'] / denominator))
    return result


def validate_csv(prefix, report):
    with prefix.with_suffix('.csv').open(newline='') as stream:
        rows = list(csv.DictReader(stream))
    for row in rows:
        for field in ('sample', 'wall_ns', 'internal_ns', 'output_bytes'):
            row[field] = int(row[field]) if row[field] else None
    check(rows == report['samples'], 'raw_csv_equivalence', str(prefix))
    with prefix.with_name(prefix.name + '-summary.csv').open(newline='') as stream:
        rows = list(csv.DictReader(stream))
    for row in rows:
        row['samples'] = int(row['samples'])
        for field in ('p50_us', 'p95_us', 'min_us', 'max_us'):
            row[field] = float(row[field])
    check(rows == report['summary'], 'summary_csv_equivalence', str(prefix))


def metric_rows(summary, aggregate=False):
    field = 'median_run_' if aggregate else ''
    groups = {(r['operation'], r['scope'], r['implementation']): r for r in summary}
    result = []
    for operation, scope in sorted({(r['operation'], r['scope']) for r in summary}):
        row = {'operation': operation, 'scope': scope,
               'classification': 'control' if operation in CONTROLS else 'functional'}
        for impl in IMPLEMENTATIONS:
            for percentile in ('p50', 'p95'):
                row[f'{impl}_{percentile}_us'] = groups[operation, scope, impl][field + percentile + '_us']
        for impl, label in (('git', 'git'), ('scion_baseline', 'baseline')):
            for percentile in ('p50', 'p95'):
                row[f'{label}_over_scion_{percentile}'] = row[f'{impl}_{percentile}_us'] / row[f'scion_{percentile}_us']
        result.append(row)
    return result


def main():
    plan = load(DATA / 'evaluation-plan.json')
    result = load(DATA / 'evaluation-result.json')
    events = [json.loads(line) for line in (DATA / 'evaluation-events.jsonl').read_text().splitlines()]
    index_path = ROOT / 'docs/benchmarks/metadata-2026-10-26-evidence-index.json'
    index = load(index_path)
    verify_file(result['plan'])
    verify_file(result['events'])
    check(plan['status'] == 'declared' and plan['fixed_default_trials'] == 5
          and plan['memory_trials'] == 5, 'predeclaration', 'fixed counts')
    check(datetime.fromisoformat(plan['created_utc']) < datetime.fromisoformat(result['started_utc']),
          'predeclaration', 'plan preceded measurements')
    check(result['status'] == 'complete' and len(result['stages']) == len(plan['stages']) == 9,
          'execution', 'all nine declared stages completed')
    check(len(events) == 29 and [x['sequence'] for x in events] == list(range(1, 30)),
          'execution_events', 'complete ordered event stream')
    times = [datetime.fromisoformat(event['utc']) for event in events]
    check(times == sorted(times), 'execution_events', 'timestamps are ordered')
    check(events[0]['event'] == 'evaluation_started' and events[0]['plan'] == result['plan']
          and events[-1]['event'] == 'evaluation_finished' and events[-1]['status'] == 'complete'
          and events[-1]['completed_stages'] == 9, 'execution_events', 'start/end markers')
    for number, (declared, actual) in enumerate(zip(plan['stages'], result['stages']), 1):
        check(actual['stage'] == number and actual['status'] == 'complete' and actual['returncode'] == 0,
              'execution_stages', declared['name'])
        for key in ('name', 'command', 'cwd', 'environment', 'log'):
            check(actual[key] == declared[key], 'execution_stage_plan_match', f'{number} {key}')
        start, after, finish = events[(number - 1) * 3 + 1:number * 3 + 1]
        check(start['event'] == 'stage_start' and after['event'] == 'identity_after'
              and finish['event'] == 'stage_finished', 'execution_events', f'stage {number} event types')
        check(start['identity_before'] == after['identity_after'] == plan['source_identity'],
              'source_guards', f'stage {number} unchanged source and binaries')
        actual_fields = {key: value for key, value in finish.items() if key not in {'event', 'sequence', 'utc'}}
        check(actual_fields == actual, 'execution_events', f'stage {number} final record')
        for file_ in actual['outputs']:
            verify_file(file_)
        verify_file(actual['validation']['report'])
        check(actual['validation']['validation'] == 'passed', 'execution_stages', f'{number} validation')
        if number > 1:
            check(datetime.fromisoformat(result['stages'][number - 2]['finished_utc']) <= datetime.fromisoformat(actual['started_utc']),
                  'sequential_execution', f'{number} no overlapping stages')
    measured = plan['source_identity']
    check(measured['candidate']['commit'] == COMMIT and measured['candidate']['status'] == '',
          'source_identity', 'measured public candidate')
    check(measured['candidate']['tree'] == git('rev-parse', COMMIT + '^{tree}').decode().strip(),
          'source_identity', 'measured candidate tree is the public Git tree')
    for label, value in measured['artifacts'].items():
        verify_file(value)
    candidate_source = measured['candidate']['comparison_source']
    for path, expected in candidate_source['source_sha256'].items():
        check(git_file_sha(COMMIT, path) == expected and sha(ROOT / path) == expected,
              'public_candidate_source_hashes', path)
    baseline_source = load(ROOT / 'docs/benchmarks/git-comparison-2026-10-25-git-2.56-full.json')['source']
    for path, expected in baseline_source['source_sha256'].items():
        check(git_file_sha(baseline_source['commit'], path) == expected,
              'public_baseline_source_hashes', path)
    for label in ('candidate', 'baseline'):
        variant = plan['memory_build_validation'][label]
        source, build = variant['source'], variant['build']['data']
        check(source['commit'] == build['source_commit'] and source['tree'] == build['source_tree']
              and source['dirty'] is False and build['source_status_before'] == build['source_status_after'] == ''
              and build['build_exit_code'] == 0, 'memory_build_provenance', label)
        check(sha(variant['driver']) == build['executable_sha256'] == variant['driver_sha256'],
              'memory_build_provenance', label + ' binary')
        check(git_file_sha(source['commit'], source['benchmark_path']) == source['benchmark_source_sha256']
              == build['benchmark_source_sha256'] == 'a3aabcb63f820fe2ac566c7f9b36856ec45612cda2c3604e22986ccc3ec93a00',
              'memory_build_provenance', label + ' unchanged original workload')
        check(sha(variant['build']['verified_report']) == variant['build']['sha256'],
              'memory_build_provenance', label + ' build report')

    mapping = []
    transformations = collections.Counter()
    check(index['measured_commit'] == COMMIT, 'publication_mapping', 'measured source')
    check(len({row['source'] for row in index['files']}) == len(index['files'])
          and len({row['published_path'] for row in index['files']}) == len(index['files']),
          'publication_mapping', 'unique source and destination paths')
    for row in index['files']:
        original = Path(row['source']).read_bytes()
        published = (ROOT / row['published_path']).read_bytes()
        check(len(original) == row['source_bytes'] and digest(original) == row['source_sha256']
              and len(published) == row['published_bytes'] and digest(published) == row['published_sha256'],
              'publication_hashes', row['published_path'])
        if row['transformation'] == 'CSV CRLF to LF':
            check(row['published_path'].endswith('.csv') and original.replace(b'\r\n', b'\n') == published,
                  'publication_bytes', row['published_path'])
        else:
            check(row['transformation'] == 'verbatim' and original == published,
                  'publication_bytes', row['published_path'])
        transformations[row['transformation']] += 1
        mapping.append(row['published_path'])

    reports = {}
    raw_observation_count = 0
    for name in [*(f'git-full-{i}' for i in range(1, 6)), 'git-large-full', 'git-large-queries']:
        report = load(DATA / (name + '.json'))
        replay = name.endswith('queries')
        summary = computed_summary(report['samples'], replay)
        raw_observation_count += len(report['samples'])
        check(summary == report['summary'], 'independent_percentile_recomputation', name)
        check(computed_ratios(summary) == report['comparisons'], 'independent_ratio_recomputation', name)
        check(report['source'] == candidate_source and report['source']['commit'] == COMMIT,
              'report_source_identity', name)
        check(report['build']['performed'] is True and report['build']['incremental'] is False
              and report['build']['static_crt_requested'] is True
              and report['build']['source_changed_during_build'] is False
              and report['build']['linkage']['linkage'] == 'static',
              'report_build_identity', name)
        check(report['build']['driver_sha256'] == measured['artifacts']['candidate_static_driver']['sha256']
              and report['baseline']['driver_sha256'] == measured['artifacts']['baseline_durable_driver']['sha256']
              and report['host']['git_sha256'] == measured['artifacts']['git_binary']['sha256'],
              'report_binary_identity', name)
        check(report['configuration']['git']['core.fsync'] == 'all'
              and report['configuration']['git']['core.fsyncMethod'] == 'fsync'
              and report['configuration']['scion']['sqlite'] == 'WAL, synchronous=FULL',
              'durability', name)
        fixture = report['fixture']
        check(fixture['scion'] == fixture['scion_baseline'], 'verified_fixture_identity', name)
        check(load(Path(report['work_directory']) / 'fixture.json') == fixture,
              'retained_fixture_marker', name)
        check(sha(report['graph_snapshot']['path']) == report['graph_snapshot']['sha256'],
              'retained_graph_snapshot', name)
        check(report['verification']['file_contents'] == 'byte-identical'
              and report['verification']['query_results'] == 'all checked', 'reported_correctness', name)
        validate_csv(DATA / name, report)
        reports[name] = report
    check(reports['git-large-queries']['reuse']['source_report_sha256'] == sha(DATA / 'git-large-full.json')
          and reports['git-large-queries']['fixture'] == reports['git-large-full']['fixture']
          and reports['git-large-queries']['verification']['prepared_commits'] == 'not measured; queries-only reuse',
          'large_replay_provenance', 'new full fixture reused with prepared commits excluded')

    default = [reports[f'git-full-{i}'] for i in range(1, 6)]
    aggregate = load(DATA / 'git-five-full-summary.json')
    recomputed = []
    for row_index, first in enumerate(default[0]['summary']):
        row = {key: first[key] for key in ('operation', 'scope', 'implementation')}
        row.update(run_count=5, samples_per_run=31)
        for percentile in ('p50', 'p95'):
            values = [report['summary'][row_index][percentile + '_us'] for report in default]
            row.update({f'run_{percentile}_us': values,
                        f'median_run_{percentile}_us': statistics.median(values),
                        f'min_run_{percentile}_us': min(values), f'max_run_{percentile}_us': max(values)})
        recomputed.append(row)
    check(recomputed == aggregate['summary'], 'independent_aggregate_recomputation', 'all medians/ranges')
    default_metrics = metric_rows(recomputed, aggregate=True)
    for metric, comparison in zip(default_metrics, aggregate['comparisons']):
        check(comparison['operation'] == metric['operation'] and comparison['scope'] == metric['scope'],
              'aggregate_ratio_keys', metric['operation'])
        for label in ('git', 'baseline'):
            check(comparison[label + '_over_scion_ratio_of_median_run_p50'] == metric[label + '_over_scion_p50'],
                  'independent_aggregate_ratio', metric['operation'] + ' ' + label)
            values = [next(r for r in report['comparisons'] if r['operation'] == metric['operation'])[label + '_over_scion'] for report in default]
            check(comparison[label + '_over_scion_per_run'] == {'values': values, 'min': min(values), 'max': max(values)},
                  'aggregate_run_ranges', metric['operation'] + ' ' + label)
    for number, item in enumerate(aggregate['inputs'], 1):
        check(item['path'] == str(DATA / f'git-full-{number}.json')
              and item['sha256'] == sha(item['path']), 'aggregate_input_identity', str(number))
    # The published strict validator independently checks the full shared identity/schema.
    sys.path.insert(0, str(ROOT / 'scripts'))
    import summarize_git_runs
    rebuilt = summarize_git_runs.aggregate([DATA / f'git-full-{i}.json' for i in range(1, 6)])
    for key in ('identity', 'inputs', 'runs', 'summary', 'comparisons'):
        check(rebuilt[key] == aggregate[key], 'strict_summarizer_crosscheck', key)

    scaling = load(DATA / 'scaling-paired.json')
    check(scaling['status'] == 'complete' and len(scaling['trials']) == scaling['requested_trials'] == 5,
          'memory_trials', 'five complete trials')
    expected_memory = [(op, scale, samples) for scale in (1000, 10000, 100000)
                       for op, samples in (('initial_commit', 1), ('single_file_commit', 50), ('single_file_diff', 50))]
    expected_memory += [('ancestry_positive', 10000, 100), ('ancestry_negative', 10000, 100)]
    for number, trial in enumerate(scaling['trials'], 1):
        order = ['baseline', 'candidate'] if number % 2 else ['candidate', 'baseline']
        check(trial['trial'] == number and trial['execution_order'] == order, 'memory_trial_order', str(number))
        for label in ('baseline', 'candidate'):
            run = trial['runs'][label]
            check(run['returncode'] == 0, 'memory_trial_exit', f'{number} {label}')
            lines = run['stdout'].splitlines()
            check(lines[0] == '# Scion engine benchmark; MemoryStore; release optimizations; microseconds',
                  'memory_original_workload', f'{number} {label} banner')
            rows = list(csv.DictReader(lines[1:]))
            for row in rows:
                row['scale'], row['samples'] = int(row['scale']), int(row['samples'])
                row['p50_us'], row['p95_us'] = float(row['p50_us']), float(row['p95_us'])
            check([(r['operation'], r['scale'], r['samples']) for r in rows] == expected_memory
                  and rows == run['rows'], 'memory_stdout_rows', f'{number} {label}')
    memory_metrics = []
    memory_misses = []
    for row_index, expected in enumerate(expected_memory):
        metric = dict(operation=expected[0], scale=expected[1], samples_per_trial=expected[2])
        stored = scaling['summary'][row_index]
        for label in ('baseline', 'candidate'):
            for percentile in ('p50', 'p95'):
                values = [t['runs'][label]['rows'][row_index][percentile + '_us'] for t in scaling['trials']]
                check(stored[label][f'trial_{percentile}_us'] == values
                      and stored[label][f'median_trial_{percentile}_us'] == statistics.median(values),
                      'independent_memory_medians', f'{expected} {label} {percentile}')
                metric[f'{label}_{percentile}_us'] = statistics.median(values)
        metric['baseline_over_candidate_p50'] = metric['baseline_p50_us'] / metric['candidate_p50_us']
        metric['baseline_over_candidate_p95'] = metric['baseline_p95_us'] / metric['candidate_p95_us']
        check(metric['baseline_over_candidate_p50'] == stored['speedup_ratio_of_median_trial_p50'],
              'independent_memory_ratios', str(expected))
        memory_metrics.append(metric)
        for number, trial in enumerate(scaling['trials'], 1):
            ratio = trial['runs']['baseline']['rows'][row_index]['p50_us'] / trial['runs']['candidate']['rows'][row_index]['p50_us']
            if ratio < 2:
                memory_misses.append(dict(trial=number, operation=expected[0], scale=expected[1], baseline_over_candidate_p50=ratio))

    default_per_run_misses = []
    default_per_run_tail_misses = []
    for number, report in enumerate(default, 1):
        for row in metric_rows(report['summary']):
            if row['baseline_over_scion_p50'] < 2 or row['git_over_scion_p50'] < 1:
                default_per_run_misses.append(dict(run=number, **row))
            if row['baseline_over_scion_p95'] < 2 or row['git_over_scion_p95'] < 1:
                default_per_run_tail_misses.append(dict(run=number, **row))
    large_full_metrics = metric_rows(reports['git-large-full']['summary'])
    large_replay_metrics = metric_rows(reports['git-large-queries']['summary'])
    def count_metrics(rows):
        functions = [r for r in rows if r['classification'] == 'functional']
        return {'functional_cases': len(functions),
                'functional_p50_git_wins': sum(r['git_over_scion_p50'] > 1 for r in functions),
                'functional_p50_baseline_at_least_2x': sum(r['baseline_over_scion_p50'] >= 2 for r in functions),
                'functional_p50_git_speedup_range': [min(r['git_over_scion_p50'] for r in functions), max(r['git_over_scion_p50'] for r in functions)],
                'functional_p50_baseline_speedup_range': [min(r['baseline_over_scion_p50'] for r in functions), max(r['baseline_over_scion_p50'] for r in functions)]}
    source_after = git('rev-parse', 'HEAD').decode().strip()
    audit = {
        'schema': 1, 'status': 'passed', 'created_utc': datetime.now(timezone.utc).isoformat(),
        'scope': 'Independent read-only evidence and arithmetic audit; no new benchmark timings.',
        'method': 'Raw latency percentiles, ratios, median-of-run aggregation, MemoryStore CSV rows, and trial medians recomputed independently with Python stdlib; existing strict summarizer additionally cross-checked.',
        'measured_commit': COMMIT, 'checkout_head_at_audit': source_after,
        'source_note': 'Evidence/documentation commits after the completed evaluation are permitted. Measured source hashes were verified against the original public commit and unchanged production files; current HEAD need not equal the measured revision.',
        'auditor_script': {'path': str(Path(__file__).resolve()), 'sha256': sha(__file__)},
        'checks_by_category': dict(CHECKS),
        'evidence': {'plan_sha256': sha(DATA / 'evaluation-plan.json'),
                     'events_sha256': sha(DATA / 'evaluation-events.jsonl'),
                     'result_sha256': sha(DATA / 'evaluation-result.json'),
                     'index_sha256': sha(index_path), 'default_aggregate_sha256': sha(DATA / 'git-five-full-summary.json'),
                     'memory_report_sha256': sha(DATA / 'scaling-paired.json'),
                     'large_full_sha256': sha(DATA / 'git-large-full.json'),
                     'large_replay_sha256': sha(DATA / 'git-large-queries.json')},
        'execution': {'completed_stages': 9, 'returncodes': [x['returncode'] for x in result['stages']],
                      'events': len(events), 'source_identity_guards': 18,
                      'raw_git_comparison_observations': raw_observation_count,
                      'memory_trials': 5, 'memory_cases': 11,
                      'lost_partial_batch': 'Excluded as declared; no previous partial values reconstructed.'},
        'publication': {'mapped_files': len(mapping), 'transformations': dict(transformations), 'paths': mapping},
        'default': {'summary': count_metrics(default_metrics), 'metrics': default_metrics,
                    'per_run_p50_misses_including_controls': default_per_run_misses,
                    'aggregate_p95_misses_including_controls': [r for r in default_metrics if r['baseline_over_scion_p95'] < 2 or r['git_over_scion_p95'] < 1],
                    'per_run_p95_misses_including_controls': default_per_run_tail_misses},
        'memory': {'cases': 11, 'p50_at_least_2x': sum(r['baseline_over_candidate_p50'] >= 2 for r in memory_metrics),
                   'speedup_range': [min(r['baseline_over_candidate_p50'] for r in memory_metrics), max(r['baseline_over_candidate_p50'] for r in memory_metrics)],
                   'metrics': memory_metrics, 'per_trial_p50_misses': memory_misses,
                   'initial_commit_note': 'Each initial-commit row is one observation per trial; no within-trial latency distribution exists.'},
        'large_full': {'summary': count_metrics(large_full_metrics), 'metrics': large_full_metrics,
                       'p50_misses_including_controls': [r for r in large_full_metrics if r['baseline_over_scion_p50'] < 2 or r['git_over_scion_p50'] < 1],
                       'p95_misses_including_controls': [r for r in large_full_metrics if r['baseline_over_scion_p95'] < 2 or r['git_over_scion_p95'] < 1]},
        'large_replay': {'summary': count_metrics(large_replay_metrics), 'metrics': large_replay_metrics,
                         'p50_misses_including_controls': [r for r in large_replay_metrics if r['baseline_over_scion_p50'] < 2 or r['git_over_scion_p50'] < 1],
                         'p95_misses_including_controls': [r for r in large_replay_metrics if r['baseline_over_scion_p95'] < 2 or r['git_over_scion_p95'] < 1]},
        'limits': ['These records are reproducibility evidence, not a signed attestation of the host or absence of external host contention.',
                   'Default p50 aggregate gains do not establish every-run or every-tail gains; all below-target controls and observations are listed.',
                   'Synthetic warm-cache workload; explicit graph preparation and static candidate linkage are part of the declared comparison.'],
    }
    target = DATA / 'evidence-audit.json'
    with target.open('x', encoding='utf-8') as stream:
        json.dump(audit, stream, indent=2, allow_nan=False)
        stream.write('\n')
    print(json.dumps({'status': audit['status'], 'audit': str(target), 'sha256': sha(target),
                      'checks': sum(CHECKS.values()), 'default': audit['default']['summary'],
                      'memory': {k: audit['memory'][k] for k in ('cases', 'p50_at_least_2x', 'speedup_range')},
                      'large_full': audit['large_full']['summary'], 'large_replay': audit['large_replay']['summary'],
                      'publication': {'count': len(mapping), 'transformations': dict(transformations)}}, indent=2))


if __name__ == '__main__':
    main()
