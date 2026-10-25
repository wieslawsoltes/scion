"""Shared build provenance and supported GNU/Linux static-CRT verification."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys

STATIC_CRT_TARGETS = {
    "x86_64-unknown-linux-gnu": "Advanced Micro Devices X86-64",
    "aarch64-unknown-linux-gnu": "AArch64",
}


def host_target(environment: dict[str, str]) -> str:
    output = subprocess.run(["rustc", "-vV"], env=environment, check=True,
                            capture_output=True, text=True).stdout
    match = re.search(r"^host: (\S+)$", output, re.MULTILINE)
    if match is None:
        raise ValueError("rustc did not report its host target")
    return match[1]


def rustflags(environment: dict[str, str]) -> dict:
    """Use Cargo's precedence; an unset environment does not prove no config flags."""
    if "CARGO_ENCODED_RUSTFLAGS" in environment:
        value = environment["CARGO_ENCODED_RUSTFLAGS"]
        return {"source": "CARGO_ENCODED_RUSTFLAGS", "args": value.split("\x1f") if value else []}
    if "RUSTFLAGS" in environment:
        return {"source": "RUSTFLAGS", "args": environment["RUSTFLAGS"].split()}
    return {"source": "Cargo configuration / target defaults", "args": None}


def static_crt_environment(environment: dict[str, str], target: str) -> dict[str, str]:
    if target not in STATIC_CRT_TARGETS:
        raise ValueError(f"static CRT is supported only for {', '.join(STATIC_CRT_TARGETS)}; got {target}")
    result = environment.copy()
    # Encoded flags take precedence over RUSTFLAGS, including an explicitly empty
    # value. Preserve inherited arguments, then make the requested feature last.
    if "CARGO_ENCODED_RUSTFLAGS" in result:
        flags = rustflags(result)["args"] + ["-C", "target-feature=+crt-static"]
        result["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    else:
        result["RUSTFLAGS"] = (result.get("RUSTFLAGS", "") + " -C target-feature=+crt-static").strip()
    return result


def requested_crt(environment: dict[str, str]) -> str:
    result = "target-default"
    flags = rustflags(environment)["args"] or []
    for index, flag in enumerate(flags):
        option = flags[index + 1] if flag == "-C" and index + 1 < len(flags) else flag.removeprefix("-C")
        if option.startswith("target-feature="):
            for feature in option.removeprefix("target-feature=").split(","):
                if feature == "+crt-static": result = "static"
                if feature == "-crt-static": result = "dynamic"
    return result


def binary_linkage(path: Path, target: str, *, require_static: bool = False) -> dict:
    """Inspect ELF startup linkage; runtime dlopen/NSS behavior is not certified."""
    if require_static and target not in STATIC_CRT_TARGETS:
        raise ValueError(f"unsupported static-CRT target: {target}")
    if "linux" not in target:
        return {"verification": "not inspected on this platform"}
    environment = dict(os.environ, LC_ALL="C")
    try:
        output = subprocess.run(["readelf", "--file-header", "--program-headers", "--dynamic", "--wide", str(path)],
                                env=environment, check=True, capture_output=True, text=True).stdout
    except FileNotFoundError:
        if require_static:
            raise ValueError("static-CRT verification requires readelf (GNU binutils)") from None
        return {"verification": "readelf unavailable"}
    machine = re.search(r"^\s*Machine:\s*(.+)$", output, re.MULTILINE)
    kind = re.search(r"^\s*Type:\s*(\S+)", output, re.MULTILINE)
    entry = re.search(r"^\s*Entry point address:\s*(0x[0-9a-fA-F]+)", output, re.MULTILINE)
    if machine is None or kind is None or kind[1] not in {"EXEC", "DYN"} or entry is None or int(entry[1], 16) == 0:
        raise ValueError(f"not an ELF executable: {path}")
    interpreter = bool(re.search(r"^\s*INTERP\s", output, re.MULTILINE))
    needed = re.findall(r"\(NEEDED\)\s+Shared library:\s*\[([^\]]+)\]", output)
    # A static PIE has a dynamic section for relocations, but no interpreter or
    # DT_NEEDED libraries. Checking PT_DYNAMIC alone would reject a valid build.
    linkage = "dynamic" if interpreter or "(NEEDED)" in output else "static"
    if target in STATIC_CRT_TARGETS and machine[1].strip() != STATIC_CRT_TARGETS[target]:
        raise ValueError(f"ELF machine {machine[1].strip()!r} does not match {target}: {path}")
    if require_static and linkage != "static":
        raise ValueError(f"--static-crt requires an ELF executable without PT_INTERP or DT_NEEDED: {path}")
    return {"verification": "readelf PT_INTERP and DT_NEEDED", "format": "ELF", "machine": machine[1].strip(),
            "elf_type": kind[1], "linkage": linkage, "interpreter": interpreter, "needed_libraries": needed}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--static-crt", action="store_true")
    parser.add_argument("binaries", nargs="+", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps({str(path): binary_linkage(path, args.target, require_static=args.static_crt)
                          for path in args.binaries}, indent=2))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
