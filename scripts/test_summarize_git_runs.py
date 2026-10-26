"""Strict repeated-report validation using small synthetic data; no benchmarks."""
import contextlib
import copy
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import summarize_git_runs as runner


def source(commit):
    return {"commit": commit * 40, "dirty": False, "status": "",
            "tracked_diff_sha256": runner.EMPTY_SHA256,
            "source_sha256": {"Cargo.toml": "1" * 64, "Cargo.lock": "2" * 64,
                              "crates/scion-engine/examples/comparison_driver.rs": "3" * 64}}


def fixture(run, samples=3):
    build = {"command": ["cargo", "build", "--release", "--locked"], "performed": True,
             "incremental": False, "source_changed_during_build": False,
             "driver_sha256": "a" * 64, "driver": "/candidate",
             "target": "x86_64-unknown-linux-gnu", "static_crt_requested": True,
             "rustflags": {"source": "RUSTFLAGS", "args": ["-C", "target-feature=+crt-static"]},
             "rustflags_provenance": "build environment used by this invocation",
             "linkage": {"format": "ELF", "linkage": "static", "interpreter": False, "needed_libraries": []}}
    old_build = {key: copy.deepcopy(build[key]) for key in (
        "command", "performed", "incremental", "source_changed_during_build", "driver_sha256")}
    old_build["driver_sha256"] = "b" * 64
    ids = {name: f"{index:064x}" for index, name in enumerate(
        ("root", "tip", "previous", "trunk", "left", "right", "side"), 1)}
    report = {
        "schema": 1, "mode": "full",
        "dimensions": {"files": 10, "commits": 8, "branch_commits": 2, "samples": samples,
                       "fanout": 2, "blob_bytes": 16, "variants": 2, "warmup": 1},
        "configuration": {
            "git": {"core.fsync": "all", "core.fsyncMethod": "fsync"},
            "scion": {"backend": "LocalStore", "sqlite": "WAL, synchronous=FULL",
                      "graph_queries": "live repository"},
            "resident_reads": {"path_indices": [0, 1, 2], "warmup_passes": 1},
            "cache": "warm", "sample_order": "six permutations",
        },
        "source": source("1"), "build": build,
        "host": {"utc": f"2026-10-25T00:00:{run:02d}Z", "platform": "test Linux",
                 "machine": "x86_64", "cpu": "fixture CPU", "logical_cpus": 2,
                 "python": "fixture Python", "rustc": "fixture Rust", "git": "git version 2.56.0",
                 "git_sha256": "d" * 64, "git_exec_path": "/fixture/git-core",
                 "filesystem": f"available space differs in run {run}"},
        "baseline": {"implementation": "scion_baseline", "source": source("2"),
                     "build": old_build, "driver_sha256": "b" * 64,
                     "source_report_sha256": "c" * 64,
                     "source_report": "/historical.json", "driver": "/baseline",
                     "store_directory": f"/fixture-{run}/scion-baseline",
                     "fixture_scope": "independent store with identical logical requests and verified identical Scion IDs"},
        "verification": runner.VERIFICATION.copy(),
        "fixture": {"scion": ids, "scion_baseline": ids.copy(),
                    "git": {key: value[-40:] for key, value in ids.items()},
                    "path": "dir0000/file000000.txt", "content": "x" * 16},
        "work_directory": f"/fixture-{run}", "samples": [],
    }
    for operation_index, (operation, scope) in enumerate(runner.OPERATIONS.items()):
        for implementation_index, implementation in enumerate(runner.IMPLEMENTATIONS, 1):
            for sample in range(samples):
                value = (1000 + run * 100 + operation_index * 10) * implementation_index + sample
                report["samples"].append({"operation": operation, "scope": scope,
                    "implementation": implementation, "sample": sample, "wall_ns": value,
                    "internal_ns": None, "output_bytes": None})
    refresh(report)
    return report


def refresh(report):
    report["summary"] = runner.summarize(report["samples"])
    report["comparisons"] = runner.comparison_rows(report["summary"])


class RepeatedRunTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.paths = [self.directory / f"run-{index}.json" for index in range(1, 6)]
        self.restore()

    def restore(self):
        for index, path in enumerate(self.paths, 1):
            path.write_text(json.dumps(fixture(index)))

    def mutate(self, action, index=0):
        report = json.loads(self.paths[index].read_text())
        action(report)
        self.paths[index].write_text(json.dumps(report))

    def test_complete_fixed_batch_retains_all_groups_controls_and_per_run_ranges(self):
        report = runner.aggregate(self.paths)
        self.assertEqual(report["completed_trials"], 5)
        self.assertEqual(len(report["inputs"]), 5)
        self.assertEqual(len(report["summary"]), 33)
        self.assertEqual(len(report["comparisons"]), 11)
        self.assertEqual(sum(row["classification"] == "control" for row in report["comparisons"]), 3)
        self.assertIn("not pooled", report["aggregation"])
        for summary in report["summary"]:
            self.assertEqual(summary["samples_per_run"], 3)
            self.assertEqual(len(summary["run_p50_us"]), 5)
            self.assertEqual(summary["min_run_p50_us"], min(summary["run_p50_us"]))
            self.assertEqual(summary["max_run_p95_us"], max(summary["run_p95_us"]))
        for item, path in zip(report["inputs"], self.paths):
            self.assertEqual(item["path"], str(path.resolve()))
            self.assertEqual(item["sha256"], runner.hashlib.sha256(path.read_bytes()).hexdigest())
        dependency = report["summarizer"]["summary_dependency"]
        self.assertEqual(dependency["function"], "compare_git.summarize")
        self.assertEqual(dependency["sha256"], runner.hashlib.sha256(Path(dependency["path"]).read_bytes()).hexdigest())
        json.dumps(report, allow_nan=False)

    def test_aggregate_ratio_uses_run_medians_not_median_ratios_or_pooled_samples(self):
        paths = self.paths[:3]
        for index, (baseline, candidate) in enumerate([(1, 1), (100, 2), (101, 100)]):
            report = fixture(index, samples=1)
            for row in report["samples"]:
                row["wall_ns"] = {"scion": candidate, "scion_baseline": baseline, "git": 200}[row["implementation"]] * 1000
            refresh(report)
            paths[index].write_text(json.dumps(report))
        report = runner.aggregate(paths, expected_trials=3)
        for row in report["comparisons"]:
            self.assertEqual(row["baseline_over_scion_ratio_of_median_run_p50"], 50)
            self.assertEqual(row["baseline_over_scion_per_run"]["values"], [1, 50, 1.01])
            self.assertEqual(row["baseline_over_scion_per_run"]["min"], 1)
            self.assertEqual(row["baseline_over_scion_per_run"]["max"], 50)

    def test_fixed_count_and_duplicate_paths_or_copied_reports_are_rejected(self):
        for paths, count in [(self.paths[:4], 5), (self.paths, 4), (self.paths, 0), (self.paths, True)]:
            with self.subTest(count=count, paths=len(paths)), self.assertRaises(ValueError):
                runner.aggregate(paths, count)
        with self.assertRaisesRegex(ValueError, "duplicate input"):
            runner.aggregate(self.paths[:4] + [self.paths[0]])
        copied = self.directory / "copied.json"
        copied.write_bytes(self.paths[0].read_bytes())
        with self.assertRaisesRegex(ValueError, "duplicate input"):
            runner.aggregate(self.paths[:4] + [copied])
        reformatted = json.loads(self.paths[0].read_text())
        reformatted["samples"].reverse()
        copied.write_text(json.dumps(reformatted, indent=2, sort_keys=True))
        with self.assertRaisesRegex(ValueError, "duplicate input report content"):
            runner.aggregate(self.paths[:4] + [copied])
        linked = self.directory / "hard-link.json"
        linked.hardlink_to(self.paths[0])
        with self.assertRaisesRegex(ValueError, "duplicate input"):
            runner.aggregate(self.paths[:4] + [linked])

    def test_recorded_native_default_flags_are_retained_without_inventing_build_flags(self):
        for index in range(len(self.paths)):
            self.mutate(lambda data: data["build"].update(
                target=None, static_crt_requested=False,
                rustflags={"source": "Cargo configuration / target defaults", "args": None},
                linkage={"format": "ELF", "linkage": "dynamic"}), index=index)
        report = runner.aggregate(self.paths)
        self.assertEqual(report["identity"]["build"]["rustflags"],
                         {"source": "Cargo configuration / target defaults", "args": None})
        self.mutate(lambda data: data["build"].update(
            rustflags={"source": "RUSTFLAGS", "args": []}), index=1)
        with self.assertRaisesRegex(ValueError, "mixed build"):
            runner.aggregate(self.paths)

    def test_missing_groups_duplicate_indices_bad_scopes_and_noninteger_wall_times_fail(self):
        changes = [lambda data: data["samples"].pop(),
                   lambda data: data["samples"].__setitem__(0, copy.deepcopy(data["samples"][1])),
                   lambda data: data["samples"][0].update(sample=3),
                   lambda data: data["samples"][0].update(sample=True),
                   lambda data: data["samples"][0].update(scope="resident"),
                   lambda data: data["samples"][0].update(implementation="unexpected"),
                   lambda data: data["samples"][0].update(wall_ns=0),
                   lambda data: data["samples"][0].update(wall_ns=-1),
                   lambda data: data["samples"][0].update(wall_ns=True),
                   lambda data: data["samples"][0].update(wall_ns=1.5),
                   lambda data: data["samples"][0].update(internal_ns=-1)]
        for index, change in enumerate(changes):
            self.restore()
            self.mutate(change)
            with self.subTest(change=index), self.assertRaises(ValueError):
                runner.aggregate(self.paths)

    def test_stored_summaries_and_ratios_must_match_complete_raw_samples(self):
        changes = [lambda data: data["summary"][0].update(p50_us=999),
                   lambda data: data["summary"][0].update(p95_us=999),
                   lambda data: data["summary"][0].update(samples=True),
                   lambda data: data["summary"].pop(),
                   lambda data: data["comparisons"][0].update(baseline_over_scion=1000),
                   lambda data: data["comparisons"][0].update(git_over_scion=True),
                   lambda data: data["comparisons"].pop()]
        for index, change in enumerate(changes):
            self.restore()
            self.mutate(change)
            with self.subTest(change=index), self.assertRaises(ValueError):
                runner.aggregate(self.paths)

    def test_mixed_workload_source_build_git_or_baseline_identity_is_rejected(self):
        changes = [lambda data: data["dimensions"].update(files=20),
                   lambda data: data["configuration"].update(cache="cold"),
                   lambda data: data["source"].update(commit="3" * 40),
                   lambda data: data["source"]["source_sha256"].update({"Cargo.lock": "4" * 64}),
                   lambda data: data["build"].update(driver_sha256="e" * 64),
                   lambda data: data["build"]["rustflags"]["args"].append("-Copt-level=1"),
                   lambda data: data["host"].update(git="git version 2.55.0"),
                   lambda data: data["host"].update(git_sha256="f" * 64),
                   lambda data: data["host"].update(cpu="another CPU"),
                   lambda data: data["baseline"].update(source_report_sha256="9" * 64),
                   lambda data: data["baseline"]["source"].update(commit="4" * 40),
                   lambda data: data["fixture"].update(content="different input")]
        for index, change in enumerate(changes):
            self.restore()
            self.mutate(change, index=1)
            with self.subTest(change=index), self.assertRaisesRegex(ValueError, "mixed"):
                runner.aggregate(self.paths)

    def test_dirty_changed_partial_replayed_or_unverified_reports_fail(self):
        changes = [lambda data: data.update(schema=True),
                   lambda data: data.update(mode="queries_only"),
                   lambda data: data.update(status="failed"),
                   lambda data: data.update(error="partial run"),
                   lambda data: data["source"].update(dirty=True),
                   lambda data: data["source"].update(status=" M dirty"),
                   lambda data: data["build"].update(source_changed_during_build=True),
                   lambda data: data["build"].update(incremental=True),
                   lambda data: data["baseline"]["source"].update(dirty=True),
                   lambda data: data["baseline"].update(driver_sha256="e" * 64),
                   lambda data: data["baseline"].update(fixture_scope="shared retained store"),
                   lambda data: data["verification"].pop("query_results"),
                   lambda data: data["fixture"]["scion_baseline"].update(tip="e" * 64),
                   lambda data: data["configuration"]["scion"].update(sqlite="synchronous=NORMAL")]
        for index, change in enumerate(changes):
            self.restore()
            self.mutate(change)
            with self.subTest(change=index), self.assertRaises(ValueError):
                runner.aggregate(self.paths)

    def test_duplicate_json_fields_nonfinite_values_and_changed_inputs_fail(self):
        original = self.paths[0].read_text()
        for invalid in (original.replace('"schema": 1', '"schema": 1, "schema": 1', 1),
                        original.replace('"wall_ns": 1100', '"wall_ns": NaN', 1)):
            self.paths[0].write_text(invalid)
            with self.assertRaises(ValueError):
                runner.aggregate(self.paths)
        self.paths[0].write_text(original)
        validate = runner.validate_report
        calls = []

        def change_after_read(report):
            result = validate(report)
            if not calls:
                self.paths[0].write_text(original + "\n")
            calls.append(True)
            return result

        with patch.object(runner, "validate_report", side_effect=change_after_read), \
                self.assertRaisesRegex(ValueError, "input changed"):
            runner.aggregate(self.paths)

    def test_cli_creates_new_output_and_never_overwrites_inputs_or_existing_output(self):
        output = self.directory / "summary.json"
        arguments = [*map(str, self.paths), "--output", str(output)]
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(runner.main(arguments), 0)
        report = json.loads(output.read_text())
        self.assertEqual(report["status"], "complete")
        for destination in (output, self.paths[0]):
            before = destination.read_bytes()
            with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                runner.main([*map(str, self.paths), "--output", str(destination)])
            self.assertEqual(error.exception.code, 2)
            self.assertEqual(destination.read_bytes(), before)
        missing = self.directory / "rejected.json"
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            runner.main([*map(str, self.paths[:4]), "--output", str(missing)])
        self.assertFalse(missing.exists())


if __name__ == "__main__":
    unittest.main()
