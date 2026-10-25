"""Original-workload parsing and provenance guards; no Rust benchmarks run."""
import contextlib
import copy
import io
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import compare_scaling as runner


def stdout(p50=10.0):
    lines = [runner.BANNER, ",".join(runner.HEADER)]
    for operation, scale, samples in runner.ROWS:
        p95 = p50 if samples == 1 else p50 + 3
        lines.append(f"{operation},{scale},{samples},{p50:.3f},{p95:.3f}")
    return "\n".join(lines) + "\n"


class ParsingTests(unittest.TestCase):
    def test_full_original_workload_and_one_observation_initial_commits(self):
        parsed = runner.parse_rows(stdout())
        self.assertEqual(len(parsed), 11)
        self.assertEqual([(row["operation"], row["scale"], row["samples"])
                          for row in parsed], runner.ROWS)
        self.assertEqual([row["samples"] for row in parsed], [1, 50, 50] * 3 + [100, 100])
        for row in parsed:
            if row["operation"] == "initial_commit":
                self.assertEqual(row["p50_us"], row["p95_us"])

    def test_missing_extra_changed_or_malformed_rows_are_rejected(self):
        original = stdout()
        invalid = [
            original.replace(runner.BANNER, "different benchmark"),
            original.replace("operation,scale,samples", "operation,scale,count"),
            "\n".join(original.splitlines()[:-1]),
            original + "unexpected output\n",
            original.replace("initial_commit,1000,1", "initial_commit,100,1"),
            original.replace("single_file_commit,1000,50", "single_file_commit,1000,49"),
            original.replace("ancestry_negative,10000,100", "ancestry_positive,10000,100"),
            original.replace("10.000,13.000", "10.000,13.000,extra", 1),
            original.replace("10.000,13.000", "-1,13.000", 1),
            original.replace("10.000,13.000", "nan,13.000", 1),
            original.replace("10.000,13.000", "10.000,inf", 1),
            original.replace("10.000,13.000", "10.000,9.999", 1),
            original.replace("10.000,10.000", "10.000,11.000", 1),
        ]
        for output in invalid:
            with self.subTest(output=output), self.assertRaises(ValueError):
                runner.parse_rows(output)

    def test_ratio_is_of_trial_medians_not_pooled_samples_or_median_ratios(self):
        trials = [{"runs": {"baseline": {"rows": runner.parse_rows(stdout(baseline))},
                             "candidate": {"rows": runner.parse_rows(stdout(candidate))}}}
                  for baseline, candidate in [(1, 1), (100, 2), (101, 100)]]
        summary = runner.summarize(trials)
        for row in summary:
            self.assertEqual(row["baseline"]["trial_p50_us"], [1, 100, 101])
            self.assertEqual(row["candidate"]["median_trial_p50_us"], 2)
            self.assertEqual(row["speedup_ratio_of_median_trial_p50"], 50)
        self.assertIn("One initial-commit observation", summary[0]["observation_note"])
        trials[0]["runs"]["candidate"]["rows"] = runner.parse_rows(stdout(0))
        self.assertIsNone(runner.summarize(trials[:1])[0]["speedup_ratio_of_median_trial_p50"])

    def test_order_alternates_and_complete_stdout_and_stderr_are_retained(self):
        variants = {label: {"driver": f"/{label}", "source": {"root": f"/{label}-root"}}
                    for label in ("baseline", "candidate")}
        report = {"variants": variants, "trials": []}
        calls, checkpoints = [], []

        def process(command, **kwargs):
            calls.append((command, kwargs["cwd"]))
            return subprocess.CompletedProcess(command, 0, stdout().encode(), b"diagnostic\n")

        with patch.object(runner, "assert_unchanged") as unchanged, \
                patch.object(runner.subprocess, "run", side_effect=process), \
                contextlib.redirect_stderr(io.StringIO()):
            runner.run_trials(report, 3, lambda: checkpoints.append(True))
        self.assertEqual([command[0] for command, _ in calls],
                         ["/baseline", "/candidate", "/candidate", "/baseline", "/baseline", "/candidate"])
        self.assertEqual(unchanged.call_count, 14)
        self.assertEqual(len(checkpoints), 6)
        for trial in report["trials"]:
            for run in trial["runs"].values():
                self.assertEqual(run["stdout"], stdout())
                self.assertEqual(run["stderr"], "diagnostic\n")
                self.assertEqual(len(run["rows"]), 11)

    def test_failed_or_malformed_driver_stops_before_the_other_run_with_evidence(self):
        variants = {label: {"driver": f"/{label}", "source": {"root": f"/{label}-root"}}
                    for label in ("baseline", "candidate")}
        for code, output in [(1, b"partial output\n"), (0, stdout().replace(
                "single_file_commit,1000,50", "single_file_commit,1000,1").encode()),
                             (0, b"\xff")]:
            report = {"variants": variants, "trials": []}
            process = subprocess.CompletedProcess([], code, output, b"error\n")
            with patch.object(runner, "assert_unchanged"), \
                    patch.object(runner.subprocess, "run", return_value=process) as run, \
                    contextlib.redirect_stderr(io.StringIO()), self.assertRaises(ValueError):
                runner.run_trials(report, 2, lambda: None)
            self.assertEqual(run.call_count, 1)
            record = report["trials"][0]["runs"]["baseline"]
            self.assertEqual(record["stdout"].encode("utf-8", errors="surrogateescape"), output)
            self.assertNotIn("summary", report)
            json.dumps(report, allow_nan=False)

    def test_cli_persists_complete_and_failed_reports_without_overwriting_existing_files(self):
        variants = [{"driver": f"/{label}", "source": {"root": f"/{label}-root"}}
                    for label in ("baseline", "candidate")]
        with tempfile.TemporaryDirectory() as directory:
            for code in (0, 7):
                path = Path(directory) / f"report-{code}.json"
                arguments = ["--baseline-root", "/baseline-root", "--baseline-driver", "/baseline",
                             "--candidate-root", "/candidate-root", "--candidate-driver", "/candidate",
                             "--trials", "2", "--output", str(path)]
                process = subprocess.CompletedProcess([], code, stdout().encode(), b"stderr\n")
                with patch.object(runner, "variant_metadata", side_effect=copy.deepcopy(variants)), \
                        patch.object(runner, "assert_unchanged"), \
                        patch.object(runner.subprocess, "run", return_value=process), \
                        patch.object(runner.platform, "platform", return_value="fixture"), \
                        contextlib.redirect_stderr(io.StringIO()), contextlib.redirect_stdout(io.StringIO()):
                    result = runner.main(arguments)
                report = json.loads(path.read_text())
                self.assertEqual(result, 0 if code == 0 else 1)
                self.assertEqual(report["status"], "complete" if code == 0 else "failed")
                self.assertEqual(report["trials"][0]["runs"]["baseline"]["stdout"], stdout())
                self.assertEqual("summary" in report, code == 0)
                self.assertIn("finished_utc", report)
                previous = path.read_bytes()
                with patch.object(runner, "variant_metadata", side_effect=copy.deepcopy(variants)), \
                        contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                    runner.main(arguments)
                self.assertEqual(error.exception.code, 2)
                self.assertEqual(path.read_bytes(), previous)


class ProvenanceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name).resolve()
        self.root = self.directory / "source"
        self.root.mkdir()
        self.hooks = self.directory / "empty-hooks"
        self.hooks.mkdir()
        self.driver = self.directory / "driver"
        self.driver.write_bytes(b"test executable; never executed")
        self.driver.chmod(0o755)
        benchmark = self.root / runner.BENCHMARK
        benchmark.parent.mkdir(parents=True)
        original = Path(__file__).resolve().parents[1] / runner.BENCHMARK
        benchmark.write_bytes(original.read_bytes())
        (self.root / "Cargo.lock").write_text("# test lockfile\n")
        (self.root / ".gitignore").write_text("target/\n")
        subprocess.run(["git", "init", "--quiet", str(self.root)], check=True)
        runner.git(self.root, "add", ".")
        self.commit()
        self.source = runner.source_metadata(self.root)
        self.report_path = self.directory / "build.json"
        # Same required schema as the actual preserved 6a11ff7 build report.
        self.build = {
            "source_directory": str(self.root), "source_commit": self.source["commit"],
            "source_tree": self.source["tree"], "source_archive_sha256": "a" * 64,
            "source_status_before": "", "source_status_after": "", "build_exit_code": 0,
            "benchmark_source_sha256": self.source["benchmark_source_sha256"],
            "executable": str(self.driver), "executable_sha256": runner.sha256(self.driver),
            "rustc": "rustc fixture", "cargo": "cargo fixture",
            "command": ["cargo", "bench", "--locked", "--offline", "-p", "scion-engine",
                        "--bench", "scaling", "--no-run"],
            "environment": {"CARGO_INCREMENTAL": "0", "RUSTFLAGS": None},
            "timings_performed": False,
        }
        self.write_build()

    def commit(self):
        runner.git(self.root, "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                   "-c", "commit.gpgsign=false", "-c", f"core.hooksPath={self.hooks}",
                   "commit", "--quiet", "-m", "fixture")

    def write_build(self):
        self.report_path.write_text(json.dumps(self.build))

    def variant(self):
        return {"source": self.source, "driver": str(self.driver),
                "driver_sha256": runner.sha256(self.driver),
                "build": runner.build_metadata(self.report_path, self.source, runner.sha256(self.driver))}

    def test_clean_sources_pin_the_original_benchmark_and_reject_dirty_or_changed_workload(self):
        self.assertEqual(self.source["benchmark_source_sha256"], runner.BENCHMARK_SHA256)
        (self.root / "unexpected").write_text("untracked")
        with self.assertRaisesRegex(ValueError, "dirty"):
            runner.source_metadata(self.root)
        (self.root / "unexpected").unlink()
        with (self.root / runner.BENCHMARK).open("a") as stream:
            stream.write("\n// changed workload\n")
        with self.assertRaisesRegex(ValueError, "dirty"):
            runner.source_metadata(self.root)
        runner.git(self.root, "add", runner.BENCHMARK)
        self.commit()
        with self.assertRaisesRegex(ValueError, "benchmark source hash differs"):
            runner.source_metadata(self.root)

    def test_build_report_matches_complete_identity_and_retains_its_actual_flags(self):
        build = self.variant()["build"]
        self.assertEqual(build["data"], self.build)
        self.assertEqual(build["sha256"], runner.sha256(self.report_path))
        self.assertEqual(build["data"]["environment"]["CARGO_INCREMENTAL"], "0")
        unknown = runner.build_metadata(None, self.source, runner.sha256(self.driver))
        self.assertIsNone(unknown["verified_report"])
        self.assertIn("unknown", unknown["note"])

    def test_mismatched_or_incomplete_build_provenance_is_rejected(self):
        original = copy.deepcopy(self.build)
        bad_fields = {
            "source_commit": "b" * 40, "source_tree": "b" * 40,
            "benchmark_source_sha256": "b" * 64, "executable_sha256": "b" * 64,
            "source_status_before": " M changed", "source_status_after": "?? untracked",
            "build_exit_code": 1, "rustc": "", "cargo": None, "command": [], "environment": None,
        }
        for field, value in bad_fields.items():
            self.build = {**original, field: value}
            self.write_build()
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.variant()

    def test_source_executable_and_build_report_mutations_are_detected(self):
        variant = self.variant()
        runner.assert_unchanged(variant)
        self.driver.write_bytes(b"changed executable")
        with self.assertRaisesRegex(ValueError, "executable changed"):
            runner.assert_unchanged(variant)
        self.driver.write_bytes(b"test executable; never executed")
        self.build["extra"] = "changed provenance"
        self.write_build()
        with self.assertRaisesRegex(ValueError, "provenance changed"):
            runner.assert_unchanged(variant)
        self.build.pop("extra")
        self.write_build()
        (self.root / "Cargo.lock").write_text("# changed lockfile\n")
        runner.git(self.root, "add", "Cargo.lock")
        self.commit()
        with self.assertRaisesRegex(ValueError, "source identity changed"):
            runner.assert_unchanged(variant)

    def test_report_cannot_overwrite_inputs_or_dirty_either_source_tree(self):
        variants = {"baseline": self.variant(), "candidate": self.variant()}
        for path in (self.driver, self.report_path, self.root / runner.BENCHMARK):
            with self.subTest(path=path), self.assertRaisesRegex(ValueError, "already exists"):
                runner.output_path(path, variants)
        with self.assertRaisesRegex(ValueError, "Git-ignored"):
            runner.output_path(self.root / "report.json", variants)
        for path in (self.directory / "report.json", self.root / "target" / "report.json"):
            self.assertEqual(runner.output_path(path, variants), path)
        alias = self.directory / "source-link"
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "Git-ignored"):
            runner.output_path(alias / "report.json", variants)


if __name__ == "__main__":
    unittest.main()
