#!/usr/bin/env python3
"""Compare bounded scalar Cookie-header projection with PHP 8.4 HTTP SAPI."""
import argparse
import http.client
import json
import subprocess
import sys
from pathlib import Path

from differential_upload_size import start_server

CASES = [
    ("basic", b"a=1; b=2"),
    ("first-wins", b"a=1; a=2; b=3"),
    ("normalized-collision", b"a.b=1; a_b=2"),
    ("reverse-collision", b"a_b=1; a.b=2"),
    ("leading-and-trailing-whitespace", b" a = 1 ;  c =d "),
    ("value-percent-and-plus", b"a=hello+world; p=%41%2B%00"),
    ("quoted-value", b'x="quoted value"; y=ok'),
    ("empty-components", b";foo=1;;bar=2;"),
    ("literal-percent-name", b"a%2Eb=foo; a.b=bar"),
    ("binary-escape", b"a=%FF%00; b=tail"),
    ("equals-in-value", b"a=b=c; c=3"),
    ("no-equals", b"foo; bar=2"),
    ("empty-name", b"=ignored; foo=ok"),
    ("tab-after-semicolon", b"a=1;\tb=2"),
    ("case-sensitive", b"a=1; A=2"),
    ("multi-block", b"x=" + b"Z" * 4097 + b"; y=tail"),
]


def php_cookie(port, header):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request(
            "GET", "/", headers={"Cookie": header.decode("latin1")}
        )
        response = connection.getresponse()
        data = response.read()
        if response.status != 200:
            raise RuntimeError(f"PHP HTTP {response.status}: {data[:200]!r}")
        return json.loads(data)["cookie_pairs"]
    finally:
        connection.close()


def rust_cookie(binary, header):
    result = subprocess.run(
        [str(binary)], input=header, capture_output=True, timeout=15, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"Rust exit {result.returncode}: {result.stderr[:300]!r}")
    return json.loads(result.stdout)["cookie_pairs"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    version = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True,
    ).strip()
    if not version.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")

    router = Path(__file__).resolve().with_name("cookie_projection.php")
    server, port = start_server(args.php_bin, 1, router)
    passed = 0
    failures = 0
    try:
        for label, header in CASES:
            try:
                expected = php_cookie(port, header)
                actual = rust_cookie(args.rust_bin, header)
                if expected != actual:
                    raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
                passed += 1
            except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                failures += 1
                print(f"FAIL Cookie {label}: {error}", file=sys.stderr)
    finally:
        server.terminate()
        server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} PHP 8.4 HTTP Cookie scalar cases failed")
    print(f"PHP {version} Cookie HTTP SAPI: {passed} scalar cases matched")


if __name__ == "__main__":
    main()
