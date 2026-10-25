#!/usr/bin/env python3
"""Run the real Spanner metadata contract against an isolated local emulator.

Provide --gateway for Google's standalone gateway_main binary, or run the
official emulator container and use --http-endpoint / --grpc-endpoint.
No Google account or production credentials are used.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def free_port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def request(endpoint, path, payload=None):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(
        endpoint + "/v1/" + path, data=data,
        headers={"Content-Type": "application/json"},
    )
    with OPENER.open(req, timeout=10) as response:
        return json.load(response)


def wait_operation(endpoint, operation):
    deadline = time.monotonic() + 30
    while not operation.get("done"):
        if time.monotonic() > deadline:
            raise RuntimeError("Spanner emulator operation did not complete")
        time.sleep(0.1)
        operation = request(endpoint, operation["name"])
    if "error" in operation:
        raise RuntimeError("Spanner emulator administration failed: " + json.dumps(operation["error"]))


def run_tests(args, endpoint, grpc_endpoint):
    parsed = urllib.parse.urlsplit(endpoint)
    if parsed.scheme != "http" or parsed.hostname not in {"127.0.0.1", "localhost", "::1"} or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ValueError("the emulator fixture requires an explicit local HTTP endpoint")
    if grpc_endpoint.rsplit(":", 1)[0] not in {"127.0.0.1", "localhost", "[::1]"}:
        raise ValueError("the emulator fixture requires a loopback gRPC endpoint")
    deadline = time.monotonic() + 30
    while True:
        try:
            request(endpoint, "projects/scion-test/instanceConfigs")
            break
        except (OSError, urllib.error.URLError):
            if time.monotonic() > deadline:
                raise RuntimeError("Spanner emulator did not become ready") from None
            time.sleep(0.1)
    instance = "contract-" + str(time.time_ns())
    project = "projects/scion-test"
    wait_operation(endpoint, request(endpoint, project + "/instances", {
        "instanceId": instance,
        "instance": {"config": project + "/instanceConfigs/emulator-config", "displayName": "Scion integration tests", "nodeCount": 1},
    }))
    database = project + "/instances/" + instance + "/databases/scion"
    schema = (ROOT / "crates/scion-store-spanner/schema.sql").read_text()
    statements = [statement.strip() for statement in schema.split(";") if statement.strip()]
    wait_operation(endpoint, request(endpoint, project + "/instances/" + instance + "/databases", {
        "createStatement": "CREATE DATABASE scion", "extraStatements": statements,
    }))
    environment = os.environ.copy()
    environment["SPANNER_EMULATOR_HOST"] = grpc_endpoint
    environment["SCION_TEST_SPANNER_DATABASE"] = database
    environment.pop("GOOGLE_APPLICATION_CREDENTIALS", None)
    for key in ("NO_PROXY", "no_proxy"):
        previous = environment.get(key, "")
        environment[key] = previous + ("," if previous else "") + "localhost,127.0.0.1,::1"
    command = [args.cargo, "test", "-p", "scion-store-spanner", "--test", "emulator", "--locked"]
    if args.offline:
        command.append("--offline")
    command.extend(["--", "--nocapture"])
    subprocess.run(command, cwd=ROOT, env=environment, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gateway", type=Path, help="standalone gateway_main; emulator_main must be beside it")
    parser.add_argument("--http-endpoint", default="http://127.0.0.1:9020")
    parser.add_argument("--grpc-endpoint", default="127.0.0.1:9010")
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    if not args.gateway:
        run_tests(args, args.http_endpoint.rstrip("/"), args.grpc_endpoint)
        return
    gateway = args.gateway.resolve(strict=True)
    http_port, grpc_port = free_port(), free_port()
    while grpc_port == http_port:
        grpc_port = free_port()
    with tempfile.TemporaryDirectory(prefix="scion-spanner-") as directory:
        log_path = Path(directory) / "emulator.log"
        with log_path.open("wb") as log:
            process = subprocess.Popen([
                str(gateway), "--hostname", "127.0.0.1", "--grpc_port", str(grpc_port),
                "--http_port", str(http_port),
                "--grpc_binary", str(gateway.parent / "emulator_main"),
            ], cwd=gateway.parent, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                run_tests(args, f"http://127.0.0.1:{http_port}", f"127.0.0.1:{grpc_port}")
            except BaseException:
                print(log_path.read_text(errors="replace")[-8000:])
                raise
            finally:
                # gateway_main starts emulator_main as a child. Terminate the
                # isolated process group so failed tests leave no listener.
                import signal
                try:
                    os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)


if __name__ == "__main__":
    main()
