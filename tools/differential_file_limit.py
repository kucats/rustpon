#!/usr/bin/env python3
"""Compare opt-in max_file_uploads projection/admission against PHP 8.4 HTTP SAPI."""
import argparse
import json
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from differential_multipart import BOUNDARY, body_for, php_post

# The PHP SAPI does not charge empty filenames against max_file_uploads.
# Counts apply before duplicate-name overwrites and MAX_FILE_SIZE failures.
CASES = [
    (0, "zero", [(b"f", b"one", b"one.bin"), (b"tail", b"yes", None)], []),
    (1, "empty-before", [(b"empty", b"", b""), (b"f", b"one", b"one.bin"),
                         (b"g", b"two", b"two.bin"), (b"tail", b"yes", None)], [1]),
    (1, "empty-after", [(b"f", b"one", b"one.bin"), (b"empty", b"", b""),
                        (b"g", b"two", b"two.bin")], [0]),
    (2, "two-then-third", [(b"f", b"one", b"one.bin"), (b"g", b"two", b"two.bin"),
                           (b"h", b"three", b"three.bin"), (b"tail", b"yes", None)], [0, 1]),
    (2, "empty-then-two", [(b"empty", b"", b""), (b"f", b"one", b"one.bin"),
                           (b"g", b"two", b"two.bin"), (b"h", b"three", b"three.bin")], [1, 2]),
    (2, "form-rejected-counts", [(b"MAX_FILE_SIZE", b"1", None),
                                 (b"f", b"oversize", b"one.bin"),
                                 (b"g", b"x", b"two.bin"),
                                 (b"h", b"late", b"three.bin")], [2]),
    (2, "duplicate-counts", [(b"f", b"first", b"one.bin"),
                             (b"f", b"second", b"two.bin"),
                             (b"g", b"late", b"three.bin")], [1]),
    (2, "nested-append", [(b"f[]", b"first", b"one.bin"),
                          (b"f[]", b"second", b"two.bin"),
                          (b"f[]", b"late", b"three.bin")], [0, 1]),
]


def start_server(php_bin, limit, router):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    server = subprocess.Popen(
        [php_bin, "-d", f"max_file_uploads={limit}", "-d", "max_input_vars=10000",
         "-S", f"127.0.0.1:{port}", str(router)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    return server, port
            except OSError:
                if server.poll() is not None:
                    raise RuntimeError("PHP SAPI exited before accepting connections")
                time.sleep(0.05)
        raise RuntimeError("PHP SAPI failed to start")
    except BaseException:
        server.terminate()
        server.wait()
        raise


def run_rust(binary, mode, limit, spool, body):
    proc = subprocess.run(
        [str(binary), mode, "--php-max-file-uploads", str(limit),
         BOUNDARY.decode(), str(spool)],
        input=body, capture_output=True, timeout=15, check=False,
    )
    if proc.returncode:
        raise RuntimeError(f"{mode} failed ({proc.returncode}): {proc.stderr[:500]!r}")
    if list(spool.iterdir()):
        raise AssertionError(f"{mode} left staging paths in {spool}")
    return json.loads(proc.stdout)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    version = subprocess.check_output([args.php_bin, "-r", "echo PHP_MAJOR_VERSION, '.', PHP_MINOR_VERSION;"],
                                      text=True).strip()
    if version != "8.4":
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    failures = 0
    with tempfile.TemporaryDirectory(prefix="rustpon-file-limit-") as temp:
        spool = Path(temp)
        for limit in sorted({case[0] for case in CASES}):
            server, port = start_server(args.php_bin, limit, router)
            try:
                for _, label, parts, accepted in (case for case in CASES if case[0] == limit):
                    body = body_for(parts)
                    try:
                        php = php_post(port, body)
                        files = run_rust(args.rust_bin, "--files", limit, spool, body)
                        admitted = run_rust(args.rust_bin, "--admit", limit, spool, body)
                        for result in (files, admitted):
                            for key in ("files_tree", "files_tree_typed"):
                                if result[key] != php[key]:
                                    raise AssertionError(f"{key}: PHP={php[key]!r} Rust={result[key]!r}")
                        if admitted["accepted_indices"] != accepted:
                            raise AssertionError(f"indices {admitted['accepted_indices']} != {accepted}")
                        # The ordinary form fields still survive the file quota.
                        plain = subprocess.run(
                            [str(args.rust_bin), "--post", BOUNDARY.decode(), str(spool)],
                            input=body, capture_output=True, timeout=15, check=True,
                        )
                        if json.loads(plain.stdout)["post_tree"] != php["post_tree"]:
                            raise AssertionError("POST tree changed under max_file_uploads")
                    except (AssertionError, RuntimeError, subprocess.CalledProcessError) as error:
                        failures += 1
                        print(f"FAIL {label} (max_file_uploads={limit}): {error}", file=sys.stderr)
            finally:
                server.terminate()
                server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} max_file_uploads differential case(s) failed")
    print(f"PHP 8.4 max_file_uploads SAPI differential: {len(CASES)} cases matched")


if __name__ == "__main__":
    main()
