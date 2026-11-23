//! Redis protocol (RESP2 and RESP3) front end for the key-value model.
//!
//! Supported: PING ECHO GET SET MGET MSET DEL EXISTS INCR DECR INCRBY DECRBY
//! KEYS DBSIZE STRLEN APPEND GETSET SETNX TTL SELECT INFO COMMAND CLIENT
//! HELLO QUIT. Values are binary-safe. Expiry, hashes, lists, sets,
//! pub/sub and MULTI are not implemented and answer with an error.

use crate::error::Error;
use crate::Database;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;

fn read_line(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    if r.take(1 << 20).read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

fn parse_len(b: &[u8]) -> io::Result<usize> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n <= 512 << 20)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "bad RESP length"))
}

fn read_command(r: &mut impl BufRead) -> io::Result<Option<Vec<Vec<u8>>>> {
    let Some(line) = read_line(r)? else {
        return Ok(None);
    };
    if line.first() != Some(&b'*') {
        // inline command, as typed into telnet
        return Ok(Some(line.split(|b| b.is_ascii_whitespace()).filter(|p| !p.is_empty()).map(<[u8]>::to_vec).collect()));
    }
    let n = parse_len(&line[1..])?;
    let mut args = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let Some(h) = read_line(r)? else {
            return Ok(None);
        };
        if h.first() != Some(&b'$') {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "expected bulk string"));
        }
        let mut buf = vec![0u8; parse_len(&h[1..])? + 2];
        r.read_exact(&mut buf)?;
        buf.truncate(buf.len() - 2);
        args.push(buf);
    }
    Ok(Some(args))
}

fn bulk(out: &mut Vec<u8>, proto: u8, v: Option<&[u8]>) {
    match v {
        // RESP3 has a dedicated null; RESP2 uses the null bulk string
        None => out.extend_from_slice(if proto >= 3 { b"_\r\n".as_slice() } else { b"$-1\r\n".as_slice() }),
        Some(v) => {
            out.extend_from_slice(format!("${}\r\n", v.len()).as_bytes());
            out.extend_from_slice(v);
            out.extend_from_slice(b"\r\n");
        }
    }
}

fn int(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(format!(":{n}\r\n").as_bytes());
}

fn err(out: &mut Vec<u8>, msg: &str) {
    out.extend_from_slice(format!("-ERR {}\r\n", msg.replace(['\r', '\n'], " ")).as_bytes());
}

/// Redis glob: `*`, `?`, and `\` to escape.
fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => (0..=s.len()).any(|i| glob(&p[1..], &s[i..])),
        Some(b'?') => !s.is_empty() && glob(&p[1..], &s[1..]),
        Some(b'\\') if p.len() > 1 => s.first() == Some(&p[1]) && glob(&p[2..], &s[1..]),
        Some(c) => s.first() == Some(c) && glob(&p[1..], &s[1..]),
    }
}

fn key(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn exec(db: &mut Database, args: &[Vec<u8>], proto: &mut u8, out: &mut Vec<u8>) -> Result<bool, Error> {
    let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
    let argc = |n: usize, out: &mut Vec<u8>| {
        let ok = args.len() == n + 1;
        if !ok {
            err(out, &format!("wrong number of arguments for '{}' command", name.to_lowercase()));
        }
        ok
    };
    match name.as_str() {
        "PING" => match args.get(1) {
            Some(m) => bulk(out, *proto, Some(m)),
            None => out.extend_from_slice(b"+PONG\r\n"),
        },
        "ECHO" if argc(1, out) => bulk(out, *proto, Some(&args[1])),
        "QUIT" => {
            out.extend_from_slice(b"+OK\r\n");
            return Ok(false);
        }
        "SELECT" | "CLIENT" => out.extend_from_slice(b"+OK\r\n"),
        "COMMAND" => out.extend_from_slice(b"*0\r\n"),
        "HELLO" => {
            let want = args.get(1).and_then(|v| std::str::from_utf8(v).ok()).and_then(|v| v.parse::<u8>().ok());
            match want {
                Some(v) if v != 2 && v != 3 => out.extend_from_slice(b"-NOPROTO unsupported protocol version\r\n"),
                _ => {
                    *proto = want.unwrap_or(*proto);
                    // a map in RESP3, a flat array in RESP2
                    out.extend_from_slice(if *proto >= 3 { b"%3\r\n".as_slice() } else { b"*6\r\n".as_slice() });
                    out.extend_from_slice(b"$6\r\nserver\r\n$6\r\npolydb\r\n$7\r\nversion\r\n");
                    bulk(out, *proto, Some(env!("CARGO_PKG_VERSION").as_bytes()));
                    out.extend_from_slice(format!("$5\r\nproto\r\n:{}\r\n", *proto).as_bytes());
                }
            }
        }
        "INFO" => {
            let info = format!(
                "# Server\r\nredis_version:7.0.0\r\npolydb_version:{}\r\n# Keyspace\r\ndb0:keys={}\r\n",
                env!("CARGO_PKG_VERSION"),
                db.kv_keys("")?.len()
            );
            bulk(out, *proto, Some(info.as_bytes()));
        }
        "GET" if argc(1, out) => bulk(out, *proto, db.kv_get(&key(&args[1]))?.as_deref()),
        "STRLEN" if argc(1, out) => int(out, db.kv_get(&key(&args[1]))?.map_or(0, |v| v.len() as i64)),
        "SET" => {
            if args.len() != 3 {
                err(out, "only plain 'SET key value' is supported (no EX/PX/NX/XX options)");
            } else {
                db.kv_set(&key(&args[1]), &args[2])?;
                out.extend_from_slice(b"+OK\r\n");
            }
        }
        "SETNX" | "GETSET" | "APPEND" if argc(2, out) => {
            // read-modify-write: do it in a transaction so it cannot interleave
            let k = key(&args[1]);
            loop {
                db.begin()?;
                let old = db.kv_get(&k)?;
                let new: Option<Vec<u8>> = match name.as_str() {
                    "SETNX" => old.is_none().then(|| args[2].clone()),
                    "GETSET" => Some(args[2].clone()),
                    _ => Some([old.clone().unwrap_or_default(), args[2].clone()].concat()),
                };
                if let Some(v) = &new {
                    db.kv_set(&k, v)?;
                }
                match db.commit() {
                    Err(Error::Conflict(_)) => continue,
                    Err(e) => return Err(e),
                    Ok(()) => {}
                }
                match name.as_str() {
                    "SETNX" => int(out, new.is_some() as i64),
                    "GETSET" => bulk(out, *proto, old.as_deref()),
                    _ => int(out, new.map_or(0, |v| v.len() as i64)),
                }
                break;
            }
        }
        "MGET" if args.len() > 1 => {
            out.extend_from_slice(format!("*{}\r\n", args.len() - 1).as_bytes());
            for k in &args[1..] {
                bulk(out, *proto, db.kv_get(&key(k))?.as_deref());
            }
        }
        "MSET" if args.len() > 2 && args.len() % 2 == 1 => {
            db.begin()?;
            for pair in args[1..].chunks(2) {
                db.kv_set(&key(&pair[0]), &pair[1])?;
            }
            db.commit()?;
            out.extend_from_slice(b"+OK\r\n");
        }
        "DEL" | "UNLINK" if args.len() > 1 => {
            let mut n = 0;
            for k in &args[1..] {
                n += db.kv_del(&key(k))? as i64;
            }
            int(out, n);
        }
        "EXISTS" if args.len() > 1 => {
            let mut n = 0;
            for k in &args[1..] {
                n += db.kv_get(&key(k))?.is_some() as i64;
            }
            int(out, n);
        }
        "INCR" | "DECR" if argc(1, out) => int(out, db.kv_incr(&key(&args[1]), if name == "INCR" { 1 } else { -1 })?),
        "INCRBY" | "DECRBY" if argc(2, out) => match std::str::from_utf8(&args[2]).ok().and_then(|s| s.parse::<i64>().ok()) {
            Some(by) => int(out, db.kv_incr(&key(&args[1]), if name == "INCRBY" { by } else { -by })?),
            None => err(out, "value is not an integer or out of range"),
        },
        "KEYS" if argc(1, out) => {
            let pat = &args[1];
            let literal = pat.iter().position(|b| matches!(b, b'*' | b'?' | b'\\' | b'[')).unwrap_or(pat.len());
            let keys: Vec<String> =
                db.kv_keys(&key(&pat[..literal]))?.into_iter().filter(|k| glob(pat, k.as_bytes())).collect();
            out.extend_from_slice(format!("*{}\r\n", keys.len()).as_bytes());
            for k in keys {
                bulk(out, *proto, Some(k.as_bytes()));
            }
        }
        "DBSIZE" => int(out, db.kv_keys("")?.len() as i64),
        "TTL" | "PTTL" if argc(1, out) => int(out, if db.kv_get(&key(&args[1]))?.is_some() { -1 } else { -2 }),
        // the `if argc(..)` guards above fall through to here after writing their own error
        _ if !out.is_empty() => {}
        _ => err(out, &format!("unknown or unsupported command '{}'", name.to_lowercase())),
    }
    Ok(true)
}

pub fn handle(stream: TcpStream, mut db: Database) {
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut proto = 2u8;
    loop {
        let args = match read_command(&mut reader) {
            Ok(Some(a)) => a,
            _ => return,
        };
        if args.is_empty() {
            continue;
        }
        let mut out = Vec::new();
        let keep_going = match exec(&mut db, &args, &mut proto, &mut out) {
            Ok(k) => k,
            Err(e) => {
                if db.in_transaction() {
                    let _ = db.rollback();
                }
                out.clear();
                let prefix = if matches!(e, Error::Type(_)) { "value is not an integer or out of range: " } else { "" };
                err(&mut out, &format!("{prefix}{e}"));
                true
            }
        };
        if writer.write_all(&out).is_err() || !keep_going {
            return;
        }
    }
}
