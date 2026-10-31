#!/usr/bin/env python3
"""Compare the real C ABI (ctypes) with PHP 8.4 parse_str(), using binary data."""
import argparse
import ctypes
import json
import random
import subprocess
import sys
from pathlib import Path

OK = 0
INVALID_ARGUMENT = 1
PARSE_REJECTED = 2
BUFFER_TOO_SMALL = 4
MAX_INPUT = 256 * 1024


def call(parse, value):
    data = ctypes.create_string_buffer(value, len(value) + 1)
    wanted = ctypes.c_size_t(999)
    status = parse(data, len(value), None, 0, ctypes.byref(wanted))
    if status != BUFFER_TOO_SMALL:
        return status, b""
    assert 0 < wanted.value <= 1024 * 1024
    capacity = wanted.value
    output = ctypes.create_string_buffer(capacity + 1)
    output.raw = b"\xa5" * (capacity + 1)
    actual = ctypes.c_size_t(0)
    assert parse(data, len(value), output, capacity - 1, ctypes.byref(actual)) == BUFFER_TOO_SMALL
    assert actual.value == capacity
    assert output.raw == b"\xa5" * (capacity + 1), "partial output exposed"
    actual.value = 0
    status = parse(data, len(value), output, capacity, ctypes.byref(actual))
    assert actual.value == capacity
    assert output.raw[capacity] == 0xa5, "wrote past buffer"
    return status, output.raw[:capacity]


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--library", type=Path, required=True)
    cli.add_argument("--php-bin", default="php")
    args = cli.parse_args()

    dll = ctypes.CDLL(str(args.library.resolve()))
    abi = dll.rustpon_ffi_abi_version
    abi.argtypes = []
    abi.restype = ctypes.c_uint32
    assert abi() == 0x00010000
    parse = dll.rustpon_v1_form_post_json
    parse.argtypes = [
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.c_void_p, ctypes.c_size_t,
        ctypes.POINTER(ctypes.c_size_t),
    ]
    parse.restype = ctypes.c_int32

    examples = [
        b"", b"a=x", b"a=one&a=two", b"a[]=x&a[]=y",
        b"x%5By%5D=z&n%5B0%5D=one", b"nul=hi%00%ff",
        b"k=percent%2+plus+space&msg=%E6%97%A5%E6%9C%AC",
        b"a%00b=bad", b"a=1&=ignored&&blank", b"x[-1]=a&x[]=b",
    ]
    rng = random.Random(0xF11AB1)
    names = [b"a", b"b", b"c[]", b"arr[x]", b"arr[0]", b"hello.world"]
    values = [b"", b"first", b"two", b"%00%ff", b"plus+space", b"=%2g", b"%E6%9D%B1"]
    for _ in range(64):
        pairs = [rng.choice(names) + b"=" + rng.choice(values)
                 for _ in range(rng.randint(1, 6))]
        examples.append(b"&".join(pairs))

    oracle = Path(__file__).resolve().with_name("php_projection.php")
    successes = 0
    rejected = 0
    for i, body in enumerate(examples):
        status, result = call(parse, body)
        if b"%00" in body.split(b"=", 1)[0]:
            assert status == PARSE_REJECTED, (i, status, body)
            rejected += 1
            continue
        expected = subprocess.run([args.php_bin, str(oracle)], input=body,
                                  capture_output=True, timeout=15, check=True)
        assert status == OK, (i, status, body)
        assert json.loads(result) == json.loads(expected.stdout), (i, body, result, expected.stdout)
        successes += 1

    required = ctypes.c_size_t(999)
    assert parse(None, 1, None, 0, ctypes.byref(required)) == INVALID_ARGUMENT
    assert required.value == 0
    valid_over_limit = ctypes.create_string_buffer(MAX_INPUT + 1)
    required.value = 999
    assert parse(valid_over_limit, MAX_INPUT + 1, None, 0,
                 ctypes.byref(required)) == INVALID_ARGUMENT
    assert required.value == 0
    for _ in range(128):
        status, _ = call(parse, b"a=x&nested[]=ok")
        assert status == OK
    print(f"FFI/PHP parse_str: {successes} compatible, {rejected} strict rejection, "
          "128 repeated calls and ABI failure-path checks")


if __name__ == "__main__":
    sys.exit(main())
