//! Regression tests for bugs found by the property and differential tests.

mod common;
use common::*;
use polydb::{Database, Value};

/// An inverted range (`BETWEEN 10 AND 5`) on an indexed column made
/// BTreeMap::range panic, killing the connection thread.
#[test]
fn inverted_range_on_an_index_is_empty_not_a_panic() {
    let mut db = Database::in_memory();
    ok(
        &mut db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)",
    );
    ok(&mut db, "CREATE INDEX t_a ON t (a)");
    ok(&mut db, "CREATE INDEX t_ab ON t (a, b)");
    ok(&mut db, "INSERT INTO t VALUES (1, 7, 'x'), (2, 8, 'y')");
    for q in [
        "SELECT id FROM t WHERE a BETWEEN 10 AND 5",
        "SELECT id FROM t WHERE a > 9 AND a < 3",
        "SELECT id FROM t WHERE a = 7 AND b > 'z' AND b < 'a'",
        "SELECT id FROM t WHERE id > 5 AND id < 2",
        "SELECT key FROM KV WHERE key > 'z' AND key < 'a'",
    ] {
        assert_eq!(rows(&mut db, q), Vec::<Vec<Value>>::new(), "{q}");
    }
    ok(&mut db, "db.c.createIndex({v: 1})");
    ok(&mut db, "db.c.insert({v: 5})");
    assert_eq!(rows(&mut db, "db.c.find({v: {$gt: 9, $lt: 2}})").len(), 0);
}

/// Rows with a NULL in a non-leading column of a composite index had no
/// index entry, so lookups on the leading column missed them.
#[test]
fn composite_index_includes_rows_with_nulls() {
    let mut db = Database::in_memory();
    ok(
        &mut db,
        "CREATE TABLE t (id INT PRIMARY KEY, a INT, c REAL)",
    );
    ok(&mut db, "CREATE INDEX t_ac ON t (a, c)");
    ok(
        &mut db,
        "INSERT INTO t VALUES (1, 24, 1.5), (2, 24, NULL), (3, NULL, 2.0)",
    );
    assert!(plan(&mut db, "SELECT id FROM t WHERE a >= 22").contains("t_ac"));
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE a >= 22 ORDER BY id"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE a = 24 ORDER BY id"),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );
    ok(&mut db, "CREATE UNIQUE INDEX t_u ON t (a, c)");
    ok(&mut db, "INSERT INTO t VALUES (4, 24, NULL)"); // NULLs never conflict
}

/// A document index declared descending was read as if stored descending.
#[test]
fn descending_document_index_sorts_correctly() {
    let mut db = Database::in_memory();
    ok(&mut db, "db.c.createIndex({b: -1})");
    ok(
        &mut db,
        "db.c.insert([{_id: 1, b: 3.5}, {_id: 2, b: 7}, {_id: 3, b: 5}, {_id: 4}])",
    );
    let got: Vec<Value> = rows(&mut db, "db.c.find({b: {$gte: 3}}).sort({b: -1})")
        .into_iter()
        .map(|r| r[0].get("b").cloned().unwrap())
        .collect();
    assert_eq!(got, vec![Value::Int(7), Value::Int(5), Value::Float(3.5)]);
    let got: Vec<Value> = rows(&mut db, "db.c.find({}).sort({b: 1})")
        .into_iter()
        .map(|r| r[0].get("b").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(
        got,
        vec![Value::Null, Value::Float(3.5), Value::Int(5), Value::Int(7)]
    );
    let p = rows(
        &mut db,
        "SELECT b FROM DOCUMENT.c WHERE b > 0 ORDER BY b DESC",
    );
    assert_eq!(
        p,
        vec![
            vec![Value::Int(7)],
            vec![Value::Int(5)],
            vec![Value::Float(3.5)]
        ]
    );
}
