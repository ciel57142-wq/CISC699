//! Relational engine: DDL, DML, query features, NULL semantics, types,
//! functions, transactions, constraints and their interactions, errors.

mod common;
use common::*;
use polydb::{Database, Error, Output, Value};

fn i(n: i64) -> Value {
    Value::Int(n)
}
fn t(s: &str) -> Value {
    Value::Text(s.into())
}
const N: Value = Value::Null;

fn setup() -> Database {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE emp (id INT PRIMARY KEY, name TEXT NOT NULL, dept TEXT, salary INT, boss INT)");
    ok(&mut db, "INSERT INTO emp VALUES (1, 'Ada', 'eng', 120, NULL), (2, 'Bob', 'eng', 90, 1), (3, 'Cy', 'ops', 80, 1), (4, 'Di', 'ops', NULL, 3), (5, 'Ed', NULL, 70, 3)");
    db
}

#[test]
fn ddl_create_drop_alter() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (x INT)");
    assert!(matches!(
        db.execute("CREATE TABLE a (x INT)"),
        Err(Error::Schema(_))
    ));
    ok(&mut db, "CREATE TABLE IF NOT EXISTS a (x INT)");
    assert!(matches!(
        db.execute("CREATE TABLE b (x INT, x TEXT)"),
        Err(Error::Schema(_))
    ));
    assert!(matches!(
        db.execute("CREATE TABLE c (x BLOB)"),
        Err(Error::Parse(_))
    ));
    assert!(matches!(
        db.execute("CREATE TABLE d (x REAL PRIMARY KEY)"),
        Err(Error::Schema(_))
    ));
    ok(&mut db, "INSERT INTO a VALUES (1), (2)");
    ok(&mut db, "ALTER TABLE a ADD COLUMN y TEXT DEFAULT 'z'");
    assert_eq!(
        rows(&mut db, "SELECT x, y FROM a ORDER BY x"),
        vec![vec![i(1), t("z")], vec![i(2), t("z")]]
    );
    ok(&mut db, "ALTER TABLE a RENAME COLUMN y TO w");
    ok(&mut db, "ALTER TABLE a DROP COLUMN w");
    assert_eq!(
        rows(&mut db, "SELECT * FROM a ORDER BY x"),
        vec![vec![i(1)], vec![i(2)]]
    );
    ok(&mut db, "DROP TABLE a");
    assert!(matches!(db.execute("DROP TABLE a"), Err(Error::Schema(_))));
    ok(&mut db, "DROP TABLE IF EXISTS a");
    assert_eq!(rows(&mut db, "SHOW TABLES"), Vec::<Vec<Value>>::new());
}

#[test]
fn select_where_order_group_having_distinct_limit() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT name FROM emp WHERE salary > 75 AND dept = 'eng' ORDER BY salary DESC"
        ),
        vec![vec![t("Ada")], vec![t("Bob")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT name FROM emp ORDER BY dept, name LIMIT 2 OFFSET 1"
        ),
        vec![vec![t("Ada")], vec![t("Bob")]]
    );
    assert_eq!(
        rows(&mut db, "SELECT dept, COUNT(*), SUM(salary), AVG(salary), MIN(name), MAX(salary) FROM emp GROUP BY dept ORDER BY dept"),
        vec![
            vec![N, i(1), i(70), Value::Float(70.0), t("Ed"), i(70)],
            vec![t("eng"), i(2), i(210), Value::Float(105.0), t("Ada"), i(120)],
            vec![t("ops"), i(2), i(80), Value::Float(80.0), t("Cy"), i(80)],
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT dept FROM emp GROUP BY dept HAVING COUNT(*) > 1 ORDER BY 1"
        ),
        vec![vec![t("eng")], vec![t("ops")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT DISTINCT dept FROM emp WHERE dept IS NOT NULL ORDER BY dept"
        ),
        vec![vec![t("eng")], vec![t("ops")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(DISTINCT dept), COUNT(dept), COUNT(*) FROM emp"
        ),
        vec![vec![i(2), i(4), i(5)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(*), SUM(salary) FROM emp WHERE id > 100"
        ),
        vec![vec![i(0), N]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT dept, COUNT(*) FROM emp WHERE id > 100 GROUP BY dept"
        ),
        Vec::<Vec<Value>>::new()
    );
    assert_eq!(
        rows(&mut db, "SELECT name AS n FROM emp ORDER BY n DESC LIMIT 1"),
        vec![vec![t("Ed")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT 1 + 2 * 3, 7 / 2, 7 % 3, 7.0 / 2, -(-3), 'a' || 'b'"
        ),
        vec![vec![i(7), i(3), i(1), Value::Float(3.5), i(3), t("ab")]]
    );
}

#[test]
fn joins_inner_left_right_full_cross_self() {
    let mut db = setup();
    ok(
        &mut db,
        "CREATE TABLE dept (code TEXT PRIMARY KEY, floor INT)",
    );
    ok(&mut db, "INSERT INTO dept VALUES ('eng', 3), ('hr', 1)");
    assert_eq!(
        rows(
            &mut db,
            "SELECT e.name, d.floor FROM emp e JOIN dept d ON d.code = e.dept ORDER BY e.id"
        ),
        vec![vec![t("Ada"), i(3)], vec![t("Bob"), i(3)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(*) FROM emp e LEFT JOIN dept d ON d.code = e.dept WHERE d.code IS NULL"
        ),
        vec![vec![i(3)]]
    );
    assert_eq!(rows(&mut db, "SELECT d.code, e.name FROM emp e RIGHT JOIN dept d ON d.code = e.dept ORDER BY d.code, e.name"), vec![vec![t("eng"), t("Ada")], vec![t("eng"), t("Bob")], vec![t("hr"), N]]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(*) FROM emp e FULL JOIN dept d ON d.code = e.dept"
        ),
        vec![vec![i(6)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM emp CROSS JOIN dept"),
        vec![vec![i(10)]]
    );
    // self join: employee and boss
    assert_eq!(
        rows(
            &mut db,
            "SELECT e.name, b.name FROM emp e LEFT JOIN emp b ON b.id = e.boss ORDER BY e.id"
        ),
        vec![
            vec![t("Ada"), N],
            vec![t("Bob"), t("Ada")],
            vec![t("Cy"), t("Ada")],
            vec![t("Di"), t("Cy")],
            vec![t("Ed"), t("Cy")]
        ]
    );
    // three-way join with aggregation
    assert_eq!(
        rows(&mut db, "SELECT b.name, COUNT(*) FROM emp e JOIN emp b ON b.id = e.boss JOIN dept d ON d.code = b.dept GROUP BY b.name"),
        vec![vec![t("Ada"), i(2)]]
    );
}

#[test]
fn union_subqueries_and_nesting() {
    let mut db = setup();
    assert_eq!(
        rows(
            &mut db,
            "SELECT dept FROM emp WHERE id < 3 UNION SELECT dept FROM emp WHERE id = 2 ORDER BY 1"
        ),
        vec![vec![t("eng")]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT dept FROM emp WHERE id < 3 UNION ALL SELECT dept FROM emp WHERE id = 2"
        )
        .len(),
        3
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT name FROM emp WHERE salary = (SELECT MAX(salary) FROM emp)"
        ),
        vec![vec![t("Ada")]]
    );
    assert_eq!(rows(&mut db, "SELECT name FROM emp e WHERE salary > (SELECT AVG(salary) FROM emp x WHERE x.dept = e.dept) ORDER BY name"), vec![vec![t("Ada")]]);
    assert_eq!(rows(&mut db, "SELECT name FROM emp e WHERE EXISTS (SELECT 1 FROM emp r WHERE r.boss = e.id) ORDER BY id"), vec![vec![t("Ada")], vec![t("Cy")]]);
    assert_eq!(rows(&mut db, "SELECT name FROM emp e WHERE NOT EXISTS (SELECT 1 FROM emp r WHERE r.boss = e.id) ORDER BY id").len(), 3);
    assert_eq!(rows(&mut db, "SELECT name FROM emp WHERE boss IN (SELECT id FROM emp WHERE dept = 'ops') ORDER BY id"), vec![vec![t("Di")], vec![t("Ed")]]);
    // doubly nested and correlated two levels out
    assert_eq!(
        rows(&mut db, "SELECT name FROM emp a WHERE EXISTS (SELECT 1 FROM emp b WHERE b.boss = a.id AND EXISTS (SELECT 1 FROM emp c WHERE c.boss = b.id AND c.dept IS NULL AND a.id = 1))"),
        vec![vec![t("Ada")]]
    );
    assert_eq!(rows(&mut db, "SELECT x.d, x.n FROM (SELECT dept AS d, COUNT(*) AS n FROM emp GROUP BY dept) AS x WHERE x.n > 1 ORDER BY x.d"), vec![vec![t("eng"), i(2)], vec![t("ops"), i(2)]]);
    assert_eq!(
        rows(&mut db, "SELECT (SELECT COUNT(*) FROM emp) * 2"),
        vec![vec![i(10)]]
    );
    assert_eq!(rows(&mut db, "SELECT CASE WHEN salary >= 100 THEN 'high' WHEN salary IS NULL THEN 'none' ELSE 'low' END FROM emp ORDER BY id"), vec![vec![t("high")], vec![t("low")], vec![t("low")], vec![t("none")], vec![t("low")]]);
    assert_eq!(
        rows(&mut db, "SELECT ((((1 + 2) * (3 + 4)) - ((5))) / 2)"),
        vec![vec![i(8)]]
    );
}

#[test]
fn null_semantics() {
    let mut db = setup();
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM emp WHERE salary = NULL"),
        vec![vec![i(0)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM emp WHERE salary <> 90"),
        vec![vec![i(3)]],
        "NULL <> 90 is unknown"
    );
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM emp WHERE NOT (salary > 85)"),
        vec![vec![i(2)]]
    );
    assert_eq!(rows(&mut db, "SELECT NULL AND FALSE, NULL OR TRUE, NULL AND TRUE, NULL + 1, NULL || 'x', COALESCE(NULL, NULL, 3), NULLIF(2, 2)"), vec![vec![Value::Bool(false), Value::Bool(true), N, N, N, i(3), N]]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT 1 IN (2, NULL), 2 IN (2, NULL), 1 NOT IN (2, NULL)"
        ),
        vec![vec![N, Value::Bool(true), N]]
    );
    // NULLs sort first ascending, group together, are skipped by aggregates
    assert_eq!(
        rows(&mut db, "SELECT salary FROM emp ORDER BY salary LIMIT 1"),
        vec![vec![N]]
    );
    assert_eq!(
        rows(&mut db, "SELECT AVG(salary) FROM emp"),
        vec![vec![Value::Float(90.0)]]
    );
}

#[test]
fn types_dates_json_functions() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE ev (id INT PRIMARY KEY, at TIMESTAMP, d DATE, t TIME, meta JSON, ok BOOL, r REAL)");
    ok(&mut db, "INSERT INTO ev VALUES (1, '2024-03-01 08:30:00', '2024-03-01', '08:30', '{\"tags\": [\"a\", \"b\"], \"n\": 2}', true, 1.5)");
    ok(&mut db, "INSERT INTO ev VALUES (2, '2023-12-31T23:59:59', '2023-12-31', '23:59:59.5', '{\"n\": 5}', false, 2)");
    assert_eq!(
        rows(
            &mut db,
            "SELECT id FROM ev WHERE at > TIMESTAMP '2024-01-01 00:00:00'"
        ),
        vec![vec![i(1)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT t FROM ev WHERE id = 2"),
        vec![vec![t("23:59:59.5")]]
    );
    assert_eq!(
        rows(&mut db, "SELECT meta.n, meta.tags.1 FROM ev WHERE id = 1"),
        vec![vec![i(2), t("b")]]
    );
    assert_eq!(
        rows(&mut db, "SELECT id FROM ev WHERE meta.n > 3"),
        vec![vec![i(2)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT r FROM ev WHERE id = 2"),
        vec![vec![Value::Float(2.0)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT YEAR(at), MONTH(d), MINUTE(t), DAY(at) FROM ev WHERE id = 1"
        ),
        vec![vec![i(2024), i(3), i(30), i(1)]]
    );
    assert_eq!(rows(&mut db, "SELECT UPPER('ab'), LOWER('AB'), LENGTH('héllo'), TRIM('  x '), SUBSTR('abcdef', 2, 3), ROUND(2.567, 2), ABS(-3), CONCAT('a', 1, NULL, 'b')"), vec![vec![t("AB"), t("ab"), i(5), t("x"), t("bcd"), Value::Float(2.57), i(3), t("a1b")]]);
    assert_eq!(
        rows(
            &mut db,
            "SELECT CAST('42' AS INT), CAST(3.9 AS INT), CAST(1 AS TEXT), CAST('true' AS BOOL)"
        ),
        vec![vec![i(42), i(3), t("1"), Value::Bool(true)]]
    );
    assert!(matches!(
        db.execute("INSERT INTO ev (id, d) VALUES (3, '2024-13-01')"),
        Err(Error::Type(_))
    ));
    assert!(matches!(
        db.execute("INSERT INTO ev (id, ok) VALUES (3, 'yes')"),
        Err(Error::Type(_))
    ));
    assert!(matches!(db.execute("SELECT 1 / 0"), Err(Error::Type(_))));
    assert!(matches!(
        db.execute("SELECT 9223372036854775807 + 1"),
        Err(Error::Type(_))
    ));
    assert!(rows(&mut db, "SELECT NOW()")[0][0].display().len() == 19);
}

#[test]
fn update_delete_and_transactions() {
    let mut db = setup();
    assert_eq!(
        ok(
            &mut db,
            "UPDATE emp SET salary = salary + 10 WHERE dept = 'eng'"
        ),
        Output::Done("2 row(s) updated".into())
    );
    assert_eq!(
        scalar(&mut db, "SELECT SUM(salary) FROM emp WHERE dept = 'eng'"),
        i(230)
    );
    // updating the primary key of every row in one statement (Halloween-safe)
    ok(&mut db, "UPDATE emp SET id = id + 100");
    assert_eq!(
        rows(&mut db, "SELECT MIN(id), MAX(id), COUNT(*) FROM emp"),
        vec![vec![i(101), i(105), i(5)]]
    );
    assert_eq!(
        ok(&mut db, "DELETE FROM emp WHERE salary IS NULL"),
        Output::Done("1 row(s) deleted".into())
    );
    ok(&mut db, "BEGIN");
    ok(&mut db, "DELETE FROM emp");
    assert_eq!(scalar(&mut db, "SELECT COUNT(*) FROM emp"), i(0));
    ok(&mut db, "ROLLBACK");
    assert_eq!(scalar(&mut db, "SELECT COUNT(*) FROM emp"), i(4));
    ok(&mut db, "BEGIN");
    ok(
        &mut db,
        "INSERT INTO emp VALUES (200, 'New', 'hr', 1, NULL)",
    );
    assert!(db
        .execute("INSERT INTO emp VALUES (200, 'Dup', 'hr', 1, NULL)")
        .is_err());
    ok(&mut db, "COMMIT");
    assert_eq!(
        scalar(&mut db, "SELECT name FROM emp WHERE id = 200"),
        t("New"),
        "a failed statement does not abort the transaction"
    );
    assert!(matches!(
        db.execute("BEGIN").and_then(|_| db.execute("BEGIN")),
        Err(Error::Txn(_))
    ));
    ok(&mut db, "ROLLBACK");
}

#[test]
fn malformed_queries_give_typed_errors() {
    let mut db = setup();
    for (q, parse) in [
        ("SELEC name FROM emp", true),
        ("SELECT name FROM", true),
        ("SELECT name FROM emp WHERE", true),
        ("SELECT name, FROM emp", true),
        ("SELECT (1", true),
        ("INSERT INTO emp VALUES (1,", true),
        ("SELECT 'unterminated", true),
        ("SELECT name FROM emp ORDER", true),
        ("SELECT * FROM nope", false),
        ("SELECT nope FROM emp", false),
        ("SELECT e.nope FROM emp e", false),
        ("SELECT name FROM emp a JOIN emp b ON a.id = b.id", false),
    ] {
        match db.execute(q) {
            Err(Error::Parse(_)) if parse => {}
            Err(Error::Schema(_)) if !parse => {}
            other => panic!("{q}: unexpected {other:?}"),
        }
    }
    assert!(matches!(
        db.execute("SELECT name FROM emp GROUP BY dept HAVING"),
        Err(Error::Parse(_))
    ));
    assert!(matches!(
        db.execute("SELECT SUM(name) FROM emp"),
        Err(Error::Type(_))
    ));
}

// ------------------------------------------------------------ constraints

#[test]
fn primary_unique_default_not_null_check_interactions() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE acct (org INT, num INT, email TEXT UNIQUE, status TEXT NOT NULL DEFAULT 'open', balance INT DEFAULT 0 CHECK (balance >= 0), PRIMARY KEY (org, num), UNIQUE (org, email))");
    ok(
        &mut db,
        "INSERT INTO acct (org, num, email) VALUES (1, 1, 'a@x'), (1, 2, 'b@x'), (2, 1, NULL)",
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT status, balance FROM acct WHERE org = 1 AND num = 1"
        ),
        vec![vec![t("open"), i(0)]]
    );
    let constraint = |db: &mut Database, q: &str| {
        assert!(
            matches!(db.execute(q), Err(Error::Constraint(_))),
            "{q} should violate a constraint"
        )
    };
    constraint(&mut db, "INSERT INTO acct (org, num) VALUES (1, 1)"); // composite PK
    constraint(
        &mut db,
        "INSERT INTO acct (org, num, email) VALUES (3, 3, 'a@x')",
    ); // UNIQUE email
    constraint(
        &mut db,
        "INSERT INTO acct (org, num, status) VALUES (3, 3, NULL)",
    ); // NOT NULL beats DEFAULT
    constraint(
        &mut db,
        "INSERT INTO acct (org, num, balance) VALUES (3, 3, -1)",
    ); // CHECK
    constraint(
        &mut db,
        "UPDATE acct SET balance = balance - 1 WHERE org = 1",
    ); // CHECK on update, all or nothing
    constraint(&mut db, "UPDATE acct SET num = 2 WHERE org = 1 AND num = 1"); // PK collision on update
    constraint(&mut db, "INSERT INTO acct (num, email) VALUES (9, 'z@x')"); // NULL in PK
    ok(
        &mut db,
        "INSERT INTO acct (org, num, email) VALUES (3, 3, NULL)",
    ); // NULL emails never collide
    assert_eq!(scalar(&mut db, "SELECT SUM(balance) FROM acct"), i(0));
    // constraint failures roll back with the transaction
    ok(&mut db, "BEGIN");
    ok(
        &mut db,
        "UPDATE acct SET email = 'moved@x' WHERE org = 1 AND num = 1",
    );
    ok(
        &mut db,
        "INSERT INTO acct (org, num, email) VALUES (5, 5, 'a@x')",
    ); // old value free inside the txn
    ok(&mut db, "ROLLBACK");
    constraint(
        &mut db,
        "INSERT INTO acct (org, num, email) VALUES (5, 5, 'a@x')",
    );
}

#[test]
fn foreign_key_scenarios() {
    let mut db = Database::in_memory();
    ok(
        &mut db,
        "CREATE TABLE parent (id INT PRIMARY KEY, code TEXT UNIQUE)",
    );
    ok(&mut db, "CREATE TABLE child (id INT PRIMARY KEY, pid INT REFERENCES parent(id) ON DELETE CASCADE, pcode TEXT REFERENCES parent(code) ON DELETE SET NULL)");
    ok(
        &mut db,
        "CREATE TABLE grandchild (id INT PRIMARY KEY, cid INT REFERENCES child(id))",
    );
    ok(&mut db, "INSERT INTO parent VALUES (1, 'a'), (2, 'b')");
    ok(
        &mut db,
        "INSERT INTO child VALUES (10, 1, 'b'), (11, 2, 'a')",
    );
    ok(&mut db, "INSERT INTO grandchild VALUES (100, 11)");
    let constraint = |db: &mut Database, q: &str| {
        assert!(matches!(db.execute(q), Err(Error::Constraint(_))), "{q}")
    };
    constraint(&mut db, "INSERT INTO child VALUES (12, 3, NULL)"); // missing target
    constraint(&mut db, "INSERT INTO child VALUES (12, 1, 'zz')");
    constraint(&mut db, "UPDATE parent SET id = 9 WHERE id = 1"); // referenced key change is RESTRICT
    constraint(&mut db, "DELETE FROM parent WHERE id = 2"); // cascade reaches child 11, restricted by grandchild
    assert_eq!(
        scalar(&mut db, "SELECT COUNT(*) FROM child"),
        i(2),
        "the failed cascade left nothing half-done"
    );
    // delete parent 1: child 10 cascades away, child 11's pcode 'a' is set to NULL
    ok(&mut db, "DELETE FROM parent WHERE id = 1");
    assert_eq!(
        rows(&mut db, "SELECT id, pid, pcode FROM child ORDER BY id"),
        vec![vec![i(11), i(2), N]]
    );
    ok(&mut db, "UPDATE parent SET id = 9 WHERE id = 2 AND 1 = 0"); // no-op update is fine
    ok(&mut db, "INSERT INTO child VALUES (12, NULL, NULL)"); // NULL references nothing
                                                              // invariant: every non-null child.pid exists in parent
    assert_eq!(scalar(&mut db, "SELECT COUNT(*) FROM child c WHERE c.pid IS NOT NULL AND NOT EXISTS (SELECT 1 FROM parent p WHERE p.id = c.pid)"), i(0));
}

#[test]
fn constraint_violations_inside_transactions_roll_back_cleanly() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE p (id INT PRIMARY KEY)");
    ok(
        &mut db,
        "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id))",
    );
    ok(&mut db, "BEGIN");
    ok(&mut db, "INSERT INTO p VALUES (1)");
    ok(&mut db, "INSERT INTO c VALUES (1, 1)"); // sees the uncommitted parent
    ok(&mut db, "ROLLBACK");
    assert!(
        db.execute("INSERT INTO c VALUES (1, 1)").is_err(),
        "the parent was rolled back"
    );
    assert_eq!(scalar(&mut db, "SELECT COUNT(*) FROM c"), i(0));
}

#[test]
fn persistence_of_schema_constraints_and_data() {
    let dir = tmp("rel-persist");
    {
        let mut db = Database::open(&dir).unwrap();
        load_business(&mut db, 300, seed());
        ok(
            &mut db,
            "CREATE INDEX cust_city ON customers (city, country)",
        );
        db.checkpoint().unwrap();
        ok(&mut db, "UPDATE customers SET city = 'Oslo' WHERE id < 10");
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        scalar(
            &mut db,
            "SELECT COUNT(*) FROM customers WHERE city = 'Oslo' AND id < 10"
        ),
        i(10)
    );
    assert!(plan(&mut db, "SELECT id FROM customers WHERE city = 'Oslo'").contains("cust_city"));
    assert!(
        matches!(
            db.execute("INSERT INTO products VALUES (99999, 'x', 'y', -1)"),
            Err(Error::Constraint(_))
        ),
        "CHECK survives restart"
    );
    assert!(
        matches!(
            db.execute("INSERT INTO departments VALUES (99, 'eng', 1)"),
            Err(Error::Constraint(_))
        ),
        "UNIQUE survives restart"
    );
    assert!(
        matches!(
            db.execute("INSERT INTO transactions VALUES (999999, 999999, 1, 1, NULL)"),
            Err(Error::Constraint(_))
        ),
        "FK survives restart"
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}
