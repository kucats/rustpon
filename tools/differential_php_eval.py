#!/usr/bin/env python3
"""Deterministic PHP CLI oracle for the deliberately narrow php_eval prototype.

Only supported integer/boolean/null expressions, exact division and signed remainder are compared. Rejections are asserted
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

BITWISE_NOT_PROGRAMS = [
    "echo ~0;", "echo ~1;", "echo ~-1;", "echo ~~0;",
    "echo ~0x7fff_ffff_ffff_ffff;",
    "echo ~(0-9223372036854775807-1);",
    "echo ~2**3;", "echo ~2**3+1;", "echo ~(2**3+1);",
    "echo -~2**3;", "echo ~-2;", "echo !~0;",
    "echo ~1<=>-2;", "echo ~1===-2;",
    "echo ~+3;", "echo +~3;", "echo ~-~0;",
    "$x=5; echo ~$x;", "$x=5; echo ~($x+1);",
    "$x=1; $x=~$x; echo $x;", "echo ~(1<=>2);",
]


BITWISE_BINARY_PROGRAMS = [
    "echo 6&3;", "echo 6^3;", "echo 6|3;",
    "echo 1&2|4^3;", "echo 1|2^3&4;", "echo 7^2|8&3;",
    "echo 1^2^3;", "echo (1&3)===1;", "echo 1&3===1;",
    "echo 1&3==1;", "echo 1^3==1;", "echo 1|3==1;",
    "echo 1|2<3;", "echo true&false;", "echo true^false;",
    "echo true|false;", "echo 1&true;", "echo 1^true;",
    "echo 1|true;", "echo -1&0x7fff_ffff_ffff_ffff;",
    "echo ~0^0x7fff_ffff_ffff_ffff;",
    "$x=6; $x=$x&3; echo $x;",
    "$x=6; $x=$x^3; echo $x;",
    "$x=6; $x=$x|3; echo $x;",
]


ABS_PROGRAMS = [
    "echo abs(0);", "echo abs(7);", "echo abs(-7);",
    "echo abs(true);", "echo abs(false);", "echo ABS(-9);",
    "echo abs(1-10);", "echo abs(-7)+2;",
    "echo abs(intdiv(-9,2));", "echo intdiv(abs(-9),2);",
    "echo abs(abs(-3));", "echo abs(0-9223372036854775807);",
    "$x=-8; echo abs($x);", "$x=true; echo abs($x);",
]



MIN_MAX_PROGRAMS = [
    "echo min(2,3);", "echo max(2,3);",
    "echo MIN(3,1,2);", "echo MAX(1,3,2);",
    "echo min(1+2,2*2);", "echo max(1,2,3,4);",
    "echo min(true,1);", "echo min(1,true);",
    "echo max(true,2);", "echo max(2,true);",
    "echo min(false,0);", "echo min(0,false);",
    "echo min(false,-1);", "echo max(false,-1);",
    "echo min(true,2,-1);", "echo max(2,true,-1);",
    "echo min(0-9223372036854775807-1,9223372036854775807);",
    "echo max(0-9223372036854775807-1,9223372036854775807);",
    "echo min(max(1,2),3);", "echo max(min(1,2),3);",
    "echo is_bool(min(true,2));", "echo is_int(max(2,true));",
    "echo min(1<2,2);", "echo max(0,1<2);",
    "$x=2; $y=true; echo min($x,$y);",
    "$x=2; $y=true; echo max($y,$x);",
]


TYPE_PREDICATE_PROGRAMS = [
    "echo is_int(0);", "echo is_int(-1);", "echo is_int(true);",
    "echo is_bool(false);", "echo is_bool(1);",
    "echo IS_BOOL(1<2);", "echo IS_INT(true+0);",
    "echo is_integer(7);", "echo is_long(false);",
    "echo is_int(0-9223372036854775807-1);",
    "echo is_bool(is_int(1));", "echo is_int(intdiv(7,3));",
    "echo is_bool(abs(-3));",
    "$x=true; echo is_bool($x);",
    "$x=3; $x=$x<2; echo is_bool($x);",
    "$x=false; $x=$x+1; echo is_int($x);",
]


EMPTY_EXPRESSION_PROGRAMS = [
    "echo empty(0);", "echo empty(1);",
    "echo EMPTY(false);", "echo empty(null);",
    "echo empty(1-1);", "echo empty(1<2);",
    "echo empty(1&2);", "echo empty(intdiv(8,4)-2);",
    "echo empty(isset($missing));", "echo empty(empty($missing));",
    "$x=1; echo empty($x-1);", "$x=0; echo empty($x+1);",
    "$x=7; echo empty(($x));", "$x=0; echo empty(($x));",
    "$x=null; echo empty($x+0);",
    "echo is_bool(empty(0));", "echo empty(0)===true;",
]


VARIABLE_PROBE_PROGRAMS = [
    "echo isset($missing);", "echo empty($missing);",
    "echo ISSET($missing);", "echo EMPTY($missing);",
    "$x=null; echo isset($x);", "$x=null; echo empty($x);",
    "$x=false; echo isset($x);", "$x=false; echo empty($x);",
    "$x=0; echo isset($x);", "$x=0; echo empty($x);",
    "$x=-1; echo empty($x);", "$x=7; echo empty($x);",
    "$x=7; $y=0; echo isset($x,$y,);",
    "$x=7; $y=null; echo isset($x,$y);",
    "$x=7; echo isset($x,$missing);",
    "$x=7; echo !empty($x);",
    "$x=null; echo is_bool(isset($x));",
    "$x=0; echo empty($x)===true;",
    "$x=7; echo isset($x)===true;",
]


UNSET_PROGRAMS = [
    "$x=1; unset($x); echo isset($x);",
    "$x=1; unset($x); echo empty($x);",
    "$x=1; $y=2; unset($x,$y,); echo isset($x,$y);",
    "$x=1; unset($x); $x=3; echo $x;",
    "$x=1; unset($missing); echo $x;",
    "$x=1; UNSET($x); echo empty($x);",
    "$x=1; $y=2; unset($x); echo isset($y);",
    "unset($missing); echo isset($missing);",
    "unset($x,); echo 1;",
]


NULL_PROGRAMS = [
    "echo null;", "echo NULL;", "echo NuLl;",
    "echo null===null;", "echo null===0;", "echo null==0;",
    "echo null==false;", "echo null==-1;", "echo null!=1;",
    "echo null<=>-1;", "echo -1<=>null;",
    "echo null<=>0;", "echo null<=>1;",
    "echo null+2;", "echo 2+null;", "echo null*7;",
    "echo null|2;", "echo null&2;", "echo null^2;",
    "echo !null;", "echo !!null;", "echo -null;",
    "echo boolval(null);", "echo intval(null);",
    "echo is_null(null);", "echo IS_NULL(false);",
    "echo is_bool(null);", "echo is_int(null);",
    "echo is_scalar(null);", "echo is_scalar(true);",
    "echo is_scalar(0);", "echo is_numeric(null);",
    "echo is_numeric(true);", "echo is_numeric(-3);",
    "echo min(null,2);", "echo max(null,2);",
    "echo min(null,0);", "echo max(null,0);",
    "echo min(0,null);", "echo max(0,null);",
    "echo is_null(min(null,1));",
    "$x=null; echo is_null($x);",
    "$x=null; $x=$x+1; echo is_int($x);",
    "$x=NULL; echo boolval($x);",
]

SCALAR_CONVERSION_PROGRAMS = [
    "echo boolval(0);", "echo boolval(1);", "echo boolval(-1);",
    "echo boolval(false);", "echo boolval(true);",
    "echo BOOLVAL(1<2);", "echo boolval(0-9223372036854775807-1);",
    "echo intval(0);", "echo intval(-9);",
    "echo intval(true);", "echo intval(false);",
    "echo INTVAL(9223372036854775807);",
    "echo intval(0-9223372036854775807-1);",
    "echo intval(2<1);", "echo intval(boolval(-3));",
    "echo intval(7,2);", "echo intval(-42,16);",
    "echo intval(123,0);", "echo intval(123,1);",
    "echo intval(123,999);", "echo intval(123,-100);",
    "echo intval(true,37);", "echo intval(false,-1);",
    "echo intval(123,true);", "echo intval(123,false);",
    "echo intval(9223372036854775807,-1);",
    "echo intval(0-9223372036854775807-1,2);",
    "echo intval(intdiv(-7,2),-42);",
    "echo is_int(intval(true,37));",
    "echo intval(7,intval(true));",
    "$x=7; $base=-2; echo intval($x,$base);",
    "echo boolval(intval(false));",
    "echo is_int(intval(true));", "echo is_bool(boolval(5));",
    "echo is_bool(intval(true));", "echo is_int(boolval(1));",
    "echo boolval(1+2*3);", "echo intval(true)+2;",
    "$x=false; $x=intval($x); echo is_int($x);",
    "$x=-7; echo boolval($x);",
]


INTDIV_PROGRAMS = [
    "echo intdiv(7,3);", "echo intdiv(-7,3);",
    "echo intdiv(7,-3);", "echo intdiv(-7,-3);",
    "echo intdiv(true,2);", "echo intdiv(false,2);",
    "echo INTDIV(9,2);", "echo intdiv(9,2)*3;",
    "echo intdiv(8,intdiv(4,2));",
    "echo intdiv(5|2,2);",
    "echo intdiv(0-9223372036854775807-1,1);",
    "echo intdiv(9223372036854775807,1);",
    "$x=9; echo intdiv($x,2);",
    "$x=-9; $y=2; echo intdiv($x,$y);",
]


SHIFT_PROGRAMS = [
    "echo 1<<2;", "echo 16>>2;", "echo 1<<3+1;",
    "echo 1<<2<<1;", "echo 1<<2<3;", "echo 1<<2===4;",
    "echo 1<<2&3;", "echo 1<<2<=>3;",
    "echo true<<2;", "echo 2<<true;",
    "echo 1<<63;", "echo 1<<64;", "echo 1<<65;",
    "echo -1<<64;", "echo -1>>64;", "echo -8>>65;",
    "echo 1>>64;", "echo (0-9223372036854775807-1)>>63;",
    "echo (0-9223372036854775807-1)<<1;",
    "echo 9223372036854775807>>63;",
    "echo 1<<9223372036854775807;",
    "$x=3; $x=$x<<2; echo $x;",
    "$x=-8; echo $x>>2;",
    "echo (2<<3)+1;", "echo 8>>(1+1);",
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

COMPOUND_ASSIGNMENT_PROGRAMS = [
    "$x=1; $x+=2; echo $x;",
    "$x=10; $x-=3; echo $x;",
    "$x=2; $x+=3*4; echo $x;",
    "$x=2; $x-=1<2; echo $x;",
    "$x=true; $x+=2; echo is_int($x);",
    "$x=false; $x-=1; echo $x;",
    "$x=null; $x-=3; echo $x;",
    "$x=5; $x+=true; echo $x;",
    "$x=0-9223372036854775807-1; $x+=1; echo $x;",
    "$x=9223372036854775807; $x-=1; echo $x;",
    "$x=1; $x+=2; $x-=3; echo is_int($x);",
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
    compound_assignments = list(COMPOUND_ASSIGNMENT_PROGRAMS)
    for _ in range(128):
        start = rng.randrange(-100000, 100001)
        delta = rng.randrange(-100000, 100001)
        extra = rng.randrange(-50, 51)
        operator = rng.choice(["+=", "-="])
        compound_assignments.append(
            f"$x={start}; $y={delta}; $x {operator} $y+({extra}); echo $x;"
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
    bitwise_not = list(BITWISE_NOT_PROGRAMS)
    for _ in range(64):
        value = rng.randrange(-500, 501)
        offset = rng.randrange(-100, 101)
        bitwise_not.append(f"$x={value}; echo ~($x+({offset}));")
    bitwise_binary = list(BITWISE_BINARY_PROGRAMS)
    for _ in range(128):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        op = rng.choice(["&", "^", "|"])
        bitwise_binary.append(f"$x={left}; $y={right}; echo $x {op} $y;")
    for _ in range(32):
        left = rng.randrange(0, 256)
        middle = rng.randrange(0, 256)
        right = rng.randrange(0, 256)
        bitwise_binary.append(f"echo {left} | {middle} ^ {right} & 7;")
    intdiv_programs = list(INTDIV_PROGRAMS)
    for _ in range(128):
        numerator = rng.randrange(-100000, 100001)
        denominator = rng.choice([n for n in range(-100, 101) if n])
        intdiv_programs.append(f"echo intdiv({numerator},{denominator});")
    abs_programs = list(ABS_PROGRAMS)
    for _ in range(128):
        value = rng.randrange(-100000, 100001)
        delta = rng.randrange(-1000, 1001)
        abs_programs.append(f"$x={value}; echo abs($x+({delta}));")
    shifts = list(SHIFT_PROGRAMS)
    for _ in range(128):
        value = rng.randrange(-100000, 100001)
        count = rng.randrange(0, 72)
        op = rng.choice(["<<", ">>"])
        shifts.append(f"$x={value}; echo $x {op} {count};")
    spaceship = list(SPACESHIP_PROGRAMS)
    for _ in range(64):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        spaceship.append(f"$x={left}; $y={right}; echo ($x+1)<=>($y*2);")
    for _ in range(32):
        left = rng.randrange(-500, 501)
        right = rng.randrange(-500, 501)
        spaceship.append(f"$x={left}; $y={right}; echo ($x<=>$y)===-1;")
    type_predicates = list(TYPE_PREDICATE_PROGRAMS)
    for _ in range(96):
        value = rng.randrange(-100000, 100001)
        predicate = rng.choice(["is_int", "IS_INT", "is_integer", "is_long", "is_bool", "IS_BOOL"])
        type_predicates.append(f"$x={value}; echo {predicate}($x);")
    for _ in range(32):
        left = rng.randrange(-100, 101)
        right = rng.randrange(-100, 101)
        predicate = rng.choice(["is_int", "is_bool"])
        type_predicates.append(f"$x=({left}<{right}); echo {predicate}($x);")
    variable_probes = list(VARIABLE_PROBE_PROGRAMS)
    for _ in range(128):
        value = rng.randrange(-100000, 100001)
        other = rng.choice(["null", "true", "false", "0", "1"])
        variable_probes.append(
            f"$x={value}; $y={other}; echo isset($x,$y,);"
        )
        variable_probes.append(f"$x={value}; echo empty($x);")
        variable_probes.append(f"$x={value}; echo isset($x,$missing);")
    unset_programs = list(UNSET_PROGRAMS)
    for _ in range(64):
        value = rng.randrange(-100000, 100001)
        other = rng.randrange(-100000, 100001)
        unset_programs.append(f"$x={value}; unset($x); echo isset($x);")
        unset_programs.append(f"$x={value}; unset($x); $x={other}; echo $x;")
        unset_programs.append(
            f"$x={value}; $y={other}; unset($x,$y,); echo empty($y);"
        )
        unset_programs.append(f"$x={value}; unset($missing); echo $x;")
    empty_expressions = list(EMPTY_EXPRESSION_PROGRAMS)
    for _ in range(128):
        value = rng.randrange(-500, 501)
        delta = rng.randrange(-500, 501)
        empty_expressions.append(f"$x={value}; echo empty($x+({delta}));")
        empty_expressions.append(f"$x={value}; echo is_bool(empty($x+({delta})));")
    null_programs = list(NULL_PROGRAMS)
    for _ in range(64):
        value = rng.randrange(-100000, 100001)
        null_programs.append(f"echo null<=>{value};")
        null_programs.append(f"echo {value}<=>null;")
        null_programs.append(f"echo is_null(min(null,{value}));")
        null_programs.append(f"echo is_numeric({value});")
        null_programs.append(f"$x=null; echo is_scalar($x+{value});")
    scalar_conversions = list(SCALAR_CONVERSION_PROGRAMS)
    for _ in range(128):
        value = rng.randrange(-100000, 100001)
        delta = rng.randrange(-1000, 1001)
        scalar_conversions.append(f"$x={value}; echo boolval($x+({delta}));")
        scalar_conversions.append(f"$x={value}; echo intval(boolval($x+({delta})));")
    for _ in range(64):
        value = rng.randrange(-100000, 100001)
        scalar_conversions.append(f"$x={value}; echo is_bool(boolval($x));")
        scalar_conversions.append(f"$x={value}; echo is_int(intval($x));")
    for _ in range(128):
        value = rng.randrange(-100000, 100001)
        base = rng.choice([-100, -1, 0, 1, 2, 8, 10, 16, 36, 37, 999])
        scalar_conversions.append(f"$x={value}; echo intval($x,{base});")
        scalar_conversions.append(
            f"$x={value}; echo is_int(intval(boolval($x),{base}));"
        )
    extrema_programs = list(MIN_MAX_PROGRAMS)
    for _ in range(192):
        function = rng.choice(["min", "max", "MIN", "MAX"])
        count = rng.randrange(2, 6)
        operands = [
            rng.choice(["true", "false", str(rng.randrange(-200, 201))])
            for _ in range(count)
        ]
        expression = f"{function}({','.join(operands)})"
        extrema_programs.append(f"echo {expression};")
        extrema_programs.append(f"echo is_bool({expression});")
    cases = (
        [f"echo {expr};" for expr in expressions]
        + variable_programs + radix_programs + arithmetic_programs + comparisons + powers
        + booleans + logical_not + bitwise_not + bitwise_binary + shifts + spaceship
        + intdiv_programs + abs_programs + type_predicates + extrema_programs
        + scalar_conversions + null_programs + variable_probes + empty_expressions
        + unset_programs + compound_assignments
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
        b"<?php echo intdiv();",
        b"<?php echo intdiv(1);",
        b"<?php echo intdiv(1,2,3);",
        b"<?php echo intdiv(1,);",
        b"<?php echo intdiv(,2);",
        b"<?php echo intdivide(1,2);",
        b"<?php echo intdiv(1,2;",
        b"<?php echo intdiv($missing,2);",
        b"<?php echo is_int();",
        b"<?php echo is_bool();",
        b"<?php echo is_int(1,2);",
        b"<?php echo is_bool(true,);",
        b"<?php echo is_integer(,1);",
        b"<?php echo is_long(1;",
        b"<?php echo is_intx(1);",
        b"<?php echo is_boolean(true);",
        b"<?php echo is_int($missing);",
        b"<?php echo isset();",
        b"<?php echo empty();",
        b"<?php echo isset(1);",
        b"<?php echo empty(0,1);",
        b"<?php echo empty(0,);",
        b"<?php echo empty(1+);",
        b"<?php echo empty($missing+1);",
        b"<?php echo isset($x,1);",
        b"<?php echo empty($x,$y);",
        b"<?php echo empty($x,);",
        b"<?php echo isset($x,,);",
        b"<?php echo isset($x;",
        b"<?php echo empty($x;",
        b"<?php echo issetx($x);",
        b"<?php echo emptyx($x);",
        b"<?php echo isset($x[0]);",
        b"<?php echo empty($x->foo);",
        b"<?php $x=1; unset($x); echo $x;",
        b"<?php unset(); echo 1;",
        b"<?php unset($x,,); echo 1;",
        b"<?php unset($x,1); echo 1;",
        b"<?php unset($x[0]); echo 1;",
        b"<?php unset($GLOBALS); echo 1;",
        b"<?php unset($x; echo 1;",
        b"<?php unset($x) echo 1;",
        b"<?php unsetx($x); echo 1;",
        b"<?php echo nullx;",
        b"<?php echo null();",
        b"<?php echo is_null();",
        b"<?php echo is_null(1,2);",
        b"<?php echo is_null(1,);",
        b"<?php echo is_scalar();",
        b"<?php echo is_numeric();",
        b"<?php echo is_scalar($missing);",
        b"<?php echo abs();",
        b"<?php echo abs(1,2);",
        b"<?php echo abs(,1);",
        b"<?php echo abs(1,);",
        b"<?php echo abs(1;",
        b"<?php echo absolute(-1);",
        b"<?php echo abs($missing);",
        b"<?php echo boolval();",
        b"<?php echo intval();",
        b"<?php echo boolval(1,2);",
        b"<?php echo intval(1,2,3);",
        b"<?php echo intval(1,);",
        b"<?php echo intval(1,,2);",
        b"<?php echo intval(1,$missing);",
        b"<?php echo intval(1,1/0);",
        b"<?php echo boolval(1,);",
        b"<?php echo intval(,1);",
        b"<?php echo boolvalue(1);",
        b"<?php echo integer_value(1);",
        b"<?php echo boolval($missing);",
        b"<?php echo intval($missing);",
        b"<?php echo boolval(1;",
        b"<?php echo min();",
        b"<?php echo max();",
        b"<?php echo min(1);",
        b"<?php echo max(true);",
        b"<?php echo min(1,);",
        b"<?php echo max(,2);",
        b"<?php echo min(1,2,);",
        b"<?php echo max(1,2,3,);",
        b"<?php echo min(1,2;",
        b"<?php echo minx(1,2);",
        b"<?php echo maximal(1,2);",
        b"<?php echo max($missing,2);",
        # PHP promotes abs(PHP_INT_MIN) to float; intentionally unsupported.
        b"<?php echo abs(0-9223372036854775807-1);",
        b"<?php echo 1&;",
        b"<?php echo 1^;",
        b"<?php echo 1|;",
        b"<?php echo &1;",
        b"<?php echo ^1;",
        b"<?php echo |1;",
        b"<?php echo 1<<;",
        b"<?php echo 1>>;",
        b"<?php echo <<1;",
        b"<?php echo >>1;",
        b"<?php echo 1<<<2;",
        b"<?php echo 1>>>2;",
        b"<?php echo 1&&2;",
        b"<?php echo 1||2;",
        b"<?php echo 1&$missing;",
        b"<?php echo 1^$missing;",
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
        b"<?php $x+=1; echo $x;",
        b"<?php $x=1; $x+=; echo $x;",
        b"<?php $x=1; $x+ =2; echo $x;",
        b"<?php $x=1; $x-=1/0; echo $x;",
        # PHP promotes integer overflow to float; intentionally unsupported.
        b"<?php $x=9223372036854775807; $x+=1; echo $x;",
        b"<?php $x=0-9223372036854775807-1; $x-=1; echo $x;",
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

    # PHP 8.4 raises TypeError for bitwise not applied to booleans.
    # Unlike arbitrary unsupported syntax, these are verified on both runtimes.
    type_errors = [
        b"<?php echo ~true;",
        b"<?php echo ~null;",
        b"<?php echo ~false;",
        b"<?php echo ~(1<2);",
        b"<?php echo ~!0;",
        b"<?php $x=true; echo ~$x;",
    ]
    for index, code in enumerate(type_errors):
        php = execute([args.php_bin, "-d", "display_errors=0"], code)
        rust = execute([str(args.rust_bin)], code)
        if (php.returncode == 0 or b"TypeError" not in php.stderr
                or rust.returncode == 0 or rust.stdout):
            failures += 1
            print(
                f"FAIL bitwise_type_error={index} php={php.returncode}:{php.stderr[:100]!r} "
                f"rust={rust.returncode}:{rust.stdout[:80]!r}",
                file=sys.stderr,
            )

    # PHP 8.4 throws ArithmeticError for negative shift counts.
    negative_shifts = [
        b"<?php echo 1<<-1;",
        b"<?php echo 1>>-1;",
        b"<?php echo 1<<(0-2);",
        b"<?php $x=-1; echo 2>>$x;",
        b"<?php echo 1<<(-9223372036854775807);",
    ]
    for index, code in enumerate(negative_shifts):
        php = execute([args.php_bin, "-d", "display_errors=0"], code)
        rust = execute([str(args.rust_bin)], code)
        if (php.returncode == 0 or b"ArithmeticError" not in php.stderr
                or rust.returncode == 0 or rust.stdout):
            failures += 1
            print(
                f"FAIL negative_shift={index} php={php.returncode}:{php.stderr[:100]!r} "
                f"rust={rust.returncode}:{rust.stdout[:80]!r}",
                file=sys.stderr,
            )

    # intdiv() raises distinct PHP runtime exceptions for zero divisors
    # and the one signed-64-bit overflow case.
    intdiv_errors = [
        (b"<?php echo intdiv(1,0);", b"DivisionByZeroError"),
        (b"<?php echo intdiv(1,0*3);", b"DivisionByZeroError"),
        (b"<?php echo intdiv(0-9223372036854775807-1,-1);", b"ArithmeticError"),
    ]
    for index, (code, error_class) in enumerate(intdiv_errors):
        php = execute([args.php_bin, "-d", "display_errors=0"], code)
        rust = execute([str(args.rust_bin)], code)
        if (php.returncode == 0 or error_class not in php.stderr
                or rust.returncode == 0 or rust.stdout):
            failures += 1
            print(
                f"FAIL intdiv_error={index} php={php.returncode}:{php.stderr[:100]!r} "
                f"rust={rust.returncode}:{rust.stdout[:80]!r}",
                file=sys.stderr,
            )

    print(
        f"PHP CLI integer/boolean/null/variable/comparison/spaceship/power/bitwise/shift/intdiv/abs/type-predicates/min-max/scalar-conversions/empty-expression/unset-lifecycle differential: "
        f"{len(cases)} compatible cases, {len(invalid)} strict rejections, "
        f"{len(type_errors)} PHP TypeError rejections, "
        f"{len(negative_shifts)} PHP shift ArithmeticError rejections, "
        f"{len(intdiv_errors)} PHP intdiv runtime-error rejections, {failures} failures"
    )
    return int(failures != 0)


if __name__ == "__main__":
    sys.exit(main())
