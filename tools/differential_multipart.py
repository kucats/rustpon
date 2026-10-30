#!/usr/bin/env python3
"""Compare PHP's real multipart HTTP registration with rustpon's POST adapter."""
import argparse
import http.client
import json
import random
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

BOUNDARY = b"rustpon-diff"
# (field name, bytes, optional filename). No personal or external fixtures.
CASES = [
    ("empty", []),
    ("single", [(b"alpha", b"one", None)]),
    ("empty-value", [(b"alpha", b"", None)]),
    ("binary", [(b"bytes", b"a\x00\xffz", None)]),
    ("utf8", [(b"message", "日本語".encode(), None)]),
    ("duplicates", [(b"alpha", b"old", None), (b"alpha", b"new", None)]),
    ("append", [(b"a[]", b"one", None), (b"a[]", b"two", None)]),
    ("nested", [(b"a[x][y]", b"nested", None)]),
    ("bracket-leading-space", [(b"a[ x]", b"v", None)]),
    ("bracket-two-spaces", [(b"a[  ]", b"v", None)]),
    ("bracket-single-space-append", [(b"a[ ]", b"v", None)]),
    ("unterminated-first-bracket", [(b"a[x", b"v", None)]),
    ("unterminated-empty-bracket", [(b"a[", b"v", None)]),
    ("unterminated-first-space", [(b"a[ x", b"v", None)]),
    ("unterminated-first-dots", [(b"a[foo.bar[", b"v", None)]),
    ("unterminated-nested-bracket", [(b"a[x][", b"v", None)]),
    ("unterminated-nested-key", [(b"a[x][y", b"v", None)]),
    ("unterminated-third-bracket", [(b"a[x][y][", b"v", None)]),
    ("file-bracket-leading-space", [(b"f[ x]", b"file", b"x.bin")]),
    # PHP 8.4 stops accepting later uploads after a malformed file name,
    # while ordinary form fields following the bad control still register.
    ("file-invalid-first", [(b"bad[x", b"bad", b"bad.bin"),
                            (b"later", b"ok", b"later.bin"),
                            (b"plain", b"still", None)]),
    ("file-invalid-after-good", [(b"good", b"first", b"good.bin"),
                                 (b"bad[x", b"bad", b"bad.bin"),
                                 (b"later", b"ok", b"later.bin"),
                                 (b"plain", b"still", None)]),
    ("file-invalid-trailing-bracket", [(b"bad]", b"bad", b"bad.bin"),
                                       (b"later", b"ok", b"later.bin")]),
    ("file-invalid-nested-tail", [(b"bad[x][", b"bad", b"bad.bin"),
                                  (b"later", b"ok", b"later.bin")]),
    ("file-invalid-root-ignored", [(b"[ignored]", b"bad", b"bad.bin"),
                                   (b"later", b"ok", b"later.bin")]),
    ("integer", [(b"a[0]", b"zero", None), (b"a[2]", b"two", None)]),
    ("negative-append", [(b"a[-2]", b"neg", None), (b"a[]", b"next", None)]),
    ("normalize", [(b"first.name", b"old", None), (b"first_name", b"new", None)]),
    ("replace", [(b"a[x]", b"old", None), (b"a", b"mid", None), (b"a[]", b"new", None)]),
    ("file-and-field", [(b"upload", b"DATA", b"sample.bin"), (b"message", b"yes", None)]),
    ("file-only", [(b"upload", b"DATA", b"sample.bin")]),
    ("file-binary-copy", [(b"upload", b"\x00\xffxyz", b"binary.bin")]),
    # Copy boundaries at 8192 bytes, including NUL and non-UTF-8 data.
    ("file-block-minus-one", [(b"upload", b"\x00\xff" + b"x" * (8192 - 3), b"before.bin")]),
    ("file-exact-block", [(b"upload", b"\x00\xff" + b"x" * (8192 - 2), b"exact.bin")]),
    ("file-block-plus-one", [(b"upload", b"\x00\xff" + b"x" * (8192 - 1), b"after.bin")]),
    ("file-exact-two-blocks", [(b"upload", b"\x00\xff" + b"x" * (16384 - 2), b"two.bin")]),
    ("file-multi-buffer-copy", [(b"upload", b"\x00\xff" + b"x" * (8192 + 17), b"large.bin")]),
    ("file-three-blocks-binary", [(b"upload", bytes(range(256)) * 96 + b"\xff", b"three.bin")]),
    # Short-write regression companion: PHP SAPI must preserve every binary byte.
    ("file-short-write-oracle", [(b"upload", bytes((i * 47 + 3) % 256 for i in range(8192 * 2 + 9)), b"short-write.bin")]),
    # Transactional copy-to-Vec companion: 8 KiB + 29 binary bytes must
    # match the PHP 8.4 HTTP SAPI upload exactly, including the last block.
    ("file-transactional-vec-oracle", [
        (b"upload", bytes((i * 43 + 251) % 256 for i in range(8192 + 29)), b"transaction.bin")
    ]),
    # Admitted transactional copy must preserve PHP's last-write-wins
    # $_FILES identity even when the surviving payload crosses a copy block.
    ("file-transactional-vec-overwrite-oracle", [
        (b"upload", b"replaced", b"transaction.bin"),
        (b"upload", bytes((i * 43 + 251) % 256 for i in range(8192 + 29)), b"transaction.bin"),
    ]),
    # PHP replaces both earlier same-name uploads. The surviving payload
    # exercises binary copy across the 8 KiB block boundary.
    ("file-transactional-vec-triple-overwrite-oracle", [
        (b"upload", b"first", b"transaction.bin"),
        (b"upload", b"second", b"transaction.bin"),
        (b"upload", bytes((i * 43 + 251) % 256 for i in range(8192 + 29)), b"transaction.bin"),
    ]),
    # Source-read EINTR regression companion: unchanged 8 KiB + 19 byte
    # uploads must remain byte-for-byte identical to PHP 8.4 HTTP SAPI.
    ("file-interrupted-source-read-oracle", [
        (b"upload", bytes((i * 37 + 23) % 256 for i in range(8192 + 19)), b"source-eintr.bin")
    ]),
    # A Rust writer may return EINTR after modifying its source descriptor.
    # The normal (non-injected) path must still match PHP's binary upload.
    ("file-interrupted-writer-oracle", [
        (b"upload", bytes((i * 97 + 11) % 256 for i in range(8192 + 13)), b"eintr.bin")
    ]),
    # Post-read/pre-write integrity-check companion: verify unchanged binary
    # uploads still match the PHP 8.4 HTTP SAPI across multiple read blocks.
    ("file-prewrite-snapshot-oracle", [
        (b"upload", bytes((i * 19 + 7) % 256 for i in range(8192 * 2 + 3)), b"snapshot.bin")
    ]),
    # Distinct data at every copy boundary: catch skipped/repeated source blocks.
    # Post-write readback companion: byte-varying data crosses three full
    # blocks and a short final block in both the Rust and PHP 8.4 paths.
    ("file-readback-boundary-oracle", [
        (b"upload", bytes((i * 61 + 5) % 256 for i in range(8192 * 3 + 5)), b"readback.bin")
    ]),
    # Readback EINTR fault-injection companion. The non-injected upload must
    # preserve every binary byte across two blocks and a short final block.
    ("file-readback-eintr-oracle", [
        (b"upload", bytes((i * 59 + 13) % 256 for i in range(8192 * 2 + 11)), b"readback-eintr.bin")
    ]),
    # Positional-read fault-injection companion: the non-faulted, 8 KiB + 7
    # binary upload must match PHP 8.4 FILES metadata and all payload bytes.
    ("file-overreported-read-guard-oracle", [
        (b"upload", bytes((i * 29 + 173) % 256 for i in range(8192 + 7)), b"overreport.bin")
    ]),
    ("file-offset-sensitive-binary", [(b"upload", bytes((i * 31 + 17) % 251 for i in range(8192 * 2 + 17)), b"offset.bin")]),
    # More than four copy blocks, exercising every post-write source check.
    ("file-four-blocks-plus-one", [(b"upload", bytes((i * 73 + 19) % 256 for i in range(8192 * 4 + 1)), b"four-blocks.bin")]),
    # The Rust copy path pins the original anonymous inode. Two equal-sized,
    # distinct binary uploads must retain their own PHP 8.4 FILES metadata and
    # payload bytes even when the staging descriptors have identical lengths.
    ("same-size-distinct-inodes", [
        (b"first", b"\x00\xff\x01", b"first.bin"),
        (b"second", b"\xff\x00\x02", b"second.bin"),
    ]),
    ("two-files", [(b"upload", b"first", b"one.bin"),
                   (b"attachment", b"second", b"two.bin")]),
    ("nested-files", [(b"f[x][]", b"first", b"one.bin"),
                      (b"f[x][]", b"second", b"two.bin")]),
    ("file-indices", [(b"f[00]", b"first", b"one.bin"),
                      (b"f[0]", b"second", b"two.bin")]),
    ("file-overwrite", [(b"f", b"first", b"one.bin"),
                        (b"f", b"second", b"two.bin")]),
    ("file-nested-then-flat", [(b"f[x]", b"first", b"one.bin"),
                               (b"f", b"second", b"two.bin")]),
    ("file-flat-then-nested", [(b"f", b"first", b"one.bin"),
                               (b"f[x]", b"second", b"two.bin")]),
    ("file-same-index", [(b"f[0]", b"first", b"one.bin"),
                          (b"f[0]", b"second", b"two.bin")]),
    ("file-normalized-root", [(b"a.b", b"first", b"one.bin"),
                              (b"a_b", b"second", b"two.bin")]),
    ("file-append-both", [(b"f[]", b"first", b"one.bin"),
                           (b"f[]", b"second", b"two.bin")]),
    ("nested-tmp-name-key", [(b"f[tmp_name]", b"data", b"one.bin")]),
    ("nested-error-key", [(b"f[error]", b"data", b"one.bin")]),
    ("nested-size-key", [(b"f[size]", b"data", b"one.bin")]),
    ("empty-file-data", [(b"f", b"", b"zero.bin")]),
    ("windows-filename", [(b"f", b"content", b"C:\\fakepath\\leaf.bin")]),
    ("empty-upload-name", [(b"f", b"", b"")]),
    ("path-filename", [(b"f", b"payload", b"folder/item.bin")]),
    ("max-before-exceeded", [(b"MAX_FILE_SIZE", b"2", None),
                            (b"f", b"abc", b"x.txt")]),
    ("max-equal", [(b"MAX_FILE_SIZE", b"3", None),
                   (b"f", b"abc", b"x.txt")]),
    ("max-after-ignored", [(b"f", b"abc", b"x.txt"),
                           (b"MAX_FILE_SIZE", b"2", None)]),
    ("max-zero-unlimited", [(b"MAX_FILE_SIZE", b"0", None),
                            (b"f", b"abc", b"x.txt")]),
    ("max-negative", [(b"MAX_FILE_SIZE", b"-1", None),
                      (b"f", b"abc", b"x.txt")]),
    ("max-case-insensitive", [(b"max_file_size", b"2", None),
                              (b"f", b"abc", b"x.txt")]),
    ("max-nested", [(b"MAX_FILE_SIZE", b"2", None),
                    (b"f[x][]", b"abc", b"x.txt")]),
    ("max-changed", [(b"MAX_FILE_SIZE", b"2", None),
                     (b"first", b"abc", b"x.txt"),
                     (b"MAX_FILE_SIZE", b"10", None),
                     (b"second", b"abc", b"y.txt")]),
    ("max-collision-recovery", [(b"MAX_FILE_SIZE", b"1", None),
                                (b"f", b"blocked", b"one.bin"),
                                (b"MAX_FILE_SIZE", b"100", None),
                                (b"f", b"\x00\xff", b"two.bin")]),
    ("file-zero-overwritten", [(b"f", b"", b"zero.bin"),
                               (b"f", b"\x00\xff", b"two.bin")]),
    ("file-append-zero-binary", [(b"f[]", b"", b"zero.bin"),
                                 (b"f[]", b"\x00\xff", b"two.bin")]),
    ("file-near-delimiter", [(b"f", b"start\r\n--rustpon-difx\r\nend", b"near.bin")]),
    # PHP registers only the bytes before the first Content-Type semicolon.
    ("file-type-charset", [(b"f", b"hi", b"x.txt", b"text/plain; charset=UTF-8")]),
    ("file-type-space", [(b"f", b"hi", b"x.txt", b'text/plain ; charset="utf-8"')]),
    ("file-type-opaque-param", [(b"f", b"hi", b"x.txt", b"text/plain; invalid")]),
    ("file-type-mixedcase", [(b"f", b"hi", b"x.txt", b"TEXT/PLAIN; charset=UTF-8")]),
    # The PHP 8.4 SAPI does not require valid MIME syntax for per-file
    # Content-Type metadata. Leading OWS is stripped, trailing OWS retained.
    ("file-type-empty", [(b"f", b"\x00\xffhi", b"x.txt", b"")]),
    ("file-type-blank", [(b"f", b"\x00\xffhi", b"x.txt", b"   ")]),
    ("file-type-no-slash", [(b"f", b"\x00\xffhi", b"x.txt", b"whatever")]),
    ("file-type-extra-slash", [(b"f", b"\x00\xffhi", b"x.txt", b"foo/bar/baz")]),
    ("file-type-comma", [(b"f", b"\x00\xffhi", b"x.txt", b"foo/bar, baz")]),
    ("file-type-trailing-space", [(b"f", b"\x00\xffhi", b"x.txt", b"text/plain ")]),
    ("file-type-tabs", [(b"f", b"\x00\xffhi", b"x.txt", b"\ttext/plain\t")]),

    # PHP 8.4 HTTP SAPI accepts duplicate disposition keys (last one wins)
    # and ignores syntactically valid extension parameters, including filename*.
    ("disposition-duplicate-name", [
        (b"new", b"renamed", None, b"application/octet-stream",
         b"form-data; name=old; name=new"),
    ]),
    ("disposition-duplicate-filename", [
        (b"up", b"\x00\xffN", b"new.bin", b"application/octet-stream",
         b"form-data; name=up; filename=old.bin; filename=new.bin"),
    ]),
    ("disposition-filename-star-field", [
        (b"plain", b"\x00\xffV", None, b"application/octet-stream",
         b"form-data; name=plain; filename*=UTF-8''unused.bin"),
    ]),
    ("disposition-filename-star-ignored", [
        (b"up", b"payload", b"plain.bin", b"application/octet-stream",
         b"form-data; name=up; filename=plain.bin; filename*=UTF-8''encoded.bin"),
    ]),
    ("disposition-unknown-quoted", [
        (b"up", b"payload", b"doc.bin", b"application/octet-stream",
         b'form-data; name=up; audit="a;b"; filename=doc.bin'),
    ]),
    ("disposition-empty-separators", [
        (b"up", b"payload", b"doc.bin", b"application/octet-stream",
         b"form-data; name=up;; filename=doc.bin;"),
    ]),
    # PHP 8.4 HTTP SAPI decodes only escaped quotes and backslashes inside
    # quoted Content-Disposition parameters. Other backslashes are literal.
    ("disposition-quoted-name-unknown-escape", [
        (b"a\\xb", b"field", None, b"application/octet-stream",
         b'form-data; name="a\\xb"'),
    ]),
    ("disposition-quoted-name-double-backslash", [
        (b"a\\b", b"field", None, b"application/octet-stream",
         b'form-data; name="a\\\\b"'),
    ]),
    ("disposition-quoted-name-escaped-quote", [
        (b'a"b', b"field", None, b"application/octet-stream",
         b'form-data; name="a\\"b"'),
    ]),
    ("disposition-quoted-filename-escaped-quote", [
        (b"upload", b"xyz", b'foo"bar.bin', b"application/octet-stream",
         b'form-data; name="upload"; filename="foo\\"bar.bin"'),
    ]),
    ("disposition-quoted-filename-double-backslash", [
        (b"upload", b"\x00\xff", b"folder\\leaf.bin", b"application/octet-stream",
         b'form-data; name="upload"; filename="folder\\\\leaf.bin"'),
    ]),
    ("disposition-quoted-filename-unknown-escape", [
        (b"upload", b"payload", b"folder\\q.bin", b"application/octet-stream",
         b'form-data; name="upload"; filename="folder\\q.bin"'),
    ]),
]

def body_for(parts):
    output = bytearray()
    for part in parts:
        name, data, filename = part[:3]
        content_type = part[3] if len(part) >= 4 else b"application/octet-stream"
        output.extend(b"--" + BOUNDARY + b"\r\n")
        if len(part) == 5:
            # Raw disposition fixtures exercise PHP SAPI parameter handling.
            # The first tuple fields still describe expected parsed metadata.
            output.extend(b"Content-Disposition: " + part[4])
        else:
            output.extend(b'Content-Disposition: form-data; name="' + name + b'"')
            if filename is not None:
                output.extend(b'; filename="' + filename + b'"')
        if filename is not None:
            output.extend(b"\r\nContent-Type: " + content_type)
        output.extend(b"\r\n\r\n")
        output.extend(data)
        output.extend(b"\r\n")
    output.extend(b"--" + BOUNDARY + b"--\r\n")
    return bytes(output)

def php_post(port, data):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        conn.request("POST", "/", body=data, headers={
            "Content-Type": "multipart/form-data; boundary=" + BOUNDARY.decode()})
        response = conn.getresponse()
        output = response.read()
        if response.status != 200:
            raise RuntimeError(f"PHP status {response.status}: {output[:200]!r}")
        return json.loads(output)
    finally:
        conn.close()

def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--rust-bin", type=Path, required=True)
    cli.add_argument("--php-bin", default="php")
    opt = cli.parse_args()
    router = Path(__file__).resolve().with_name("multipart_projection.php")
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    server = subprocess.Popen([opt.php_bin, "-d", "max_input_vars=10000",
                               "-S", f"127.0.0.1:{port}", str(router)],
                              stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                              stderr=subprocess.DEVNULL)
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
            raise RuntimeError("PHP SAPI server failed to start")
        rng = random.Random(0x4D554C5449504152)
        names = [b"alpha", b"beta", b"a[]", b"a[0]", b"a[x]", b"words.here", b"n[-2]"]
        values = [b"", b"hello", b"\x00\xff", b"equal=sign", "東京".encode()]
        cases = list(CASES)
        for i in range(40):
            fields = [(rng.choice(names), rng.choice(values), None)
                      for _ in range(rng.randrange(1, 6))]
            cases.append((f"generated-{i:03d}", fields))
        failures = 0
        with tempfile.TemporaryDirectory(prefix="rustpon-multipart-") as spool:
            for label, parts in cases:
                data = body_for(parts)
                php = php_post(port, data)
                # PHP 8.4 HTTP SAPI's anonymous upload staging is owner-only.
                # The Rust unit tests enforce the same private inode invariant.
                if any(mode != 0o600 for mode in php["upload_modes"]):
                    failures += 1
                    print(f"FAIL {label}: PHP temporary upload modes={php['upload_modes']!r}", file=sys.stderr)
                rust = subprocess.run([str(opt.rust_bin), "--post", BOUNDARY.decode(), spool],
                                      input=data, capture_output=True, timeout=15, check=False)
                if rust.returncode:
                    failures += 1
                    print(f"FAIL {label}: Rust {rust.returncode} {rust.stderr[:200]!r}", file=sys.stderr)
                    continue
                actual = json.loads(rust.stdout)["post_tree"]
                projected = subprocess.run(
                    [str(opt.rust_bin), "--files", BOUNDARY.decode(), spool],
                    input=data, capture_output=True, timeout=15, check=False)
                if projected.returncode:
                    failures += 1
                    print(f"FAIL {label}: Rust files projection {projected.stderr[:200]!r}", file=sys.stderr)
                    continue
                projected_data = json.loads(projected.stdout)
                actual_files_tree = projected_data["files_tree"]
                if projected_data["files_tree_typed"] != php["files_tree_typed"]:
                    failures += 1
                    print(f"FAIL {label}: PHP typed FILES={php['files_tree_typed']!r} Rust={projected_data['files_tree_typed']!r}", file=sys.stderr)
                admitted = subprocess.run(
                    [str(opt.rust_bin), "--admit", BOUNDARY.decode(), spool],
                    input=data, capture_output=True, timeout=15, check=False)
                if admitted.returncode:
                    failures += 1
                    print(f"FAIL {label}: admission {admitted.stderr[:200]!r}", file=sys.stderr)
                    continue
                admission_data = json.loads(admitted.stdout)
                if admission_data["files_tree_typed"] != php["files_tree_typed"]:
                    failures += 1
                    print(f"FAIL {label}: admitted typed FILES tree differs from PHP", file=sys.stderr)
                if admission_data["files_tree"] != php["files_tree"]:
                    failures += 1
                    print(f"FAIL {label}: admitted FILES tree disagrees with PHP", file=sys.stderr)
                expected_accepted = {
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
                    "file-append-zero-binary": [0, 1],
                    "file-overwrite": [1],
                    "file-transactional-vec-overwrite-oracle": [1],
                    "file-transactional-vec-triple-overwrite-oracle": [2],
                    "file-nested-then-flat": [1],
                    "file-flat-then-nested": [1],
                    "file-same-index": [1],
                    "file-normalized-root": [1],
                    "file-append-both": [0, 1],
                    "empty-upload-name": [],
                    "file-invalid-first": [],
                    "file-invalid-after-good": [0],
                    "file-invalid-trailing-bracket": [],
                    "file-invalid-nested-tail": [],
                    "file-invalid-root-ignored": [1],
                }
                if label in expected_accepted and admission_data["accepted_indices"] != expected_accepted[label]:
                    failures += 1
                    print(f"FAIL {label}: admission indices {admission_data['accepted_indices']!r}", file=sys.stderr)

                if php["files_tree"] != actual_files_tree:
                    failures += 1
                    print(f"FAIL {label}: PHP FILES tree={php['files_tree']!r} Rust={actual_files_tree!r}", file=sys.stderr)
                if php["post_tree"] != actual:
                    failures += 1
                    print(f"FAIL {label}: PHP={php['post_tree']!r} Rust={actual!r}", file=sys.stderr)
                # The raw wire IR retains overwritten uploads; PHP FILES does not.
                # PHP FILES tree equality above covers duplicate-name semantics.
                # Wire IR also retains uploads rejected by MAX_FILE_SIZE.
                # The wire IR retains overwritten uploads, whereas PHP
                # exposes only the last descriptor of a repeated scalar field.
                # Validate the winning bytes instead of counting both files.
                transactional_overwrites = (
                    "file-transactional-vec-overwrite-oracle",
                    "file-transactional-vec-triple-overwrite-oracle",
                )
                raw_indices = None
                if php["files"]:
                    if label in transactional_overwrites:
                        raw_indices = set(admission_data["accepted_indices"])
                    elif label not in ("file-overwrite", "max-changed",
                            "max-collision-recovery", "file-zero-overwritten",
                            "file-nested-then-flat", "file-flat-then-nested",
                            "file-same-index", "file-normalized-root",
                            "file-invalid-after-good", "file-invalid-root-ignored"):
                        raw_indices = set(range(len(parts)))
                if raw_indices is not None:
                    raw = subprocess.run([str(opt.rust_bin), BOUNDARY.decode(), spool],
                                          input=data, capture_output=True, timeout=15, check=False)
                    if raw.returncode:
                        failures += 1
                        print(f"FAIL {label}: raw multipart error {raw.stderr[:200]!r}", file=sys.stderr)
                        continue
                    wire = json.loads(raw.stdout)["parts"]
                    expected_files = [
                        [part["name_hex"],
                         bytes.fromhex(part["filename_hex"])
                               .replace(b"\\", b"/").split(b"/")[-1].hex(),
                         part["content_type_hex"] or "",
                         len(bytes.fromhex(part["data_hex"])), part["data_hex"]]
                        for index, part in enumerate(wire)
                        if index in raw_indices and part["filename_hex"] is not None
                    ]
                    if php["files"] != expected_files:
                        failures += 1
                        # Avoid printing megabytes of binary hex in CI logs.
                        print(f"FAIL {label}: PHP file lengths="
                              f"{[record[3] for record in php['files']]!r}, "
                              f"Rust lengths={[record[3] for record in expected_files]!r}, "
                              f"admitted={admission_data['accepted_indices']!r}",
                              file=sys.stderr)
        print(f"PHP SAPI multipart: {len(cases)} cases, {failures} failures")
        return int(failures != 0)
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()

if __name__ == "__main__":
    sys.exit(main())
