#!/usr/bin/env python3
"""Compare one-shot C ABI multipart metadata/copy to a live PHP 8.4 SAPI."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
import json
import os
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
    "max-collision-recovery": [3],
    "file-zero-overwritten": [1],
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

    session_open = dll.rustpon_v1_multipart_session_open
    session_open.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_uint64),
    ]
    session_open.restype = ctypes.c_int32
    session_files = dll.rustpon_v1_multipart_session_files_json
    session_files.argtypes = [
        ctypes.c_uint64, ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    session_files.restype = ctypes.c_int32
    session_upload = dll.rustpon_v1_multipart_session_upload_copy
    session_upload.argtypes = [
        ctypes.c_uint64, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    session_upload.restype = ctypes.c_int32
    session_close = dll.rustpon_v1_multipart_session_close
    session_close.argtypes = [ctypes.c_uint64]
    session_close.restype = ctypes.c_int32

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
        # Compare byte-for-byte PHP SAPI upload contents across the Rust
        # fixed-size copy buffer, including zero-length and binary payloads.
        for size in [0, 1, 8191, 8192, 8193, 16384, 16385, 32785]:
            data = bytes((index * 37 + 19) % 256 for index in range(size))
            cases.append((f"copy-boundary-{size}", [(b"upload", data, b"data.bin")]))
        for i in range(40):
            fields = [(rng.choice(names), rng.choice(values), None)
                      for _ in range(rng.randrange(1, 6))]
            cases.append((f"generated-{i:03d}", fields))

        with tempfile.TemporaryDirectory(prefix="rustpon-ffi-diff-") as spool:
            raw_path = spool.encode()
            pathbuf = buf(raw_path)
            boundary = buf(BOUNDARY)
            tested_copies = 0
            stateful_copies = 0
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

                handle = ctypes.c_uint64(0)
                status = session_open(*fixed, ctypes.byref(handle))
                assert status == OK and handle.value > 0, (label, status)
                baseline = len(os.listdir("/proc/self/fd"))
                try:
                    status, stateful_metadata = run(session_files, (handle.value,))
                    assert status == OK and stateful_metadata == encoded, (label, status)
                    # Session API must not reparse between size query and copy.
                    eligible = OVERRIDES.get(
                        label,
                        [i for i, part in enumerate(parts) if part[2] is not None],
                    )
                    for index, part in enumerate(parts):
                        _, data, filename = part[:3]
                        if filename is None:
                            continue
                        code, payload = copy(session_upload, (handle.value, index))
                        if index in eligible:
                            assert code == OK and payload == data, (label, index, code)
                        else:
                            assert code == NOT_ADMITTED and not payload, (label, index, code)
                        stateful_copies += 1
                    if label == "two-files":
                        # ctypes releases the GIL; concurrent accesses to one
                        # handle must serialize the file seek+read operation.
                        with ThreadPoolExecutor(max_workers=4) as workers:
                            concurrent = list(workers.map(
                                lambda _: copy(session_upload, (handle.value, 0)),
                                range(48),
                            ))
                        assert all(record == (OK, b"first") for record in concurrent), label
                    assert len(os.listdir("/proc/self/fd")) == baseline, (label, "FD leak while active")
                finally:
                    assert session_close(handle.value) == OK, label
                assert session_close(handle.value) == 8, (label, "stale handle")
                assert list(Path(spool).iterdir()) == [], (label, "session cleanup")

                eligible = OVERRIDES.get(
                    label,
                    [i for i, part in enumerate(parts) if part[2] is not None],
                )
                for index, part in enumerate(parts):
                    _, data, filename = part[:3]
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
                  f"{tested_copies} one-shot copies/rejections, "
                  f"{stateful_copies} stateful copies/rejections, "
                  "zero staged file leaks")
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


if __name__ == "__main__":
    sys.exit(main())
