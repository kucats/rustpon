#!/usr/bin/env python3
"""PHP 8.4 HTTP SAPI oracle for complete HTTP bodies with truncated multipart."""
import argparse
import json
import subprocess
import tempfile
from pathlib import Path
from differential_multipart import BOUNDARY, body_for, php_post
from differential_upload_size import start_server

CASES = [
    ("single", [(b"f", b"unfinished", b"a.bin")], 0, []),
    ("empty-data", [(b"f", b"", b"a.bin")], 0, []),
    ("empty-filename", [(b"f", b"abc", b"")], 0, []),
    ("field-before", [(b"note", b"keep", None), (b"f", b"abc", b"a.bin")], 0, []),
    ("file-before", [(b"first", b"ok", b"one.bin"), (b"last", b"abc", b"two.bin")], 0, [0]),
    ("nested", [(b"f[x][]", b"payload", b"a.bin")], 0, []),
    ("buffer-boundary", [(b"f", b"Z" * 16387, b"big.bin")], 0, []),
    ("quota-precedes-partial", [(b"f", b"abc", b"a.bin")], 2, []),
    ("field-eof-single", [(b"tail", b"unfinished", None)], 0, []),
    ("field-eof-empty", [(b"tail", b"", None)], 0, []),
    ("field-eof-after-field", [(b"note", b"keep", None),
                               (b"tail", b"unfinished", None)], 0, []),
    ("field-eof-after-file", [(b"first", b"ok", b"one.bin"),
                              (b"tail", b"unfinished", None)], 0, [0]),
    ("field-eof-large", [(b"tail", b"Z" * 16387, None)], 0, []),
    ("field-header-eof", [(b"tail", b"ignored", None)], 0, [],
     b'name="tail"\r\n'),
    ("field-header-after-file", [(b"first", b"ok", b"one.bin"),
                                  (b"tail", b"ignored", None)], 0, [0],
     b'name="tail"\r\n'),
    # Fifth element: cut immediately after this complete header line, before
    # the blank line. PHP 8.4 already registers the upload with error=3.
    ("header-eof-single", [(b"f", b"abc", b"a.bin")], 0, [],
     b'filename="a.bin"\r\n'),
    ("header-eof-after-field", [(b"note", b"keep", None),
                                (b"f", b"abc", b"a.bin")], 0, [],
     b'filename="a.bin"\r\n'),
    ("header-eof-after-file", [(b"first", b"ok", b"one.bin"),
                               (b"f", b"abc", b"a.bin")], 0, [0],
     b'filename="a.bin"\r\n'),
]

def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--rust-bin", type=Path, required=True)
    cli.add_argument("--php-bin", default="php")
    args = cli.parse_args()
    patch = subprocess.check_output([args.php_bin, "-r", "echo PHP_VERSION;"], text=True).strip()
    if not patch.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {patch}")
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    checked = 0
    with tempfile.TemporaryDirectory(prefix="rustpon-partial-") as tmp:
        spool = Path(tmp)
        for case in CASES:
            label, parts, quota, expected_indices = case[:4]
            wire = body_for(parts)
            if len(case) == 5:
                marker = case[4]
                wire = wire[:wire.index(marker) + len(marker)]
            else:
                suffix = b"\r\n--" + BOUNDARY + b"--\r\n"
                assert wire.endswith(suffix)
                wire = wire[:-len(suffix)]
            server, port = start_server(args.php_bin, quota, router, post_limit=2 * 1024 * 1024)
            try:
                php = php_post(port, wire)
                proc = subprocess.run(
                    [str(args.rust_bin), "--admit", "--php-partial-upload",
                     "--php-upload-max-filesize", str(quota),
                     BOUNDARY.decode(), str(spool)],
                    input=wire, capture_output=True, timeout=15, check=False,
                )
                if proc.returncode:
                    raise AssertionError(f"{label}: Rust exit {proc.returncode}: {proc.stderr[:500]!r}")
                rust = json.loads(proc.stdout)
                for key in ("files_tree", "files_tree_typed"):
                    if php[key] != rust[key]:
                        raise AssertionError(f"{label}: {key}: PHP={php[key]!r} Rust={rust[key]!r}")
                post_proc = subprocess.run(
                    [str(args.rust_bin), "--post", "--php-partial-upload",
                     BOUNDARY.decode(), str(spool)],
                    input=wire, capture_output=True, timeout=15, check=False,
                )
                if post_proc.returncode:
                    raise AssertionError(
                        f"{label}: Rust POST exit {post_proc.returncode}: {post_proc.stderr[:500]!r}"
                    )
                rust_post = json.loads(post_proc.stdout)["post_tree"]
                if php["post_tree"] != rust_post:
                    raise AssertionError(
                        f"{label}: post_tree: PHP={php['post_tree']!r} Rust={rust_post!r}"
                    )
                if rust["accepted_indices"] != expected_indices:
                    raise AssertionError(f"{label}: accepted {rust['accepted_indices']!r}")
                if list(spool.iterdir()):
                    raise AssertionError(f"{label}: leaked staged upload paths")
                checked += 1
            finally:
                server.terminate()
                server.wait(timeout=10)
    print(f"PHP {patch} partial-upload HTTP SAPI differential: {checked} cases matched")

if __name__ == "__main__":
    main()
