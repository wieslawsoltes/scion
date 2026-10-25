#!/usr/bin/env python3
"""Validate metadata, assemble releases, and publish crates in dependency order.

Python 3.11+; standard library only. Run from any directory. Publication requires
the explicit --execute flag and a CARGO_REGISTRY_TOKEN in the environment.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile

from build_support import STATIC_CRT_TARGETS, binary_linkage, requested_crt, rustflags

ROOT = Path(__file__).resolve().parents[1]
BINARIES = ("scn", "scion-server", "scion-cache", "scion-daemon")


def run(*args: str, capture: bool = False) -> str:
    result = subprocess.run(args, cwd=ROOT, check=True, text=True,
                            stdout=subprocess.PIPE if capture else None)
    return result.stdout.strip() if capture else ""


def metadata() -> dict:
    return json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1", "--locked", capture=True))


def packages(data: dict) -> list[dict]:
    members = set(data["workspace_members"])
    return sorted((p for p in data["packages"] if p["id"] in members), key=lambda p: p["name"])


def version(data: dict) -> str:
    versions = {p["version"] for p in packages(data)}
    if len(versions) != 1:
        raise ValueError(f"All workspace crates must share a release version; got {sorted(versions)}")
    return versions.pop()


def check_metadata(data: dict) -> None:
    by_name = {p["name"]: p for p in packages(data)}
    for package in by_name.values():
        for key in ("description", "license", "repository", "readme", "rust_version"):
            if not package.get(key):
                raise ValueError(f"{package['name']}: missing package.{key}")
        if package["publish"] == []:
            raise ValueError(f"{package['name']}: publish=false prevents independent shipment")
        for dependency in package["dependencies"]:
            if dependency["name"] in by_name and dependency["req"] == "*":
                raise ValueError(f"{package['name']}: path dependency {dependency['name']} needs a version")
    version(data)
    print(f"Validated metadata for {len(by_name)} publishable crates.")


def ordered_packages(data: dict) -> list[dict]:
    remaining = {p["name"]: p for p in packages(data)}
    result = []
    while remaining:
        ready = sorted(name for name, p in remaining.items()
                       if not any(d["name"] in remaining and d["kind"] != "dev"
                                  for d in p["dependencies"]))
        if not ready:
            raise ValueError(f"Dependency cycle between publishable crates: {sorted(remaining)}")
        for name in ready:
            result.append(remaining.pop(name))
    return result


def check_tag(data: dict) -> None:
    tag = os.environ.get("SCION_RELEASE_TAG", "")
    expected = "v" + version(data)
    if tag != expected or not re.fullmatch(r"v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?", tag):
        raise ValueError(f"SCION_RELEASE_TAG must equal {expected!r}, got {tag!r}")
    tagged = run("git", "rev-parse", f"refs/tags/{tag}^{{commit}}", capture=True)
    current = run("git", "rev-parse", "HEAD", capture=True)
    if tagged != current:
        raise ValueError(f"HEAD is not the commit identified by {tag}")
    print(f"Validated {tag} at {current}.")


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def checksum(path: Path) -> None:
    path.with_name(path.name + ".sha256").write_text(f"{sha256(path)}  {path.name}\n", encoding="utf-8")


def bundle(data: dict, target: str, output: Path, *, static_crt: bool = False) -> None:
    if static_crt and target not in STATIC_CRT_TARGETS:
        raise ValueError(f"unsupported static-CRT target: {target}")
    if static_crt and requested_crt(os.environ) != "static":
        raise ValueError("bundle --static-crt requires the same +crt-static Rust flags used for the build")
    suffix = ".exe" if "windows" in target else ""
    build = Path(data["target_directory"]) / target / "release"
    name = f"scion-v{version(data)}-{target}"
    output.mkdir(parents=True, exist_ok=True)
    epoch = int(run("git", "show", "-s", "--format=%ct", "HEAD", capture=True))
    with tempfile.TemporaryDirectory(prefix="scion-release-") as temp:
        staging = Path(temp) / name
        staging.mkdir()
        linkage = {}
        for binary in BINARIES:
            source = build / (binary + suffix)
            if not source.is_file():
                raise ValueError(f"Missing binary: {source}")
            linkage[binary] = binary_linkage(source, target, require_static=static_crt)
            shutil.copy2(source, staging / source.name)
        for file in ("README.md", "LICENSE-MIT", "LICENSE-APACHE", "SECURITY.md"):
            shutil.copy2(ROOT / file, staging / file)
        shutil.copytree(ROOT / "docs", staging / "docs")
        (staging / "RELEASE.json").write_text(json.dumps({
            "version": version(data), "target": target,
            "commit": run("git", "rev-parse", "HEAD", capture=True),
            "rustc": run("rustc", "--version", capture=True),
            "cargo": run("cargo", "--version", capture=True),
            "binaries": list(BINARIES),
            "rustflags": rustflags(os.environ),
            "rustflags_provenance": "bundle environment; use the same environment for Cargo build and bundle",
            "requested_crt": requested_crt(os.environ),
            "static_crt_required": static_crt,
            "linkage": linkage,
        }, indent=2) + "\n", encoding="utf-8")
        if suffix:
            archive = output / (name + ".zip")
            with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as stream:
                for path in sorted(staging.rglob("*")):
                    if path.is_file():
                        info = zipfile.ZipInfo(path.relative_to(staging.parent).as_posix(),
                                               time.gmtime(max(epoch, 315532800))[:6])
                        info.compress_type = zipfile.ZIP_DEFLATED
                        info.external_attr = 0o100644 << 16
                        stream.writestr(info, path.read_bytes())
        else:
            archive = output / (name + ".tar.gz")
            def normalized(info: tarfile.TarInfo) -> tarfile.TarInfo:
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                info.mtime = epoch
                info.mode = 0o755 if info.isdir() or Path(info.name).name in BINARIES else 0o644
                return info
            with archive.open("wb") as raw, gzip.GzipFile(fileobj=raw, mode="wb", mtime=epoch, filename="") as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as stream:
                    stream.add(staging, arcname=name, filter=normalized)
        checksum(archive)
        print(archive)


def source(data: dict, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=True)
    name = f"scion-v{version(data)}-source"
    archive = output.resolve() / (name + ".tar.gz")
    run("git", "archive", "--format=tar.gz", f"--prefix={name}/", f"--output={archive}", "HEAD")
    checksum(archive)
    print(archive)


def collect(source_dir: Path, output: Path) -> None:
    output.mkdir(parents=True, exist_ok=True)
    accepted = (".tar.gz", ".zip", ".crate")
    count = 0
    for path in sorted(source_dir.rglob("*")):
        if path.is_file() and path.name.endswith(accepted):
            destination = output / path.name
            if destination.exists() and sha256(destination) != sha256(path):
                raise ValueError(f"Conflicting release artifacts named {path.name}")
            shutil.copy2(path, destination)
            count += 1
    if not count:
        raise ValueError(f"No release artifacts found under {source_dir}")
    assets = sorted(p for p in output.iterdir() if p.is_file() and p.name.endswith(accepted))
    (output / "SHA256SUMS").write_text("".join(f"{sha256(p)}  {p.name}\n" for p in assets), encoding="utf-8")
    print(f"Collected {len(assets)} artifacts and SHA256SUMS.")


def registry_version(name: str, crate_version: str) -> dict | None:
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/{name}/{crate_version}",
        headers={"User-Agent": "scion-release (https://github.com/wieslawsoltes/scion)"})
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)["version"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def publish(data: dict, execute: bool) -> None:
    check_metadata(data)
    if execute and not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise ValueError("Set CARGO_REGISTRY_TOKEN before executing publication")
    if execute:
        check_tag(data)
    for package in ordered_packages(data):
        name, crate_version = package["name"], package["version"]
        print(f"Publish {name} {crate_version}", flush=True)
        if not execute:
            continue
        archive = Path(data["target_directory"]) / "package" / f"{name}-{crate_version}.crate"
        if not archive.exists():
            raise ValueError("Run cargo package --workspace --all-features --locked first")
        published = registry_version(name, crate_version)
        if published is not None:
            if published["checksum"] != sha256(archive):
                raise ValueError(f"{name} {crate_version} already exists with different contents")
            print("  Already published with matching checksum; continuing.", flush=True)
            continue
        run("cargo", "publish", "--package", name, "--all-features", "--locked")
        # Wait for index propagation before a dependent crate is packaged/published.
        for attempt in range(24):
            if registry_version(name, crate_version) is not None:
                break
            time.sleep(5)
        else:
            raise ValueError(f"Timed out waiting for {name} {crate_version}; rerun to resume")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("check-metadata", "check-tag", "order", "version"):
        commands.add_parser(name)
    binaries = commands.add_parser("bundle")
    binaries.add_argument("--target", required=True)
    binaries.add_argument("--static-crt", action="store_true", help="verify supported GNU/Linux static linkage and matching build flags")
    binaries.add_argument("--output", type=Path, default=ROOT / "dist")
    sources = commands.add_parser("source")
    sources.add_argument("--output", type=Path, default=ROOT / "dist")
    collection = commands.add_parser("collect")
    collection.add_argument("--input", type=Path, required=True)
    collection.add_argument("--output", type=Path, default=ROOT / "dist")
    publication = commands.add_parser("publish")
    publication.add_argument("--execute", action="store_true")
    args = parser.parse_args()
    if args.command == "collect":
        collect(args.input, args.output)
        return
    data = metadata()
    if args.command == "check-metadata":
        check_metadata(data)
    elif args.command == "check-tag":
        check_tag(data)
    elif args.command == "order":
        print("\n".join(p["name"] for p in ordered_packages(data)))
    elif args.command == "version":
        print(version(data))
    elif args.command == "bundle":
        bundle(data, args.target, args.output, static_crt=args.static_crt)
    elif args.command == "source":
        source(data, args.output)
    elif args.command == "publish":
        publish(data, args.execute)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
