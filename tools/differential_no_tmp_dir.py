#!/usr/bin/env python3
"""Live PHP 8.4 HTTP SAPI oracle for a missing upload temporary directory.

Rust's matching metadata is checked by deterministic fault-injection unit
tests. This script independently pins the real SAPI's error=6 shape and
precedence; it does not claim that a synthetic Rust CLI fault is a real SAPI.
"""
import argparse
import socket
import subprocess
import tempfile
import time
from pathlib import Path

from differential_multipart import body_for, php_post


def tagged(value, preserve_integers):
    if isinstance(value, dict):
        return ["a", [[["s", key.encode().hex()], tagged(child, preserve_integers)]
                      for key, child in value.items()]]
    if isinstance(value, int) and preserve_integers:
        return ["i", str(value)]
    return ["s", str(value).encode().hex()]


def start_php(php_bin, router, missing_dir, max_upload_bytes):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    server = subprocess.Popen(
        [php_bin, "-d", f"upload_tmp_dir={missing_dir}",
         "-d", f"sys_temp_dir={missing_dir}",
         "-d", f"upload_max_filesize={max_upload_bytes}",
         "-d", "post_max_size=2097152",
         "-S", f"127.0.0.1:{port}", str(router)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
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
        server.wait(timeout=10)
        raise


def file_error(filename, code):
    return {"name": filename, "full_path": filename, "type": "",
            "tmp_name": "", "error": code, "size": 0}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    version = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True,
    ).strip()
    if not version.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    cases = [
        ("single", 0, [(b"f", b"abc", b"a.bin")],
         {"f": file_error("a.bin", 6)}, {}),
        ("later-fields", 0,
         [(b"f", b"abc", b"a.bin"), (b"note", b"kept", None),
          (b"g", b"q", b"b.bin")],
         {"f": file_error("a.bin", 6), "g": file_error("b.bin", 6)},
         {"note": "kept"}),
        ("empty-filename", 0, [(b"f", b"", b"")],
         {"f": file_error("", 4)}, {}),
        # PHP 8.4 attempts to create the temp file before checking payload
        # size, so missing-dir error=6 takes precedence over INI size error=1.
        ("missing-dir-precedes-ini", 1, [(b"f", b"abc", b"a.bin")],
         {"f": file_error("a.bin", 6)}, {}),
    ]
    with tempfile.TemporaryDirectory(prefix="rustpon-no-tmp-") as tmp:
        missing_dir = Path(tmp) / "nonexistent-upload-directory"
        if missing_dir.exists():
            raise AssertionError("oracle directory must not exist")
        for label, quota, parts, expected_files, expected_post in cases:
            server, port = start_php(args.php_bin, router, missing_dir, quota)
            try:
                php = php_post(port, body_for(parts))
                for key, expected in [
                    ("files_tree", tagged(expected_files, False)),
                    ("files_tree_typed", tagged(expected_files, True)),
                    ("post_tree", tagged(expected_post, True)),
                ]:
                    if php[key] != expected:
                        raise AssertionError(
                            f"{label}: {key}: PHP={php[key]!r} expected={expected!r}"
                        )
                if php["files"]:
                    raise AssertionError(f"{label}: unexpected accepted upload")
            finally:
                server.terminate()
                server.wait(timeout=10)
    print(f"PHP {version} missing-temp-dir HTTP SAPI oracle: {len(cases)} cases matched")


if __name__ == "__main__":
    main()
