#!/usr/bin/env python3
"""Binary-safe, deterministic PHP 8.4 parse_str vs rustpon URL-encoded differential."""
import argparse
import json
import random
import subprocess
import sys
from pathlib import Path

CASES = [
    ("empty", b""), ("separators", b"&&&"), ("single", b"a=one"),
    ("missing-value", b"a"), ("empty-value", b"a="),
    ("blank-name", b"=ignored&a=kept"), ("extra-equals", b"a=x=y=z"),
    ("blank-pairs", b"&a=1&&b=2&"), ("duplicate", b"a=1&a=2"),
    ("plus", b"hello+world=hi+there"), ("percent-value", b"data=%00%FF%2B%25%41"),
    ("percent-key", b"x%5By%5D=1"), ("bad-escape", b"a=%2+%&b=%GG"),
    ("binary-value", b"x=a%00b%FFz"), ("raw-binary", b"x=\x80\xff"),
    ("dots", b"a.b=x&a_b=y"), ("spaces", b"first+last=x&first_last=y"),
    ("leading-space", b"+alpha=v"), ("integer-index", b"a%5B0%5D=x&a%5B1%5D=y"),
    ("append", b"a%5B%5D=x&a%5B%5D=y"),
    ("nested", b"a%5Bx%5D%5By%5D=z"),
    ("array-to-scalar-to-array", b"a%5Bx%5D=old&a=new&a%5B%5D=last"),
    ("scalar-to-array", b"a=old&a%5Bx%5D=new"),
    ("negative-append", b"a%5B-5%5D=x&a%5B%5D=y"),
    ("numeric-string", b"a%5B01%5D=x&a%5B1%5D=y"),
    ("root-numeric", b"0=x&1=y"), ("utf8", "word=日本語".encode()),
    ("mixed-case-hex", b"a%5bx%5d=one&b=%2f%2F"),
    ("escaped-ampersand", b"a%26b=x%26y&z=1"),
    ("escaped-equals", b"a%3Db=1%3D2"),
    ("trailing-percent", b"percent=%&half=%2&bad=%GG"),
    ("double-encoded", b"a%255Bb%255D=x&percent=%2525"),
    ("percent-spaces", b"first%20last=x&first+last=y"),
    ("utf8-encoded", b"key=%E6%9D%B1%E4%BA%AC"),
    ("mixed-hex", b"n%5b0%5d=%ff&n%5B1%5D=%FF"),
    ("empty-value-no-equal", b"bare&bare=&x=1"),
    ("bracket-leading-space", b"a%5B+x%5D=1"),
    ("bracket-leading-tab", b"a%5B%09x%5D=1"),
    ("bracket-leading-newline", b"a%5B%0Ax%5D=1"),
    ("bracket-two-spaces", b"a%5B++%5D=1"),
    ("bracket-space-tab", b"a%5B+%09%5D=1"),
    ("bracket-single-space-append", b"a%5B+%5D=1"),
    ("bracket-single-tab-append", b"a%5B%09%5D=1"),
    ("bracket-single-vtab-append", b"a%5B%0B%5D=1"),
    ("bracket-single-formfeed-append", b"a%5B%0C%5D=1"),
]

def run(command, body):
    return subprocess.run(command, input=body, capture_output=True, timeout=15, check=False)

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    oracle = str(Path(__file__).resolve().with_name("php_projection.php"))
    php = [args.php_bin, "-d", "max_input_vars=10000", oracle]
    rust = [str(args.rust_bin)]
    rng = random.Random(0x52555354504F4E)
    names = [b"alpha", b"beta", b"arr%5B%5D", b"arr%5B0%5D",
             b"arr%5Bx%5D", b"item.name", b"two+words", b"n%5B-2%5D"]
    values = [b"", b"0", b"hello", b"%00", b"a%2Bb", b"%FF", b"x%3Dy", b"+a+"]
    cases = list(CASES)
    for i in range(64):
        pairs = [rng.choice(names) + b"=" + rng.choice(values)
                 for _ in range(rng.randrange(1, 7))]
        cases.append((f"generated-{i:03d}", b"&".join(pairs)))
    failures = 0
    for label, data in cases:
        expected = run(php, data)
        actual = run(rust, data)
        if expected.returncode or actual.returncode:
            failures += 1
            print(f"FAIL {label}: PHP={expected.returncode} Rust={actual.returncode} body={data!r}; stderr={actual.stderr[:200]!r}", file=sys.stderr)
            continue
        try:
            lhs = json.loads(expected.stdout)
            rhs = json.loads(actual.stdout)["post_tree"]
        except (KeyError, ValueError) as exc:
            failures += 1
            print(f"FAIL {label}: JSON {exc}", file=sys.stderr)
            continue
        if lhs != rhs:
            failures += 1
            print(f"FAIL {label}: body={data!r}\nPHP={lhs!r}\nRust={rhs!r}", file=sys.stderr)
    # Explicit strict deviations: no partial JSON publication.
    for label, data in [
        ("NUL-in-name", b"a%00b=1"),
        ("name-over-quota", b"a" * 4097 + b"=x"),
        ("value-over-quota", b"a=" + b"x" * (64 * 1024 + 1)),
    ]:
        result = run(rust, data)
        if result.returncode == 0 or result.stdout:
            failures += 1
            print(f"FAIL {label}: expected nonzero exit without stdout", file=sys.stderr)
    print(f"PHP differential: {len(cases)} compatibility, 3 strict, {failures} failures")
    return int(failures != 0)
if __name__ == "__main__":
    sys.exit(main())
