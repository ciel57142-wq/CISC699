//! TCP server: one thread per connection, one [`Database`] session per connection.
//!
//! Four listeners share the same data:
//!
//! | Protocol   | Default port | Speaks |
//! |------------|--------------|--------|
//! | native     | 7878         | one command per line in, one JSON object per line out |
//! | Redis      | 6380         | RESP (redis-cli, redis client libraries) |
//! | PostgreSQL | 5433         | wire protocol v3, simple-query flow (psql, libpq, psycopg2) |
//! | MongoDB    | 27018        | OP_MSG / BSON (pymongo and other drivers) |
//!
//! There is no authentication or TLS: bind to localhost or a trusted network.

use crate::error::{Error, Result};
use crate::value::Value;
use crate::{Database, Output};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread;

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    /// `None` disables a listener; port 0 picks a free port.
    pub native: Option<u16>,
    pub redis: Option<u16>,
    pub postgres: Option<u16>,
    pub mongo: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Config { host: "127.0.0.1".into(), native: Some(7878), redis: Some(6380), postgres: Some(5433), mongo: Some(27018) }
    }
}

/// Addresses the server is actually listening on.
#[derive(Debug, Clone, Default)]
pub struct Running {
    pub native: Option<SocketAddr>,
    pub redis: Option<SocketAddr>,
    pub postgres: Option<SocketAddr>,
    pub mongo: Option<SocketAddr>,
}

fn listen(db: &Database, host: &str, port: Option<u16>, handler: fn(TcpStream, Database)) -> Result<Option<SocketAddr>> {
    let Some(port) = port else {
        return Ok(None);
    };
    let listener = TcpListener::bind((host, port))
        .map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("cannot listen on {host}:{port}: {e}"))))?;
    let addr = listener.local_addr()?;
    let db = db.session();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_nodelay(true);
            let session = db.session();
            thread::spawn(move || handler(stream, session));
        }
    });
    Ok(Some(addr))
}

/// Start the listeners on background threads and return their addresses.
pub fn start(db: &Database, cfg: &Config) -> Result<Running> {
    Ok(Running {
        native: listen(db, &cfg.host, cfg.native, handle_native)?,
        redis: listen(db, &cfg.host, cfg.redis, crate::resp::handle)?,
        postgres: listen(db, &cfg.host, cfg.postgres, crate::pgwire::handle)?,
        mongo: listen(db, &cfg.host, cfg.mongo, crate::mongo::handle)?,
    })
}

/// Start the listeners and serve until the process is stopped.
pub fn serve(db: &Database, cfg: &Config) -> Result<()> {
    let r = start(db, cfg)?;
    for (name, addr) in [("native", r.native), ("redis", r.redis), ("postgres", r.postgres), ("mongodb", r.mongo)] {
        if let Some(a) = addr {
            println!("{name:<9} listening on {a}");
        }
    }
    loop {
        thread::park();
    }
}

// ------------------------------------------------------------ native protocol

fn obj(pairs: Vec<(&str, Value)>) -> Value {
    Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<BTreeMap<_, _>>())
}

pub(crate) fn error_kind(e: &Error) -> &'static str {
    match e {
        Error::Io(_) => "io",
        Error::Parse(_) => "parse",
        Error::Schema(_) => "schema",
        Error::Constraint(_) => "constraint",
        Error::Type(_) => "type",
        Error::Corrupt(_) => "corrupt",
        Error::Txn(_) => "transaction",
        Error::Conflict(_) => "conflict",
    }
}

/// Encode a command result as the native protocol's one-line JSON reply.
pub fn result_to_json(res: &Result<Output>) -> String {
    let v = match res {
        Err(e) => obj(vec![("ok", false.into()), ("kind", error_kind(e).into()), ("error", e.to_string().into())]),
        Ok(Output::Done(m)) => obj(vec![("ok", true.into()), ("type", "done".into()), ("message", m.as_str().into())]),
        Ok(Output::Scalar(v)) => obj(vec![("ok", true.into()), ("type", "value".into()), ("value", v.clone())]),
        Ok(Output::Docs(d)) => obj(vec![("ok", true.into()), ("type", "docs".into()), ("docs", Value::Array(d.clone()))]),
        Ok(Output::Rows { columns, rows }) => obj(vec![
            ("ok", true.into()),
            ("type", "rows".into()),
            ("columns", Value::Array(columns.iter().map(|c| c.as_str().into()).collect())),
            ("rows", Value::Array(rows.iter().map(|r| Value::Array(r.clone())).collect())),
        ]),
    };
    v.to_json()
}

/// Decode a native-protocol reply (used by the bundled client).
pub fn result_from_json(line: &str) -> std::result::Result<Output, String> {
    let v = Value::parse(line).map_err(|e| format!("malformed reply: {e}"))?;
    if v.get("ok") != Some(&Value::Bool(true)) {
        return Err(v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string());
    }
    let arr = |k: &str| match v.get(k) {
        Some(Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };
    Ok(match v.get("type").and_then(Value::as_str) {
        Some("done") => Output::Done(v.get("message").and_then(Value::as_str).unwrap_or("").to_string()),
        Some("value") => Output::Scalar(v.get("value").cloned().unwrap_or(Value::Null)),
        Some("docs") => Output::Docs(arr("docs")),
        Some("rows") => Output::Rows {
            columns: arr("columns").iter().map(Value::display).collect(),
            rows: arr("rows")
                .into_iter()
                .map(|r| match r {
                    Value::Array(a) => a,
                    other => vec![other],
                })
                .collect(),
        },
        _ => return Err("malformed reply: unknown type".into()),
    })
}

fn handle_native(stream: TcpStream, mut db: Database) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        // Cap a single command at 64 MB so a client cannot exhaust memory.
        match (&mut reader).take(64 << 20).read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        let mut reply = result_to_json(&db.execute(cmd));
        reply.push('\n');
        if writer.write_all(reply.as_bytes()).is_err() {
            return;
        }
    }
}
