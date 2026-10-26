#!/usr/bin/env python3
"""Validate and summarize a fixed batch of complete three-way Git comparisons.

The default plan is five full runs. All operation groups and diagnostic controls
are retained. Aggregates describe run p50/p95 summaries, never pooled samples.
This program reads reports only; it does not build or execute benchmarks.
"""

import argparse
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path, PurePosixPath
import statistics
import sys

from compare_git import summarize


OPERATIONS = {
    "ancestry_negative": "fresh_process", "ancestry_positive": "fresh_process",
    "file_read": "fresh_process", "file_read_spread": "resident",
    "merge_base_ancestor": "fresh_process", "merge_base_divergent": "fresh_process",
    "noop": "resident", "prepared_commit": "resident",
    "process_startup": "fresh_process", "repository_open": "fresh_process",
    "small_diff": "fresh_process",
}
CONTROLS = {"noop", "process_startup", "repository_open"}
IMPLEMENTATIONS = ("scion", "scion_baseline", "git")
GROUPS = {(operation, scope, implementation)
          for operation, scope in OPERATIONS.items() for implementation in IMPLEMENTATIONS}
DIMENSIONS = {"files", "commits", "branch_commits", "samples", "fanout",
              "blob_bytes", "variants", "warmup"}
EMPTY_SHA256 = hashlib.sha256(b"").hexdigest()
VERIFICATION = {
    "query_results": "all checked", "file_contents": "byte-identical",
    "changed_paths": "one matching path", "commit_parents": "identical logical DAG topology",
    "commit_graph": "Git --reachable --changed-paths written before query timings",
    "baseline": "executable SHA-256 matches its unchanged clean-source report; identical logical requests, query results, and file bytes; full-mode changeset IDs checked after every edit and chain",
}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(value, length=64):
    return isinstance(value, str) and len(value) == length and all(c in "0123456789abcdef" for c in value)


def number(value):
    try:
        return type(value) in (int, float) and math.isfinite(value)
    except OverflowError:
        return False


def text(value):
    return isinstance(value, str) and bool(value)


def clean_source(source, label):
    require(isinstance(source, dict), f"missing {label} source provenance")
    require(digest(source.get("commit"), 40), f"invalid {label} source commit")
    require(source.get("dirty") is False and source.get("status") == ""
            and source.get("tracked_diff_sha256") == EMPTY_SHA256,
            f"{label} source must be clean and unchanged")
    hashes = source.get("source_sha256")
    require(isinstance(hashes, dict) and hashes, f"missing {label} source hashes")
    require({"Cargo.toml", "Cargo.lock", "crates/scion-engine/examples/comparison_driver.rs"} <= hashes.keys(),
            f"incomplete {label} source hashes")
    for path, value in hashes.items():
        require(text(path) and not PurePosixPath(path).is_absolute()
                and ".." not in PurePosixPath(path).parts and digest(value),
                f"invalid {label} source hash entry")


def clean_build(build, label, modern=False):
    require(isinstance(build, dict), f"missing {label} build provenance")
    command = build.get("command")
    require(isinstance(command, list) and command and all(text(value) for value in command),
            f"invalid {label} build command")
    require(type(build.get("performed")) is bool and build.get("incremental") is False
            and build.get("source_changed_during_build") is False,
            f"{label} build must record unchanged source and disabled incremental compilation")
    require(digest(build.get("driver_sha256")), f"invalid {label} executable hash")
    if modern:
        flags = build.get("rustflags")
        require(isinstance(flags, dict) and "args" in flags and text(flags.get("source"))
                and ((isinstance(flags["args"], list)
                      and all(isinstance(value, str) for value in flags["args"]))
                     or (flags["args"] is None and flags["source"] == "Cargo configuration / target defaults")),
                "missing candidate Rust flag provenance")
        require(type(build.get("static_crt_requested")) is bool
                and "target" in build and (build["target"] is None or text(build["target"]))
                and text(build.get("rustflags_provenance"))
                and isinstance(build.get("linkage"), dict) and build["linkage"],
                "incomplete candidate target/linkage provenance")


def comparison_rows(summary):
    lookup = {(row["operation"], row["scope"], row["implementation"]): row for row in summary}
    rows = []
    for operation, scope in sorted(OPERATIONS.items()):
        candidate = lookup[operation, scope, "scion"]["p50_us"]
        ratios = {f"{name}_over_scion": lookup[operation, scope, implementation]["p50_us"] / candidate
                  for name, implementation in (("git", "git"), ("baseline", "scion_baseline"))}
        require(all(number(value) for value in ratios.values()), "non-finite comparison ratio")
        rows.append({"operation": operation, "scope": scope, **ratios})
    return rows


def validate_report(report):
    require(isinstance(report, dict) and type(report.get("schema")) is int and report["schema"] == 1,
            "expected schema 1 report")
    require(report.get("mode") == "full" and report.get("status", "complete") == "complete"
            and "error" not in report, "expected a complete full-mode report")
    dimensions = report.get("dimensions")
    require(isinstance(dimensions, dict) and set(dimensions) == DIMENSIONS, "invalid dimensions")
    require(all(type(value) is int and value >= (0 if key == "warmup" else 1)
                for key, value in dimensions.items()), "dimensions must contain valid integer counts")
    configuration = report.get("configuration")
    require(isinstance(configuration, dict) and configuration
            and isinstance(configuration.get("git"), dict)
            and isinstance(configuration.get("scion"), dict)
            and isinstance(configuration.get("resident_reads"), dict), "missing comparison configuration")
    require(configuration["git"].get("core.fsync") == "all"
            and configuration["git"].get("core.fsyncMethod") == "fsync"
            and configuration["scion"].get("backend") == "LocalStore"
            and configuration["scion"].get("sqlite") == "WAL, synchronous=FULL",
            "expected the fully durable LocalStore/Git comparison")
    source, build, host, baseline = (report.get(key) for key in ("source", "build", "host", "baseline"))
    clean_source(source, "candidate")
    clean_build(build, "candidate", modern=True)
    require(isinstance(host, dict) and text(host.get("git")) and digest(host.get("git_sha256")),
            "missing Git binary/version provenance")
    host_fields = ("platform", "machine", "cpu", "logical_cpus", "python", "rustc")
    require(all(key in host for key in host_fields)
            and all(text(host[key]) for key in host_fields if key != "logical_cpus")
            and type(host["logical_cpus"]) is int and host["logical_cpus"] > 0,
            "incomplete host/toolchain provenance")
    require(isinstance(baseline, dict) and baseline.get("implementation") == "scion_baseline",
            "missing preserved baseline provenance")
    clean_source(baseline.get("source"), "baseline")
    clean_build(baseline.get("build"), "baseline")
    require(digest(baseline.get("source_report_sha256")) and digest(baseline.get("driver_sha256"))
            and baseline["driver_sha256"] == baseline["build"]["driver_sha256"],
            "inconsistent baseline original-report/executable identity")
    require(baseline.get("fixture_scope") == "independent store with identical logical requests and verified identical Scion IDs",
            "baseline must use an independent full fixture")
    verification = report.get("verification")
    require(isinstance(verification, dict)
            and all(verification.get(key) == value for key, value in VERIFICATION.items()),
            "missing completed correctness verification")
    fixture = report.get("fixture")
    require(isinstance(fixture, dict) and isinstance(fixture.get("scion"), dict)
            and fixture["scion"] and fixture["scion"] == fixture.get("scion_baseline")
            and isinstance(fixture.get("git"), dict) and fixture["git"],
            "missing or inconsistent full fixture identities")
    snapshot = report.get("graph_snapshot")
    if snapshot is not None:
        require(isinstance(snapshot, dict) and digest(snapshot.get("sha256"))
                and isinstance(snapshot.get("metadata"), dict)
                and verification.get("graph_snapshot") == "artifact SHA-256 unchanged; complete verified publication snapshot; all graph answers match the same fixture and preserved baseline",
                "incomplete frozen graph verification")

    samples = report.get("samples")
    count = dimensions["samples"]
    require(isinstance(samples, list) and len(samples) == len(GROUPS) * count,
            "expected exactly 33 complete sample groups")
    groups = {key: set() for key in GROUPS}
    for sample in samples:
        require(isinstance(sample, dict), "invalid sample row")
        key = tuple(sample.get(field) for field in ("operation", "scope", "implementation"))
        require(all(isinstance(value, str) for value in key) and key in groups,
                "unexpected operation/scope/implementation group")
        index = sample.get("sample")
        require(type(index) is int and 0 <= index < count and index not in groups[key],
                "duplicate, missing or out-of-range sample index")
        require(type(sample.get("wall_ns")) is int and sample["wall_ns"] > 0,
                "wall_ns must be a positive integer")
        for field in ("internal_ns", "output_bytes"):
            value = sample.get(field)
            require(value is None or (type(value) is int and value >= 0), f"invalid {field}")
        groups[key].add(index)
    require(all(indices == set(range(count)) for indices in groups.values()), "incomplete sample group")
    try:
        computed = summarize(samples)
    except (ArithmeticError, ValueError) as error:
        raise ValueError("sample values cannot produce finite summaries") from error
    stored = report.get("summary")
    require(isinstance(stored, list) and len(stored) == len(GROUPS), "missing complete stored summary")
    for row in stored:
        require(isinstance(row, dict) and type(row.get("samples")) is int
                and all(number(row.get(key)) for key in ("p50_us", "p95_us", "min_us", "max_us")),
                "invalid stored percentile summary")
    require(stored == computed, "stored summary differs from raw samples")
    comparisons = comparison_rows(computed)
    stored_ratios = report.get("comparisons")
    require(isinstance(stored_ratios, list) and len(stored_ratios) == len(OPERATIONS)
            and all(isinstance(row, dict) and number(row.get("git_over_scion"))
                    and number(row.get("baseline_over_scion")) for row in stored_ratios),
            "missing complete stored comparison ratios")
    require(stored_ratios == comparisons, "stored ratios differ from recomputed summaries")
    # Artifact paths, timestamps, free-space readings and preparation times vary
    # across independent runs. Workload and executable identities must not vary.
    identity = {
        "dimensions": dimensions, "configuration": configuration, "source": source,
        "build": {key: value for key, value in build.items() if key != "driver"},
        "git": {key: host.get(key) for key in ("git", "git_sha256", "git_exec_path")},
        "host": {key: host[key] for key in host_fields},
        "baseline": {key: baseline[key] for key in ("source_report_sha256", "driver_sha256", "source", "build", "fixture_scope")},
        "fixture": fixture,
        "graph_snapshot": {key: snapshot[key] for key in ("sha256", "metadata")} if snapshot else None,
    }
    return identity, computed, comparisons


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result, f"duplicate JSON field: {key}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"non-finite JSON value: {value}")


def aggregate(paths, expected_trials=5):
    require(type(expected_trials) is int and expected_trials > 0, "expected trial count must be positive")
    require(len(paths) == expected_trials, f"expected exactly {expected_trials} reports")
    seen_paths, seen_inodes, seen_hashes, seen_content = set(), set(), set(), set()
    inputs, runs, identity = [], [], None
    for number_, path in enumerate(paths, 1):
        path = Path(path).expanduser().resolve(strict=True)
        require(path.is_file(), f"input must be a regular file: {path}")
        stat = path.stat()
        inode = (stat.st_dev, stat.st_ino)
        raw = path.read_bytes()
        checksum = hashlib.sha256(raw).hexdigest()
        require(path not in seen_paths and inode not in seen_inodes and checksum not in seen_hashes,
                f"duplicate input report: {path}")
        seen_paths.add(path)
        seen_inodes.add(inode)
        seen_hashes.add(checksum)
        report = json.loads(raw, object_pairs_hook=unique_object, parse_constant=reject_constant)
        current, summary, comparisons = validate_report(report)
        canonical = dict(report)
        canonical["samples"] = sorted(report["samples"], key=lambda row: (
            row["operation"], row["scope"], row["implementation"], row["sample"]))
        content_hash = hashlib.sha256(json.dumps(
            canonical, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()).hexdigest()
        require(content_hash not in seen_content, f"duplicate input report content: {path}")
        seen_content.add(content_hash)
        if identity is None:
            identity = current
        else:
            for field, value in current.items():
                require(identity[field] == value, f"mixed {field} across reports: {path}")
        inputs.append({"path": str(path), "sha256": checksum,
                       "canonical_content_sha256": content_hash, "bytes": len(raw)})
        runs.append({"run": number_, "input_sha256": checksum, "host": report["host"],
                     "work_directory": report.get("work_directory"), "summary": summary,
                     "comparisons": comparisons})
    summary = []
    for index, original in enumerate(runs[0]["summary"]):
        row = {key: original[key] for key in ("operation", "scope", "implementation")}
        row.update(run_count=expected_trials, samples_per_run=original["samples"])
        for percentile in ("p50", "p95"):
            values = [run["summary"][index][f"{percentile}_us"] for run in runs]
            row[f"run_{percentile}_us"] = values
            row[f"median_run_{percentile}_us"] = statistics.median(values)
            row[f"min_run_{percentile}_us"] = min(values)
            row[f"max_run_{percentile}_us"] = max(values)
        require(all(number(value) for key, value in row.items() if key.endswith("_us") and not isinstance(value, list)),
                "run-summary aggregation overflowed")
        summary.append(row)
    lookup = {(row["operation"], row["scope"], row["implementation"]): row for row in summary}
    comparisons = []
    for index, (operation, scope) in enumerate(sorted(OPERATIONS.items())):
        row = {"operation": operation, "scope": scope,
               "classification": "control" if operation in CONTROLS else "functional"}
        denominator = lookup[operation, scope, "scion"]["median_run_p50_us"]
        for name, implementation in (("git", "git"), ("baseline", "scion_baseline")):
            ratio = lookup[operation, scope, implementation]["median_run_p50_us"] / denominator
            require(number(ratio), "aggregate comparison ratio overflowed")
            row[f"{name}_over_scion_ratio_of_median_run_p50"] = ratio
            values = [run["comparisons"][index][f"{name}_over_scion"] for run in runs]
            row[f"{name}_over_scion_per_run"] = {"values": values, "min": min(values), "max": max(values)}
        comparisons.append(row)
    for item in inputs:
        require(hashlib.sha256(Path(item["path"]).read_bytes()).hexdigest() == item["sha256"],
                f"input changed during summarization: {item['path']}")
    dependency = Path(summarize.__code__.co_filename).resolve()
    return {
        "schema": 1, "status": "complete", "mode": "full_run_aggregate",
        "expected_trials": expected_trials, "completed_trials": len(runs),
        "created_utc": datetime.now(timezone.utc).isoformat(),
        "aggregation": "Medians of run p50 and run p95 values are reported separately. Ratios divide median run p50 values; individual operation samples are not pooled or reconstructed. Per-run values and ranges retain all runs and controls.",
        "summarizer": {
            "path": str(Path(__file__).resolve()),
            "sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            "python": sys.version,
            "summary_dependency": {"function": f"{summarize.__module__}.{summarize.__name__}",
                                   "path": str(dependency),
                                   "sha256": hashlib.sha256(dependency.read_bytes()).hexdigest()},
        },
        "identity": identity, "inputs": inputs, "runs": runs,
        "summary": summary, "comparisons": comparisons,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=Path)
    parser.add_argument("--expected-trials", type=int, default=5)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    try:
        destination = args.output.expanduser().absolute()
        resolved = destination.resolve()
        require(all(resolved != path.expanduser().resolve() for path in args.reports),
                "output must not overwrite an input report")
        require(not os.path.lexists(destination) and not resolved.exists(),
                "output already exists; choose a new report path")
        result = aggregate(args.reports, args.expected_trials)
        resolved.parent.mkdir(parents=True, exist_ok=True)
        with resolved.open("x", encoding="utf-8") as stream:
            json.dump(result, stream, indent=2, allow_nan=False)
            stream.write("\n")
    except (OSError, ValueError) as error:
        parser.error(str(error))
    print(resolved)
    return 0


if __name__ == "__main__":
    sys.exit(main())
