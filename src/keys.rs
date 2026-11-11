//! How the three data models share one ordered keyspace.
//!
//! ```text
//! k \0 <key>                 key-value entry
//! r \0 <table> \0 <pk>       relational row   (JSON array of column values)
//! d \0 <coll>  \0 <id>       document         (JSON object)
//! m \0 t \0 <table>          table schema
//! m \0 c \0 <coll>           collection marker
//! m \0 s \0 <name>           sequence counter (row ids, document ids)
//! i \0 <owner> \0 <field> \0 <value><id>   secondary index entry (empty value)
//! ```
//! Table and collection names are plain identifiers and can never contain
//! `\0`, so no model can read or clobber another model's keys.

use crate::error::{Error, Result};
use crate::storage::Ctx;
use crate::value::Value;

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !name.chars().next().unwrap().is_ascii_digit()
}

fn key(parts: &[&[u8]]) -> Vec<u8> {
    let mut k = Vec::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            k.push(0);
        }
        k.extend_from_slice(p);
    }
    k
}

pub const KV_PREFIX: &[u8] = b"k\0";
pub const TABLE_META_PREFIX: &[u8] = b"m\0t\0";
pub const COLL_META_PREFIX: &[u8] = b"m\0c\0";

pub fn kv(k: &str) -> Vec<u8> {
    key(&[b"k", k.as_bytes()])
}
pub fn table_meta(t: &str) -> Vec<u8> {
    key(&[b"m", b"t", t.as_bytes()])
}
pub fn coll_meta(c: &str) -> Vec<u8> {
    key(&[b"m", b"c", c.as_bytes()])
}
pub fn seq(name: &str) -> Vec<u8> {
    key(&[b"m", b"s", name.as_bytes()])
}
pub fn row_prefix(t: &str) -> Vec<u8> {
    key(&[b"r", t.as_bytes(), b""])
}
pub fn doc_prefix(c: &str) -> Vec<u8> {
    key(&[b"d", c.as_bytes(), b""])
}

/// Order-preserving encoding of a primary key / document id.
/// Integers sort numerically, strings lexicographically, integers before strings.
pub fn encode_id(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Int(i) => {
            let mut out = vec![2u8];
            out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
            Ok(out)
        }
        Value::Text(s) => {
            let mut out = vec![4u8];
            out.extend_from_slice(s.as_bytes());
            Ok(out)
        }
        Value::Object(o) if o.len() == 1 => match o.get("$oid") {
            // MongoDB ObjectId, kept in extended-JSON form {"$oid": "<24 hex>"}
            Some(Value::Text(h)) if h.len() == 24 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                let mut out = vec![7u8];
                out.extend((0..12).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)));
                Ok(out)
            }
            _ => Err(Error::Type(format!("keys must be integers, strings or ObjectIds, got {}", v.to_json()))),
        },
        other => Err(Error::Type(format!("keys must be integers or strings, got {}", other.to_json()))),
    }
}

/// All index entries of one table (`t:<name>`) or collection (`c:<name>`).
pub fn index_owner_prefix(owner: &str) -> Vec<u8> {
    key(&[b"i", owner.as_bytes(), b""])
}

/// Index entries of one field. An entry is this prefix, then the encoded
/// value, then the id bytes of the row or document it points at.
pub fn index_prefix(owner: &str, field: &str) -> Vec<u8> {
    key(&[b"i", owner.as_bytes(), field.as_bytes(), b""])
}

/// Self-delimiting encoding of an indexable scalar. Ints and floats share
/// one encoding so `1` and `1.0` land on the same entry. NULL, arrays and
/// objects are not indexed.
pub fn encode_index_value(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Bool(b) => Some(vec![1, *b as u8]),
        Value::Int(_) | Value::Float(_) => {
            let f = v.as_f64()?;
            let f = if f == 0.0 { 0.0 } else { f };
            let bits = f.to_bits();
            let ordered = if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) };
            let mut out = vec![2u8];
            out.extend_from_slice(&ordered.to_be_bytes());
            Some(out)
        }
        Value::Text(s) => {
            let mut out = vec![4u8];
            for &b in s.as_bytes() {
                out.push(b);
                if b == 0 {
                    out.push(0xff);
                }
            }
            out.extend_from_slice(&[0, 0]);
            Some(out)
        }
        _ => None,
    }
}

/// Transactional auto-increment counter (first value is 1).
pub fn next_seq(ctx: &mut Ctx, name: &str) -> Result<i64> {
    let k = seq(name);
    let cur = match ctx.get(&k) {
        Some(raw) => String::from_utf8_lossy(&raw)
            .parse::<i64>()
            .map_err(|_| Error::Corrupt(format!("sequence '{name}' is not a number")))?,
        None => 0,
    };
    let next = cur + 1;
    ctx.put(k, next.to_string().into_bytes());
    Ok(next)
}
