#!/usr/bin/env python3
"""Deterministic PHP CLI oracle for the deliberately narrow php_eval prototype.

Only supported integer/boolean expressions, exact division and signed remainder are compared. Rejections are asserted
separately; this is not evidence of general PHP language compatibility.
"""
import argparse
import random
import subprocess
import sys
from pathlib import Path


FIXED = [
    "0", "00", "01", "07", "010", "0_7", "00_7", "0o10", "0O10",
    "0x2a", "0X2A", "0b1010", "0B1010", "1_000", "0xF_F",
    "0b1_0", "0o7_7", "0x7fff_ffff_ffff_ffff",
    "0o777777777777777777777",
    "1", "42", "-1", "+3", "1+2*3", "(1+2)*3",
    "8-3-2", "2*-3+4", "-(-5)", "1- -2",
    "9223372036854775807", "0-9223372036854775807",
    "((3))", "2 * (3 + (4 - 5))", "3+-2",
]


LOGICAL_NOT_PROGRAMS = [
    "echo !true;", "echo !false;", "echo !0;", "echo !1;",
    "echo !-1;", "echo !!true;", "echo !!false;",
    "echo !(1<2);", "echo !2**0;", "echo !2**2;",
    "echo !false===true;", "echo !false+2;", "echo !1<2;",
    "echo !true==false;", "echo !true!==false;",
    "$flag=false; echo !$flag;", "$x=0; $x=!$x; echo $x;",
]

SPACESHIP_PROGRAMS = [
    "echo 1<=>2;", "echo 2<=>2;", "echo 3<=>2;",
    "echo true<=>2;", "echo false<=>-1;", "echo false<=>0;",
    "echo true<=>false;", "echo 1+2<=>2*2;",
    "echo (1<=>2)===-1;", "echo (2<=>1)+2;",
    "echo !0<=>true;", "echo (1<=>2)<0;",
    "echo (0-9223372036854775807-1)<=>0;",
    "echo 9223372036854775807<=>(0-9223372036854775807-1);",
    "$x=4; $y=5; echo $x<=>$y;",
    "$x=1<=>2; echo $x+2;",
]

ASSIGNMENT_PROGRAMS = [
    "$x=7; echo $x;",
    "$x=7; $x=$x+2; echo $x*3;",
    "$foo_1=3; $_bar=4; echo $foo_1*$_bar;",
    "$A=5; $a=2; echo $A-$a;",
    "$zero=0; echo -$zero;",
    "$x=1; $x=2; $x=3; echo $x;",
    "$x=5; $y=$x*2; echo $y-$x;",
    "$first=10; $second=$first-7; echo ($first+$second)*2;",
]


def radix_literal(value, radix, separator=False):
    if radix == 10:
        digits, prefix = str(value), ""
    elif radix == 8:
        digits, prefix = format(value, "o"), "0o"
    elif radix == 16:
        digits, prefix = format(value, "x"), "0x"
    elif radix == 2:
        digits, prefix = format(value, "b"), "0b"
    elif radix == 80:
        digits, prefix = format(value, "o"), "0"
    else:
        raise ValueError(radix)
    if separator and len(digits) > 1:
        groups = []
        while digits:
            groups.append(digits[-3:])
            digits = digits[:-3]
        digits = "_".join(reversed(groups))
    return prefix + digits


def generated(rng, depth):
    if depth == 0:
        return str(rng.randrange(0, 10))
    if rng.randrange(4) == 0:
        return rng.choice(["+", "-"]) + "(" + generated(rng, depth - 1) + ")"
    lhs = generated(rng, depth - 1)
    rhs = generated(rng, depth - 1)
    return "(" + lhs + " " + rng.choice(["+", "-", "*"]) + " " + rhs + ")"


def execute(command, code):
    return subprocess.run(command, input=code, capture_output=True, timeout=10, check=False)


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument("--rust-bin", type=Path, required=True)
    cli.add_argument("--php-bin", default="php")
    args = cli.parse_args()

    rng = random.Random(0x52555354504F4E)
    expressions = FIXED + [generated(rng, rng.randrange(1, 5)) for _ in range(128)]
    variable_programs = list(ASSIGNMENT_PROGRAMS)
    for _ in range(64):
        start = rng.randrange(-50, 51)
        delta = rng.randrange(0, 20)
        subtract = rng.randrange(-10, 11)
        variable_programs.append(
            f"$x={start}; $y={delta}; $x=$x+$y; echo ($x - ({subtract}))*2;"
        )
    radix_programs = []
    for _ in range(128):
        value = rng.randrange(0, 1_000_000)
        radix = rng.choice([2, 8, 10, 16, 80])
        literal = radix_literal(value, radix, separator=bool(rng.randrange(2)))
        radix_programs.append(f"echo ({literal}+3)*2;")
    for _ in range(64):
        value = rng.randrange(0, 1_000_000)
        radix = rng.choice([2, 8, 10, 16, 80])
        literal = radix_literal(value, radix, separator=bool(rng.randrange(2)))
        radix_programs.append(f"$x={literal}; echo $x*2;")
    arithmetic_programs = [
        "echo 8/4;", "echo 0/2;", "echo 7%3;", "echo -7%3;",
        "echo 7%-3;", "echo -7%-3;", "echo 8/2*3%5;",
        "echo 3+8/4*5;", "$x=20; $y=3; echo $x%$y;",
        "echo (0-9223372036854775807-1)%-1;",
        "echo (0-9223372036854775807-1)/2;",
    ]
    for _ in range(128):
        quotient = rng.randrange(-1000, 1001)
        divisor = rng.choice([n for n in range(-20, 21) if n])
        dividend = quotient * divisor
        arithmetic_programs.append(f"echo ({dividend})/({divisor});")
    for _ in range(128):
        dividend = rng.randrange(-1000, 1001)
        divisor = rng.choice([n for n in range(-20, 21) if n])
        arithmetic_programs.append(f"$x={dividend}; $y={divisor}; echo $x%$y;")
    comparisons = [
        "echo 1<2;", "echo 1>2;", "echo 2<=2;", "echo 2>=3;",
        "echo 1==1;", "echo 1===1;", "echo 1!==1;", "echo 1!=2;",
        "echo 1<>2;", "echo 1+2*3==7;", "echo (2+3)*4>=19;",
        "echo 0x7fff_ffff_ffff_ffff>0;",
        "echo (0-9223372036854775807-1)<0;",
        "$x=4; $y=5; echo $x+1==($y*1);",
        "$x=4; $y=5; echo $x>$y;",
    ]
    comparison_ops = ["<", "<=", ">", ">=", "==", "!=", "===", "!==", "<>"]
    for _ in range(192):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        op = rng.choice(comparison_ops)
        comparisons.append(
            f"$x={left}; $y={right}; echo ($x+1) {op} ($y*2);"
        )
    powers = [
        "echo 2**3;", "echo 2**3**2;", "echo (2**3)**2;",
        "echo -2**2;", "echo (-2)**2;", "echo (-2)**3;",
        "echo 2**-(-2);", "echo 0**0;", "echo 0**7;",
        "echo 2**0;", "echo 2**62;", "echo (-2)**63;", "echo -1**63;",
        "echo 2**2*3;", "echo 2*3**2;", "echo 2**(1+2);",
        "echo 1+2**3==9;", "$x=3; echo $x**3;",
        "echo (0-9223372036854775807-1)**1;",
    ]
    for _ in range(192):
        base = rng.randrange(-5, 6)
        exponent = rng.randrange(0, 11)
        delta = rng.randrange(-20, 21)
        powers.append(f"$x={base}; echo ($x**{exponent})+({delta});")
    booleans = [
        "echo true;", "echo false;", "echo TRUE;", "echo FALSE;",
        "echo true==1;", "echo true===1;", "echo true!==1;",
        "echo false==0;", "echo false===0;", "echo true>2;",
        "echo false<-1;", "echo (1<2);", "echo (1<2)===true;",
        "echo (1<2)+1;", "echo (2>3)+1;", "echo (1<2)*3;",
        "echo (1<2)**3;", "echo +true;", "echo -true;",
        "$x=1<2; echo $x;", "$x=2<1; echo $x;",
        "$x=true; $y=false; echo $x==1;",
        "$x=2<1; $x=$x+5; echo $x;",
        "$x=1<2; $x=3; echo $x;",
    ]
    for _ in range(192):
        a = rng.randrange(-50, 51)
        b = rng.randrange(-50, 51)
        c = rng.randrange(-10, 11)
        op = rng.choice(comparison_ops)
        booleans.append(
            f"$flag=({a} {op} {b}); echo ($flag+{c})===({c}+({a} {op} {b}));"
        )
    logical_not = list(LOGICAL_NOT_PROGRAMS)
    for value in range(-10, 11):
        logical_not.append(f"$x={value}; echo !($x<0);")
        logical_not.append(f"$x={value}; echo !!$x;")
    spaceship = list(SPACESHIP_PROGRAMS)
    for _ in range(64):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        spaceship.append(f"$x={left}; $y={right}; echo ($x+1)<=>($y*2);")
    for _ in range(32):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        spaceship.append(f"$x={left}; $y={right}; echo ($x<=>$y)===-1;")
    cases = (
        [f"echo {expr};" for expr in expressions]
        + variable_programs + radix_programs + arithmetic_programs + comparisons + powers
        + booleans + logical_not + spaceship
    )
    failures = 0
    for index, program in enumerate(cases):
        code = ("<?php " + program).encode("ascii")
        php = execute([args.php_bin, "-d", "display_errors=0"], code)
        rust = execute([str(args.rust_bin)], code)
        if php.returncode != 0 or rust.returncode != 0 or php.stdout != rust.stdout:
            failures += 1
            print(
                f"FAIL case={index} program={program!r} "
                f"php={php.returncode}:{php.stdout!r} "
                f"rust={rust.returncode}:{rust.stdout!r} "
                f"stderr={rust.stderr[:160]!r}",
                file=sys.stderr,
            )

    invalid = [
        b"<?php echo 08;",
        b"<?php echo 09;",
        b"<?php echo 0_8;",
        b"<?php echo 00_8;",
        b"<?php echo 0o8;",
        b"<?php echo 0b102;",
        b"<?php echo 0o78;",
        b"<?php echo 0x;",
        b"<?php echo 0b;",
        b"<?php echo 0o;",
        b"<?php echo 0x_1;",
        b"<?php echo 0b_1;",
        b"<?php echo 0o_1;",
        b"<?php echo 1_;",
        b"<?php echo 1__0;",
        b"<?php echo 0x1_;",
        b"<?php echo 0b1_;",
        b"<?php echo 0o7_;",
        b"<?php echo 0x1g;",
        b"<?php echo 0x8000000000000000;",
        b"<?php echo 0xffffffffffffffff;",
        b"<?php echo 1/2;",
        b"<?php echo -3/2;",
        b"<?php echo 1/0;",
        b"<?php echo 2**-1;",
        b"<?php echo 0**-1;",
        b"<?php echo 2**63;",
        b"<?php echo (-2)**64;",
        b"<?php echo 1**4294967296;",
        b"<?php echo 2**;",
        b"<?php echo 2***3;",
        b"<?php echo 2**3**;",
        b"<?php echo 1<;",
        b"<?php echo 1!==;",
        b"<?php echo 1<2<3;",
        b"<?php echo 1<=>;",
        b"<?php echo 1<=>2<=>3;",
        b"<?php echo 1<==>2;",
        b"<?php echo truex;",
        b"<?php echo FALSE_1;",
        b"<?php echo true&&false;",
        b"<?php echo !;",
        b"<?php echo !();",
        b"<?php echo !!!;",
        b"<?php echo !$missing;",
        b"<?php echo true<false<true;",
        b"<?php echo (1<2;",
        b"<?php echo 1%0;",
        b"<?php echo (0-9223372036854775807-1)/-1;",
        b"<?php echo $x;",
        b"<?php echo 1+;",
        b"<?php echo 1--2;",
        b"<?php echo 1++2;",
        b"<?php echo 1; echo 2;",
        b"<?php echo 9223372036854775807+1;",
        b"<?php echo " + b"(" * 33 + b"1" + b")" * 33 + b";",
        b"<?php echo " + b"+".join([b"1"] * 130) + b";",
        b"<?php echo 1;" + b" " * 4096,
        b"<?php echo $missing;",
        b"<?php $x=1; echo $missing;",
        b"<?php $x=$x+1; echo $x;",
        b"<?php $x==1; echo $x;",
        b"<?php $GLOBALS=2; echo 1;",
        b"<?php $x=1; echo $x; $x=2;",
        b"<?php $" + b"a" * 65 + b"=1; echo 1;",
        b"<?php " + b"".join(f"$v{i}={i};".encode("ascii") for i in range(33)) + b"echo 1;",
        b"<?php " + b"$x=1;" * 65 + b"echo $x;",
    ]
    for index, code in enumerate(invalid):
        rust = execute([str(args.rust_bin)], code)
        if rust.returncode == 0 or rust.stdout:
            failures += 1
            print(
                f"FAIL invalid={index}: status={rust.returncode} stdout={rust.stdout[:80]!r}",
                file=sys.stderr,
            )

    print(
        f"PHP CLI integer/boolean/variable/comparison/spaceship/power differential: {len(cases)} compatible "
        f"cases, {len(invalid)} strict rejections, {failures} failures"
    )
    return int(failures != 0)


if __name__ == "__main__":
    sys.exit(main())
