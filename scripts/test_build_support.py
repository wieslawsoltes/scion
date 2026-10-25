"""Regression checks for static build selection, artifact validation and provenance."""
import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import build_support
import compare_git
import release

TARGET = "x86_64-unknown-linux-gnu"
DRIVER = "comparison_driver.exe" if os.name == "nt" else "comparison_driver"
STATIC_PIE = """ELF Header:
  Type:                              DYN (Position-Independent Executable file)
  Machine:                           Advanced Micro Devices X86-64
  Entry point address:               0x12340
Program Headers:
  DYNAMIC        0x001000 0x001000 0x001000 0x000100 0x000100 RW 0x8
Dynamic section at offset 0x1000 contains 2 entries:
 0x000000000000001e (FLAGS)              BIND_NOW
 0x0000000000000000 (NULL)               0x0
"""


def readelf_result(output):
    return subprocess.CompletedProcess(["readelf"], 0, stdout=output, stderr="")


class BuildSupportTests(unittest.TestCase):
    def test_inherited_plain_and_encoded_flags(self):
        original = {"RUSTFLAGS": "-C opt-level=2 -Ctarget-feature=-crt-static"}
        result = build_support.static_crt_environment(original, TARGET)
        self.assertEqual(original["RUSTFLAGS"], "-C opt-level=2 -Ctarget-feature=-crt-static")
        self.assertEqual(build_support.rustflags(result)["args"],
                         ["-C", "opt-level=2", "-Ctarget-feature=-crt-static", "-C", "target-feature=+crt-static"])
        self.assertEqual(build_support.requested_crt(result), "static")
        encoded = {"RUSTFLAGS": "ignored", "CARGO_ENCODED_RUSTFLAGS": "-C\x1flink-arg=path with spaces"}
        result = build_support.static_crt_environment(encoded, TARGET)
        self.assertEqual(result["RUSTFLAGS"], "ignored")
        self.assertEqual(build_support.rustflags(result), {
            "source": "CARGO_ENCODED_RUSTFLAGS",
            "args": ["-C", "link-arg=path with spaces", "-C", "target-feature=+crt-static"],
        })
        self.assertEqual(build_support.static_crt_environment({"CARGO_ENCODED_RUSTFLAGS": ""}, TARGET),
                         {"CARGO_ENCODED_RUSTFLAGS": "-C\x1ftarget-feature=+crt-static"})
        self.assertIsNone(build_support.rustflags({})["args"])

    def test_unsupported_static_target(self):
        for target in ("x86_64-apple-darwin", "x86_64-pc-windows-msvc", "x86_64-unknown-linux-musl"):
            with self.subTest(target=target), self.assertRaises(ValueError):
                build_support.static_crt_environment({}, target)

    def test_static_pie_dynamic_section_is_not_a_shared_dependency(self):
        with patch.object(build_support.subprocess, "run", return_value=readelf_result(STATIC_PIE)):
            info = build_support.binary_linkage(Path("worker"), TARGET, require_static=True)
        self.assertEqual(info["linkage"], "static")
        self.assertEqual(info["elf_type"], "DYN")
        self.assertFalse(info["interpreter"])
        self.assertEqual(info["needed_libraries"], [])

    def test_interpreter_or_needed_library_rejects_static_claim(self):
        for dynamic in ("  INTERP 0x0010 0x0010\n", " 0x1 (NEEDED) Shared library: [libc.so.6]\n"):
            with self.subTest(dynamic=dynamic), patch.object(build_support.subprocess, "run", return_value=readelf_result(STATIC_PIE + dynamic)):
                with self.assertRaisesRegex(ValueError, "PT_INTERP or DT_NEEDED"):
                    build_support.binary_linkage(Path("worker"), TARGET, require_static=True)
                self.assertEqual(build_support.binary_linkage(Path("worker"), TARGET)["linkage"], "dynamic")

    def test_architecture_mismatch_and_shared_library_rejected(self):
        for invalid in (STATIC_PIE.replace("Advanced Micro Devices X86-64", "AArch64"), STATIC_PIE.replace("0x12340", "0x0")):
            with patch.object(build_support.subprocess, "run", return_value=readelf_result(invalid)), self.assertRaises(ValueError):
                build_support.binary_linkage(Path("worker"), TARGET, require_static=True)

    def test_non_linux_path_does_not_require_readelf(self):
        with patch.object(build_support.subprocess, "run") as process:
            self.assertEqual(build_support.binary_linkage(Path("worker.exe"), "x86_64-pc-windows-msvc"),
                             {"verification": "not inspected on this platform"})
            process.assert_not_called()

    def test_static_requires_readelf(self):
        with patch.object(build_support.subprocess, "run", side_effect=FileNotFoundError):
            with self.assertRaisesRegex(ValueError, "binutils"):
                build_support.binary_linkage(Path("worker"), TARGET, require_static=True)
            self.assertEqual(build_support.binary_linkage(Path("worker"), TARGET)["verification"], "readelf unavailable")

    def test_comparison_explicit_target_path_and_ordinary_mode(self):
        with patch.object(compare_git, "ENV", {"CARGO_TARGET_DIR": "target/custom", "RUSTFLAGS": "-C opt-level=2"}), \
                patch.object(compare_git, "host_target", return_value=TARGET):
            command, environment, product, target = compare_git.comparison_build(static_crt=True, offline=True)
            self.assertEqual(command[command.index("--target") + 1], TARGET)
            self.assertEqual(product, compare_git.ROOT / "target/custom" / TARGET / "release/examples" / DRIVER)
            self.assertEqual(target, TARGET)
            self.assertIn("--offline", command)
            self.assertEqual(build_support.requested_crt(environment), "static")
        with patch.object(compare_git, "ENV", {}):
            command, environment, product, target = compare_git.comparison_build()
            self.assertEqual(command, ["cargo", "build", "--release", "--locked", "-p", "scion-engine", "--example", "comparison_driver"])
            self.assertEqual(product, compare_git.ROOT / "target/release/examples" / DRIVER)
            self.assertEqual(environment, {})
            self.assertIsNone(target)

    def test_no_build_static_rejects_dynamic_before_any_measurement(self):
        with tempfile.TemporaryDirectory() as temp:
            driver = Path(temp) / "driver"
            driver.write_bytes(b"fixture")
            stderr = io.StringIO()
            with patch.object(sys, "argv", ["compare_git.py", "--static-crt", "--no-build", "--driver", str(driver)]), \
                    patch.object(compare_git, "ENV", {}), patch.object(compare_git, "host_target", return_value=TARGET), \
                    patch.object(compare_git, "source_metadata", return_value={"source_sha256": {}}), \
                    patch.object(build_support.subprocess, "run", return_value=readelf_result(STATIC_PIE + "  INTERP 0x10\n")) as process, \
                    contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as error:
                compare_git.main()
            self.assertEqual(error.exception.code, 2)
            self.assertIn("PT_INTERP or DT_NEEDED", stderr.getvalue())
            self.assertEqual(process.call_count, 1)
            self.assertEqual(process.call_args.args[0][0], "readelf")

    def test_static_output_cannot_overwrite_baseline(self):
        with tempfile.TemporaryDirectory() as temp:
            product = Path(temp) / TARGET / "release/examples" / DRIVER
            stderr = io.StringIO()
            with patch.object(sys, "argv", ["compare_git.py", "--static-crt", "--baseline-driver", str(product), "--baseline-report", str(Path(temp) / "baseline.json")]), \
                    patch.object(compare_git, "ENV", {"CARGO_TARGET_DIR": temp}), patch.object(compare_git, "host_target", return_value=TARGET), \
                    patch.object(compare_git, "baseline_metadata", return_value={"driver_sha256": "unused"}), \
                    patch.object(compare_git, "source_metadata", return_value={"source_sha256": {}}), \
                    patch.object(build_support.subprocess, "run") as process, \
                    contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as error:
                compare_git.main()
            self.assertEqual(error.exception.code, 2)
            self.assertIn("outside Cargo's output path", stderr.getvalue())
            process.assert_not_called()

    def test_release_manifest_records_target_flags_and_observed_linkage(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            build = root / "target" / TARGET / "release"
            build.mkdir(parents=True)
            for binary in release.BINARIES: (build / binary).write_bytes(b"fixture")
            for name in ("README.md", "LICENSE-MIT", "LICENSE-APACHE", "SECURITY.md"): (root / name).write_text(name)
            (root / "docs").mkdir()
            data = {"target_directory": str(root / "target"), "workspace_members": ["p"],
                    "packages": [{"id": "p", "name": "p", "version": "0.1.0"}]}
            def command(*args, **kwargs):
                return "1700000000" if args[:2] == ("git", "show") else "test-build"
            flags = {"CARGO_ENCODED_RUSTFLAGS": "-C\x1ftarget-feature=+crt-static", "RUSTFLAGS": "ignored"}
            with patch.object(release, "ROOT", root), patch.object(release, "run", side_effect=command), \
                    patch.dict(os.environ, flags, clear=True), \
                    patch.object(build_support.subprocess, "run", return_value=readelf_result(STATIC_PIE)), \
                    contextlib.redirect_stdout(io.StringIO()):
                release.bundle(data, TARGET, root / "dist", static_crt=True)
            archive = root / "dist" / f"scion-v0.1.0-{TARGET}.tar.gz"
            with tarfile.open(archive) as stream:
                metadata = json.load(stream.extractfile(f"scion-v0.1.0-{TARGET}/RELEASE.json"))
            self.assertEqual(metadata["target"], TARGET)
            self.assertEqual(metadata["rustflags"], build_support.rustflags(flags))
            self.assertEqual(metadata["requested_crt"], "static")
            self.assertTrue(metadata["static_crt_required"])
            self.assertEqual(set(metadata["linkage"]), set(release.BINARIES))
            self.assertTrue(all(info["linkage"] == "static" for info in metadata["linkage"].values()))


if __name__ == "__main__":
    unittest.main()
