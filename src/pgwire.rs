//! PostgreSQL wire protocol (v3) front end.
//!
//! Implements the startup handshake and the *simple query* flow, which is
//! what `psql`, libpq's `PQexec` and psycopg2 use. Every polydb command
//! works here, not just SQL: document and key-value results come back as
//! ordinary result sets.
//!
//! The extended protocol is supported too: Parse / Bind / Describe /
//! Execute (with row limits and PortalSuspended) / Close / Sync / Flush,
//! named and unnamed statements and portals, `$n` parameters in text or
//! binary format, binary results, and CancelRequest. Parameters sent
//! without a type take the type of the column they meet.
//!
//! Not implemented: COPY, authentication and TLS. The SQL dialect is
//! polydb's, so tools that inspect `pg_catalog` will not work.

use crate::error::Error;
use crate::pbsql::parser::Param;
use crate::server::Limits;
use crate::value::Value;
use crate::{Database, Output};
use std::io::{self, Read, Write};
use std::net::TcpStream;

fn msg(out: &mut Vec<u8>, tag: u8, body: &[u8]) {
    out.push(tag);
    out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
    out.extend_from_slice(body);
}

fn ready(out: &mut Vec<u8>, db: &Database) {
    msg(out, b'Z', &[if db.in_transaction() { b'T' } else { b'I' }]);
}

fn sqlstate(e: &Error) -> &'static str {
    match e {
        Error::Parse(_) => "42601",
        Error::Schema(_) => "42P01",
        Error::Constraint(_) => "23505",
        Error::Type(_) => "22023",
        Error::Txn(_) => "25000",
        Error::Conflict(_) => "40001",
        Error::Cancelled(_) => "57014",
        Error::Io(_) | Error::Corrupt(_) | Error::Storage(_) => "XX000",
    }
}

fn error(out: &mut Vec<u8>, code: &str, message: &str) {
    let mut body = Vec::new();
    for (field, text) in [
        (b'S', "ERROR"),
        (b'V', "ERROR"),
        (b'C', code),
        (b'M', message),
    ] {
        body.push(field);
        body.extend_from_slice(text.as_bytes());
        body.push(0);
    }
    body.push(0);
    msg(out, b'E', &body);
}

fn complete(out: &mut Vec<u8>, tag: &str) {
    let mut body = tag.as_bytes().to_vec();
    body.push(0);
    msg(out, b'C', &body);
}

/// Text-format cell and the type OID that describes it.
fn cell(v: &Value) -> (Option<String>, i32) {
    match v {
        Value::Null => (None, 25),
        Value::Bool(b) => (Some(if *b { "t" } else { "f" }.to_string()), 16),
        Value::Int(i) => (Some(i.to_string()), 20),
        Value::Float(f) => (Some(f.to_string()), 701),
        Value::Text(s) => (Some(s.clone()), 25),
        other => (Some(other.to_json()), 25),
    }
}

fn result_set(out: &mut Vec<u8>, columns: &[String], rows: &[Vec<Value>]) {
    let mut body = (columns.len() as i16).to_be_bytes().to_vec();
    for (i, name) in columns.iter().enumerate() {
        let oid = rows
            .iter()
            .map(|r| cell(&r[i]))
            .find(|c| c.0.is_some())
            .map_or(25, |c| c.1);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes()); // table oid
        body.extend_from_slice(&0i16.to_be_bytes()); // column number
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes()); // type length
        body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
        body.extend_from_slice(&0i16.to_be_bytes()); // text format
    }
    msg(out, b'T', &body);
    for r in rows {
        let mut body = (r.len() as i16).to_be_bytes().to_vec();
        for v in r {
            match cell(v).0 {
                None => body.extend_from_slice(&(-1i32).to_be_bytes()),
                Some(s) => {
                    body.extend_from_slice(&(s.len() as i32).to_be_bytes());
                    body.extend_from_slice(s.as_bytes());
                }
            }
        }
        msg(out, b'D', &body);
    }
    complete(out, &format!("SELECT {}", rows.len()));
}

/// Split a query string on `;` outside quotes.
fn split_statements(q: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    let mut escaped = false;
    for (i, c) in q.char_indices() {
        match quote {
            Some(qc) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' && qc == '"' {
                    escaped = true;
                } else if c == qc {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                ';' => {
                    out.push(&q[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&q[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

const SESSION_SETTINGS: &[&str] = &[
    "client_encoding",
    "datestyle",
    "extra_float_digits",
    "application_name",
    "search_path",
    "session",
    "statement_timeout",
    "timezone",
    "time",
    "standard_conforming_strings",
    "names",
    "client_min_messages",
    "intervalstyle",
    "bytea_output",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "transaction",
];

/// Run one command (with bound parameters for the extended protocol) and
/// return its output together with its CommandComplete tag.
fn run_command(db: &mut Database, stmt: &str, params: &[Param]) -> Result<(Output, String), Error> {
    let mut words = stmt.split_whitespace().map(str::to_ascii_lowercase);
    let first = words.next().unwrap_or_default();
    let second = words.next().unwrap_or_default();
    let second = second.trim_end_matches(['=', ';']).to_string();

    // PostgreSQL session chatter that drivers send and that has no meaning here.
    let noop = match first.as_str() {
        "set" if SESSION_SETTINGS.contains(&second.as_str()) => Some("SET"),
        "reset" | "discard" | "deallocate" | "listen" | "unlisten" => Some("OK"),
        // PostgreSQL only warns when these have no transaction to act on.
        "begin" | "start" if db.in_transaction() => Some("BEGIN"),
        "commit" | "end" if !db.in_transaction() => Some("COMMIT"),
        "rollback" | "abort" if !db.in_transaction() => Some("ROLLBACK"),
        _ => None,
    };
    if let Some(tag) = noop {
        return Ok((Output::Done(String::new()), tag.to_string()));
    }
    let out = match first.as_str() {
        "start" => db.begin().map(|_| Output::Done(String::new())),
        "end" => db.commit().map(|_| Output::Done(String::new())),
        "abort" => db.rollback().map(|_| Output::Done(String::new())),
        _ => db.execute_params(stmt, params),
    }?;
    let tag = match &out {
        Output::Done(m) => {
            let n = m
                .split_whitespace()
                .next()
                .and_then(|w| w.parse::<u64>().ok())
                .unwrap_or(0);
            match (first.as_str(), second.as_str()) {
                ("insert", _) => format!("INSERT 0 {n}"),
                ("update", _) => format!("UPDATE {n}"),
                ("delete", _) => format!("DELETE {n}"),
                ("create", "index" | "unique") => "CREATE INDEX".into(),
                ("alter", _) => "ALTER TABLE".into(),
                ("create", _) => "CREATE TABLE".into(),
                ("drop", "index") => "DROP INDEX".into(),
                ("drop", _) => "DROP TABLE".into(),
                ("begin" | "start", _) => "BEGIN".into(),
                ("commit" | "end", _) => "COMMIT".into(),
                ("rollback" | "abort", _) => "ROLLBACK".into(),
                _ => "OK".into(),
            }
        }
        other => format!("SELECT {}", rows_of(other).1.len()),
    };
    Ok((out, tag))
}

/// Columns and rows of a result (documents and scalars become one-column rows).
fn rows_of(out: &Output) -> (Vec<String>, Vec<Vec<Value>>) {
    match out {
        Output::Rows { columns, rows } => (columns.clone(), rows.clone()),
        Output::Docs(docs) => (
            vec!["document".into()],
            docs.iter()
                .map(|d| vec![Value::Text(d.to_json())])
                .collect(),
        ),
        Output::Scalar(v) => (vec!["value".into()], vec![vec![v.clone()]]),
        Output::Done(_) => (vec![], vec![]),
    }
}

/// Run one statement of a simple query; returns false if it failed.
fn run_statement(db: &mut Database, stmt: &str, out: &mut Vec<u8>) -> bool {
    match run_command(db, stmt, &[]) {
        Err(e) => {
            error(out, sqlstate(&e), &e.to_string());
            false
        }
        Ok((Output::Done(_), tag)) => {
            complete(out, &tag);
            true
        }
        Ok((o, _)) => {
            let (columns, rows) = rows_of(&o);
            result_set(out, &columns, &rows);
            true
        }
    }
}

// ------------------------------------------------------ extended protocol

/// A statement prepared with Parse.
struct Prepared {
    query: String,
    /// Parameter type OIDs (0 = unspecified, resolved from context).
    types: Vec<i32>,
    nparams: usize,
}

/// A portal created by Bind: a statement with values, run on first use.
struct Portal {
    query: String,
    params: Vec<Param>,
    formats: Vec<i16>,
    result: Option<(Vec<String>, Vec<Vec<Value>>, String, bool)>,
    sent: usize,
}

fn cstr(b: &[u8], pos: &mut usize) -> io::Result<String> {
    let rest = b.get(*pos..).ok_or_else(|| bad_msg("truncated message"))?;
    let end = rest
        .iter()
        .position(|c| *c == 0)
        .ok_or_else(|| bad_msg("unterminated string"))?;
    let s = String::from_utf8_lossy(&rest[..end]).into_owned();
    *pos += end + 1;
    Ok(s)
}

fn i16_at(b: &[u8], pos: &mut usize) -> io::Result<i16> {
    let s = b
        .get(*pos..*pos + 2)
        .ok_or_else(|| bad_msg("truncated message"))?;
    *pos += 2;
    Ok(i16::from_be_bytes([s[0], s[1]]))
}

fn i32_at(b: &[u8], pos: &mut usize) -> io::Result<i32> {
    let s = b
        .get(*pos..*pos + 4)
        .ok_or_else(|| bad_msg("truncated message"))?;
    *pos += 4;
    Ok(i32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn bad_msg(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

/// Decode one bound parameter from its type OID and format (0 text, 1 binary).
fn decode_param(oid: i32, format: i16, raw: Option<&[u8]>) -> Result<Param, Error> {
    let Some(raw) = raw else {
        return Ok(Param::Typed(Value::Null));
    };
    let bad = |what: &str| Error::Type(format!("invalid {what} parameter value"));
    if format == 1 {
        let n = |k: usize| raw.len() == k;
        return Ok(Param::Typed(match oid {
            16 if n(1) => Value::Bool(raw[0] != 0),
            21 if n(2) => Value::Int(i16::from_be_bytes([raw[0], raw[1]]) as i64),
            23 if n(4) => {
                Value::Int(i32::from_be_bytes(raw.try_into().map_err(|_| bad("int4"))?) as i64)
            }
            20 if n(8) => Value::Int(i64::from_be_bytes(raw.try_into().map_err(|_| bad("int8"))?)),
            700 if n(4) => {
                Value::Float(f32::from_be_bytes(raw.try_into().map_err(|_| bad("float4"))?) as f64)
            }
            701 if n(8) => Value::Float(f64::from_be_bytes(
                raw.try_into().map_err(|_| bad("float8"))?,
            )),
            25 | 1043 | 19 | 18 | 0 | 114 => {
                let t = String::from_utf8(raw.to_vec()).map_err(|_| bad("text"))?;
                if oid == 114 {
                    Value::parse(&t).map_err(|_| bad("json"))?
                } else {
                    Value::Text(t)
                }
            }
            16 | 20 | 21 | 23 | 700 | 701 => return Err(bad("binary")),
            other => {
                return Err(Error::Type(format!(
                    "binary parameters of type OID {other} are not supported; send them as text"
                )))
            }
        }));
    }
    let t = std::str::from_utf8(raw)
        .map_err(|_| bad("text"))?
        .to_string();
    Ok(match oid {
        0 | 705 => Param::Untyped(t),
        16 => Param::Typed(match t.as_str() {
            "t" | "true" | "1" | "TRUE" | "y" | "yes" | "on" => Value::Bool(true),
            "f" | "false" | "0" | "FALSE" | "n" | "no" | "off" => Value::Bool(false),
            _ => return Err(bad("boolean")),
        }),
        20 | 21 | 23 | 26 => {
            Param::Typed(Value::Int(t.trim().parse().map_err(|_| bad("integer"))?))
        }
        700 | 701 => Param::Typed(Value::Float(t.trim().parse().map_err(|_| bad("float"))?)),
        1700 => Param::Typed(match t.trim().parse::<i64>() {
            Ok(i) => Value::Int(i),
            Err(_) => Value::Float(t.trim().parse().map_err(|_| bad("numeric"))?),
        }),
        114 | 3802 => Param::Typed(Value::parse(&t).map_err(|_| bad("json"))?),
        _ => Param::Typed(Value::Text(t)),
    })
}

/// Type OID and binary form of a result value.
fn binary_cell(v: &Value) -> (i32, Option<Vec<u8>>) {
    match v {
        Value::Null => (25, None),
        Value::Bool(b) => (16, Some(vec![*b as u8])),
        Value::Int(i) => (20, Some(i.to_be_bytes().to_vec())),
        Value::Float(f) => (701, Some(f.to_be_bytes().to_vec())),
        Value::Text(s) => (25, Some(s.as_bytes().to_vec())),
        other => (25, Some(other.to_json().into_bytes())),
    }
}

fn format_of(formats: &[i16], col: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(col).copied().unwrap_or(0),
    }
}

fn row_description(out: &mut Vec<u8>, columns: &[String], rows: &[Vec<Value>], formats: &[i16]) {
    let mut body = (columns.len() as i16).to_be_bytes().to_vec();
    for (i, name) in columns.iter().enumerate() {
        let oid = rows
            .iter()
            .filter_map(|r| r.get(i))
            .map(cell)
            .find(|c| c.0.is_some())
            .map_or(25, |c| c.1);
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&format_of(formats, i).to_be_bytes());
    }
    msg(out, b'T', &body);
}

/// RowDescription from static types (Describe of a statement, before it runs).
fn typed_row_description(out: &mut Vec<u8>, cols: &[(String, Option<crate::pbsql::ast::Ty>)]) {
    use crate::pbsql::ast::Ty;
    let mut body = (cols.len() as i16).to_be_bytes().to_vec();
    for (name, ty) in cols {
        let oid: i32 = match ty {
            Some(Ty::Int) => 20,
            Some(Ty::Float) => 701,
            Some(Ty::Bool) => 16,
            Some(Ty::Date) => 1082,
            Some(Ty::Time) => 1083,
            Some(Ty::Timestamp) => 1114,
            Some(Ty::Json) => 114,
            Some(Ty::Text) | None => 25,
        };
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(&0i32.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&oid.to_be_bytes());
        body.extend_from_slice(&(-1i16).to_be_bytes());
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
    }
    msg(out, b'T', &body);
}

fn data_row(out: &mut Vec<u8>, r: &[Value], formats: &[i16]) {
    let mut body = (r.len() as i16).to_be_bytes().to_vec();
    for (i, v) in r.iter().enumerate() {
        let bytes = if format_of(formats, i) == 1 {
            binary_cell(v).1
        } else {
            cell(v).0.map(String::into_bytes)
        };
        match bytes {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(b) => {
                body.extend_from_slice(&(b.len() as i32).to_be_bytes());
                body.extend_from_slice(&b);
            }
        }
    }
    msg(out, b'D', &body);
}

/// Run a portal's statement once and keep its result for Describe / Execute.
fn ensure_run(db: &mut Database, p: &mut Portal) -> Result<(), Error> {
    if p.result.is_none() {
        let (o, tag) = run_command(db, &p.query, &p.params)?;
        let returns_rows = !matches!(o, Output::Done(_));
        let (cols, rows) = rows_of(&o);
        p.result = Some((cols, rows, tag, returns_rows));
    }
    Ok(())
}

/// Connection keys for CancelRequest: (process id, secret) -> the session's cancel flag.
type CancelMap =
    std::collections::HashMap<(i32, i32), std::sync::Arc<std::sync::atomic::AtomicBool>>;
static CANCEL: std::sync::Mutex<Option<CancelMap>> = std::sync::Mutex::new(None);
static NEXT_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

struct CancelGuard((i32, i32));
impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(m) = CANCEL.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            m.remove(&self.0);
        }
    }
}

fn session(s: &mut TcpStream, db: &mut Database, limits: &Limits) -> io::Result<()> {
    // Startup: the client may first probe for TLS / GSS encryption, which we decline.
    loop {
        let mut len = [0u8; 4];
        s.read_exact(&mut len)?;
        let len = i32::from_be_bytes(len) as usize;
        if !(8..=10_000).contains(&len) {
            return Ok(());
        }
        let mut body = vec![0u8; len - 4];
        s.read_exact(&mut body)?;
        match i32::from_be_bytes([body[0], body[1], body[2], body[3]]) {
            80877103 | 80877104 => s.write_all(b"N")?,
            196608 => break,
            80877102 => {
                // CancelRequest: process id and secret key of the target connection
                if body.len() >= 12 {
                    let pid = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
                    let key = i32::from_be_bytes([body[8], body[9], body[10], body[11]]);
                    if let Some(flag) = CANCEL
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .and_then(|m| m.get(&(pid, key)))
                    {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }
                return Ok(());
            }
            _ => {
                let mut out = Vec::new();
                error(&mut out, "0A000", "unsupported frontend protocol version");
                return s.write_all(&out);
            }
        }
    }
    let mut out = Vec::new();
    msg(&mut out, b'R', &0i32.to_be_bytes()); // AuthenticationOk
    for (k, v) in [
        ("server_version", "14.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("TimeZone", "UTC"),
    ] {
        msg(&mut out, b'S', format!("{k}\0{v}\0").as_bytes());
    }
    let pid = NEXT_PID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let secret = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(7) as i32)
        ^ pid.rotate_left(16);
    CANCEL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert((pid, secret), db.cancel_handle());
    let _guard = CancelGuard((pid, secret));
    let mut key = pid.to_be_bytes().to_vec();
    key.extend_from_slice(&secret.to_be_bytes());
    msg(&mut out, b'K', &key); // BackendKeyData
    ready(&mut out, db);
    s.write_all(&out)?;

    let mut skipping = false; // after an extended-protocol error, ignore messages until Sync
    let mut statements: std::collections::HashMap<String, Prepared> = Default::default();
    let mut portals: std::collections::HashMap<String, Portal> = Default::default();
    loop {
        let mut hdr = [0u8; 5];
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if !(4..=limits.max_request_bytes.saturating_add(4)).contains(&len) {
            let mut out = Vec::new();
            error(&mut out, "54000", "message exceeds the request size limit");
            let _ = s.write_all(&out);
            return Ok(());
        }
        let mut body = vec![0u8; len - 4];
        s.read_exact(&mut body)?;
        let mut out = Vec::new();
        match hdr[0] {
            b'X' => return Ok(()),
            b'Q' => {
                skipping = false;
                let end = body.iter().position(|b| *b == 0).unwrap_or(body.len());
                let query = String::from_utf8_lossy(&body[..end]);
                let stmts = split_statements(&query);
                if stmts.is_empty() {
                    msg(&mut out, b'I', &[]);
                }
                for st in stmts {
                    if !run_statement(db, st, &mut out) {
                        break;
                    }
                }
                ready(&mut out, db);
            }
            b'S' => {
                skipping = false;
                ready(&mut out, db);
            }
            b'H' => {}
            _ if skipping => {}
            tag @ (b'P' | b'B' | b'D' | b'E' | b'C') => {
                if let Err(e) = extended(db, tag, &body, &mut statements, &mut portals, &mut out) {
                    skipping = true; // PostgreSQL ignores everything up to the next Sync
                    error(&mut out, sqlstate(&e), &e.to_string());
                }
            }
            _ => {
                skipping = true;
                error(&mut out, "08P01", "unsupported protocol message");
            }
        }
        s.write_all(&out)?;
    }
}

/// Handle one extended-protocol message.
fn extended(
    db: &mut Database,
    tag: u8,
    body: &[u8],
    statements: &mut std::collections::HashMap<String, Prepared>,
    portals: &mut std::collections::HashMap<String, Portal>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let proto = |e: io::Error| Error::Parse(format!("malformed message: {e}"));
    let mut pos = 0;
    match tag {
        b'P' => {
            let name = cstr(body, &mut pos).map_err(proto)?;
            let query = cstr(body, &mut pos).map_err(proto)?;
            let n = i16_at(body, &mut pos).map_err(proto)?.max(0) as usize;
            let mut types = Vec::with_capacity(n);
            for _ in 0..n {
                types.push(i32_at(body, &mut pos).map_err(proto)?);
            }
            let nparams = crate::pbsql::parser::param_count(&query)
                .unwrap_or(0)
                .max(types.len());
            if !name.is_empty() && statements.contains_key(&name) {
                return Err(Error::Schema(format!(
                    "prepared statement \"{name}\" already exists"
                )));
            }
            statements.insert(
                name,
                Prepared {
                    query,
                    types,
                    nparams,
                },
            );
            msg(out, b'1', &[]); // ParseComplete
        }
        b'B' => {
            let portal = cstr(body, &mut pos).map_err(proto)?;
            let name = cstr(body, &mut pos).map_err(proto)?;
            let st = statements.get(&name).ok_or_else(|| {
                Error::Schema(format!("prepared statement \"{name}\" does not exist"))
            })?;
            let nf = i16_at(body, &mut pos).map_err(proto)?.max(0) as usize;
            let mut pformats = Vec::with_capacity(nf);
            for _ in 0..nf {
                pformats.push(i16_at(body, &mut pos).map_err(proto)?);
            }
            let np = i16_at(body, &mut pos).map_err(proto)?.max(0) as usize;
            if np != st.nparams {
                return Err(Error::Parse(format!(
                    "bind message supplies {np} parameters, but prepared statement \"{name}\" requires {}",
                    st.nparams
                )));
            }
            let mut params = Vec::with_capacity(np);
            for i in 0..np {
                let len = i32_at(body, &mut pos).map_err(proto)?;
                let raw = if len < 0 {
                    None
                } else {
                    let r = body
                        .get(pos..pos + len as usize)
                        .ok_or_else(|| Error::Parse("truncated parameter".into()))?;
                    pos += len as usize;
                    Some(r)
                };
                let oid = st.types.get(i).copied().unwrap_or(0);
                params.push(decode_param(oid, format_of(&pformats, i), raw)?);
            }
            let nr = i16_at(body, &mut pos).map_err(proto)?.max(0) as usize;
            let mut formats = Vec::with_capacity(nr);
            for _ in 0..nr {
                formats.push(i16_at(body, &mut pos).map_err(proto)?);
            }
            portals.insert(
                portal,
                Portal {
                    query: st.query.clone(),
                    params,
                    formats,
                    result: None,
                    sent: 0,
                },
            );
            msg(out, b'2', &[]); // BindComplete
        }
        b'D' => {
            let kind = *body
                .first()
                .ok_or_else(|| Error::Parse("empty Describe".into()))?;
            pos = 1;
            let name = cstr(body, &mut pos).map_err(proto)?;
            if kind == b'S' {
                let st = statements.get(&name).ok_or_else(|| {
                    Error::Schema(format!("prepared statement \"{name}\" does not exist"))
                })?;
                let mut pd = (st.nparams as i16).to_be_bytes().to_vec();
                for i in 0..st.nparams {
                    let oid = st.types.get(i).copied().filter(|o| *o != 0).unwrap_or(25);
                    pd.extend_from_slice(&oid.to_be_bytes());
                }
                msg(out, b't', &pd); // ParameterDescription
                match db.describe(&st.query)?.1 {
                    Some(cols) => typed_row_description(out, &cols),
                    None => msg(out, b'n', &[]), // NoData
                }
            } else {
                let p = portals
                    .get_mut(&name)
                    .ok_or_else(|| Error::Schema(format!("portal \"{name}\" does not exist")))?;
                ensure_run(db, p)?;
                let (cols, rows, _, returns_rows) = p.result.as_ref().expect("ran");
                if *returns_rows {
                    row_description(out, cols, rows, &p.formats);
                } else {
                    msg(out, b'n', &[]);
                }
            }
        }
        b'E' => {
            let name = cstr(body, &mut pos).map_err(proto)?;
            let max = i32_at(body, &mut pos).map_err(proto)?;
            let p = portals
                .get_mut(&name)
                .ok_or_else(|| Error::Schema(format!("portal \"{name}\" does not exist")))?;
            ensure_run(db, p)?;
            let (_, rows, tag, returns_rows) = p.result.as_ref().expect("ran");
            if !returns_rows {
                complete(out, tag);
                return Ok(());
            }
            let take = if max > 0 { max as usize } else { usize::MAX };
            let end = rows.len().min(p.sent.saturating_add(take));
            for r in &rows[p.sent..end] {
                data_row(out, r, &p.formats);
            }
            p.sent = end;
            if end < rows.len() {
                msg(out, b's', &[]); // PortalSuspended: Execute again for more
            } else {
                complete(out, tag);
            }
        }
        b'C' => {
            let kind = *body
                .first()
                .ok_or_else(|| Error::Parse("empty Close".into()))?;
            pos = 1;
            let name = cstr(body, &mut pos).map_err(proto)?;
            if kind == b'S' {
                statements.remove(&name);
            } else {
                portals.remove(&name);
            }
            msg(out, b'3', &[]); // CloseComplete
        }
        _ => unreachable!(),
    }
    Ok(())
}

pub fn handle(stream: TcpStream, db: Database) {
    handle_with(stream, db, &Limits::default())
}

pub fn handle_with(mut stream: TcpStream, mut db: Database, limits: &Limits) {
    let _ = session(&mut stream, &mut db, limits);
}
