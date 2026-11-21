//! MongoDB wire protocol front end for the document model.
//!
//! Speaks OP_MSG (and the legacy OP_QUERY handshake) with a BSON codec, so
//! MongoDB drivers can connect. Supported commands: hello / isMaster, ping,
//! buildInfo, insert, find, getMore, killCursors, update (multi, upsert),
//! delete, count, aggregate (see [`crate::aggregate`] for the stages),
//! distinct, drop, dropDatabase, listCollections, listDatabases,
//! createIndexes (single field).
//!
//! Databases are isolated: collections of database `d` are stored as
//! `d.<coll>`, except the default database (`--mongo-default-db`, "test"
//! unless configured), whose collections are the bare names the native
//! `db.<coll>` API uses. `find` and `aggregate` return results in batches
//! through server-side cursors (first batch 101 documents unless
//! `batchSize` says otherwise; idle cursors expire after 10 minutes).
//!
//! Differences from MongoDB: field order inside documents is not
//! preserved, and there is no authentication.
//! ObjectIds and dates are kept as `{"$oid": ...}` / `{"$date": ...}`.

use crate::doc::{self, FindOptions};
use crate::error::Error;
use crate::server::Limits;
use crate::value::Value;
use crate::Database;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ----------------------------------------------------------------------- BSON

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("BSON: {msg}"))
}

fn rd_i32(b: &[u8], pos: usize) -> io::Result<i32> {
    b.get(pos..pos + 4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| bad("truncated"))
}

fn rd_i64(b: &[u8], pos: usize) -> io::Result<i64> {
    let s = b.get(pos..pos + 8).ok_or_else(|| bad("truncated"))?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(i64::from_le_bytes(a))
}

fn rd_cstr(b: &[u8], pos: usize) -> io::Result<(String, usize)> {
    let rest = b.get(pos..).ok_or_else(|| bad("truncated"))?;
    let end = rest
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| bad("unterminated name"))?;
    Ok((String::from_utf8_lossy(&rest[..end]).into_owned(), end + 1))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn wrap(key: &str, v: Value) -> Value {
    Value::Object(BTreeMap::from([(key.to_string(), v)]))
}

/// Decode one BSON document at the start of `b`.
/// Returns (document, name of its first field, bytes consumed).
pub fn decode_doc(b: &[u8]) -> io::Result<(Value, Option<String>, usize)> {
    let (pairs, used) = decode_elems(b, 0)?;
    let first = pairs.first().map(|p| p.0.clone());
    Ok((Value::Object(pairs.into_iter().collect()), first, used))
}

/// Deepest nesting of BSON documents and arrays accepted from a client.
/// Decoding is recursive; this keeps a hostile message from overflowing the
/// stack. Documents are checked again against `value::MAX_DOC_DEPTH` when stored.
const MAX_BSON_DEPTH: usize = 100;

fn decode_elems(b: &[u8], depth: usize) -> io::Result<(Vec<(String, Value)>, usize)> {
    if depth >= MAX_BSON_DEPTH {
        return Err(bad("document nesting is deeper than 100 levels"));
    }
    let total = rd_i32(b, 0)? as usize;
    if total < 5 || total > b.len() {
        return Err(bad("bad document length"));
    }
    let b = &b[..total];
    let mut pos = 4;
    let mut out = Vec::new();
    while pos < total - 1 {
        let ty = b[pos];
        let (name, n) = rd_cstr(b, pos + 1)?;
        pos += 1 + n;
        let need = |len: usize| {
            if pos + len <= total {
                Ok(())
            } else {
                Err(bad("truncated value"))
            }
        };
        let v = match ty {
            0x01 => {
                let v = Value::Float(f64::from_bits(rd_i64(b, pos)? as u64));
                pos += 8;
                v
            }
            0x02 | 0x0D | 0x0E => {
                let len = rd_i32(b, pos)? as usize;
                need(4 + len)?;
                if len == 0 {
                    return Err(bad("bad string length"));
                }
                let v = Value::Text(
                    String::from_utf8_lossy(&b[pos + 4..pos + 4 + len - 1]).into_owned(),
                );
                pos += 4 + len;
                v
            }
            0x03 | 0x04 => {
                let (pairs, used) = decode_elems(&b[pos..], depth + 1)?;
                pos += used;
                if ty == 0x04 {
                    Value::Array(pairs.into_iter().map(|p| p.1).collect())
                } else {
                    Value::Object(pairs.into_iter().collect())
                }
            }
            0x05 => {
                let len = rd_i32(b, pos)? as usize;
                need(5 + len)?;
                let v = wrap("$binary", Value::Text(hex(&b[pos + 5..pos + 5 + len])));
                pos += 5 + len;
                v
            }
            0x06 | 0x0A | 0xFF | 0x7F => Value::Null,
            0x07 => {
                need(12)?;
                let v = wrap("$oid", Value::Text(hex(&b[pos..pos + 12])));
                pos += 12;
                v
            }
            0x08 => {
                need(1)?;
                pos += 1;
                Value::Bool(b[pos - 1] != 0)
            }
            0x09 => {
                let v = wrap("$date", Value::Int(rd_i64(b, pos)?));
                pos += 8;
                v
            }
            0x0B => {
                let (_, a) = rd_cstr(b, pos)?;
                let (_, c) = rd_cstr(b, pos + a)?;
                pos += a + c;
                Value::Null
            }
            0x10 => {
                let v = Value::Int(rd_i32(b, pos)? as i64);
                pos += 4;
                v
            }
            0x11 | 0x12 => {
                let v = Value::Int(rd_i64(b, pos)?);
                pos += 8;
                v
            }
            0x13 => {
                need(16)?;
                pos += 16;
                Value::Null
            }
            other => return Err(bad(&format!("unsupported element type 0x{other:02x}"))),
        };
        out.push((name, v));
    }
    Ok((out, total))
}

fn put_cstr(out: &mut Vec<u8>, s: &str) {
    out.extend(s.bytes().filter(|b| *b != 0));
    out.push(0);
}

fn encode_elem(out: &mut Vec<u8>, key: &str, v: &Value) {
    let head = |ty: u8, out: &mut Vec<u8>| {
        out.push(ty);
        put_cstr(out, key);
    };
    match v {
        Value::Null => head(0x0A, out),
        Value::Bool(b) => {
            head(0x08, out);
            out.push(*b as u8);
        }
        Value::Int(i) => match i32::try_from(*i) {
            Ok(small) => {
                head(0x10, out);
                out.extend_from_slice(&small.to_le_bytes());
            }
            Err(_) => {
                head(0x12, out);
                out.extend_from_slice(&i.to_le_bytes());
            }
        },
        Value::Float(f) => {
            head(0x01, out);
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Text(s) => {
            head(0x02, out);
            out.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
            out.push(0);
        }
        Value::Array(a) => {
            head(0x04, out);
            let start = out.len();
            out.extend_from_slice(&[0; 4]);
            for (i, x) in a.iter().enumerate() {
                encode_elem(out, &i.to_string(), x);
            }
            out.push(0);
            let len = (out.len() - start) as i32;
            out[start..start + 4].copy_from_slice(&len.to_le_bytes());
        }
        Value::Object(o) => {
            // extended-JSON wrappers turn back into native BSON types
            if o.len() == 1 {
                match o.iter().next() {
                    Some((k, Value::Text(h)))
                        if k == "$oid"
                            && h.len() == 24
                            && h.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    {
                        head(0x07, out);
                        out.extend(
                            (0..12)
                                .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)),
                        );
                        return;
                    }
                    Some((k, Value::Int(ms))) if k == "$date" => {
                        head(0x09, out);
                        out.extend_from_slice(&ms.to_le_bytes());
                        return;
                    }
                    Some((k, Value::Int(n))) if k == "$numberLong" => {
                        head(0x12, out);
                        out.extend_from_slice(&n.to_le_bytes());
                        return;
                    }
                    Some((k, Value::Text(h)))
                        if k == "$binary"
                            && h.len() % 2 == 0
                            && h.bytes().all(|b| b.is_ascii_hexdigit()) =>
                    {
                        head(0x05, out);
                        out.extend_from_slice(&(h.len() as i32 / 2).to_le_bytes());
                        out.push(0);
                        out.extend(
                            (0..h.len() / 2)
                                .map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)),
                        );
                        return;
                    }
                    _ => {}
                }
            }
            head(0x03, out);
            encode_doc_into(out, o);
        }
    }
}

fn encode_doc_into(out: &mut Vec<u8>, o: &BTreeMap<String, Value>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (k, v) in o {
        encode_elem(out, k, v);
    }
    out.push(0);
    let len = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

/// Encode an object as a BSON document (non-objects encode as an empty document).
pub fn encode_doc(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    match v {
        Value::Object(o) => encode_doc_into(&mut out, o),
        _ => encode_doc_into(&mut out, &BTreeMap::new()),
    }
    out
}

// ------------------------------------------------------------------- commands

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn long(n: i64) -> Value {
    wrap("$numberLong", Value::Int(n))
}

fn ok(mut pairs: Vec<(&'static str, Value)>) -> Value {
    pairs.push(("ok", Value::Float(1.0)));
    obj(pairs)
}

fn fail(code: i64, code_name: &str, msg: &str) -> Value {
    obj(vec![
        ("ok", Value::Float(0.0)),
        ("errmsg", msg.into()),
        ("code", Value::Int(code)),
        ("codeName", code_name.into()),
    ])
}

fn error_code(e: &Error) -> (i64, &'static str) {
    match e {
        Error::Constraint(_) => (11000, "DuplicateKey"),
        Error::Conflict(_) => (112, "WriteConflict"),
        Error::Parse(_) => (9, "FailedToParse"),
        Error::Type(_) => (14, "TypeMismatch"),
        Error::Schema(_) => (73, "InvalidNamespace"),
        _ => (1, "InternalError"),
    }
}

fn fail_with(e: &Error) -> Value {
    let (code, name) = error_code(e);
    fail(code, name, &e.to_string())
}

fn write_error(index: usize, e: &Error) -> Value {
    obj(vec![
        ("index", Value::Int(index as i64)),
        ("code", Value::Int(error_code(e).0)),
        ("errmsg", e.to_string().into()),
    ])
}

fn cursor(ns: &str, batch: Vec<Value>) -> Value {
    ok(vec![(
        "cursor",
        obj(vec![
            ("firstBatch", Value::Array(batch)),
            ("id", long(0)),
            ("ns", ns.into()),
        ]),
    )])
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(v) => v.as_f64().is_some_and(|n| n != 0.0),
        None => false,
    }
}

fn count_of(v: Option<&Value>) -> Option<usize> {
    v.and_then(Value::as_f64)
        .filter(|n| *n > 0.0)
        .map(|n| n as usize)
}

fn list<'a>(cmd: &'a Value, key: &str) -> &'a [Value] {
    match cmd.get(key) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Open cursors of one connection.
pub struct Cursors {
    open: HashMap<i64, Cursor>,
    next: i64,
    /// MongoDB database whose collections are stored under their bare names.
    default_db: String,
}

struct Cursor {
    ns: String,
    docs: VecDeque<Value>,
    touched: Instant,
}

/// Idle cursors are dropped after this long.
const CURSOR_TIMEOUT: Duration = Duration::from_secs(600);
/// Most cursors one connection may keep open (the oldest is dropped beyond this).
const MAX_CURSORS: usize = 256;
/// A batch stops growing once it holds this many bytes of documents.
const MAX_BATCH_BYTES: usize = 8 << 20;
const DEFAULT_FIRST_BATCH: usize = 101;

impl Cursors {
    pub fn new(default_db: &str) -> Cursors {
        Cursors {
            open: HashMap::new(),
            next: 1,
            default_db: default_db.to_string(),
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.open
            .retain(|_, c| now.duration_since(c.touched) < CURSOR_TIMEOUT);
    }

    /// Storage name of a collection in a database: bare for the default
    /// database (compatible with the native `db.<coll>` API), `db.coll` otherwise.
    fn coll(&self, db: &str, coll: &str) -> String {
        if db.is_empty() || db == self.default_db {
            coll.to_string()
        } else {
            format!("{db}.{coll}")
        }
    }

    /// Split a stored collection name into (database, collection).
    fn split<'a>(&'a self, stored: &'a str) -> (&'a str, &'a str) {
        match stored.split_once('.') {
            Some((d, c)) => (d, c),
            None => (self.default_db.as_str(), stored),
        }
    }

    /// Take one batch; returns (batch, cursor id or 0 when exhausted).
    fn batch(
        &mut self,
        ns: &str,
        mut docs: VecDeque<Value>,
        size: Option<usize>,
        existing: Option<i64>,
    ) -> (Vec<Value>, i64) {
        let limit = size.unwrap_or(usize::MAX).max(1);
        let mut out = Vec::new();
        let mut bytes = 0;
        while out.len() < limit {
            let Some(d) = docs.front() else { break };
            let n = d.to_json().len();
            if !out.is_empty() && bytes + n > MAX_BATCH_BYTES {
                break;
            }
            bytes += n;
            out.push(docs.pop_front().expect("front exists"));
        }
        if docs.is_empty() {
            if let Some(id) = existing {
                self.open.remove(&id);
            }
            return (out, 0);
        }
        let id = existing.unwrap_or_else(|| {
            let id = self.next;
            self.next += 1;
            id
        });
        if self.open.len() >= MAX_CURSORS && !self.open.contains_key(&id) {
            if let Some(oldest) = self
                .open
                .iter()
                .min_by_key(|(_, c)| c.touched)
                .map(|(k, _)| *k)
            {
                self.open.remove(&oldest);
            }
        }
        self.open.insert(
            id,
            Cursor {
                ns: ns.to_string(),
                docs,
                touched: Instant::now(),
            },
        );
        (out, id)
    }
}

fn first_batch(cursors: &mut Cursors, ns: &str, docs: Vec<Value>, cmd: &Value) -> Value {
    let single = truthy(cmd.get("singleBatch"));
    let size = cmd
        .get("cursor")
        .and_then(|c| count_of(c.get("batchSize")))
        .or_else(|| count_of(cmd.get("batchSize")))
        .unwrap_or(DEFAULT_FIRST_BATCH);
    let size = (!single).then_some(size);
    let (batch, id) = if single {
        (docs, 0)
    } else {
        cursors.batch(ns, docs.into(), size, None)
    };
    ok(vec![(
        "cursor",
        obj(vec![
            ("firstBatch", Value::Array(batch)),
            ("id", long(id)),
            ("ns", ns.into()),
        ]),
    )])
}

const COMMANDS: &[&str] = &[
    "hello",
    "ismaster",
    "ping",
    "endsessions",
    "abortsessions",
    "getmore",
    "killcursors",
    "dropdatabase",
    "buildinfo",
    "whatsmyuri",
    "insert",
    "find",
    "aggregate",
    "count",
    "distinct",
    "update",
    "delete",
    "drop",
    "create",
    "listcollections",
    "listdatabases",
    "createindexes",
    "listindexes",
    "dropindexes",
];

// --------------------------------------------- key order from raw BSON

/// Size of one BSON element value starting at `pos`.
fn bson_value_len(b: &[u8], ty: u8, pos: usize) -> Option<usize> {
    let i32_at = |p: usize| {
        b.get(p..p + 4)
            .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    };
    Some(match ty {
        0x01 | 0x09 | 0x11 | 0x12 => 8,
        0x02 | 0x0D | 0x0E => 4 + i32_at(pos)?,
        0x03 | 0x04 => i32_at(pos)?,
        0x05 => 5 + i32_at(pos)?,
        0x06 | 0x0A | 0xFF | 0x7F => 0,
        0x07 => 12,
        0x08 => 1,
        0x10 => 4,
        0x13 => 16,
        0x0B => {
            let a = b.get(pos..)?.iter().position(|c| *c == 0)? + 1;
            let c = b.get(pos + a..)?.iter().position(|c| *c == 0)? + 1;
            a + c
        }
        _ => return None,
    })
}

/// (name, type, value bytes) of each element of the document at the start of `b`, in order.
fn bson_elems(b: &[u8]) -> Vec<(String, u8, &[u8])> {
    let mut out = Vec::new();
    let Some(total) = b
        .get(0..4)
        .map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as usize)
    else {
        return out;
    };
    let mut pos = 4;
    while pos + 1 < total.min(b.len()) {
        let ty = b[pos];
        let Some(nlen) = b
            .get(pos + 1..)
            .and_then(|r| r.iter().position(|c| *c == 0))
        else {
            break;
        };
        let name = String::from_utf8_lossy(&b[pos + 1..pos + 1 + nlen]).into_owned();
        let vpos = pos + 2 + nlen;
        let Some(len) = bson_value_len(b, ty, vpos) else {
            break;
        };
        let Some(v) = b.get(vpos..vpos + len) else {
            break;
        };
        out.push((name, ty, v));
        pos = vpos + len;
    }
    out
}

/// A sub-document's keys in written order as `[[key, value], ...]`.
fn ordered_pairs(doc_bytes: &[u8]) -> Option<Value> {
    let (v, _, _) = decode_doc(doc_bytes).ok()?;
    let pairs = bson_elems(doc_bytes)
        .into_iter()
        .map(|(k, _, _)| {
            Value::Array(vec![
                Value::Text(k.clone()),
                v.get(&k).cloned().unwrap_or(Value::Null),
            ])
        })
        .collect();
    Some(Value::Array(pairs))
}

/// Replace order-sensitive specs in a decoded command (index keys, find
/// sort, `$sort` stages) with order-preserving pair lists read from the raw BSON.
fn restore_key_order(cmd: &mut BTreeMap<String, Value>, raw: &[u8]) {
    for (name, ty, bytes) in bson_elems(raw) {
        match (name.as_str(), ty) {
            ("sort", 0x03) => {
                if let Some(p) = ordered_pairs(bytes) {
                    cmd.insert(name, p);
                }
            }
            ("indexes", 0x04) => {
                if let Some(Value::Array(list)) = cmd.get_mut("indexes") {
                    for ((_, ty, ixb), ix) in bson_elems(bytes).into_iter().zip(list.iter_mut()) {
                        if ty != 0x03 {
                            continue;
                        }
                        for (k, kty, kb) in bson_elems(ixb) {
                            if k == "key" && kty == 0x03 {
                                if let (Some(p), Value::Object(o)) = (ordered_pairs(kb), &mut *ix) {
                                    o.insert("key".into(), p);
                                }
                            }
                        }
                    }
                }
            }
            ("pipeline", 0x04) => {
                if let Some(Value::Array(stages)) = cmd.get_mut("pipeline") {
                    for ((_, ty, sb), st) in bson_elems(bytes).into_iter().zip(stages.iter_mut()) {
                        if ty != 0x03 {
                            continue;
                        }
                        for (k, kty, kb) in bson_elems(sb) {
                            if k == "$sort" && kty == 0x03 {
                                if let (Some(p), Value::Object(o)) = (ordered_pairs(kb), &mut *st) {
                                    o.insert("$sort".into(), p);
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Execute one database command and build its reply document.
fn command(
    db: &mut Database,
    cursors: &mut Cursors,
    name: &str,
    cmd: &Value,
    conn_id: i64,
) -> Value {
    cursors.expire();
    let empty = Value::Object(BTreeMap::new());
    let dbname = cmd
        .get("$db")
        .and_then(Value::as_str)
        .unwrap_or(cursors.default_db.as_str())
        .to_string();
    let short = cmd.get(name).and_then(Value::as_str).unwrap_or("");
    let stored = cursors.coll(&dbname, short);
    let coll = stored.as_str();
    let ns = format!("{dbname}.{short}");
    let res: Result<Value, Error> = (|| {
        Ok(match name.to_ascii_lowercase().as_str() {
            "hello" | "ismaster" => ok(vec![
                ("ismaster", true.into()),
                ("isWritablePrimary", true.into()),
                ("helloOk", true.into()),
                ("maxBsonObjectSize", Value::Int(16 << 20)),
                ("maxMessageSizeBytes", Value::Int(48_000_000)),
                ("maxWriteBatchSize", Value::Int(100_000)),
                ("localTime", wrap("$date", Value::Int(now_ms()))),
                ("minWireVersion", Value::Int(0)),
                ("maxWireVersion", Value::Int(17)),
                ("readOnly", false.into()),
                ("connectionId", Value::Int(conn_id)),
            ]),
            "ping" | "endsessions" | "abortsessions" => ok(vec![]),
            "getmore" => {
                let id = match cmd.get("getMore") {
                    Some(Value::Object(o)) => o.get("$numberLong").and_then(Value::as_i64),
                    Some(v) => v.as_i64(),
                    None => None,
                }
                .unwrap_or(0);
                let short = cmd.get("collection").and_then(Value::as_str).unwrap_or("");
                let ns = format!("{dbname}.{short}");
                match cursors.open.remove(&id) {
                    Some(c) if c.ns == ns => {
                        let (batch, next) =
                            cursors.batch(&ns, c.docs, count_of(cmd.get("batchSize")), Some(id));
                        ok(vec![(
                            "cursor",
                            obj(vec![
                                ("nextBatch", Value::Array(batch)),
                                ("id", long(next)),
                                ("ns", ns.as_str().into()),
                            ]),
                        )])
                    }
                    Some(c) => {
                        cursors.open.insert(id, c);
                        fail(
                            13,
                            "Unauthorized",
                            "cursor belongs to a different namespace",
                        )
                    }
                    None => fail(43, "CursorNotFound", &format!("cursor id {id} not found")),
                }
            }
            "killcursors" => {
                let mut killed = Vec::new();
                let mut missing = Vec::new();
                for c in list(cmd, "cursors") {
                    let id = c
                        .as_i64()
                        .or_else(|| c.get("$numberLong").and_then(Value::as_i64))
                        .unwrap_or(0);
                    if cursors.open.remove(&id).is_some() {
                        killed.push(long(id));
                    } else {
                        missing.push(long(id));
                    }
                }
                ok(vec![
                    ("cursorsKilled", Value::Array(killed)),
                    ("cursorsNotFound", Value::Array(missing)),
                ])
            }
            "dropdatabase" => {
                let mut n = 0;
                for c in db.doc_collections()? {
                    if cursors.split(&c).0 == dbname {
                        db.doc_drop(&c)?;
                        n += 1;
                    }
                }
                ok(vec![
                    ("dropped", dbname.as_str().into()),
                    ("collections", Value::Int(n)),
                ])
            }
            "buildinfo" => ok(vec![
                ("version", "6.0.0".into()),
                (
                    "versionArray",
                    Value::Array(vec![
                        Value::Int(6),
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(0),
                    ]),
                ),
                (
                    "gitVersion",
                    format!("polydb-{}", env!("CARGO_PKG_VERSION")).into(),
                ),
                ("maxBsonObjectSize", Value::Int(16 << 20)),
            ]),
            "whatsmyuri" => ok(vec![("you", "127.0.0.1:0".into())]),
            "insert" => {
                let mut n = 0;
                let mut errors = Vec::new();
                for (i, d) in list(cmd, "documents").iter().enumerate() {
                    match db.doc_insert(coll, d.clone()) {
                        Ok(_) => n += 1,
                        Err(e) => {
                            errors.push(write_error(i, &e));
                            if cmd.get("ordered") != Some(&Value::Bool(false)) {
                                break;
                            }
                        }
                    }
                }
                let mut reply = vec![("n", Value::Int(n))];
                if !errors.is_empty() {
                    reply.push(("writeErrors", Value::Array(errors)));
                }
                ok(reply)
            }
            "find" => {
                let opts = FindOptions {
                    sort: cmd
                        .get("sort")
                        .filter(|s| {
                            matches!(s, Value::Object(o) if !o.is_empty())
                                || matches!(s, Value::Array(a) if !a.is_empty())
                        })
                        .cloned(),
                    skip: count_of(cmd.get("skip")).unwrap_or(0),
                    limit: count_of(cmd.get("limit")),
                };
                let mut docs = db.doc_find(coll, cmd.get("filter").unwrap_or(&empty), &opts)?;
                if let Some(p) = cmd.get("projection") {
                    docs = docs.into_iter().map(|d| doc::project(d, p)).collect();
                }
                first_batch(cursors, &ns, docs, cmd)
            }
            "aggregate" => {
                let docs = db.doc_aggregate(coll, list(cmd, "pipeline"))?;
                first_batch(cursors, &ns, docs, cmd)
            }
            "count" => {
                let n = db
                    .doc_find(
                        coll,
                        cmd.get("query").unwrap_or(&empty),
                        &FindOptions::default(),
                    )?
                    .len();
                ok(vec![("n", Value::Int(n as i64))])
            }
            "distinct" => {
                let key = cmd.get("key").and_then(Value::as_str).unwrap_or("");
                let mut values: Vec<Value> = Vec::new();
                for d in db.doc_find(
                    coll,
                    cmd.get("query").unwrap_or(&empty),
                    &FindOptions::default(),
                )? {
                    let found = match doc::get_path(&d, key) {
                        Some(Value::Array(a)) => a.clone(),
                        Some(v) => vec![v.clone()],
                        None => vec![],
                    };
                    for v in found {
                        if !values
                            .iter()
                            .any(|x| x.total_cmp(&v) == std::cmp::Ordering::Equal)
                        {
                            values.push(v);
                        }
                    }
                }
                values.sort_by(|a, b| a.total_cmp(b));
                ok(vec![("values", Value::Array(values))])
            }
            "update" => {
                let (mut n, mut errors, mut upserted) = (0i64, Vec::new(), Vec::new());
                for (i, u) in list(cmd, "updates").iter().enumerate() {
                    let res = match u.get("u") {
                        Some(upd @ Value::Object(_)) => db.doc_update_with(
                            coll,
                            u.get("q").unwrap_or(&empty),
                            upd,
                            truthy(u.get("multi")),
                            truthy(u.get("upsert")),
                        ),
                        _ => Err(Error::Parse(
                            "pipeline-style updates are not supported".into(),
                        )),
                    };
                    match res {
                        Ok((changed, new_id)) => {
                            n += changed as i64;
                            if let Some(id) = new_id {
                                n += 1;
                                upserted
                                    .push(obj(vec![("index", Value::Int(i as i64)), ("_id", id)]));
                            }
                        }
                        Err(e) => {
                            errors.push(write_error(i, &e));
                            if cmd.get("ordered") != Some(&Value::Bool(false)) {
                                break;
                            }
                        }
                    }
                }
                let modified = n - upserted.len() as i64;
                let mut reply = vec![("n", Value::Int(n)), ("nModified", Value::Int(modified))];
                if !upserted.is_empty() {
                    reply.push(("upserted", Value::Array(upserted)));
                }
                if !errors.is_empty() {
                    reply.push(("writeErrors", Value::Array(errors)));
                }
                ok(reply)
            }
            "delete" => {
                let (mut n, mut errors) = (0i64, Vec::new());
                for (i, d) in list(cmd, "deletes").iter().enumerate() {
                    let limit = count_of(d.get("limit")).map(|l| l as u64);
                    match db.doc_delete_with(coll, d.get("q").unwrap_or(&empty), limit) {
                        Ok(k) => n += k as i64,
                        Err(e) => {
                            errors.push(write_error(i, &e));
                            if cmd.get("ordered") != Some(&Value::Bool(false)) {
                                break;
                            }
                        }
                    }
                }
                let mut reply = vec![("n", Value::Int(n))];
                if !errors.is_empty() {
                    reply.push(("writeErrors", Value::Array(errors)));
                }
                ok(reply)
            }
            "drop" => {
                if db.doc_drop(coll)? {
                    ok(vec![
                        ("ns", ns.as_str().into()),
                        ("nIndexesWas", Value::Int(1)),
                    ])
                } else {
                    fail(26, "NamespaceNotFound", "ns not found")
                }
            }
            "create" => ok(vec![]),
            "listcollections" => {
                let batch = db
                    .doc_collections()?
                    .iter()
                    .filter_map(|c| {
                        let (d, name) = cursors.split(c);
                        (d == dbname).then(|| name.to_string())
                    })
                    .map(|c| {
                        obj(vec![
                            ("name", c.into()),
                            ("type", "collection".into()),
                            ("options", Value::Object(BTreeMap::new())),
                            ("info", obj(vec![("readOnly", false.into())])),
                        ])
                    })
                    .collect();
                cursor(&format!("{dbname}.$cmd.listCollections"), batch)
            }
            "listdatabases" => {
                let mut names: Vec<String> = db
                    .doc_collections()?
                    .iter()
                    .map(|c| cursors.split(c).0.to_string())
                    .collect();
                names.push(cursors.default_db.clone());
                names.sort();
                names.dedup();
                let dbs = names
                    .into_iter()
                    .map(|n| {
                        obj(vec![
                            ("name", n.into()),
                            ("sizeOnDisk", Value::Int(0)),
                            ("empty", false.into()),
                        ])
                    })
                    .collect();
                ok(vec![
                    ("databases", Value::Array(dbs)),
                    ("totalSize", Value::Int(0)),
                ])
            }
            "createindexes" => {
                let before = db.doc_indexes(coll)?.len() as i64 + 1;
                let mut created = 0;
                for ix in list(cmd, "indexes") {
                    let fields: Vec<(String, i64)> = match ix.get("key") {
                        Some(k @ (Value::Object(_) | Value::Array(_))) => doc::sort_spec(k)?
                            .into_iter()
                            .map(|(f, desc)| (f, if desc { -1 } else { 1 }))
                            .collect(),
                        _ => return Err(Error::Parse("each index needs a key document".into())),
                    };
                    let unique = truthy(ix.get("unique"));
                    let iname = ix.get("name").and_then(Value::as_str).map(String::from);
                    created += db.doc_create_index_with(coll, fields, unique, iname)? as i64;
                }
                ok(vec![
                    ("createdCollectionAutomatically", false.into()),
                    ("numIndexesBefore", Value::Int(before)),
                    ("numIndexesAfter", Value::Int(before + created)),
                ])
            }
            "listindexes" => {
                let ixs = db.doc_indexes(coll)?;
                let batch = crate::index_docs(&ixs);
                first_batch(
                    cursors,
                    &format!("{dbname}.$cmd.listIndexes.{short}"),
                    batch,
                    cmd,
                )
            }
            "dropindexes" => {
                let ixs = db.doc_indexes(coll)?;
                let before = ixs.len() as i64 + 1;
                match cmd.get("index") {
                    Some(Value::Text(n)) if n == "*" => {
                        for ix in ixs {
                            db.doc_drop_index(coll, &ix.name)?;
                        }
                    }
                    Some(Value::Text(n)) => {
                        if !db.doc_drop_index(coll, n)? {
                            return Ok(fail(
                                27,
                                "IndexNotFound",
                                &format!("index not found with name [{n}]"),
                            ));
                        }
                    }
                    Some(k @ (Value::Object(_) | Value::Array(_))) => {
                        let want: Vec<(String, i64)> = doc::sort_spec(k)?
                            .into_iter()
                            .map(|(f, d)| (f, if d { -1 } else { 1 }))
                            .collect();
                        match ixs.iter().find(|i| i.fields == want) {
                            Some(ix) => {
                                db.doc_drop_index(coll, &ix.name)?;
                            }
                            None => {
                                return Ok(fail(27, "IndexNotFound", "can't find index with key"))
                            }
                        }
                    }
                    _ => {
                        return Err(Error::Parse(
                            "dropIndexes needs an index name, key or \"*\"".into(),
                        ))
                    }
                }
                ok(vec![("nIndexesWas", Value::Int(before))])
            }
            _ => fail(59, "CommandNotFound", &format!("no such command: '{name}'")),
        })
    })();
    res.unwrap_or_else(|e| fail_with(&e))
}

// ----------------------------------------------------------------------- wire

fn send(
    s: &mut TcpStream,
    request_id: i32,
    response_to: i32,
    opcode: i32,
    body: &[u8],
) -> io::Result<()> {
    let mut out = Vec::with_capacity(16 + body.len());
    out.extend_from_slice(&(16 + body.len() as i32).to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&response_to.to_le_bytes());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(body);
    s.write_all(&out)
}

fn session(s: &mut TcpStream, db: &mut Database, limits: &Limits) -> io::Result<()> {
    let conn_id = s.peer_addr().map(|a| a.port() as i64).unwrap_or(0);
    let mut cursors = Cursors::new(&limits.mongo_default_db);
    let mut reply_id = 0i32;
    loop {
        let mut hdr = [0u8; 16];
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let len = rd_i32(&hdr, 0)? as usize;
        let request_id = rd_i32(&hdr, 4)?;
        let opcode = rd_i32(&hdr, 12)?;
        if !(16..=limits.max_request_bytes.min(48_000_000).max(16)).contains(&len) {
            return Ok(());
        }
        let mut body = vec![0u8; len - 16];
        s.read_exact(&mut body)?;
        reply_id = reply_id.wrapping_add(1);
        match opcode {
            // OP_MSG
            2013 => {
                let flags = rd_i32(&body, 0)? as u32;
                let end = body
                    .len()
                    .saturating_sub(if flags & 1 != 0 { 4 } else { 0 });
                let mut pos = 4;
                let mut cmd: Option<(BTreeMap<String, Value>, String)> = None;
                let mut sequences: Vec<(String, Vec<Value>)> = Vec::new();
                while pos < end {
                    let kind = body[pos];
                    pos += 1;
                    match kind {
                        0 => {
                            let (doc, first, used) = decode_doc(&body[pos..end])?;
                            if let Value::Object(mut o) = doc {
                                restore_key_order(&mut o, &body[pos..pos + used]);
                                cmd = Some((o, first.unwrap_or_default()));
                            }
                            pos += used;
                        }
                        1 => {
                            let size = rd_i32(&body, pos)? as usize;
                            let sec_end = pos + size;
                            if size < 5 || sec_end > end {
                                return Err(bad("bad document sequence"));
                            }
                            let (ident, n) = rd_cstr(&body, pos + 4)?;
                            let mut p = pos + 4 + n;
                            let mut docs = Vec::new();
                            while p < sec_end {
                                let (d, _, used) = decode_doc(&body[p..sec_end])?;
                                docs.push(d);
                                p += used;
                            }
                            sequences.push((ident, docs));
                            pos = sec_end;
                        }
                        _ => return Err(bad("unknown OP_MSG section")),
                    }
                }
                let Some((mut doc, mut name)) = cmd else {
                    return Err(bad("OP_MSG without a command"));
                };
                if name.starts_with('$') {
                    // the command is the first field; tolerate clients that put `$db` first
                    if let Some(k) = doc.keys().find(|k| {
                        !k.starts_with('$') && COMMANDS.contains(&k.to_ascii_lowercase().as_str())
                    }) {
                        name = k.clone();
                    }
                }
                for (ident, docs) in sequences {
                    doc.insert(ident, Value::Array(docs));
                }
                let reply = command(db, &mut cursors, &name, &Value::Object(doc), conn_id);
                if flags & 2 == 0 {
                    // not "moreToCome": the client expects an answer
                    let mut out = vec![0, 0, 0, 0, 0];
                    out.extend(encode_doc(&reply));
                    send(s, reply_id, request_id, 2013, &out)?;
                }
            }
            // OP_QUERY: only used by drivers for the initial handshake
            2004 => {
                let (_, n) = rd_cstr(&body, 4)?;
                let (doc, first, _) = decode_doc(
                    body.get(4 + n + 8..)
                        .ok_or_else(|| bad("truncated OP_QUERY"))?,
                )?;
                let reply = command(db, &mut cursors, &first.unwrap_or_default(), &doc, conn_id);
                let mut out = Vec::new();
                out.extend_from_slice(&8i32.to_le_bytes()); // AwaitCapable
                out.extend_from_slice(&0i64.to_le_bytes()); // cursor id
                out.extend_from_slice(&0i32.to_le_bytes()); // starting from
                out.extend_from_slice(&1i32.to_le_bytes()); // number returned
                out.extend(encode_doc(&reply));
                send(s, reply_id, request_id, 1, &out)?;
            }
            _ => return Ok(()),
        }
    }
}

pub fn handle(stream: TcpStream, db: Database) {
    handle_with(stream, db, &Limits::default())
}

pub fn handle_with(mut stream: TcpStream, mut db: Database, limits: &Limits) {
    let _ = session(&mut stream, &mut db, limits);
}
