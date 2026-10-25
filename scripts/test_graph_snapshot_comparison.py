"""Prepared graph comparisons keep live reads and the preserved worker unchanged."""
import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import compare_git


class GraphSnapshotComparisonTests(unittest.TestCase):
    def test_only_candidate_graph_queries_receive_snapshot_arguments(self):
        snapshot = {"path": "/tmp/frozen.snapshot", "metadata": {"source": {"identity": "ab" * 32}}}
        for operation in (*compare_git.GRAPH_OPERATIONS, "file_read", "small_diff", "repository_open"):
            for implementation in ("scion", "scion_baseline"):
                with self.subTest(operation=operation, implementation=implementation):
                    command = compare_git.query_command("worker", "store", implementation, operation, snapshot)
                    self.assertEqual(command[:4], ["worker", "store", "--once", "--read-only"])
                    selected = implementation == "scion" and operation in compare_git.GRAPH_OPERATIONS
                    self.assertEqual("--graph-snapshot" in command, selected)
                    if selected:
                        self.assertEqual(command[4:], ["--graph-snapshot", snapshot["path"], "--snapshot-identity", "ab" * 32])
        self.assertEqual(compare_git.query_command("worker", "store", "scion", "ancestry_positive"),
                         ["worker", "store", "--once", "--read-only"])

    def test_preparation_records_full_process_time_size_identity_and_hash(self):
        with tempfile.TemporaryDirectory() as temp:
            work = Path(temp)
            payload = b"stand-in for the checked Rust snapshot fixture"
            def run(command):
                self.assertEqual(command[:3], ["driver", work / "scion", "--prepare-graph-snapshot"])
                self.assertEqual(Path(command[3]).parent, work)
                Path(command[3]).write_bytes(payload)
                metadata = {"repository": "benchmark", "source": {"identity": "cd" * 32, "sequence": 7},
                            "node_count": 7, "file_bytes": len(payload)}
                return subprocess.CompletedProcess(command, 0, stdout=json.dumps(metadata).encode()), 123456
            with patch.object(compare_git, "run", side_effect=run):
                report = compare_git.prepare_graph_snapshot("driver", work / "scion", work)
            self.assertEqual(report["preparation_wall_ns"], 123456)
            self.assertEqual(report["sha256"], hashlib.sha256(payload).hexdigest())
            self.assertEqual(report["metadata"]["source"]["identity"], "cd" * 32)
            self.assertEqual(Path(report["path"]).read_bytes(), payload)
            self.assertIn("no live-store fallback", report["scope"])

    def test_failed_or_malformed_export_removes_only_its_new_artifact(self):
        for kind in ("process", "repository", "identity", "empty", "size"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temp:
                work = Path(temp)
                original = work / "preserved.snapshot"
                original.write_bytes(b"preserved")
                def run(command):
                    if kind == "process":
                        raise RuntimeError("export failed")
                    Path(command[3]).write_bytes(b"new")
                    metadata = {"repository": "benchmark", "source": {"identity": "ab" * 32},
                                "node_count": 3, "file_bytes": 3}
                    if kind == "repository": metadata["repository"] = "foreign"
                    if kind == "identity": metadata["source"]["identity"] = "not-an-id"
                    if kind == "empty": metadata["node_count"] = 0
                    if kind == "size": metadata["file_bytes"] = 100
                    return subprocess.CompletedProcess(command, 0, stdout=json.dumps(metadata).encode()), 1
                with patch.object(compare_git, "run", side_effect=run), self.assertRaises((RuntimeError, ValueError)):
                    compare_git.prepare_graph_snapshot("driver", work / "scion", work)
                self.assertEqual(list(work.iterdir()), [original])
                self.assertEqual(original.read_bytes(), b"preserved")


if __name__ == "__main__":
    unittest.main()
