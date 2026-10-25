//! Research-only, bounded PHP integer-expression evaluator.
//!
//! This is the first standalone language execution slice, NOT a PHP VM.
//! Supported grammar: "<?php echo <expr>;" with decimal i64 literals,
//! unary +/-, parentheses, and binary +, -, * at PHP precedence.
//! Unsupported PHP constructs and arithmetic overflow fail closed.

use std::io::{self, Read, Write};

const MAX_SOURCE: usize = 4096;
const MAX_DEPTH: usize = 32;
const MAX_WORK: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvalError {
    SourceTooLong,
    Syntax,
    TrailingInput,
    UnsupportedNumber,
    Overflow,
    DepthLimit,
    WorkLimit,
}

struct Parser<'a> {
    source: &'a [u8],
    offset: usize,
    work: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            offset: 0,
            work: 0,
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
        let mut value = 0_i64;
        while let Some(digit @ b'0'..=b'9') = self.peek() {
            self.offset += 1;
            value = value
                .checked_mul(10)
                .and_then(|n| n.checked_add(i64::from(digit - b'0')))
                .ok_or(EvalError::Overflow)?;
        }
        if start == self.offset {
            return Err(EvalError::Syntax);
        }
        // PHP treats a leading-zero integer as octal; do not silently
        // interpret it as decimal in this deliberately narrow subset.
        if self.offset - start > 1 && self.source[start] == b'0' {
            return Err(EvalError::UnsupportedNumber);
        }
        self.charge()?;
        Ok(value)
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
    fn reject_unsupported_syntax_without_partial_result() {
        assert_eq!(eval("echo 01;"), Err(EvalError::UnsupportedNumber));
        assert_eq!(eval("echo 1 / 2;"), Err(EvalError::Syntax));
        assert_eq!(eval("echo $x;"), Err(EvalError::Syntax));
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
