#!/usr/bin/env python3
"""PHP 8.4 HTTP SAPI GET versus raw bounded QUERY_STRING projection."""
import argparse
import http.client
import json
import subprocess
import sys
from pathlib import Path

from differential_upload_size import start_server

CASES = [
    ("&", "default", b"a=1&b=2"),
    ("&", "percent", b"n%00tail=1&x=%00%FF"),
    ("|;", "mixed", b"a=1|b=2;c=3&d=4"),
    ("|;", "encoded-separator", b"a=1%7Cb=2|x=3"),
    ("|;", "nested", b"a%5B%5D=1|a%5B%5D=2;a%5Bx%5D=3"),
    ("|;", "blank", b"a=1||=ignored;b=2|"),
    ("", "empty-ini-fallback", b"a=1&b=2|c=3"),
    ("=", "equals-separator", b"a=1&b=2"),
    (";", "semicolon", b"a=1;b=2&c=3"),
    ("&;", "two-separators", b"a=1;b=2&c=3"),
    ("|", "binary", b"v=%00%FF%7C|x=3"),
    ("|;", "empty", b""),
    ("|;", "medium", b"v=" + b"X" * 2049),
]
QUOTA_CASES = [
    ("&", "plain", b"a=1&b=2&c=3&d=4"),
    ("&", "blank", b"&&a=1&b=2"),
    ("&", "ignored", b"a=1&=ignored&b=2&c=3"),
    ("&", "nul", b"a=1&%00ignored=2&b=3&c=4"),
    ("|;", "custom-blank", b"a=1||b=2;c=3"),
    ("|;", "custom-ignored", b"a=1|=ignored;b=2|c=3"),
    ("|;", "custom-nul", b"a=1|%00ignored=2;b=3|c=4"),
]


def php_get(port, query):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request("GET", "/?" + query.decode("ascii"))
        response = connection.getresponse()
        data = response.read()
        if response.status != 200:
            raise RuntimeError(f"PHP GET HTTP {response.status}: {data[:200]!r}")
        return json.loads(data)["get_tree"]
    finally:
        connection.close()


def rust_get(binary, query, separators, quota=None):
    cmd = [str(binary), "--php-query-string", "--php-arg-separators", separators]
    if quota is not None:
        cmd.extend(["--php-max-input-vars", str(quota)])
    result = subprocess.run(cmd, input=query, capture_output=True, timeout=15, check=False)
    if result.returncode:
        raise RuntimeError(f"Rust exit {result.returncode}: {result.stderr[:200]!r}")
    return json.loads(result.stdout)["get_tree"]


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
    router = Path(__file__).resolve().with_name("query_projection.php")
    count, failures = 0, 0
    for separators, label, query in CASES:
        server, port = start_server(
            args.php_bin, 2 * 1024 * 1024, router, arg_separators=separators,
        )
        try:
            expected = php_get(port, query)
            actual = rust_get(args.rust_bin, query, separators)
            if expected != actual:
                raise AssertionError(f"PHP={expected!r} Rust={actual!r}")
            count += 1
        except (AssertionError, RuntimeError, ValueError, KeyError) as error:
            failures += 1
            print(f"FAIL {label}, separators={separators!r}: {error}", file=sys.stderr)
        finally:
            server.terminate()
            server.wait(timeout=10)
    for quota in (0, 1, 2, 3, 4):
        for separators, label, query in QUOTA_CASES:
            server, port = start_server(
                args.php_bin, 2 * 1024 * 1024, router,
                max_input_vars=quota, arg_separators=separators,
            )
            try:
                expected = php_get(port, query)
                actual = rust_get(args.rust_bin, query, separators, quota)
                if expected != actual:
                    raise AssertionError(f"PHP={expected!r} Rust={actual!r}")
                count += 1
            except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                failures += 1
                print(
                    f"FAIL {label}, separators={separators!r}, quota={quota}: {error}",
                    file=sys.stderr,
                )
            finally:
                server.terminate()
                server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} PHP SAPI GET differential cases failed")
    print(f"PHP {version} GET HTTP SAPI: {count} cases matched")


if __name__ == "__main__":
    main()
