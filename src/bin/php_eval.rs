//! Research-only, bounded PHP integer-expression evaluator.
//!
//! This is the first standalone language execution slice, NOT a PHP VM.
//! Supported grammar: bounded typed scalar assignments followed by one echo;
//! PHP 8.4 decimal/octal/binary/hex i64 literals (with digit separators),
//! variables, typed integer/boolean values, unary +/-, logical/bitwise not, parentheses, arithmetic, integer powers, comparisons (including <=>), and integer bitwise &, ^, |.
//! Division is supported only when its result is an exact i64.
//! Variables are ASCII identifiers; there is no PHP symbol table.
//! Unsupported PHP constructs and arithmetic overflow fail closed.

use std::io::{self, Read, Write};

const MAX_SOURCE: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_WORK: usize = 256;
const MAX_VARIABLES: usize = 32;
const MAX_ASSIGNMENTS: usize = 64;
const MAX_VARIABLE_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalError {
    SourceTooLong,
    Syntax,
    TrailingInput,
    Overflow,
    DivisionByZero,
    UnsupportedFloat,
    BitwiseTypeError,
    DepthLimit,
    WorkLimit,
    UnknownVariable,
    VariableLimit,
    AssignmentLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Comparison {
    Spaceship,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

// Bounded subset of PHP's runtime value model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Value {
    Int(i64),
    Bool(bool),
}

impl Value {
    fn as_int(self) -> i64 {
        match self {
            Self::Int(value) => value,
            Self::Bool(value) => i64::from(value),
        }
    }

    fn as_bool(self) -> bool {
        match self {
            Self::Int(value) => value != 0,
            Self::Bool(value) => value,
        }
    }

    fn php_equal(self, other: Self) -> bool {
        if matches!(self, Self::Bool(_)) || matches!(other, Self::Bool(_)) {
            self.as_bool() == other.as_bool()
        } else {
            self == other
        }
    }

    fn php_order(self, other: Self) -> (i64, i64) {
        if matches!(self, Self::Bool(_)) || matches!(other, Self::Bool(_)) {
            (i64::from(self.as_bool()), i64::from(other.as_bool()))
        } else {
            (self.as_int(), other.as_int())
        }
    }

    fn echo(self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Bool(true) => "1".to_owned(),
            Self::Bool(false) => String::new(),
        }
    }
}

struct Parser<'a> {
    source: &'a [u8],
    offset: usize,
    work: usize,
    bindings: Vec<(&'a [u8], Value)>,
    assignments: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            offset: 0,
            work: 0,
            bindings: Vec::new(),
            assignments: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.source.get(self.offset).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|b| b.is_ascii_whitespace()) {
            self.offset += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        self.skip_space();
        if self.peek() == Some(byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn charge(&mut self) -> Result<(), EvalError> {
        self.work += 1;
        if self.work > MAX_WORK {
            Err(EvalError::WorkLimit)
        } else {
            Ok(())
        }
    }

    fn number(&mut self) -> Result<i64, EvalError> {
        self.skip_space();
        let start = self.offset;
        let source = self.source;
        let mut radix = 10_u32;
        let mut prefixed = false;

        if source.get(start) == Some(&b'0') {
            match source.get(start + 1) {
                Some(b'x' | b'X') => {
                    radix = 16;
                    prefixed = true;
                }
                Some(b'b' | b'B') => {
                    radix = 2;
                    prefixed = true;
                }
                Some(b'o' | b'O') => {
                    radix = 8;
                    prefixed = true;
                }
                Some(b'0'..=b'9' | b'_') => radix = 8,
                _ => {}
            }
        }
        if prefixed {
            self.offset += 2;
        }
        let mut digits = 0_usize;
        let mut value = 0_i64;
        while let Some(&byte) = source.get(self.offset) {
            if byte == b'_' {
                // PHP 8.4 permits separators strictly between base-valid digits.
                let next_is_digit = source
                    .get(self.offset + 1)
                    .and_then(|&next| (next as char).to_digit(radix))
                    .is_some();
                if digits == 0 || !next_is_digit {
                    return Err(EvalError::Syntax);
                }
                self.offset += 1;
                continue;
            }
            let Some(digit) = (byte as char).to_digit(radix) else {
                break;
            };
            digits += 1;
            self.offset += 1;
            value = value
                .checked_mul(i64::from(radix))
                .and_then(|n| n.checked_add(i64::from(digit)))
                .ok_or(EvalError::Overflow)?;
        }
        if digits == 0 {
            return Err(EvalError::Syntax);
        }
        self.charge()?;
        Ok(value)
    }

    fn variable_name(&mut self) -> Result<&'a [u8], EvalError> {
        if !self.consume(b'$') {
            return Err(EvalError::Syntax);
        }
        let start = self.offset;
        if !self
            .peek()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        {
            return Err(EvalError::Syntax);
        }
        self.offset += 1;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            self.offset += 1;
            if self.offset - start > MAX_VARIABLE_BYTES {
                return Err(EvalError::VariableLimit);
            }
        }
        let name = &self.source[start..self.offset];
        // PHP owns $GLOBALS; never model it as an ordinary local.
        if name == b"GLOBALS" {
            return Err(EvalError::Syntax);
        }
        self.charge()?;
        Ok(name)
    }

    fn assignment(&mut self) -> Result<(), EvalError> {
        self.assignments += 1;
        if self.assignments > MAX_ASSIGNMENTS {
            return Err(EvalError::AssignmentLimit);
        }
        let name = self.variable_name()?;
        if !self.consume(b'=') || self.peek() == Some(b'=') {
            return Err(EvalError::Syntax);
        }
        self.charge()?;
        let value = self.bitwise_or(0)?;
        if !self.consume(b';') {
            return Err(EvalError::Syntax);
        }
        if let Some((_, slot)) = self.bindings.iter_mut().find(|(key, _)| *key == name) {
            *slot = value;
        } else {
            if self.bindings.len() >= MAX_VARIABLES {
                return Err(EvalError::VariableLimit);
            }
            self.bindings.push((name, value));
        }
        Ok(())
    }

    fn primary(&mut self, depth: usize) -> Result<Value, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        if self.consume(b'(') {
            let value = self.bitwise_or(depth + 1)?;
            if !self.consume(b')') {
                return Err(EvalError::Syntax);
            }
            Ok(value)
        } else if self.peek() == Some(b'$') {
            let name = self.variable_name()?;
            self.bindings
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| *value)
                .ok_or(EvalError::UnknownVariable)
        } else {
            self.skip_space();
            for (literal, value) in [(b"true".as_slice(), true), (b"false".as_slice(), false)] {
                let remaining = &self.source[self.offset..];
                if remaining
                    .get(..literal.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(literal))
                    && !remaining
                        .get(literal.len())
                        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    self.offset += literal.len();
                    self.charge()?;
                    return Ok(Value::Bool(value));
                }
            }
            self.number().map(Value::Int)
        }
    }

    fn unary(&mut self, depth: usize) -> Result<Value, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        if self.consume(b'+') {
            self.charge()?;
            Ok(Value::Int(self.unary(depth + 1)?.as_int()))
        } else if self.consume(b'-') {
            self.charge()?;
            self.unary(depth + 1)?
                .as_int()
                .checked_neg()
                .map(Value::Int)
                .ok_or(EvalError::Overflow)
        } else if self.consume(b'!') {
            // PHP logical negation returns a boolean and binds tighter than
            // arithmetic/comparisons, but looser than exponentiation.
            self.charge()?;
            Ok(Value::Bool(!self.unary(depth + 1)?.as_bool()))
        } else if self.consume(b'~') {
            // PHP 8.4 bitwise not is defined on integers, not booleans.
            // Preserve the signed 64-bit representation without overflow.
            self.charge()?;
            match self.unary(depth + 1)? {
                Value::Int(value) => Ok(Value::Int(!value)),
                Value::Bool(_) => Err(EvalError::BitwiseTypeError),
            }
        } else {
            self.power(depth)
        }
    }

    // PHP 8.4 exponentiation binds more tightly than unary +/- and
    // associates to the right: -2**2 == -(2**2), 2**3**2 == 2**(3**2).
    fn power(&mut self, depth: usize) -> Result<Value, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        let base = self.primary(depth)?;
        self.skip_space();
        if !self.source[self.offset..].starts_with(b"**") {
            return Ok(base);
        }
        self.offset += 2;
        self.charge()?;
        let exponent = self.unary(depth + 1)?.as_int();
        // Negative exponents and out-of-range integer powers require PHP
        // floating-point semantics, deliberately unsupported in this slice.
        let exponent = u32::try_from(exponent).map_err(|_| EvalError::UnsupportedFloat)?;
        base.as_int()
            .checked_pow(exponent)
            .map(Value::Int)
            .ok_or(EvalError::UnsupportedFloat)
    }

    fn multiplicative(&mut self, depth: usize) -> Result<Value, EvalError> {
        let mut lhs = self.unary(depth)?;
        loop {
            self.skip_space();
            // A remaining double star is not a multiplication token.
            if self.source[self.offset..].starts_with(b"**") {
                return Err(EvalError::Syntax);
            }
            let op = match self.peek() {
                Some(b'*' | b'/' | b'%') => self.peek().ok_or(EvalError::Syntax)?,
                _ => break,
            };
            self.offset += 1;
            self.charge()?;
            let rhs = self.unary(depth)?.as_int();
            let left = lhs.as_int();
            lhs = Value::Int(match op {
                b'*' => left.checked_mul(rhs).ok_or(EvalError::Overflow)?,
                b'/' => {
                    if rhs == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    let quotient = left.checked_div(rhs).ok_or(EvalError::UnsupportedFloat)?;
                    if left % rhs != 0 {
                        return Err(EvalError::UnsupportedFloat);
                    }
                    quotient
                }
                b'%' => {
                    if rhs == 0 {
                        return Err(EvalError::DivisionByZero);
                    }
                    // PHP defines PHP_INT_MIN % -1 as zero; Rust's
                    // checked_rem rejects the signed overflow case.
                    if rhs == -1 {
                        0
                    } else {
                        left.checked_rem(rhs).ok_or(EvalError::Overflow)?
                    }
                }
                _ => return Err(EvalError::Syntax),
            });
        }
        Ok(lhs)
    }

    fn additive(&mut self, depth: usize) -> Result<Value, EvalError> {
        let mut lhs = self.multiplicative(depth)?;
        loop {
            self.skip_space();
            let op = match self.peek() {
                Some(b'+' | b'-') => self.peek().unwrap(),
                _ => break,
            };
            self.offset += 1;
            self.charge()?;
            let rhs = self.multiplicative(depth)?.as_int();
            lhs = Value::Int(
                if op == b'+' {
                    lhs.as_int().checked_add(rhs)
                } else {
                    lhs.as_int().checked_sub(rhs)
                }
                .ok_or(EvalError::Overflow)?,
            );
        }
        Ok(lhs)
    }

    // PHP 8.4 compares a boolean operand using truthiness for loose
    // equality and ordering. Strict equality preserves the value type.
    fn comparison(&mut self, depth: usize) -> Result<Value, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        let lhs = self.additive(depth)?;
        self.skip_space();
        let remaining = &self.source[self.offset..];
        let operators: &[(&[u8], Comparison)] = &[
            (b"<=>", Comparison::Spaceship),
            (b"===", Comparison::StrictEqual),
            (b"!==", Comparison::StrictNotEqual),
            (b"<=", Comparison::LessEqual),
            (b">=", Comparison::GreaterEqual),
            (b"==", Comparison::Equal),
            (b"!=", Comparison::NotEqual),
            (b"<>", Comparison::NotEqual),
            (b"<", Comparison::Less),
            (b">", Comparison::Greater),
        ];
        let Some((bytes, operator)) = operators
            .iter()
            .find(|(bytes, _)| remaining.starts_with(bytes))
        else {
            return Ok(lhs);
        };
        self.offset += bytes.len();
        self.charge()?;
        let rhs = self.additive(depth)?;
        let (left, right) = lhs.php_order(rhs);
        let result = match operator {
            Comparison::Spaceship => Value::Int(if left < right {
                -1
            } else if left > right {
                1
            } else {
                0
            }),
            Comparison::Equal => Value::Bool(lhs.php_equal(rhs)),
            Comparison::NotEqual => Value::Bool(!lhs.php_equal(rhs)),
            Comparison::StrictEqual => Value::Bool(lhs == rhs),
            Comparison::StrictNotEqual => Value::Bool(lhs != rhs),
            Comparison::Less => Value::Bool(left < right),
            Comparison::LessEqual => Value::Bool(left <= right),
            Comparison::Greater => Value::Bool(left > right),
            Comparison::GreaterEqual => Value::Bool(left >= right),
        };
        Ok(result)
    }

    // PHP 8.4 precedence: comparisons bind tighter than bitwise AND,
    // which binds tighter than XOR, which binds tighter than OR.
    // Binary bitwise operators convert booleans to integers (unlike ~).
    fn bitwise_and(&mut self, depth: usize) -> Result<Value, EvalError> {
        let mut lhs = self.comparison(depth)?;
        loop {
            self.skip_space();
            if self.source[self.offset..].starts_with(b"&&") {
                return Err(EvalError::Syntax);
            }
            if !self.consume(b'&') {
                break;
            }
            self.charge()?;
            let rhs = self.comparison(depth)?;
            lhs = Value::Int(lhs.as_int() & rhs.as_int());
        }
        Ok(lhs)
    }

    fn bitwise_xor(&mut self, depth: usize) -> Result<Value, EvalError> {
        let mut lhs = self.bitwise_and(depth)?;
        while self.consume(b'^') {
            self.charge()?;
            let rhs = self.bitwise_and(depth)?;
            lhs = Value::Int(lhs.as_int() ^ rhs.as_int());
        }
        Ok(lhs)
    }

    fn bitwise_or(&mut self, depth: usize) -> Result<Value, EvalError> {
        let mut lhs = self.bitwise_xor(depth)?;
        loop {
            self.skip_space();
            if self.source[self.offset..].starts_with(b"||") {
                return Err(EvalError::Syntax);
            }
            if !self.consume(b'|') {
                break;
            }
            self.charge()?;
            let rhs = self.bitwise_xor(depth)?;
            lhs = Value::Int(lhs.as_int() | rhs.as_int());
        }
        Ok(lhs)
    }

    fn evaluate(&mut self) -> Result<Value, EvalError> {
        if self.source.len() > MAX_SOURCE {
            return Err(EvalError::SourceTooLong);
        }
        // PHP lexes adjacent ++ and -- as increment/decrement tokens,
        // not as two independent arithmetic signs. This slice has no
        // variables or increment/decrement semantics, so reject both.
        if self
            .source
            .windows(2)
            .any(|pair| pair == b"++" || pair == b"--")
        {
            return Err(EvalError::Syntax);
        }
        self.skip_space();
        if self.source[self.offset..].starts_with(b"<?php") {
            self.offset += 5;
            if self.peek().is_some_and(|b| !b.is_ascii_whitespace()) {
                return Err(EvalError::Syntax);
            }
            self.skip_space();
        }
        while self.peek() == Some(b'$') {
            self.assignment()?;
            self.skip_space();
        }
        if !self.source[self.offset..].starts_with(b"echo") {
            return Err(EvalError::Syntax);
        }
        self.offset += 4;
        if self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(EvalError::Syntax);
        }
        let value = self.bitwise_or(0)?;
        if !self.consume(b';') {
            return Err(EvalError::Syntax);
        }
        self.skip_space();
        if self.source[self.offset..].starts_with(b"?>") {
            self.offset += 2;
            self.skip_space();
        }
        if self.offset != self.source.len() {
            return Err(EvalError::TrailingInput);
        }
        Ok(value)
    }
}

fn run() -> Result<(), String> {
    let mut source = Vec::new();
    io::stdin()
        .take((MAX_SOURCE + 1) as u64)
        .read_to_end(&mut source)
        .map_err(|error| error.to_string())?;
    let value = Parser::new(&source)
        .evaluate()
        .map_err(|error| format!("unsupported PHP expression: {error:?}"))?;
    // PHP echo emits "1" for true and zero bytes for false.
    // No output is published before the entire program has been accepted.
    let output = value.echo();
    io::stdout()
        .lock()
        .write_all(output.as_bytes())
        .map_err(|error| error.to_string())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str) -> Result<i64, EvalError> {
        Parser::new(source.as_bytes()).evaluate().map(Value::as_int)
    }

    #[test]
    fn precedence_parentheses_and_php_tags() {
        assert_eq!(eval("<?php echo 1 + 2 * 3;"), Ok(7));
        assert_eq!(eval("echo (1 + 2) * 3;"), Ok(9));
        assert_eq!(eval("echo 8 - 3 - 2;"), Ok(3));
        assert_eq!(eval("echo 2 * -3 + +4;"), Ok(-2));
        assert_eq!(eval("echo -(-5);"), Ok(5));
        assert_eq!(eval(" \n<?php\n echo 0; ?>\n"), Ok(0));
        assert_eq!(eval("echo 9223372036854775807;"), Ok(i64::MAX));
    }

    #[test]
    fn php84_logical_not_truthiness_and_precedence() {
        for (program, expected) in [
            ("echo !true;", Value::Bool(false)),
            ("echo !false;", Value::Bool(true)),
            ("echo !0;", Value::Bool(true)),
            ("echo !1;", Value::Bool(false)),
            ("echo !-1;", Value::Bool(false)),
            ("echo !!true;", Value::Bool(true)),
            ("echo !!false;", Value::Bool(false)),
            ("echo !(1<2);", Value::Bool(false)),
            ("echo !2**0;", Value::Bool(false)),
            ("echo !2**2;", Value::Bool(false)),
            ("echo !false===true;", Value::Bool(true)),
            ("echo !false+2;", Value::Int(3)),
            ("echo !1<2;", Value::Bool(true)),
            ("echo !true==false;", Value::Bool(true)),
            ("echo !true!==false;", Value::Bool(false)),
            ("$flag=false; echo !$flag;", Value::Bool(true)),
            ("$x=0; $x=!$x; echo $x;", Value::Bool(true)),
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Ok(expected),
                "{program}"
            );
        }
        for program in ["echo !;", "echo !();", "echo !!!;", "echo !$missing;"] {
            assert!(
                Parser::new(program.as_bytes()).evaluate().is_err(),
                "{program}"
            );
        }
    }

    #[test]
    fn php84_integer_bitwise_not_precedence_and_types() {
        for (program, expected) in [
            ("echo ~0;", Value::Int(-1)),
            ("echo ~1;", Value::Int(-2)),
            ("echo ~-1;", Value::Int(0)),
            ("echo ~~0;", Value::Int(0)),
            ("echo ~0x7fff_ffff_ffff_ffff;", Value::Int(i64::MIN)),
            ("echo ~(0-9223372036854775807-1);", Value::Int(i64::MAX)),
            ("echo ~2**3;", Value::Int(-9)),
            ("echo ~2**3+1;", Value::Int(-8)),
            ("echo ~(2**3+1);", Value::Int(-10)),
            ("echo -~2**3;", Value::Int(9)),
            ("echo ~-2;", Value::Int(1)),
            ("echo !~0;", Value::Bool(false)),
            ("echo ~1<=>-2;", Value::Int(0)),
            ("echo ~1===-2;", Value::Bool(true)),
            ("echo ~+3;", Value::Int(-4)),
            ("echo +~3;", Value::Int(-4)),
            ("echo ~-~0;", Value::Int(-2)),
            ("$x=5; echo ~$x;", Value::Int(-6)),
            ("$x=5; echo ~($x+1);", Value::Int(-7)),
            ("$x=1; $x=~$x; echo $x;", Value::Int(-2)),
            ("echo ~(1<=>2);", Value::Int(0)),
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Ok(expected),
                "{program}"
            );
        }
        for program in [
            "echo ~true;",
            "echo ~false;",
            "echo ~(1<2);",
            "echo ~!0;",
            "$x=true; echo ~$x;",
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Err(EvalError::BitwiseTypeError),
                "{program}"
            );
        }
        for program in ["echo ~;", "echo ~();", "echo ~$missing;"] {
            assert!(
                Parser::new(program.as_bytes()).evaluate().is_err(),
                "{program}"
            );
        }
    }

    #[test]
    fn php84_binary_bitwise_precedence_coercion_and_boundaries() {
        for (program, expected) in [
            ("echo 6&3;", Value::Int(2)),
            ("echo 6^3;", Value::Int(5)),
            ("echo 6|3;", Value::Int(7)),
            ("echo 1&2|4^3;", Value::Int(7)),
            ("echo 1|2^3&4;", Value::Int(3)),
            ("echo 7^2|8&3;", Value::Int(5)),
            ("echo 1^2^3;", Value::Int(0)),
            ("echo (1&3)===1;", Value::Bool(true)),
            ("echo 1&3===1;", Value::Int(0)),
            ("echo 1&3==1;", Value::Int(0)),
            ("echo 1^3==1;", Value::Int(1)),
            ("echo 1|3==1;", Value::Int(1)),
            ("echo 1|2<3;", Value::Int(1)),
            ("echo true&false;", Value::Int(0)),
            ("echo true^false;", Value::Int(1)),
            ("echo true|false;", Value::Int(1)),
            ("echo 1&true;", Value::Int(1)),
            ("echo 1^true;", Value::Int(0)),
            ("echo 1|true;", Value::Int(1)),
            ("echo -1&0x7fff_ffff_ffff_ffff;", Value::Int(i64::MAX)),
            ("echo ~0^0x7fff_ffff_ffff_ffff;", Value::Int(i64::MIN)),
            ("$x=6; $x=$x&3; echo $x;", Value::Int(2)),
            ("$x=6; $x=$x^3; echo $x;", Value::Int(5)),
            ("$x=6; $x=$x|3; echo $x;", Value::Int(7)),
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Ok(expected),
                "{program}"
            );
        }
        for program in [
            "echo 1&;",
            "echo 1^;",
            "echo 1|;",
            "echo &1;",
            "echo ^1;",
            "echo |1;",
            "echo 1&&2;",
            "echo 1||2;",
            "echo 1&$missing;",
            "echo 1^$missing;",
        ] {
            assert!(
                Parser::new(program.as_bytes()).evaluate().is_err(),
                "{program}"
            );
        }
    }

    #[test]
    fn php84_boolean_values_and_coercion() {
        for (program, expected) in [
            ("echo true;", Value::Bool(true)),
            ("echo false;", Value::Bool(false)),
            ("echo TRUE;", Value::Bool(true)),
            ("echo FALSE;", Value::Bool(false)),
            ("echo 1<2;", Value::Bool(true)),
            ("echo 1>2;", Value::Bool(false)),
            ("$x=1<2; echo $x;", Value::Bool(true)),
            ("$x=2<1; echo $x;", Value::Bool(false)),
            ("$x=true; $y=false; echo $x==1;", Value::Bool(true)),
            ("echo true===1;", Value::Bool(false)),
            ("echo true==1;", Value::Bool(true)),
            ("echo false===0;", Value::Bool(false)),
            ("echo false==0;", Value::Bool(true)),
            ("echo true!==1;", Value::Bool(true)),
            ("echo true>2;", Value::Bool(false)),
            ("echo false<-1;", Value::Bool(true)),
            ("echo (1<2)===true;", Value::Bool(true)),
            ("echo (1<2)+1;", Value::Int(2)),
            ("echo (1<2)*3;", Value::Int(3)),
            ("echo (2>3)+1;", Value::Int(1)),
            ("echo (1<2)**3;", Value::Int(1)),
            ("echo +true;", Value::Int(1)),
            ("echo -true;", Value::Int(-1)),
            ("$x=2<1; $x=$x+5; echo $x;", Value::Int(5)),
            ("$x=1<2; $x=3; echo $x;", Value::Int(3)),
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Ok(expected),
                "{program}"
            );
        }
        assert_eq!(Value::Bool(true).echo(), "1");
        assert_eq!(Value::Bool(false).echo(), "");
        assert_eq!(Value::Int(0).echo(), "0");
    }

    #[test]
    fn boolean_boundary_rejections() {
        for program in [
            "echo truex;",
            "echo FALSE_1;",
            "echo true&&false;",
            "echo true<false<true;",
            "echo (1<2;",
        ] {
            assert!(eval(program).is_err(), "{program}");
        }
    }

    #[test]
    fn php84_checked_powers_and_precedence() {
        for (program, expected) in [
            ("echo 2**3;", 8),
            ("echo 2**3**2;", 512),
            ("echo (2**3)**2;", 64),
            ("echo -2**2;", -4),
            ("echo (-2)**2;", 4),
            ("echo (-2)**3;", -8),
            ("echo 2**-(-2);", 4),
            ("echo 0**0;", 1),
            ("echo 0**7;", 0),
            ("echo 2**0;", 1),
            ("echo 2**62;", 4611686018427387904),
            ("echo (-2)**63;", i64::MIN),
            ("echo -1**63;", -1),
            ("echo 2**2*3;", 12),
            ("echo 2*3**2;", 18),
            ("echo 2**(1+2);", 8),
            ("echo 1+2**3==9;", 1),
            ("$x=3; echo $x**3;", 27),
            ("echo (0-9223372036854775807-1)**1;", i64::MIN),
        ] {
            assert_eq!(eval(program), Ok(expected), "{program}");
        }
    }

    #[test]
    fn unsupported_powers_fail_closed() {
        for (program, expected) in [
            ("echo 2**-1;", EvalError::UnsupportedFloat),
            ("echo 0**-1;", EvalError::UnsupportedFloat),
            ("echo 2**63;", EvalError::UnsupportedFloat),
            ("echo (-2)**64;", EvalError::UnsupportedFloat),
            ("echo 1**4294967296;", EvalError::UnsupportedFloat),
            ("echo 2**;", EvalError::Syntax),
            ("echo 2***3;", EvalError::Syntax),
            ("echo 2**3**;", EvalError::Syntax),
        ] {
            assert_eq!(eval(program), Err(expected), "{program}");
        }
    }

    #[test]
    fn php84_spaceship_returns_integer_with_boolean_coercion() {
        for (program, expected) in [
            ("echo 1<=>2;", Value::Int(-1)),
            ("echo 2<=>2;", Value::Int(0)),
            ("echo 3<=>2;", Value::Int(1)),
            ("echo true<=>2;", Value::Int(0)),
            ("echo false<=>-1;", Value::Int(-1)),
            ("echo false<=>0;", Value::Int(0)),
            ("echo true<=>false;", Value::Int(1)),
            ("echo 1+2<=>2*2;", Value::Int(-1)),
            ("echo (1<=>2)===-1;", Value::Bool(true)),
            ("echo (2<=>1)+2;", Value::Int(3)),
            ("echo !0<=>true;", Value::Int(0)),
            ("echo (1<=>2)<0;", Value::Bool(true)),
            ("echo (0-9223372036854775807-1)<=>0;", Value::Int(-1)),
            (
                "echo 9223372036854775807<=>(0-9223372036854775807-1);",
                Value::Int(1),
            ),
            ("$x=4; $y=5; echo $x<=>$y;", Value::Int(-1)),
            ("$x=1<=>2; echo $x+2;", Value::Int(1)),
        ] {
            assert_eq!(
                Parser::new(program.as_bytes()).evaluate(),
                Ok(expected),
                "{program}"
            );
        }
        for program in ["echo 1<=>;", "echo 1<=>2<=>3;", "echo 1<==>2;"] {
            assert!(
                Parser::new(program.as_bytes()).evaluate().is_err(),
                "{program}"
            );
        }
    }

    #[test]
    fn php84_integer_comparison_precedence_and_boundaries() {
        for (program, expected) in [
            ("echo 1<2;", 1),
            ("echo 1>2;", 0),
            ("echo 2<=2;", 1),
            ("echo 2>=3;", 0),
            ("echo 1==1;", 1),
            ("echo 1===1;", 1),
            ("echo 1!==1;", 0),
            ("echo 1!=2;", 1),
            ("echo 1<>2;", 1),
            ("echo 1+2*3==7;", 1),
            ("echo (2+3)*4>=19;", 1),
            ("echo 0x7fff_ffff_ffff_ffff>0;", 1),
            ("echo (0-9223372036854775807-1)<0;", 1),
            ("$x=4; $y=5; echo $x+1==($y*1);", 1),
            ("$x=4; $y=5; echo $x>$y;", 0),
        ] {
            assert_eq!(eval(program), Ok(expected), "{program}");
        }
    }

    #[test]
    fn unsupported_comparison_forms_fail_closed() {
        assert_eq!(eval("echo 1<;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1!==;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1<2<3;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo (1<2);"), Ok(1));
        assert_eq!(eval("$x=1<2; echo $x;"), Ok(1));
    }

    #[test]
    fn php84_exact_division_and_signed_remainder() {
        assert_eq!(eval("echo 8/4;"), Ok(2));
        assert_eq!(eval("echo 0/2;"), Ok(0));
        assert_eq!(eval("echo 7%3;"), Ok(1));
        assert_eq!(eval("echo -7%3;"), Ok(-1));
        assert_eq!(eval("echo 7%-3;"), Ok(1));
        assert_eq!(eval("echo -7%-3;"), Ok(-1));
        assert_eq!(eval("echo 8/2*3%5;"), Ok(2));
        assert_eq!(eval("echo 3+8/4*5;"), Ok(13));
        assert_eq!(eval("$x=20; $y=3; echo $x%$y;"), Ok(2));
        assert_eq!(eval("echo (0-9223372036854775807-1)%-1;"), Ok(0));
        assert_eq!(eval("echo (0-9223372036854775807-1)/2;"), Ok(i64::MIN / 2));
    }

    #[test]
    fn reject_fractional_and_zero_division_without_output() {
        assert_eq!(eval("echo 1/2;"), Err(EvalError::UnsupportedFloat));
        assert_eq!(eval("echo -3/2;"), Err(EvalError::UnsupportedFloat));
        assert_eq!(eval("echo 1/0;"), Err(EvalError::DivisionByZero));
        assert_eq!(eval("echo 1%0;"), Err(EvalError::DivisionByZero));
        assert_eq!(
            eval("echo (0-9223372036854775807-1)/-1;"),
            Err(EvalError::UnsupportedFloat)
        );
        assert_eq!(eval("echo 2/0+1;"), Err(EvalError::DivisionByZero));
    }

    #[test]
    fn php84_integer_radices_and_separators() {
        for (literal, expected) in [
            ("0", 0),
            ("00", 0),
            ("01", 1),
            ("010", 8),
            ("0_7", 7),
            ("00_7", 7),
            ("0o10", 8),
            ("0O10", 8),
            ("0x2a", 42),
            ("0X2A", 42),
            ("0b1010", 10),
            ("0B1010", 10),
            ("1_000", 1000),
            ("0xF_F", 255),
            ("0b1_0", 2),
            ("0o7_7", 63),
            ("0x7fff_ffff_ffff_ffff", i64::MAX),
            ("0o777777777777777777777", i64::MAX),
        ] {
            assert_eq!(eval(&format!("echo {literal};")), Ok(expected), "{literal}");
        }
        assert_eq!(eval("$x=0x2a; $y=0b101; echo $x+$y;"), Ok(47));
        assert_eq!(eval("echo (0o10+010)*2;"), Ok(32));
    }

    #[test]
    fn invalid_integer_literals_fail_without_output() {
        for literal in [
            "08",
            "09",
            "0_8",
            "00_8",
            "0o8",
            "0b102",
            "0o78",
            "0x",
            "0b",
            "0o",
            "0x_1",
            "0b_1",
            "0o_1",
            "1_",
            "1__0",
            "0x1_",
            "0b1_",
            "0o7_",
            "0x1g",
            "0x8000000000000000",
            "0xffffffffffffffff",
            "9223372036854775808",
        ] {
            assert!(eval(&format!("echo {literal};")).is_err(), "{literal}");
        }
    }

    #[test]
    fn variable_binding_order_case_and_reassignment() {
        assert_eq!(eval("<?php $x=7; echo $x;"), Ok(7));
        assert_eq!(eval("<?php $x=7; $x=$x+2; echo $x*3;"), Ok(27));
        assert_eq!(eval("<?php $foo_1=3; $_bar=4; echo $foo_1*$_bar;"), Ok(12));
        assert_eq!(eval("<?php $A=5; $a=2; echo $A-$a;"), Ok(3));
        assert_eq!(eval("<?php $zero=0; echo -$zero;"), Ok(0));
        assert_eq!(eval("<?php $x=1; $x=2; $x=3; echo $x;"), Ok(3));
        assert_eq!(eval("<?php $x=5; $y=$x*2; echo $y-$x;"), Ok(5));
    }

    #[test]
    fn invalid_variables_and_bounded_bindings_fail_closed() {
        assert_eq!(eval("echo $missing;"), Err(EvalError::UnknownVariable));
        assert_eq!(eval("$x=$x+1; echo $x;"), Err(EvalError::UnknownVariable));
        assert_eq!(eval("$x==1; echo $x;"), Err(EvalError::Syntax));
        assert_eq!(eval("$GLOBALS=1; echo 1;"), Err(EvalError::Syntax));
        assert_eq!(eval("$x=1; echo $x; $x=2;"), Err(EvalError::TrailingInput));
        let long_name = format!("$a{}=1; echo 1;", "a".repeat(64));
        assert_eq!(eval(&long_name), Err(EvalError::VariableLimit));
        let too_many = format!(
            "{}echo 1;",
            (0..33).map(|i| format!("$v{i}={i};")).collect::<String>()
        );
        assert_eq!(eval(&too_many), Err(EvalError::VariableLimit));
        let writes = format!("{}echo $x;", "$x=1;".repeat(65));
        assert_eq!(eval(&writes), Err(EvalError::AssignmentLimit));
    }

    #[test]
    fn reject_unsupported_syntax_without_partial_result() {
        assert_eq!(eval("echo 08;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1 / 2;"), Err(EvalError::UnsupportedFloat));
        assert_eq!(eval("echo $x;"), Err(EvalError::UnknownVariable));
        assert_eq!(eval("echo 1+;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1--2;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1++2;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo 1; echo 2;"), Err(EvalError::TrailingInput));
        assert_eq!(
            eval("echo 9223372036854775807+1;"),
            Err(EvalError::Overflow)
        );
        assert_eq!(eval("echo 9223372036854775808;"), Err(EvalError::Overflow));
        assert_eq!(eval("<?phpecho 1;"), Err(EvalError::Syntax));
    }

    #[test]
    fn bounded_depth_work_and_source() {
        let deep = format!("echo {}1{};", "(".repeat(33), ")".repeat(33));
        assert_eq!(eval(&deep), Err(EvalError::DepthLimit));
        let work = format!("echo {};", vec!["1"; 130].join("+"));
        assert_eq!(eval(&work), Err(EvalError::WorkLimit));
        let large = format!("echo 1;{}", " ".repeat(MAX_SOURCE));
        assert_eq!(eval(&large), Err(EvalError::SourceTooLong));
    }
}
