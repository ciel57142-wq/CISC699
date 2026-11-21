//! Storage safety: WAL recovery edge cases, process safety and locking,
//! concurrency, and resource limits.

mod common;
use common::*;
use polydb::server::{self, Config, Limits};
use polydb::{Database, Error, Output, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_polydb");

fn count(db: &mut Database, t: &str) -> i64 {
    scalar(db, &format!("SELECT COUNT(*) FROM {t}"))
        .as_i64()
        .unwrap()
}

#[test]
fn clean_shutdown_and_repeated_recovery_are_stable() {
    let dir = tmp("rec-repeat");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, v TEXT)");
        for i in 0..50 {
            ok(&mut db, &format!("INSERT INTO t VALUES ({i}, 'v{i}')"));
        }
    }
    // tear the last record, then recover several times: the state must be identical each time
    let wal = dir.join("data.wal");
    let bytes = fs::read(&wal).unwrap();
    fs::write(&wal, &bytes[..bytes.len() - 3]).unwrap();
    let mut states = Vec::new();
    for _ in 0..4 {
        let mut db = Database::open(&dir).unwrap();
        states.push(dump(&mut db));
    }
    assert!(states.windows(2).all(|w| w[0] == w[1]));
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        count(&mut db, "t"),
        49,
        "only the torn final commit is lost"
    );
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn uncommitted_transactions_do_not_survive() {
    let dir = tmp("rec-uncommitted");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)");
        ok(&mut db, "INSERT INTO t VALUES (1)");
        ok(&mut db, "BEGIN");
        ok(&mut db, "INSERT INTO t VALUES (2)");
        ok(&mut db, "SET k v");
        ok(&mut db, "db.c.insert({_id: 1})");
        // dropped without COMMIT: like a crash mid-transaction
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(count(&mut db, "t"), 1);
    assert_eq!(db.kv_get("k").unwrap(), None);
    assert_eq!(scalar(&mut db, "db.c.count({})"), Value::Int(0));
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn interrupted_checkpoints_recover() {
    let dir = tmp("rec-checkpoint");
    let expected;
    {
        let mut db = Database::open(&dir).unwrap();
        load_business(&mut db, 200, seed());
        expected = dump(&mut db);
        db.checkpoint().unwrap();
    }
    // 1. a crash while writing the next snapshot leaves a stray temporary file
    fs::write(dir.join("data.snap.tmp"), b"half a snapshot").unwrap();
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(dump(&mut db), expected);
    }
    // 2. a crash after the snapshot rename but before the log was emptied:
    //    the old log is replayed over the new snapshot, which is idempotent
    let mut db = Database::open(&dir).unwrap();
    ok(
        &mut db,
        "UPDATE customers SET city = 'Recovered' WHERE id = 1",
    );
    ok(&mut db, "SET after:checkpoint yes");
    let after = dump(&mut db);
    drop(db);
    let wal = fs::read(dir.join("data.wal")).unwrap();
    {
        let mut db = Database::open(&dir).unwrap();
        db.checkpoint().unwrap();
    }
    fs::write(dir.join("data.wal"), &wal).unwrap(); // the log that should have been emptied
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(dump(&mut db), after);
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corruption_after_committed_data_is_detected_and_salvageable() {
    let dir = tmp("rec-corrupt");
    let committed;
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)");
        for i in 0..20 {
            ok(&mut db, &format!("INSERT INTO t VALUES ({i})"));
        }
        committed = fs::metadata(dir.join("data.wal")).unwrap().len() as usize;
        for i in 20..40 {
            ok(&mut db, &format!("INSERT INTO t VALUES ({i})"));
        }
    }
    let wal = dir.join("data.wal");
    let mut bytes = fs::read(&wal).unwrap();
    bytes[committed + 12] ^= 0x55; // damage a record after 20 good commits, with more after it
    fs::write(&wal, &bytes).unwrap();
    assert!(
        matches!(Database::open(&dir), Err(Error::Corrupt(_))),
        "interior damage must not be silently truncated"
    );
    let mut db =
        Database::open_with(&dir, &polydb::storage::OpenOptions { salvage: true }).unwrap();
    assert_eq!(
        count(&mut db, "t"),
        20,
        "every commit before the damage is kept"
    );
    drop(db);
    // after salvage the log is clean again
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(count(&mut db, "t"), 20);
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

fn wait_for(addr: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn killing_a_writing_server_loses_no_acknowledged_write_and_releases_the_lock() {
    let dir = tmp("rec-kill");
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut child = Command::new(BIN)
        .args([
            "serve",
            dir.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--redis-port",
            "off",
            "--pg-port",
            "off",
            "--mongo-port",
            "off",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    wait_for(addr);
    // the directory is locked while the server runs
    assert!(matches!(Database::open(&dir), Err(Error::Io(_))));
    let s = TcpStream::connect(addr).unwrap();
    let mut w = s.try_clone().unwrap();
    let mut rd = BufReader::new(s);
    let mut line = String::new();
    w.write_all(b"CREATE TABLE t (id INT PRIMARY KEY, payload TEXT)\n")
        .unwrap();
    rd.read_line(&mut line).unwrap();
    // write until killed; remember what was acknowledged
    let killer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        child.kill().unwrap();
        child.wait().unwrap();
    });
    let mut acked = Vec::new();
    for i in 0.. {
        if w.write_all(format!("INSERT INTO t VALUES ({i}, '{}')\n", "x".repeat(100)).as_bytes())
            .is_err()
        {
            break;
        }
        line.clear();
        match rd.read_line(&mut line) {
            Ok(n) if n > 0 && line.contains("\"ok\":true") => acked.push(i),
            _ => break,
        }
    }
    killer.join().unwrap();
    assert!(!acked.is_empty());
    let mut db = Database::open(&dir).expect("the lock is released when the process dies");
    let present: Vec<i64> = rows(&mut db, "SELECT id FROM t ORDER BY id")
        .into_iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect();
    for i in &acked {
        assert!(
            present.binary_search(i).is_ok(),
            "acknowledged write {i} lost ({} acked, {} present)",
            acked.len(),
            present.len()
        );
    }
    assert!(
        present.len() <= acked.len() + 1,
        "at most the in-flight write is extra"
    );
    drop(db);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn concurrent_transfers_keep_the_total_and_counters_are_exact() {
    let db = Database::in_memory();
    let mut s = db.session();
    ok(&mut s, "CREATE TABLE acct (id INT PRIMARY KEY, bal INT)");
    for i in 0..10 {
        ok(&mut s, &format!("INSERT INTO acct VALUES ({i}, 1000)"));
    }
    let mut handles = Vec::new();
    for t in 0..6u64 {
        let mut s = db.session();
        handles.push(std::thread::spawn(move || {
            let mut r = Rng::new(t);
            let mut done = 0;
            let mut incrs = 0u64;
            while done < 60 {
                let (a, b) = (r.below(10), r.below(10));
                let amt = r.range(1, 50);
                let res = (|| -> polydb::Result<()> {
                    s.begin()?;
                    s.execute(&format!("UPDATE acct SET bal = bal - {amt} WHERE id = {a}"))?;
                    s.execute(&format!("UPDATE acct SET bal = bal + {amt} WHERE id = {b}"))?;
                    s.commit()
                })();
                match res {
                    Ok(()) => done += 1,
                    Err(Error::Conflict(_)) => {}
                    Err(e) => {
                        let _ = s.rollback();
                        panic!("{e}");
                    }
                }
                if t % 2 == 0 {
                    s.execute("INCR counter").unwrap();
                    incrs += 1;
                }
            }
            incrs
        }));
    }
    // readers always see a consistent total while transfers run
    let mut reader = db.session();
    for _ in 0..50 {
        assert_eq!(
            scalar(&mut reader, "SELECT SUM(bal) FROM acct"),
            Value::Int(10_000)
        );
    }
    let mut total = 0;
    for h in handles {
        total += h.join().unwrap();
    }
    assert_eq!(
        scalar(&mut reader, "SELECT SUM(bal) FROM acct"),
        Value::Int(10_000)
    );
    assert_eq!(
        reader.kv_get("counter").unwrap(),
        Some(total.to_string().into_bytes()),
        "no increment lost"
    );
}

#[test]
fn large_values_and_large_results() {
    let mut db = Database::in_memory();
    let big = "z".repeat(8 << 20);
    db.kv_set("big", big.as_bytes()).unwrap();
    assert_eq!(db.kv_get("big").unwrap().map(|v| v.len()), Some(8 << 20));
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    db.begin().unwrap();
    for chunk in 0..50 {
        let vals: Vec<String> = (0..1000)
            .map(|i| format!("({}, {})", chunk * 1000 + i, i))
            .collect();
        ok(&mut db, &format!("INSERT INTO t VALUES {}", vals.join(",")));
    }
    db.commit().unwrap();
    match ok(&mut db, "SELECT id, v FROM t") {
        Output::Rows { rows, .. } => assert_eq!(rows.len(), 50_000),
        other => panic!("{other:?}"),
    }
    // and a large query text
    let in_list: Vec<String> = (0..20_000).map(|i| i.to_string()).collect();
    assert_eq!(
        scalar(
            &mut db,
            &format!("SELECT COUNT(*) FROM t WHERE id IN ({})", in_list.join(","))
        ),
        Value::Int(20_000)
    );
}

#[test]
fn connection_limit_and_malformed_messages() {
    let db = Database::in_memory();
    let cfg = Config {
        host: "127.0.0.1".into(),
        native: Some(0),
        redis: Some(0),
        postgres: Some(0),
        mongo: Some(0),
    };
    let r = server::start_with_limits(
        &db,
        &cfg,
        Limits {
            max_connections: 2,
            max_request_bytes: 1 << 16,
            ..Default::default()
        },
    )
    .unwrap();
    let native = r.native.unwrap();
    let a = TcpStream::connect(native).unwrap();
    let b = TcpStream::connect(native).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut c = TcpStream::connect(native).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let _ = c.write_all(b"GET x\n");
    let mut buf = [0u8; 8];
    assert_eq!(
        c.read(&mut buf).unwrap_or(0),
        0,
        "a third connection is refused"
    );
    drop((a, b));
    std::thread::sleep(Duration::from_millis(200));
    // invalid lengths on each binary protocol close the connection, not the server
    let pg = r.postgres.unwrap();
    let mongo = r.mongo.unwrap();
    for (addr, bytes) in [
        (pg, vec![0x7f, 0xff, 0xff, 0xff, 0, 3, 0, 0]), // absurd startup length
        (
            mongo,
            vec![
                0xff, 0xff, 0xff, 0x7f, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 7, 0, 0,
            ],
        ), // 2 GB OP_MSG
        (
            mongo,
            vec![10, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 7, 0, 0],
        ), // shorter than a header
        (r.redis.unwrap(), b"*99999999999\r\n".to_vec()),
        (r.redis.unwrap(), b"$-5\r\n".to_vec()),
    ] {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let _ = s.write_all(&bytes);
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink); // the server closes (or answers and closes)
    }
    let mut s = db.session();
    s.execute("SET still alive").unwrap();
    let t = TcpStream::connect(native).unwrap();
    let mut w = t.try_clone().unwrap();
    w.write_all(b"GET still\n").unwrap();
    let mut line = String::new();
    BufReader::new(t).read_line(&mut line).unwrap();
    assert!(line.contains("alive"), "{line}");
}
