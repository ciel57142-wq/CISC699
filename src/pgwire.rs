//! PostgreSQL wire protocol (v3) front end.
//!
//! Implements the startup handshake and the *simple query* flow, which is
//! what `psql`, libpq's `PQexec` and psycopg2 use. Every polydb command
//! works here, not just SQL: document and key-value results come back as
//! ordinary result sets.
//!
//! Not implemented: the extended protocol (Parse/Bind/Execute, i.e.
//! server-side prepared statements), COPY, authentication and TLS. The SQL
//! dialect is polydb's, so tools that inspect `pg_catalog` will not work.

use crate::error::Error;
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
        Error::Io(_) | Error::Corrupt(_) => "XX000",
    }
}

fn error(out: &mut Vec<u8>, code: &str, message: &str) {
    let mut body = Vec::new();
    for (field, text) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', message)] {
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
        let oid = rows.iter().map(|r| cell(&r[i])).find(|c| c.0.is_some()).map_or(25, |c| c.1);
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
    out.into_iter().map(str::trim).filter(|s| !s.is_empty()).collect()
}

const SESSION_SETTINGS: &[&str] = &[
    "client_encoding", "datestyle", "extra_float_digits", "application_name", "search_path", "session",
    "statement_timeout", "timezone", "time", "standard_conforming_strings", "names", "client_min_messages",
    "intervalstyle", "bytea_output", "lock_timeout", "idle_in_transaction_session_timeout", "transaction",
];

/// Run one statement; returns false if it failed.
fn run_statement(db: &mut Database, stmt: &str, out: &mut Vec<u8>) -> bool {
    let mut words = stmt.split_whitespace().map(str::to_ascii_lowercase);
    let first = words.next().unwrap_or_default();
    let second = words.next().unwrap_or_default();
    let second = second.trim_end_matches(|c| c == '=' || c == ';');

    // PostgreSQL session chatter that drivers send and that has no meaning here.
    let noop = match first.as_str() {
        "set" if SESSION_SETTINGS.contains(&second) => Some("SET"),
        "reset" | "discard" | "deallocate" | "listen" | "unlisten" => Some("OK"),
        // PostgreSQL only warns when these have no transaction to act on.
        "begin" | "start" if db.in_transaction() => Some("BEGIN"),
        "commit" | "end" if !db.in_transaction() => Some("COMMIT"),
        "rollback" | "abort" if !db.in_transaction() => Some("ROLLBACK"),
        _ => None,
    };
    if let Some(tag) = noop {
        complete(out, tag);
        return true;
    }
    let res = match first.as_str() {
        "start" => db.begin().map(|_| Output::Done(String::new())),
        "end" => db.commit().map(|_| Output::Done(String::new())),
        "abort" => db.rollback().map(|_| Output::Done(String::new())),
        _ => db.execute(stmt),
    };
    match res {
        Err(e) => {
            error(out, sqlstate(&e), &e.to_string());
            false
        }
        Ok(Output::Rows { columns, rows }) => {
            result_set(out, &columns, &rows);
            true
        }
        Ok(Output::Docs(docs)) => {
            let rows: Vec<Vec<Value>> = docs.into_iter().map(|d| vec![Value::Text(d.to_json())]).collect();
            result_set(out, &["document".to_string()], &rows);
            true
        }
        Ok(Output::Scalar(v)) => {
            result_set(out, &["value".to_string()], &[vec![v]]);
            true
        }
        Ok(Output::Done(m)) => {
            let n = m.split_whitespace().next().and_then(|w| w.parse::<u64>().ok()).unwrap_or(0);
            let tag = match (first.as_str(), second) {
                ("insert", _) => format!("INSERT 0 {n}"),
                ("update", _) => format!("UPDATE {n}"),
                ("delete", _) => format!("DELETE {n}"),
                ("create", "index") => "CREATE INDEX".into(),
                ("create", _) => "CREATE TABLE".into(),
                ("drop", "index") => "DROP INDEX".into(),
                ("drop", _) => "DROP TABLE".into(),
                ("begin" | "start", _) => "BEGIN".into(),
                ("commit" | "end", _) => "COMMIT".into(),
                ("rollback" | "abort", _) => "ROLLBACK".into(),
                _ => "OK".into(),
            };
            complete(out, &tag);
            true
        }
    }
}

fn session(s: &mut TcpStream, db: &mut Database) -> io::Result<()> {
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
            80877102 => return Ok(()), // cancel request
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
    msg(&mut out, b'K', &[0, 0, 0, 1, 0, 0, 0, 1]); // BackendKeyData
    ready(&mut out, db);
    s.write_all(&out)?;

    let mut skipping = false; // after an extended-protocol error, ignore messages until Sync
    loop {
        let mut hdr = [0u8; 5];
        if s.read_exact(&mut hdr).is_err() {
            return Ok(());
        }
        let len = i32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
        if !(4..=256 << 20).contains(&len) {
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
            _ => {
                skipping = true;
                error(
                    &mut out,
                    "0A000",
                    "polydb supports the simple query protocol only (no server-side prepared statements)",
                );
            }
        }
        s.write_all(&out)?;
    }
}

pub fn handle(mut stream: TcpStream, mut db: Database) {
    let _ = session(&mut stream, &mut db);
}
