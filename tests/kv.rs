//! Key-value model: commands, TTL and expiry cleanup, restart, namespaces,
//! concurrency, large values and errors.

mod common;
use common::*;
use polydb::{Database, Error, Output, Value};
use std::time::Duration;

#[test]
fn basic_commands_across_interfaces() {
    let mut db = Database::in_memory();
    ok(&mut db, "SET a 1");
    ok(&mut db, "SET KV 'b' = 'two'");
    assert_eq!(
        ok(&mut db, "GET a"),
        Output::Scalar(Value::Text("1".into()))
    );
    assert_eq!(
        ok(&mut db, "GET FROM KV 'b'"),
        Output::Scalar(Value::Text("two".into()))
    );
    assert_eq!(ok(&mut db, "EXISTS a"), Output::Scalar(Value::Bool(true)));
    assert_eq!(
        ok(&mut db, "EXISTS KV 'nope'"),
        Output::Scalar(Value::Bool(false))
    );
    assert_eq!(ok(&mut db, "INCR a 9"), Output::Scalar(Value::Int(10)));
    assert_eq!(
        ok(&mut db, "INCREMENT KV 'a' BY -3"),
        Output::Scalar(Value::Int(7))
    );
    assert!(matches!(db.execute("INCR b"), Err(Error::Type(_))));
    assert_eq!(ok(&mut db, "DEL a"), Output::Scalar(Value::Int(1)));
    assert_eq!(ok(&mut db, "DEL a"), Output::Scalar(Value::Int(0)));
    assert!(matches!(db.execute("SET onlykey"), Err(Error::Parse(_))));
    assert!(matches!(db.execute("GET a b"), Err(Error::Parse(_))));
    // namespaces are isolated from each other and from the default keyspace
    ok(&mut db, "SET KV.x 'k' = '1'");
    ok(&mut db, "SET KV.y 'k' = '2'");
    assert_eq!(
        ok(&mut db, "GET FROM KV.x 'k'"),
        Output::Scalar(Value::Text("1".into()))
    );
    assert_eq!(
        ok(&mut db, "GET FROM KV.y 'k'"),
        Output::Scalar(Value::Text("2".into()))
    );
    assert_eq!(ok(&mut db, "GET k"), Output::Scalar(Value::Null));
    assert_eq!(
        rows(&mut db, "SELECT key FROM KV.x"),
        vec![vec![Value::Text("k".into())]]
    );
}

#[test]
fn ttl_reporting_persist_and_expiry() {
    let mut db = Database::in_memory();
    db.kv_set_ttl("t", b"v", Some(300)).unwrap();
    let left = db.kv_ttl("t").unwrap().unwrap();
    assert!(left > 200 && left <= 300, "{left}");
    assert!(db.kv_expire("t", None).unwrap());
    assert_eq!(db.kv_ttl("t").unwrap(), Some(-1));
    assert!(db.kv_expire("t", Some(50)).unwrap());
    assert!(!db.kv_expire("missing", Some(50)).unwrap());
    std::thread::sleep(Duration::from_millis(120));
    assert_eq!(db.kv_get("t").unwrap(), None);
    assert_eq!(db.kv_ttl("t").unwrap(), None);
    assert!(
        !db.kv_expire("t", Some(1000)).unwrap(),
        "an expired key cannot be revived by EXPIRE"
    );
    // EXPIRE 0 deletes immediately
    db.kv_set("z", b"1").unwrap();
    db.kv_expire("z", Some(0)).unwrap();
    assert_eq!(db.kv_get("z").unwrap(), None);
}

#[test]
fn background_cleanup_removes_expired_entries_without_writes() {
    let mut db = Database::in_memory();
    let baseline = db.entry_count();
    for i in 0..200 {
        db.kv_set_ttl(&format!("tmp{i}"), b"x", Some(50)).unwrap();
    }
    db.kv_set("keep", b"1").unwrap();
    assert!(
        db.entry_count() >= baseline + 200 * 3,
        "value + ttl + expiry index per key"
    );
    // the reaper runs every 200 ms; no reads or writes from us in the meantime
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while db.entry_count() > baseline + 1 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        db.entry_count(),
        baseline + 1,
        "expired keys and their metadata were removed"
    );
    assert_eq!(db.kv_keys("").unwrap(), vec!["keep".to_string()]);
}

#[test]
fn expired_keys_stay_gone_after_restart() {
    let dir = tmp("kv-restart");
    {
        let mut db = Database::open(&dir).unwrap();
        db.kv_set_ttl("short", b"x", Some(30)).unwrap();
        db.kv_set_ttl("long", b"y", Some(60_000)).unwrap();
        db.kv_set("forever", b"z").unwrap();
    }
    std::thread::sleep(Duration::from_millis(80));
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(db.kv_get("short").unwrap(), None);
    assert_eq!(db.kv_get("long").unwrap(), Some(b"y".to_vec()));
    let ttl = db.kv_ttl("long").unwrap().unwrap();
    assert!(ttl > 50_000, "the absolute expiry survives restart: {ttl}");
    assert_eq!(
        db.kv_keys("").unwrap(),
        vec!["forever".to_string(), "long".to_string()]
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn ttl_is_transactional() {
    let mut db = Database::in_memory();
    db.kv_set("k", b"1").unwrap();
    db.begin().unwrap();
    db.kv_expire("k", Some(10)).unwrap();
    db.rollback().unwrap();
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(
        db.kv_get("k").unwrap(),
        Some(b"1".to_vec()),
        "a rolled-back EXPIRE has no effect"
    );
    assert_eq!(db.kv_ttl("k").unwrap(), Some(-1));
}

#[test]
fn concurrent_increments_and_large_values() {
    let db = Database::in_memory();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let mut s = db.session();
            std::thread::spawn(move || {
                for _ in 0..250 {
                    s.kv_incr("hits", 1).unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let mut s = db.session();
    assert_eq!(s.kv_get("hits").unwrap(), Some(b"2000".to_vec()));
    let big: Vec<u8> = (0..(4 << 20)).map(|i| (i % 251) as u8).collect();
    s.kv_set("blob", &big).unwrap();
    assert_eq!(
        s.kv_get("blob").unwrap().as_deref(),
        Some(&big[..]),
        "binary-safe large value"
    );
}

#[test]
fn many_keys_prefix_and_range_queries() {
    let mut db = Database::in_memory();
    db.begin().unwrap();
    for i in 0..dataset_size() {
        db.kv_set(&format!("user:{i:06}"), format!("{i}").as_bytes())
            .unwrap();
    }
    db.commit().unwrap();
    let n = dataset_size() as i64;
    assert_eq!(
        scalar(&mut db, "SELECT COUNT(*) FROM KV WHERE key LIKE 'user:%'"),
        Value::Int(n)
    );
    assert_eq!(
        scalar(
            &mut db,
            "SELECT COUNT(*) FROM KV WHERE key >= 'user:000100' AND key < 'user:000200'"
        ),
        Value::Int(100)
    );
    assert!(
        plan(&mut db, "SELECT key FROM KV WHERE key LIKE 'user:0001%'").contains("KVPrefixScan")
    );
    assert_eq!(db.kv_keys("user:0001").unwrap().len(), 100);
}
