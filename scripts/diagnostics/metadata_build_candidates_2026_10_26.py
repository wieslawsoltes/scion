#!/usr/bin/env python3
"""Rebuild clean public benchmark candidates with explicit provenance."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

ROOT = Path('/tmp/scion-metadata-compression-work')
OUT = Path('/tmp/scion-final-measurements')
OUT.mkdir(exist_ok=True)
BASE = dict(os.environ)
BASE.update(PATH='/tmp/ferroui-macos-cargo/bin:' + BASE['PATH'],
            RUSTUP_HOME='/tmp/ferroui-macos-rustup',
            CARGO_HOME='/tmp/ferroui-macos-cargo', CARGO_INCREMENTAL='0')
KEYS = ('PATH', 'RUSTUP_HOME', 'CARGO_HOME', 'CARGO_TARGET_DIR', 'CARGO_INCREMENTAL',
        'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET',
        'CARGO_BUILD_RUSTFLAGS', 'RUSTC', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER',
        'CC', 'CXX', 'CFLAGS', 'CXXFLAGS', 'LDFLAGS')

def output(*command):
    return subprocess.check_output(command, cwd=ROOT, env=BASE, text=True).strip()

def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

for label, target, command, flags in (
    ('static-candidate-build', '/tmp/scion-metadata-static-target',
     ['cargo', 'build', '--release', '--locked', '-p', 'scion-engine', '--example',
      'comparison_driver', '--target', 'x86_64-unknown-linux-gnu', '--offline'],
     {'RUSTFLAGS': '-C target-feature=+crt-static'}),
    ('scaling-candidate-build', '/tmp/scion-metadata-bench-target',
     ['cargo', 'bench', '--locked', '--offline', '-p', 'scion-engine', '--bench',
      'scaling', '--no-run'], {}),
):
    report = OUT / (label + '.json')
    log = OUT / (label + '.log')
    if report.exists() or log.exists():
        raise RuntimeError('Refusing to overwrite existing candidate build evidence')
    environment = dict(BASE, CARGO_TARGET_DIR=target, **flags)
    before = output('git', 'status', '--porcelain=v1', '--untracked-files=all')
    commit = output('git', 'rev-parse', 'HEAD')
    if before or commit != 'bd393c7cd064d03d231d6b0af8a05afb181f13c0':
        raise RuntimeError('Candidate must be clean exact public bd393c7')
    data = {
        'source_directory': str(ROOT), 'source_commit': commit,
        'source_tree': output('git', 'rev-parse', 'HEAD^{tree}'),
        'source_archive_sha256': hashlib.sha256(subprocess.check_output(
            ['git', 'archive', 'HEAD'], cwd=ROOT)).hexdigest(),
        'source_status_before': before,
        'benchmark_source_sha256': sha(ROOT / 'crates/scion-engine/benches/scaling.rs'),
        'driver_source_sha256': sha(ROOT / 'crates/scion-engine/examples/comparison_driver.rs'),
        'rustc': output('rustc', '-vV'), 'cargo': output('cargo', '-vV'),
        'environment': {key: environment.get(key) for key in KEYS},
        'command': command, 'build_log': str(log), 'timings_performed': False,
    }
    print('Building ' + label, flush=True)
    started = time.monotonic()
    with log.open('xb') as stream:
        process = subprocess.run(command, cwd=ROOT, env=environment,
                                 stdout=stream, stderr=subprocess.STDOUT)
    data.update(build_exit_code=process.returncode,
                build_elapsed_seconds=time.monotonic() - started,
                source_status_after=output('git', 'status', '--porcelain=v1', '--untracked-files=all'))
    if not process.returncode:
        if label.startswith('static'):
            executable = Path(target) / 'x86_64-unknown-linux-gnu/release/examples/comparison_driver'
        else:
            candidates = [p for p in (Path(target) / 'release/deps').glob('scaling-*')
                          if p.is_file() and os.access(p, os.X_OK) and p.suffix != '.d']
            if len(candidates) != 1:
                raise RuntimeError('Expected exactly one scaling executable')
            executable = candidates[0]
        data.update(executable=str(executable), executable_sha256=sha(executable),
                    executable_bytes=executable.stat().st_size)
    with report.open('x') as stream:
        json.dump(data, stream, indent=2)
        stream.write('\n')
    if process.returncode or data['source_status_after'] or output('git', 'rev-parse', 'HEAD') != commit:
        raise RuntimeError('Build failed or source changed; inspect ' + str(report))
    print(label + ': ' + data['executable_sha256'], flush=True)
