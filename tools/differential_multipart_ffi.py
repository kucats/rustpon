#!/usr/bin/env python3
"""Compare one-shot C ABI multipart metadata/copy to a live PHP 8.4 SAPI."""
import argparse
import ctypes
import json
import random
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import differential_multipart as corpus

BOUNDARY = corpus.BOUNDARY
OK, BUFFER_TOO_SMALL, NOT_ADMITTED = 0, 4, 7

# Expected successful physical upload wire indices, verified against PHP's
# canonical name collision and MAX_FILE_SIZE metadata behavior.
OVERRIDES = {
    "empty-upload-name": [],
    "file-overwrite": [1],
    "file-nested-then-flat": [1],
    "file-flat-then-nested": [1],
    "file-same-index": [1],
    "file-normalized-root": [1],
    "max-before-exceeded": [],
    "max-equal": [1],
    "max-after-ignored": [0],
    "max-zero-unlimited": [1],
    "max-negative": [],
    "max-case-insensitive": [],
    "max-nested": [],
    "max-changed": [3],
}


def buf(payload):
    return ctypes.create_string_buffer(payload, len(payload) + 1)


def run(fn, fixed, output_expected=True):
    length = ctypes.c_size_t(123)
    code = fn(*fixed, None, 0, ctypes.byref(length))
    if code != BUFFER_TOO_SMALL:
        assert length.value == 0
        return code, b""
    assert 0 < length.value <= 1024 * 1024
    required = length.value
    result = ctypes.create_string_buffer(required + 1)
    result.raw = b"\xa5" * (required + 1)
    short = ctypes.c_size_t(0)
    assert fn(*fixed, result, required - 1, ctypes.byref(short)) == BUFFER_TOO_SMALL
    assert short.value == required
    assert result.raw == b"\xa5" * (required + 1)
    copied = ctypes.c_size_t(0)
    assert fn(*fixed, result, required, ctypes.byref(copied)) == OK
    assert copied.value == required
    assert result.raw[required] == 0xa5
    return OK, result.raw[:required]


def copy(fn, fixed):
    length = ctypes.c_size_t(987)
    status = fn(*fixed, None, 0, ctypes.byref(length))
    if status == NOT_ADMITTED:
        assert length.value == 0
        return status, b""
    if status == OK:
        assert length.value == 0  # Valid zero-byte upload.
        return OK, b""
    assert status == BUFFER_TOO_SMALL
    n = length.value
    assert 0 < n <= 1024 * 1024
    out = ctypes.create_string_buffer(n + 1)
    out.raw = b"\xa5" * (n + 1)
    wanted = ctypes.c_size_t(0)
    assert fn(*fixed, out, n - 1, ctypes.byref(wanted)) == BUFFER_TOO_SMALL
    assert wanted.value == n and out.raw == b"\xa5" * (n + 1)
    wanted = ctypes.c_size_t(0)
    assert fn(*fixed, out, n, ctypes.byref(wanted)) == OK
    assert wanted.value == n and out.raw[n] == 0xa5
    return OK, out.raw[:n]


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--library", type=Path, required=True)
    cli.add_argument("--php-bin", default="php")
    opt = cli.parse_args()
    dll = ctypes.CDLL(str(opt.library.resolve()))
    metadata = dll.rustpon_v1_multipart_files_json
    metadata.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    metadata.restype = ctypes.c_int32
    upload = dll.rustpon_v1_multipart_upload_copy
    upload.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    upload.restype = ctypes.c_int32
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    server = subprocess.Popen(
        [opt.php_bin, "-d", "max_input_vars=10000",
         "-S", f"127.0.0.1:{port}", str(router)],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                    break
            except OSError:
                if server.poll() is not None:
                    raise RuntimeError("PHP SAPI server exited")
                time.sleep(0.05)
        else:
            raise RuntimeError("PHP SAPI unavailable")

        rng = random.Random(0x4D554C5449504152)
        names = [b"alpha", b"beta", b"a[]", b"a[0]", b"a[x]", b"words.here", b"n[-2]"]
        values = [b"", b"hello", b"\x00\xff", b"equal=sign", "東京".encode()]
        cases = list(corpus.CASES)
        for i in range(40):
            fields = [(rng.choice(names), rng.choice(values), None)
                      for _ in range(rng.randrange(1, 6))]
            cases.append((f"generated-{i:03d}", fields))

        with tempfile.TemporaryDirectory(prefix="rustpon-ffi-diff-") as spool:
            raw_path = spool.encode()
            pathbuf = buf(raw_path)
            boundary = buf(BOUNDARY)
            tested_copies = 0
            for label, parts in cases:
                body = corpus.body_for(parts)
                bodybuf = buf(body)
                fixed = (boundary, len(BOUNDARY), bodybuf, len(body),
                         pathbuf, len(raw_path))
                actual_php = corpus.php_post(port, body)
                status, encoded = run(metadata, fixed)
                assert status == OK, (label, status)
                typed = json.loads(encoded)
                assert typed == actual_php["files_tree_typed"], (label, typed, actual_php["files_tree_typed"])
                assert list(Path(spool).iterdir()) == [], (label, "staging directory leak")

                eligible = OVERRIDES.get(
                    label,
                    [i for i, (_, _, filename) in enumerate(parts) if filename is not None],
                )
                for index, (_, data, filename) in enumerate(parts):
                    if filename is None:
                        continue
                    code, payload = copy(upload, fixed + (index,))
                    if index in eligible:
                        assert code == OK and payload == data, (label, index, code, payload, data)
                    else:
                        assert code == NOT_ADMITTED and not payload, (label, index, code)
                    tested_copies += 1
                    assert list(Path(spool).iterdir()) == [], (label, "post-copy leak")
            print(f"FFI/PHP multipart: {len(cases)} typed FILES trees, "
                  f"{tested_copies} upload copies/rejections, zero staged file leaks")
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


if __name__ == "__main__":
    sys.exit(main())
