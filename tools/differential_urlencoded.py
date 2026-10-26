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
    ("unterminated-first-bracket", b"a%5Bx=1"),
    ("unterminated-empty-bracket", b"a%5B=1"),
    ("unterminated-first-space", b"a%5B+x=1"),
    ("unterminated-first-dots", b"a%5Bfoo.bar%5B=1"),
    ("unterminated-nested-bracket", b"a%5Bx%5D%5B=1"),
    ("unterminated-nested-key", b"a%5Bx%5D%5By=1"),
    ("unterminated-third-bracket", b"a%5Bx%5D%5By%5D%5B=1"),
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
    # The opt-in field-count policy mirrors parse_str(max_input_vars=N).
    # Keep this separate from the strict/default comparison above.
    limited = [
        (0, b"a=1&b=2&c=3"),
        (1, b"a=1&b=2&c=3"),
        (2, b"a=1&b=2&c=3"),
        (3, b"a=1&b=2&c=3"),
        (2, b"a=1&a=2&b=3"),
        (2, b"a%5Bx%5D=1&a%5By%5D=2&b=3"),
    ]
    for limit, data in limited:
        expected = run([args.php_bin, "-d", f"max_input_vars={limit}", oracle], data)
        actual = run([str(args.rust_bin), "--php-max-input-vars", str(limit)], data)
        try:
            if expected.returncode or actual.returncode:
                raise ValueError(f"exit PHP={expected.returncode} Rust={actual.returncode}")
            lhs = json.loads(expected.stdout)
            rhs = json.loads(actual.stdout)["post_tree"]
            if lhs != rhs:
                raise ValueError(f"PHP={lhs!r} Rust={rhs!r}")
        except (KeyError, ValueError) as exc:
            failures += 1
            print(f"FAIL max_input_vars={limit} body={data!r}: {exc}", file=sys.stderr)
    # PHP counts an over-depth field against max_input_vars even when the
    # variable is dropped. Compare the full tagged tree, not only exit codes.
    nested = [
        (0, b"a%5Bx%5D=1&ok=2"),
        (1, b"a%5Bx%5D=1&ok=2"),
        (1, b"a%5Bx%5D%5By%5D=1&ok=2"),
        (2, b"a%5Bx%5D%5By%5D=1&ok=2"),
        (1, b"a%5Bx%5D%5By=1&ok=2"),
        (2, b"a%5Bx%5D%5By=1&ok=2"),
        (64, b"a" + b"%5Bx%5D" * 65 + b"=deep&ok=2"),
    ]
    combined = [
        (1, 1, b"a%5Bx%5D%5By%5D=1&ok=2"),
        (2, 1, b"a%5Bx%5D%5By%5D=1&ok=2&tail=3"),
        (3, 2, b"a%5Bx%5D%5By%5D%5Bz%5D=1&a%5Bx%5D%5By%5D=2&ok=3"),
        (0, 2, b"a%5Bx%5D=1&ok=2"),
    ]
    for depth, data in nested:
        expected = run([args.php_bin, "-d", "max_input_vars=10000",
                        "-d", f"max_input_nesting_level={depth}", oracle], data)
        actual = run([str(args.rust_bin), "--php-max-input-nesting-level", str(depth)], data)
        try:
            if expected.returncode or actual.returncode:
                raise ValueError(f"exit PHP={expected.returncode} Rust={actual.returncode}")
            lhs = json.loads(expected.stdout)
            rhs = json.loads(actual.stdout)["post_tree"]
            if lhs != rhs:
                raise ValueError(f"PHP={lhs!r} Rust={rhs!r}")
        except (KeyError, ValueError) as exc:
            failures += 1
            print(f"FAIL nesting={depth} body={data!r}: {exc}", file=sys.stderr)
    for count, depth, data in combined:
        expected = run([args.php_bin, "-d", f"max_input_vars={count}",
                        "-d", f"max_input_nesting_level={depth}", oracle], data)
        actual = run([str(args.rust_bin), "--php-max-input-vars", str(count),
                      "--php-max-input-nesting-level", str(depth)], data)
        try:
            if expected.returncode or actual.returncode:
                raise ValueError(f"exit PHP={expected.returncode} Rust={actual.returncode}")
            lhs = json.loads(expected.stdout)
            rhs = json.loads(actual.stdout)["post_tree"]
            if lhs != rhs:
                raise ValueError(f"PHP={lhs!r} Rust={rhs!r}")
        except (KeyError, ValueError) as exc:
            failures += 1
            print(f"FAIL vars={count} nesting={depth} body={data!r}: {exc}", file=sys.stderr)
    # PHP's arg_separator.input is a set of raw separator bytes, not a
    # percent-decoded delimiter. Default rustpon remains '&'-only.
    semicolon_cases = [
        ("semicolon-basic", b"a=1;b=2&c=3"),
        ("semicolon-leading-trailing", b";;a=1;;;b=2;"),
        ("semicolon-percent-value", b"a=1%3B2;b=3"),
        ("semicolon-percent-key", b"a%3Bb=1;c=2"),
        ("semicolon-duplicates", b"a=1;a=2&b=3"),
        ("semicolon-arrays", b"arr%5B%5D=x;arr%5B%5D=y"),
        ("semicolon-empty-value", b"empty=;x=1"),
        ("semicolon-invalid-escape", b"z=1;bad=%GG"),
    ]
    for label, data in semicolon_cases:
        expected = run([args.php_bin, "-d", "arg_separator.input=&;", oracle], data)
        actual = run([str(args.rust_bin), "--php-semicolon-separator"], data)
        try:
            if expected.returncode or actual.returncode:
                raise ValueError(f"exit PHP={expected.returncode} Rust={actual.returncode}")
            lhs = json.loads(expected.stdout)
            rhs = json.loads(actual.stdout)["post_tree"]
            if lhs != rhs:
                raise ValueError(f"PHP={lhs!r} Rust={rhs!r}")
        except (KeyError, ValueError) as exc:
            failures += 1
            print(f"FAIL {label} body={data!r}: {exc}", file=sys.stderr)
    # Check the configured separator alongside PHP max_input_vars.
    combined_sep = b"a=1;b=2;c=3"
    expected = run([args.php_bin, "-d", "arg_separator.input=&;",
                    "-d", "max_input_vars=2", oracle], combined_sep)
    actual = run([str(args.rust_bin), "--php-semicolon-separator",
                  "--php-max-input-vars", "2"], combined_sep)
    try:
        if expected.returncode or actual.returncode:
            raise ValueError(f"exit PHP={expected.returncode} Rust={actual.returncode}")
        if json.loads(expected.stdout) != json.loads(actual.stdout)["post_tree"]:
            raise ValueError("PHP/rustpon projection mismatch")
    except (KeyError, ValueError) as exc:
        failures += 1
        print(f"FAIL semicolon/max_input_vars: {exc}", file=sys.stderr)
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
    print(f"PHP differential: {len(cases)} compatibility, {len(limited)} max_input_vars, {len(nested)} nesting, {len(combined)} combined, {len(semicolon_cases)} semicolon, 1 semicolon/vars, 3 strict, {failures} failures")
    return int(failures != 0)
if __name__ == "__main__":
    sys.exit(main())
