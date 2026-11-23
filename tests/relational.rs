//! Relational engine integration tests: schema and constraints, data
//! manipulation, query execution, indexes, ALTER TABLE, persistence across
//! restarts, and negative cases. Every test runs SQL against the real
//! storage engine (in memory, or a database directory for restart tests).

use polydb::{Database, Error, Output, Value};
use std::fs;
use std::path::PathBuf;

// ------------------------------------------------------------------ helpers

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-rel-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn ok(db: &mut Database, sql: &str) -> Output {
    db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match ok(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn columns(db: &mut Database, sql: &str) -> Vec<String> {
    match ok(db, sql) {
        Output::Rows { columns, .. } => columns,
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn one(db: &mut Database, sql: &str) -> Value {
    let r = rows(db, sql);
    assert_eq!(r.len(), 1, "{sql} returned {r:?}");
    assert_eq!(r[0].len(), 1, "{sql} returned {r:?}");
    r[0][0].clone()
}

fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    rows(db, sql)
        .into_iter()
        .map(|r| match &r[0] {
            Value::Int(i) => *i,
            other => panic!("{sql}: expected an integer, got {other:?}"),
        })
        .collect()
}

fn done(db: &mut Database, sql: &str) -> String {
    match ok(db, sql) {
        Output::Done(m) => m,
        other => panic!("expected a status from {sql}, got {other:?}"),
    }
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

fn i(n: i64) -> Value {
    Value::Int(n)
}

const NULL: Value = Value::Null;

/// The statement fails with the given kind of error and changes nothing.
fn fails(db: &mut Database, sql: &str, kind: fn(&Error) -> bool) -> String {
    let before = db.entry_count();
    match db.execute(sql) {
        Ok(out) => panic!("{sql:?} should fail, got {out:?}"),
        Err(e) => {
            assert!(kind(&e), "{sql:?} gave the wrong kind of error: {e:?}");
            if !db.in_transaction() {
                assert_eq!(db.entry_count(), before, "failed statement {sql:?} left changes behind");
            }
            e.to_string()
        }
    }
}

fn constraint(e: &Error) -> bool {
    matches!(e, Error::Constraint(_))
}
fn schema(e: &Error) -> bool {
    matches!(e, Error::Schema(_))
}
fn parse(e: &Error) -> bool {
    matches!(e, Error::Parse(_))
}
fn type_err(e: &Error) -> bool {
    matches!(e, Error::Type(_))
}

/// CHECK TABLE reports every check of every table as ok.
fn consistent(db: &mut Database) {
    for r in rows(db, "CHECK TABLES") {
        assert_eq!(r[2], t("ok"), "integrity check failed: {r:?}");
    }
}

fn plan(db: &mut Database, sql: &str) -> String {
    rows(db, &format!("EXPLAIN {sql}")).into_iter().map(|r| r[0].display()).collect::<Vec<_>>().join("\n")
}

fn shop(db: &mut Database) {
    ok(db, "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT NOT NULL, email VARCHAR(40) UNIQUE, city TEXT, vip BOOL DEFAULT false)");
    ok(db, "INSERT INTO customers (id, name, email, city) VALUES (1, 'Ada', 'ada@x.org', 'London'), (2, 'Linus', 'linus@x.org', 'Portland'), (3, 'Grace', NULL, 'NYC'), (4, 'Alan', 'alan@x.org', 'London')");
    ok(db, "CREATE TABLE orders (id SERIAL PRIMARY KEY, customer_id INT NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total REAL CHECK (total >= 0), status TEXT DEFAULT 'new')");
    ok(db, "INSERT INTO orders (customer_id, total) VALUES (1, 9.5), (1, 20), (2, 5.25), (4, 100)");
    ok(db, "CREATE INDEX orders_customer ON orders (customer_id)");
}

// ======================================================= schema / constraints

#[test]
fn create_table_with_every_constraint_kind_and_describe_it() {
    let mut db = Database::in_memory();
    shop(&mut db);
    let d = rows(&mut db, "DESCRIBE orders");
    assert_eq!(d[0], vec![t("id"), t("int"), t("NO"), t("PRI"), NULL, t("auto_increment")]);
    assert_eq!(d[1], vec![t("customer_id"), t("int"), t("NO"), t("MUL"), NULL, t("references customers(id)")]);
    assert_eq!(d[3], vec![t("status"), t("text"), t("YES"), t(""), t("new"), t("")]);
    let d = rows(&mut db, "DESCRIBE customers");
    assert_eq!(d[2], vec![t("email"), t("varchar(40)"), t("YES"), t("UNI"), NULL, t("")]);
    assert_eq!(d[4][4], Value::Bool(false));
    assert_eq!(done(&mut db, "CREATE TABLE IF NOT EXISTS orders (x INT)"), "table 'orders' already exists");
    consistent(&mut db);
}

#[test]
fn invalid_table_definitions_are_rejected() {
    let mut db = Database::in_memory();
    fails(&mut db, "CREATE TABLE t (a INT, a TEXT)", schema);
    fails(&mut db, "CREATE TABLE t (a INT PRIMARY KEY, b INT PRIMARY KEY)", schema);
    fails(&mut db, "CREATE TABLE t (a REAL PRIMARY KEY)", schema);
    fails(&mut db, "CREATE TABLE t (a BLOB)", parse);
    fails(&mut db, "CREATE TABLE t (a INT, PRIMARY KEY (b))", schema);
    fails(&mut db, "CREATE TABLE t (a INT, PRIMARY KEY (a, a))", schema);
    fails(&mut db, "CREATE TABLE t (a INT DEFAULT 'x')", type_err);
    fails(&mut db, "CREATE TABLE t (a INT DEFAULT b)", schema);
    fails(&mut db, "CREATE TABLE t (a TEXT AUTOINCREMENT)", schema);
    fails(&mut db, "CREATE TABLE t (a INT, CHECK (b > 0))", schema);
    fails(&mut db, "CREATE TABLE t (a INT CHECK (a IN (SELECT 1)))", schema);
    fails(&mut db, "CREATE TABLE t (a INT REFERENCES missing(id))", schema);
    fails(&mut db, "CREATE TABLE \"bad name\" (a INT)", schema);
    fails(&mut db, "CREATE TABLE t (a VARCHAR(0))", parse);
    fails(&mut db, "CREATE TABLE t (a INT, UNIQUE (a, a))", schema);
    ok(&mut db, "CREATE TABLE p (id INT PRIMARY KEY, code TEXT, n INT)");
    // the referenced columns must be unique, and types must match
    fails(&mut db, "CREATE TABLE c (x TEXT REFERENCES p(code))", schema);
    fails(&mut db, "CREATE TABLE c (x TEXT REFERENCES p(id))", schema);
    fails(&mut db, "CREATE TABLE c (x INT NOT NULL REFERENCES p ON DELETE SET NULL)", schema);
    fails(&mut db, "CREATE TABLE c (x INT REFERENCES p ON DELETE SET DEFAULT)", parse);
    assert_eq!(rows(&mut db, "SHOW TABLES"), vec![vec![t("p")]]);
}

#[test]
fn not_null_default_and_type_validation() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n INT NOT NULL DEFAULT 7, r REAL, s VARCHAR(3), b BOOL, x TEXT DEFAULT 'd')");
    ok(&mut db, "INSERT INTO t (id) VALUES (1)");
    assert_eq!(rows(&mut db, "SELECT n, r, s, b, x FROM t"), vec![vec![i(7), NULL, NULL, NULL, t("d")]]);
    ok(&mut db, "INSERT INTO t VALUES (2, DEFAULT, 1, 'abc', true, DEFAULT)");
    assert_eq!(rows(&mut db, "SELECT n, r, x FROM t WHERE id = 2"), vec![vec![i(7), Value::Float(1.0), t("d")]]);
    // whole floats are accepted for INT, other mismatches are type errors
    ok(&mut db, "INSERT INTO t (id, n) VALUES (3.0, 4.0)");
    assert_eq!(one(&mut db, "SELECT n FROM t WHERE id = 3"), i(4));
    fails(&mut db, "INSERT INTO t (id, n) VALUES (4, 4.5)", type_err);
    fails(&mut db, "INSERT INTO t (id, n) VALUES (4, '4')", type_err);
    fails(&mut db, "INSERT INTO t (id, b) VALUES (4, 1)", type_err);
    fails(&mut db, "INSERT INTO t (id, s) VALUES (4, 'abcd')", type_err);
    fails(&mut db, "INSERT INTO t (id, n) VALUES (4, NULL)", constraint);
    fails(&mut db, "INSERT INTO t (id) VALUES (NULL)", constraint);
    fails(&mut db, "UPDATE t SET n = NULL WHERE id = 1", constraint);
    fails(&mut db, "UPDATE t SET s = 'toolong' WHERE id = 1", type_err);
    // VARCHAR length counts characters, not bytes
    ok(&mut db, "INSERT INTO t (id, s) VALUES (5, 'äöü')");
    // non-finite numbers are refused instead of silently stored as NULL
    fails(&mut db, "INSERT INTO t (id, r) VALUES (6, 1.0 / 0)", type_err);
    fails(&mut db, "INSERT INTO t (id, r) VALUES (6, 1e308 * 10)", type_err);
    // SET col = DEFAULT
    ok(&mut db, "UPDATE t SET n = 99, x = 'y' WHERE id = 1");
    ok(&mut db, "UPDATE t SET n = DEFAULT, x = DEFAULT WHERE id = 1");
    assert_eq!(rows(&mut db, "SELECT n, x FROM t WHERE id = 1"), vec![vec![i(7), t("d")]]);
    ok(&mut db, "CREATE TABLE d (a INT DEFAULT 1, b TEXT DEFAULT 'z')");
    ok(&mut db, "INSERT INTO d DEFAULT VALUES");
    assert_eq!(rows(&mut db, "SELECT * FROM d"), vec![vec![i(1), t("z")]]);
    consistent(&mut db);
}

#[test]
fn auto_increment_columns() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id SERIAL PRIMARY KEY, v TEXT)");
    ok(&mut db, "INSERT INTO t (v) VALUES ('a'), ('b')");
    ok(&mut db, "INSERT INTO t VALUES (10, 'explicit')");
    ok(&mut db, "INSERT INTO t (v) VALUES ('c')");
    assert_eq!(ints(&mut db, "SELECT id FROM t ORDER BY id"), vec![1, 2, 10, 11]);
    // a failed insert does not consume sequence values
    fails(&mut db, "INSERT INTO t (v) VALUES ('d'), ('e'), (NULL), (1)", type_err);
    ok(&mut db, "INSERT INTO t (v) VALUES ('d')");
    assert_eq!(one(&mut db, "SELECT MAX(id) FROM t"), i(12));
    ok(&mut db, "CREATE TABLE u (k INT AUTOINCREMENT, name TEXT)");
    ok(&mut db, "INSERT INTO u (name) VALUES ('x'), ('y')");
    assert_eq!(ints(&mut db, "SELECT k FROM u ORDER BY k"), vec![1, 2]);
}

#[test]
fn unique_constraints_single_composite_and_nulls() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, email TEXT UNIQUE, a INT, b INT, CONSTRAINT ab_u UNIQUE (a, b))");
    ok(&mut db, "INSERT INTO t VALUES (1, 'x', 1, 1), (2, 'y', 1, 2), (3, NULL, 2, 1)");
    let m = fails(&mut db, "INSERT INTO t VALUES (4, 'x', 9, 9)", constraint);
    assert!(m.contains("t_email_key"), "{m}");
    let m = fails(&mut db, "INSERT INTO t VALUES (4, 'z', 1, 2)", constraint);
    assert!(m.contains("ab_u"), "{m}");
    // NULLs never conflict
    ok(&mut db, "INSERT INTO t VALUES (4, NULL, NULL, 2), (5, NULL, NULL, 2)");
    // duplicates inside one statement are caught too
    fails(&mut db, "INSERT INTO t VALUES (6, 'q', 7, 7), (7, 'q', 8, 8)", constraint);
    fails(&mut db, "UPDATE t SET email = 'x' WHERE id = 2", constraint);
    // swapping unique values in one statement is fine: uniqueness is checked on the result
    ok(&mut db, "UPDATE t SET email = CASE id WHEN 1 THEN 'y' WHEN 2 THEN 'x' END WHERE id IN (1, 2)");
    assert_eq!(rows(&mut db, "SELECT email FROM t WHERE id <= 2 ORDER BY id"), vec![vec![t("y")], vec![t("x")]]);
    consistent(&mut db);
}

#[test]
fn check_constraints() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE acct (id INT PRIMARY KEY, balance INT CHECK (balance >= 0), lo INT, hi INT, CONSTRAINT range_ok CHECK (lo <= hi))");
    ok(&mut db, "INSERT INTO acct VALUES (1, 10, 1, 5), (2, NULL, NULL, 3)"); // NULL passes a CHECK
    let m = fails(&mut db, "INSERT INTO acct VALUES (3, -1, 0, 0)", constraint);
    assert!(m.contains("acct_balance_check"), "{m}");
    let m = fails(&mut db, "UPDATE acct SET lo = 10 WHERE id = 1", constraint);
    assert!(m.contains("range_ok"), "{m}");
    fails(&mut db, "UPDATE acct SET balance = balance - 11", constraint);
    ok(&mut db, "UPDATE acct SET balance = balance - 10");
    assert_eq!(one(&mut db, "SELECT balance FROM acct WHERE id = 1"), i(0));
}

// ============================================================== foreign keys

#[test]
fn foreign_keys_restrict_child_inserts_and_parent_deletes() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE dept (id INT PRIMARY KEY, name TEXT)");
    ok(&mut db, "CREATE TABLE emp (id INT PRIMARY KEY, dept_id INT REFERENCES dept(id), name TEXT)");
    ok(&mut db, "INSERT INTO dept VALUES (1, 'eng'), (2, 'ops')");
    ok(&mut db, "INSERT INTO emp VALUES (10, 1, 'a'), (11, 1, 'b'), (12, NULL, 'c')");
    let m = fails(&mut db, "INSERT INTO emp VALUES (13, 3, 'd')", constraint);
    assert!(m.contains("emp_dept_id_fkey"), "{m}");
    fails(&mut db, "UPDATE emp SET dept_id = 9 WHERE id = 10", constraint);
    // RESTRICT is the default
    fails(&mut db, "DELETE FROM dept WHERE id = 1", constraint);
    fails(&mut db, "UPDATE dept SET id = 5 WHERE id = 1", constraint);
    ok(&mut db, "DELETE FROM dept WHERE id = 2"); // unreferenced
    // a child and its parent inserted in one statement, either order
    ok(&mut db, "CREATE TABLE node (id INT PRIMARY KEY, parent INT REFERENCES node(id))");
    ok(&mut db, "INSERT INTO node VALUES (2, 1), (1, NULL), (3, 3)");
    fails(&mut db, "DROP TABLE dept", constraint);
    fails(&mut db, "TRUNCATE dept", constraint);
    consistent(&mut db);
}

#[test]
fn foreign_key_cascade_and_set_null_actions() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (id INT PRIMARY KEY)");
    ok(&mut db, "CREATE TABLE b (id INT PRIMARY KEY, a_id INT REFERENCES a ON DELETE CASCADE ON UPDATE CASCADE)");
    ok(&mut db, "CREATE TABLE c (id INT PRIMARY KEY, b_id INT REFERENCES b ON DELETE CASCADE)");
    ok(&mut db, "CREATE TABLE note (id INT PRIMARY KEY, b_id INT REFERENCES b(id) ON DELETE SET NULL)");
    ok(&mut db, "INSERT INTO a VALUES (1), (2)");
    ok(&mut db, "INSERT INTO b VALUES (10, 1), (11, 1), (20, 2)");
    ok(&mut db, "INSERT INTO c VALUES (100, 10), (101, 11), (200, 20)");
    ok(&mut db, "INSERT INTO note VALUES (1, 10), (2, 20)");
    // ON UPDATE CASCADE rewrites the children's key
    ok(&mut db, "UPDATE a SET id = 5 WHERE id = 2");
    assert_eq!(one(&mut db, "SELECT a_id FROM b WHERE id = 20"), i(5));
    // cascading through two levels, plus SET NULL
    assert_eq!(done(&mut db, "DELETE FROM a WHERE id = 1"), "1 row(s) deleted");
    assert_eq!(ints(&mut db, "SELECT id FROM b"), vec![20]);
    assert_eq!(ints(&mut db, "SELECT id FROM c"), vec![200]);
    assert_eq!(rows(&mut db, "SELECT id, b_id FROM note ORDER BY id"), vec![vec![i(1), NULL], vec![i(2), i(20)]]);
    consistent(&mut db);
}

#[test]
fn a_cascade_that_hits_a_restrict_undoes_the_whole_statement() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE p (id INT PRIMARY KEY)");
    ok(&mut db, "CREATE TABLE c (id INT PRIMARY KEY, p_id INT REFERENCES p ON DELETE CASCADE)");
    ok(&mut db, "CREATE TABLE g (id INT PRIMARY KEY, c_id INT REFERENCES c)"); // RESTRICT
    ok(&mut db, "INSERT INTO p VALUES (1), (2)");
    ok(&mut db, "INSERT INTO c VALUES (10, 1), (20, 2)");
    ok(&mut db, "INSERT INTO g VALUES (100, 20)");
    let m = fails(&mut db, "DELETE FROM p", constraint);
    assert!(m.contains("g_c_id_fkey"), "{m}");
    assert_eq!(ints(&mut db, "SELECT id FROM p ORDER BY id"), vec![1, 2]);
    assert_eq!(ints(&mut db, "SELECT id FROM c ORDER BY id"), vec![10, 20]);
    ok(&mut db, "DELETE FROM p WHERE id = 1");
    assert_eq!(ints(&mut db, "SELECT id FROM c"), vec![20]);
}

#[test]
fn self_referencing_cascade_removes_a_long_chain() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE chain (id INT PRIMARY KEY, prev INT REFERENCES chain(id) ON DELETE CASCADE)");
    ok(&mut db, "INSERT INTO chain VALUES (1, NULL)");
    let values: Vec<String> = (2..=500).map(|n| format!("({n}, {})", n - 1)).collect();
    ok(&mut db, &format!("INSERT INTO chain VALUES {}", values.join(", ")));
    ok(&mut db, "DELETE FROM chain WHERE id = 1");
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM chain"), i(0));
}

#[test]
fn composite_keys_and_composite_foreign_keys() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE course (dept TEXT, num INT, title TEXT, PRIMARY KEY (dept, num))");
    ok(&mut db, "CREATE TABLE enroll (student INT, dept TEXT, num INT, FOREIGN KEY (dept, num) REFERENCES course (dept, num) ON DELETE CASCADE, PRIMARY KEY (student, dept, num))");
    ok(&mut db, "INSERT INTO course VALUES ('cs', 101, 'Intro'), ('cs', 201, 'Data'), ('ma', 101, 'Calc')");
    ok(&mut db, "INSERT INTO enroll VALUES (1, 'cs', 101), (1, 'ma', 101), (2, 'cs', 101), (2, 'cs', 201)");
    fails(&mut db, "INSERT INTO course VALUES ('cs', 101, 'dup')", constraint);
    fails(&mut db, "INSERT INTO enroll VALUES (3, 'cs', 999)", constraint);
    assert!(plan(&mut db, "SELECT * FROM course WHERE dept = 'cs' AND num = 201").contains("primary key lookup"));
    assert!(plan(&mut db, "SELECT * FROM course WHERE dept = 'cs'").contains("primary key prefix scan"));
    assert_eq!(rows(&mut db, "SELECT title FROM course WHERE dept = 'cs' AND num = 201"), vec![vec![t("Data")]]);
    assert_eq!(ints(&mut db, "SELECT num FROM course WHERE dept = 'cs' ORDER BY num"), vec![101, 201]);
    assert_eq!(ints(&mut db, "SELECT num FROM course WHERE dept = 'cs' AND num > 150"), vec![201]);
    ok(&mut db, "DELETE FROM course WHERE dept = 'cs' AND num = 101");
    assert_eq!(rows(&mut db, "SELECT student, dept, num FROM enroll ORDER BY student, dept"), vec![
        vec![i(1), t("ma"), i(101)],
        vec![i(2), t("cs"), i(201)],
    ]);
    consistent(&mut db);
}

// ========================================================= data manipulation

#[test]
fn multi_row_insert_is_all_or_nothing() {
    let mut db = Database::in_memory();
    shop(&mut db);
    let before = rows(&mut db, "SELECT * FROM orders ORDER BY id");
    fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (1, 1), (2, 2), (3, -5)", constraint);
    fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (1, 1), (99, 2)", constraint);
    fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (1, 1), (2)", parse);
    fails(&mut db, "INSERT INTO orders (customer_id, nope) VALUES (1, 1)", schema);
    fails(&mut db, "INSERT INTO orders (customer_id, customer_id) VALUES (1, 1)", schema);
    assert_eq!(rows(&mut db, "SELECT * FROM orders ORDER BY id"), before);
    consistent(&mut db);
}

#[test]
fn insert_from_a_select() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "CREATE TABLE big_spenders (customer TEXT PRIMARY KEY, spent REAL)");
    let m = done(
        &mut db,
        "INSERT INTO big_spenders SELECT c.name, SUM(o.total) FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.name HAVING SUM(o.total) > 10",
    );
    assert_eq!(m, "2 row(s) inserted");
    assert_eq!(rows(&mut db, "SELECT * FROM big_spenders ORDER BY customer"), vec![
        vec![t("Ada"), Value::Float(29.5)],
        vec![t("Alan"), Value::Float(100.0)],
    ]);
    fails(&mut db, "INSERT INTO big_spenders SELECT name FROM customers", parse);
    fails(&mut db, "INSERT INTO big_spenders SELECT name, 1.0 FROM customers WHERE city = 'London'", constraint);
}

#[test]
fn updates_are_evaluated_against_the_old_rows() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, a INT, b INT)");
    ok(&mut db, "INSERT INTO t VALUES (1, 1, 2), (2, 3, 4), (3, 5, 6), (4, 7, 8)");
    // SET a = b, b = a swaps (both read the old row)
    ok(&mut db, "UPDATE t SET a = b, b = a WHERE id = 1");
    assert_eq!(rows(&mut db, "SELECT a, b FROM t WHERE id = 1"), vec![vec![i(2), i(1)]]);
    // shifting every primary key up by one does not collide with itself
    assert_eq!(done(&mut db, "UPDATE t SET id = id + 1"), "4 row(s) updated");
    assert_eq!(ints(&mut db, "SELECT id FROM t ORDER BY id"), vec![2, 3, 4, 5]);
    // conditional update with CASE and a subquery
    ok(&mut db, "UPDATE t SET a = CASE WHEN a > 4 THEN a * 10 ELSE -a END WHERE id IN (SELECT id FROM t WHERE b >= 4)");
    assert_eq!(ints(&mut db, "SELECT a FROM t ORDER BY id"), vec![2, -3, 50, 70]);
    assert_eq!(done(&mut db, "UPDATE t SET a = 0 WHERE id > 100"), "0 row(s) updated");
    // an update that would merge two keys fails and changes nothing
    fails(&mut db, "UPDATE t SET id = 2", constraint);
    fails(&mut db, "UPDATE t SET a = 1, a = 2", schema);
    fails(&mut db, "UPDATE t SET nope = 1", schema);
    fails(&mut db, "UPDATE t SET a = 'x'", type_err);
    fails(&mut db, "UPDATE t SET a = 1 WHERE COUNT(*) > 0", parse);
    consistent(&mut db);
}

#[test]
fn delete_with_conditions_and_subqueries() {
    let mut db = Database::in_memory();
    shop(&mut db);
    assert_eq!(done(&mut db, "DELETE FROM orders WHERE total < (SELECT AVG(total) FROM orders)"), "3 row(s) deleted");
    assert_eq!(ints(&mut db, "SELECT id FROM orders"), vec![4]);
    assert_eq!(done(&mut db, "DELETE FROM customers WHERE NOT EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = customers.id)"), "3 row(s) deleted");
    assert_eq!(rows(&mut db, "SELECT name FROM customers"), vec![vec![t("Alan")]]);
    ok(&mut db, "TRUNCATE orders");
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM orders"), i(0));
    consistent(&mut db);
}

#[test]
fn expressions_casts_and_functions() {
    let mut db = Database::in_memory();
    let r = rows(
        &mut db,
        "SELECT CAST('42' AS INT), CAST(2.6 AS INT), '7'::REAL, CAST(1 AS BOOL), CAST('no' AS BOOLEAN), CAST(3.5 AS TEXT), CAST(NULL AS INT)",
    );
    assert_eq!(r, vec![vec![i(42), i(3), Value::Float(7.0), Value::Bool(true), Value::Bool(false), t("3.5"), NULL]]);
    fails(&mut db, "SELECT CAST('abc' AS INT)", type_err);
    fails(&mut db, "SELECT CAST(1e30 AS INT)", type_err);
    let r = rows(
        &mut db,
        "SELECT substr('hello', 2, 3), substr('hello', 4), replace('a-b-c', '-', '+'), trim('  x '), round(2.567, 2), round(7), floor(-1.5), ceil(1.2), nullif(1, 1), ifnull(NULL, 'd'), greatest(3, NULL, 9), least(3, 1), typeof(1.5), concat('a', NULL, 1)",
    );
    assert_eq!(r, vec![vec![
        t("ell"), t("lo"), t("a+b+c"), t("x"), Value::Float(2.57), i(7), Value::Float(-2.0), Value::Float(2.0), NULL, t("d"), i(9), i(1), t("real"), t("a1"),
    ]]);
    let r = rows(&mut db, "SELECT CASE WHEN 1 > 2 THEN 'a' WHEN 2 > 1 THEN 'b' END, CASE 3 WHEN 1 THEN 'x' ELSE 'other' END, CASE WHEN NULL THEN 1 END");
    assert_eq!(r, vec![vec![t("b"), t("other"), NULL]]);
    // three-valued logic
    let r = rows(&mut db, "SELECT NULL = NULL, NULL OR true, NULL AND false, NOT NULL, 1 IN (2, NULL), 1 NOT IN (2, 3)");
    assert_eq!(r, vec![vec![NULL, Value::Bool(true), Value::Bool(false), NULL, NULL, Value::Bool(true)]]);
    fails(&mut db, "SELECT 1 / 0", type_err);
    fails(&mut db, "SELECT 1.5 % 0", type_err);
    fails(&mut db, "SELECT substr('x')", parse);
    fails(&mut db, "SELECT nosuchfn(1)", parse);
    fails(&mut db, "SELECT upper(1)", type_err);
}

#[test]
fn like_handles_long_text_without_exhausting_the_stack() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (s TEXT)");
    let long = "a".repeat(200_000);
    ok(&mut db, &format!("INSERT INTO t VALUES ('{long}b')"));
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM t WHERE s LIKE '%b'"), i(1));
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM t WHERE s LIKE '%a%a%c%'"), i(0));
    assert_eq!(one(&mut db, "SELECT 'abc' LIKE 'a_c' AND 'abc' NOT LIKE 'a%d' AND '' LIKE '%'"), Value::Bool(true));
}

// =========================================================== query execution

#[test]
fn projection_filtering_sorting_and_paging() {
    let mut db = Database::in_memory();
    shop(&mut db);
    assert_eq!(columns(&mut db, "SELECT id AS key, name, upper(city) FROM customers"), vec!["key", "name", "upper(city)"]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY city DESC, name"), vec![2, 3, 1, 4]);
    // NULL ordering: smallest by default, explicit NULLS FIRST / LAST
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY email"), vec![3, 1, 4, 2]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY email NULLS LAST"), vec![1, 4, 2, 3]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY email DESC NULLS FIRST"), vec![3, 2, 4, 1]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY id LIMIT 2 OFFSET 1"), vec![2, 3]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers ORDER BY id LIMIT ALL OFFSET 3"), vec![4]);
    assert!(rows(&mut db, "SELECT id FROM customers LIMIT 0").is_empty());
    assert!(rows(&mut db, "SELECT id FROM customers OFFSET 10").is_empty());
    assert_eq!(rows(&mut db, "SELECT DISTINCT city FROM customers ORDER BY 1").len(), 3);
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE email IS NULL OR city = 'Portland' ORDER BY id"), vec![2, 3]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE NOT vip AND name LIKE 'A%' ORDER BY id"), vec![1, 4]);
    fails(&mut db, "SELECT id FROM customers WHERE name", type_err);
    fails(&mut db, "SELECT id FROM customers ORDER BY", parse);
    fails(&mut db, "SELECT id FROM customers LIMIT -1", parse);
}

#[test]
fn inner_left_right_full_and_cross_joins() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE l (id INT PRIMARY KEY, k INT)");
    ok(&mut db, "CREATE TABLE r (id INT PRIMARY KEY, k INT)");
    ok(&mut db, "INSERT INTO l VALUES (1, 10), (2, 20), (3, NULL)");
    ok(&mut db, "INSERT INTO r VALUES (7, 20), (8, 30), (9, NULL)");
    let q = |kind: &str| format!("SELECT l.id, r.id FROM l {kind} JOIN r ON l.k = r.k ORDER BY l.id NULLS LAST, r.id");
    assert_eq!(rows(&mut db, &q("INNER")), vec![vec![i(2), i(7)]]);
    assert_eq!(rows(&mut db, &q("LEFT")), vec![vec![i(1), NULL], vec![i(2), i(7)], vec![i(3), NULL]]);
    assert_eq!(rows(&mut db, &q("RIGHT")), vec![vec![i(2), i(7)], vec![NULL, i(8)], vec![NULL, i(9)]]);
    assert_eq!(
        rows(&mut db, &q("FULL OUTER")),
        vec![vec![i(1), NULL], vec![i(2), i(7)], vec![i(3), NULL], vec![NULL, i(8)], vec![NULL, i(9)]]
    );
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM l CROSS JOIN r"), i(9));
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM l, r WHERE l.k < r.k"), i(3));
    // equi-joins use a hash join, other conditions a nested loop; both give the same answer
    assert!(plan(&mut db, &q("LEFT")).contains("hash left join"));
    assert!(plan(&mut db, "SELECT * FROM l JOIN r ON l.k < r.k").contains("nested loop inner join"));
    assert_eq!(
        rows(&mut db, "SELECT l.id, r.id FROM l JOIN r ON l.k = r.k AND r.id > 0"),
        rows(&mut db, "SELECT l.id, r.id FROM l JOIN r ON l.k + 0 = r.k + 0 AND r.id > 0")
    );
    // self join and a three-way join with a derived table
    ok(&mut db, "CREATE TABLE emp (id INT PRIMARY KEY, boss INT, name TEXT)");
    ok(&mut db, "INSERT INTO emp VALUES (1, NULL, 'ceo'), (2, 1, 'vp'), (3, 2, 'dev')");
    assert_eq!(
        rows(&mut db, "SELECT e.name, b.name FROM emp e LEFT JOIN emp b ON e.boss = b.id ORDER BY e.id"),
        vec![vec![t("ceo"), NULL], vec![t("vp"), t("ceo")], vec![t("dev"), t("vp")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT e.name, s.n FROM emp e JOIN (SELECT boss, COUNT(*) AS n FROM emp WHERE boss IS NOT NULL GROUP BY boss) s ON s.boss = e.id JOIN l ON l.id = e.id ORDER BY e.name"
        ),
        vec![vec![t("ceo"), i(1)], vec![t("vp"), i(1)]]
    );
    fails(&mut db, "SELECT * FROM l JOIN l ON l.id = l.id", schema);
    fails(&mut db, "SELECT id FROM l JOIN r ON l.k = r.k", schema); // ambiguous
    fails(&mut db, "SELECT * FROM l JOIN r", parse);
    fails(&mut db, "SELECT * FROM l LEFT r ON 1 = 1", parse);
    fails(&mut db, "SELECT * FROM (SELECT 1)", parse);
}

#[test]
fn grouping_having_and_aggregates() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "INSERT INTO orders (customer_id, total) VALUES (2, 5.25), (3, NULL)");
    let r = rows(&mut db, "SELECT customer_id, COUNT(*), COUNT(total), SUM(total), MIN(total), MAX(total) FROM orders GROUP BY customer_id ORDER BY 1");
    assert_eq!(r, vec![
        vec![i(1), i(2), i(2), Value::Float(29.5), Value::Float(9.5), Value::Float(20.0)],
        vec![i(2), i(2), i(2), Value::Float(10.5), Value::Float(5.25), Value::Float(5.25)],
        vec![i(3), i(1), i(0), NULL, NULL, NULL],
        vec![i(4), i(1), i(1), Value::Float(100.0), Value::Float(100.0), Value::Float(100.0)],
    ]);
    assert_eq!(one(&mut db, "SELECT COUNT(DISTINCT total) FROM orders"), i(4));
    assert_eq!(one(&mut db, "SELECT SUM(DISTINCT total) FROM orders"), Value::Float(134.75));
    assert_eq!(one(&mut db, "SELECT AVG(total) FROM orders WHERE customer_id = 1"), Value::Float(14.75));
    // group by alias and position; HAVING with an aggregate not in the select list
    assert_eq!(
        rows(&mut db, "SELECT city AS c, COUNT(*) FROM customers GROUP BY c HAVING MAX(id) > 2 ORDER BY 2 DESC, 1"),
        vec![vec![t("London"), i(2)], vec![t("NYC"), i(1)]]
    );
    assert_eq!(rows(&mut db, "SELECT city, COUNT(*) FROM customers GROUP BY 1 ORDER BY 1").len(), 3);
    // aggregate over nothing
    assert_eq!(rows(&mut db, "SELECT COUNT(*), SUM(total), MAX(total) FROM orders WHERE id < 0"), vec![vec![i(0), NULL, NULL]]);
    assert!(rows(&mut db, "SELECT customer_id, COUNT(*) FROM orders WHERE id < 0 GROUP BY customer_id").is_empty());
    // integer and real keys that are equal fall in one group
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM (SELECT 1 AS k UNION ALL SELECT 1.0) x GROUP BY k"), i(2));
    fails(&mut db, "SELECT COUNT(*) FROM orders WHERE SUM(total) > 1", parse);
    fails(&mut db, "SELECT id FROM orders HAVING id > 1", parse);
    fails(&mut db, "SELECT SUM(COUNT(*)) FROM orders", parse);
    fails(&mut db, "SELECT SUM(status) FROM orders", type_err);
    fails(&mut db, "SELECT COUNT(*) FROM orders GROUP BY COUNT(*)", parse);
}

#[test]
fn subqueries_scalar_in_exists_and_derived() {
    let mut db = Database::in_memory();
    shop(&mut db);
    assert_eq!(one(&mut db, "SELECT name FROM customers WHERE id = (SELECT customer_id FROM orders WHERE total = 100)"), t("Alan"));
    assert_eq!(
        rows(&mut db, "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders) ORDER BY name"),
        vec![vec![t("Ada")], vec![t("Alan")], vec![t("Linus")]]
    );
    assert_eq!(rows(&mut db, "SELECT name FROM customers WHERE id NOT IN (SELECT customer_id FROM orders)"), vec![vec![t("Grace")]]);
    // NOT IN with a NULL in the list is unknown for every row
    assert!(rows(&mut db, "SELECT name FROM customers WHERE id NOT IN (SELECT NULL UNION SELECT 1)").is_empty());
    // correlated: each customer's largest order
    assert_eq!(
        rows(&mut db, "SELECT c.name, (SELECT MAX(total) FROM orders o WHERE o.customer_id = c.id) AS top FROM customers c ORDER BY c.id"),
        vec![vec![t("Ada"), Value::Float(20.0)], vec![t("Linus"), Value::Float(5.25)], vec![t("Grace"), NULL], vec![t("Alan"), Value::Float(100.0)]]
    );
    assert_eq!(
        ints(&mut db, "SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.total > 10) ORDER BY id"),
        vec![1, 4]
    );
    assert_eq!(
        one(&mut db, "SELECT SUM(n) FROM (SELECT customer_id, COUNT(*) AS n FROM orders GROUP BY customer_id) AS per"),
        i(4)
    );
    // uncorrelated subqueries run once, correlated ones per row
    let p = plan(&mut db, "SELECT name FROM customers WHERE id IN (SELECT customer_id FROM orders)");
    assert!(p.contains("uncorrelated"), "{p}");
    let p = plan(&mut db, "SELECT name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)");
    assert!(p.contains("correlated, evaluated per row"), "{p}");
    // a subquery nested two deep that refers to the outermost query
    assert_eq!(
        ints(&mut db, "SELECT id FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND EXISTS (SELECT 1 FROM customers c2 WHERE c2.id = o.customer_id AND c2.city = c.city AND c.city = 'London')) ORDER BY id"),
        vec![1, 4]
    );
    fails(&mut db, "SELECT (SELECT id FROM customers)", type_err);
    fails(&mut db, "SELECT (SELECT id, name FROM customers WHERE id = 1)", schema);
    fails(&mut db, "SELECT 1 WHERE 1 IN (SELECT id, name FROM customers)", schema);
    fails(&mut db, "SELECT * FROM customers WHERE id IN (SELECT nope FROM orders)", schema);
}

#[test]
fn union_intersect_except() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (x INT)");
    ok(&mut db, "CREATE TABLE b (x INT)");
    ok(&mut db, "INSERT INTO a VALUES (1), (2), (2), (3)");
    ok(&mut db, "INSERT INTO b VALUES (2), (3), (4)");
    assert_eq!(ints(&mut db, "SELECT x FROM a UNION SELECT x FROM b ORDER BY x"), vec![1, 2, 3, 4]);
    assert_eq!(ints(&mut db, "SELECT x FROM a UNION ALL SELECT x FROM b ORDER BY 1"), vec![1, 2, 2, 2, 3, 3, 4]);
    assert_eq!(ints(&mut db, "SELECT x FROM a INTERSECT SELECT x FROM b ORDER BY x"), vec![2, 3]);
    assert_eq!(ints(&mut db, "SELECT x FROM a EXCEPT SELECT x FROM b"), vec![1]);
    assert_eq!(ints(&mut db, "SELECT x FROM a EXCEPT ALL SELECT x FROM b ORDER BY x"), vec![1, 2]);
    assert_eq!(ints(&mut db, "SELECT x FROM a UNION SELECT x FROM b ORDER BY x DESC LIMIT 2"), vec![4, 3]);
    fails(&mut db, "SELECT x FROM a UNION SELECT x, x FROM b", schema);
    fails(&mut db, "SELECT x FROM a UNION SELECT x FROM b ORDER BY x + 1", parse);
}

// ==================================================================== indexes

#[test]
fn index_creation_deletion_and_use() {
    let mut db = Database::in_memory();
    shop(&mut db);
    assert_eq!(done(&mut db, "CREATE INDEX cust_city_name ON customers (city, name)"), "index 'cust_city_name' created on customers(city, name)");
    assert!(plan(&mut db, "SELECT * FROM customers WHERE city = 'London'").contains("index cust_city_name (city =)"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE city = 'London' AND name = 'Ada'").contains("(city =, name =)"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE city = 'London' AND name > 'B'").contains("(city =, name range)"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE email = 'ada@x.org'").contains("index customers_email_key"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE id IN (1, 3)").contains("primary key IN lookup (2 keys)"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE id BETWEEN 2 AND 3").contains("primary key range scan"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE id = -1").contains("primary key lookup"));
    assert!(plan(&mut db, "SELECT * FROM customers WHERE name = 'Ada'").contains("full scan"));
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE city = 'London' AND name > 'Ab'"), vec![1, 4]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE city = 'London' AND name > 'Ae'"), vec![4]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE id BETWEEN 2 AND 3"), vec![2, 3]);
    assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE id IN (3, 1, 3, 99)"), vec![1, 3]);
    fails(&mut db, "CREATE INDEX cust_city_name ON customers (id)", schema);
    assert_eq!(done(&mut db, "CREATE INDEX IF NOT EXISTS cust_city_name ON customers (id)"), "index 'cust_city_name' already exists");
    fails(&mut db, "CREATE INDEX i2 ON customers (nope)", schema);
    fails(&mut db, "CREATE INDEX i2 ON nope (id)", schema);
    fails(&mut db, "CREATE INDEX i2 ON customers (city, city)", schema);
    fails(&mut db, "CREATE UNIQUE INDEX cu ON customers (city)", constraint);
    assert_eq!(done(&mut db, "DROP INDEX cust_city_name"), "index 'cust_city_name' dropped");
    assert!(plan(&mut db, "SELECT * FROM customers WHERE city = 'London'").contains("full scan"));
    fails(&mut db, "DROP INDEX cust_city_name", schema);
    assert_eq!(done(&mut db, "DROP INDEX IF EXISTS cust_city_name"), "index 'cust_city_name' does not exist, nothing dropped");
    // a unique constraint that a foreign key relies on cannot be dropped
    ok(&mut db, "CREATE TABLE refs (email TEXT REFERENCES customers(email))");
    fails(&mut db, "DROP INDEX customers_email_key ON customers", constraint);
    consistent(&mut db);
}

#[test]
fn unique_index_enforces_and_tolerates_nulls() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT)");
    ok(&mut db, "INSERT INTO t VALUES (1, 1, 'x'), (2, NULL, 'x'), (3, NULL, 'y')");
    ok(&mut db, "CREATE UNIQUE INDEX t_ab ON t (a, b)");
    ok(&mut db, "CREATE UNIQUE INDEX t_a ON t (a)");
    fails(&mut db, "INSERT INTO t VALUES (4, 1, 'z')", constraint);
    ok(&mut db, "INSERT INTO t VALUES (4, NULL, 'x')");
    fails(&mut db, "CREATE UNIQUE INDEX t_b ON t (b)", constraint);
    // composite index still finds rows whose later column is NULL
    assert_eq!(ints(&mut db, "SELECT id FROM t WHERE a IS NULL ORDER BY id"), vec![2, 3, 4]);
    ok(&mut db, "CREATE INDEX t_ba ON t (b, a)");
    assert_eq!(ints(&mut db, "SELECT id FROM t WHERE b = 'x' ORDER BY id"), vec![1, 2, 4]);
    assert!(plan(&mut db, "SELECT id FROM t WHERE b = 'x'").contains("index t_ba"));
    consistent(&mut db);
}

#[test]
fn indexes_stay_consistent_through_updates_and_deletes() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, grp INT, name TEXT, score REAL)");
    ok(&mut db, "CREATE INDEX t_grp ON t (grp)");
    ok(&mut db, "CREATE INDEX t_grp_name ON t (grp, name)");
    ok(&mut db, "CREATE INDEX t_score ON t (score)");
    let vals: Vec<String> = (1..=300).map(|n| format!("({n}, {}, 'n{}', {}.5)", n % 7, n % 13, n % 50)).collect();
    ok(&mut db, &format!("INSERT INTO t VALUES {}", vals.join(", ")));
    ok(&mut db, "UPDATE t SET grp = grp + 10, name = name || 'x' WHERE id % 3 = 0");
    ok(&mut db, "UPDATE t SET id = id + 1000 WHERE id % 5 = 0");
    ok(&mut db, "DELETE FROM t WHERE grp = 2 OR score > 45");
    ok(&mut db, "UPDATE t SET score = NULL WHERE id % 11 = 0");
    consistent(&mut db);
    // every indexed access path agrees with a full scan of the same predicate
    for (indexed, scan) in [
        ("grp = 3", "grp + 0 = 3"),
        ("grp = 13", "grp + 0 = 13"),
        ("grp = 4 AND name = 'n4'", "grp + 0 = 4 AND name || '' = 'n4'"),
        ("grp = 11 AND name >= 'n2x'", "grp + 0 = 11 AND name || '' >= 'n2x'"),
        ("score >= 10 AND score < 20", "score + 0 >= 10 AND score + 0 < 20"),
        ("score > 40.5", "score + 0 > 40.5"),
        ("id > 1100", "id + 0 > 1100"),
        ("id BETWEEN 10 AND 40", "id + 0 BETWEEN 10 AND 40"),
    ] {
        let a = ints(&mut db, &format!("SELECT id FROM t WHERE {indexed} ORDER BY id"));
        let b = ints(&mut db, &format!("SELECT id FROM t WHERE {scan} ORDER BY id"));
        assert_eq!(a, b, "{indexed}");
        assert!(plan(&mut db, &format!("SELECT id FROM t WHERE {scan}")).contains("full scan"));
        assert!(!plan(&mut db, &format!("SELECT id FROM t WHERE {indexed}")).contains("full scan"), "{indexed}");
    }
}

/// Random inserts, updates and deletes against an indexed table, checked
/// after every step against a plain in-test model.
#[test]
fn randomized_operations_match_a_model() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT UNIQUE)");
    ok(&mut db, "CREATE INDEX t_a ON t (a)");
    ok(&mut db, "CREATE INDEX t_ab ON t (a, b)");
    let mut model: std::collections::BTreeMap<i64, (i64, String)> = Default::default();
    let mut seed: u64 = 0x9e3779b97f4a7c15;
    let mut rnd = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    for step in 0..600 {
        let id = rnd(60) as i64;
        let a = rnd(8) as i64;
        let b = format!("b{}", rnd(80));
        match rnd(4) {
            0 | 1 => {
                let dup = model.contains_key(&id) || model.values().any(|(_, x)| *x == b);
                let r = db.execute(&format!("INSERT INTO t VALUES ({id}, {a}, '{b}')"));
                assert_eq!(r.is_err(), dup, "step {step}: insert {id} {b}: {r:?}");
                if !dup {
                    model.insert(id, (a, b));
                }
            }
            2 => {
                let r = db.execute(&format!("UPDATE t SET a = {a}, b = '{b}' WHERE id = {id}"));
                let conflict = model.contains_key(&id) && model.iter().any(|(k, (_, x))| *k != id && *x == b);
                assert_eq!(r.is_err(), conflict, "step {step}: update {id} {b}: {r:?}");
                if !conflict {
                    if let Some(v) = model.get_mut(&id) {
                        *v = (a, b);
                    }
                }
            }
            _ => {
                ok(&mut db, &format!("DELETE FROM t WHERE a = {a} AND id > {id}"));
                model.retain(|k, v| !(v.0 == a && *k > id));
            }
        }
        if step % 50 == 0 || step == 599 {
            for probe in 0..8 {
                let want: Vec<i64> = model.iter().filter(|(_, v)| v.0 == probe).map(|(k, _)| *k).collect();
                assert_eq!(ints(&mut db, &format!("SELECT id FROM t WHERE a = {probe} ORDER BY id")), want);
            }
            let want: Vec<i64> = model.keys().copied().filter(|k| (10..30).contains(k)).collect();
            assert_eq!(ints(&mut db, "SELECT id FROM t WHERE id >= 10 AND id < 30 ORDER BY id"), want);
            consistent(&mut db);
        }
    }
}

// ================================================================ ALTER TABLE

#[test]
fn alter_table_add_drop_and_rename_columns() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "ALTER TABLE customers ADD COLUMN points INT NOT NULL DEFAULT 0");
    assert_eq!(ints(&mut db, "SELECT points FROM customers"), vec![0, 0, 0, 0]);
    fails(&mut db, "ALTER TABLE customers ADD COLUMN must INT NOT NULL", constraint);
    fails(&mut db, "ALTER TABLE customers ADD COLUMN name TEXT", schema);
    fails(&mut db, "ALTER TABLE customers ADD COLUMN pk2 INT PRIMARY KEY", schema);
    ok(&mut db, "ALTER TABLE customers ADD seq INT AUTOINCREMENT");
    assert_eq!(ints(&mut db, "SELECT seq FROM customers ORDER BY id"), vec![1, 2, 3, 4]);
    ok(&mut db, "INSERT INTO customers (id, name) VALUES (5, 'Edsger')");
    assert_eq!(one(&mut db, "SELECT seq FROM customers WHERE id = 5"), i(5));
    // rename a column used by an index, a unique constraint, a CHECK and a foreign key
    ok(&mut db, "CREATE INDEX cust_city ON customers (city)");
    ok(&mut db, "ALTER TABLE customers ADD CONSTRAINT points_ok CHECK (points >= 0)");
    ok(&mut db, "ALTER TABLE customers RENAME COLUMN city TO town");
    ok(&mut db, "ALTER TABLE customers RENAME points TO pts");
    ok(&mut db, "ALTER TABLE customers RENAME COLUMN id TO cid");
    assert!(plan(&mut db, "SELECT * FROM customers WHERE town = 'London'").contains("index cust_city (town =)"));
    assert_eq!(ints(&mut db, "SELECT cid FROM customers WHERE town = 'London' ORDER BY cid"), vec![1, 4]);
    fails(&mut db, "UPDATE customers SET pts = -1", constraint);
    fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (99, 1)", constraint);
    ok(&mut db, "DELETE FROM customers WHERE cid = 1"); // cascade still follows the renamed key
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM orders WHERE customer_id = 1"), i(0));
    // drop a column: its indexes and CHECKs go with it
    ok(&mut db, "ALTER TABLE customers DROP COLUMN town");
    assert!(rows(&mut db, "DESCRIBE customers").iter().all(|r| r[0] != t("town")));
    fails(&mut db, "SELECT town FROM customers", schema);
    ok(&mut db, "ALTER TABLE customers DROP COLUMN pts");
    ok(&mut db, "ALTER TABLE customers DROP COLUMN IF EXISTS pts");
    fails(&mut db, "ALTER TABLE customers DROP COLUMN pts", schema);
    fails(&mut db, "ALTER TABLE customers DROP COLUMN cid", schema); // primary key
    fails(&mut db, "ALTER TABLE customers RENAME COLUMN name TO email", schema);
    assert_eq!(rows(&mut db, "SELECT * FROM customers WHERE cid = 2"), vec![vec![i(2), t("Linus"), t("linus@x.org"), Value::Bool(false), i(2)]]);
    consistent(&mut db);
}

#[test]
fn alter_table_rename_table_keeps_data_indexes_and_references() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "ALTER TABLE customers RENAME TO clients");
    fails(&mut db, "SELECT * FROM customers", schema);
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM clients"), i(4));
    assert!(plan(&mut db, "SELECT * FROM clients WHERE email = 'ada@x.org'").contains("index customers_email_key"));
    fails(&mut db, "INSERT INTO clients (id, name, email) VALUES (9, 'x', 'ada@x.org')", constraint);
    fails(&mut db, "INSERT INTO orders (customer_id) VALUES (42)", constraint);
    assert!(rows(&mut db, "DESCRIBE orders")[1][5].display().contains("references clients(id)"));
    ok(&mut db, "ALTER TABLE orders RENAME TO purchases");
    ok(&mut db, "INSERT INTO purchases (customer_id, total) VALUES (2, 1)");
    assert_eq!(one(&mut db, "SELECT MAX(id) FROM purchases"), i(5)); // the sequence moved too
    fails(&mut db, "ALTER TABLE purchases RENAME TO clients", schema);
    fails(&mut db, "ALTER TABLE purchases RENAME TO \"bad name\"", schema);
    consistent(&mut db);
}

#[test]
fn alter_column_defaults_nullability_and_types() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n TEXT, v INT)");
    ok(&mut db, "INSERT INTO t VALUES (1, '10', NULL), (2, '20', 5)");
    ok(&mut db, "ALTER TABLE t ALTER COLUMN v SET DEFAULT 42");
    ok(&mut db, "INSERT INTO t (id, n) VALUES (3, '30')");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 3"), i(42));
    ok(&mut db, "ALTER TABLE t ALTER v DROP DEFAULT");
    ok(&mut db, "INSERT INTO t (id, n) VALUES (4, '40')");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 4"), NULL);
    fails(&mut db, "ALTER TABLE t ALTER COLUMN v SET NOT NULL", constraint);
    ok(&mut db, "UPDATE t SET v = 0 WHERE v IS NULL");
    ok(&mut db, "ALTER TABLE t ALTER COLUMN v SET NOT NULL");
    fails(&mut db, "INSERT INTO t (id) VALUES (5)", constraint);
    ok(&mut db, "ALTER TABLE t ALTER COLUMN v DROP NOT NULL");
    fails(&mut db, "ALTER TABLE t ALTER COLUMN id DROP NOT NULL", schema);
    // TYPE conversion is all or nothing, and rebuilds indexes on the column
    ok(&mut db, "CREATE INDEX t_n ON t (n)");
    ok(&mut db, "ALTER TABLE t ALTER COLUMN n TYPE INT");
    assert_eq!(ints(&mut db, "SELECT n FROM t WHERE n > 15 ORDER BY n"), vec![20, 30, 40]);
    assert!(plan(&mut db, "SELECT * FROM t WHERE n = 20").contains("index t_n"));
    fails(&mut db, "INSERT INTO t (id, n) VALUES (6, 'oops'::INT + 0)", type_err);
    consistent(&mut db);
}

#[test]
fn failed_alter_statements_change_nothing() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE p (id INT PRIMARY KEY)");
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, s TEXT, p_id INT, v INT)");
    ok(&mut db, "INSERT INTO p VALUES (1)");
    ok(&mut db, "INSERT INTO t VALUES (1, 'a', 1, 1), (2, 'a', 2, -1), (3, 'x', NULL, 3)");
    fails(&mut db, "ALTER TABLE t ALTER COLUMN s TYPE INT", type_err);
    fails(&mut db, "ALTER TABLE t ADD CONSTRAINT s_u UNIQUE (s)", constraint);
    fails(&mut db, "ALTER TABLE t ADD CHECK (v > 0)", constraint);
    fails(&mut db, "ALTER TABLE t ADD FOREIGN KEY (p_id) REFERENCES p(id)", constraint);
    fails(&mut db, "ALTER TABLE t ADD PRIMARY KEY (s)", schema);
    fails(&mut db, "ALTER TABLE t DROP CONSTRAINT nope", schema);
    // several actions in one statement: the last one fails, so none happen
    fails(&mut db, "ALTER TABLE t ADD COLUMN w INT DEFAULT 1, RENAME COLUMN s TO s2, ALTER COLUMN s2 TYPE INT", type_err);
    assert_eq!(columns(&mut db, "SELECT * FROM t"), vec!["id", "s", "p_id", "v"]);
    ok(&mut db, "DELETE FROM t WHERE id = 2");
    ok(&mut db, "ALTER TABLE t ADD CONSTRAINT t_p FOREIGN KEY (p_id) REFERENCES p(id), ADD CONSTRAINT v_pos CHECK (v > 0), ADD UNIQUE (s)");
    fails(&mut db, "INSERT INTO t VALUES (4, 'b', 7, 1)", constraint);
    fails(&mut db, "INSERT INTO t VALUES (4, 'b', 1, 0)", constraint);
    fails(&mut db, "INSERT INTO t VALUES (4, 'a', 1, 1)", constraint);
    ok(&mut db, "ALTER TABLE t DROP CONSTRAINT t_p, DROP CONSTRAINT v_pos, DROP CONSTRAINT t_s_key");
    ok(&mut db, "INSERT INTO t VALUES (4, 'a', 7, 0)");
    consistent(&mut db);
}

#[test]
fn drop_table_cascade_and_restrict() {
    let mut db = Database::in_memory();
    shop(&mut db);
    fails(&mut db, "DROP TABLE customers", constraint);
    fails(&mut db, "DROP TABLE customers RESTRICT", constraint);
    ok(&mut db, "DROP TABLE customers CASCADE");
    // the foreign key went away with the referenced table
    ok(&mut db, "INSERT INTO orders (customer_id, total) VALUES (999, 1)");
    assert!(rows(&mut db, "DESCRIBE orders")[1][5].display().is_empty());
    // re-creating a dropped table starts from scratch, sequences included
    ok(&mut db, "DROP TABLE orders");
    ok(&mut db, "CREATE TABLE orders (id SERIAL PRIMARY KEY, x INT)");
    ok(&mut db, "INSERT INTO orders (x) VALUES (1)");
    assert_eq!(one(&mut db, "SELECT id FROM orders"), i(1));
    assert_eq!(done(&mut db, "DROP TABLE IF EXISTS nope"), "table 'nope' does not exist, nothing dropped");
    fails(&mut db, "DROP TABLE nope", schema);
    consistent(&mut db);
}

// ============================================================ transactions

#[test]
fn ddl_and_constraint_failures_inside_transactions() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "BEGIN");
    ok(&mut db, "ALTER TABLE customers ADD COLUMN tier INT DEFAULT 1");
    ok(&mut db, "CREATE UNIQUE INDEX cust_name ON customers (name)");
    ok(&mut db, "INSERT INTO customers (id, name) VALUES (9, 'Zed')");
    // a failing statement inside the transaction undoes only itself
    assert!(db.execute("INSERT INTO customers (id, name) VALUES (10, 'Zed')").is_err());
    assert!(db.execute("DELETE FROM customers WHERE id = 9 OR id = 999 OR nope = 1").is_err());
    assert_eq!(one(&mut db, "SELECT tier FROM customers WHERE id = 9"), i(1));
    ok(&mut db, "ROLLBACK");
    assert_eq!(columns(&mut db, "SELECT * FROM customers").len(), 5);
    assert!(rows(&mut db, "SELECT * FROM customers WHERE id = 9").is_empty());
    ok(&mut db, "BEGIN");
    ok(&mut db, "DELETE FROM customers WHERE id = 1");
    ok(&mut db, "COMMIT");
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM orders WHERE customer_id = 1"), i(0));
    consistent(&mut db);
}

// ========================================================== restart behaviour

#[test]
fn schema_constraints_indexes_and_sequences_survive_restart() {
    let dir = tmp("restart");
    {
        let mut db = Database::open(&dir).unwrap();
        shop(&mut db);
        ok(&mut db, "CREATE TABLE course (dept TEXT, num INT, title VARCHAR(20) CHECK (length(title) > 1), PRIMARY KEY (dept, num))");
        ok(&mut db, "INSERT INTO course VALUES ('cs', 1, 'Intro'), ('cs', 2, 'Data')");
        ok(&mut db, "CREATE UNIQUE INDEX course_title ON course (title)");
        ok(&mut db, "CREATE INDEX cust_city_name ON customers (city, name)");
        ok(&mut db, "ALTER TABLE customers ADD COLUMN pts INT NOT NULL DEFAULT 5");
        ok(&mut db, "ALTER TABLE orders RENAME COLUMN status TO state");
    }
    for round in 0..2 {
        let mut db = Database::open(&dir).unwrap();
        consistent(&mut db);
        // constraints are still enforced
        fails(&mut db, "INSERT INTO customers (id, name, email) VALUES (50, 'x', 'ada@x.org')", constraint);
        fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (77, 1)", constraint);
        fails(&mut db, "INSERT INTO orders (customer_id, total) VALUES (1, -1)", constraint);
        fails(&mut db, "INSERT INTO course VALUES ('cs', 1, 'Dup key')", constraint);
        fails(&mut db, "INSERT INTO course VALUES ('cs', 3, 'Data')", constraint);
        fails(&mut db, "INSERT INTO course VALUES ('cs', 3, 'X')", constraint);
        fails(&mut db, "INSERT INTO course VALUES ('cs', 3, 'This title is far too long')", type_err);
        // indexes are still used and correct
        assert!(plan(&mut db, "SELECT * FROM customers WHERE city = 'London' AND name = 'Ada'").contains("cust_city_name"));
        let london = if round == 0 { vec![1, 4] } else { vec![1] };
        assert_eq!(ints(&mut db, "SELECT id FROM customers WHERE city = 'London' ORDER BY id"), london);
        assert!(plan(&mut db, "SELECT * FROM orders WHERE customer_id = 1").contains("orders_customer"));
        // defaults, renamed columns and the auto-increment sequence carry on
        assert_eq!(rows(&mut db, "SELECT DISTINCT pts FROM customers"), vec![vec![i(5)]]);
        ok(&mut db, &format!("INSERT INTO orders (customer_id, total) VALUES (2, {round})"));
        assert_eq!(one(&mut db, &format!("SELECT id FROM orders WHERE total = {round}")), i(5 + round as i64));
        assert_eq!(one(&mut db, &format!("SELECT state FROM orders WHERE total = {round}")), t("new"));
        // cascades still follow the stored foreign key
        if round == 0 {
            ok(&mut db, "DELETE FROM customers WHERE id = 4");
            assert_eq!(one(&mut db, "SELECT COUNT(*) FROM orders WHERE customer_id = 4"), i(0));
        }
        if round == 0 {
            db.checkpoint().unwrap(); // the second round reads from the snapshot
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_statement_is_not_written_to_the_log() {
    let dir = tmp("atomic-log");
    {
        let mut db = Database::open(&dir).unwrap();
        shop(&mut db);
        assert!(db.execute("DELETE FROM customers WHERE id = 1 OR 1 / (id - 3) > 0").is_err());
        assert!(db.execute("INSERT INTO orders (customer_id, total) VALUES (1, 1), (1, -1)").is_err());
        assert!(db.execute("ALTER TABLE customers ADD COLUMN z INT DEFAULT 0, ALTER COLUMN name TYPE INT").is_err());
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM customers"), i(4));
    assert_eq!(one(&mut db, "SELECT COUNT(*) FROM orders"), i(4));
    assert_eq!(columns(&mut db, "SELECT * FROM customers").len(), 5);
    consistent(&mut db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reindex_rebuilds_entries_and_check_table_reports_them() {
    let mut db = Database::in_memory();
    shop(&mut db);
    ok(&mut db, "CREATE INDEX cust_city ON customers (city)");
    let before = rows(&mut db, "CHECK TABLE customers");
    assert!(before.iter().any(|r| r[1] == t("index cust_city") && r[3] == t("4 entries")), "{before:?}");
    assert_eq!(done(&mut db, "REINDEX TABLE customers"), "2 index(es) rebuilt on 1 table(s)");
    assert_eq!(done(&mut db, "REINDEX"), "3 index(es) rebuilt on 2 table(s)");
    assert_eq!(rows(&mut db, "CHECK TABLE customers"), before);
    consistent(&mut db);
    fails(&mut db, "CHECK TABLE nope", schema);
    fails(&mut db, "REINDEX TABLE nope", schema);
}

// ============================================================ negative input

#[test]
fn invalid_sql_is_rejected_with_typed_errors() {
    let mut db = Database::in_memory();
    shop(&mut db);
    for sql in [
        "SELEC * FROM customers",
        "SELECT * FROM",
        "SELECT * FROM customers WHERE",
        "SELECT * FROM customers ORDER BY id DESC NULLS",
        "SELECT CASE END",
        "SELECT CASE WHEN 1 THEN 2",
        "SELECT CAST(1 AS)",
        "SELECT 1 FROM customers WHERE id IN ()",
        "SELECT (SELECT 1",
        "SELECT * FROM customers WHERE EXISTS SELECT 1",
        "INSERT INTO customers VALUES",
        "INSERT customers VALUES (1)",
        "UPDATE customers SET",
        "UPDATE customers id = 1",
        "DELETE customers",
        "CREATE TABLE x",
        "CREATE TABLE x ()",
        "CREATE TABLE x (a INT,)",
        "CREATE TABLE x (a INT CONSTRAINT)",
        "CREATE TABLE x (a INT REFERENCES)",
        "CREATE INDEX ON customers (id)",
        "CREATE UNIQUE TABLE x (a INT)",
        "ALTER TABLE customers",
        "ALTER TABLE customers FROB x",
        "ALTER TABLE customers ALTER COLUMN name SET",
        "ALTER TABLE customers RENAME COLUMN name",
        "DROP TABLE",
        "SELECT 'unterminated",
        "SELECT \"unterminated",
        "SELECT 1 ; SELECT 2",
        "SELECT @x",
        "SELECT x::BLOB FROM customers",
        "EXPLAIN DROP TABLE customers",
    ] {
        fails(&mut db, sql, parse);
    }
    for sql in [
        "SELECT * FROM nope",
        "SELECT nope FROM customers",
        "SELECT c.name FROM customers",
        "SELECT x.* FROM customers",
        "INSERT INTO nope VALUES (1)",
        "UPDATE nope SET a = 1",
        "DELETE FROM nope",
        "ALTER TABLE nope ADD COLUMN a INT",
        "ALTER TABLE customers ALTER COLUMN nope SET DEFAULT 1",
        "DESCRIBE nope",
        "TRUNCATE nope",
    ] {
        fails(&mut db, sql, schema);
    }
    consistent(&mut db);
}

#[test]
fn subquery_and_expression_limits_are_enforced() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY)");
    ok(&mut db, "INSERT INTO t VALUES (1)");
    let nest = |n: usize| {
        let mut q = "SELECT id FROM t".to_string();
        for _ in 0..n {
            q = format!("SELECT id FROM t WHERE id IN ({q})");
        }
        q
    };
    assert_eq!(ints(&mut db, &nest(6)), vec![1]);
    let m = fails(&mut db, &nest(7), parse);
    assert!(m.contains("subqueries are nested deeper than 6"), "{m}");
    // each clause of a subquery gets the remaining operator budget, not a fresh one
    let chain = |n: usize| format!("SELECT (SELECT 0{} FROM t) FROM t", "+1".repeat(n));
    assert!(db.execute(&chain(95)).is_ok());
    assert!(matches!(db.execute(&chain(101)), Err(Error::Parse(_))));
    let cases = (0..60).map(|k| format!("WHEN id = {k} THEN {k}")).collect::<Vec<_>>().join(" ");
    assert_eq!(one(&mut db, &format!("SELECT CASE {cases} END FROM t")), i(1));
}
