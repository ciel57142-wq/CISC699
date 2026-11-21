//! Tests for the relational extensions, ordered indexes, the shared planner,
//! EXPLAIN and PBSQL (document, key-value and cross-model queries).

use polydb::{Database, Error, Output, Value};
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-engine-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

fn rows(db: &mut Database, q: &str) -> Vec<Vec<Value>> {
    match db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{q}: expected rows, got {other:?}"),
    }
}

fn cols(db: &mut Database, q: &str) -> Vec<String> {
    match db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        Output::Rows { columns, .. } => columns,
        other => panic!("{q}: expected rows, got {other:?}"),
    }
}

fn ok(db: &mut Database, q: &str) -> Output {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn err(db: &mut Database, q: &str) -> Error {
    match db.execute(q) {
        Ok(o) => panic!("{q}: expected an error, got {o:?}"),
        Err(e) => e,
    }
}

fn plan(db: &mut Database, q: &str) -> String {
    rows(db, &format!("EXPLAIN {q}")).into_iter().map(|r| r[0].display()).collect::<Vec<_>>().join("\n")
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}
fn i(n: i64) -> Value {
    Value::Int(n)
}

// ------------------------------------------------------------ relational

#[test]
fn alter_table_add_rename_drop_column_persists() {
    let dir = tmp("alter");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE u (id INT PRIMARY KEY, name TEXT)");
        ok(&mut db, "INSERT INTO u VALUES (1, 'a'), (2, 'b')");
        ok(&mut db, "ALTER TABLE u ADD COLUMN age INT DEFAULT 18");
        assert_eq!(rows(&mut db, "SELECT id, age FROM u ORDER BY id"), vec![vec![i(1), i(18)], vec![i(2), i(18)]]);
        ok(&mut db, "INSERT INTO u (id, name) VALUES (3, 'c')");
        assert_eq!(rows(&mut db, "SELECT age FROM u WHERE id = 3"), vec![vec![i(18)]]);
        assert!(matches!(err(&mut db, "ALTER TABLE u ADD COLUMN x INT NOT NULL"), Error::Constraint(_)));
        ok(&mut db, "ALTER TABLE u RENAME COLUMN name TO full_name");
        assert_eq!(rows(&mut db, "SELECT full_name FROM u WHERE id = 1"), vec![vec![t("a")]]);
        assert!(matches!(err(&mut db, "SELECT name FROM u"), Error::Schema(_)));
        ok(&mut db, "CREATE INDEX u_age ON u (age)");
        ok(&mut db, "ALTER TABLE u DROP COLUMN age");
        assert_eq!(cols(&mut db, "SELECT * FROM u"), vec!["id", "full_name"]);
        assert!(matches!(err(&mut db, "ALTER TABLE u DROP COLUMN id"), Error::Schema(_)));
        ok(&mut db, "ALTER TABLE u RENAME TO people");
        assert!(matches!(err(&mut db, "SELECT * FROM u"), Error::Schema(_)));
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id, full_name FROM people ORDER BY id"),
        vec![vec![i(1), t("a")], vec![i(2), t("b")], vec![i(3), t("c")]]
    );
    ok(&mut db, "INSERT INTO people VALUES (4, 'd')");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM people"), vec![vec![i(4)]]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unique_constraints_single_and_multi_column() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (id INT PRIMARY KEY, email TEXT UNIQUE, first TEXT, last TEXT, UNIQUE (first, last))");
    ok(&mut db, "INSERT INTO a VALUES (1, 'x@y', 'Ada', 'L')");
    assert!(matches!(err(&mut db, "INSERT INTO a VALUES (2, 'x@y', 'B', 'C')"), Error::Constraint(_)));
    assert!(matches!(err(&mut db, "INSERT INTO a VALUES (2, 'z@y', 'Ada', 'L')"), Error::Constraint(_)));
    ok(&mut db, "INSERT INTO a VALUES (2, 'z@y', 'Ada', 'M')");
    // NULLs never collide
    ok(&mut db, "INSERT INTO a VALUES (3, NULL, 'N', NULL), (4, NULL, 'N', NULL)");
    // UPDATE is checked too, and a row may keep its own value
    assert!(matches!(err(&mut db, "UPDATE a SET email = 'x@y' WHERE id = 2"), Error::Constraint(_)));
    ok(&mut db, "UPDATE a SET email = 'x@y' WHERE id = 1");
    // within one statement, and atomically
    assert!(matches!(err(&mut db, "INSERT INTO a VALUES (5, 'q', 'Q', 'Q'), (6, 'q', 'R', 'R')"), Error::Constraint(_)));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM a"), vec![vec![i(4)]]);
    // unique index on existing duplicate data is refused
    ok(&mut db, "CREATE TABLE d (v INT)");
    ok(&mut db, "INSERT INTO d VALUES (1), (1)");
    assert!(matches!(err(&mut db, "CREATE UNIQUE INDEX d_v ON d (v)"), Error::Constraint(_)));
    // and the lookup goes through the unique index
    assert!(plan(&mut db, "SELECT * FROM a WHERE email = 'x@y'").contains("IndexScan on a using a_email_key"));
}

#[test]
fn unique_in_a_transaction_and_rollback() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (id INT PRIMARY KEY, email TEXT UNIQUE)");
    ok(&mut db, "BEGIN");
    ok(&mut db, "INSERT INTO a VALUES (1, 'x')");
    assert!(matches!(err(&mut db, "INSERT INTO a VALUES (2, 'x')"), Error::Constraint(_)));
    ok(&mut db, "ROLLBACK");
    ok(&mut db, "INSERT INTO a VALUES (2, 'x')");
    assert_eq!(rows(&mut db, "SELECT id FROM a WHERE email = 'x'"), vec![vec![i(2)]]);
}

#[test]
fn defaults_and_explicit_precedence() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE e (id INT PRIMARY KEY, status TEXT NOT NULL DEFAULT 'new', n INT DEFAULT -1, at TIMESTAMP DEFAULT CURRENT_TIMESTAMP)");
    ok(&mut db, "INSERT INTO e (id) VALUES (1)");
    ok(&mut db, "INSERT INTO e (id, status, n) VALUES (2, 'done', 5)");
    ok(&mut db, "INSERT INTO e VALUES (3, DEFAULT, DEFAULT, DEFAULT)");
    let r = rows(&mut db, "SELECT id, status, n, at IS NOT NULL FROM e ORDER BY id");
    assert_eq!(r[0], vec![i(1), t("new"), i(-1), Value::Bool(true)]);
    assert_eq!(r[1][1..3], [t("done"), i(5)]);
    assert_eq!(r[2][1..3], [t("new"), i(-1)]);
    assert!(matches!(err(&mut db, "CREATE TABLE bad (id INT DEFAULT 'x')"), Error::Type(_)));
}

#[test]
fn composite_primary_keys() {
    let dir = tmp("cpk");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE enroll (student INT, course TEXT, grade REAL, PRIMARY KEY (student, course))");
        ok(&mut db, "INSERT INTO enroll VALUES (1, 'math', 3.5), (1, 'art', 4.0), (2, 'math', 2.0)");
        assert!(matches!(err(&mut db, "INSERT INTO enroll VALUES (1, 'math', 1.0)"), Error::Constraint(_)));
        assert!(matches!(err(&mut db, "INSERT INTO enroll VALUES (NULL, 'x', 1.0)"), Error::Constraint(_)));
        assert!(matches!(err(&mut db, "UPDATE enroll SET course = 'art' WHERE student = 1 AND course = 'math'"), Error::Constraint(_)));
        ok(&mut db, "UPDATE enroll SET course = 'bio' WHERE student = 1 AND course = 'math'");
        assert!(plan(&mut db, "SELECT * FROM enroll WHERE student = 1 AND course = 'art'").contains("PrimaryKeyLookup"));
        assert!(plan(&mut db, "SELECT * FROM enroll WHERE student = 1").contains("PrimaryKeyRangeScan"));
        assert_eq!(rows(&mut db, "SELECT course FROM enroll WHERE student = 1 ORDER BY course"), vec![vec![t("art")], vec![t("bio")]]);
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(rows(&mut db, "SELECT grade FROM enroll WHERE student = 1 AND course = 'bio'"), vec![vec![Value::Float(3.5)]]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn foreign_keys_restrict_cascade_set_null() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");
    ok(&mut db, "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id) ON DELETE CASCADE, item TEXT)");
    ok(&mut db, "CREATE TABLE notes (id INT PRIMARY KEY, user_id INT, FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE SET NULL)");
    ok(&mut db, "CREATE TABLE audits (id INT PRIMARY KEY, user_id INT REFERENCES users)");
    ok(&mut db, "INSERT INTO users VALUES (1, 'a'), (2, 'b'), (3, 'c')");
    assert!(matches!(err(&mut db, "INSERT INTO orders VALUES (1, 9, 'x')"), Error::Constraint(_)));
    ok(&mut db, "INSERT INTO orders VALUES (1, 1, 'x'), (2, 1, 'y'), (3, 2, 'z'), (4, NULL, 'w')");
    ok(&mut db, "INSERT INTO notes VALUES (1, 1), (2, 2)");
    ok(&mut db, "INSERT INTO audits VALUES (1, 3)");
    assert!(matches!(err(&mut db, "UPDATE orders SET user_id = 7 WHERE id = 1"), Error::Constraint(_)));
    // RESTRICT
    assert!(matches!(err(&mut db, "DELETE FROM users WHERE id = 3"), Error::Constraint(_)));
    assert!(matches!(err(&mut db, "UPDATE users SET id = 30 WHERE id = 3"), Error::Constraint(_)));
    // CASCADE + SET NULL in one statement
    assert_eq!(ok(&mut db, "DELETE FROM users WHERE id = 1"), Output::Done("1 row(s) deleted".into()));
    assert_eq!(rows(&mut db, "SELECT id FROM orders ORDER BY id"), vec![vec![i(3)], vec![i(4)]]);
    assert_eq!(rows(&mut db, "SELECT user_id FROM notes WHERE id = 1"), vec![vec![Value::Null]]);
    // a failed cascade leaves nothing behind
    ok(&mut db, "INSERT INTO audits VALUES (2, 2)");
    assert!(matches!(err(&mut db, "DELETE FROM users WHERE id = 2"), Error::Constraint(_)));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM orders WHERE user_id = 2"), vec![vec![i(1)]]);
    assert!(matches!(err(&mut db, "DROP TABLE users"), Error::Constraint(_)));
    // referencing a non-key column is refused
    assert!(matches!(err(&mut db, "CREATE TABLE bad (x TEXT REFERENCES users(name))"), Error::Schema(_)));
}

#[test]
fn subqueries_in_exists_scalar_and_from() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE users (id INT PRIMARY KEY, name TEXT, age INT)");
    ok(&mut db, "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total REAL)");
    ok(&mut db, "INSERT INTO users VALUES (1, 'Ada', 36), (2, 'Linus', 54), (3, 'Grace', NULL)");
    ok(&mut db, "INSERT INTO orders VALUES (1, 1, 10.0), (2, 1, 5.0), (3, 2, 7.5)");
    assert_eq!(
        rows(&mut db, "SELECT name FROM users WHERE id IN (SELECT user_id FROM orders WHERE total > 6) ORDER BY name"),
        vec![vec![t("Ada")], vec![t("Linus")]]
    );
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)"), vec![vec![t("Grace")]]);
    // NOT IN with a NULL in the list is unknown -> no rows
    ok(&mut db, "INSERT INTO orders VALUES (4, NULL, 1.0)");
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE id NOT IN (SELECT user_id FROM orders)"), Vec::<Vec<Value>>::new());
    // correlated EXISTS and scalar subqueries
    assert_eq!(
        rows(&mut db, "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total < 6)"),
        vec![vec![t("Ada")]]
    );
    assert_eq!(
        rows(&mut db, "SELECT name, (SELECT SUM(total) FROM orders o WHERE o.user_id = u.id) AS spent FROM users u ORDER BY id"),
        vec![vec![t("Ada"), Value::Float(15.0)], vec![t("Linus"), Value::Float(7.5)], vec![t("Grace"), Value::Null]]
    );
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE age > (SELECT AVG(age) FROM users)"), vec![vec![t("Linus")]]);
    assert!(matches!(err(&mut db, "SELECT (SELECT id FROM users) FROM users"), Error::Type(_)));
    // derived table
    assert_eq!(
        rows(&mut db, "SELECT s.uid, s.n FROM (SELECT user_id AS uid, COUNT(*) AS n FROM orders WHERE user_id IS NOT NULL GROUP BY user_id) AS s ORDER BY s.n DESC"),
        vec![vec![i(1), i(2)], vec![i(2), i(1)]]
    );
    // subqueries in UPDATE / DELETE
    ok(&mut db, "DELETE FROM orders WHERE user_id NOT IN (SELECT id FROM users) OR user_id IS NULL");
    ok(&mut db, "UPDATE users SET age = (SELECT COUNT(*) FROM orders o WHERE o.user_id = users.id) WHERE age IS NULL");
    assert_eq!(rows(&mut db, "SELECT age FROM users WHERE id = 3"), vec![vec![i(0)]]);
    let p = plan(&mut db, "SELECT u.name FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.user_id = u.id)");
    assert!(p.contains("SubPlan 0 (correlated"), "{p}");
}

#[test]
fn union_and_union_all() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (v INT)");
    ok(&mut db, "CREATE TABLE b (v INT)");
    ok(&mut db, "INSERT INTO a VALUES (1), (2), (2), (NULL)");
    ok(&mut db, "INSERT INTO b VALUES (2), (3), (NULL)");
    assert_eq!(rows(&mut db, "SELECT v FROM a UNION SELECT v FROM b ORDER BY v"), vec![vec![Value::Null], vec![i(1)], vec![i(2)], vec![i(3)]]);
    assert_eq!(rows(&mut db, "SELECT v FROM a UNION ALL SELECT v FROM b").len(), 7);
    assert_eq!(rows(&mut db, "SELECT v FROM a UNION ALL SELECT v FROM b ORDER BY 1 DESC LIMIT 2"), vec![vec![i(3)], vec![i(2)]]);
    assert!(matches!(err(&mut db, "SELECT v FROM a UNION SELECT v, v FROM b"), Error::Schema(_)));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM (SELECT v FROM a UNION SELECT v FROM b) AS u"), vec![vec![i(4)]]);
}

#[test]
fn date_time_types() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE ev (id INT PRIMARY KEY, d DATE, t TIME, ts TIMESTAMP)");
    ok(&mut db, "INSERT INTO ev VALUES (1, '2024-02-29', '9:05', '2024-02-29T10:00:00Z'), (2, DATE '2023-12-31', '23:59:59', '2024-01-01')");
    assert!(matches!(err(&mut db, "INSERT INTO ev VALUES (3, '2023-02-29', NULL, NULL)"), Error::Type(_)));
    assert!(matches!(err(&mut db, "INSERT INTO ev VALUES (3, NULL, '25:00', NULL)"), Error::Type(_)));
    assert_eq!(
        rows(&mut db, "SELECT d, t, ts FROM ev WHERE id = 1"),
        vec![vec![t("2024-02-29"), t("09:05:00"), t("2024-02-29 10:00:00")]]
    );
    assert_eq!(rows(&mut db, "SELECT id FROM ev WHERE d < DATE '2024-01-01'"), vec![vec![i(2)]]);
    assert_eq!(rows(&mut db, "SELECT id FROM ev ORDER BY ts DESC"), vec![vec![i(1)], vec![i(2)]]);
    assert_eq!(rows(&mut db, "SELECT YEAR(d), MONTH(d), EXTRACT(DAY FROM d), HOUR(ts) FROM ev WHERE id = 1"), vec![vec![i(2024), i(2), i(29), i(10)]]);
    assert_eq!(rows(&mut db, "SELECT CAST('2024-05-06 07:08:09' AS DATE)"), vec![vec![t("2024-05-06")]]);
    ok(&mut db, "CREATE INDEX ev_d ON ev (d)");
    assert!(plan(&mut db, "SELECT id FROM ev WHERE d >= '2024-01-01'").contains("IndexRangeScan"));
    assert_eq!(rows(&mut db, "SELECT id FROM ev WHERE d >= '2024-01-01'"), vec![vec![i(1)]]);
}

// --------------------------------------------------------------- indexes

#[test]
fn range_scans_use_indexes_and_match_full_scans() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE p (id INT PRIMARY KEY, age INT, name TEXT, score REAL)");
    for k in 0..200 {
        ok(&mut db, &format!("INSERT INTO p VALUES ({k}, {}, 'n{:03}', {})", k % 50, k % 37, (k as f64) / 4.0));
    }
    let queries = [
        "SELECT id FROM p WHERE age >= 25 ORDER BY id",
        "SELECT id FROM p WHERE age > 25 AND age <= 30 ORDER BY id",
        "SELECT id FROM p WHERE age < 3 ORDER BY id",
        "SELECT id FROM p WHERE age = 7 ORDER BY id",
        "SELECT id FROM p WHERE age BETWEEN 10 AND 12 ORDER BY id",
        "SELECT id FROM p WHERE age IN (1, 2, 49) ORDER BY id",
        "SELECT id FROM p WHERE name LIKE 'n01%' ORDER BY id",
        "SELECT id FROM p WHERE name >= 'n030' AND name < 'n032' ORDER BY id",
        "SELECT id FROM p WHERE score > 40.5 ORDER BY id",
        "SELECT id FROM p WHERE age > 2.5 AND age < 4 ORDER BY id",
        "SELECT id FROM p WHERE id >= 190 ORDER BY id",
        "SELECT id FROM p WHERE id > 10 AND id < 13 ORDER BY id",
    ];
    let before: Vec<_> = queries.iter().map(|q| rows(&mut db, q)).collect();
    ok(&mut db, "CREATE INDEX p_age ON p (age)");
    ok(&mut db, "CREATE INDEX p_name ON p (name)");
    ok(&mut db, "CREATE INDEX p_score ON p (score)");
    for (q, b) in queries.iter().zip(&before) {
        assert_eq!(&rows(&mut db, q), b, "{q} changed after indexing");
    }
    let p = plan(&mut db, "SELECT id FROM p WHERE age >= 25");
    assert!(p.contains("IndexRangeScan on p using p_age") && p.contains("age >= 25"), "{p}");
    assert!(plan(&mut db, "SELECT id FROM p WHERE age = 7").contains("IndexScan on p using p_age"));
    assert!(plan(&mut db, "SELECT id FROM p WHERE id > 10 AND id < 13").contains("PrimaryKeyRangeScan"));
    assert!(plan(&mut db, "SELECT id FROM p WHERE age IN (1, 2)").contains("IndexScan on p using p_age"));
    assert!(plan(&mut db, "SELECT id FROM p WHERE score < 1").contains("IndexRangeScan on p using p_score"));
    // not usable: no index on the expression
    assert!(plan(&mut db, "SELECT id FROM p WHERE age + 1 > 3").contains("TableScan on p"));
    // EXPLAIN ANALYZE proves the index scan examined only matching entries
    let a = rows(&mut db, "EXPLAIN ANALYZE SELECT id FROM p WHERE age = 7");
    let text: Vec<String> = a.iter().map(|r| r[0].display()).collect();
    assert!(text[0].contains("examined=4") || text.iter().any(|l| l.contains("IndexScan") && l.contains("examined=4")), "{text:?}");
}

#[test]
fn composite_index_leading_equality_and_range() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE s (id INT PRIMARY KEY, city TEXT, age INT)");
    for k in 0..60 {
        ok(&mut db, &format!("INSERT INTO s VALUES ({k}, '{}', {})", ["Berlin", "Paris", "Rome"][k % 3], k));
    }
    ok(&mut db, "CREATE INDEX s_city_age ON s (city, age)");
    let p = plan(&mut db, "SELECT id FROM s WHERE city = 'Paris' AND age > 40");
    assert!(p.contains("IndexRangeScan on s using s_city_age") && p.contains("city = 'Paris' AND age > 40"), "{p}");
    assert_eq!(rows(&mut db, "SELECT id FROM s WHERE city = 'Paris' AND age > 50 ORDER BY id"), vec![vec![i(52)], vec![i(55)], vec![i(58)]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM s WHERE city = 'Rome'"), vec![vec![i(20)]]);
    assert!(plan(&mut db, "SELECT id FROM s WHERE city = 'Rome'").contains("using s_city_age"));
    // a range on the second column alone cannot use the composite index
    assert!(plan(&mut db, "SELECT id FROM s WHERE age > 40").contains("TableScan"));
    // maintenance on update / delete
    ok(&mut db, "UPDATE s SET city = 'Oslo' WHERE id = 52");
    ok(&mut db, "DELETE FROM s WHERE id = 55");
    assert_eq!(rows(&mut db, "SELECT id FROM s WHERE city = 'Paris' AND age > 50 ORDER BY id"), vec![vec![i(58)]]);
    assert_eq!(rows(&mut db, "SELECT id FROM s WHERE city = 'Oslo' AND age = 52"), vec![vec![i(52)]]);
}

#[test]
fn index_maintenance_survives_rollback_and_restart() {
    let dir = tmp("ixrecover");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
        ok(&mut db, "CREATE INDEX t_v ON t (v)");
        ok(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 10)");
        ok(&mut db, "BEGIN");
        ok(&mut db, "UPDATE t SET v = 99 WHERE id = 1");
        ok(&mut db, "DELETE FROM t WHERE id = 3");
        ok(&mut db, "INSERT INTO t VALUES (4, 10)");
        assert_eq!(rows(&mut db, "SELECT id FROM t WHERE v = 10 ORDER BY id"), vec![vec![i(4)]]);
        ok(&mut db, "ROLLBACK");
        assert_eq!(rows(&mut db, "SELECT id FROM t WHERE v = 10 ORDER BY id"), vec![vec![i(1)], vec![i(3)]]);
        // a failed statement inside a transaction leaves the index as it was
        ok(&mut db, "BEGIN");
        assert!(db.execute("UPDATE t SET v = 'x' WHERE id = 1").is_err());
        ok(&mut db, "COMMIT");
        db.checkpoint().unwrap();
        ok(&mut db, "UPDATE t SET v = 30 WHERE id = 2");
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE v >= 20 ORDER BY id"), vec![vec![i(2)]]);
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE v = 10 ORDER BY id"), vec![vec![i(1)], vec![i(3)]]);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

// --------------------------------------------------------------- planner

#[test]
fn join_strategy_selection_and_pushdown() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE u (id INT PRIMARY KEY, name TEXT)");
    ok(&mut db, "CREATE TABLE o (id INT PRIMARY KEY, uid INT, amt INT)");
    ok(&mut db, "INSERT INTO u VALUES (1, 'a'), (2, 'b'), (3, 'c')");
    ok(&mut db, "INSERT INTO o VALUES (1, 1, 5), (2, 1, 7), (3, 2, 9)");
    // the right side is reachable by primary key: index lookup join
    let p = plan(&mut db, "SELECT u.name, o.amt FROM o JOIN u ON u.id = o.uid");
    assert!(p.contains("IndexLookupJoin") && p.contains("PrimaryKeyLookup on u"), "{p}");
    // no index on o.uid: hash join
    let p = plan(&mut db, "SELECT u.name, o.amt FROM u JOIN o ON o.uid = u.id");
    assert!(p.contains("HashJoin"), "{p}");
    // non-equi: nested loop
    assert!(plan(&mut db, "SELECT u.name FROM u JOIN o ON o.amt > u.id").contains("NestedLoopJoin"));
    // WHERE on one table is pushed into its scan
    let p = plan(&mut db, "SELECT u.name FROM u JOIN o ON o.uid = u.id WHERE o.amt > 6 AND u.name <> 'z'");
    assert!(p.contains("Filter: (amt > 6)") && p.contains("Filter: (name != 'z')"), "{p}");
    // ... but not into the null-supplying side of a LEFT JOIN
    let r = rows(&mut db, "SELECT u.name, o.amt FROM u LEFT JOIN o ON o.uid = u.id WHERE o.amt IS NULL");
    assert_eq!(r, vec![vec![t("c"), Value::Null]]);
    let r = rows(&mut db, "SELECT u.name, o.amt FROM u LEFT JOIN o ON o.uid = u.id AND o.amt > 6 ORDER BY u.id");
    assert_eq!(r, vec![vec![t("a"), i(7)], vec![t("b"), i(9)], vec![t("c"), Value::Null]]);
    // limit pushdown into a plain scan
    assert!(plan(&mut db, "SELECT name FROM u LIMIT 2").contains("Stops after: 2"));
    assert_eq!(rows(&mut db, "SELECT name FROM u LIMIT 2").len(), 2);
    // results agree across strategies
    assert_eq!(
        rows(&mut db, "SELECT u.name, SUM(o.amt) FROM u JOIN o ON o.uid = u.id GROUP BY u.name ORDER BY u.name"),
        vec![vec![t("a"), i(12)], vec![t("b"), i(9)]]
    );
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM u, o"), vec![vec![i(9)]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM u CROSS JOIN o WHERE o.uid = u.id"), vec![vec![i(3)]]);
}

#[test]
fn explain_analyze_reports_real_statistics() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE u (id INT PRIMARY KEY, age INT)");
    for k in 1..=10 {
        ok(&mut db, &format!("INSERT INTO u VALUES ({k}, {})", k * 10));
    }
    let out: Vec<String> = rows(&mut db, "EXPLAIN ANALYZE SELECT * FROM u WHERE age >= 50").into_iter().map(|r| r[0].display()).collect();
    let joined = out.join("\n");
    assert!(joined.contains("TableScan on u"), "{joined}");
    assert!(joined.contains("actual rows=6") && joined.contains("examined=10"), "{joined}");
    assert!(joined.contains("Rows returned: 6") && joined.contains("Execution time:"), "{joined}");
    // plain EXPLAIN does not execute
    let p = plan(&mut db, "SELECT * FROM u WHERE age >= 50");
    assert!(!p.contains("actual rows"));
    // EXPLAIN of a DELETE shows its access path; EXPLAIN ANALYZE runs it
    assert!(plan(&mut db, "DELETE FROM u WHERE id = 3").contains("PrimaryKeyLookup on u"));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM u"), vec![vec![i(10)]]);
    ok(&mut db, "EXPLAIN ANALYZE DELETE FROM u WHERE id = 3");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM u"), vec![vec![i(9)]]);
}

// ----------------------------------------------------------------- PBSQL

#[test]
fn pbsql_document_queries() {
    let mut db = Database::in_memory();
    ok(&mut db, r#"INSERT INTO DOCUMENT.users VALUES {"_id": 1, "name": "Alice", "age": 30, "profile": {"address": {"city": "Berlin", "country": "Germany"}}, "tags": ["a", "b"]}"#);
    ok(&mut db, r#"INSERT INTO DOCUMENT.users VALUES {"_id": 2, "name": "Bob", "age": 25, "profile": {"address": {"city": "Paris", "country": "France"}}}, {"_id": 3, "name": "Cleo", "profile": {"address": {"city": "Munich", "country": "Germany"}}}"#);
    // the document is visible through the native document API too
    assert_eq!(ok(&mut db, "db.users.count({})"), Output::Scalar(i(3)));
    assert_eq!(
        rows(&mut db, "SELECT name, profile.address.city FROM DOCUMENT.users WHERE profile.address.country = 'Germany' ORDER BY name"),
        vec![vec![t("Alice"), t("Berlin")], vec![t("Cleo"), t("Munich")]]
    );
    assert_eq!(cols(&mut db, "SELECT profile.address.city FROM DOCUMENT.users"), vec!["profile.address.city"]);
    // missing fields are NULL
    assert_eq!(rows(&mut db, "SELECT name FROM DOCUMENT.users WHERE age IS NULL"), vec![vec![t("Cleo")]]);
    assert_eq!(rows(&mut db, "SELECT tags.1 FROM DOCUMENT.users WHERE _id = 1"), vec![vec![t("b")]]);
    // grouping over documents
    assert_eq!(
        rows(&mut db, "SELECT profile.address.country AS c, COUNT(*), AVG(age) FROM DOCUMENT.users GROUP BY c ORDER BY c"),
        vec![vec![t("France"), i(1), Value::Float(25.0)], vec![t("Germany"), i(2), Value::Float(30.0)]]
    );
    // SELECT * returns documents
    match ok(&mut db, "SELECT * FROM DOCUMENT.users WHERE _id = 2") {
        Output::Docs(d) => assert_eq!(d[0].get("name"), Some(&t("Bob"))),
        other => panic!("{other:?}"),
    }
    // update and delete
    assert_eq!(ok(&mut db, "UPDATE DOCUMENT.users SET profile.address.city = 'Hamburg', age = 31 WHERE name = 'Alice'"), Output::Done("1 document(s) updated".into()));
    assert_eq!(rows(&mut db, "SELECT profile.address.city, age FROM DOCUMENT.users WHERE _id = 1"), vec![vec![t("Hamburg"), i(31)]]);
    ok(&mut db, "DELETE FROM DOCUMENT.users WHERE age < 30");
    assert_eq!(ok(&mut db, "db.users.count({})"), Output::Scalar(i(2)));
    // document indexes are used by PBSQL
    ok(&mut db, "CREATE INDEX users_age ON DOCUMENT.users (age)");
    assert!(plan(&mut db, "SELECT name FROM DOCUMENT.users WHERE age > 20").contains("DocumentIndexRangeScan on DOCUMENT.users"));
    assert!(plan(&mut db, "SELECT name FROM DOCUMENT.users WHERE age = 31").contains("DocumentIndexScan"));
    assert!(plan(&mut db, "SELECT name FROM DOCUMENT.users WHERE _id = 1").contains("DocumentIdLookup"));
    assert_eq!(rows(&mut db, "SELECT name FROM DOCUMENT.users WHERE age > 20"), vec![vec![t("Alice")]]);
    // insert with a column list
    ok(&mut db, "INSERT INTO DOCUMENT.users (_id, name, age) VALUES (9, 'Zed', 40)");
    assert_eq!(rows(&mut db, "SELECT name FROM DOCUMENT.users WHERE age >= 40"), vec![vec![t("Zed")]]);
}

#[test]
fn pbsql_key_value_statements() {
    let mut db = Database::in_memory();
    ok(&mut db, r#"SET KV 'user:123' = '{"name":"Alice"}'"#);
    ok(&mut db, "SET KV 'user:124' = 'Bob'");
    ok(&mut db, "SET KV 'other' = 'x'");
    assert_eq!(ok(&mut db, "GET FROM KV 'user:124'"), Output::Scalar(t("Bob")));
    assert_eq!(ok(&mut db, "GET user:124"), Output::Scalar(t("Bob")), "legacy KV commands see the same data");
    assert_eq!(ok(&mut db, "EXISTS KV 'user:123'"), Output::Scalar(Value::Bool(true)));
    assert_eq!(ok(&mut db, "INCREMENT KV 'hits' BY 5"), Output::Scalar(i(5)));
    assert_eq!(ok(&mut db, "INCREMENT KV 'hits'"), Output::Scalar(i(6)));
    assert_eq!(
        rows(&mut db, "SELECT key, value FROM KV WHERE key LIKE 'user:%' ORDER BY key"),
        vec![vec![t("user:123"), t(r#"{"name":"Alice"}"#)], vec![t("user:124"), t("Bob")]]
    );
    assert!(plan(&mut db, "SELECT key FROM KV WHERE key LIKE 'user:%'").contains("KVPrefixScan"));
    assert!(plan(&mut db, "SELECT value FROM KV WHERE key = 'other'").contains("KVGet"));
    assert!(plan(&mut db, "SELECT value FROM KV WHERE key >= 'u'").contains("KVRangeScan"));
    assert_eq!(rows(&mut db, "SELECT key FROM KV WHERE key >= 'u' ORDER BY key").len(), 2);
    // JSON values can be looked into
    assert_eq!(rows(&mut db, "SELECT value.name FROM KV WHERE key = 'user:123'"), vec![vec![t("Alice")]]);
    // namespaces are separate keyspaces
    ok(&mut db, "SET KV.sessions 'session:1' = 'tok1'");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM KV.sessions"), vec![vec![i(1)]]);
    assert_eq!(ok(&mut db, "GET FROM KV 'session:1'"), Output::Scalar(Value::Null));
    ok(&mut db, "DELETE FROM KV WHERE key = 'user:123'");
    assert_eq!(ok(&mut db, "EXISTS KV 'user:123'"), Output::Scalar(Value::Bool(false)));
    ok(&mut db, "INSERT INTO KV (key, value) VALUES ('a', 'b')");
    ok(&mut db, "UPDATE KV SET value = 'c' WHERE key = 'a'");
    assert_eq!(ok(&mut db, "GET FROM KV 'a'"), Output::Scalar(t("c")));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM KV WHERE key LIKE 'user:%'"), vec![vec![i(1)]]);
}

#[test]
fn pbsql_cross_model_join() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");
    ok(&mut db, "INSERT INTO users VALUES (1, 'Ada'), (2, 'Linus'), (3, 'Grace')");
    ok(&mut db, r#"INSERT INTO DOCUMENT.profiles VALUES {"user_id": 1, "profile_data": {"lang": "en"}}, {"user_id": 2, "profile_data": {"lang": "fi"}}"#);
    ok(&mut db, "CREATE INDEX profiles_user ON DOCUMENT.profiles (user_id)");
    ok(&mut db, "SET KV.user_sessions 'session:1' = 'active'");
    let q = "SELECT users.id, users.name, profiles.profile_data, sessions.value \
             FROM RELATIONAL.users AS users \
             JOIN DOCUMENT.profiles AS profiles ON profiles.user_id = users.id \
             LEFT JOIN KV.user_sessions AS sessions ON sessions.key = CONCAT('session:', users.id) \
             ORDER BY users.id";
    let r = rows(&mut db, q);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][..2], [i(1), t("Ada")]);
    assert_eq!(r[0][2].get("lang"), Some(&t("en")));
    assert_eq!(r[0][3], t("active"));
    assert_eq!(r[1][3], Value::Null);
    let p = plan(&mut db, q);
    assert!(p.contains("DocumentIndexScan on DOCUMENT.profiles"), "{p}");
    assert!(p.contains("KVGet on KV.user_sessions"), "{p}");
    assert!(p.contains("cross-model"), "{p}");
    // document -> relational lookup, filtering and projection across models
    let r = rows(&mut db, "SELECT p.profile_data.lang, u.name FROM DOCUMENT.profiles p JOIN users u ON u.id = p.user_id WHERE p.profile_data.lang = 'fi'");
    assert_eq!(r, vec![vec![t("fi"), t("Linus")]]);
    assert!(plan(&mut db, "SELECT u.name FROM DOCUMENT.profiles p JOIN users u ON u.id = p.user_id").contains("PrimaryKeyLookup on users"));
    // document -> KV
    ok(&mut db, "SET KV 'lang:en' = 'English'");
    let r = rows(&mut db, "SELECT p.user_id, k.value FROM DOCUMENT.profiles p JOIN KV k ON k.key = 'lang:' || p.profile_data.lang");
    assert_eq!(r, vec![vec![i(1), t("English")]]);
    // aggregation across models
    let r = rows(&mut db, "SELECT COUNT(*) FROM users u LEFT JOIN DOCUMENT.profiles p ON p.user_id = u.id WHERE p.user_id IS NULL");
    assert_eq!(r, vec![vec![i(1)]]);
}

#[test]
fn pbsql_transactions_span_models() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)");
    ok(&mut db, "BEGIN");
    ok(&mut db, "INSERT INTO t VALUES (1)");
    ok(&mut db, r#"INSERT INTO DOCUMENT.c VALUES {"_id": 1}"#);
    ok(&mut db, "SET KV 'k' = 'v'");
    ok(&mut db, "ROLLBACK");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM t"), vec![vec![i(0)]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM DOCUMENT.c"), vec![vec![i(0)]]);
    assert_eq!(ok(&mut db, "GET FROM KV 'k'"), Output::Scalar(Value::Null));
    ok(&mut db, "BEGIN");
    ok(&mut db, "INSERT INTO t VALUES (1)");
    ok(&mut db, r#"INSERT INTO DOCUMENT.c VALUES {"_id": 1}"#);
    ok(&mut db, "SET KV 'k' = 'v'");
    ok(&mut db, "COMMIT");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM t"), vec![vec![i(1)]]);
    assert_eq!(ok(&mut db, "GET FROM KV 'k'"), Output::Scalar(t("v")));
    // a conflicting concurrent write is detected for PBSQL range reads too
    let mut other = db.session();
    ok(&mut db, "BEGIN");
    rows(&mut db, "SELECT id FROM t WHERE id > 0");
    ok(&mut db, "INSERT INTO t VALUES (100)");
    ok(&mut other, "INSERT INTO t VALUES (5)");
    assert!(matches!(db.execute("COMMIT"), Err(Error::Conflict(_))));
}

#[test]
fn pbsql_errors_are_typed() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)");
    assert!(matches!(err(&mut db, "SELECT FROM"), Error::Parse(_)));
    assert!(matches!(err(&mut db, "SELECT x FROM t"), Error::Schema(_)));
    assert!(matches!(err(&mut db, "SELECT * FROM DOCUMENT.\"bad name\""), Error::Schema(_)));
    assert!(matches!(err(&mut db, "SELECT nosuch(1)"), Error::Parse(_)));
    assert!(matches!(err(&mut db, "SELECT id FROM t WHERE COUNT(*) > 1"), Error::Parse(_)));
    assert!(matches!(err(&mut db, "INSERT INTO DOCUMENT.c VALUES (1, 2)"), Error::Type(_)));
    assert!(matches!(err(&mut db, "GET FROM KV 1.5"), Error::Type(_)));
    assert!(matches!(err(&mut db, "SELECT * FROM t a JOIN t a ON true"), Error::Schema(_)));
}
