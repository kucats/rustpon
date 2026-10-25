//! Research-only, bounded PHP integer-expression evaluator.
//!
//! This is the first standalone language execution slice, NOT a PHP VM.
//! Supported grammar: bounded local integer assignments followed by one echo;
//! PHP 8.4 decimal/octal/binary/hex i64 literals (with digit separators),
//! variables, unary +/-, parentheses and +, -, *.
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
    DepthLimit,
    WorkLimit,
    UnknownVariable,
    VariableLimit,
    AssignmentLimit,
}

struct Parser<'a> {
    source: &'a [u8],
    offset: usize,
    work: usize,
    bindings: Vec<(&'a [u8], i64)>,
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
        let value = self.additive(0)?;
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

    fn primary(&mut self, depth: usize) -> Result<i64, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        if self.consume(b'(') {
            let value = self.additive(depth + 1)?;
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
            self.number()
        }
    }

    fn unary(&mut self, depth: usize) -> Result<i64, EvalError> {
        if depth > MAX_DEPTH {
            return Err(EvalError::DepthLimit);
        }
        if self.consume(b'+') {
            self.charge()?;
            self.unary(depth + 1)
        } else if self.consume(b'-') {
            self.charge()?;
            self.unary(depth + 1)?
                .checked_neg()
                .ok_or(EvalError::Overflow)
        } else {
            self.primary(depth)
        }
    }

    fn multiplicative(&mut self, depth: usize) -> Result<i64, EvalError> {
        let mut lhs = self.unary(depth)?;
        while self.consume(b'*') {
            self.charge()?;
            let rhs = self.unary(depth)?;
            lhs = lhs.checked_mul(rhs).ok_or(EvalError::Overflow)?;
        }
        Ok(lhs)
    }

    fn additive(&mut self, depth: usize) -> Result<i64, EvalError> {
        let mut lhs = self.multiplicative(depth)?;
        loop {
            self.skip_space();
            let op = match self.peek() {
                Some(b'+' | b'-') => self.peek().unwrap(),
                _ => break,
            };
            self.offset += 1;
            self.charge()?;
            let rhs = self.multiplicative(depth)?;
            lhs = if op == b'+' {
                lhs.checked_add(rhs)
            } else {
                lhs.checked_sub(rhs)
            }
            .ok_or(EvalError::Overflow)?;
        }
        Ok(lhs)
    }

    fn evaluate(&mut self) -> Result<i64, EvalError> {
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
        let value = self.additive(0)?;
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
    // No output is published before the entire program has been accepted.
    io::stdout()
        .lock()
        .write_all(value.to_string().as_bytes())
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
        Parser::new(source.as_bytes()).evaluate()
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
            "08", "09", "0_8", "00_8", "0o8", "0b102", "0o78",
            "0x", "0b", "0o", "0x_1", "0b_1", "0o_1",
            "1_", "1__0", "0x1_", "0b1_", "0o7_",
            "0x1g", "0x8000000000000000", "0xffffffffffffffff",
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
        assert_eq!(eval("echo 1 / 2;"), Err(EvalError::Syntax));
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
