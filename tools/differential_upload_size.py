#!/usr/bin/env python3
"""Compare opt-in upload_max_filesize metadata and admission with PHP 8.4 HTTP SAPI."""
import argparse
import json
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from differential_multipart import BOUNDARY, body_for, php_post

# A zero php.ini upload_max_filesize disables the limit. Exact-sized files
# succeed; exceeding it affects only that upload, even before a later file.
CASES = [
    (0, "zero-unlimited", [(b"f", b"abc", b"a.bin")], [0]),
    (1, "exact", [(b"f", b"x", b"a.bin")], [0]),
    (1, "over-then-good", [(b"f", b"ab", b"a.bin"), (b"g", b"x", b"b.bin")], [1]),
    (2, "exact-two", [(b"f", b"ab", b"a.bin")], [0]),
    (2, "over-and-next", [(b"f", b"abc", b"a.bin"), (b"g", b"x", b"b.bin")], [1]),
    (2, "form-size", [(b"MAX_FILE_SIZE", b"1", None),
                       (b"f", b"ab", b"a.bin"), (b"g", b"x", b"b.bin")], [2]),
    (2, "ini-precedes-form", [(b"MAX_FILE_SIZE", b"1", None),
                              (b"f", b"abc", b"a.bin"), (b"g", b"x", b"b.bin")], [2]),
    (2, "empty-control", [(b"f", b"", b""), (b"g", b"x", b"b.bin")], [1]),
    (2, "overwrite-after-ini-error", [(b"f", b"abc", b"a.bin"),
                                      (b"f", b"x", b"b.bin")], [1]),
    (2, "nested", [(b"f[]", b"abc", b"a.bin"),
                   (b"f[]", b"x", b"b.bin")], [1]),
    (2, "stream-drain-overlong-then-good",
     [(b"f", b"X" * 512, b"large.bin"), (b"g", b"ok", b"small.bin")], [1]),
]

# The upload-count quota is applied before the size error is assigned.
# An oversized file consumes a slot, even if a later duplicate replaces it.
COMBINED_CASES = [
    (1, 2, "oversized-counts", [(b"f", b"abc", b"a.bin"),
                               (b"g", b"x", b"b.bin"), (b"tail", b"yes", None)], []),
    (2, 2, "oversized-then-good-then-skip", [(b"f", b"abc", b"a.bin"),
                                            (b"g", b"x", b"b.bin"),
                                            (b"h", b"y", b"c.bin")], [1]),
    (1, 2, "empty-then-oversized", [(b"empty", b"", b""),
                                    (b"f", b"abc", b"a.bin"),
                                    (b"g", b"x", b"b.bin")], []),
    (2, 2, "ini-precedes-form-with-count", [(b"MAX_FILE_SIZE", b"1", None),
                                           (b"f", b"abc", b"a.bin"),
                                           (b"g", b"xy", b"b.bin"),
                                           (b"h", b"x", b"c.bin")], []),
    (2, 2, "duplicate-counts-before-overwrite", [(b"f", b"abc", b"a.bin"),
                                                 (b"f", b"x", b"b.bin"),
                                                 (b"g", b"y", b"c.bin")], [1]),
    (1, 0, "zero-size-unlimited-count-still-applies", [(b"f", b"abc", b"a.bin"),
                                                      (b"g", b"x", b"b.bin")], [0]),
    (0, 2, "zero-upload-count", [(b"f", b"x", b"a.bin")], []),
]


def start_server(php_bin, limit, router, file_limit=20, post_limit=1024,
                 max_input_vars=None):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    command = [php_bin, "-d", f"upload_max_filesize={limit}",
               "-d", f"post_max_size={post_limit}",
               "-d", f"max_file_uploads={file_limit}"]
    if max_input_vars is not None:
        command.extend(["-d", f"max_input_vars={max_input_vars}"])
    command.extend(["-S", f"127.0.0.1:{port}", str(router)])
    server = subprocess.Popen(
        command,
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


def run_rust(binary, mode, limit, spool, body, file_limit=None, reverse=False):
    command = [str(binary), mode]
    if file_limit is not None and reverse:
        command.extend(["--php-max-file-uploads", str(file_limit)])
    command.extend(["--php-upload-max-filesize", str(limit)])
    if file_limit is not None and not reverse:
        command.extend(["--php-max-file-uploads", str(file_limit)])
    command.extend([BOUNDARY.decode(), str(spool)])
    proc = subprocess.run(
        command, input=body, capture_output=True, timeout=15, check=False,
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
    version = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_MAJOR_VERSION, '.', PHP_MINOR_VERSION;"],
        text=True,
    ).strip()
    if version != "8.4":
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")
    patch = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True,
    ).strip()
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    failures = 0
    with tempfile.TemporaryDirectory(prefix="rustpon-upload-size-") as temp:
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
                                    raise AssertionError(
                                        f"{key}: PHP={php[key]!r} Rust={result[key]!r}"
                                    )
                        if admitted["accepted_indices"] != accepted:
                            raise AssertionError(
                                f"indices {admitted['accepted_indices']} != {accepted}"
                            )
                    except (AssertionError, RuntimeError) as error:
                        failures += 1
                        print(f"FAIL {label} (upload_max_filesize={limit}): {error}",
                              file=sys.stderr)
            finally:
                server.terminate()
                server.wait(timeout=10)
        # Compare both option orders, both metadata and consuming admission.
        for file_limit, size_limit, label, parts, accepted in COMBINED_CASES:
            server, port = start_server(args.php_bin, size_limit, router, file_limit)
            try:
                body = body_for(parts)
                php = php_post(port, body)
                for reverse in (False, True):
                    for mode in ("--files", "--admit"):
                        try:
                            result = run_rust(args.rust_bin, mode, size_limit, spool,
                                              body, file_limit, reverse)
                            for key in ("files_tree", "files_tree_typed"):
                                if result[key] != php[key]:
                                    raise AssertionError(
                                        f"{key}: PHP={php[key]!r} Rust={result[key]!r}"
                                    )
                            if mode == "--admit" and result["accepted_indices"] != accepted:
                                raise AssertionError(
                                    f"indices {result['accepted_indices']} != {accepted}"
                                )
                        except (AssertionError, RuntimeError) as error:
                            failures += 1
                            print(f"FAIL {label} (max_file_uploads={file_limit}, "
                                  f"upload_max_filesize={size_limit}, {mode}, "
                                  f"reverse={reverse}): {error}", file=sys.stderr)
            finally:
                server.terminate()
                server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} upload quota differential case(s) failed")
    print(f"PHP {patch} upload_max_filesize SAPI differential: {len(CASES)} cases matched")
    print(f"PHP {patch} combined upload quota SAPI differential: "
          f"{len(COMBINED_CASES)} cases x 2 modes x 2 flag orders matched")


if __name__ == "__main__":
    main()
