//! Hardening tests: failure paths, hostile input and the command-line contract.
//!
//! `tests/integration.rs` checks that correct use gives correct answers. This
//! file checks what happens when things go wrong: bad arguments, damaged
//! files, a port that is taken, garbage on a socket, a process killed without
//! warning. Every test here runs offline and needs nothing but the crate.
//!
//! The nesting tests are the regression tests for DEFECT-01, recorded in
//! `docs/REPRODUCIBILITY.md`.

use polydb::server::{self, Config};
use polydb::{Database, Error, Output, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_polydb");

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-harden-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    let _ = fs::remove_file(&p);
    p
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

/// Run the binary with `args`, feed it `stdin`, return (exit code, stdout, stderr).
fn run_cli(args: &[&str], stdin: &str) -> (Option<i32>, String, String) {
    let mut child = Command::new(BIN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn polydb");
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_string();
    // Write from a thread so a child that stops reading cannot block the test.
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(data.as_bytes());
    });
    let out = child.wait_with_output().expect("wait for polydb");
    let _ = writer.join();
    (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

/// A server on free ports with every protocol enabled.
fn start_all() -> (Database, server::Running) {
    let db = Database::in_memory();
    let cfg = Config { host: "127.0.0.1".into(), native: Some(0), redis: Some(0), postgres: Some(0), mongo: Some(0) };
    let running = server::start(&db, &cfg).expect("start server");
    (db, running)
}

/// Send one native-protocol command and parse the JSON reply.
fn ask(addr: SocketAddr, cmd: &str) -> Value {
    let stream = TcpStream::connect(addr).expect("connect native port");
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut w = stream.try_clone().unwrap();
    w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).expect("read reply");
    Value::parse(line.trim()).unwrap_or_else(|e| panic!("reply to {cmd:?} is not JSON ({e}): {line:?}"))
}

/// Deterministic pseudo-random bytes (fixed seed, no clock, no OS entropy).
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) as u8
        })
        .collect()
}

/// Write `bytes` to `addr`, then read until the server answers or closes.
/// Never panics on socket errors: the point is that the *server* survives.
fn poke(addr: SocketAddr, bytes: &[u8]) {
    let Ok(mut s) = TcpStream::connect(addr) else {
        panic!("server at {addr} stopped accepting connections");
    };
    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = s.write_all(bytes);
    let _ = s.shutdown(std::net::Shutdown::Write);
    let mut sink = [0u8; 4096];
    while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
}

// ------------------------------------------------------------ command line

#[test]
fn cli_help_prints_usage_and_exits_zero() {
    for flag in ["--help", "-h", "help"] {
        let (code, out, _) = run_cli(&[flag], "");
        assert_eq!(code, Some(0), "{flag}");
        assert!(out.contains("USAGE") && out.contains("polydb serve"), "{flag}: {out}");
    }
}

#[test]
fn cli_rejects_bad_server_options_with_a_message() {
    let cases: [(&[&str], &str); 4] = [
        (&["serve", "--memory", "--bogus"], "unknown option --bogus"),
        (&["serve", "--memory", "--port", "notaport"], "'notaport' is not a port number"),
        (&["serve", "--memory", "--port", "70000"], "'70000' is not a port number"),
        (&["serve", "--memory", "--port"], "--port needs a value"),
    ];
    for (args, want) in cases {
        let (code, _, err) = run_cli(args, "");
        assert_eq!(code, Some(1), "{args:?} should exit 1");
        assert!(err.contains(want), "{args:?}: stderr was {err:?}");
    }
}

#[test]
fn cli_client_reports_unreachable_server() {
    // Bind then drop to get a port that is very likely closed.
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let (code, _, err) = run_cli(&["client", &format!("127.0.0.1:{port}")], "");
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot connect to"), "stderr was {err:?}");
}

#[test]
fn cli_database_path_that_is_a_file_is_a_clean_error() {
    let path = tmp("not-a-dir");
    fs::write(&path, b"this is a file, not a database directory").unwrap();
    let (code, _, err) = run_cli(&[path.to_str().unwrap()], "GET x\n");
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot open database"), "stderr was {err:?}");
    fs::remove_file(&path).unwrap();
}

/// The scripted run in `tests/fixtures/smoke.commands` must print exactly
/// `tests/fixtures/smoke.expected`. This is the deterministic execution path
/// that `scripts/smoke.sh` and the README quick start rely on.
#[test]
fn cli_scripted_run_matches_golden_output() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let commands = fs::read_to_string(dir.join("smoke.commands")).expect("tests/fixtures/smoke.commands");
    let expected = fs::read_to_string(dir.join("smoke.expected")).expect("tests/fixtures/smoke.expected");
    let (code, out, err) = run_cli(&["--memory"], &commands);
    assert_eq!(code, Some(0), "stderr: {err}");
    assert_eq!(out, expected, "output differs from tests/fixtures/smoke.expected");
    // and it is repeatable
    let (_, again, _) = run_cli(&["--memory"], &commands);
    assert_eq!(again, expected);
}

#[test]
fn cli_errors_do_not_stop_a_piped_script() {
    let (code, out, _) = run_cli(&["--memory"], "SELEKT 1\nSET k v\nGET k\n");
    assert_eq!(code, Some(0));
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with("error: parse error"), "{out}");
    assert_eq!(lines[2], "v");
}

// ------------------------------------------------------------ invalid input

#[test]
fn invalid_statements_return_typed_errors_and_change_nothing() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let before = db.entry_count();

    let cases: [(&str, fn(&Error) -> bool); 10] = [
        ("", |e| matches!(e, Error::Parse(_))),
        ("SELECT", |e| matches!(e, Error::Parse(_))),
        ("SELECT * FROM t WHERE", |e| matches!(e, Error::Parse(_))),
        ("INSERT INTO t VALUES (1, 'dup')", |e| matches!(e, Error::Constraint(_))),
        ("INSERT INTO t VALUES (2, NULL)", |e| matches!(e, Error::Constraint(_))),
        ("INSERT INTO t VALUES ('x', 'y')", |e| matches!(e, Error::Type(_))),
        ("SELECT * FROM missing", |e| matches!(e, Error::Schema(_))),
        ("SELECT nope FROM t", |e| matches!(e, Error::Schema(_))),
        ("db.c.insert({unterminated", |e| matches!(e, Error::Parse(_))),
        ("COMMIT", |e| matches!(e, Error::Txn(_))),
    ];
    for (cmd, is_expected) in cases {
        match db.execute(cmd) {
            Ok(out) => panic!("{cmd:?} should fail, got {out:?}"),
            Err(e) => assert!(is_expected(&e), "{cmd:?} gave the wrong error kind: {e:?}"),
        }
    }
    assert_eq!(db.entry_count(), before, "a failed statement left data behind");
    // a multi-row insert that fails on its last row inserts none of them
    assert!(db.execute("INSERT INTO t VALUES (5, 'e'), (6, 'f'), (1, 'dup')").is_err());
    assert_eq!(db.entry_count(), before);
}

#[test]
fn division_by_zero_and_integer_overflow_do_not_panic() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE n (id INT PRIMARY KEY, v INT)").unwrap();
    db.execute("INSERT INTO n VALUES (1, 9223372036854775807)").unwrap();
    // Either an error or a value is acceptable; a panic is not.
    for sql in ["SELECT 1 / 0 FROM n", "SELECT 1 % 0 FROM n", "SELECT v + 1 FROM n", "SELECT v * v FROM n", "SELECT -v - 2 FROM n"] {
        let _ = db.execute(sql);
    }
    db.execute("SET counter 9223372036854775807").unwrap();
    assert!(db.execute("INCR counter").is_err(), "INCR past i64::MAX must be an error, not a wrap");
    assert_eq!(db.execute("GET counter").unwrap(), Output::Scalar(t("9223372036854775807")));
}

/// Deeply nested input must be rejected, not crash the process. Run in a
/// child process so a stack overflow shows up as a failed exit status
/// instead of taking the test runner down.
fn nested_input_survives(command: &str) -> bool {
    let (code, out, _) = run_cli(&["--memory"], &format!("{command}\nSET alive yes\nGET alive\n"));
    code == Some(0) && out.lines().last() == Some("yes")
}

#[test]
fn moderately_nested_input_is_handled() {
    let depth = 200;
    let json = format!("db.c.insert({{a: {}1{}}})", "[".repeat(depth), "]".repeat(depth));
    let sql = format!("SELECT {}1{} FROM t", "(".repeat(depth), ")".repeat(depth));
    assert!(nested_input_survives(&json));
    assert!(nested_input_survives(&sql));
}

/// DEFECT-01 (fixed): input nested 200,000 levels deep used to overflow the
/// parser's stack and abort the process. It must now be a parse error.
#[test]
fn extremely_nested_json_does_not_crash_the_process() {
    let depth = 200_000;
    let json = format!("db.c.insert({{a: {}1{}}})", "[".repeat(depth), "]".repeat(depth));
    assert!(nested_input_survives(&json), "process died on {depth}-deep JSON");
}

#[test]
fn extremely_nested_sql_does_not_crash_the_process() {
    let depth = 200_000;
    for sql in [
        format!("SELECT {}1{} FROM t", "(".repeat(depth), ")".repeat(depth)),
        format!("SELECT {}1 FROM t", "-".repeat(depth)),
        format!("SELECT 1 FROM t WHERE {}true", "NOT ".repeat(depth)),
        format!("SELECT 1{} FROM t", "+1".repeat(depth)),
        format!("SELECT 1 FROM t WHERE true{}", " OR true".repeat(depth)),
        format!("SELECT {}1{} FROM t", "abs(".repeat(depth), ")".repeat(depth)),
    ] {
        assert!(nested_input_survives(&sql), "process died on SQL starting {:?}", &sql[..40]);
    }
}

/// The limits are exact, documented numbers: what is inside them works and
/// what is outside them is an error that names the limit.
#[test]
fn nesting_limits_are_enforced_at_their_documented_values() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();

    // Stored documents: 100 levels. {a: [[..1..]]} with k arrays has k + 2 levels.
    let doc = |arrays: usize| format!("db.c.insert({{_id: {arrays}, a: {}1{}}})", "[".repeat(arrays), "]".repeat(arrays));
    db.execute(&doc(98)).expect("a 100-level document is accepted");
    match db.execute(&doc(99)) {
        Err(Error::Type(m)) => assert!(m.contains("deeper than 100 levels"), "{m}"),
        other => panic!("a 101-level document should be a type error, got {other:?}"),
    }
    // JSON text of any kind: 128 levels
    match db.execute(&doc(127)) {
        Err(Error::Parse(m)) => assert!(m.contains("deeper than 128 levels"), "{m}"),
        other => panic!("129 levels of JSON should be a parse error, got {other:?}"),
    }
    // the accepted document can be read back, matched and updated
    match db.execute("db.c.find({_id: 98})").unwrap() {
        Output::Docs(d) => assert_eq!(d.len(), 1),
        other => panic!("{other:?}"),
    }
    db.execute("db.c.update({_id: 98}, {$set: {b: 1}})").unwrap();
    assert_eq!(db.execute("db.c.count({b: 1})").unwrap(), Output::Scalar(Value::Int(1)));

    // SQL nesting: 32 levels (the expression itself is level 1)
    let parens = |n: usize| format!("SELECT {}1{} FROM t", "(".repeat(n), ")".repeat(n));
    db.execute(&parens(31)).expect("31 nested parentheses are accepted");
    match db.execute(&parens(32)) {
        Err(Error::Parse(m)) => assert!(m.contains("nested deeper than 32 levels"), "{m}"),
        other => panic!("32 nested parentheses should be a parse error, got {other:?}"),
    }

    // SQL depth budget: 100 chained operators and nested sub-expressions per expression
    let chain = |n: usize| format!("SELECT 0{} FROM t", "+1".repeat(n));
    match db.execute(&chain(100)).expect("100 operators are accepted") {
        Output::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(100)]]),
        other => panic!("{other:?}"),
    }
    match db.execute(&chain(101)) {
        Err(Error::Parse(m)) => assert!(m.contains("more than 100 operators"), "{m}"),
        other => panic!("101 operators should be a parse error, got {other:?}"),
    }
    // nesting and chaining share the budget: 30 NOTs around a 71-operator chain is 101
    let mixed = |n: usize| format!("SELECT id FROM t WHERE {}(0{} = 0)", "NOT ".repeat(30), "+1".repeat(n));
    assert!(db.execute(&mixed(60)).is_ok());
    assert!(matches!(db.execute(&mixed(100)), Err(Error::Parse(_))));
    // function arguments sit side by side, so they do not add up
    let args = (0..50).map(|i| format!("{i}+1+1")).collect::<Vec<_>>().join(", ");
    db.execute(&format!("SELECT COALESCE({args}) FROM t")).expect("50 small arguments are not a deep expression");
    // the budget is per expression, not per statement, and lists are not chains
    let many_rows = (2..2000).map(|i| format!("({i} + 0)")).collect::<Vec<_>>().join(", ");
    db.execute(&format!("INSERT INTO t VALUES {many_rows}")).expect("a long VALUES list is not a deep expression");
    let in_list = (0..5000).map(|i| format!("-{i}")).collect::<Vec<_>>().join(", ");
    db.execute(&format!("SELECT id FROM t WHERE id IN ({in_list})")).expect("a long IN list is not a deep expression");
}

/// An update with a long dotted path could build a document deeper than the
/// parser can read back. It must be refused, leaving the document readable.
#[test]
fn updates_cannot_create_documents_too_deep_to_read_back() {
    let mut db = Database::in_memory();
    db.execute("db.c.insert({_id: 1})").unwrap();
    let path = vec!["a"; 200].join(".");
    match db.execute(&format!("db.c.update({{_id: 1}}, {{$set: {{'{path}': 1}}}})")) {
        Err(Error::Type(m)) => assert!(m.contains("deeper than 100 levels"), "{m}"),
        other => panic!("expected a type error, got {other:?}"),
    }
    match db.execute("db.c.find({_id: 1})").unwrap() {
        Output::Docs(d) => assert_eq!(d[0].to_json(), r#"{"_id":1}"#),
        other => panic!("{other:?}"),
    }
}

// ------------------------------------------------------------ damaged files

fn seed(db: &mut Database) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one')").unwrap();
    db.execute("db.c.insert({_id: 1, v: 'doc'})").unwrap();
    db.execute("SET k value").unwrap();
}

#[test]
fn corrupt_snapshot_is_refused_rather_than_half_loaded() {
    let dir = tmp("snap");
    {
        let mut db = Database::open(&dir).unwrap();
        seed(&mut db);
        db.checkpoint().unwrap();
    }
    let snap = dir.join("data.snap");
    let mut bytes = fs::read(&snap).expect("checkpoint should have written data.snap");
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0x01;
    fs::write(&snap, &bytes).unwrap();
    match Database::open(&dir) {
        Err(Error::Corrupt(msg)) => assert!(msg.contains("data.snap"), "{msg}"),
        Err(other) => panic!("expected a Corrupt error, got {other:?}"),
        Ok(_) => panic!("a snapshot with a flipped bit was opened without complaint"),
    }
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn garbage_log_file_opens_as_empty_database() {
    let dir = tmp("garbage-wal");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("data.wal"), noise(7, 4096)).unwrap();
    let mut db = Database::open(&dir).expect("garbage in the log is treated as a torn tail");
    assert_eq!(db.entry_count(), 0);
    db.execute("SET k v").unwrap();
    drop(db);
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.execute("GET k").unwrap(), Output::Scalar(t("v")));
    fs::remove_dir_all(&dir).unwrap();
}

/// Documents a known limit rather than a guarantee: recovery stops at the
/// first record that fails its checksum, so damage in the *middle* of the log
/// also drops every commit after it. Earlier commits must survive intact.
#[test]
fn damage_in_the_middle_of_the_log_keeps_everything_before_it() {
    let dir = tmp("mid-wal");
    let first_commit_len;
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("SET first 1").unwrap();
        first_commit_len = fs::metadata(dir.join("data.wal")).unwrap().len() as usize;
        db.execute("SET second 2").unwrap();
        db.execute("SET third 3").unwrap();
    }
    let wal = dir.join("data.wal");
    let mut bytes = fs::read(&wal).unwrap();
    bytes[first_commit_len + 9] ^= 0xff; // inside the second record's payload
    fs::write(&wal, &bytes).unwrap();
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.execute("GET first").unwrap(), Output::Scalar(t("1")));
    assert_eq!(db.execute("GET second").unwrap(), Output::Scalar(Value::Null));
    assert_eq!(db.execute("GET third").unwrap(), Output::Scalar(Value::Null));
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------------------------ network

#[test]
fn port_already_in_use_is_a_clean_error() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let db = Database::in_memory();
    let cfg = Config { host: "127.0.0.1".into(), native: Some(port), redis: None, postgres: None, mongo: None };
    match server::start(&db, &cfg) {
        Err(Error::Io(e)) => assert!(e.to_string().contains(&format!("cannot listen on 127.0.0.1:{port}")), "{e}"),
        Err(other) => panic!("expected an Io error, got {other:?}"),
        Ok(_) => panic!("binding a port that is in use should fail"),
    }
    // the same thing from the command line: exit 1 and say which port
    let (code, _, err) = run_cli(&["serve", "--memory", "--port", &port.to_string(), "--redis-port", "off", "--pg-port", "off", "--mongo-port", "off"], "");
    assert_eq!(code, Some(1));
    assert!(err.contains("cannot listen on"), "stderr was {err:?}");
}

#[test]
fn disabled_listeners_are_not_opened() {
    let db = Database::in_memory();
    let cfg = Config { host: "127.0.0.1".into(), native: Some(0), redis: None, postgres: None, mongo: None };
    let r = server::start(&db, &cfg).unwrap();
    assert!(r.native.is_some() && r.redis.is_none() && r.postgres.is_none() && r.mongo.is_none());
}

#[test]
fn garbage_on_every_port_leaves_the_server_running() {
    let (_db, r) = start_all();
    let native = r.native.unwrap();
    assert_eq!(ask(native, "SET canary alive").get("ok"), Some(&Value::Bool(true)));

    let ports = [native, r.redis.unwrap(), r.postgres.unwrap(), r.mongo.unwrap()];
    for (i, addr) in ports.iter().enumerate() {
        // random bytes of several sizes, same bytes on every run
        for (j, len) in [1usize, 7, 64, 1024, 65536].iter().enumerate() {
            poke(*addr, &noise((i * 10 + j) as u64 + 1, *len));
        }
        poke(*addr, b""); // connect and hang up
        poke(*addr, &[0u8; 64]); // all zero bytes
        poke(*addr, &[0xff; 64]); // all ones: negative lengths in every protocol
    }
    // protocol-shaped but wrong
    let redis = r.redis.unwrap();
    poke(redis, b"*999999999999999999999\r\n");
    poke(redis, b"*-5\r\n");
    poke(redis, b"*1\r\n$-1\r\n");
    poke(redis, b"*2\r\n$3\r\nGET\r\n:5\r\n");
    poke(redis, b"*1\r\n$3\r\nGE"); // truncated mid-argument
    let pg = r.postgres.unwrap();
    poke(pg, &[0, 0, 0, 8, 0, 3, 0, 0]); // startup with no parameters
    poke(pg, &[0x7f, 0xff, 0xff, 0xff, 0, 3, 0, 0]); // startup claiming 2 GB
    poke(pg, &[0, 0, 0, 3]); // startup shorter than its own header
    poke(pg, &[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f]); // SSLRequest then nothing
    let mongo = r.mongo.unwrap();
    poke(mongo, &[16, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 0x07, 0, 0]); // OP_MSG, empty body
    poke(mongo, &[0xff, 0xff, 0xff, 0x7f, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 0x07, 0, 0]); // 2 GB message
    poke(mongo, &[4, 0, 0, 0]); // length smaller than a header
    let mut bad_bson = vec![26, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 0x07, 0, 0, 0, 0, 0, 0, 0];
    bad_bson.extend_from_slice(&[0xff, 0xff, 0xff, 0x7f, 0]); // document claiming 2 GB
    poke(mongo, &bad_bson);
    // native: not UTF-8, and a very long line
    poke(native, &[0xc3, 0x28, b'\n']);
    let mut long = vec![b'x'; 1 << 20];
    long.push(b'\n');
    poke(native, &long);

    // every listener still accepts, and the data is untouched
    for addr in ports {
        TcpStream::connect(addr).unwrap_or_else(|e| panic!("{addr} no longer accepts connections: {e}"));
    }
    assert_eq!(ask(native, "GET canary").get("value"), Some(&t("alive")));
}

#[test]
fn native_protocol_reports_errors_as_json_and_keeps_the_connection() {
    let (_db, r) = start_all();
    let stream = TcpStream::connect(r.native.unwrap()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut ask = |cmd: &str| {
        w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        Value::parse(line.trim()).unwrap()
    };
    for (cmd, kind) in [
        ("SELEKT 1", "parse"),
        ("SELECT * FROM nope", "schema"),
        ("db.c.insert(42)", "type"),
        ("ROLLBACK", "transaction"),
    ] {
        let reply = ask(cmd);
        assert_eq!(reply.get("ok"), Some(&Value::Bool(false)), "{cmd}");
        assert_eq!(reply.get("kind"), Some(&t(kind)), "{cmd}: {}", reply.to_json());
    }
    // same connection is still usable afterwards
    assert_eq!(ask("SET after errors").get("ok"), Some(&Value::Bool(true)));
    assert_eq!(ask("GET after").get("value"), Some(&t("errors")));
}

#[test]
fn a_client_that_disconnects_mid_transaction_leaves_nothing_behind() {
    let (db, r) = start_all();
    {
        let mut s = TcpStream::connect(r.native.unwrap()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut reader = BufReader::new(s.try_clone().unwrap());
        for cmd in ["BEGIN", "SET orphan 1", "db.c.insert({_id: 1})"] {
            s.write_all(format!("{cmd}\n").as_bytes()).unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
        }
    } // dropped without COMMIT
    std::thread::sleep(Duration::from_millis(100));
    let mut local = db.session();
    assert_eq!(local.execute("GET orphan").unwrap(), Output::Scalar(Value::Null));
    assert_eq!(local.entry_count(), 0);
}

/// One client must not be able to stop the server for everyone. Connection
/// threads have a much smaller stack than the main thread, so the worst
/// inputs that are still *accepted* are exercised here too, through the server.
#[test]
fn deeply_nested_input_over_the_network_does_not_stop_the_server() {
    let (_db, r) = start_all();
    let native = r.native.unwrap();
    assert_eq!(ask(native, "CREATE TABLE t (id INT PRIMARY KEY)").get("ok"), Some(&Value::Bool(true)));
    ask(native, "INSERT INTO t VALUES (1)");

    let depth = 100_000;
    let rejected = [
        format!("db.c.insert({{a: {}1{}}})", "[".repeat(depth), "]".repeat(depth)),
        format!("db.c.find({{a: {}1{}}})", "{b: ".repeat(depth), "}".repeat(depth)),
        format!("SELECT {}1{} FROM t", "(".repeat(depth), ")".repeat(depth)),
        format!("SELECT 1{} FROM t", "+1".repeat(depth)),
    ];
    for cmd in &rejected {
        let reply = ask(native, cmd);
        assert_eq!(reply.get("kind"), Some(&t("parse")), "{}", reply.to_json());
    }
    // the deepest inputs that are allowed, evaluated on a connection thread
    let accepted = [
        format!("db.c.insert({{_id: 1, a: {}1{}}})", "[".repeat(98), "]".repeat(98)),
        "db.c.find({_id: 1})".to_string(),
        format!("SELECT {}1{} FROM t", "(".repeat(31), ")".repeat(31)),
        format!("SELECT 0{} FROM t", "+1".repeat(100)),
        format!("SELECT id FROM t WHERE true{}", " AND true".repeat(100)),
        format!("SELECT {}1 FROM t", "- ".repeat(30)),
        format!("SELECT id FROM t WHERE {}true", "NOT ".repeat(30)),
    ];
    for cmd in &accepted {
        let reply = ask(native, cmd);
        assert_eq!(reply.get("ok"), Some(&Value::Bool(true)), "{}", reply.to_json());
    }

    // MongoDB port: an OP_MSG whose BSON document is nested 50,000 levels deep
    let levels = 50_000usize;
    let mut bson = vec![5u8, 0, 0, 0, 0]; // innermost: empty document
    for _ in 0..levels {
        let mut outer = Vec::with_capacity(bson.len() + 8);
        outer.extend_from_slice(&((bson.len() + 4 + 1 + 2 + 1) as u32).to_le_bytes());
        outer.push(0x03); // embedded document
        outer.extend_from_slice(b"a\0");
        outer.extend_from_slice(&bson);
        outer.push(0);
        bson = outer;
    }
    let mut msg = Vec::new();
    msg.extend_from_slice(&((16 + 4 + 1 + bson.len()) as u32).to_le_bytes());
    msg.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 0x07, 0, 0]); // request id, response to, OP_MSG
    msg.extend_from_slice(&[0, 0, 0, 0, 0]); // flags, section kind 0
    msg.extend_from_slice(&bson);
    poke(r.mongo.unwrap(), &msg);

    assert_eq!(ask(native, "SELECT COUNT(*) FROM t").get("ok"), Some(&Value::Bool(true)), "server stopped");
    TcpStream::connect(r.mongo.unwrap()).expect("MongoDB listener stopped");
}

// ---------------------------------------------------------- crash durability

/// Start the real server binary on a data directory, write through the
/// network, kill the process with no warning, and reopen the directory.
/// Every acknowledged write must be there.
#[test]
fn acknowledged_writes_survive_killing_the_server() {
    let dir = tmp("kill");
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut child = Command::new(BIN)
        .args(["serve", dir.to_str().unwrap(), "--port", &port.to_string(), "--redis-port", "off", "--pg-port", "off", "--mongo-port", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn polydb serve");

    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(addr).is_err() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("server did not start listening on {addr} within 15 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    assert_eq!(ask(addr, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").get("ok"), Some(&Value::Bool(true)));
    for i in 0..50 {
        assert_eq!(ask(addr, &format!("INSERT INTO t VALUES ({i}, 'row{i}')")).get("ok"), Some(&Value::Bool(true)));
    }
    assert_eq!(ask(addr, "db.c.insert({_id: 1, v: 'doc'})").get("ok"), Some(&Value::Bool(true)));
    assert_eq!(ask(addr, "SET k durable").get("ok"), Some(&Value::Bool(true)));

    child.kill().expect("kill server"); // SIGKILL on Unix: no chance to flush
    child.wait().unwrap();

    let mut db = Database::open(&dir).expect("reopen after kill");
    match db.execute("SELECT COUNT(*) FROM t").unwrap() {
        Output::Rows { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(50)]]),
        other => panic!("{other:?}"),
    }
    assert_eq!(db.execute("GET k").unwrap(), Output::Scalar(t("durable")));
    match db.execute("db.c.find({_id: 1})").unwrap() {
        Output::Docs(d) => assert_eq!(d.len(), 1),
        other => panic!("{other:?}"),
    }
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}
