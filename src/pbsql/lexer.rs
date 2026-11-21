//! PBSQL tokenizer.
//!
//! SQL-style tokens plus embedded JSON: a `{` or `[` starts a JSON literal
//! (relaxed shell syntax accepted) that becomes one [`Tok::Json`] token, so
//! documents can be written inline (`INSERT INTO DOCUMENT.users VALUES {...}`).

use crate::error::{Error, Result};
use crate::value::{JsonParser, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Unquoted word: `lower` is used for keywords and relational names,
    /// `orig` keeps the case for document field paths and collection names.
    Word {
        lower: String,
        orig: String,
    },
    /// Quoted identifier (`"x"` or `` `x` ``), case preserved.
    Ident(String),
    Int(i64),
    Float(f64),
    Str(String),
    Json(Value),
    Sym(&'static str),
}

pub fn perr<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Parse(msg.into()))
}

pub fn lex(src: &str) -> Result<Vec<Tok>> {
    let cs: Vec<(usize, char)> = src.char_indices().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i].1;
        let next = cs.get(i + 1).map(|x| x.1);
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && next == Some('-') {
            while i < cs.len() && cs[i].1 != '\n' {
                i += 1;
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < cs.len() && (cs[i].1.is_alphanumeric() || cs[i].1 == '_') {
                i += 1;
            }
            let orig: String = cs[start..i].iter().map(|x| x.1).collect();
            out.push(Tok::Word {
                lower: orig.to_lowercase(),
                orig,
            });
        } else if c.is_ascii_digit()
            || (c == '.' && next.is_some_and(|n| n.is_ascii_digit()) && !after_name(&out))
        {
            let start = i;
            let mut float = false;
            while i < cs.len() && (cs[i].1.is_ascii_digit() || cs[i].1 == '.') {
                float |= cs[i].1 == '.';
                i += 1;
            }
            if i < cs.len() && (cs[i].1 == 'e' || cs[i].1 == 'E') {
                let mut j = i + 1;
                if j < cs.len() && (cs[j].1 == '+' || cs[j].1 == '-') {
                    j += 1;
                }
                if j < cs.len() && cs[j].1.is_ascii_digit() {
                    float = true;
                    i = j;
                    while i < cs.len() && cs[i].1.is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = cs[start..i].iter().map(|x| x.1).collect();
            let tok = if float {
                None
            } else {
                text.parse::<i64>().ok().map(Tok::Int)
            };
            match tok {
                Some(t) => out.push(t),
                None => match text.parse::<f64>() {
                    Ok(f) => out.push(Tok::Float(f)),
                    Err(_) => return perr(format!("invalid number '{text}'")),
                },
            }
        } else if c == '\'' {
            i += 1;
            let mut s = String::new();
            loop {
                match cs.get(i).map(|x| x.1) {
                    None => return perr("unterminated string literal"),
                    Some('\'') if cs.get(i + 1).map(|x| x.1) == Some('\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some(ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c == '"' || c == '`' {
            i += 1;
            let start = i;
            while i < cs.len() && cs[i].1 != c {
                i += 1;
            }
            if i >= cs.len() {
                return perr("unterminated quoted identifier");
            }
            out.push(Tok::Ident(cs[start..i].iter().map(|x| x.1).collect()));
            i += 1;
        } else if c == '{' || c == '[' {
            let byte = cs[i].0;
            let mut jp = JsonParser::at(src, byte);
            let v = jp.value()?;
            out.push(Tok::Json(v));
            while i < cs.len() && cs[i].0 < jp.pos {
                i += 1;
            }
        } else {
            let two: Option<&'static str> = match (c, next) {
                ('<', Some('=')) => Some("<="),
                ('>', Some('=')) => Some(">="),
                ('<', Some('>')) => Some("<>"),
                ('!', Some('=')) => Some("!="),
                ('|', Some('|')) => Some("||"),
                _ => None,
            };
            if let Some(s) = two {
                out.push(Tok::Sym(s));
                i += 2;
                continue;
            }
            let one: &'static str = match c {
                '(' => "(",
                ')' => ")",
                ',' => ",",
                '*' => "*",
                '=' => "=",
                '<' => "<",
                '>' => ">",
                '+' => "+",
                '-' => "-",
                '/' => "/",
                '%' => "%",
                '.' => ".",
                ';' => ";",
                _ => return perr(format!("unexpected character '{c}'")),
            };
            out.push(Tok::Sym(one));
            i += 1;
        }
    }
    Ok(out)
}

/// `a.1` is a path step, not the number `.1`.
fn after_name(out: &[Tok]) -> bool {
    matches!(out.last(), Some(Tok::Word { .. } | Tok::Ident(_)))
}
