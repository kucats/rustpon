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
    ("i64-max-append-leaf-skipped", b"a[9223372036854775807]=max; a[]=ignored; b=ok"),
    ("i64-max-append-intermediate-skipped", b"a[9223372036854775807]=max; a[][x]=ignored; b=ok"),
    ("i64-max-append-nested-leaf-skipped", b"a[x][9223372036854775807]=max; a[x][]=ignored; b=ok"),
    ("i64-max-append-nested-intermediate-skipped", b"a[x][9223372036854775807]=max; a[x][][y]=ignored; b=ok"),
    ("i64-max-append-after-explicit-index", b"a[9223372036854775807]=max; a[]=ignored; a[0]=zero; a[]=ignored; b=ok"),
    ("numeric-leading-zero", b"a[01]=x; a[]=y"),
    ("single-space-append", b"a[ ]=x; a[]=y"),
    ("two-space-key", b"a[  ]=x; a[]=y"),
    ("normalized-root-collision", b"a.b[x]=1; a_b[y]=2"),
    ("numeric-root-and-string-root", b"0[x]=1; 01[y]=2"),
    ("binary-percent-value", b"a[x]=%00%FF+; b[y]=ok"),
    ("unterminated-scalar-bracket", b"a[foo=one; a_foo=two; b[x]=three"),
    ("trailing-after-bracket", b"a[foo]tail=1; a[bar]=2"),
    # PHP discards arbitrary suffixes after a complete array bracket path.
    # A later '[' without a matching ']' also terminates registration at
    # the last complete bracket, rather than rejecting the entire request.
    ("unfinished-second-bracket", b"a[x][=one"),
    ("unfinished-third-bracket", b"a[x][y][=two"),
    ("unfinished-fifth-bracket", b"a[x][y][z][w][v=four"),
    ("junk-then-bracket", b"a[x]noise[y]=one"),
    ("junk-after-second-level", b"a[x][y]noise[z]=two"),
    ("junk-after-third-level", b"a[x][y][z]noise[w]=three"),
    ("suffix-replaced-by-normal", b"a[x]tail[y]=first; a[x]=second; b=ok"),
    ("unfinished-nested-overwrites", b"a[x][y]=old; a[x][=new"),
    ("opening-bracket-in-key", b"a[x][y[z]=bracket"),
    ("plus-sign-index", b"a[+1]=x; a[]=y"),
    ("negative-zero-key", b"a[-0]=x; a[]=y"),
    ("multiple-root-arrays", b"a[x]=1; b[2]=2; c=3; b[]=4"),
    ("two-level-siblings", b"a[x][y]=one; a[x][z]=two"),
    ("two-level-overwrite", b"a[x][y]=one; a[x][y]=two"),
    ("two-level-scalar-promotion", b"a[x]=old; a[x][y]=new"),
    ("two-level-array-demotion", b"a[x][y]=old; a[x]=new"),
    ("two-level-append", b"a[][x]=one; a[][x]=two"),
    ("two-level-shared-index", b"a[0][x]=one; a[0][y]=two"),
    ("two-level-append-child", b"a[x][]=one; a[x][]=two"),
    ("two-level-nested-append", b"a[][]=one; a[][]=two"),
    ("two-level-mixed", b"a[0][x]=one; a[0]=two; a[0][y]=three"),
    ("two-level-trailing-text", b"a[x][y]tail=one"),
    ("two-level-numeric", b"a[1][2]=x; a[1][]=y"),
    ("two-level-binary", b"a[x][y]=%00%FF+"),
    ("one-level-vertical-tab-append", b"a[\x0b]=first; a[]=second"),
    ("three-level-siblings", b"a[x][y][z]=one; a[x][y][w]=two"),
    ("three-level-overwrite", b"a[x][y][z]=old; a[x][y][z]=new"),
    ("three-level-promotion", b"a[x][y]=old; a[x][y][z]=new"),
    ("three-level-demotion", b"a[x][y][z]=old; a[x][y]=new"),
    ("three-level-append-root", b"a[][x][y]=one; a[][x][y]=two"),
    ("three-level-append-middle", b"a[x][][z]=one; a[x][][z]=two"),
    ("three-level-append-leaf", b"a[x][y][]=one; a[x][y][]=two"),
    ("three-level-nested-append", b"a[][][]=one; a[][][]=two"),
    ("three-level-numeric", b"a[1][2][3]=x; a[1][2][]=y"),
    ("three-level-trailing-text", b"a[x][y][z]tail=one"),
    ("three-level-binary", b"a[x][y][z]=%00%FF+"),
    ("three-level-scalar-root", b"a=one; a[x][y][z]=two; a=three"),
    ("three-level-middle-scalar", b"a[x][y]=old; a[x][y][z]=new"),
    ("four-level-siblings", b"a[x][y][z][w]=one; a[x][y][z][v]=two"),
    ("four-level-overwrite", b"a[x][y][z][w]=old; a[x][y][z][w]=new"),
    ("four-level-promotion", b"a[x][y][z]=old; a[x][y][z][w]=new"),
    ("four-level-demotion", b"a[x][y][z][w]=old; a[x][y][z]=new"),
    ("four-level-append-root", b"a[][x][y][z]=one; a[][x][y][z]=two"),
    ("four-level-append-second", b"a[x][][z][w]=one; a[x][][z][w]=two"),
    ("four-level-append-third", b"a[x][y][][w]=one; a[x][y][][w]=two"),
    ("four-level-append-leaf", b"a[x][y][z][]=one; a[x][y][z][]=two"),
    ("four-level-nested-append", b"a[][][][]=one; a[][][][]=two"),
    ("four-level-numeric", b"a[1][2][3][4]=x; a[1][2][3][]=y"),
    ("four-level-trailing-text", b"a[x][y][z][w]tail=one"),
    ("four-level-binary", b"a[0][0][0][0]=%00%FF+"),
    ("five-level-sibling-and-overwrite", b"a[x][y][z][w][v]=old; a[x][y][z][w][u]=new; a[x][y][z][w][v]=last"),
    ("five-level-append", b"a[][][][][]=one; a[][][][][]=two"),
    ("six-level-numeric", b"a[0][1][2][3][4][5]=six"),
    ("16-level-scalar-replacement", b"a=old; a" + b"[x]" * 16 + b"=new"),
    ("32-level-nested", b"a" + b"[x]" * 32 + b"=middle"),
    ("64-level-boundary", b"a" + b"[x]" * 64 + b"=limit; tail=ok"),
    ("65-level-drops-existing-scalar", b"a=old; b=keep; a" + b"[x]" * 65 + b"=ignored"),
    ("65-level-drops-existing-array", b"a[x]=old; b=keep; a" + b"[x]" * 65 + b"=ignored"),
    ("65-level-recreates-root-last", b"a=old; b=keep; a" + b"[x]" * 65 + b"=ignored; a[y]=reborn"),
]

# The INI depth setting is independent of max_input_vars. Over-depth
# registration deletes the normalized root even when it existed already.
NESTING_CASES = [
    ("existing-root-reset", b"a[x]=old; a[x][y]=nested; b=ok; a[z]=restored"),
    ("mixed-depth-roots", b"alpha[x][y][z]=deep; beta[x]=shallow; gamma[x][y]=medium; tail=ok"),
    ("64-level-exact", b"a" + b"[x]" * 64 + b"=value; tail=ok"),
    ("65-level-then-recovery", b"a[x]=one; a" + b"[x]" * 65 + b"=skip; a[y]=after"),
    ("127-level-boundary", b"a" + b"[x]" * 127 + b"=value; tail=ok"),
    ("128-level-boundary", b"a" + b"[x]" * 128 + b"=value; tail=ok"),
    ("129-level-reset-recovery", b"a=old; a" + b"[x]" * 129 + b"=skip; a[y]=after"),
    ("129-level-quota-independent", b"a[x]=old; a" + b"[x]" * 129 + b"=skip; b=keep"),
    ("scalar-promoted-or-reset", b"a=old; a[x]=candidate; b=ok"),
    ("unfinished-suffix-uses-complete-depth", b"a[x][=leaf; b=ok"),
]

LIMIT_CASES = [
    ("ordinary", b"a=1; b=2; c=3; d=4"),
    ("duplicate-consumes-slot", b"a=1; a=2; b=3"),
    ("ignored-empty-name", b"=ignored; a=1; b=2; c=3"),
    ("blank-separators", b";; a=1;;b=2; c=3"),
    ("ignored-bracket-root", b"[bad]=ignored; a=1; b=2"),
    # Exactly the independent 128-pair hard ceiling, even when PHP only
    # admits the first 0-3 variables and all later wire pairs are duplicates.
    ("hard-pair-budget-at-boundary", b"; ".join([b"a=1"] * 128)),
    ("array-append", b"a[]=first; a[]=second; a[]=third"),
    ("array-promotion", b"a=one; a[x]=two; b=three"),
    ("array-duplicate-index", b"a[x]=one; a[x]=two; b=three"),
    ("unfinished-bracket-counts-quota", b"a[x][=first; a[x]tail[y]=second; b=third"),
    ("i64-max-append-consumes-quota", b"a[9223372036854775807]=max; a[]=ignored; b=quota_limited"),
    ("four-level-quota-and-overwrite", b"a[x][y][z][w]=first; a[x][y][z][w]=second; b=third"),
    ("overdepth-consumes-quota", b"a=old; a" + b"[x]" * 65 + b"=ignored; b=tail"),
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


def rust_cookie(binary, header, arrays=False, max_input_vars=None,
                max_input_nesting_level=None):
    command = [str(binary)] + (["--php-arrays"] if arrays else [])
    if max_input_vars is not None:
        command.extend(["--php-max-input-vars", str(max_input_vars)])
    if max_input_nesting_level is not None:
        command.extend(["--php-max-input-nesting-level",
                        str(max_input_nesting_level)])
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
            (False, scalar_router, "cookie_pairs", LIMIT_CASES[:6]),
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
    for depth in (0, 1, 2, 3, 64, 127, 128):
        server, port = start_server(
            args.php_bin, 1, array_router, max_input_nesting_level=depth,
        )
        try:
            for label, header in NESTING_CASES:
                try:
                    expected = php_cookie(port, header, key="cookie_tree")
                    actual = rust_cookie(
                        args.rust_bin, header, arrays=True,
                        max_input_nesting_level=depth,
                    )
                    if expected != actual:
                        raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
                    passed += 1
                except (AssertionError, RuntimeError, ValueError, KeyError) as error:
                    failures += 1
                    print(
                        f"FAIL Cookie max_input_nesting_level={depth} {label}: {error}",
                        file=sys.stderr,
                    )
        finally:
            server.terminate()
            server.wait(timeout=10)

    # An over-depth wire pair still consumes a max_input_vars slot, and
    # can delete the root before the quota skips the next pair.
    server, port = start_server(
        args.php_bin, 1, array_router, max_input_vars=2,
        max_input_nesting_level=1,
    )
    try:
        header = b"a[x]=old; a[x][y]=too_deep; b=quota_skipped"
        expected = php_cookie(port, header, key="cookie_tree")
        actual = rust_cookie(
            args.rust_bin, header, arrays=True, max_input_vars=2,
            max_input_nesting_level=1,
        )
        if expected != actual:
            raise AssertionError(f"PHP={expected!r}, Rust={actual!r}")
        passed += 1
    except (AssertionError, RuntimeError, ValueError, KeyError) as error:
        failures += 1
        print(f"FAIL Cookie combined PHP INI limits: {error}", file=sys.stderr)
    finally:
        server.terminate()
        server.wait(timeout=10)

    if failures:
        raise SystemExit(f"{failures} PHP 8.4 HTTP Cookie cases failed")
    print(
        f"PHP {version} Cookie HTTP SAPI: {passed} cases matched "
        f"({len(CASES)} scalar, {len(ARRAY_CASES)} up-to-64-level array, "
        f"{4 * (6 + len(LIMIT_CASES))} quota cases, "
        f"{7 * len(NESTING_CASES) + 1} configured nesting cases)"
    )


if __name__ == "__main__":
    main()
