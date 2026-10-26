#!/usr/bin/env python3
"""Run the declared replacement Scion evaluation once, with immutable evidence.

This orchestration lives outside the measured source worktrees. It does not edit
source, select trials, retry failed runs, or reconstruct the lost partial batch.
Use --check to inspect preconditions and the complete plan without running any
benchmarks or creating output files. The default runs every planned stage.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


sys.dont_write_bytecode = True
ROOT = Path('/tmp/scion-metadata-compression-work')
MEMORY_BASELINE_ROOT = Path('/tmp/scion-scaling-baseline')
DURABLE_BASELINE_ROOT = Path('/tmp/scion-durable-baseline')
OUTPUT = Path('/tmp/scion-final-measurements')
WORK = Path('/tmp/scion-official-benchmarks')
STATIC_TARGET = Path('/tmp/scion-metadata-static-target')
MEMORY_TARGET = Path('/tmp/scion-metadata-bench-target')
BASELINE_DRIVER = Path('/tmp/scion-durable-baseline-target/release/examples/comparison_driver')
BASELINE_REPORT = ROOT / 'docs/benchmarks/git-comparison-2026-10-25-git-2.56-full.json'
CANDIDATE_BUILD = OUTPUT / 'scaling-candidate-build.json'
MEMORY_BASELINE_BUILD = OUTPUT / 'scaling-baseline-build.json'
PLAN_PATH = OUTPUT / 'evaluation-plan.json'
EVENTS_PATH = OUTPUT / 'evaluation-events.jsonl'
RESULT_PATH = OUTPUT / 'evaluation-result.json'
CANDIDATE_COMMIT = 'bd393c7cd064d03d231d6b0af8a05afb181f13c0'
CANDIDATE_TREE = 'db7d7d7bcd063d6c6a46babd5595e5129caf7fcf'
MEMORY_BASELINE_COMMIT = '6a11ff76052d10951e9f7d029f0313de7c78101d'
DURABLE_BASELINE_COMMIT = '78f3b0d21e2539b6fb6197e08749860ab5db1385'
BASELINE_DRIVER_SHA256 = 'ff6a44c736b135d4ec920f4d497a92fa3cac4a2936ff284a6c4abeb1ef6aa6c6'
BASELINE_REPORT_SHA256 = '9fba04c8435444111d3b033773909c88328473fc1013a15c395984b5f2dff886'
ORIGINAL_MEMORY_DRIVER_SHA256 = '5d57e2666f61a1ec99863317055ba02b4e5dd8abf12d301ae7ab5eb0a888b831'
ENVIRONMENT_KEYS = (
    'PATH', 'RUSTUP_HOME', 'CARGO_HOME', 'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL',
    'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET',
    'CARGO_BUILD_RUSTFLAGS', 'CARGO_BUILD_TARGET_DIR', 'RUSTC', 'RUSTC_WRAPPER',
    'RUSTC_WORKSPACE_WRAPPER', 'CC', 'CXX', 'CFLAGS', 'CXXFLAGS', 'LDFLAGS',
    'PYTHONDONTWRITEBYTECODE', 'LC_ALL', 'TZ', 'GIT_CONFIG_NOSYSTEM',
    'GIT_CONFIG_GLOBAL',
)
FORBIDDEN_OVERRIDES = (
    'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET',
    'CARGO_BUILD_RUSTFLAGS', 'CARGO_BUILD_TARGET_DIR', 'RUSTC', 'RUSTC_WRAPPER',
    'RUSTC_WORKSPACE_WRAPPER', 'CC', 'CXX', 'CFLAGS', 'CXXFLAGS', 'LDFLAGS',
)
ENV = None
compare_git = None
compare_scaling = None
summarize_git_runs = None


def utc():
    return datetime.now(timezone.utc).isoformat()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def file_identity(path):
    path = Path(path).resolve(strict=True)
    require(path.is_file(), f'not a regular file: {path}')
    return {'path': str(path), 'bytes': path.stat().st_size, 'sha256': sha256(path)}


def output(command, cwd=ROOT):
    return subprocess.run(command, cwd=cwd, env=ENV, check=True,
                          capture_output=True, text=True).stdout.strip()


def git(root, *arguments):
    return output(['git', '--no-optional-locks', *arguments], cwd=root)


def worktree_identity(root):
    require(Path(git(root, 'rev-parse', '--show-toplevel')).resolve() == root.resolve(),
            f'not the expected worktree root: {root}')
    return {
        'root': str(root), 'commit': git(root, 'rev-parse', 'HEAD'),
        'tree': git(root, 'rev-parse', 'HEAD^{tree}'),
        'status': git(root, 'status', '--porcelain=v1', '--untracked-files=all'),
    }


def environment():
    conflicts = [key for key in FORBIDDEN_OVERRIDES if key in os.environ]
    require(not conflicts, 'unset build overrides before running: ' + ', '.join(conflicts))
    result = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
    result.update({
        'PATH': '/tmp/scion-git-current/bin:/tmp/ferroui-macos-cargo/bin:' + os.environ.get('PATH', ''),
        'RUSTUP_HOME': '/tmp/ferroui-macos-rustup',
        'CARGO_HOME': '/tmp/ferroui-macos-cargo',
        'CARGO_TARGET_DIR': str(STATIC_TARGET),
        'CARGO_INCREMENTAL': '0', 'PYTHONDONTWRITEBYTECODE': '1',
        'LC_ALL': 'C', 'TZ': 'UTC',
        'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull,
    })
    return result


def env_record(environment_):
    # Deliberately record only build/measurement settings, never arbitrary secrets.
    return {key: environment_.get(key) for key in ENVIRONMENT_KEYS}


def load_modules():
    global compare_git, compare_scaling, summarize_git_runs
    sys.path.insert(0, str(ROOT / 'scripts'))
    import compare_git as comparison
    import compare_scaling as scaling
    import summarize_git_runs as summarizer
    compare_git, compare_scaling, summarize_git_runs = comparison, scaling, summarizer


def build_executable(report_path):
    record = json.loads(report_path.read_text())
    require(isinstance(record, dict) and isinstance(record.get('executable'), str),
            f'missing executable path in build record: {report_path}')
    driver = Path(record['executable']).resolve(strict=True)
    require(driver.is_file() and os.access(driver, os.X_OK),
            f'not an executable: {driver}')
    return driver


def collect_identity(artifacts, historical_source):
    candidate = worktree_identity(ROOT)
    memory = worktree_identity(MEMORY_BASELINE_ROOT)
    durable = worktree_identity(DURABLE_BASELINE_ROOT)
    for label, identity, commit in (
        ('candidate', candidate, CANDIDATE_COMMIT),
        ('MemoryStore baseline', memory, MEMORY_BASELINE_COMMIT),
        ('durable baseline', durable, DURABLE_BASELINE_COMMIT),
    ):
        require(identity['commit'] == commit, f'{label} source commit changed')
        require(identity['status'] == '', f'{label} source is dirty: {identity["status"]}')
    require(candidate['tree'] == CANDIDATE_TREE, 'candidate tree differs from public bd393c7')
    candidate['comparison_source'] = compare_git.source_metadata()
    candidate['scaling_source'] = compare_scaling.source_metadata(ROOT)
    memory['scaling_source'] = compare_scaling.source_metadata(MEMORY_BASELINE_ROOT)
    durable['source_sha256'] = {
        relative: sha256(DURABLE_BASELINE_ROOT / relative)
        for relative in sorted(historical_source['source_sha256'])
    }
    require(durable['source_sha256'] == historical_source['source_sha256'],
            'durable baseline source hashes differ from the historical report')
    return {
        'candidate': candidate, 'memory_baseline': memory, 'durable_baseline': durable,
        'artifacts': {label: file_identity(path) for label, path in artifacts.items()},
    }


def report_outputs(prefix):
    return [str(path) for path in compare_git.report_outputs(prefix)]


def stage(name, command, outputs, kind, *, memory=False):
    settings = ENV.copy()
    if memory:
        settings['CARGO_TARGET_DIR'] = str(MEMORY_TARGET)
    return {
        'name': name, 'kind': kind, 'command': [str(part) for part in command],
        'cwd': str(ROOT), 'environment': env_record(settings),
        'outputs': [str(path) for path in outputs],
        'log': str(OUTPUT / (name + '.log')),
    }


def create_stages(memory_baseline_driver, memory_candidate_driver):
    common = [sys.executable, ROOT / 'scripts/compare_git.py',
              '--static-crt', '--offline', '--graph-snapshot',
              '--baseline-driver', BASELINE_DRIVER, '--baseline-report', BASELINE_REPORT]
    stages = []
    default_reports = []
    for number in range(1, 6):
        prefix = OUTPUT / f'git-full-{number}'
        default_reports.append(prefix.with_suffix('.json'))
        # Retain the complete fixtures as well as every raw report.
        command = common + ['--work-root', WORK, '--keep-workdir', '--output', prefix]
        stages.append(stage(f'git-full-{number}', command, report_outputs(prefix), 'full'))
    aggregate = OUTPUT / 'git-five-full-summary.json'
    stages.append(stage('git-five-full-summary',
                        [sys.executable, ROOT / 'scripts/summarize_git_runs.py',
                         *default_reports, '--expected-trials', '5', '--output', aggregate],
                        [aggregate], 'aggregate'))
    scaling = OUTPUT / 'scaling-paired.json'
    stages.append(stage('scaling-paired',
                        [sys.executable, ROOT / 'scripts/compare_scaling.py',
                         '--baseline-root', MEMORY_BASELINE_ROOT,
                         '--baseline-driver', memory_baseline_driver,
                         '--baseline-build-report', MEMORY_BASELINE_BUILD,
                         '--candidate-root', ROOT, '--candidate-driver', memory_candidate_driver,
                         '--candidate-build-report', CANDIDATE_BUILD,
                         '--trials', '5', '--output', scaling],
                        [scaling], 'scaling', memory=True))
    large = OUTPUT / 'git-large-full'
    stages.append(stage('git-large-full',
                        common + ['--commits', '100000', '--branch-commits', '10000',
                                  '--samples', '31', '--warmup', '3',
                                  '--work-root', WORK, '--keep-workdir', '--output', large],
                        report_outputs(large), 'full'))
    replay = OUTPUT / 'git-large-queries'
    stages.append(stage('git-large-queries',
                        common + ['--reuse-report', large.with_suffix('.json'),
                                  '--samples', '31', '--warmup', '3', '--output', replay],
                        report_outputs(replay), 'replay'))
    return stages


def ensure_absent(paths):
    for path in paths:
        require(not os.path.lexists(path), f'refusing to overwrite existing output: {path}')


def preflight():
    require(output(['git', '--version']) == 'git version 2.56.0', 'Git 2.56.0 is required')
    rustc, cargo = output(['rustc', '-vV']), output(['cargo', '-vV'])
    require(rustc.startswith('rustc 1.99.1 (58cf8f9c3 2026-10-13)'),
            'the declared Rust 1.99.1 compiler is required')
    require(sha256(BASELINE_DRIVER) == BASELINE_DRIVER_SHA256,
            'rebuilt durable baseline is not byte-identical to the historical executable')
    require(sha256(BASELINE_REPORT) == BASELINE_REPORT_SHA256,
            'the historical baseline report changed')
    historical = json.loads(BASELINE_REPORT.read_text())
    require(historical['source']['commit'] == DURABLE_BASELINE_COMMIT,
            'unexpected historical baseline source commit')
    # Use the exact build-support logic that each measured command will invoke.
    _, _, static_driver, target = compare_git.comparison_build(static_crt=True, offline=True)
    require(static_driver.is_file() and os.access(static_driver, os.X_OK),
            'prebuild the static candidate before starting the fixed batch')
    linkage = compare_git.binary_linkage(static_driver, target, require_static=True)
    memory_baseline_driver = build_executable(MEMORY_BASELINE_BUILD)
    memory_candidate_driver = build_executable(CANDIDATE_BUILD)
    require(sha256(memory_baseline_driver) == ORIGINAL_MEMORY_DRIVER_SHA256,
            'rebuilt MemoryStore baseline is not byte-identical to the historical executable')
    memory_variants = {
        'baseline': compare_scaling.variant_metadata(
            MEMORY_BASELINE_ROOT, memory_baseline_driver, MEMORY_BASELINE_BUILD),
        'candidate': compare_scaling.variant_metadata(
            ROOT, memory_candidate_driver, CANDIDATE_BUILD),
    }
    artifacts = {
        'candidate_static_driver': static_driver,
        'candidate_memory_driver': memory_candidate_driver,
        'baseline_durable_driver': BASELINE_DRIVER,
        'baseline_memory_driver': memory_baseline_driver,
        'candidate_memory_build': CANDIDATE_BUILD,
        'baseline_memory_build': MEMORY_BASELINE_BUILD,
        'historical_baseline_report': BASELINE_REPORT,
        'git_binary': Path(shutil.which('git', path=ENV['PATH'])).resolve(),
        'compare_git_script': ROOT / 'scripts/compare_git.py',
        'compare_scaling_script': ROOT / 'scripts/compare_scaling.py',
        'summarizer_script': ROOT / 'scripts/summarize_git_runs.py',
        'build_support_script': ROOT / 'scripts/build_support.py',
        'orchestration_script': Path(__file__).resolve(),
    }
    identity = collect_identity(artifacts, historical['source'])
    stages = create_stages(memory_baseline_driver, memory_candidate_driver)
    outputs = [PLAN_PATH, EVENTS_PATH, RESULT_PATH]
    for item in stages:
        outputs.extend([Path(item['log']), *(Path(path) for path in item['outputs'])])
    ensure_absent(outputs)
    plan = {
        'schema': 1, 'status': 'declared', 'created_utc': utc(),
        'purpose': 'Fixed replacement evaluation of the public metadata-compression implementation.',
        'prior_batch': {
            'status': 'raw evidence unavailable after temporary environment reset',
            'last_retained_progress': 'Three trials reported complete and the start of trial four.',
            'exclusion': 'No previous partial timings are reconstructed, pooled, or used as evidence in this batch.',
        },
        'fixed_default_trials': 5, 'memory_trials': 5,
        'default_dimensions': {'files': 100000, 'commits': 10000, 'branch_commits': 1000,
                               'samples': 31, 'fanout': 100, 'blob_bytes': 1024,
                               'variants': 128, 'warmup': 3},
        'large_dimensions': {'files': 100000, 'commits': 100000, 'branch_commits': 10000,
                             'samples': 31, 'fanout': 100, 'blob_bytes': 1024,
                             'variants': 128, 'warmup': 3},
        'large_scope': 'A new full 120036-commit fixture, followed by a query replay against its new report; both reports retained.',
        'retention': 'Every raw report, command log, fixture, and control is retained; failures stop the plan without automatic retry.',
        'quiet_execution': 'Sequential commands; all prebuilds complete before the first measurement; no concurrent source edits, builds, tests, or other benchmark tasks.',
        'aggregation': 'Default ratios divide medians of five run p50 values. Memory ratios divide medians of five trial p50 values. Individual operation samples are not pooled or reconstructed.',
        'identity_guard': 'Compare complete recorded source and artifact identities before and after every stage.',
        'source_identity': identity, 'memory_build_validation': memory_variants,
        'static_linkage': linkage, 'toolchain': {'rustc': rustc, 'cargo': cargo},
        'environment': env_record(ENV),
        'stages': stages,
    }
    return plan, artifacts, historical['source']


def write_new_json(path, value):
    with Path(path).open('x', encoding='utf-8') as stream:
        json.dump(value, stream, indent=2, allow_nan=False)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())


def existing_outputs(item):
    paths = [Path(item['log']), *(Path(path) for path in item['outputs'])]
    return [file_identity(path) for path in paths if path.is_file()]


def validate_stage(item):
    for path in item['outputs']:
        require(Path(path).is_file(), f'successful command did not create output: {path}')
    report = json.loads(Path(item['outputs'][0]).read_text())
    if item['kind'] == 'full':
        summarize_git_runs.validate_report(report)
        expected = {'files': 100000, 'commits': 10000, 'branch_commits': 1000,
                    'samples': 31, 'fanout': 100, 'blob_bytes': 1024,
                    'variants': 128, 'warmup': 3}
        if item['name'] == 'git-large-full':
            expected.update(commits=100000, branch_commits=10000)
        require(report['dimensions'] == expected, 'full comparison dimensions changed')
        commit = next(row for row in report['comparisons'] if row['operation'] == 'prepared_commit')
        print(f"{item['name']} complete: prepared commit {commit['baseline_over_scion']:.3f}x baseline, "
              f"{commit['git_over_scion']:.3f}x Git", flush=True)
    elif item['kind'] == 'aggregate':
        require(report.get('status') == 'complete' and report.get('completed_trials') == 5
                and report.get('expected_trials') == 5, 'incomplete fixed-batch aggregate')
    elif item['kind'] == 'scaling':
        require(report.get('status') == 'complete' and report.get('requested_trials') == 5
                and len(report.get('trials', [])) == 5, 'incomplete MemoryStore batch')
        require(report['summary'] == compare_scaling.summarize(report['trials']),
                'MemoryStore summary differs from retained trial rows')
    elif item['kind'] == 'replay':
        original = json.loads((OUTPUT / 'git-large-full.json').read_text())
        require(report.get('mode') == 'queries_only', 'large replay did not run in queries-only mode')
        require(report['fixture'] == original['fixture'] and report['dimensions'] == original['dimensions'],
                'large replay changed fixture identities or dimensions')
        require(report['reuse']['source_report_sha256'] == sha256(OUTPUT / 'git-large-full.json'),
                'large replay source report identity differs')
        require(report['summary'] == compare_git.summarize(report['samples']),
                'large replay summary differs from raw samples')
        expected = {(operation, scope, implementation)
                    for operation, scope in summarize_git_runs.OPERATIONS.items()
                    if operation not in {'noop', 'prepared_commit'}
                    for implementation in summarize_git_runs.IMPLEMENTATIONS}
        groups = {}
        for sample in report['samples']:
            key = tuple(sample[field] for field in ('operation', 'scope', 'implementation'))
            require(key in expected, 'unexpected large replay sample group')
            require(type(sample['sample']) is int and 0 <= sample['sample'] < 31
                    and type(sample['wall_ns']) is int and sample['wall_ns'] > 0,
                    'invalid large replay observation')
            indices = groups.setdefault(key, set())
            require(sample['sample'] not in indices, 'duplicate large replay sample index')
            indices.add(sample['sample'])
        require(set(groups) == expected and all(values == set(range(31)) for values in groups.values()),
                'incomplete large replay sample groups')
        require(report['verification']['prepared_commits'] == 'not measured; queries-only reuse',
                'large replay lacks explicit prepared-commit exclusion')
    return {'report': file_identity(item['outputs'][0]), 'validation': 'passed'}


def execute(plan, artifacts, historical_source):
    OUTPUT.mkdir(parents=True, exist_ok=True)
    WORK.mkdir(parents=True, exist_ok=True)
    write_new_json(PLAN_PATH, plan)
    plan_identity = file_identity(PLAN_PATH)
    result = {'schema': 1, 'started_utc': utc(), 'status': 'running',
              'plan': plan_identity, 'stages': []}
    with EVENTS_PATH.open('x', encoding='utf-8') as events:
        event_number = 0

        def event(kind, **values):
            nonlocal event_number
            event_number += 1
            events.write(json.dumps({'sequence': event_number, 'utc': utc(),
                                     'event': kind, **values}, allow_nan=False) + '\n')
            events.flush()
            os.fsync(events.fileno())

        def guard():
            require(file_identity(PLAN_PATH) == plan_identity, 'immutable evaluation plan changed')
            observed = collect_identity(artifacts, historical_source)
            require(observed == plan['source_identity'], 'source or artifact identity changed during evaluation')
            return observed

        event('evaluation_started', plan=plan_identity)
        try:
            for number, item in enumerate(plan['stages'], 1):
                ensure_absent([item['log'], *item['outputs']])
                before = guard()
                event('stage_start', stage=number, name=item['name'],
                      command=item['command'], cwd=item['cwd'],
                      environment=item['environment'], identity_before=before)
                print(f"Starting fixed stage {number}/{len(plan['stages'])}: {item['name']}", flush=True)
                command_env = ENV.copy()
                if item['kind'] == 'scaling':
                    command_env['CARGO_TARGET_DIR'] = str(MEMORY_TARGET)
                record = {'stage': number, 'name': item['name'], 'started_utc': utc(),
                          'command': item['command'], 'cwd': item['cwd'],
                          'log': item['log'], 'environment': item['environment']}
                result['stages'].append(record)
                start = time.monotonic()
                try:
                    with Path(item['log']).open('xb') as log:
                        completed = subprocess.run(item['command'], cwd=item['cwd'],
                                                   env=command_env, stdout=log, stderr=subprocess.STDOUT,
                                                   check=False)
                    record['returncode'] = completed.returncode
                    after = guard()
                    event('identity_after', stage=number, name=item['name'], identity_after=after)
                    require(completed.returncode == 0,
                            f"{item['name']} failed with exit {completed.returncode}; see {item['log']}")
                    record['validation'] = validate_stage(item)
                    record['status'] = 'complete'
                except BaseException as error:
                    record['status'] = 'failed'
                    record['error'] = f'{type(error).__name__}: {error}'
                    # Retain even a failed command's available outputs and guard result.
                    try:
                        after = guard()
                        event('identity_after_failure', stage=number, identity_after=after)
                    except BaseException as identity_error:
                        record['identity_error'] = f'{type(identity_error).__name__}: {identity_error}'
                    raise
                finally:
                    record['finished_utc'] = utc()
                    record['elapsed_seconds'] = time.monotonic() - start
                    record['outputs'] = existing_outputs(item)
                    event('stage_finished', **record)
            guard()
            result['status'] = 'complete'
        except BaseException as error:
            result['status'] = 'failed'
            result['error'] = f'{type(error).__name__}: {error}'
            print(result['error'], file=sys.stderr, flush=True)
        finally:
            result['finished_utc'] = utc()
            event('evaluation_finished', status=result['status'],
                  completed_stages=sum(item.get('status') == 'complete' for item in result['stages']))
    result['events'] = file_identity(EVENTS_PATH)
    write_new_json(RESULT_PATH, result)
    print(f"Fixed evaluation {result['status']}: {RESULT_PATH}", flush=True)
    return 0 if result['status'] == 'complete' else 1


def main():
    global ENV
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='read-only preflight; no output files or timing runs')
    args = parser.parse_args()
    ENV = environment()
    # Imported helpers use os.environ for their read-only toolchain probes.
    os.environ.clear()
    os.environ.update(ENV)
    load_modules()
    plan, artifacts, historical_source = preflight()
    if args.check:
        print(json.dumps(plan, indent=2, allow_nan=False))
        return 0
    return execute(plan, artifacts, historical_source)


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f'Preflight rejected: {error}', file=sys.stderr)
        sys.exit(1)
