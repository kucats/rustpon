#!/usr/bin/env python3
"""PHP 8.4 HTTP SAPI post_max_size oracle for rustpon's opt-in admission."""
import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

from differential_multipart import BOUNDARY, body_for, php_post
from differential_upload_size import start_server

CASES = [
    ("field-only", [(b"note", b"hello", None)]),
    ("file-only", [(b"f", b"abc", b"a.bin")]),
    ("mixed", [(b"note", b"hello", None), (b"f", b"abc", b"a.bin"),
               (b"g", b"x", b"b.bin")]),
    ("nested-files", [(b"f[]", b"abc", b"a.bin"),
                      (b"f[]", b"def", b"b.bin")]),
]


def run_rust(binary, spool, data, limit):
    proc = subprocess.run(
        [str(binary), "--admit", "--php-post-max-size", str(limit),
         BOUNDARY.decode(), str(spool)],
        input=data, capture_output=True, timeout=15, check=False,
    )
    if proc.returncode:
        raise RuntimeError(f"rustpon exit={proc.returncode}: {proc.stderr[:500]!r}")
    if list(spool.iterdir()):
        raise AssertionError("rustpon left staging paths")
    return json.loads(proc.stdout)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    patch = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True,
    ).strip()
    if not patch.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {patch}")
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    failures = 0
    checked = 0
    with tempfile.TemporaryDirectory(prefix="rustpon-post-size-") as temp:
        spool = Path(temp)
        for label, parts in CASES:
            body = body_for(parts)
            for limit in (0, len(body) - 1, len(body), len(body) + 1):
                server, port = start_server(
                    args.php_bin, 1024, router, post_limit=limit,
                )
                try:
                    php = php_post(port, body)
                    rust = run_rust(args.rust_bin, spool, body, limit)
                    for key in ("files_tree", "files_tree_typed"):
                        if php[key] != rust[key]:
                            raise AssertionError(
                                f"{key}: PHP={php[key]!r} Rust={rust[key]!r}"
                            )
                    if limit != 0 and len(body) > limit:
                        if php["post_tree"] != ["a", []]:
                            raise AssertionError("PHP did not discard oversized POST")
                        if rust["accepted_indices"]:
                            raise AssertionError("oversized upload remained accepted")
                    checked += 1
                except (AssertionError, RuntimeError, ValueError) as error:
                    failures += 1
                    print(
                        f"FAIL {label} (post_max_size={limit}, body={len(body)}): {error}",
                        file=sys.stderr,
                    )
                finally:
                    server.terminate()
                    server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} post_max_size differential case(s) failed")
    print(f"PHP {patch} post_max_size HTTP SAPI differential: {checked} cases matched")


if __name__ == "__main__":
    main()
