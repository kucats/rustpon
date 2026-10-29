#!/usr/bin/env python3
"""Compare bounded URL-encoded Content-Length admission to PHP 8.4 HTTP SAPI.

The Rust CLI receives the exact body length that the HTTP transport validated.
This does not test HTTP header parsing, chunked framing, or Zend integration.
"""
import argparse
import http.client
import json
import subprocess
import sys
from pathlib import Path

from differential_upload_size import start_server

CASES = [
    ("empty", b""),
    ("simple", b"a=1&b=2"),
    ("nested", b"a%5Bx%5D=1&a%5By%5D=2"),
    ("nul-name", b"x%00tail=1&b=2"),
    ("ignored-name", b"=ignored&a=1"),
    ("blank-pairs", b"a=1&&b=2&"),
    ("binary-value", b"v=%00%FF%2B"),
    ("malformed-escape", b"a=%&b=%GG"),
    ("multi-buffer", b"v=" + b"X" * (8192 + 17)),
]


def php_post(port, body):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request(
            "POST", "/", body=body,
            headers={"Content-Type": "application/x-www-form-urlencoded"},
        )
        response = connection.getresponse()
        payload = response.read()
        if response.status != 200:
            raise RuntimeError(f"PHP HTTP status={response.status}: {payload[:200]!r}")
        return json.loads(payload)["post_tree"]
    finally:
        connection.close()


def rust_post(binary, body, limit):
    command = [
        str(binary), "--php-content-length", str(len(body)),
        "--php-post-max-size", str(limit),
    ]
    process = subprocess.run(
        command, input=body, capture_output=True, timeout=15, check=False,
    )
    if process.returncode:
        raise RuntimeError(
            f"Rust exit={process.returncode}: {process.stderr[:200]!r}"
        )
    return json.loads(process.stdout)["post_tree"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    version = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True
    ).strip()
    if not version.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    checked = 0
    failures = 0
    for label, body in CASES:
        limits = sorted({0, max(1, len(body) - 1), len(body), len(body) + 1})
        for limit in limits:
            server, port = start_server(
                args.php_bin, 2 * 1024 * 1024, router, post_limit=limit,
            )
            try:
                expected = php_post(port, body)
                actual = rust_post(args.rust_bin, body, limit)
                if expected != actual:
                    raise AssertionError(f"PHP={expected!r} Rust={actual!r}")
                checked += 1
            except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                failures += 1
                print(
                    f"FAIL {label} (length={len(body)}, post_max_size={limit}): {error}",
                    file=sys.stderr,
                )
            finally:
                server.terminate()
                server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} URL-encoded HTTP SAPI differential cases failed")
    print(f"PHP {version} URL-encoded Content-Length HTTP SAPI: {checked} cases matched")


if __name__ == "__main__":
    main()
