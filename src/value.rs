//! The one value type shared by all three data models, plus its JSON codec.

use crate::error::{Error, Result};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Float(v)
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_string())
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl Value {
    /// Type class used for cross-type ordering: null < bool < number < text < array < object.
    pub(crate) fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) => 2,
            Value::Text(_) => 3,
            Value::Array(_) => 4,
            Value::Object(_) => 5,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// Field lookup on an object.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(o) => o.get(key),
            _ => None,
        }
    }

    /// A total order over all values. Ints and floats compare numerically.
    pub fn total_cmp(&self, other: &Value) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
            (Value::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Text(a), Value::Text(b)) => a.cmp(b),
            (Value::Array(a), Value::Array(b)) => {
                for (x, y) in a.iter().zip(b) {
                    let o = x.total_cmp(y);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            (Value::Object(a), Value::Object(b)) => {
                for ((ka, va), (kb, vb)) in a.iter().zip(b) {
                    let o = ka.cmp(kb).then_with(|| va.total_cmp(vb));
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                a.len().cmp(&b.len())
            }
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// Human-facing rendering: text is shown raw, NULL as `NULL`, everything else as JSON.
    pub fn display(&self) -> String {
        match self {
            Value::Null => "NULL".to_string(),
            Value::Text(s) => s.clone(),
            v => v.to_json(),
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = String::new();
        self.write_json(&mut s);
        s
    }

    fn write_json(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => out.push_str(&i.to_string()),
            Value::Float(f) => {
                if f.is_finite() {
                    // `{:?}` always keeps a float-looking form ("1.0", "1e16"), so the type round-trips.
                    out.push_str(&format!("{f:?}"));
                } else {
                    out.push_str("null");
                }
            }
            Value::Text(s) => write_json_string(s, out),
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write_json(out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }

    /// True if this value nests deeper than `limit` levels, counted the way
    /// the parser counts: a scalar is 1, `[1]` is 2. Stops descending at
    /// the limit, so it is safe to call on a value of any depth.
    pub fn deeper_than(&self, limit: usize) -> bool {
        if limit == 0 {
            return true;
        }
        match self {
            Value::Array(a) => a.iter().any(|v| v.deeper_than(limit - 1)),
            Value::Object(o) => o.values().any(|v| v.deeper_than(limit - 1)),
            _ => false,
        }
    }

    /// Parse JSON. Also accepts the relaxed shell style: unquoted keys and single-quoted strings.
    /// Nesting deeper than [`MAX_DEPTH`] levels is a parse error.
    pub fn parse(s: &str) -> Result<Value> {
        let mut p = JsonParser::at(s, 0);
        let v = p.value()?;
        p.ws();
        if p.pos != s.len() {
            return Err(Error::Parse(format!("unexpected trailing text in JSON at byte {}", p.pos)));
        }
        Ok(v)
    }
}

fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Deepest nesting of arrays and objects (counting the innermost scalar)
/// that is parsed or stored. The parser and everything that walks a
/// [`Value`] are recursive, so without a limit a deeply nested input
/// overflows the stack and aborts the process.
pub const MAX_DEPTH: usize = 128;

/// Deepest nesting of a stored document. Lower than [`MAX_DEPTH`] so that a
/// stored document can always be parsed back, also when a protocol reply
/// wraps it in a few more levels.
pub const MAX_DOC_DEPTH: usize = 100;

/// A positional JSON parser, so callers can parse a value embedded in a larger command.
pub struct JsonParser<'a> {
    src: &'a str,
    s: &'a [u8],
    pub pos: usize,
    depth: usize,
}

impl<'a> JsonParser<'a> {
    pub fn at(src: &'a str, pos: usize) -> Self {
        JsonParser { src, s: src.as_bytes(), pos, depth: 0 }
    }

    fn err<T>(&self, msg: &str) -> Result<T> {
        Err(Error::Parse(format!("JSON: {msg} at byte {}", self.pos)))
    }

    pub fn ws(&mut self) {
        while self.pos < self.s.len() && self.s[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    pub fn value(&mut self) -> Result<Value> {
        if self.depth >= MAX_DEPTH {
            return self.err("nesting is deeper than 128 levels");
        }
        self.depth += 1;
        let v = self.value_unchecked();
        self.depth -= 1;
        v
    }

    fn value_unchecked(&mut self) -> Result<Value> {
        self.ws();
        let Some(&c) = self.s.get(self.pos) else {
            return self.err("unexpected end of input");
        };
        match c {
            b'{' => {
                self.pos += 1;
                let mut map = BTreeMap::new();
                loop {
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b'}') => {
                            self.pos += 1;
                            break;
                        }
                        Some(b',') => {
                            self.pos += 1;
                            continue;
                        }
                        None => return self.err("unterminated object"),
                        _ => {}
                    }
                    let key = self.key()?;
                    self.ws();
                    if self.s.get(self.pos) != Some(&b':') {
                        return self.err("expected ':'");
                    }
                    self.pos += 1;
                    let v = self.value()?;
                    map.insert(key, v);
                }
                Ok(Value::Object(map))
            }
            b'[' => {
                self.pos += 1;
                let mut arr = Vec::new();
                loop {
                    self.ws();
                    match self.s.get(self.pos) {
                        Some(b']') => {
                            self.pos += 1;
                            break;
                        }
                        Some(b',') => {
                            self.pos += 1;
                            continue;
                        }
                        None => return self.err("unterminated array"),
                        _ => {}
                    }
                    arr.push(self.value()?);
                }
                Ok(Value::Array(arr))
            }
            b'"' | b'\'' => Ok(Value::Text(self.string()?)),
            b'-' | b'+' | b'0'..=b'9' | b'.' => self.number(),
            _ => {
                for (word, v) in [("true", Value::Bool(true)), ("false", Value::Bool(false)), ("null", Value::Null)] {
                    if self.src[self.pos..].starts_with(word) {
                        self.pos += word.len();
                        return Ok(v);
                    }
                }
                self.err("unexpected character")
            }
        }
    }

    fn key(&mut self) -> Result<String> {
        match self.s.get(self.pos) {
            Some(b'"') | Some(b'\'') => self.string(),
            _ => {
                let start = self.pos;
                while self.pos < self.s.len() {
                    let c = self.s[self.pos];
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'.' || c >= 0x80 {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                if start == self.pos {
                    return self.err("expected object key");
                }
                Ok(self.src[start..self.pos].to_string())
            }
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.pos;
        let mut float = false;
        while self.pos < self.s.len() {
            match self.s[self.pos] {
                b'0'..=b'9' | b'-' | b'+' => {}
                b'.' | b'e' | b'E' => float = true,
                _ => break,
            }
            self.pos += 1;
        }
        let text = &self.src[start..self.pos];
        if !float {
            if let Ok(i) = text.parse::<i64>() {
                return Ok(Value::Int(i));
            }
        }
        match text.parse::<f64>() {
            Ok(f) => Ok(Value::Float(f)),
            Err(_) => self.err("invalid number"),
        }
    }

    fn hex4(&mut self) -> Result<u32> {
        if self.pos + 4 > self.s.len() || !self.src.is_char_boundary(self.pos + 4) {
            return self.err("truncated \\u escape");
        }
        match u32::from_str_radix(&self.src[self.pos..self.pos + 4], 16) {
            Ok(v) => {
                self.pos += 4;
                Ok(v)
            }
            Err(_) => self.err("invalid \\u escape"),
        }
    }

    fn string(&mut self) -> Result<String> {
        let quote = self.s[self.pos];
        self.pos += 1;
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let Some(&c) = self.s.get(self.pos) else {
                return self.err("unterminated string");
            };
            self.pos += 1;
            if c == quote {
                break;
            }
            if c != b'\\' {
                buf.push(c);
                continue;
            }
            let Some(&e) = self.s.get(self.pos) else {
                return self.err("unterminated escape");
            };
            self.pos += 1;
            match e {
                b'n' => buf.push(b'\n'),
                b'r' => buf.push(b'\r'),
                b't' => buf.push(b'\t'),
                b'b' => buf.push(8),
                b'f' => buf.push(12),
                b'u' => {
                    let mut cp = self.hex4()?;
                    if (0xD800..0xDC00).contains(&cp) && self.src[self.pos..].starts_with("\\u") {
                        self.pos += 2;
                        let lo = self.hex4()?;
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                    }
                    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                    let mut tmp = [0u8; 4];
                    buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                }
                other => buf.push(other), // \" \' \\ \/
            }
        }
        String::from_utf8(buf).or_else(|_| self.err("invalid UTF-8 in string"))
    }
}
