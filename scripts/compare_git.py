#!/usr/bin/env python3
"""Reproducible LocalStore/Git comparison; stdlib only, no kernel-cache flushing.

Prepared commits: two resident processes, full request/ack latency, durable
objects plus ref updates. Queries: a fresh process for each engine and sample.
Run --quick for a smoke test, or --reuse-report to repeat every query on a
retained fixture. See docs/git-comparison.md for interpretation.
"""
from __future__ import annotations

import argparse
import csv
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
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
AUTHOR = "Benchmark <benchmark@example.invalid>"
EPOCH = 1_700_000_000
GIT_CONFIG = {
    "core.fsync": "all",
    "core.fsyncMethod": "fsync",
    "feature.manyFiles": "true",
    "core.commitGraph": "true",
    "fastimport.unpackLimit": "0",
    "gc.auto": "0",
    "maintenance.auto": "false",
    "user.name": "Benchmark",
    "user.email": "benchmark@example.invalid",
}


def environment():
    result = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    result.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, LC_ALL="C", TZ="UTC", CARGO_INCREMENTAL="0")
    return result


ENV = environment()


def run(command, *, input=None, accepted=(0,), cwd=ROOT):
    start = time.perf_counter_ns()
    result = subprocess.run([str(part) for part in command], input=input, stdout=subprocess.PIPE, stderr=subprocess.PIPE, cwd=cwd, env=ENV, check=False)
    elapsed = time.perf_counter_ns() - start
    if result.returncode not in accepted:
        raise RuntimeError(f"{command!r}: exit {result.returncode}\n{result.stderr.decode(errors='replace')}")
    return result, elapsed


def body(variant, size):
    prefix = f"fixture {variant:03}\n".encode()
    line = b"one two three four five six seven eight nine ten\n"
    return (prefix + line * (size // len(line) + 1))[:size - 1] + b"\n"


def file_path(index, fanout):
    return f"dir{index // fanout:04}/file{index:06}.txt"


def encoded(request):
    return json.dumps(request, separators=(",", ":")).encode() + b"\n"


class ScionWorker:
    def __init__(self, executable, store, log, *, read_only=False):
        self.log = open(log, "w+b")
        command = [str(executable), str(store)] + (["--read-only"] if read_only else [])
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, env=ENV)
        ready = self.process.stdout.readline()
        if not ready or json.loads(ready) != {"ready": True}:
            raise RuntimeError(f"Scion worker did not become ready: {ready!r}")

    def request(self, request):
        payload = encoded(request)
        start = time.perf_counter_ns()
        self.process.stdin.write(payload)
        self.process.stdin.flush()
        reply = self.process.stdout.readline()
        elapsed = time.perf_counter_ns() - start
        if not reply:
            self.log.seek(0)
            raise RuntimeError(f"Scion worker exited: {self.log.read().decode(errors='replace')}")
        result = json.loads(reply)
        if not result["ok"]:
            raise RuntimeError(result["error"])
        return result, elapsed

    def close(self):
        self.process.stdin.close()
        if self.process.wait(timeout=60):
            raise RuntimeError("Scion worker failed on shutdown")
        self.log.close()


class GitWorker:
    def __init__(self, directory, log):
        self.directory = directory
        self.log = open(log, "w+b")
        self.process = subprocess.Popen(["git", "-C", str(directory), "fast-import", "--quiet", "--done"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, env=ENV)
        self.next_mark = 1

    def mark(self):
        value = self.next_mark
        self.next_mark += 1
        return value

    def submit(self, payload, mark, checkpoint=True):
        suffix = (b"checkpoint\n" if checkpoint else b"") + f"get-mark :{mark}\n".encode()
        wire = payload + suffix
        start = time.perf_counter_ns()
        self.process.stdin.write(wire)
        self.process.stdin.flush()
        reply = self.process.stdout.readline()
        elapsed = time.perf_counter_ns() - start
        if not reply:
            self.log.seek(0)
            raise RuntimeError(f"Git fast-import exited: {self.log.read().decode(errors='replace')}")
        identifier = reply.decode().strip()
        if len(identifier) != 40 or any(c not in "0123456789abcdef" for c in identifier):
            raise RuntimeError(f"invalid Git acknowledgement: {identifier!r}")
        return identifier, elapsed

    def header(self, branch, mark, parent, message, sequence):
        message = message.encode()
        payload = (f"commit refs/heads/{branch}\nmark :{mark}\nauthor {AUTHOR} {EPOCH + sequence} +0000\ncommitter {AUTHOR} {EPOCH + sequence} +0000\ndata {len(message)}\n".encode() + message + b"\n")
        if parent is not None:
            payload += f"from {parent}\n".encode()
        return payload

    def initialize(self, files, fanout, variants, blob_bytes):
        payload = bytearray()
        marks = []
        for variant in range(variants):
            mark = self.mark()
            marks.append(mark)
            content = body(variant, blob_bytes)
            payload.extend(f"blob\nmark :{mark}\ndata {len(content)}\n".encode() + content + b"\n")
        root = self.mark()
        payload.extend(self.header("bench", root, None, "initial", 0))
        for index in range(files):
            payload.extend(f"M 100644 :{marks[index % variants]} {file_path(index, fanout)}\n".encode())
        payload.extend(b"\n")
        return (*self.submit(payload, root), root)

    def commit(self, parent, path, content, sequence):
        mark = self.mark()
        content = content.encode()
        payload = self.header("bench", mark, parent, f"edit {sequence}", sequence)
        payload += f"M 100644 inline {path}\ndata {len(content)}\n".encode() + content + b"\n\n"
        return self.submit(payload, mark)

    def chain(self, parent, count, label):
        payload = bytearray()
        for sequence in range(count):
            mark = self.mark()
            payload.extend(self.header(label, mark, parent, f"{label} {sequence}", sequence + 1) + b"\n")
            parent = f":{mark}"
        return self.submit(payload, mark)

    def noop(self, mark):
        return self.submit(b"", mark, checkpoint=False)

    def close(self):
        self.process.stdin.write(b"done\n")
        self.process.stdin.close()
        if self.process.wait(timeout=60):
            self.log.seek(0)
            raise RuntimeError(self.log.read().decode(errors="replace"))
        self.log.close()


class GitReader:
    """One fully consumed cat-file batch request per measurement."""

    def __init__(self, directory, log):
        self.log = open(log, "w+b")
        self.process = subprocess.Popen(
            ["git", "-C", str(directory), "cat-file", "--batch"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=self.log, env=ENV,
        )

    def read(self, revision, path):
        payload = f"{revision}:{path}\n".encode()
        start = time.perf_counter_ns()
        self.process.stdin.write(payload)
        self.process.stdin.flush()
        header = self.process.stdout.readline().split()
        if len(header) != 3 or header[1] != b"blob":
            raise RuntimeError(f"unexpected cat-file header: {header!r}")
        size = int(header[2])
        content = self.process.stdout.read(size + 1)
        elapsed = time.perf_counter_ns() - start
        if len(content) != size + 1 or content[-1:] != b"\n":
            raise RuntimeError("incomplete cat-file response")
        return content[:-1], elapsed

    def close(self):
        self.process.stdin.close()
        if self.process.wait(timeout=60):
            raise RuntimeError("Git cat-file failed on shutdown")
        self.log.close()


def source_metadata():
    status = run(["git", "status", "--porcelain=v1"])[0].stdout.decode()
    diff = run(["git", "diff", "--binary", "HEAD"])[0].stdout
    paths = [ROOT / "Cargo.toml", ROOT / "Cargo.lock", Path(__file__)]
    for crate in ("scion-types", "scion-store", "scion-graph", "scion-engine"):
        paths.extend((ROOT / "crates" / crate).rglob("*.rs"))
        paths.append(ROOT / "crates" / crate / "Cargo.toml")
    return {
        "commit": run(["git", "rev-parse", "HEAD"])[0].stdout.decode().strip(),
        "dirty": bool(status),
        "status": status,
        "tracked_diff_sha256": hashlib.sha256(diff).hexdigest(),
        "source_sha256": {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in sorted(set(paths))},
    }


def host_metadata(workdir):
    git_binary = Path(shutil.which("git", path=ENV.get("PATH")) or "git").resolve()
    cpu = "unknown"
    if Path("/proc/cpuinfo").exists():
        cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), cpu)
    filesystems = "unknown"
    if os.name == "posix":
        try:
            filesystems = run(["df", "-T", str(workdir)])[0].stdout.decode()
        except RuntimeError:
            filesystems = run(["df", str(workdir)])[0].stdout.decode()
    return {
        "utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "platform": platform.platform(), "machine": platform.machine(), "cpu": cpu,
        "logical_cpus": os.cpu_count(), "python": sys.version,
        "git": run(["git", "--version"])[0].stdout.decode().strip(),
        "git_binary": str(git_binary),
        "git_sha256": hashlib.sha256(git_binary.read_bytes()).hexdigest(),
        "git_exec_path": run(["git", "--exec-path"])[0].stdout.decode().strip(),
        "rustc": run(["rustc", "-vV"])[0].stdout.decode().strip(),
        "filesystem": filesystems,
        "memory_kib": next((line.split()[1] for line in Path("/proc/meminfo").read_text().splitlines() if line.startswith("MemTotal:")), None) if Path("/proc/meminfo").exists() else None,
    }


def retained_fixture(report_path):
    """Load only a retained fixture that matches its report's exact marker."""
    raw = report_path.read_bytes()
    report = json.loads(raw)
    if report["schema"] != 1:
        raise ValueError("unsupported source-report schema")
    dimensions = report["dimensions"]
    required_dimensions = {"files", "commits", "branch_commits", "samples", "fanout", "blob_bytes", "variants", "warmup"}
    if set(dimensions) != required_dimensions or any(type(value) is not int for value in dimensions.values()):
        raise ValueError("invalid source-report dimensions")
    if any(value < (0 if key == "warmup" else 1) for key, value in dimensions.items()) or dimensions["blob_bytes"] < 32:
        raise ValueError("invalid source-report dimensions")
    fixture = report["fixture"]
    for implementation, length in (("scion", 64), ("git", 40)):
        for key in ("root", "tip", "previous", "trunk", "left", "right", "side"):
            identifier = fixture[implementation][key]
            if not isinstance(identifier, str) or len(identifier) != length or any(c not in "0123456789abcdef" for c in identifier):
                raise ValueError(f"invalid {implementation} fixture identifier: {key}")
    if fixture["path"] != file_path(0, dimensions["fanout"]):
        raise ValueError("source report does not describe the generated fixture's edit path")
    if not isinstance(fixture["content"], str) or len(fixture["content"].encode()) != dimensions["blob_bytes"]:
        raise ValueError("source report has invalid expected file content")
    work = Path(report["work_directory"]).resolve()
    if not (work / "git").is_dir() or not (work / "scion" / "metadata.sqlite3").is_file():
        raise ValueError("fixture was not retained; the original run requires --keep-workdir")
    if json.loads((work / "fixture.json").read_text()) != fixture:
        raise ValueError("retained fixture marker differs from the source report")
    provenance = {
        "source_report": str(report_path.resolve()),
        "source_report_sha256": hashlib.sha256(raw).hexdigest(),
        "original_source_commit": report["source"]["commit"],
        "original_git": report["host"]["git"],
        "original_driver_sha256": report["build"]["driver_sha256"],
        "scope": "queries only; no prepared commits or resident writer no-ops are measured",
    }
    return dimensions.copy(), fixture, work, provenance


def record(rows, operation, scope, implementation, sample, wall_ns, internal_ns=None, output_bytes=None):
    rows.append({"operation": operation, "scope": scope, "implementation": implementation, "sample": sample, "wall_ns": wall_ns, "internal_ns": internal_ns, "output_bytes": output_bytes})


def summarize(rows):
    groups = {}
    for row in rows:
        groups.setdefault((row["operation"], row["scope"], row["implementation"]), []).append(row["wall_ns"])
    result = []
    for (operation, scope, implementation), values in sorted(groups.items()):
        values.sort()
        result.append({"operation": operation, "scope": scope, "implementation": implementation, "samples": len(values), "p50_us": statistics.median(values) / 1000, "p95_us": values[math.ceil(len(values) * .95) - 1] / 1000, "min_us": values[0] / 1000, "max_us": values[-1] / 1000})
    return result


def save(output, result):
    output.parent.mkdir(parents=True, exist_ok=True)
    output.with_suffix(".json").write_text(json.dumps(result, indent=2) + "\n")
    with output.with_suffix(".csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=["operation", "scope", "implementation", "sample", "wall_ns", "internal_ns", "output_bytes"])
        writer.writeheader()
        writer.writerows(result["samples"])
    with output.with_name(output.name + "-summary.csv").open("w", newline="") as stream:
        writer = csv.DictWriter(stream, fieldnames=["operation", "scope", "implementation", "samples", "p50_us", "p95_us", "min_us", "max_us"])
        writer.writeheader()
        writer.writerows(result["summary"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--files", type=int)
    parser.add_argument("--commits", type=int)
    parser.add_argument("--branch-commits", type=int)
    parser.add_argument("--samples", type=int)
    parser.add_argument("--warmup", type=int)
    parser.add_argument("--fanout", type=int)
    parser.add_argument("--blob-bytes", type=int)
    parser.add_argument("--variants", type=int)
    parser.add_argument("--reuse-report", type=Path, help="repeat all queries on the fixture retained by a previous report; no commit timings")
    parser.add_argument("--output", type=Path, default=ROOT / "target/git-comparison/results")
    parser.add_argument("--work-root", type=Path)
    parser.add_argument("--keep-workdir", action="store_true")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--offline", action="store_true")
    parser.add_argument("--driver", type=Path, default=ROOT / "target/release/examples" / ("comparison_driver.exe" if os.name == "nt" else "comparison_driver"))
    args = parser.parse_args()
    reused = None
    if args.reuse_report is not None:
        if args.quick or args.work_root is not None or any(value is not None for value in (args.files, args.commits, args.branch_commits, args.fanout, args.blob_bytes, args.variants)):
            parser.error("--reuse-report keeps fixture dimensions; only --samples and --warmup may override them")
        try:
            dimensions, fixture, retained_work, reused = retained_fixture(args.reuse_report)
        except (OSError, ValueError, KeyError, TypeError) as error:
            parser.error(f"cannot reuse fixture: {error}")
        protected = {args.reuse_report.resolve(), (retained_work / "fixture.json").resolve()}
        outputs = (args.output.with_suffix(".json"), args.output.with_suffix(".csv"), args.output.with_name(args.output.name + "-summary.csv"))
        if any(output.resolve() in protected for output in outputs):
            parser.error("--output must not overwrite the source report or retained fixture marker")
        if args.samples is not None: dimensions["samples"] = args.samples
        if args.warmup is not None: dimensions["warmup"] = args.warmup
    else:
        dimensions = {
            "files": args.files if args.files is not None else (1000 if args.quick else 100_000),
            "commits": args.commits if args.commits is not None else (1000 if args.quick else 10_000),
            "branch_commits": args.branch_commits if args.branch_commits is not None else (100 if args.quick else 1000),
            "samples": args.samples if args.samples is not None else (7 if args.quick else 31),
            "fanout": args.fanout if args.fanout is not None else 100,
            "blob_bytes": args.blob_bytes if args.blob_bytes is not None else 1024,
            "variants": args.variants if args.variants is not None else 128,
            "warmup": args.warmup if args.warmup is not None else 3,
        }
    args.fanout, args.blob_bytes, args.variants, args.warmup = (dimensions[key] for key in ("fanout", "blob_bytes", "variants", "warmup"))
    if min(value for key, value in dimensions.items() if key != "warmup") < 1 or args.warmup < 0 or args.blob_bytes < 32:
        parser.error("dimensions must be positive, warmup nonnegative, and blob size at least 32 bytes")
    source_before = source_metadata()
    build = ["cargo", "build", "--release", "--locked", "-p", "scion-engine", "--example", "comparison_driver"]
    if args.offline: build.append("--offline")
    if not args.no_build:
        print("Building optimized comparison driver", file=sys.stderr, flush=True)
        completed = subprocess.run(build, cwd=ROOT, env=ENV, check=False)
        if completed.returncode: raise SystemExit(completed.returncode)
    source_after = source_metadata()
    driver = args.driver.resolve()
    if not driver.is_file(): parser.error(f"driver does not exist: {driver}")
    work = retained_work if reused is not None else Path(tempfile.mkdtemp(prefix="scion-git-comparison-", dir=args.work_root))
    git_dir, scion_dir = work / "git", work / "scion"
    result = {
        "schema": 1, "mode": "queries_only" if reused is not None else "full", "dimensions": dimensions, "host": host_metadata(work), "source": source_after,
        "build": {"command": build, "performed": not args.no_build, "incremental": False, "source_changed_during_build": source_before["source_sha256"] != source_after["source_sha256"], "driver_sha256": hashlib.sha256(driver.read_bytes()).hexdigest()},
        "configuration": {"git": GIT_CONFIG, "scion": {"backend": "LocalStore", "sqlite": "WAL, synchronous=FULL", "object_durability": "small objects and publication metadata in FULL-synchronous SQLite; larger loose objects use file fsync and directory fsync on Unix", "read_queries": "read-only SQLite connection with live WAL; no schema migrations"}, "cache": f"warm OS cache; {args.warmup} warmups; fresh query processes; no cache dropping", "sample_order": "alternating Scion/Git, then Git/Scion", "git_object_format": "sha1", "scion_object_format": "blake3", "fixture": f"files share {args.variants} deterministic {args.blob_bytes}-byte text bodies; metadata-heavy fixture, not a real repository"},
        "work_directory": str(work), "setup": [], "samples": [], "verification": {},
    }
    if reused is not None:
        result["reuse"] = reused
    rows = result["samples"]
    scion = git = reader = None
    try:
        if reused is None:
            run(["git", "init", "--bare", "--object-format=sha1", git_dir])
        for key, value in GIT_CONFIG.items(): run(["git", "-C", git_dir, "config", key, value])
        result["configuration"]["git_effective"] = run(["git", "-C", git_dir, "config", "--list", "--local"])[0].stdout.decode()
        if reused is None:
            scion = ScionWorker(driver, scion_dir, work / "scion.stderr")
            git = GitWorker(git_dir, work / "git.stderr")
            print(f"Initializing {dimensions['files']} paths in {work}", file=sys.stderr, flush=True)
            initial = {"op": "initialize", **{key: dimensions[key] for key in ("files", "fanout", "variants", "blob_bytes")}}
            response, elapsed = scion.request(initial)
            scion_root = scion_tip = response["result"]
            result["setup"].append({"operation": "initialize", "implementation": "scion", "wall_ns": elapsed})
            git_root, elapsed, root_mark = git.initialize(dimensions["files"], args.fanout, args.variants, args.blob_bytes)
            git_tip = git_root
            result["setup"].append({"operation": "initialize", "implementation": "git", "wall_ns": elapsed})
            for sample in range(dimensions["samples"]):
                for implementation in (["scion", "git"] if sample % 2 == 0 else ["git", "scion"]):
                    if implementation == "scion":
                        response, elapsed = scion.request({"op": "noop"})
                        record(rows, "noop", "resident", implementation, sample, elapsed, response["operation_ns"])
                    else:
                        _, elapsed = git.noop(root_mark)
                        record(rows, "noop", "resident", implementation, sample, elapsed)
            print("Measuring durable prepared commits", file=sys.stderr, flush=True)
            changed_path = file_path(0, args.fanout)
            for sample in range(args.warmup + dimensions["samples"]):
                sequence = sample + 1
                content = (f"edit {sequence:06}\n".encode() + body(0, args.blob_bytes))[0:args.blob_bytes - 1] + b"\n"
                text = content.decode()
                old_scion, old_git = scion_tip, git_tip
                for implementation in (["scion", "git"] if sample % 2 == 0 else ["git", "scion"]):
                    if implementation == "scion":
                        response, elapsed = scion.request({"op": "commit", "parent": scion_tip, "path": changed_path, "content": text, "sequence": sequence})
                        scion_tip = response["result"]
                        internal = response["operation_ns"]
                    else:
                        git_tip, elapsed = git.commit(git_tip, changed_path, text, sequence)
                        internal = None
                    if sample >= args.warmup:
                        record(rows, "prepared_commit", "resident", implementation, sample - args.warmup, elapsed, internal)
            fixture = {"scion": {"root": scion_root, "tip": scion_tip, "previous": old_scion}, "git": {"root": git_root, "tip": git_tip, "previous": old_git}, "path": changed_path, "content": text}
            for label, count, parent_label in [("trunk", dimensions["commits"], "root"), ("left", dimensions["branch_commits"], "trunk"), ("right", dimensions["branch_commits"], "trunk"), ("side", 1, "root")]:
                print(f"Preparing {label}: {count} commits (setup excluded from comparisons)", file=sys.stderr, flush=True)
                response, elapsed = scion.request({"op": "chain", "parent": fixture["scion"][parent_label], "count": count, "label": label})
                fixture["scion"][label] = response["result"]
                result["setup"].append({"operation": f"chain_{label}", "implementation": "scion", "wall_ns": elapsed})
                identifier, elapsed = git.chain(fixture["git"][parent_label], count, label)
                fixture["git"][label] = identifier
                result["setup"].append({"operation": f"chain_{label}", "implementation": "git", "wall_ns": elapsed})
            git.close(); git = None
            scion.close(); scion = None
        else:
            print(f"Reusing retained fixture in {work}; measuring queries only", file=sys.stderr, flush=True)
        changed_path, text = fixture["path"], fixture["content"]
        content = text.encode()
        for command in [["git", "-C", git_dir, "repack", "-ad"], ["git", "-C", git_dir, "commit-graph", "write", "--reachable", "--changed-paths"]]:
            _, elapsed = run(command)
            result["setup"].append({"operation": "git_query_preparation", "command": [str(part) for part in command], "wall_ns": elapsed})
        if reused is None:
            (work / "fixture.json").write_text(json.dumps(fixture, indent=2) + "\n")
        result["fixture"] = fixture
        sc, gt = fixture["scion"], fixture["git"]
        operations = [
            ("repository_open", {"op": "noop"}, ["rev-parse", "--is-bare-repository"]),
            ("ancestry_positive", {"op": "ancestry", "ancestor": sc["root"], "descendant": sc["trunk"]}, ["merge-base", "--is-ancestor", gt["root"], gt["trunk"]]),
            ("ancestry_negative", {"op": "ancestry", "ancestor": sc["side"], "descendant": sc["trunk"]}, ["merge-base", "--is-ancestor", gt["side"], gt["trunk"]]),
            ("merge_base_ancestor", {"op": "merge_bases", "left": sc["root"], "right": sc["trunk"]}, ["merge-base", "--all", gt["root"], gt["trunk"]]),
            ("merge_base_divergent", {"op": "merge_bases", "left": sc["left"], "right": sc["right"]}, ["merge-base", "--all", gt["left"], gt["right"]]),
            ("small_diff", {"op": "diff", "base": sc["previous"], "target": sc["tip"]}, ["diff", "--no-ext-diff", "--no-textconv", "--no-renames", "--unified=3", gt["previous"], gt["tip"], "--"]),
            ("file_read", {"op": "read", "revision": sc["tip"], "path": changed_path}, ["show", f"{gt['tip']}:{changed_path}"]),
        ]
        print("Measuring fresh-process queries with warm OS cache", file=sys.stderr, flush=True)
        for operation, request, git_arguments in operations:
            for sample in range(args.warmup + dimensions["samples"]):
                for implementation in (["scion", "git"] if sample % 2 == 0 else ["git", "scion"]):
                    if implementation == "scion":
                        completed, elapsed = run([driver, scion_dir, "--once", "--read-only"], input=encoded(request))
                        response = json.loads(completed.stdout)
                        if not response["ok"]: raise RuntimeError(response["error"])
                        value = response["result"]
                        internal = response["operation_ns"]
                        expected = {"repository_open": True, "ancestry_positive": True, "ancestry_negative": False, "merge_base_ancestor": [sc["root"]], "merge_base_divergent": [sc["trunk"]], "file_read": text}
                        if operation == "small_diff":
                            if len(value) != 1 or value[0]["path"] != changed_path or not value[0]["patch"]:
                                raise RuntimeError(f"incorrect Scion diff: {value!r}")
                        elif value != expected[operation]: raise RuntimeError(f"incorrect Scion {operation}: {value!r}")
                    else:
                        completed, elapsed = run(["git", "-C", git_dir, *git_arguments], accepted=(0, 1) if operation == "ancestry_negative" else (0,))
                        internal = None
                        if operation == "ancestry_negative" and completed.returncode != 1: raise RuntimeError("incorrect Git negative ancestry")
                        if operation == "file_read" and completed.stdout != content: raise RuntimeError("file content differs between implementations")
                        if operation.startswith("merge_base_") and completed.stdout.decode().strip() != gt["root" if operation.endswith("ancestor") else "trunk"]: raise RuntimeError("incorrect Git merge base")
                        if operation == "small_diff" and f"diff --git a/{changed_path}".encode() not in completed.stdout: raise RuntimeError("incorrect Git diff")
                    if sample >= args.warmup:
                        record(rows, operation, "fresh_process", implementation, sample - args.warmup, elapsed, internal, len(completed.stdout))
        for sample in range(dimensions["samples"]):
            for implementation in (["scion", "git"] if sample % 2 == 0 else ["git", "scion"]):
                completed, elapsed = run([driver, "--version"] if implementation == "scion" else ["git", "--version"])
                record(rows, "process_startup", "fresh_process", implementation, sample, elapsed, output_bytes=len(completed.stdout))
        print("Measuring resident reads across deterministic warmed paths", file=sys.stderr, flush=True)
        scion = ScionWorker(driver, scion_dir, work / "scion-read.stderr", read_only=True)
        reader = GitReader(git_dir, work / "git-read.stderr")
        indices = [(sample * 104729) % dimensions["files"] for sample in range(dimensions["samples"])]
        result["configuration"]["resident_reads"] = {
            "path_indices": indices,
            "warmup_passes": args.warmup,
            "scope": "resident Scion worker and resident git cat-file --batch;one request/response each",
            "cache": "each selected path is read once in every warmup pass before measurement",
        }
        for iteration in range((args.warmup + 1) * len(indices)):
            sample = iteration % len(indices)
            index = indices[sample]
            path = file_path(index, args.fanout)
            expected = content if index == 0 else body(index % args.variants, args.blob_bytes)
            for implementation in (["scion", "git"] if iteration % 2 == 0 else ["git", "scion"]):
                if implementation == "scion":
                    response, elapsed = scion.request({"op": "read", "revision": sc["tip"], "path": path})
                    actual = response["result"].encode()
                    internal = response["operation_ns"]
                else:
                    actual, elapsed = reader.read(gt["tip"], path)
                    internal = None
                if actual != expected:
                    raise RuntimeError(f"incorrect resident {implementation} read at {path}")
                if iteration >= args.warmup * len(indices):
                    record(rows, "file_read_spread", "resident", implementation, sample, elapsed, internal, len(actual))
        reader.close(); reader = None
        scion.close(); scion = None
        result["verification"] = {"query_results": "all checked", "file_contents": "byte-identical", "changed_paths": "one matching path", "commit_parents": "identical logical DAG topology", "commit_graph": "Git --reachable --changed-paths written before query timings"}
        if reused is not None:
            result["verification"]["prepared_commits"] = "not measured; queries-only reuse"
            result["verification"]["fixture_marker"] = "identical to the source report"
        result["summary"] = summarize(rows)
        save(args.output, result)
        lookup = {(row["operation"], row["scope"], row["implementation"]): row for row in result["summary"]}
        print("operation,scope,scion_p50_us,git_p50_us,git_over_scion")
        for operation, scope in sorted({(row["operation"], row["scope"]) for row in result["summary"]}):
            scion_p50 = lookup[operation, scope, "scion"]["p50_us"]
            git_p50 = lookup[operation, scope, "git"]["p50_us"]
            print(f"{operation},{scope},{scion_p50:.3f},{git_p50:.3f},{git_p50 / scion_p50:.3f}")
        print(f"Raw results: {args.output.with_suffix('.json')}", file=sys.stderr)
        if args.keep_workdir or reused is not None: print(f"Fixture retained: {work}", file=sys.stderr)
    finally:
        for worker in (git, scion, reader):
            if worker is not None:
                try: worker.close()
                except Exception:
                    worker.process.kill()
                    worker.process.wait(timeout=5)
                    worker.log.close()
        if not args.keep_workdir and reused is None: shutil.rmtree(work)


if __name__ == "__main__":
    main()
