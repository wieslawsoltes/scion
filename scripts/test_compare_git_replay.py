"""Retained fixture provenance and store-routing regressions; no benchmark runs."""
import contextlib
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import compare_git


class RetainedReplayTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.work = self.root / "work"
        for name in ("git", "scion", "scion-baseline"):
            (self.work / name).mkdir(parents=True)
        for name in ("scion", "scion-baseline"):
            (self.work / name / "metadata.sqlite3").write_bytes(name.encode())
        self.driver = self.root / "candidate.csv"
        self.driver.write_bytes(b"candidate executable")
        self.baseline_driver = self.root / "baseline.csv"
        self.baseline_driver.write_bytes(b"baseline executable")
        self.baseline_report = self.root / "baseline-report.json"
        source = {
            "commit": "a" * 40, "dirty": False, "status": "",
            "source_sha256": {"Cargo.toml": "b" * 64},
        }
        build = {
            "driver_sha256": hashlib.sha256(self.baseline_driver.read_bytes()).hexdigest(),
            "source_changed_during_build": False,
        }
        self.baseline_report.write_text(json.dumps({
            "schema": 1, "source": source, "build": build, "host": {"git": "git version fixture"},
        }))
        self.baseline_info = compare_git.baseline_metadata(self.baseline_driver, self.baseline_report)
        keys = ("root", "tip", "previous", "trunk", "left", "right", "side")
        self.fixture = {
            "scion": {key: f"{index:064x}" for index, key in enumerate(keys, 1)},
            "git": {key: f"{index:040x}" for index, key in enumerate(keys, 1)},
            "path": compare_git.file_path(0, 2), "content": "x" * 32,
        }
        self.report = {
            "schema": 1, "mode": "full", "work_directory": str(self.work),
            "dimensions": {"files": 3, "commits": 4, "branch_commits": 2, "samples": 3,
                           "fanout": 2, "blob_bytes": 32, "variants": 2, "warmup": 1},
            "fixture": self.fixture, "source": copy.deepcopy(source),
            "host": {"git": "git version fixture"},
            "build": {"driver_sha256": hashlib.sha256(self.driver.read_bytes()).hexdigest()},
        }
        self.report_path = self.root / "retained.json"

    def independent(self):
        self.fixture["scion_baseline"] = self.fixture["scion"].copy()
        self.report["baseline"] = copy.deepcopy(self.baseline_info)
        self.report["baseline"].update(
            store_directory=str(self.work / "scion-baseline"),
            fixture_scope="independent store with identical logical requests and verified identical Scion IDs",
        )

    def write_report(self):
        self.report_path.write_text(json.dumps(self.report))
        (self.work / "fixture.json").write_text(json.dumps(self.fixture))

    def load(self, *, with_baseline=True):
        self.write_report()
        return compare_git.retained_fixture(self.report_path, self.baseline_info if with_baseline else None)

    def argv(self, *extra, with_baseline=True):
        result = ["compare_git.py", "--no-build", "--driver", str(self.driver), "--reuse-report", str(self.report_path)]
        if with_baseline:
            result.extend(["--baseline-driver", str(self.baseline_driver), "--baseline-report", str(self.baseline_report)])
        return result + list(map(str, extra))

    def assert_cli_rejected_before_work(self, argv, message):
        stderr = io.StringIO()
        with patch.object(sys, "argv", argv), \
                patch.object(compare_git, "comparison_build", return_value=(["cargo", "build"], {}, self.driver, "fixture-target")), \
                patch.object(compare_git, "source_metadata") as source, \
                patch.object(compare_git.subprocess, "run") as process, \
                contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as error:
            compare_git.main()
        self.assertEqual(error.exception.code, 2)
        self.assertIn(message, stderr.getvalue())
        source.assert_not_called()
        process.assert_not_called()

    def test_original_two_way_and_shared_replay_keep_the_shared_fixture(self):
        _, fixture, work, reuse = self.load()
        self.assertEqual(fixture, self.fixture)
        self.assertEqual(work, self.work)
        self.assertEqual(reuse["store_layout"], "shared_retained_store")
        self.assertEqual(reuse["baseline_store_directory"], str(self.work / "scion"))
        self.report["mode"] = "queries_only"
        self.report["baseline"] = copy.deepcopy(self.baseline_info)
        self.report["baseline"]["store_directory"] = str(self.work / "scion")
        self.assertEqual(self.load()[3]["store_layout"], "shared_retained_store")
        self.report["mode"] = "full"
        with self.assertRaisesRegex(ValueError, "aliases an independent baseline"):
            self.load()

    def test_independent_fixture_and_replays_preserve_both_stores(self):
        self.independent()
        _, fixture, _, reuse = self.load()
        self.assertEqual(fixture["scion_baseline"], fixture["scion"])
        self.assertEqual(reuse["store_layout"], "independent_retained_stores")
        self.assertEqual(reuse["baseline_store_directory"], str(self.work / "scion-baseline"))
        self.assertIn("independent retained", reuse["baseline_fixture_scope"])
        self.assertEqual(reuse["original_baseline_driver_sha256"], self.baseline_info["driver_sha256"])
        # A candidate/Git-only replay still preserves the original baseline
        # evidence needed to add the same baseline back on the next replay.
        self.report["mode"] = "queries_only"
        self.report["reuse"] = reuse
        del self.report["baseline"]
        self.assertEqual(self.load(with_baseline=False)[3]["store_layout"], "independent_retained_stores")
        self.assertEqual(self.load()[3]["baseline_store_directory"], str(self.work / "scion-baseline"))

    def test_independent_fixture_requires_matching_baseline_provenance(self):
        self.independent()
        original = copy.deepcopy(self.report["baseline"])
        for key, value in (
            ("driver_sha256", "0" * 64),
            ("source_report_sha256", "0" * 64),
            ("source", dict(original["source"], commit="c" * 40)),
            ("build", dict(original["build"], additional_flag="different")),
            ("reported_host", {"git": "different version"}),
        ):
            with self.subTest(key=key):
                self.report["baseline"] = copy.deepcopy(original)
                self.report["baseline"][key] = value
                self.write_report()
                self.assert_cli_rejected_before_work(self.argv(), "cannot reuse fixture")
        self.report["baseline"] = copy.deepcopy(original)
        self.report["baseline"]["source"]["dirty"] = True
        with self.assertRaisesRegex(ValueError, "clean source"):
            self.load(with_baseline=False)

    def test_baseline_ids_missing_metadata_and_paths_never_fall_back(self):
        self.independent()
        original = copy.deepcopy(self.report["baseline"])
        self.fixture["scion_baseline"]["tip"] = "e" * 64
        with self.assertRaisesRegex(ValueError, "IDs differ"):
            self.load()
        self.fixture["scion_baseline"] = self.fixture["scion"].copy()
        for path in (self.work / "scion", self.root / "unrelated"):
            with self.subTest(path=path):
                self.report["baseline"]["store_directory"] = str(path)
                with self.assertRaises(ValueError):
                    self.load()
        self.report["baseline"] = original
        del self.fixture["scion_baseline"]
        with self.assertRaisesRegex(ValueError, "IDs differ"):
            self.load()
        self.fixture["scion_baseline"] = self.fixture["scion"].copy()
        del self.report["baseline"]
        with self.assertRaisesRegex(ValueError, "missing its source baseline provenance"):
            self.load()

    def test_missing_independent_database_rejects_even_without_a_baseline_worker(self):
        self.independent()
        database = self.work / "scion-baseline/metadata.sqlite3"
        database.unlink()
        for with_baseline in (True, False):
            with self.subTest(with_baseline=with_baseline), self.assertRaisesRegex(ValueError, "was not retained"):
                self.load(with_baseline=with_baseline)
        database.parent.rmdir()
        with self.assertRaisesRegex(ValueError, "was not retained"):
            self.load()

    def test_independent_database_cannot_alias_candidate_storage(self):
        self.independent()
        database = self.work / "scion-baseline/metadata.sqlite3"
        database.unlink()
        try:
            os.link(self.work / "scion/metadata.sqlite3", database)
        except OSError as error:
            self.skipTest(f"hard links unavailable: {error}")
        with self.assertRaisesRegex(ValueError, "aliases the candidate database"):
            self.load()

    def test_fixture_marker_and_all_output_paths_remain_protected(self):
        self.independent()
        self.write_report()
        marker = self.work / "fixture.json"
        marker.write_text("{}")
        self.assert_cli_rejected_before_work(self.argv(), "fixture marker differs")
        self.write_report()
        for protected in (self.driver, self.baseline_driver, self.baseline_report, self.report_path, marker):
            with self.subTest(protected=protected):
                self.assert_cli_rejected_before_work(self.argv("--output", protected.with_suffix(".result")), "must not overwrite")
        # Protect the third output as well as the .json/.csv suffix outputs.
        summary_source = self.root / "provenance-summary.csv"
        summary_source.write_bytes(self.baseline_report.read_bytes())
        self.assert_cli_rejected_before_work(
            self.argv("--baseline-report", summary_source, "--output", self.root / "provenance"), "must not overwrite",
        )

    def test_unmeasured_preserved_baseline_paths_remain_protected(self):
        self.independent()
        reuse = self.load(with_baseline=False)[3]
        # Check both direct replay of the full report and replay of a previous
        # candidate/Git-only report that preserves baseline fixture evidence.
        for replay in (False, True):
            if replay:
                self.report["mode"] = "queries_only"
                self.report["reuse"] = reuse
                del self.report["baseline"]
            self.write_report()
            for protected in (self.baseline_driver, self.baseline_report):
                with self.subTest(replay=replay, protected=protected):
                    self.assert_cli_rejected_before_work(
                        self.argv("--output", protected.with_suffix(".result"), with_baseline=False), "must not overwrite",
                    )

    def test_main_routes_fresh_and_resident_queries_to_independent_stores(self):
        self.independent()
        self.write_report()
        marker = (self.work / "fixture.json").read_bytes()
        fresh_stores, resident_stores = [], []
        owner = self

        def file_content(path):
            index = int(Path(path).stem.removeprefix("file"))
            return self.fixture["content"].encode() if index == 0 else compare_git.body(index % 2, 32)

        def response(request):
            sc = self.fixture["scion"]
            operation = request["op"]
            if operation == "noop": value = True
            elif operation == "ancestry": value = request["ancestor"] == sc["root"]
            elif operation == "merge_bases": value = [sc["root"] if request["left"] == sc["root"] else sc["trunk"]]
            elif operation == "diff": value = [{"path": self.fixture["path"], "patch": "verified diff"}]
            elif operation == "read": value = file_content(request["path"]).decode()
            else: raise AssertionError(f"unexpected request: {request}")
            return {"ok": True, "result": value, "operation_ns": 1}

        def run(command, *, input=None, **unused):
            command = list(map(str, command))
            code, output = 0, b""
            if command[-1] == "--version":
                output = b"fixture version\n"
            elif command[0] != "git":
                fresh_stores.append((Path(command[0]), Path(command[1])))
                output = json.dumps(response(json.loads(input))).encode()
            else:
                arguments, gt = command[3:], self.fixture["git"]
                if arguments[0] == "merge-base":
                    if arguments[1] == "--is-ancestor": code = int(arguments[2] == gt["side"])
                    else: output = (gt["root"] if arguments[2] == gt["root"] else gt["trunk"]).encode()
                elif arguments[0] == "show": output = self.fixture["content"].encode()
                elif arguments[0] == "diff": output = f"diff --git a/{self.fixture['path']}".encode()
                elif arguments[0] not in ("config", "repack", "commit-graph", "rev-parse"):
                    raise AssertionError(f"unexpected Git command: {command}")
            return subprocess.CompletedProcess(command, code, stdout=output, stderr=b""), 10

        class Worker:
            def __init__(self, executable, directory, log, *, read_only=False):
                owner.assertTrue(read_only)
                resident_stores.append((executable, directory))

            def request(self, request): return response(request), 10
            def close(self): pass

        class Reader:
            def __init__(self, directory, log): pass
            def read(self, revision, path): return file_content(path), 10
            def close(self): pass

        with patch.object(sys, "argv", self.argv()), \
                patch.object(compare_git, "comparison_build", return_value=(["cargo", "build"], {}, self.driver, "fixture-target")), \
                patch.object(compare_git, "source_metadata", return_value=self.report["source"]), \
                patch.object(compare_git, "host_metadata", return_value=self.report["host"]), \
                patch.object(compare_git, "binary_linkage", return_value={"verification": "test fixture"}), \
                patch.object(compare_git, "run", side_effect=run), \
                patch.object(compare_git, "ScionWorker", Worker), patch.object(compare_git, "GitReader", Reader), \
                patch.object(compare_git, "save") as save, \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            compare_git.main()
        expected = {(self.driver, self.work / "scion"), (self.baseline_driver, self.work / "scion-baseline")}
        self.assertEqual(set(fresh_stores), expected)
        self.assertEqual(set(resident_stores), expected)
        result = save.call_args.args[1]
        self.assertEqual(result["mode"], "queries_only")
        self.assertEqual(result["reuse"]["store_layout"], "independent_retained_stores")
        self.assertEqual(result["baseline"]["store_directory"], str(self.work / "scion-baseline"))
        self.assertIn("independent retained", result["baseline"]["fixture_scope"])
        self.assertEqual(result["fixture"], self.fixture)
        self.assertEqual((self.work / "fixture.json").read_bytes(), marker)
        self.assertEqual({row["implementation"] for row in result["samples"]}, {"scion", "scion_baseline", "git"})
        self.assertEqual(len(result["samples"]), 9 * 3 * self.report["dimensions"]["samples"])
        self.assertEqual(self.report["dimensions"], result["dimensions"])


if __name__ == "__main__":
    unittest.main()
