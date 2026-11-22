//! Binary storage layout of a document.
//!
//! A stored document is `MAGIC` followed by one encoded value:
//!
//! ```text
//! null   00                    false 01        true 02
//! int    03 zigzag-varint      float 04 f64-le
//! text   05 varint(len) utf8
//! array  06 u32-le(body bytes) varint(count) value*
//! object 07 u32-le(body bytes) varint(count) { varint(klen) key value }*   keys in byte order
//! ```
//!
//! Why not JSON (the layout before 0.3): profiling showed ~65% of query
//! time went to parsing stored JSON, because every candidate document was
//! parsed in full before the filter even looked at it. In this layout every
//! container records its byte length, so a field lookup skips whole
//! subdocuments without decoding them, and filters, sort keys, group keys and
//! index maintenance read only the fields they need. Object keys are kept in
//! sorted order, so a lookup stops as soon as it passes the key.
//!
//! Documents written by older versions are JSON and always start with `{`;
//! [`Doc::from_stored`] reads both, and an update rewrites a document in the
//! binary layout.

use crate::error::{Error, Result};
use crate::value::{Value, MAX_DEPTH};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// First byte of a binary document (never `{`, the first byte of a JSON one).
pub const MAGIC: u8 = 0xD0;

const T_NULL: u8 = 0;
const T_FALSE: u8 = 1;
const T_TRUE: u8 = 2;
const T_INT: u8 = 3;
const T_FLOAT: u8 = 4;
const T_TEXT: u8 = 5;
const T_ARRAY: u8 = 6;
const T_OBJECT: u8 = 7;

fn corrupt<T>(what: &str) -> Result<T> {
    Err(Error::Corrupt(format!("stored document: {what}")))
}

// ------------------------------------------------------------------ encoding

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Rough encoded size, used to size the output buffer once.
fn size_hint(v: &Value) -> usize {
    match v {
        Value::Null | Value::Bool(_) => 1,
        Value::Int(_) => 4,
        Value::Float(_) => 9,
        Value::Text(s) => 2 + s.len(),
        Value::Array(a) => 6 + a.iter().map(size_hint).sum::<usize>(),
        Value::Object(o) => 6 + o.iter().map(|(k, v)| 1 + k.len() + size_hint(v)).sum::<usize>(),
    }
}

fn write_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(T_NULL),
        Value::Bool(false) => out.push(T_FALSE),
        Value::Bool(true) => out.push(T_TRUE),
        Value::Int(i) => {
            // zigzag varint: small magnitudes (the common case) take 1-3 bytes, not 8
            out.push(T_INT);
            put_varint(out, ((*i << 1) ^ (*i >> 63)) as u64);
        }
        Value::Float(f) => {
            out.push(T_FLOAT);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(T_TEXT);
            put_varint(out, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Array(a) => {
            out.push(T_ARRAY);
            let at = out.len();
            out.extend_from_slice(&[0; 4]);
            put_varint(out, a.len() as u64);
            for x in a {
                write_value(x, out);
            }
            patch_len(out, at);
        }
        Value::Object(o) => {
            out.push(T_OBJECT);
            let at = out.len();
            out.extend_from_slice(&[0; 4]);
            put_varint(out, o.len() as u64);
            for (k, x) in o {
                put_varint(out, k.len() as u64);
                out.extend_from_slice(k.as_bytes());
                write_value(x, out);
            }
            patch_len(out, at);
        }
    }
}

fn patch_len(out: &mut [u8], at: usize) {
    let body = (out.len() - at - 4) as u32;
    out[at..at + 4].copy_from_slice(&body.to_le_bytes());
}

/// Encode a document for storage.
pub fn encode(doc: &Value) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + size_hint(doc));
    out.push(MAGIC);
    write_value(doc, &mut out);
    out
}

// ------------------------------------------------------------------ decoding

fn varint(b: &[u8], pos: &mut usize) -> Result<u64> {
    let mut n = 0u64;
    let mut shift = 0;
    loop {
        let Some(&byte) = b.get(*pos) else {
            return corrupt("truncated length");
        };
        *pos += 1;
        if shift >= 64 {
            return corrupt("bad length");
        }
        n |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
    }
}

fn take<'a>(b: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos.checked_add(n).filter(|&e| e <= b.len());
    match end {
        Some(e) => {
            let s = &b[*pos..e];
            *pos = e;
            Ok(s)
        }
        None => corrupt("truncated value"),
    }
}

fn text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).or_else(|_| corrupt("text is not UTF-8"))
}

/// Byte length of the encoded value starting at `b[0]`.
fn value_len(b: &[u8]) -> Result<usize> {
    let mut pos = 1;
    match b.first() {
        Some(&(T_NULL | T_FALSE | T_TRUE)) => Ok(1),
        Some(&T_INT) => varint(b, &mut pos).map(|_| pos),
        Some(&T_FLOAT) => take(b, &mut pos, 8).map(|_| 9),
        Some(&T_TEXT) => {
            let n = varint(b, &mut pos)? as usize;
            take(b, &mut pos, n)?;
            Ok(pos)
        }
        Some(&(T_ARRAY | T_OBJECT)) => {
            let raw = take(b, &mut pos, 4)?;
            let n = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
            take(b, &mut pos, n)?;
            Ok(pos)
        }
        _ => corrupt("unknown type tag"),
    }
}

/// Body of a container: (element count, bytes after the count).
fn container(b: &[u8]) -> Result<(usize, &[u8])> {
    let len = value_len(b)?;
    let mut pos = 5;
    let count = varint(&b[..len], &mut pos)? as usize;
    Ok((count, &b[pos..len]))
}

/// One field of an encoded object, borrowed. Keys are sorted, so the scan
/// stops at the first key past `key`.
fn field<'a>(obj: &'a [u8], key: &str) -> Result<Option<&'a [u8]>> {
    let (count, body) = container(obj)?;
    let mut pos = 0;
    for _ in 0..count {
        let klen = varint(body, &mut pos)? as usize;
        let k = take(body, &mut pos, klen)?;
        let vlen = value_len(&body[pos..])?;
        match k.cmp(key.as_bytes()) {
            std::cmp::Ordering::Equal => return Ok(Some(&body[pos..pos + vlen])),
            std::cmp::Ordering::Greater => return Ok(None),
            std::cmp::Ordering::Less => pos += vlen,
        }
    }
    Ok(None)
}

fn element(arr: &[u8], i: usize) -> Result<Option<&[u8]>> {
    let (count, body) = container(arr)?;
    if i >= count {
        return Ok(None);
    }
    let mut pos = 0;
    for _ in 0..i {
        pos += value_len(&body[pos..])?;
    }
    let len = value_len(&body[pos..])?;
    Ok(Some(&body[pos..pos + len]))
}

/// The encoded elements of an encoded array.
fn elements(arr: &[u8]) -> Result<Vec<&[u8]>> {
    let (count, body) = container(arr)?;
    let mut out = Vec::with_capacity(count.min(body.len()));
    let mut pos = 0;
    for _ in 0..count {
        let len = value_len(&body[pos..])?;
        out.push(&body[pos..pos + len]);
        pos += len;
    }
    Ok(out)
}

/// Every encoded value a path reaches, with the rules of
/// [`super::path::path_values`]: a non-numeric segment applied to an array
/// descends into each object element.
pub fn raw_values<'a>(doc: &'a [u8], path: &str) -> Result<Vec<&'a [u8]>> {
    let mut out = Vec::new();
    raw_walk(doc, path, &mut out)?;
    Ok(out)
}

/// `path` is the rest of the dotted path (no allocation per segment).
fn raw_walk<'a>(cur: &'a [u8], path: &str, out: &mut Vec<&'a [u8]>) -> Result<()> {
    let (p, rest) = match path.split_once('.') {
        Some((p, rest)) => (p, Some(rest)),
        None => (path, None),
    };
    let next = |v: &'a [u8], out: &mut Vec<&'a [u8]>| -> Result<()> {
        match rest {
            Some(r) => raw_walk(v, r, out),
            None => {
                out.push(v);
                Ok(())
            }
        }
    };
    match cur.first() {
        Some(&T_OBJECT) => {
            if let Some(v) = field(cur, p)? {
                next(v, out)?;
            }
        }
        Some(&T_ARRAY) => match p.parse::<usize>() {
            Ok(i) => {
                if let Some(v) = element(cur, i)? {
                    next(v, out)?;
                }
            }
            Err(_) => {
                for e in elements(cur)? {
                    if e.first() == Some(&T_OBJECT) {
                        raw_walk(e, path, out)?;
                    }
                }
            }
        },
        _ => {}
    }
    Ok(())
}

/// Resolve a dotted path inside an encoded value, with the same rules as
/// [`super::path::get_path`]: object parts are field names, array parts indexes.
pub fn raw_path<'a>(mut cur: &'a [u8], path: &str) -> Result<Option<&'a [u8]>> {
    for part in path.split('.') {
        let next = match cur.first() {
            Some(&T_OBJECT) => field(cur, part)?,
            Some(&T_ARRAY) => match part.parse::<usize>() {
                Ok(i) => element(cur, i)?,
                Err(_) => None,
            },
            _ => None,
        };
        match next {
            Some(n) => cur = n,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// Decode one encoded value.
pub fn decode_value(b: &[u8]) -> Result<Value> {
    let mut pos = 0;
    read_value(b, &mut pos, 0)
}

fn read_value(b: &[u8], pos: &mut usize, depth: usize) -> Result<Value> {
    if depth >= MAX_DEPTH {
        return corrupt("nesting too deep");
    }
    let Some(&tag) = b.get(*pos) else {
        return corrupt("truncated value");
    };
    *pos += 1;
    Ok(match tag {
        T_NULL => Value::Null,
        T_FALSE => Value::Bool(false),
        T_TRUE => Value::Bool(true),
        T_INT => {
            let z = varint(b, pos)?;
            Value::Int(((z >> 1) as i64) ^ -((z & 1) as i64))
        }
        T_FLOAT => Value::Float(f64::from_le_bytes(take(b, pos, 8)?.try_into().expect("8 bytes"))),
        T_TEXT => {
            let n = varint(b, pos)? as usize;
            Value::Text(text(take(b, pos, n)?)?.to_string())
        }
        T_ARRAY | T_OBJECT => {
            take(b, pos, 4)?;
            let count = varint(b, pos)? as usize;
            // Never trust a count for preallocation beyond what the bytes can hold.
            let cap = count.min(b.len().saturating_sub(*pos));
            if tag == T_ARRAY {
                let mut a = Vec::with_capacity(cap);
                for _ in 0..count {
                    a.push(read_value(b, pos, depth + 1)?);
                }
                Value::Array(a)
            } else {
                // In-order inserts: measured faster than collect() (which
                // buffers, sorts and bulk-builds) for the small objects
                // documents are mostly made of.
                let mut m = BTreeMap::new();
                for _ in 0..count {
                    let klen = varint(b, pos)? as usize;
                    let k = text(take(b, pos, klen)?)?.to_string();
                    m.insert(k, read_value(b, pos, depth + 1)?);
                }
                Value::Object(m)
            }
        }
        _ => return corrupt("unknown type tag"),
    })
}

/// Decode an encoded object keeping only the top-level fields `keep` accepts;
/// the others are skipped without being decoded.
pub fn decode_object_where(b: &[u8], keep: impl Fn(&str) -> bool) -> Result<Value> {
    if b.first() != Some(&T_OBJECT) {
        return decode_value(b);
    }
    let (count, body) = container(b)?;
    let mut pos = 0;
    let mut out = BTreeMap::new();
    for _ in 0..count {
        let klen = varint(body, &mut pos)? as usize;
        let k = text(take(body, &mut pos, klen)?)?;
        let vlen = value_len(&body[pos..])?;
        if keep(k) {
            out.insert(k.to_string(), read_value(&body[pos..pos + vlen], &mut 0, 1)?);
        }
        pos += vlen;
    }
    Ok(Value::Object(out))
}

// ------------------------------------------------------- stored documents

/// A stored document as read from the store: borrowed binary bytes, or a
/// legacy JSON document parsed on read.
pub enum Doc<'a> {
    Bin(&'a [u8]),
    Parsed(Value),
}

impl<'a> Doc<'a> {
    pub fn from_stored(raw: &'a [u8]) -> Result<Doc<'a>> {
        match raw.first() {
            Some(&MAGIC) => Ok(Doc::Bin(&raw[1..])),
            _ => {
                let s = std::str::from_utf8(raw).map_err(|_| Error::Corrupt("document is not UTF-8".into()))?;
                Ok(Doc::Parsed(Value::parse(s).map_err(|e| Error::Corrupt(format!("stored document: {e}")))?))
            }
        }
    }

    /// The value at a dotted path, resolved exactly (see
    /// [`super::path::get_path`]). Only that value is decoded.
    pub fn path(&self, path: &str) -> Result<Option<Cow<'_, Value>>> {
        match self {
            Doc::Bin(b) => Ok(match raw_path(b, path)? {
                Some(v) => Some(Cow::Owned(decode_value(v)?)),
                None => None,
            }),
            Doc::Parsed(v) => Ok(super::path::get_path(v, path).map(Cow::Borrowed)),
        }
    }

    /// Every value a path reaches (see [`super::path::path_values`]).
    /// Only those values are decoded.
    pub fn values(&self, path: &str) -> Result<Vec<Cow<'_, Value>>> {
        match self {
            Doc::Bin(b) => raw_values(b, path)?.into_iter().map(|v| Ok(Cow::Owned(decode_value(v)?))).collect(),
            Doc::Parsed(v) => Ok(super::path::path_values(v, path).into_iter().map(Cow::Borrowed).collect()),
        }
    }

    /// The whole document.
    pub fn to_value(self) -> Result<Value> {
        match self {
            Doc::Bin(b) => decode_value(b),
            Doc::Parsed(v) => Ok(v),
        }
    }

    /// Only the top-level fields `keep` accepts.
    pub fn to_value_where(self, keep: impl Fn(&str) -> bool) -> Result<Value> {
        match self {
            Doc::Bin(b) => decode_object_where(b, keep),
            Doc::Parsed(Value::Object(mut o)) => {
                o.retain(|k, _| keep(k));
                Ok(Value::Object(o))
            }
            Doc::Parsed(v) => Ok(v),
        }
    }
}

/// Anything a filter can read field values from: a decoded document, or a
/// stored one read lazily.
pub trait Source {
    /// Every value `path` reaches, fan-out rules included.
    fn values(&self, path: &str) -> Result<Vec<Cow<'_, Value>>>;
}

impl Source for Value {
    fn values(&self, path: &str) -> Result<Vec<Cow<'_, Value>>> {
        Ok(super::path::path_values(self, path).into_iter().map(Cow::Borrowed).collect())
    }
}

impl Source for Doc<'_> {
    fn values(&self, path: &str) -> Result<Vec<Cow<'_, Value>>> {
        Doc::values(self, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_lazy_paths() {
        let src = r#"{"_id": 7, "a": {"b": [1, 2.5, "x", null, true, {"c": "deep"}]}, "e": "", "z": -9223372036854775808}"#;
        let v = Value::parse(src).unwrap();
        let enc = encode(&v);
        assert_eq!(enc[0], MAGIC);
        assert_eq!(decode_value(&enc[1..]).unwrap(), v);
        let d = Doc::from_stored(&enc).unwrap();
        for path in ["_id", "a", "a.b", "a.b.1", "a.b.5.c", "a.b.9", "a.x", "e", "z", "a.b.0.q", "nope", "a.b.x"] {
            assert_eq!(d.path(path).unwrap().map(|c| c.into_owned()), super::super::path::get_path(&v, path).cloned(), "{path}");
        }
        let fan = Value::parse(r#"{"items": [{"sku": "a", "n": [1, 2]}, 5, {"sku": "b"}, {"x": 1}, [{"sku": "nested"}]], "o": {"p": [{"q": 1}, {"q": [2]}]}}"#).unwrap();
        let fe = encode(&fan);
        let fd = Doc::from_stored(&fe).unwrap();
        for path in ["items.sku", "items.n", "items.0.sku", "items.1", "items.sku.x", "o.p.q", "o.p.1.q.0", "o.p.q.0", "items", "nope.x"] {
            let want: Vec<Value> = super::super::path::path_values(&fan, path).into_iter().cloned().collect();
            let got: Vec<Value> = fd.values(path).unwrap().into_iter().map(|c| c.into_owned()).collect();
            assert_eq!(got, want, "{path}");
        }
        let only = Doc::from_stored(&enc).unwrap().to_value_where(|k| k == "e").unwrap();
        assert_eq!(only.to_json(), r#"{"e":""}"#);
        // legacy JSON documents read the same way
        let legacy = v.to_json().into_bytes();
        assert_eq!(Doc::from_stored(&legacy).unwrap().path("a.b.5.c").unwrap().unwrap().into_owned(), Value::from("deep"));
    }

    #[test]
    fn corrupt_bytes_are_errors_not_panics() {
        let v = Value::parse(r#"{"a": [1, {"b": "xyz"}], "c": 2}"#).unwrap();
        let enc = encode(&v);
        for cut in 1..enc.len() {
            let d = Doc::Bin(&enc[1..cut]);
            let _ = d.path("a.1.b");
            let _ = Doc::Bin(&enc[1..cut]).values("a.b");
            let _ = Doc::Bin(&enc[1..cut]).to_value();
        }
        let mut flipped = enc.clone();
        for i in 1..flipped.len() {
            flipped[i] ^= 0xff;
            let _ = Doc::Bin(&flipped[1..]).to_value();
            let _ = Doc::Bin(&flipped[1..]).path("c");
            flipped[i] ^= 0xff;
        }
    }
}
