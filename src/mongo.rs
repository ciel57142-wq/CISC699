//! MongoDB wire protocol front end for the document model.
//!
//! Speaks OP_MSG (and the legacy OP_QUERY handshake) with a BSON codec, so
//! MongoDB drivers can connect. Supported commands: hello / isMaster, ping,
//! buildInfo, insert, find, update (multi, upsert), delete, count,
//! aggregate (`$match $sort $skip $limit $count`, and the `$group` form
//! drivers use for countDocuments), distinct, drop, listCollections,
//! listDatabases, createIndexes (single field).
//!
//! Differences from MongoDB: there is one database (the `$db` name is
//! ignored), `find` returns its whole result in the first batch, field
//! order inside documents is not preserved, and there is no authentication.
//! ObjectIds and dates are kept as `{"$oid": ...}` / `{"$date": ...}`.

use crate::doc::{self, FindOptions};
use crate::error::Error;
use crate::value::Value;
use crate::Database;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::time::{SystemTime, UNIX_EPOCH};

// ----------------------------------------------------------------------- BSON

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("BSON: {msg}"))
}

fn rd_i32(b: &[u8], pos: usize) -> io::Result<i32> {
    b.get(pos..pos + 4).map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated"))
}

fn rd_i64(b: &[u8], pos: usize) -> io::Result<i64> {
    let s = b.get(pos..pos + 8).ok_or_else(|| bad("truncated"))?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(i64::from_le_bytes(a))
}

fn rd_cstr(b: &[u8], pos: usize) -> io::Result<(String, usize)> {
    let rest = b.get(pos..).ok_or_else(|| bad("truncated"))?;
    let end = rest.iter().position(|c| *c == 0).ok_or_else(|| bad("unterminated name"))?;
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
        let need = |len: usize| if pos + len <= total { Ok(()) } else { Err(bad("truncated value")) };
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
                let v = Value::Text(String::from_utf8_lossy(&b[pos + 4..pos + 4 + len - 1]).into_owned());
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
                    Some((k, Value::Text(h))) if k == "$oid" && h.len() == 24 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                        head(0x07, out);
                        out.extend((0..12).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)));
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
                    Some((k, Value::Text(h))) if k == "$binary" && h.len() % 2 == 0 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                        head(0x05, out);
                        out.extend_from_slice(&(h.len() as i32 / 2).to_le_bytes());
                        out.push(0);
                        out.extend((0..h.len() / 2).map(|i| u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(0)));
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
    obj(vec![("index", Value::Int(index as i64)), ("code", Value::Int(error_code(e).0)), ("errmsg", e.to_string().into())])
}

fn cursor(ns: &str, batch: Vec<Value>) -> Value {
    ok(vec![("cursor", obj(vec![("firstBatch", Value::Array(batch)), ("id", long(0)), ("ns", ns.into())]))])
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(v) => v.as_f64().map_or(false, |n| n != 0.0),
        None => false,
    }
}

fn count_of(v: Option<&Value>) -> Option<usize> {
    v.and_then(Value::as_f64).filter(|n| *n > 0.0).map(|n| n as usize)
}

fn list<'a>(cmd: &'a Value, key: &str) -> &'a [Value] {
    match cmd.get(key) {
        Some(Value::Array(a)) => a,
        _ => &[],
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn aggregate(db: &mut Database, coll: &str, pipeline: &[Value]) -> Result<Vec<Value>, Error> {
    let empty = Value::Object(BTreeMap::new());
    let mut docs: Option<Vec<Value>> = None;
    for (i, stage) in pipeline.iter().enumerate() {
        let Value::Object(st) = stage else {
            return Err(Error::Parse("pipeline stages must be objects".into()));
        };
        let Some((name, arg)) = st.iter().next() else {
            continue;
        };
        if name == "$match" && i == 0 {
            docs = Some(db.doc_find(coll, arg, &FindOptions::default())?);
            continue;
        }
        let mut cur = match docs.take() {
            Some(d) => d,
            None => db.doc_find(coll, &empty, &FindOptions::default())?,
        };
        match name.as_str() {
            "$match" => {
                let mut kept = Vec::new();
                for d in cur {
                    if doc::matches(&d, arg)? {
                        kept.push(d);
                    }
                }
                cur = kept;
            }
            "$sort" => {
                let Value::Object(spec) = arg else {
                    return Err(Error::Type("$sort expects an object".into()));
                };
                cur.sort_by(|a, b| {
                    for (path, dir) in spec {
                        let (x, y) =
                            (doc::get_path(a, path).unwrap_or(&Value::Null), doc::get_path(b, path).unwrap_or(&Value::Null));
                        let o = x.total_cmp(y);
                        if o != std::cmp::Ordering::Equal {
                            return if dir.as_f64().map_or(false, |n| n < 0.0) { o.reverse() } else { o };
                        }
                    }
                    std::cmp::Ordering::Equal
                });
            }
            "$skip" => cur = cur.into_iter().skip(count_of(Some(arg)).unwrap_or(0)).collect(),
            "$limit" => cur.truncate(count_of(Some(arg)).unwrap_or(0)),
            "$count" => {
                let field = arg.as_str().unwrap_or("count");
                cur = if cur.is_empty() { vec![] } else { vec![obj(vec![(field, Value::Int(cur.len() as i64))])] };
            }
            "$group" => {
                // Only the whole-set count that drivers send for countDocuments:
                // {$group: {_id: <constant>, n: {$sum: 1}}}
                let constant_id = !matches!(arg.get("_id"), Some(Value::Text(s)) if s.starts_with('$'));
                let counters: Vec<&String> = match arg {
                    Value::Object(g) => g
                        .iter()
                        .filter(|(k, v)| *k != "_id" && v.get("$sum").and_then(Value::as_f64) == Some(1.0))
                        .map(|(k, _)| k)
                        .collect(),
                    _ => vec![],
                };
                let fields = match arg {
                    Value::Object(g) => g.len(),
                    _ => 0,
                };
                if !constant_id || counters.len() + 1 != fields {
                    return Err(Error::Parse("$group is only supported for counting ({_id: 1, n: {$sum: 1}})".into()));
                }
                cur = if cur.is_empty() {
                    vec![]
                } else {
                    let mut g = BTreeMap::new();
                    g.insert("_id".to_string(), arg.get("_id").cloned().unwrap_or(Value::Null));
                    for c in counters {
                        g.insert(c.clone(), Value::Int(cur.len() as i64));
                    }
                    vec![Value::Object(g)]
                };
            }
            other => return Err(Error::Parse(format!("unsupported aggregation stage {other}"))),
        }
        docs = Some(cur);
    }
    match docs {
        Some(d) => Ok(d),
        None => db.doc_find(coll, &empty, &FindOptions::default()),
    }
}

/// Execute one database command and build its reply document.
fn command(db: &mut Database, name: &str, cmd: &Value, conn_id: i64) -> Value {
    let empty = Value::Object(BTreeMap::new());
    let coll = cmd.get(name).and_then(Value::as_str).unwrap_or("");
    let ns = format!("polydb.{coll}");
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
            "ping" | "endsessions" | "killcursors" | "abortsessions" => ok(vec![]),
            "buildinfo" => ok(vec![
                ("version", "6.0.0".into()),
                ("versionArray", Value::Array(vec![Value::Int(6), Value::Int(0), Value::Int(0), Value::Int(0)])),
                ("gitVersion", format!("polydb-{}", env!("CARGO_PKG_VERSION")).into()),
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
                    sort: cmd.get("sort").filter(|s| matches!(s, Value::Object(o) if !o.is_empty())).cloned(),
                    skip: count_of(cmd.get("skip")).unwrap_or(0),
                    limit: count_of(cmd.get("limit")),
                };
                let mut docs = db.doc_find(coll, cmd.get("filter").unwrap_or(&empty), &opts)?;
                if let Some(p) = cmd.get("projection") {
                    docs = docs.into_iter().map(|d| doc::project(d, p)).collect();
                }
                cursor(&ns, docs)
            }
            "aggregate" => cursor(&ns, aggregate(db, coll, list(cmd, "pipeline"))?),
            "count" => {
                let n = db.doc_find(coll, cmd.get("query").unwrap_or(&empty), &FindOptions::default())?.len();
                ok(vec![("n", Value::Int(n as i64))])
            }
            "distinct" => {
                let key = cmd.get("key").and_then(Value::as_str).unwrap_or("");
                let mut values: Vec<Value> = Vec::new();
                for d in db.doc_find(coll, cmd.get("query").unwrap_or(&empty), &FindOptions::default())? {
                    let found = match doc::get_path(&d, key) {
                        Some(Value::Array(a)) => a.clone(),
                        Some(v) => vec![v.clone()],
                        None => vec![],
                    };
                    for v in found {
                        if !values.iter().any(|x| x.total_cmp(&v) == std::cmp::Ordering::Equal) {
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
                        _ => Err(Error::Parse("pipeline-style updates are not supported".into())),
                    };
                    match res {
                        Ok((changed, new_id)) => {
                            n += changed as i64;
                            if let Some(id) = new_id {
                                n += 1;
                                upserted.push(obj(vec![("index", Value::Int(i as i64)), ("_id", id)]));
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
                    ok(vec![("ns", ns.as_str().into()), ("nIndexesWas", Value::Int(1))])
                } else {
                    fail(26, "NamespaceNotFound", "ns not found")
                }
            }
            "create" => ok(vec![]),
            "listcollections" => {
                let batch = db
                    .doc_collections()?
                    .into_iter()
                    .map(|c| {
                        obj(vec![
                            ("name", c.into()),
                            ("type", "collection".into()),
                            ("options", Value::Object(BTreeMap::new())),
                            ("info", obj(vec![("readOnly", false.into())])),
                        ])
                    })
                    .collect();
                cursor("polydb.$cmd.listCollections", batch)
            }
            "listdatabases" => ok(vec![(
                "databases",
                Value::Array(vec![obj(vec![("name", "polydb".into()), ("sizeOnDisk", Value::Int(0)), ("empty", false.into())])]),
            )]),
            "createindexes" => {
                let mut created = 0;
                for ix in list(cmd, "indexes") {
                    match ix.get("key") {
                        Some(Value::Object(k)) if k.len() == 1 => {
                            created += db.doc_create_index(coll, k.keys().next().map(String::as_str).unwrap_or(""))? as i64;
                        }
                        _ => return Err(Error::Parse("only single-field indexes are supported".into())),
                    }
                }
                ok(vec![("numIndexesBefore", Value::Int(1)), ("numIndexesAfter", Value::Int(1 + created))])
            }
            _ => fail(59, "CommandNotFound", &format!("no such command: '{name}'")),
        })
    })();
    res.unwrap_or_else(|e| fail_with(&e))
}

// ----------------------------------------------------------------------- wire

fn send(s: &mut TcpStream, request_id: i32, response_to: i32, opcode: i32, body: &[u8]) -> io::Result<()> {
    let mut out = Vec::with_capacity(16 + body.len());
    out.extend_from_slice(&(16 + body.len() as i32).to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&response_to.to_le_bytes());
    out.extend_from_slice(&opcode.to_le_bytes());
    out.extend_from_slice(body);
    s.write_all(&out)
}

fn session(s: &mut TcpStream, db: &mut Database) -> io::Result<()> {
    let conn_id = s.peer_addr().map(|a| a.port() as i64).unwrap_or(0);
    let mut reply_id = 0i32;
    loop {
        let mut hdr = [0u8; 16];
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let len = rd_i32(&hdr, 0)? as usize;
        let request_id = rd_i32(&hdr, 4)?;
        let opcode = rd_i32(&hdr, 12)?;
        if !(16..=48_000_000).contains(&len) {
            return Ok(());
        }
        let mut body = vec![0u8; len - 16];
        s.read_exact(&mut body)?;
        reply_id = reply_id.wrapping_add(1);
        match opcode {
            // OP_MSG
            2013 => {
                let flags = rd_i32(&body, 0)? as u32;
                let end = body.len().saturating_sub(if flags & 1 != 0 { 4 } else { 0 });
                let mut pos = 4;
                let mut cmd: Option<(BTreeMap<String, Value>, String)> = None;
                let mut sequences: Vec<(String, Vec<Value>)> = Vec::new();
                while pos < end {
                    let kind = body[pos];
                    pos += 1;
                    match kind {
                        0 => {
                            let (doc, first, used) = decode_doc(&body[pos..end])?;
                            pos += used;
                            if let Value::Object(o) = doc {
                                cmd = Some((o, first.unwrap_or_default()));
                            }
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
                let Some((mut doc, name)) = cmd else {
                    return Err(bad("OP_MSG without a command"));
                };
                for (ident, docs) in sequences {
                    doc.insert(ident, Value::Array(docs));
                }
                let reply = command(db, &name, &Value::Object(doc), conn_id);
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
                let (doc, first, _) = decode_doc(body.get(4 + n + 8..).ok_or_else(|| bad("truncated OP_QUERY"))?)?;
                let reply = command(db, &first.unwrap_or_default(), &doc, conn_id);
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

pub fn handle(mut stream: TcpStream, mut db: Database) {
    let _ = session(&mut stream, &mut db);
}
