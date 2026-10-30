#!/usr/bin/env python3
"""Compare bounded scalar Cookie-header projection with PHP 8.4 HTTP SAPI."""
import argparse
import http.client
import json
import subprocess
import sys
from pathlib import Path

from differential_upload_size import start_server

CASES = [
    ("basic", b"a=1; b=2"),
    ("first-wins", b"a=1; a=2; b=3"),
    ("normalized-collision", b"a.b=1; a_b=2"),
    ("reverse-collision", b"a_b=1; a.b=2"),
    ("leading-and-trailing-whitespace", b" a = 1 ;  c =d "),
    ("value-percent-and-plus", b"a=hello+world; p=%41%2B%00"),
    ("quoted-value", b'x="quoted value"; y=ok'),
    ("empty-components", b";foo=1;;bar=2;"),
    ("literal-percent-name", b"a%2Eb=foo; a.b=bar"),
    ("binary-escape", b"a=%FF%00; b=tail"),
    ("equals-in-value", b"a=b=c; c=3"),
    ("no-equals", b"foo; bar=2"),
    ("empty-name", b"=ignored; foo=ok"),
    ("tab-after-semicolon", b"a=1;\tb=2"),
    ("case-sensitive", b"a=1; A=2"),
    ("multi-block", b"x=" + b"Z" * 4097 + b"; y=tail"),
    ("unclosed-bracket", b"foo[bar=one; foo_bar=two"),
    ("unclosed-bracket-reverse-collision", b"foo_bar=first; foo[bar=second"),
    ("nested-unclosed-brackets", b"foo[[bar=one; x[ y.z=two"),
    ("unclosed-bracket-no-equals", b"lonely["),
    ("unclosed-bracket-empty-value", b"empty[="),
    ("unclosed-bracket-percent-value", b"foo[bar=%00%FF+"),
    ("unclosed-bracket-normalized-collision", b"a[b.c=one; a_b_c=two"),
    ("bracket-root-ignored", b"[bad=ignored; [okay]=ignored; ok=yes"),
    ("unmatched-close-literal", b"foo]bar=value"),
]


ARRAY_CASES = [
    ("two-children", b"a[x]=1; a[y]=2"),
    ("duplicate-child-last-wins", b"a[x]=1; a[x]=2; a[y]=3"),
    ("scalar-promoted-to-array", b"a=first; a=second; a[x]=3"),
    ("array-ignores-later-scalar", b"a[x]=1; a=ignored; a[y]=2"),
    ("append-twice", b"a[]=x; a[]=y"),
    ("explicit-zero-then-append", b"a[0]=x; a[]=y"),
    ("append-then-explicit-zero", b"a[]=x; a[0]=y; a[]=z"),
    ("numeric-gap-append", b"a[2]=x; a[]=y"),
    ("negative-index-append", b"a[-2]=x; a[]=y"),
    ("numeric-leading-zero", b"a[01]=x; a[]=y"),
    ("single-space-append", b"a[ ]=x; a[]=y"),
    ("two-space-key", b"a[  ]=x; a[]=y"),
    ("normalized-root-collision", b"a.b[x]=1; a_b[y]=2"),
    ("numeric-root-and-string-root", b"0[x]=1; 01[y]=2"),
    ("binary-percent-value", b"a[x]=%00%FF+; b[y]=ok"),
    ("unterminated-scalar-bracket", b"a[foo=one; a_foo=two; b[x]=three"),
    ("trailing-after-bracket", b"a[foo]tail=1; a[bar]=2"),
    ("plus-sign-index", b"a[+1]=x; a[]=y"),
    ("negative-zero-key", b"a[-0]=x; a[]=y"),
    ("multiple-root-arrays", b"a[x]=1; b[2]=2; c=3; b[]=4"),
]

LIMIT_CASES = [
    ("ordinary", b"a=1; b=2; c=3; d=4"),
    ("duplicate-consumes-slot", b"a=1; a=2; b=3"),
    ("ignored-empty-name", b"=ignored; a=1; b=2; c=3"),
    ("blank-separators", b";; a=1;;b=2; c=3"),
    ("ignored-bracket-root", b"[bad]=ignored; a=1; b=2"),
    ("array-append", b"a[]=first; a[]=second; a[]=third"),
    ("array-promotion", b"a=one; a[x]=two; b=three"),
    ("array-duplicate-index", b"a[x]=one; a[x]=two; b=three"),
]


def php_cookie(port, header, key="cookie_pairs"):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=10)
    try:
        connection.request(
            "GET", "/", headers={"Cookie": header.decode("latin1")}
        )
        response = connection.getresponse()
        data = response.read()
        if response.status != 200:
            raise RuntimeError(f"PHP HTTP {response.status}: {data[:200]!r}")
        return json.loads(data)[key]
    finally:
        connection.close()


def rust_cookie(binary, header, arrays=False, max_input_vars=None):
    command = [str(binary)] + (["--php-arrays"] if arrays else [])
    if max_input_vars is not None:
        command.extend(["--php-max-input-vars", str(max_input_vars)])
    result = subprocess.run(
        command,
        input=header, capture_output=True, timeout=15, check=False,
    )
    if result.returncode:
        raise RuntimeError(f"Rust exit {result.returncode}: {result.stderr[:300]!r}")
    return json.loads(result.stdout)["cookie_tree" if arrays else "cookie_pairs"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rust-bin", type=Path, required=True)
    parser.add_argument("--php-bin", default="php")
    args = parser.parse_args()
    version = subprocess.check_output(
        [args.php_bin, "-r", "echo PHP_VERSION;"], text=True,
    ).strip()
    if not version.startswith("8.4."):
        raise RuntimeError(f"expected PHP 8.4 oracle, got {version}")

    router = Path(__file__).resolve().with_name("cookie_projection.php")
    server, port = start_server(args.php_bin, 1, router)
    passed = 0
    failures = 0
    try:
        for label, header in CASES:
            try:
                expected = php_cookie(port, header)
                actual = rust_cookie(args.rust_bin, header)
                if expected != actual:
                    raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
                passed += 1
            except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                failures += 1
                print(f"FAIL Cookie {label}: {error}", file=sys.stderr)
    finally:
        server.terminate()
        server.wait(timeout=10)
    array_router = Path(__file__).resolve().with_name("cookie_array_projection.php")
    server, port = start_server(args.php_bin, 1, array_router)
    try:
        for label, header in ARRAY_CASES:
            try:
                expected = php_cookie(port, header, key="cookie_tree")
                actual = rust_cookie(args.rust_bin, header, arrays=True)
                if expected != actual:
                    raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
                passed += 1
            except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                failures += 1
                print(f"FAIL Cookie array {label}: {error}", file=sys.stderr)
    finally:
        server.terminate()
        server.wait(timeout=10)
    # With HTTP Cookie headers PHP 8.4 counts nonempty wire names, including
    # duplicates and skipped bracket-root names; an ini limit of zero skips all.
    scalar_router = Path(__file__).resolve().with_name("cookie_projection.php")
    for limit in (0, 1, 2, 3):
        for arrays, router, expected_key, cases in (
            (False, scalar_router, "cookie_pairs", LIMIT_CASES[:5]),
            (True, array_router, "cookie_tree", LIMIT_CASES),
        ):
            server, port = start_server(
                args.php_bin, 1, router, max_input_vars=limit,
            )
            try:
                for label, header in cases:
                    try:
                        expected = php_cookie(port, header, key=expected_key)
                        actual = rust_cookie(
                            args.rust_bin, header, arrays=arrays,
                            max_input_vars=limit,
                        )
                        if expected != actual:
                            raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
                        passed += 1
                    except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                        failures += 1
                        print(
                            f"FAIL Cookie max_input_vars={limit} "
                            f"{'array' if arrays else 'scalar'} {label}: {error}",
                            file=sys.stderr,
                        )
            finally:
                server.terminate()
                server.wait(timeout=10)
    if failures:
        raise SystemExit(f"{failures} PHP 8.4 HTTP Cookie cases failed")
    print(
        f"PHP {version} Cookie HTTP SAPI: {passed} cases matched "
        f"({len(CASES)} scalar, {len(ARRAY_CASES)} one-level array, "
        f"{4 * (5 + len(LIMIT_CASES))} quota cases)"
    )


if __name__ == "__main__":
    main()
