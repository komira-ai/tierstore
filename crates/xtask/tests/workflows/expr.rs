//! The GitHub Actions expressions the conditions of the workflows are written in, evaluated, so a
//! test can run a condition of the file against each outcome of the jobs it names.
//!
//! The subset: `${{ … }}` around the expression; `||`, `&&`, `!`, `==` and `!=` with GitHub's
//! precedence (`!` binds tightest, then the comparisons, then `&&`, then `||`); parentheses;
//! string literals in single quotes; the functions `always()`, `cancelled()` and `failure()`; and dotted
//! names (`needs.version.result`) looked up in a table, a name not in it being empty, as an unset
//! output is. Strings compare as GitHub compares them, without regard to case. Anything else is
//! an error naming what was not understood.

use std::collections::BTreeMap;

/// What an expression is evaluated against.
pub struct Context {
    /// The values of the dotted names.
    pub names: BTreeMap<String, String>,
    /// Whether the run was cancelled (`cancelled()`).
    pub cancelled: bool,
    /// Whether a job this one waits for failed (`failure()`).
    pub failed: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Or,
    And,
    Not,
    Eq,
    Ne,
    Open,
    Close,
    Text(String),
    Name(String),
}

#[derive(Debug, PartialEq)]
enum Value {
    Bool(bool),
    Text(String),
}

impl Value {
    fn truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Text(t) => !t.is_empty(),
        }
    }
}

/// The operators, longest first.
const OPERATORS: [(&str, Token); 7] = [
    ("||", Token::Or),
    ("&&", Token::And),
    ("==", Token::Eq),
    ("!=", Token::Ne),
    ("!", Token::Not),
    ("(", Token::Open),
    (")", Token::Close),
];

fn is_name(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn tokens(expr: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut rest = expr.trim_start();
    while let Some(c) = rest.chars().next() {
        if let Some((op, token)) = OPERATORS.iter().find(|(op, _)| rest.starts_with(op)) {
            out.push(token.clone());
            rest = &rest[op.len()..];
        } else if let Some(text) = rest.strip_prefix('\'') {
            let end = text
                .find('\'')
                .ok_or_else(|| format!("an unclosed string in {expr:?}"))?;
            out.push(Token::Text(text[..end].to_owned()));
            rest = &text[end + 1..];
        } else if c.is_ascii_alphanumeric() || c == '_' {
            let len = rest.find(|c| !is_name(c)).unwrap_or(rest.len());
            out.push(Token::Name(rest[..len].to_owned()));
            rest = &rest[len..];
        } else {
            return Err(format!("{c:?} is not understood in {expr:?}"));
        }
        rest = rest.trim_start();
    }
    Ok(out)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    at: usize,
    context: &'a Context,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn take(&mut self, token: &Token) -> bool {
        let found = self.peek() == Some(token);
        self.at += usize::from(found);
        found
    }

    fn or(&mut self) -> Result<Value, String> {
        let mut value = self.and()?;
        while self.take(&Token::Or) {
            let right = self.and()?;
            value = Value::Bool(value.truthy() || right.truthy());
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<Value, String> {
        let mut value = self.comparison()?;
        while self.take(&Token::And) {
            let right = self.comparison()?;
            value = Value::Bool(value.truthy() && right.truthy());
        }
        Ok(value)
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let left = self.unary()?;
        let equal = if self.take(&Token::Eq) {
            true
        } else if self.take(&Token::Ne) {
            false
        } else {
            return Ok(left);
        };
        let right = self.unary()?;
        match (left, right) {
            (Value::Text(l), Value::Text(r)) => {
                Ok(Value::Bool(l.eq_ignore_ascii_case(&r) == equal))
            }
            (l, r) => Err(format!(
                "{l:?} compared with {r:?}: only strings are compared here"
            )),
        }
    }

    fn unary(&mut self) -> Result<Value, String> {
        if self.take(&Token::Not) {
            return Ok(Value::Bool(!self.unary()?.truthy()));
        }
        match self.tokens.get(self.at).cloned() {
            Some(Token::Open) => {
                self.at += 1;
                let value = self.or()?;
                if !self.take(&Token::Close) {
                    return Err("a `(` that is not closed".to_owned());
                }
                Ok(value)
            }
            Some(Token::Text(t)) => {
                self.at += 1;
                Ok(Value::Text(t))
            }
            Some(Token::Name(name)) => {
                self.at += 1;
                if self.take(&Token::Open) {
                    if !self.take(&Token::Close) {
                        return Err(format!("{name}( takes no arguments here"));
                    }
                    return match name.as_str() {
                        "always" => Ok(Value::Bool(true)),
                        "cancelled" => Ok(Value::Bool(self.context.cancelled)),
                        "failure" => Ok(Value::Bool(self.context.failed)),
                        _ => Err(format!("the function {name} is not understood")),
                    };
                }
                Ok(Value::Text(
                    self.context.names.get(&name).cloned().unwrap_or_default(),
                ))
            }
            other => Err(format!("{other:?} where a value was expected")),
        }
    }
}

/// Whether the condition `text` (`${{ … }}`) holds in `context`.
pub fn holds(text: &str, context: &Context) -> Result<bool, String> {
    let inner = text
        .trim()
        .strip_prefix("${{")
        .and_then(|t| t.strip_suffix("}}"))
        .ok_or_else(|| format!("{text:?} is not `${{{{ … }}}}`"))?;
    let mut parser = Parser {
        tokens: tokens(inner)?,
        at: 0,
        context,
    };
    let value = parser.or()?;
    if parser.at != parser.tokens.len() {
        return Err(format!(
            "{:?} left over in {text:?}",
            &parser.tokens[parser.at..]
        ));
    }
    Ok(value.truthy())
}
