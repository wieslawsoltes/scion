#!/usr/bin/env python3
"""Compare prebuilt copies of the original full MemoryStore scaling benchmark.

No builds, warmup removal, or changes to the Rust workload are performed. Each
trial runs both complete executables in alternating order. Ratios compare the
median of each executable's per-trial p50 values, not pooled operation samples.
"""

import argparse
import csv
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import subprocess
import sys


BENCHMARK = "crates/scion-engine/benches/scaling.rs"
# Original public 6a11ff7 workload. Updating this also requires reviewing ROWS.
BENCHMARK_SHA256 = "a3aabcb63f820fe2ac566c7f9b36856ec45612cda2c3604e22986ccc3ec93a00"
BANNER = "# Scion engine benchmark; MemoryStore; release optimizations; microseconds"
HEADER = ["operation", "scale", "samples", "p50_us", "p95_us"]
ROWS = [(operation, scale, samples)
        for scale in (1000, 10000, 100000)
        for operation, samples in (("initial_commit", 1),
                                   ("single_file_commit", 50),
                                   ("single_file_diff", 50))]
ROWS += [("ancestry_positive", 10000, 100), ("ancestry_negative", 10000, 100)]
ENVIRONMENT_KEYS = (
    "PATH", "RUSTUP_HOME", "CARGO_HOME", "CARGO_TARGET_DIR", "CARGO_INCREMENTAL",
    "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET",
    "CARGO_BUILD_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
    "CC", "CXX", "CFLAGS", "CXXFLAGS", "LDFLAGS",
)


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def command_output(command, root):
    return subprocess.run(command, cwd=root, check=True, capture_output=True,
                          text=True, encoding="utf-8").stdout.strip()


def git(root, *arguments):
    return command_output(["git", "--no-optional-locks", *arguments], root)


def source_metadata(root):
    root = Path(root).resolve(strict=True)
    if Path(git(root, "rev-parse", "--show-toplevel")).resolve() != root:
        raise ValueError(f"source root must be the Git worktree root: {root}")
    if git(root, "status", "--porcelain=v1", "--untracked-files=all"):
        raise ValueError(f"source tree is dirty: {root}")
    git(root, "ls-files", "--error-unmatch", "--", BENCHMARK, "Cargo.lock")
    benchmark_hash = sha256(root / BENCHMARK)
    if benchmark_hash != BENCHMARK_SHA256:
        raise ValueError(f"original scaling benchmark source hash differs: {root}")
    return {
        "root": str(root), "commit": git(root, "rev-parse", "HEAD"),
        "tree": git(root, "rev-parse", "HEAD^{tree}"), "dirty": False,
        "benchmark_path": BENCHMARK, "benchmark_source_sha256": benchmark_hash,
        "lockfile_sha256": sha256(root / "Cargo.lock"),
    }


def build_metadata(path, source, executable_hash):
    if path is None:
        return {"verified_report": None,
                "note": "Prebuilt executable build command, flags and toolchain are unknown."}
    path = Path(path).resolve(strict=True)
    raw = path.read_bytes()
    data = json.loads(raw)
    if not isinstance(data, dict):
        raise ValueError("build provenance must be a JSON object")
    expected = {
        "source_commit": source["commit"], "source_tree": source["tree"],
        "benchmark_source_sha256": source["benchmark_source_sha256"],
        "executable_sha256": executable_hash,
    }
    for key, value in expected.items():
        if data.get(key) != value:
            raise ValueError(f"build provenance {key} differs: {path}")
    if (data.get("build_exit_code") != 0
            or data.get("source_status_before") != ""
            or data.get("source_status_after") != ""):
        raise ValueError(f"build provenance must describe a successful clean build: {path}")
    if not all(isinstance(data.get(key), str) and data[key] for key in ("rustc", "cargo")):
        raise ValueError(f"build provenance must include rustc and cargo versions: {path}")
    if (not isinstance(data.get("command"), list) or not data["command"]
            or not all(isinstance(argument, str) for argument in data["command"])):
        raise ValueError(f"build provenance must include its build command: {path}")
    if not isinstance(data.get("environment"), dict):
        raise ValueError(f"build provenance must include its build environment: {path}")
    return {"verified_report": str(path), "sha256": hashlib.sha256(raw).hexdigest(),
            "data": data,
            "note": "Recorded build provenance matched to source and executable hashes; not a signed attestation."}


def variant_metadata(root, driver, build_report):
    source = source_metadata(root)
    driver = Path(driver).resolve(strict=True)
    if not driver.is_file() or not os.access(driver, os.X_OK):
        raise ValueError(f"benchmark driver must be an executable file: {driver}")
    executable_hash = sha256(driver)
    return {
        "source": source, "driver": str(driver), "driver_sha256": executable_hash,
        "build": build_metadata(build_report, source, executable_hash),
        "observed_toolchain": {
            "rustc": command_output(["rustc", "-vV"], source["root"]),
            "cargo": command_output(["cargo", "-vV"], source["root"]),
            "rustc_path": shutil.which("rustc"), "cargo_path": shutil.which("cargo"),
            "note": "Observed at comparison time; does not identify how the prebuilt driver was compiled.",
        },
    }


def assert_unchanged(variant):
    if source_metadata(variant["source"]["root"]) != variant["source"]:
        raise ValueError("source identity changed during comparison")
    if sha256(variant["driver"]) != variant["driver_sha256"]:
        raise ValueError("benchmark executable changed during comparison")
    build = variant["build"]
    if build["verified_report"] and sha256(build["verified_report"]) != build["sha256"]:
        raise ValueError("build provenance changed during comparison")


def parse_rows(stdout):
    lines = stdout.splitlines()
    if not lines or lines[0] != BANNER:
        raise ValueError("missing original scaling benchmark banner")
    rows = list(csv.reader(lines[1:]))
    if not rows or rows[0] != HEADER or len(rows) != len(ROWS) + 1:
        raise ValueError("expected the original header and exactly 11 full-profile rows")
    parsed = []
    for cells, expected in zip(rows[1:], ROWS):
        if len(cells) != 5:
            raise ValueError("scaling benchmark row must have five fields")
        operation, scale, samples, p50, p95 = cells
        scale, samples = int(scale), int(samples)
        p50, p95 = float(p50), float(p95)
        if (operation, scale, samples) != expected:
            raise ValueError(f"benchmark operation, scale or sample count differs: {cells}")
        if not (math.isfinite(p50) and math.isfinite(p95) and 0 <= p50 <= p95):
            raise ValueError(f"invalid benchmark percentiles: {cells}")
        if samples == 1 and p50 != p95:
            raise ValueError("one-observation initial commit must have identical p50 and p95")
        parsed.append(dict(zip(HEADER, (operation, scale, samples, p50, p95))))
    return parsed


def summarize(trials):
    result = []
    for index, (operation, scale, samples) in enumerate(ROWS):
        row = {"operation": operation, "scale": scale, "samples_per_trial": samples}
        for label in ("baseline", "candidate"):
            observations = [trial["runs"][label]["rows"][index] for trial in trials]
            p50 = [observation["p50_us"] for observation in observations]
            p95 = [observation["p95_us"] for observation in observations]
            row[label] = {"trial_p50_us": p50, "trial_p95_us": p95,
                          "median_trial_p50_us": statistics.median(p50),
                          "median_trial_p95_us": statistics.median(p95)}
            if not all(math.isfinite(row[label][key])
                       for key in ("median_trial_p50_us", "median_trial_p95_us")):
                raise ValueError("trial percentile aggregation overflowed")
        denominator = row["candidate"]["median_trial_p50_us"]
        row["speedup_ratio_of_median_trial_p50"] = (
            row["baseline"]["median_trial_p50_us"] / denominator if denominator else None)
        ratio = row["speedup_ratio_of_median_trial_p50"]
        if ratio is not None and not math.isfinite(ratio):
            raise ValueError("trial percentile ratio overflowed")
        if samples == 1:
            row["observation_note"] = (
                "One initial-commit observation per trial; that trial's p50 and p95 are the same observation.")
        result.append(row)
    return result


def output_path(path, variants):
    path = Path(path).expanduser().absolute()
    if os.path.lexists(path):
        raise ValueError(f"output already exists; choose a new report path: {path}")
    path = path.resolve()
    for variant in variants.values():
        root = Path(variant["source"]["root"])
        if path.is_relative_to(root):
            ignored = subprocess.run(
                ["git", "--no-optional-locks", "check-ignore", "--quiet", "--", str(path.relative_to(root))],
                cwd=root, capture_output=True).returncode
            if ignored != 0:
                raise ValueError("report inside a source tree must be in a Git-ignored output directory")
    return path


def run_trials(report, count, save):
    for number in range(count):
        order = ["baseline", "candidate"] if number % 2 == 0 else ["candidate", "baseline"]
        trial = {"trial": number + 1, "execution_order": order, "runs": {}}
        report["trials"].append(trial)
        for label in order:
            variant = report["variants"][label]
            assert_unchanged(variant)
            print(f"trial {number + 1}/{count}: {label}", file=sys.stderr, flush=True)
            command = [variant["driver"]]
            process = subprocess.run(command, cwd=variant["source"]["root"], capture_output=True)
            record = {"command": command, "cwd": variant["source"]["root"],
                      "returncode": process.returncode,
                      "stdout": process.stdout.decode("utf-8", errors="surrogateescape"),
                      "stderr": process.stderr.decode("utf-8", errors="surrogateescape")}
            trial["runs"][label] = record
            if process.returncode:
                raise ValueError(f"{label} benchmark failed in trial {number + 1}: exit {process.returncode}")
            record["rows"] = parse_rows(record["stdout"])
            assert_unchanged(variant)
            save()
    for variant in report["variants"].values():
        assert_unchanged(variant)
    report["summary"] = summarize(report["trials"])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for label in ("baseline", "candidate"):
        parser.add_argument(f"--{label}-root", required=True, type=Path)
        parser.add_argument(f"--{label}-driver", required=True, type=Path)
        parser.add_argument(f"--{label}-build-report", type=Path,
                            help="optional recorded clean-build provenance JSON")
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args(argv)
    if args.trials < 1:
        parser.error("--trials must be positive")
    try:
        variants = {label: variant_metadata(getattr(args, f"{label}_root"),
                                            getattr(args, f"{label}_driver"),
                                            getattr(args, f"{label}_build_report"))
                    for label in ("baseline", "candidate")}
        destination = output_path(args.output, variants)
        destination.parent.mkdir(parents=True, exist_ok=True)
        stream = destination.open("x", encoding="utf-8")
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))
    report = {
        "schema": 1, "status": "running", "backend": "MemoryStore",
        "profile": "original full workload; no builds or --quick; fresh process per trial",
        "aggregation": "baseline median of per-trial p50 / candidate median of per-trial p50; individual operation samples are unavailable and are not pooled",
        "initial_commit_note": "Each initial_commit row is one observation per trial, not a within-trial percentile distribution.",
        "output_encoding": "UTF-8; invalid output bytes are preserved with Python surrogateescape",
        "started_utc": datetime.now(timezone.utc).isoformat(), "requested_trials": args.trials,
        "host": {"platform": platform.platform(), "machine": platform.machine(),
                 "python": sys.version, "cpu_count": os.cpu_count(),
                 "cpu_affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None},
        "observed_environment": {key: os.environ.get(key) for key in ENVIRONMENT_KEYS},
        "variants": variants, "trials": [],
    }
    with stream:
        def save():
            stream.seek(0)
            json.dump(report, stream, indent=2, allow_nan=False)
            stream.write("\n")
            stream.truncate()
            stream.flush()

        save()
        try:
            run_trials(report, args.trials, save)
            report["status"] = "complete"
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            report["status"] = "failed"
            report["error"] = str(error)
        finally:
            report["finished_utc"] = datetime.now(timezone.utc).isoformat()
            save()
    if report["status"] != "complete":
        print(f"comparison rejected: {report['error']}; evidence: {destination}", file=sys.stderr)
        return 1
    print(destination)
    return 0


if __name__ == "__main__":
    sys.exit(main())
