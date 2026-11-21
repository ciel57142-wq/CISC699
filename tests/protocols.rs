//! Wire protocols exercised with hand-built messages (no client libraries
//! needed): PostgreSQL extended protocol and cancel, Redis expiry and
//! errors, MongoDB index management, native protocol transactions.

mod common;
use polydb::mongo::{decode_doc, encode_doc};
use polydb::server::{self, Config};
use polydb::{Database, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

fn start() -> (Database, server::Running) {
    let db = Database::in_memory();
    let cfg = Config {
        host: "127.0.0.1".into(),
        native: Some(0),
        redis: Some(0),
        postgres: Some(0),
        mongo: Some(0),
    };
    let r = server::start(&db, &cfg).unwrap();
    (db, r)
}

// ------------------------------------------------------------- PostgreSQL

struct Pg {
    s: TcpStream,
    pid: i32,
    key: i32,
}

fn pg_msg(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

impl Pg {
    fn connect(addr: SocketAddr) -> Pg {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut body = 196608i32.to_be_bytes().to_vec();
        body.extend(cstr("user"));
        body.extend(cstr("x"));
        body.push(0);
        let mut m = ((body.len() + 4) as i32).to_be_bytes().to_vec();
        m.extend(body);
        s.write_all(&m).unwrap();
        let mut pg = Pg { s, pid: 0, key: 0 };
        for (t, b) in pg.until(b'Z') {
            if t == b'K' {
                pg.pid = i32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                pg.key = i32::from_be_bytes([b[4], b[5], b[6], b[7]]);
            }
        }
        pg
    }

    fn read(&mut self) -> (u8, Vec<u8>) {
        let mut h = [0u8; 5];
        self.s.read_exact(&mut h).unwrap();
        let len = i32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        let mut b = vec![0u8; len - 4];
        self.s.read_exact(&mut b).unwrap();
        (h[0], b)
    }

    /// Messages up to and including the next one with `tag`.
    fn until(&mut self, tag: u8) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        loop {
            let m = self.read();
            let done = m.0 == tag;
            out.push(m);
            if done {
                return out;
            }
        }
    }

    fn send(&mut self, msgs: &[Vec<u8>]) {
        self.s.write_all(&msgs.concat()).unwrap();
    }
}

fn parse(name: &str, q: &str, types: &[i32]) -> Vec<u8> {
    let mut b = cstr(name);
    b.extend(cstr(q));
    b.extend((types.len() as i16).to_be_bytes());
    for t in types {
        b.extend(t.to_be_bytes());
    }
    pg_msg(b'P', &b)
}

fn bind(portal: &str, stmt: &str, params: &[Option<&str>], result_binary: bool) -> Vec<u8> {
    let mut b = cstr(portal);
    b.extend(cstr(stmt));
    b.extend(0i16.to_be_bytes()); // all parameters in text format
    b.extend((params.len() as i16).to_be_bytes());
    for p in params {
        match p {
            None => b.extend((-1i32).to_be_bytes()),
            Some(v) => {
                b.extend((v.len() as i32).to_be_bytes());
                b.extend(v.as_bytes());
            }
        }
    }
    if result_binary {
        b.extend(1i16.to_be_bytes());
        b.extend(1i16.to_be_bytes());
    } else {
        b.extend(0i16.to_be_bytes());
    }
    pg_msg(b'B', &b)
}

fn execute(portal: &str, max: i32) -> Vec<u8> {
    let mut b = cstr(portal);
    b.extend(max.to_be_bytes());
    pg_msg(b'E', &b)
}

fn describe(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend(cstr(name));
    pg_msg(b'D', &b)
}

fn sync() -> Vec<u8> {
    pg_msg(b'S', &[])
}

fn data_cells(b: &[u8]) -> Vec<Option<Vec<u8>>> {
    let n = i16::from_be_bytes([b[0], b[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]);
        pos += 4;
        if len < 0 {
            out.push(None);
        } else {
            out.push(Some(b[pos..pos + len as usize].to_vec()));
            pos += len as usize;
        }
    }
    out
}

fn tags(msgs: &[(u8, Vec<u8>)]) -> String {
    msgs.iter().map(|m| m.0 as char).collect()
}

#[test]
fn postgres_extended_protocol_lifecycle() {
    let (_db, r) = start();
    let mut pg = Pg::connect(r.postgres.unwrap());
    pg.send(&[pg_msg(
        b'Q',
        &cstr("CREATE TABLE p (id INT PRIMARY KEY, name TEXT, score REAL)"),
    )]);
    pg.until(b'Z');
    // prepare once, execute many times with different parameters (int typed, text untyped)
    pg.send(&[
        parse("ins", "INSERT INTO p VALUES ($1, $2, $3)", &[23, 0, 0]),
        sync(),
    ]);
    assert_eq!(tags(&pg.until(b'Z')), "1Z");
    for i in 0..5 {
        let (id, name, score) = (i.to_string(), format!("n{i}"), format!("{}.5", i));
        pg.send(&[
            bind("", "ins", &[Some(&id), Some(&name), Some(&score)], false),
            execute("", 0),
            sync(),
        ]);
        let m = pg.until(b'Z');
        assert_eq!(tags(&m), "2CZ", "insert {i}");
    }
    // describe a statement: parameter types and typed columns before execution
    pg.send(&[
        parse(
            "sel",
            "SELECT id, name FROM p WHERE score >= $1 ORDER BY id",
            &[],
        ),
        describe(b'S', "sel"),
        sync(),
    ]);
    let m = pg.until(b'Z');
    assert_eq!(tags(&m), "1tTZ");
    let t = &m[2].1;
    assert!(
        t.windows(4).any(|w| w == 20i32.to_be_bytes()),
        "id is described as int8"
    );
    // a row-limited execute suspends and resumes
    pg.send(&[
        bind("cur", "sel", &[Some("1")], false),
        execute("cur", 2),
        sync(),
    ]);
    let m = pg.until(b'Z');
    assert_eq!(tags(&m), "2DDsZ");
    pg.send(&[execute("cur", 0), sync()]);
    let m = pg.until(b'Z');
    assert_eq!(tags(&m), "DDCZ");
    assert_eq!(data_cells(&m[1].1)[1].as_deref(), Some(&b"n4"[..]));
    // binary results
    pg.send(&[
        bind("", "sel", &[Some("3.5")], true),
        execute("", 0),
        sync(),
    ]);
    let m = pg.until(b'Z');
    let cells = data_cells(&m.iter().find(|x| x.0 == b'D').unwrap().1);
    assert_eq!(cells[0].as_deref(), Some(&3i64.to_be_bytes()[..]));
    // wrong parameter count: an error, then everything up to Sync is skipped
    pg.send(&[bind("", "sel", &[], false), execute("", 0), sync()]);
    let m = pg.until(b'Z');
    assert_eq!(tags(&m), "EZ");
    // invalid parameter for a typed slot
    pg.send(&[
        bind("", "ins", &[Some("notanint"), Some("x"), Some("1")], false),
        execute("", 0),
        sync(),
    ]);
    assert_eq!(tags(&pg.until(b'Z')), "EZ");
    // close the statement: binding it afterwards fails
    let mut close = vec![b'S'];
    close.extend(cstr("sel"));
    pg.send(&[pg_msg(b'C', &close), sync()]);
    assert_eq!(tags(&pg.until(b'Z')), "3Z");
    pg.send(&[bind("", "sel", &[Some("1")], false), sync()]);
    assert_eq!(tags(&pg.until(b'Z')), "EZ");
    // statements are per connection
    let mut other = Pg::connect(r.postgres.unwrap());
    other.send(&[
        bind("", "ins", &[Some("9"), Some("x"), Some("1")], false),
        sync(),
    ]);
    assert_eq!(
        tags(&other.until(b'Z')),
        "EZ",
        "another connection cannot see 'ins'"
    );
    // untyped parameter compared with a text column stays text
    pg.send(&[
        parse("q", "SELECT id FROM p WHERE name = $1", &[0]),
        bind("", "q", &[Some("n3")], false),
        execute("", 0),
        sync(),
    ]);
    let m = pg.until(b'Z');
    let d = m.iter().find(|x| x.0 == b'D').unwrap();
    assert_eq!(data_cells(&d.1)[0].as_deref(), Some(&b"3"[..]));
}

#[test]
fn postgres_cancel_request_stops_a_running_query() {
    let (db, r) = start();
    let mut s = db.session();
    s.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    for i in 0..300 {
        s.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let mut pg = Pg::connect(r.postgres.unwrap());
    let (pid, key, addr) = (pg.pid, pg.key, r.postgres.unwrap());
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let mut c = TcpStream::connect(addr).unwrap();
        let mut m = 16i32.to_be_bytes().to_vec();
        m.extend(80877102i32.to_be_bytes());
        m.extend(pid.to_be_bytes());
        m.extend(key.to_be_bytes());
        c.write_all(&m).unwrap();
    });
    pg.send(&[pg_msg(b'Q', &cstr("SELECT COUNT(*) FROM t a, t b, t c"))]);
    let m = pg.until(b'Z');
    canceller.join().unwrap();
    let err = m
        .iter()
        .find(|x| x.0 == b'E')
        .expect("the query was cancelled");
    assert!(
        String::from_utf8_lossy(&err.1).contains("57014"),
        "SQLSTATE query_canceled"
    );
    pg.send(&[pg_msg(b'Q', &cstr("SELECT COUNT(*) FROM t"))]);
    assert!(
        tags(&pg.until(b'Z')).contains('D'),
        "the connection still works"
    );
}

// ------------------------------------------------------------------ Redis

fn redis(addr: SocketAddr) -> impl FnMut(&str) -> String {
    let s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut rd = BufReader::new(s);
    move |cmd: &str| {
        w.write_all(format!("{cmd}\r\n").as_bytes()).unwrap();
        let mut line = String::new();
        rd.read_line(&mut line).unwrap();
        if line.starts_with('$') && !line.starts_with("$-1") {
            let mut v = String::new();
            rd.read_line(&mut v).unwrap();
            return v.trim_end().to_string();
        }
        line.trim_end().to_string()
    }
}

#[test]
fn redis_commands_expiry_and_errors() {
    let (_db, r) = start();
    let mut ask = redis(r.redis.unwrap());
    assert_eq!(ask("SET a 1 EX 100"), "+OK");
    assert_eq!(ask("SET a 2 NX"), "$-1");
    assert_eq!(ask("SET b 2 XX"), "$-1");
    assert_eq!(ask("SET a 3 XX KEEPTTL"), "+OK");
    let ttl: i64 = ask("TTL a")[1..].parse().unwrap();
    assert!(ttl > 90 && ttl <= 100, "KEEPTTL kept the expiry: {ttl}");
    assert_eq!(ask("GET a"), "3");
    assert_eq!(ask("PERSIST a"), ":1");
    assert_eq!(ask("PERSIST a"), ":0");
    assert_eq!(ask("INCR a"), ":4");
    assert_eq!(ask("PEXPIRE a 30"), ":1");
    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(ask("EXISTS a"), ":0");
    assert_eq!(ask("TTL a"), ":-2");
    assert!(
        ask("SET a 1 EX 0").starts_with("-ERR"),
        "invalid expire time"
    );
    assert!(ask("SET a 1 BOGUS").starts_with("-ERR"));
    assert!(ask("SET a 1 NX XX").starts_with("-ERR"));
    assert!(
        ask("LPUSH l x").starts_with("-ERR"),
        "unsupported commands are errors, not crashes"
    );
    assert!(ask("GET").starts_with("-ERR"));
    assert_eq!(ask("SET n 9223372036854775807"), "+OK");
    assert!(ask("INCR n").starts_with("-ERR"), "overflow is an error");
    assert_eq!(ask("PING"), "+PONG");
}

// ---------------------------------------------------------------- MongoDB

fn mongo_cmd(s: &mut TcpStream, doc: &str) -> Value {
    let body = encode_doc(&Value::parse(doc).unwrap());
    let mut msg = Vec::new();
    msg.extend(((16 + 5 + body.len()) as i32).to_le_bytes());
    msg.extend(1i32.to_le_bytes());
    msg.extend(0i32.to_le_bytes());
    msg.extend(2013i32.to_le_bytes());
    msg.extend(0u32.to_le_bytes());
    msg.push(0);
    msg.extend(body);
    s.write_all(&msg).unwrap();
    let mut hdr = [0u8; 16];
    s.read_exact(&mut hdr).unwrap();
    let len = i32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    let mut rest = vec![0u8; len - 16];
    s.read_exact(&mut rest).unwrap();
    decode_doc(&rest[5..]).unwrap().0
}

#[test]
fn mongo_index_management() {
    let (_db, r) = start();
    let mut s = TcpStream::connect(r.mongo.unwrap()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    mongo_cmd(
        &mut s,
        "{insert: 'u', documents: [{_id: 1, e: 'a', n: 1}, {_id: 2, e: 'b', n: 2}], $db: 'test'}",
    );
    let r1 = mongo_cmd(&mut s, "{createIndexes: 'u', indexes: [{key: {e: 1}, name: 'e_u', unique: true}, {key: {n: -1, e: 1}, name: 'ne'}], $db: 'test'}");
    assert_eq!(
        r1.get("numIndexesAfter"),
        Some(&Value::Int(3)),
        "{}",
        r1.to_json()
    );
    let dup = mongo_cmd(
        &mut s,
        "{insert: 'u', documents: [{_id: 3, e: 'a'}], $db: 'test'}",
    );
    assert!(
        dup.get("writeErrors").is_some(),
        "unique index enforced over the wire: {}",
        dup.to_json()
    );
    let list = mongo_cmd(&mut s, "{listIndexes: 'u', $db: 'test'}");
    let names: Vec<String> = match list.get("cursor").and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|i| i.get("name").and_then(Value::as_str).map(String::from))
            .collect(),
        _ => vec![],
    };
    assert_eq!(names, vec!["_id_", "e_u", "ne"], "{}", list.to_json());
    assert_eq!(
        mongo_cmd(&mut s, "{dropIndexes: 'u', index: 'ne', $db: 'test'}").get("ok"),
        Some(&Value::Float(1.0))
    );
    let missing = mongo_cmd(&mut s, "{dropIndexes: 'u', index: 'ne', $db: 'test'}");
    assert_eq!(
        missing.get("codeName"),
        Some(&Value::Text("IndexNotFound".into()))
    );
    mongo_cmd(&mut s, "{dropIndexes: 'u', index: '*', $db: 'test'}");
    let list = mongo_cmd(&mut s, "{listIndexes: 'u', $db: 'test'}");
    assert!(
        matches!(list.get("cursor").and_then(|c| c.get("firstBatch")), Some(Value::Array(a)) if a.len() == 1)
    );
}

// ----------------------------------------------------------------- native

#[test]
fn native_protocol_transactions_and_errors() {
    let (_db, r) = start();
    let s = TcpStream::connect(r.native.unwrap()).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut rd = BufReader::new(s);
    let mut ask = |cmd: &str| {
        w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
        let mut l = String::new();
        rd.read_line(&mut l).unwrap();
        Value::parse(l.trim()).unwrap()
    };
    ask("CREATE TABLE t (id INT PRIMARY KEY)");
    ask("BEGIN");
    ask("INSERT INTO t VALUES (1)");
    let e = ask("INSERT INTO t VALUES (1)");
    assert_eq!(e.get("kind"), Some(&Value::Text("constraint".into())));
    ask("ROLLBACK");
    let c = ask("SELECT COUNT(*) FROM t");
    assert_eq!(c.get("rows"), Some(&Value::parse("[[0]]").unwrap()));
    let p = ask("EXPLAIN SELECT * FROM t WHERE id = 1");
    assert!(p.to_json().contains("PrimaryKeyLookup"));
    assert_eq!(
        ask("SELEKT").get("kind"),
        Some(&Value::Text("parse".into()))
    );
}
