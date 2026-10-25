#!/usr/bin/env python3
"""Exercise the real Linux writable mount against a local Scion authority.

This is an explicit capability gate: missing /dev/fuse or fusermount3 is a
failure, not a skipped success. It uses only the Python standard library.

Build first:
  cargo build --release -p scion-cli -p scion-server --features scion-cli/fuse --locked
Run:
  python3 scripts/smoke_fuse.py
"""
from __future__ import annotations

import argparse
from contextlib import suppress
import json
import mmap
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def checked(command: list[str], *, timeout: float = 60, **kwargs) -> subprocess.CompletedProcess:
    result = subprocess.run(command, capture_output=True, timeout=timeout, **kwargs)
    if result.returncode:
        raise RuntimeError(
            f"command failed ({result.returncode}): {' '.join(map(str, command))}\n"
            f"stdout: {result.stdout.decode(errors='replace')}\n"
            f"stderr: {result.stderr.decode(errors='replace')}"
        )
    return result


def run_smoke(scn: Path, server_bin: Path) -> None:
    if not __debug__:
        raise RuntimeError("run the smoke gate without Python optimization; assertions are required")
    if sys.platform != "linux":
        raise RuntimeError("the writable FUSE smoke gate requires Linux")
    if not Path("/dev/fuse").exists():
        raise RuntimeError("/dev/fuse is unavailable; run this required gate on a FUSE-capable Linux host")
    unmount = shutil.which("fusermount3")
    if unmount is None:
        raise RuntimeError("fusermount3 is unavailable; install the fuse3 package")
    for binary in (scn, server_bin):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise RuntimeError(f"missing executable: {binary}; build the release binaries with scion-cli/fuse")
    root = Path(tempfile.mkdtemp(prefix="scion-fuse-smoke-"))
    authority = root / "authority"
    ordinary = root / "ordinary"
    overlay = root / "overlay"
    mountpoint = root / "mount"
    mountpoint.mkdir()
    server_log = root / "server.log"
    mount_log = root / "mount.log"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    url = f"http://127.0.0.1:{port}"
    environment = os.environ.copy()
    for key in ("SCION_SERVER", "SCION_REPO", "SCION_OBJECTS", "SCION_TOKEN"):
        environment.pop(key, None)
    environment["NO_PROXY"] = "127.0.0.1,localhost"
    environment["no_proxy"] = environment["NO_PROXY"]
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    prefix = [str(scn), "--server", url, "--repo", "fuse-smoke"]

    def scn_call(*arguments: object, expect_json: bool = True):
        cmd = prefix + (["--json"] if expect_json else []) + [str(arg) for arg in arguments]
        result = checked(cmd, env=environment)
        return json.loads(result.stdout) if expect_json else result.stdout

    server = None
    mounted = None
    opened = []
    maps = []
    succeeded = False
    try:
        with server_log.open("wb") as log:
            server = subprocess.Popen(
                [str(server_bin), "--listen", f"127.0.0.1:{port}", "--data", str(authority), "--dev"],
                stdout=log, stderr=subprocess.STDOUT, env=environment,
            )
        deadline = time.monotonic() + 30
        while True:
            if server.poll() is not None:
                raise RuntimeError(f"authority exited early:\n{server_log.read_text(errors='replace')}")
            try:
                with opener.open(url + "/ready", timeout=1) as response:
                    if response.status == 200:
                        break
            except (urllib.error.URLError, TimeoutError):
                pass
            if time.monotonic() > deadline:
                raise RuntimeError("authority did not become ready within 30 seconds")
            time.sleep(0.05)
        scn_call("init", "--author", "FUSE smoke", "--ref", "main")
        scn_call("clone", "main", ordinary)
        initial = {
            "base.txt": b"hello base\n",
            "rename.txt": b"rename content\n",
            "delete.txt": b"open after unlink\n",
            "truncate.txt": b"truncate this content\n",
            "mapped.bin": bytes(16384),
            "reverted.txt": b"same before and after\n",
        }
        for name, contents in initial.items():
            (ordinary / name).write_bytes(contents)
        scn_call("commit", "--path", ordinary, "--author", "FUSE smoke", "-m", "fixture", "--ref", "main")
        scn_call("overlay", "init", "main", overlay, "--uid", os.getuid(), "--gid", os.getgid())
        with mount_log.open("wb") as log:
            mounted = subprocess.Popen(
                prefix + ["mount", "main", str(mountpoint), "--writable", "--overlay", str(overlay)],
                stdout=log, stderr=subprocess.STDOUT, env=environment,
            )
        deadline = time.monotonic() + 30
        while not (overlay / "mounted.json").exists():
            if mounted.poll() is not None:
                raise RuntimeError(f"mount exited early:\n{mount_log.read_text(errors='replace')}")
            if time.monotonic() > deadline:
                raise RuntimeError("FUSE mount did not initialize within 30 seconds")
            time.sleep(0.05)
        if not os.path.ismount(mountpoint):
            raise AssertionError("the mount session record exists without an active filesystem mount")

        # Immutable data is fetched on access, and the mount exposes exactly it.
        assert (mountpoint / "base.txt").read_bytes() == initial["base.txt"]
        assert set(os.listdir(mountpoint)) == set(initial)
        (mountpoint / "newdir").mkdir()
        new = (mountpoint / "newdir" / "new.txt").open("w+b", buffering=0)
        opened.append(new)
        new.seek(3)
        new.write(b"written")
        os.fsync(new.fileno())
        assert (mountpoint / "newdir" / "new.txt").read_bytes() == b"\0\0\0written"
        with (mountpoint / "newdir" / "new.txt").open("ab", buffering=0) as append:
            append.write(b" appended")
            os.fsync(append.fileno())
        assert (mountpoint / "newdir" / "new.txt").read_bytes() == b"\0\0\0written appended"
        os.chmod(mountpoint / "newdir" / "new.txt", 0o755)
        assert (mountpoint / "newdir" / "new.txt").stat().st_mode & 0o111
        (mountpoint / "empty").mkdir()
        (mountpoint / "empty").rmdir()
        os.rename(mountpoint / "newdir", mountpoint / "renamed-dir")
        # The still-open handle refers to the same inode through the directory move.
        new.seek(3)
        assert new.read(7) == b"written"

        renamed = (mountpoint / "rename.txt").open("r+b", buffering=0)
        opened.append(renamed)
        before_inode = os.fstat(renamed.fileno()).st_ino
        os.rename(mountpoint / "rename.txt", mountpoint / "renamed.txt")
        assert (mountpoint / "renamed.txt").stat().st_ino == before_inode
        assert renamed.read() == initial["rename.txt"]
        deleted = (mountpoint / "delete.txt").open("r+b", buffering=0)
        opened.append(deleted)
        os.unlink(mountpoint / "delete.txt")
        assert deleted.read() == initial["delete.txt"]
        deleted.seek(0)
        deleted.write(b"unlinked data")
        os.fsync(deleted.fileno())
        assert os.fstat(deleted.fileno()).st_nlink == 0
        with (mountpoint / "truncate.txt").open("r+b", buffering=0) as trunc:
            trunc.truncate(3)
            os.fsync(trunc.fileno())
        os.symlink("base.txt", mountpoint / "base-link")
        assert os.readlink(mountpoint / "base-link") == "base.txt"
        assert (mountpoint / "base-link").read_bytes() == initial["base.txt"]
        directory_fd = os.open(mountpoint, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)

        # Deliberately keep mmap dirty and open; overlay commit must flush kernel
        # writeback through its validated syncfs barrier before reading backing data.
        mapped_file = (mountpoint / "mapped.bin").open("r+b", buffering=0)
        opened.append(mapped_file)
        mapping = mmap.mmap(mapped_file.fileno(), 0)
        maps.append(mapping)
        mapping[8192:8199] = b"mapped!"
        reverted = (mountpoint / "reverted.txt").open("r+b", buffering=0)
        opened.append(reverted)
        reverted.write(b"DIFF")
        reverted.seek(0)
        reverted.write(initial["reverted.txt"])
        os.fsync(reverted.fileno())
        first = scn_call("overlay", "commit", overlay, "--author", "FUSE smoke", "-m", "mounted changes", "--ref", "main")
        assert scn_call("overlay", "status", overlay)["changes"] == []
        expected_mapping = bytearray(initial["mapped.bin"])
        expected_mapping[8192:8199] = b"mapped!"
        for name, expected in {
            "base.txt": initial["base.txt"],
            "renamed.txt": initial["rename.txt"],
            "truncate.txt": b"tru",
            "renamed-dir/new.txt": b"\0\0\0written appended",
            "base-link": b"base.txt",
            "mapped.bin": bytes(expected_mapping),
        }.items():
            actual = scn_call("cat", "main", name, expect_json=False)
            assert actual == expected, (name, actual[:100], expected[:100])
        absent = subprocess.run(prefix + ["cat", "main", "delete.txt"], capture_output=True, env=environment)
        assert absent.returncode != 0, "unlinked base file survived the commit"

        # Existing mmap/file handles must dirty the newly committed baseline.
        # reverted.txt was omitted from the previous delta because its edit was a
        # no-op; clearing that edit must also reset its dirty-transition marker.
        mapping[8192:8199] = b"second!"
        reverted.seek(0)
        reverted.write(b"next")
        os.fsync(reverted.fileno())
        second = scn_call("overlay", "commit", overlay, "--author", "FUSE smoke", "-m", "existing handles after commit", "--ref", "main")
        assert first["revision"] != second["revision"]
        expected_mapping[8192:8199] = b"second!"
        assert scn_call("cat", "main", "mapped.bin", expect_json=False) == bytes(expected_mapping)
        assert scn_call("cat", "main", "reverted.txt", expect_json=False) == b"next" + initial["reverted.txt"][4:]
        assert scn_call("overlay", "status", overlay)["changes"] == []
        for mapping in maps:
            mapping.close()
        maps.clear()
        for file in opened:
            file.close()
        opened.clear()
        checked([unmount, "-u", str(mountpoint)])
        mounted.wait(timeout=30)
        if mounted.returncode:
            raise RuntimeError(f"mount process exited with {mounted.returncode}:\n{mount_log.read_text(errors='replace')}")
        assert not os.path.ismount(mountpoint)
        assert scn_call("overlay", "info", overlay)["revision"] == second["revision"]
        assert scn_call("overlay", "status", overlay)["changes"] == []
        succeeded = True
        print(json.dumps({
            "result": "passed", "first_commit": first["revision"], "second_commit": second["revision"],
            "checks": ["lazy reads", "create/write/append/holes", "truncate", "directory rename", "open inode identity", "unlink while open", "symlink", "chmod", "file and directory fsync", "dirty mmap commit barrier", "no-op dirty reset", "two server commits", "clean unmount/reopen"],
        }, indent=2))
    finally:
        for mapping in maps:
            with suppress(OSError, BufferError):
                mapping.close()
        for file in opened:
            with suppress(OSError):
                file.close()
        if mounted is not None and mounted.poll() is None:
            # Unmount before removing any backing files. Never recursively remove
            # a still-mounted namespace while reporting a failed test.
            with suppress(OSError, subprocess.TimeoutExpired):
                subprocess.run([unmount, "-u", str(mountpoint)], capture_output=True, timeout=15)
            try:
                mounted.wait(timeout=15)
            except subprocess.TimeoutExpired:
                mounted.terminate()
                try:
                    mounted.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    mounted.kill()
                    mounted.wait(timeout=5)
        if server is not None and server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=10)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=5)
        if succeeded and not os.path.ismount(mountpoint):
            shutil.rmtree(root)
        else:
            print(f"FUSE smoke artifacts preserved at {root}", file=sys.stderr)
            for path in (server_log, mount_log):
                if path.exists():
                    print(f"--- {path.name} ---\n{path.read_text(errors='replace')[-16000:]}", file=sys.stderr)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scn", type=Path, default=Path("target/release/scn"))
    parser.add_argument("--server-bin", type=Path, default=Path("target/release/scion-server"))
    args = parser.parse_args()
    run_smoke(args.scn.resolve(), args.server_bin.resolve())


if __name__ == "__main__":
    main()
