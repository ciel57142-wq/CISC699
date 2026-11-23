//! The PBSQL lexer, shared by every part of the language.
//!
//! It is *parser driven*: the parser asks for the next token in the mode the
//! grammar expects at that point. Three modes exist:
//!
//! * [`Lexer::next_token`] – ordinary SQL tokens (words, quoted identifiers,
//!   numbers, string literals, operators). Used for SQL, transactions and
//!   the structural parts of every other statement.
//! * [`Lexer::raw_word`] – one run of non-blank characters, case preserved.
//!   Key-value keys are arbitrary byte strings such as `user:42` or
//!   `cache/a-b`, which are not SQL identifiers.
//! * [`Lexer::rest`] – everything up to the end of the statement, for the
//!   verbatim value of `SET key value words...`.
//!
//! JSON document literals inside `db.<collection>.<method>(...)` are read by
//! the JSON parser at the lexer's byte offset (see `pbsql::parser`).
//!
//! Every token carries its byte span, and every error carries a line and
//! column, so messages point at the offending input.

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    /// Unquoted word, lower-cased: a keyword or an identifier.
    Word(String),
    /// Quoted identifier (`"Name"` or `` `Name` ``), case preserved.
    Ident(String),
    Int(i64),
    Float(f64),
    /// `'text'` with `''` standing for one quote.
    Str(String),
    Sym(&'static str),
}

impl Tok {
    /// How the token is shown in error messages.
    pub(crate) fn describe(&self) -> String {
        match self {
            Tok::Word(w) | Tok::Ident(w) => format!("'{w}'"),
            Tok::Int(n) => n.to_string(),
            Tok::Float(f) => f.to_string(),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Sym(s) => format!("'{s}'"),
        }
    }
}

/// Byte range of a token in the statement text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Span {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// 1-based line and column (in characters) of byte offset `at`.
pub(crate) fn line_col(src: &str, at: usize) -> (usize, usize) {
    let at = at.min(src.len());
    let before = &src[..floor_char_boundary(src, at)];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    (line, col)
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `" (at line L, column C)"`, the suffix every positioned error carries.
pub(crate) fn position(src: &str, at: usize) -> String {
    let (l, c) = line_col(src, at);
    format!(" (at line {l}, column {c})")
}

/// Add a position to a parse error that does not have one yet.
pub(crate) fn locate(err: Error, src: &str, at: usize) -> Error {
    match err {
        Error::Parse(m) if !m.contains("(at line ") => Error::Parse(format!("{m}{}", position(src, at))),
        other => other,
    }
}

/// Two-character operators, longest match first.
const SYM2: &[&str] = &["<=", ">=", "<>", "!=", "||", "::"];
const SYM1: &[&str] = &["(", ")", ",", "*", "=", "<", ">", "+", "-", "/", "%", ".", ";"];

pub(crate) struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(src: &'a str) -> Self {
        Lexer { src, pos: 0 }
    }

    pub fn at(src: &'a str, pos: usize) -> Self {
        Lexer { src, pos }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn src(&self) -> &'a str {
        self.src
    }

    fn err<T>(&self, msg: impl Into<String>, at: usize) -> Result<T> {
        Err(Error::Parse(format!("{}{}", msg.into(), position(self.src, at))))
    }

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_char_at(&self, off: usize) -> Option<char> {
        self.src[self.pos..].chars().nth(off)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Skip whitespace, `-- line` comments and `/* block */` comments.
    pub fn skip_trivia(&mut self) -> Result<()> {
        loop {
            match (self.peek_char(), self.peek_char_at(1)) {
                (Some(c), _) if c.is_whitespace() => {
                    self.bump();
                }
                (Some('-'), Some('-')) => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                (Some('/'), Some('*')) => {
                    let start = self.pos;
                    self.pos += 2;
                    match self.src[self.pos..].find("*/") {
                        Some(i) => self.pos += i + 2,
                        None => return self.err("unterminated /* comment", start),
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    /// The next SQL token, or None at the end of the input.
    pub fn next_token(&mut self) -> Result<Option<Token>> {
        self.skip_trivia()?;
        let start = self.pos;
        let Some(c) = self.peek_char() else {
            return Ok(None);
        };
        let next = self.peek_char_at(1);
        let tok = if c.is_alphabetic() || c == '_' {
            while matches!(self.peek_char(), Some(ch) if ch.is_alphanumeric() || ch == '_') {
                self.bump();
            }
            Tok::Word(self.src[start..self.pos].to_lowercase())
        } else if c.is_ascii_digit() || (c == '.' && next.map_or(false, |n| n.is_ascii_digit())) {
            self.number(start)?
        } else if c == '\'' {
            Tok::Str(self.quoted('\'', start, "unterminated string literal")?)
        } else if c == '"' || c == '`' {
            let s = self.quoted(c, start, "unterminated quoted identifier")?;
            if s.is_empty() {
                return self.err("a quoted identifier cannot be empty", start);
            }
            Tok::Ident(s)
        } else {
            let rest = &self.src[self.pos..];
            if let Some(s) = SYM2.iter().find(|s| rest.starts_with(**s)) {
                self.pos += 2;
                Tok::Sym(s)
            } else if let Some(s) = SYM1.iter().find(|s| rest.starts_with(**s)) {
                self.pos += 1;
                Tok::Sym(s)
            } else {
                return self.err(format!("unexpected character '{c}'"), start);
            }
        };
        Ok(Some(Token { tok, span: Span { start, end: self.pos } }))
    }

    /// Digits with an optional fraction and exponent. An integer that does
    /// not fit in 64 bits becomes a float.
    fn number(&mut self, start: usize) -> Result<Tok> {
        let mut float = false;
        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit() || c == '.') {
            float |= self.peek_char() == Some('.');
            self.bump();
        }
        if matches!(self.peek_char(), Some('e' | 'E')) {
            let save = self.pos;
            self.bump();
            if matches!(self.peek_char(), Some('+' | '-')) {
                self.bump();
            }
            if matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                float = true;
                while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
                    self.bump();
                }
            } else {
                self.pos = save;
            }
        }
        if matches!(self.peek_char(), Some(c) if c.is_alphabetic() || c == '_') {
            // `12abc` is neither a number nor a name
            while matches!(self.peek_char(), Some(c) if c.is_alphanumeric() || c == '_') {
                self.bump();
            }
            return self.err(format!("invalid number '{}'", &self.src[start..self.pos]), start);
        }
        let text = &self.src[start..self.pos];
        if !float {
            if let Ok(n) = text.parse::<i64>() {
                return Ok(Tok::Int(n));
            }
        }
        match text.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Tok::Float(f)),
            _ => self.err(format!("invalid number '{text}'"), start),
        }
    }

    /// A quoted run where a doubled quote stands for one quote character.
    fn quoted(&mut self, q: char, start: usize, unterminated: &str) -> Result<String> {
        self.bump();
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return self.err(unterminated, start),
                Some(c) if c == q => {
                    if self.peek_char() == Some(q) {
                        self.bump();
                        s.push(q);
                    } else {
                        return Ok(s);
                    }
                }
                Some(c) => s.push(c),
            }
        }
    }

    /// Key-value argument: a bare run of non-blank characters (case kept),
    /// or a `'...'` / `"..."` string with backslash escapes.
    pub fn raw_word(&mut self) -> Result<Option<(String, Span)>> {
        self.skip_ws_only();
        let start = self.pos;
        match self.peek_char() {
            None => Ok(None),
            Some(';') if self.src[self.pos + 1..].trim().is_empty() => Ok(None),
            Some(q @ ('\'' | '"')) => {
                let s = self.escaped_string(q, start)?;
                Ok(Some((s, Span { start, end: self.pos })))
            }
            Some(_) => {
                while matches!(self.peek_char(), Some(c) if !c.is_whitespace()) {
                    self.bump();
                }
                let mut end = self.pos;
                // a statement-ending `;` is not part of the word
                if self.src[end..].trim().is_empty() && self.src[start..end].ends_with(';') {
                    end -= 1;
                }
                if end == start {
                    return Ok(None);
                }
                Ok(Some((self.src[start..end].to_string(), Span { start, end })))
            }
        }
    }

    /// Only whitespace: key-value text is taken literally, so `--` is not a comment there.
    fn skip_ws_only(&mut self) {
        while matches!(self.peek_char(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    /// `'...'` or `"..."` with `\n \t \r \0 \\ \' \"` escapes; any other
    /// escaped character stands for itself.
    pub fn escaped_string(&mut self, q: char, start: usize) -> Result<String> {
        self.bump();
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return self.err("unterminated quoted string", start),
                Some('\\') => match self.bump() {
                    None => return self.err("unterminated quoted string", start),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some('0') => out.push('\0'),
                    Some(other) => out.push(other),
                },
                Some(c) if c == q => return Ok(out),
                Some(c) => out.push(c),
            }
        }
    }

    /// The rest of the statement verbatim (surrounding blanks and a final `;` removed).
    pub fn rest(&mut self) -> (String, Span) {
        self.skip_ws_only();
        let start = self.pos;
        let mut text = self.src[start..].trim_end();
        if let Some(t) = text.strip_suffix(';') {
            text = t.trim_end();
        }
        self.pos = self.src.len();
        (text.to_string(), Span { start, end: start + text.len() })
    }

    /// Every remaining token.
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        while let Some(t) = self.next_token()? {
            out.push(t);
        }
        Ok(out)
    }
}

/// Tokens of a whole SQL text.
pub(crate) fn tokenize(src: &str) -> Result<Vec<Token>> {
    Lexer::new(src).tokenize()
}
