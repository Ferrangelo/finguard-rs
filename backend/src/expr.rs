//! Safe arithmetic expression evaluator.
//!
//! Ports the Python `_safe_eval_expr` helper (an AST-based evaluator restricted
//! to `+ - * /`, unary `+`/`-`, and parentheses) used for amount/value input
//! fields throughout the UI. Unlike the Python version, which leans on the
//! standard library's `ast` module, this is a small hand-rolled
//! tokenizer + recursive-descent parser/evaluator with no extra dependencies.
//!
//! Any malformed input yields [`Error::InvalidArgument`] rather than a panic.

use crate::{Error, Result};

// Grammar (standard precedence, left-associative binary operators):
//
//   expr   := term  (('+' | '-') term)*
//   term   := unary (('*' | '/') unary)*
//   unary  := ('+' | '-') unary | atom
//   atom   := NUMBER | '(' expr ')'

/// A single lexical token produced by the tokenizer.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
}

/// Convert the raw input into a flat list of tokens.
///
/// Whitespace (anywhere) is skipped. Number literals are parsed greedily over
/// the run of digits and decimal points; any other byte that is not a known
/// operator or parenthesis is rejected.
fn tokenize(expr: &str) -> Result<Vec<Token>> {
    let bytes = expr.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'+' => {
                tokens.push(Token::Plus);
                i += 1;
            }
            b'-' => {
                tokens.push(Token::Minus);
                i += 1;
            }
            b'*' => {
                tokens.push(Token::Star);
                i += 1;
            }
            b'/' => {
                tokens.push(Token::Slash);
                i += 1;
            }
            b'(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            b')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            b'0'..=b'9' | b'.' => {
                // Greedily consume the digit/decimal-point run, then parse it
                // with the standard float parser so we accept exactly what a
                // normal float parse accepts (e.g. `10`, `3.14`, `.5`, `42.`).
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                let literal = &expr[start..i];
                let value: f64 = literal
                    .parse()
                    .map_err(|_| Error::InvalidArgument(format!("invalid number: {literal}")))?;
                tokens.push(Token::Number(value));
            }
            other => {
                return Err(Error::InvalidArgument(format!(
                    "unexpected character: {}",
                    other as char
                )));
            }
        }
    }

    Ok(tokens)
}

/// Recursive-descent parser/evaluator over the token stream.
///
/// Evaluation happens during the parse: there is no separate AST, mirroring the
/// fact that the only thing I need from these expressions is a single `f64`.
struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    /// Peek at the current token without consuming it.
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    /// Consume and return the current token.
    fn next(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    /// `expr := term (('+' | '-') term)*`
    fn parse_expr(&mut self) -> Result<f64> {
        let mut acc = self.parse_term()?;
        while let Some(op) = self.peek() {
            match op {
                Token::Plus => {
                    self.pos += 1;
                    acc += self.parse_term()?;
                }
                Token::Minus => {
                    self.pos += 1;
                    acc -= self.parse_term()?;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    /// `term := unary (('*' | '/') unary)*`
    fn parse_term(&mut self) -> Result<f64> {
        let mut acc = self.parse_unary()?;
        while let Some(op) = self.peek() {
            match op {
                Token::Star => {
                    self.pos += 1;
                    acc *= self.parse_unary()?;
                }
                Token::Slash => {
                    self.pos += 1;
                    let rhs = self.parse_unary()?;
                    // Python's truediv raises ZeroDivisionError; refuse to
                    // produce inf/NaN and report the error instead.
                    if rhs == 0.0 {
                        return Err(Error::InvalidArgument("division by zero".to_string()));
                    }
                    acc /= rhs;
                }
                _ => break,
            }
        }
        Ok(acc)
    }

    /// `unary := ('+' | '-') unary | atom`
    fn parse_unary(&mut self) -> Result<f64> {
        match self.peek() {
            Some(Token::Plus) => {
                self.pos += 1;
                self.parse_unary()
            }
            Some(Token::Minus) => {
                self.pos += 1;
                Ok(-self.parse_unary()?)
            }
            _ => self.parse_atom(),
        }
    }

    /// `atom := NUMBER | '(' expr ')'`
    fn parse_atom(&mut self) -> Result<f64> {
        match self.next() {
            Some(Token::Number(n)) => Ok(n),
            Some(Token::LParen) => {
                let value = self.parse_expr()?;
                match self.next() {
                    Some(Token::RParen) => Ok(value),
                    _ => Err(Error::InvalidArgument("expected ')'".to_string())),
                }
            }
            Some(_) => Err(Error::InvalidArgument(
                "expected a number or '('".to_string(),
            )),
            None => Err(Error::InvalidArgument(
                "unexpected end of expression".to_string(),
            )),
        }
    }
}

/// Safely evaluate a simple arithmetic expression and return the result.
///
/// Supports decimal number literals, the binary operators `+ - * /` (with
/// `*`/`/` binding tighter than `+`/`-`, left-associative), unary `+`/`-`, and
/// arbitrarily nested parentheses. Whitespace anywhere is ignored. Everything
/// is computed as `f64`, matching the Python original.
///
/// Returns [`Error::InvalidArgument`] for any invalid input — empty or
/// whitespace-only strings, unknown characters, identifiers/function calls,
/// trailing garbage, mismatched parentheses, malformed numbers, and division
/// by zero. This function never panics.
///
/// # Examples
///
/// ```
/// use finguard_rs_backend::expr::eval;
///
/// assert_eq!(eval("2 + 3 * 4").unwrap(), 14.0);
/// assert_eq!(eval("(10 + 5) / 3").unwrap(), 5.0);
/// assert!(eval("sqrt(2)").is_err());
/// ```
pub fn eval(expr: &str) -> Result<f64> {
    let tokens = tokenize(expr)?;
    if tokens.is_empty() {
        return Err(Error::InvalidArgument("empty expression".to_string()));
    }

    let mut parser = Parser::new(tokens);
    let value = parser.parse_expr()?;

    // Reject trailing garbage such as `1 2` or `2 +* 3` (the leftover token is
    // never consumed by a complete parse).
    if parser.peek().is_some() {
        return Err(Error::InvalidArgument(
            "unexpected trailing input".to_string(),
        ));
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)] // 3.14 is an intentional literal, not PI.
    fn plain_numbers() {
        assert_eq!(eval("10").unwrap(), 10.0);
        assert_eq!(eval("3.14").unwrap(), 3.14);
        assert_eq!(eval(".5").unwrap(), 0.5);
    }

    #[test]
    fn operators_and_precedence() {
        assert_eq!(eval("10 + 5").unwrap(), 15.0);
        assert_eq!(eval("10 * 2 - 3").unwrap(), 17.0);
        assert_eq!(eval("2 + 3 * 4").unwrap(), 14.0);
        assert_eq!(eval("(10 + 5) / 3").unwrap(), 5.0);
    }

    #[test]
    fn unary() {
        assert_eq!(eval("-5 + 3").unwrap(), -2.0);
        assert_eq!(eval("-(2+1)").unwrap(), -3.0);
        assert_eq!(eval("+4").unwrap(), 4.0);
    }

    #[test]
    fn whitespace_tolerance() {
        assert_eq!(eval("  1  +  2  ").unwrap(), 3.0);
    }

    #[test]
    fn float_division() {
        assert_eq!(eval("7/2").unwrap(), 3.5);
    }

    #[test]
    fn errors() {
        assert!(eval("").is_err());
        assert!(eval("   ").is_err());
        assert!(eval("abc").is_err());
        assert!(eval("1 +").is_err());
        assert!(eval("(1+2").is_err());
        assert!(eval("1 2").is_err());
        assert!(eval("2 +* 3").is_err());
        assert!(eval("1/0").is_err());
        assert!(eval("sqrt(2)").is_err());
    }
}
