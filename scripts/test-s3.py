#!/usr/bin/env python3
"""Run Scion's real S3 integration test against an isolated local MinIO process.

Python standard library only. Supply a MinIO executable built from the release
pinned in cloud-storage.yml. The fixture creates and deletes only its temporary
directory, listens on loopback, and never connects to a user's cloud account.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import hmac
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]


def create_bucket(endpoint: str, bucket: str, access: str, secret: str) -> None:
    now = datetime.now(timezone.utc)
    date, timestamp = now.strftime("%Y%m%d"), now.strftime("%Y%m%dT%H%M%SZ")
    host = urllib.parse.urlparse(endpoint).netloc
    payload_hash = hashlib.sha256(b"").hexdigest()
    canonical_headers = f"host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{timestamp}\n"
    signed_headers = "host;x-amz-content-sha256;x-amz-date"
    canonical_request = f"PUT\n/{bucket}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    scope = f"{date}/us-east-1/s3/aws4_request"
    to_sign = f"AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{hashlib.sha256(canonical_request.encode()).hexdigest()}"
    key = ("AWS4" + secret).encode()
    for component in (date, "us-east-1", "s3", "aws4_request"):
        key = hmac.new(key, component.encode(), hashlib.sha256).digest()
    signature = hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    request = urllib.request.Request(endpoint + "/" + bucket, data=b"", method="PUT", headers={
        "x-amz-content-sha256": payload_hash,
        "x-amz-date": timestamp,
        "Authorization": f"AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
    })
    with urllib.request.urlopen(request, timeout=10) as response:
        if response.status not in (200, 201):
            raise RuntimeError("MinIO bucket creation did not succeed")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minio", default=shutil.which("minio"), required=shutil.which("minio") is None)
    parser.add_argument("--cargo", default="cargo")
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    access, secret = "scion-" + secrets.token_hex(8), secrets.token_hex(32)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    endpoint = f"http://127.0.0.1:{port}"
    environment = os.environ.copy()
    environment.update({
        "MINIO_ROOT_USER": access, "MINIO_ROOT_PASSWORD": secret, "MINIO_BROWSER": "off",
        "AWS_ACCESS_KEY_ID": access, "AWS_SECRET_ACCESS_KEY": secret, "AWS_REGION": "us-east-1",
        "SCION_TEST_S3_ENDPOINT": endpoint, "SCION_TEST_S3_BUCKET": "scion-test",
    })
    # Explicit test credentials must not inherit a caller's session credentials,
    # role configuration, alternate endpoint, or ambient proxy for loopback.
    for key in ("AWS_SESSION_TOKEN", "AWS_WEB_IDENTITY_TOKEN_FILE", "AWS_ROLE_ARN", "AWS_ENDPOINT", "AWS_ENDPOINT_URL", "AWS_ENDPOINT_URL_S3", "AWS_PROFILE"):
        environment.pop(key, None)
    environment["NO_PROXY"] = environment.get("NO_PROXY", "") + ",127.0.0.1,localhost"
    with tempfile.TemporaryDirectory(prefix="scion-s3-") as temporary:
        directory = Path(temporary)
        with (directory / "minio.log").open("w+b") as log:
            process = subprocess.Popen([
                args.minio, "server", str(directory / "objects"),
                "--address", f"127.0.0.1:{port}", "--console-address", "127.0.0.1:0",
            ], env=environment, stdout=log, stderr=subprocess.STDOUT)
            try:
                for _ in range(120):
                    if process.poll() is not None:
                        raise RuntimeError("MinIO exited before becoming ready")
                    try:
                        with urllib.request.urlopen(endpoint + "/minio/health/ready", timeout=1) as response:
                            if response.status == 200:
                                break
                    except (OSError, urllib.error.HTTPError):
                        time.sleep(0.25)
                else:
                    raise RuntimeError("MinIO did not become ready within 30 seconds")
                create_bucket(endpoint, "scion-test", access, secret)
                command = [args.cargo, "test", "-p", "scion-store-cloud", "--all-features", "--test", "s3_live", "--locked"]
                if args.offline:
                    command.append("--offline")
                command.extend(["--", "--nocapture"])
                subprocess.run(command, cwd=ROOT, env=environment, check=True)
            except BaseException:
                log.flush()
                log.seek(0)
                details = log.read().decode(errors="replace")[-12000:]
                print(details.replace(access, "[test credential]").replace(secret, "[test credential]"))
                raise
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


if __name__ == "__main__":
    main()
