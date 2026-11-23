//! PBSQL against the real storage engines: cross-model queries, statements
//! that write one model from another, transactions spanning models,
//! durability, EXPLAIN / EXPLAIN ANALYZE, NULL semantics, identifier and
//! escaping rules, and validation against the live catalog.

use polydb::{pbsql, Database, Error, Output, Value};
use std::fs;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-pbsql-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn ok(db: &mut Database, cmd: &str) -> Output {
    db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"))
}

fn rows(db: &mut Database, cmd: &str) -> Vec<Vec<Value>> {
    match ok(db, cmd) {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows from {cmd}, got {other:?}"),
    }
}

fn columns(db: &mut Database, cmd: &str) -> Vec<String> {
    match ok(db, cmd) {
        Output::Rows { columns, .. } => columns,
        other => panic!("expected rows from {cmd}, got {other:?}"),
    }
}

fn plan(db: &mut Database, cmd: &str) -> String {
    rows(db, cmd).into_iter().map(|r| r[0].display()).collect::<Vec<_>>().join("\n")
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

const NULL: Value = Value::Null;

/// One dataset across the three models.
fn seed(db: &mut Database) {
    ok(db, "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, city TEXT)");
    ok(db, "INSERT INTO users VALUES (1, 'Ada', 'London'), (2, 'Linus', 'Portland'), (3, 'Grace', 'NYC')");
    ok(
        db,
        "db.orders.insert([
            {_id: 10, user: 1, total: 9.5, ship: {city: 'London'}, tags: ['gift']},
            {_id: 11, user: 1, total: 20},
            {_id: 12, user: 2, total: 5.25, \"itemCount\": 3},
            {_id: 13, user: 9, total: 1}
        ])",
    );
    ok(db, "db.orders.createIndex({user: 1})");
    ok(db, "SET pref:1 dark");
    ok(db, "SET pref:2 {\"theme\": \"light\", \"size\": 12}");
    ok(db, "SET quota:1 100");
    ok(db, "SET quota:3 7");
}

// ============================================================ cross-model

#[test]
fn a_select_joins_tables_documents_and_keys() {
    let mut db = Database::in_memory();
    seed(&mut db);
    // table x documents
    assert_eq!(
        rows(&mut db, "SELECT u.name, o.total FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id ORDER BY o.total"),
        vec![vec![t("Linus"), Value::Float(5.25)], vec![t("Ada"), Value::Float(9.5)], vec![t("Ada"), i(20)]]
    );
    // three models in one statement, outer joins keep unmatched rows
    let r = rows(
        &mut db,
        "SELECT u.name, COUNT(o._id) AS orders, q.value AS quota
           FROM users u
           LEFT JOIN DOCUMENT(orders) o ON o.user = u.id
           LEFT JOIN KV('quota:') q ON q.key = 'quota:' || CAST(u.id AS TEXT)
          GROUP BY u.name, q.value
          ORDER BY u.name",
    );
    assert_eq!(
        r,
        vec![
            vec![t("Ada"), i(2), t("100")],
            vec![t("Grace"), i(0), t("7")],
            vec![t("Linus"), i(1), NULL],
        ]
    );
    // documents x keys without any table
    assert_eq!(
        rows(&mut db, "SELECT o._id, k.value FROM DOCUMENT(orders) o JOIN KV('pref:') k ON k.key = 'pref:' || CAST(o.user AS TEXT) WHERE o.total > 9 ORDER BY o._id"),
        vec![vec![i(10), t("dark")], vec![i(11), t("dark")]]
    );
}

#[test]
fn filters_subqueries_and_nested_document_paths_cross_models() {
    let mut db = Database::in_memory();
    seed(&mut db);
    // relational rows filtered by a document subquery
    assert_eq!(
        rows(&mut db, "SELECT name FROM users WHERE id IN (SELECT o.user FROM DOCUMENT(orders) o WHERE o.total >= 9.5) ORDER BY name"),
        vec![vec![t("Ada")]]
    );
    // correlated EXISTS against the key-value store
    assert_eq!(
        rows(&mut db, "SELECT name FROM users u WHERE NOT EXISTS (SELECT 1 FROM KV('quota:') q WHERE q.key = 'quota:' || CAST(u.id AS TEXT)) ORDER BY name"),
        vec![vec![t("Linus")]]
    );
    // nested fields, array elements, and JSON held in a key-value value
    assert_eq!(rows(&mut db, "SELECT o.ship.city, GET_PATH(o.tags, '0') FROM DOCUMENT(orders) o WHERE o._id = 10"), vec![vec![t("London"), t("gift")]]);
    assert_eq!(rows(&mut db, "SELECT k.value.theme, k.value.size + 1 FROM KV('pref:') k WHERE k.key = 'pref:2'"), vec![vec![t("light"), i(13)]]);
    // documents whose user has no row (an anti-join across models)
    assert_eq!(
        rows(&mut db, "SELECT o._id FROM DOCUMENT(orders) o LEFT JOIN users u ON u.id = o.user WHERE u.id IS NULL"),
        vec![vec![i(13)]]
    );
    // aggregates over documents
    assert_eq!(rows(&mut db, "SELECT SUM(o.total), MAX(o.total) FROM DOCUMENT(orders) o WHERE o.user = 1"), vec![vec![Value::Float(29.5), i(20)]]);
    // column headers name the field
    assert_eq!(columns(&mut db, "SELECT o.ship.city FROM DOCUMENT(orders) o"), vec!["city"]);
}

#[test]
fn writes_from_one_model_into_another_keep_the_target_constraints() {
    let mut db = Database::in_memory();
    seed(&mut db);
    ok(&mut db, "CREATE TABLE order_totals (id INT PRIMARY KEY, user_id INT NOT NULL REFERENCES users(id), total REAL CHECK (total > 0))");
    // the foreign key rejects the order of unknown user 9: nothing is inserted
    let before = db.entry_count();
    let e = db.execute("INSERT INTO order_totals SELECT o._id, o.user, o.total FROM DOCUMENT(orders) o").unwrap_err();
    assert!(matches!(e, Error::Constraint(_)), "{e:?}");
    assert_eq!(db.entry_count(), before, "a failed cross-model insert left rows behind");
    // a NOT NULL column fed from a missing document field is refused too
    let e = db.execute("INSERT INTO order_totals SELECT o._id, o.nobody, o.total FROM DOCUMENT(orders) o").unwrap_err();
    assert!(matches!(e, Error::Constraint(_)), "{e:?}");
    // filtered to valid users, it goes through the relational engine
    assert_eq!(
        ok(&mut db, "INSERT INTO order_totals SELECT o._id, o.user, o.total FROM DOCUMENT(orders) o WHERE o.user IN (SELECT id FROM users)").to_string(),
        "3 row(s) inserted"
    );
    // UPDATE driven by the key-value store
    ok(&mut db, "UPDATE users SET city = 'Quota-holder' WHERE id IN (SELECT CAST(SUBSTR(k.key, 7) AS INT) FROM KV('quota:') k)");
    assert_eq!(
        rows(&mut db, "SELECT id FROM users WHERE city = 'Quota-holder' ORDER BY id"),
        vec![vec![i(1)], vec![i(3)]]
    );
    // the relational integrity checker agrees
    for r in rows(&mut db, "CHECK TABLES") {
        assert_eq!(r[2], t("ok"), "{r:?}");
    }
}

// =========================================================== transactions

#[test]
fn a_transaction_spanning_models_commits_or_rolls_back_as_one() {
    let mut db = Database::in_memory();
    seed(&mut db);
    ok(&mut db, "BEGIN");
    ok(&mut db, "INSERT INTO users VALUES (4, 'Alan', 'London')");
    ok(&mut db, "db.orders.insert({_id: 14, user: 4, total: 3})");
    ok(&mut db, "SET quota:4 1");
    // the open transaction sees its own writes in a cross-model query
    assert_eq!(
        rows(&mut db, "SELECT u.name, o.total, q.value FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id JOIN KV('quota:') q ON q.key = 'quota:4' WHERE u.id = 4"),
        vec![vec![t("Alan"), i(3), t("1")]]
    );
    // another session does not
    let mut other = db.session();
    assert!(rows(&mut other, "SELECT * FROM DOCUMENT(orders) o WHERE o._id = 14").is_empty());
    ok(&mut db, "ROLLBACK");
    assert!(rows(&mut db, "SELECT u.id FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id WHERE u.id = 4").is_empty());
    assert_eq!(ok(&mut db, "GET quota:4"), Output::Scalar(NULL));

    ok(&mut db, "START TRANSACTION");
    ok(&mut db, "INSERT INTO users VALUES (4, 'Alan', 'London')");
    ok(&mut db, "db.orders.insert({_id: 14, user: 4, total: 3})");
    ok(&mut db, "COMMIT WORK");
    assert_eq!(rows(&mut other, "SELECT o.total FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id WHERE u.id = 4"), vec![vec![i(3)]]);
}

#[test]
fn documents_read_by_a_sql_query_are_validated_at_commit() {
    let mut db = Database::in_memory();
    seed(&mut db);
    let mut a = db.session();
    let mut b = db.session();
    ok(&mut a, "BEGIN");
    // a reads user 1's orders through SQL ...
    assert_eq!(rows(&mut a, "SELECT COUNT(*) FROM DOCUMENT(orders) o WHERE o.user = 1"), vec![vec![i(2)]]);
    // ... b adds one and commits ...
    ok(&mut b, "db.orders.insert({_id: 15, user: 1, total: 2})");
    // ... so a decision a made from a stale read must not commit
    ok(&mut a, "SET orders_of_1 2");
    match a.commit() {
        Err(Error::Conflict(_)) => {}
        other => panic!("expected a serialization conflict, got {other:?}"),
    }
    assert_eq!(ok(&mut db, "GET orders_of_1"), Output::Scalar(NULL), "nothing of the failed transaction was written");

    // the same holds for key-value data read through KV()
    ok(&mut a, "BEGIN");
    rows(&mut a, "SELECT k.value FROM KV('quota:') k WHERE k.key = 'quota:1'");
    ok(&mut b, "SET quota:1 0");
    ok(&mut a, "UPDATE users SET city = 'x' WHERE id = 1");
    assert!(matches!(a.execute("COMMIT"), Err(Error::Conflict(_))));
    assert_eq!(rows(&mut db, "SELECT city FROM users WHERE id = 1"), vec![vec![t("London")]]);
}

#[test]
fn a_failed_statement_inside_a_transaction_leaves_the_rest_intact() {
    let mut db = Database::in_memory();
    seed(&mut db);
    ok(&mut db, "BEGIN");
    ok(&mut db, "SET step 1");
    // a semantic error is caught before anything runs
    assert!(matches!(db.execute("SELECT name + 1 FROM users"), Err(Error::Type(_))));
    // a constraint error undoes only its own statement
    assert!(db.execute("INSERT INTO users VALUES (1, 'dup', NULL)").is_err());
    ok(&mut db, "COMMIT");
    assert_eq!(ok(&mut db, "GET step"), Output::Scalar(t("1")));
}

#[test]
fn cross_model_queries_work_on_a_durable_database_after_reopening() {
    let dir = tmp("durable");
    {
        let mut db = Database::open(&dir).unwrap();
        seed(&mut db);
        ok(&mut db, "CHECKPOINT");
        ok(&mut db, "db.orders.insert({_id: 16, user: 3, total: 50})");
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        rows(&mut db, "SELECT u.name, SUM(o.total) FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id GROUP BY u.name ORDER BY u.name"),
        vec![vec![t("Ada"), Value::Float(29.5)], vec![t("Grace"), i(50)], vec![t("Linus"), Value::Float(5.25)]]
    );
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

// ================================================================ EXPLAIN

#[test]
fn explain_shows_each_engine_choosing_its_access_path() {
    let mut db = Database::in_memory();
    seed(&mut db);
    // the WHERE clause is pushed into the document engine, which uses its index
    let p = plan(&mut db, "EXPLAIN SELECT o._id FROM DOCUMENT(orders) o WHERE o.user = 1 AND o.total > 1");
    assert!(p.contains("Document Scan on orders AS o") && p.contains("collection orders: index user_1"), "{p}");
    assert!(p.contains(r#"Pushed Filter: {"user":1}"#) && p.contains("Filter on o"), "{p}");
    // _id goes straight to the document
    assert!(plan(&mut db, "EXPLAIN SELECT * FROM DOCUMENT(orders) o WHERE o._id = 12").contains("_id lookup"));
    // range predicates are not pushed (they compare across types differently)
    assert!(plan(&mut db, "EXPLAIN SELECT * FROM DOCUMENT(orders) o WHERE o.total > 5").contains("full scan"));
    // key-value: point lookup, and LIKE narrows the prefix scan
    assert!(plan(&mut db, "EXPLAIN SELECT * FROM KV() k WHERE k.key = 'pref:1'").contains("key lookup"));
    assert!(plan(&mut db, "EXPLAIN SELECT * FROM KV('pref') k WHERE k.key LIKE 'pref:2%'").contains("prefix scan 'pref:2'"));
    // relational access paths are unchanged
    assert!(plan(&mut db, "EXPLAIN SELECT * FROM users WHERE id = 2").contains("Primary Key Lookup on users"));
    // document and key-value commands can be explained too
    let p = plan(&mut db, "EXPLAIN db.orders.find({user: 2}).sort({total: -1})");
    assert!(p.contains("index user_1") && p.contains("sort"), "{p}");
    assert!(plan(&mut db, "EXPLAIN KEYS pref:").contains("prefix scan 'pref:'"));
}

#[test]
fn explain_never_writes_and_explain_analyze_runs_for_real() {
    let mut db = Database::in_memory();
    seed(&mut db);
    let before = db.entry_count();
    let p = plan(&mut db, "EXPLAIN DELETE FROM users WHERE id = 1");
    assert!(p.contains("Delete on users") && p.contains("Primary Key Lookup on users"), "{p}");
    plan(&mut db, "EXPLAIN db.orders.deleteMany({})");
    plan(&mut db, "EXPLAIN INSERT INTO users VALUES (9, 'x', NULL)");
    assert_eq!(db.entry_count(), before, "plain EXPLAIN changed data");

    // EXPLAIN ANALYZE of a cross-model join: each engine's input is a plan
    // node with its actual rows, and the planner may probe the table's key
    let p = plan(&mut db, "EXPLAIN ANALYZE SELECT u.name FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id");
    assert!(p.lines().any(|l| l.contains("Document Scan on orders AS o") && l.contains("actual rows=4")), "{p}");
    assert!(p.contains("users AS u"), "{p}");
    assert!(p.contains("Execution time:") && p.contains("(3 row(s) returned)"), "{p}");
    // and of a write: it happens
    let p = plan(&mut db, "EXPLAIN ANALYZE db.orders.deleteOne({user: 9})");
    assert!(p.contains("candidates examined: 1") && p.contains("actual: 1 document(s) deleted"), "{p}");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM DOCUMENT(orders) o"), vec![vec![i(3)]]);
    let p = plan(&mut db, "EXPLAIN ANALYZE UPDATE users SET city = 'Oslo' WHERE id = 3");
    assert!(p.contains("actual: 1 row(s) updated"), "{p}");
    assert_eq!(rows(&mut db, "SELECT city FROM users WHERE id = 3"), vec![vec![t("Oslo")]]);
    // EXPLAIN ANALYZE inside a rolled-back transaction leaves nothing behind
    ok(&mut db, "BEGIN");
    plan(&mut db, "EXPLAIN ANALYZE DELETE FROM users");
    ok(&mut db, "ROLLBACK");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM users"), vec![vec![i(3)]]);
}

// ================================================================== rules

#[test]
fn null_semantics_are_the_same_for_every_model() {
    let mut db = Database::in_memory();
    seed(&mut db);
    // a field a document lacks reads as NULL, even when no document has it
    assert_eq!(rows(&mut db, "SELECT o._id, o.ship, o.nobody FROM DOCUMENT(orders) o WHERE o._id = 11"), vec![vec![i(11), NULL, NULL]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM DOCUMENT(orders) o WHERE o.ship IS NULL"), vec![vec![i(3)]]);
    // comparing with NULL is unknown, so no row qualifies either way
    assert!(rows(&mut db, "SELECT * FROM DOCUMENT(orders) o WHERE o.ship = NULL").is_empty());
    assert!(rows(&mut db, "SELECT * FROM DOCUMENT(orders) o WHERE NOT (o.nobody = 1)").is_empty());
    // NOT IN over a list holding NULL is never true
    assert!(rows(&mut db, "SELECT * FROM users WHERE id NOT IN (1, NULL)").is_empty());
    // a missing key is NULL on a LEFT JOIN, and aggregates skip NULLs
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*), COUNT(q.value) FROM users u LEFT JOIN KV('quota:') q ON q.key = 'quota:' || CAST(u.id AS TEXT)"),
        vec![vec![i(3), i(2)]]
    );
    // three-valued logic
    assert_eq!(rows(&mut db, "SELECT NULL AND FALSE, NULL OR TRUE, NULL AND TRUE, NULL + 1, NULL || 'x'"), vec![vec![Value::Bool(false), Value::Bool(true), NULL, NULL, NULL]]);
}

#[test]
fn identifiers_fold_unless_quoted_in_every_model() {
    let mut db = Database::in_memory();
    seed(&mut db);
    // unquoted table and column names fold to lower case
    assert_eq!(rows(&mut db, "SELECT NAME FROM USERS WHERE ID = 1"), vec![vec![t("Ada")]]);
    // a mixed-case document field needs quotes; unquoted it is a different (absent) field
    assert_eq!(rows(&mut db, r#"SELECT o."itemCount", o.itemcount FROM DOCUMENT(orders) o WHERE o._id = 12"#), vec![vec![i(3), NULL]]);
    // collection names in the db.<name> form keep their case
    ok(&mut db, "db.Logs.insert({_id: 1, msg: 'hi'})");
    assert_eq!(rows(&mut db, r#"SELECT l.msg FROM DOCUMENT("Logs") l"#), vec![vec![t("hi")]]);
    assert!(rows(&mut db, "SELECT * FROM DOCUMENT(logs) l").is_empty());
}

#[test]
fn string_escaping_round_trips_through_every_model() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE notes (id INT PRIMARY KEY, body TEXT)");
    // SQL: a doubled quote is one quote; a backslash is just a backslash
    ok(&mut db, r"INSERT INTO notes VALUES (1, 'it''s a\n test')");
    assert_eq!(rows(&mut db, "SELECT body FROM notes"), vec![vec![t(r"it's a\n test")]]);
    // key-value: backslash escapes inside quotes
    ok(&mut db, r#"SET "k 1" 'line\none \'q\''"#);
    assert_eq!(ok(&mut db, "GET 'k 1'"), Output::Scalar(t("line\none 'q'")));
    // documents: JSON escapes
    ok(&mut db, r#"db.c.insert({_id: 1, s: "tab\there \"q\" é"})"#);
    assert_eq!(rows(&mut db, "SELECT c.s FROM DOCUMENT(c) c"), vec![vec![t("tab\there \"q\" é")]]);
    // and the normalizer re-escapes so the text reads back identically
    let n = pbsql::normalize(r#"SET "k 1" 'line\none \'q\''"#).unwrap();
    ok(&mut db, "DEL 'k 1'");
    ok(&mut db, &n);
    assert_eq!(ok(&mut db, "GET 'k 1'"), Output::Scalar(t("line\none 'q'")));
}

#[test]
fn validation_uses_the_live_catalog_and_runs_nothing() {
    let mut db = Database::in_memory();
    seed(&mut db);
    ok(&mut db, "CREATE TABLE empty (id INT PRIMARY KEY, label TEXT)");
    let before = db.entry_count();
    // errors are found even though no row would ever reach the bad expression
    assert!(matches!(db.validate("SELECT label * 2 FROM empty"), Err(Error::Type(_))));
    assert!(matches!(db.execute("SELECT label * 2 FROM empty"), Err(Error::Type(_))));
    assert!(matches!(db.validate("DELETE FROM empty WHERE label LIKE id"), Err(Error::Type(_))));
    assert!(matches!(db.validate("SELECT nope FROM users"), Err(Error::Schema(_))));
    assert!(matches!(db.validate("SELECT * FROM missing"), Err(Error::Schema(_))));
    assert!(matches!(db.validate("SELECT name FROM users u JOIN empty e ON e.id = u.id WHERE id = 1"), Err(Error::Schema(m)) if m.contains("ambiguous")));
    assert!(matches!(db.validate("UPDATE users SET nope = 1"), Err(Error::Schema(_))));
    db.validate("SELECT u.name FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id").unwrap();
    assert_eq!(db.entry_count(), before);
    // the catalog-free check catches what can be known without a database
    assert!(matches!(pbsql::validate("SELECT 1 + 'a'"), Err(Error::Type(_))));
    pbsql::validate("SELECT * FROM table_that_may_exist_later").unwrap();
}

#[test]
fn sql_entry_point_refuses_other_models_and_errors_are_positioned() {
    let mut db = Database::in_memory();
    seed(&mut db);
    assert!(db.sql("SELECT u.name FROM users u JOIN DOCUMENT(orders) o ON o.user = u.id").is_ok());
    assert!(matches!(db.sql("SET k v"), Err(Error::Parse(m)) if m.contains("key-value statement")));
    assert!(matches!(db.sql("db.c.find({})"), Err(Error::Parse(_))));
    let e = db.execute("SELECT name\n  FROM users\n WHERE id = = 2").unwrap_err().to_string();
    assert!(e.contains("(at line 3, column 13)"), "{e}");
    // unsupported operations fail explicitly instead of being misread
    for cmd in ["CREATE VIEW v AS SELECT 1", "WITH a AS (SELECT 1) SELECT 1", "GRANT ALL ON users TO bob", "EXPLAIN CHECKPOINT"] {
        let e = db.execute(cmd).unwrap_err().to_string();
        assert!(e.contains("not supported") || e.contains("cannot be applied"), "{cmd}: {e}");
    }
}

#[test]
fn every_statement_kind_runs_through_the_unified_entry_point() {
    let mut db = Database::in_memory();
    seed(&mut db);
    ok(&mut db, "ALTER TABLE users ADD COLUMN age INT");
    ok(&mut db, "CREATE INDEX users_city ON users (city)");
    ok(&mut db, "UPDATE users SET age = 36 WHERE name = 'Ada'");
    ok(&mut db, "DELETE FROM users WHERE id = 3");
    assert_eq!(rows(&mut db, "SHOW TABLES"), vec![vec![t("users")]]);
    assert_eq!(rows(&mut db, "SHOW COLLECTIONS"), vec![vec![t("orders")]]);
    assert_eq!(ok(&mut db, "db.orders.count({user: 1})"), Output::Scalar(i(2)));
    assert_eq!(ok(&mut db, "db.orders.updateMany({user: 1}, {$inc: {total: 1}})").to_string(), "2 document(s) matched, 2 modified");
    assert_eq!(ok(&mut db, "INCR hits 5"), Output::Scalar(i(5)));
    assert_eq!(ok(&mut db, "DECR hits 2"), Output::Scalar(i(3)));
    assert_eq!(ok(&mut db, "EXISTS hits"), Output::Scalar(Value::Bool(true)));
    assert_eq!(rows(&mut db, "KEYS pref:"), vec![vec![t("pref:1")], vec![t("pref:2")]]);
    assert_eq!(ok(&mut db, "DEL hits"), Output::Scalar(i(1)));
    assert_eq!(rows(&mut db, "SELECT age FROM users WHERE id = 1 -- trailing comment"), vec![vec![i(36)]]);
}
