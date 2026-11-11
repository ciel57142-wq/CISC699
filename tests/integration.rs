use polydb::{Database, Error, FindOptions, Output, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn docs(db: &mut Database, cmd: &str) -> Vec<Value> {
    match db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")) {
        Output::Docs(d) => d,
        other => panic!("expected docs from {cmd}, got {other:?}"),
    }
}

fn scalar(db: &mut Database, cmd: &str) -> Value {
    match db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")) {
        Output::Scalar(v) => v,
        other => panic!("expected scalar from {cmd}, got {other:?}"),
    }
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

fn seed_sql(db: &mut Database) {
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL, age INT, city TEXT)").unwrap();
    db.execute(
        "INSERT INTO users VALUES (1,'Ada',36,'London'), (2,'Linus',54,'Portland'), (3,'Grace',85,'NYC'), (4,'Alan',41,'London')",
    )
    .unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total REAL)").unwrap();
    db.execute("INSERT INTO orders VALUES (10,1,9.5), (11,1,20), (12,2,5.25), (13,9,1.0)").unwrap();
}

// ------------------------------------------------------------- relational

#[test]
fn sql_select_filter_order_limit() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    let r = rows(&mut db, "SELECT name FROM users WHERE age > 40 ORDER BY age DESC LIMIT 2");
    assert_eq!(r, vec![vec![t("Grace")], vec![t("Linus")]]);
    let r = rows(&mut db, "SELECT name FROM users WHERE city = 'London' AND name LIKE 'A%' ORDER BY name");
    assert_eq!(r, vec![vec![t("Ada")], vec![t("Alan")]]);
    let r = rows(&mut db, "SELECT id FROM users WHERE age BETWEEN 40 AND 60 ORDER BY 1 OFFSET 1");
    assert_eq!(r, vec![vec![Value::Int(4)]]);
    let r = rows(&mut db, "SELECT id FROM users WHERE id IN (1, 3) OR name = 'Alan' ORDER BY id DESC");
    assert_eq!(r.len(), 3);
    assert_eq!(rows(&mut db, "SELECT 1 + 2 * 3, upper('a') || 'b'"), vec![vec![Value::Int(7), t("Ab")]]);
}

#[test]
fn sql_primary_key_lookup_and_star() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    let r = rows(&mut db, "SELECT * FROM users WHERE id = 3");
    assert_eq!(r, vec![vec![Value::Int(3), t("Grace"), Value::Int(85), t("NYC")]]);
    assert!(rows(&mut db, "SELECT * FROM users WHERE id = 99").is_empty());
    assert!(rows(&mut db, "SELECT * FROM users WHERE id = 3 AND age < 10").is_empty());
}

#[test]
fn sql_joins() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    let r = rows(
        &mut db,
        "SELECT u.name, o.total FROM users u JOIN orders o ON o.user_id = u.id ORDER BY o.total",
    );
    assert_eq!(
        r,
        vec![vec![t("Linus"), Value::Float(5.25)], vec![t("Ada"), Value::Float(9.5)], vec![t("Ada"), Value::Float(20.0)]]
    );
    let r = rows(
        &mut db,
        "SELECT u.name, o.id FROM users u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id IS NULL ORDER BY u.name",
    );
    assert_eq!(r, vec![vec![t("Alan"), Value::Null], vec![t("Grace"), Value::Null]]);
}

#[test]
fn sql_aggregates_and_grouping() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    let r = rows(&mut db, "SELECT COUNT(*), MIN(age), MAX(age), SUM(age), AVG(age) FROM users");
    assert_eq!(r, vec![vec![Value::Int(4), Value::Int(36), Value::Int(85), Value::Int(216), Value::Float(54.0)]]);
    let r = rows(&mut db, "SELECT city, COUNT(*) AS n FROM users GROUP BY city HAVING COUNT(*) > 1 ORDER BY n DESC");
    assert_eq!(r, vec![vec![t("London"), Value::Int(2)]]);
    let r = rows(
        &mut db,
        "SELECT u.name, SUM(o.total) AS spent FROM users u JOIN orders o ON o.user_id = u.id GROUP BY u.name ORDER BY spent DESC",
    );
    assert_eq!(r, vec![vec![t("Ada"), Value::Float(29.5)], vec![t("Linus"), Value::Float(5.25)]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM users WHERE age > 1000"), vec![vec![Value::Int(0)]]);
    assert_eq!(rows(&mut db, "SELECT DISTINCT city FROM users ORDER BY city").len(), 3);
}

#[test]
fn sql_update_delete() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    db.execute("UPDATE users SET age = age + 1, city = 'Paris' WHERE id = 1").unwrap();
    assert_eq!(rows(&mut db, "SELECT age, city FROM users WHERE id = 1"), vec![vec![Value::Int(37), t("Paris")]]);
    db.execute("UPDATE users SET id = 100 WHERE id = 2").unwrap();
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE id = 100"), vec![vec![t("Linus")]]);
    assert!(rows(&mut db, "SELECT name FROM users WHERE id = 2").is_empty());
    db.execute("DELETE FROM users WHERE age > 80").unwrap();
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM users"), vec![vec![Value::Int(3)]]);
    db.execute("DELETE FROM users").unwrap();
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM users"), vec![vec![Value::Int(0)]]);
}

#[test]
fn sql_constraints_and_types() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    assert!(matches!(db.execute("INSERT INTO users VALUES (1,'Dup',1,'x')"), Err(Error::Constraint(_))));
    assert!(matches!(db.execute("INSERT INTO users (id) VALUES (50)"), Err(Error::Constraint(_))));
    assert!(matches!(db.execute("INSERT INTO users VALUES (51,'x','old','y')"), Err(Error::Type(_))));
    assert!(matches!(db.execute("UPDATE users SET id = 1 WHERE id = 2"), Err(Error::Constraint(_))));
    assert!(matches!(db.execute("SELECT nope FROM users"), Err(Error::Schema(_))));
    assert!(matches!(db.execute("SELECT * FROM missing"), Err(Error::Schema(_))));
    assert!(matches!(db.execute("SELECT FROM"), Err(Error::Parse(_))));
    // a failed multi-row insert leaves nothing behind
    assert!(db.execute("INSERT INTO users VALUES (60,'ok',1,'x'), (1,'dup',1,'x')").is_err());
    assert!(rows(&mut db, "SELECT * FROM users WHERE id = 60").is_empty());
    // NULL handling
    db.execute("INSERT INTO users (id, name) VALUES (70, 'NoAge')").unwrap();
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE age IS NULL"), vec![vec![t("NoAge")]]);
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM users WHERE age > 0 OR age <= 0"), vec![vec![Value::Int(4)]]);
}

#[test]
fn sql_table_without_primary_key() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE log (msg TEXT, ok BOOL)").unwrap();
    db.execute("INSERT INTO log VALUES ('a', true), ('a', true), ('b', false)").unwrap();
    assert_eq!(rows(&mut db, "SELECT msg FROM log WHERE ok").len(), 2);
    db.execute("DROP TABLE log").unwrap();
    assert!(db.execute("SELECT * FROM log").is_err());
    db.execute("DROP TABLE IF EXISTS log").unwrap();
}

// --------------------------------------------------------------- document

#[test]
fn doc_insert_find_filters() {
    let mut db = Database::in_memory();
    db.execute(r#"db.people.insert({_id: 1, name: "Ada", age: 36, tags: ["math", "code"], addr: {city: "London"}})"#)
        .unwrap();
    db.execute(r#"db.people.insertMany([{_id: 2, name: "Linus", age: 54, addr: {city: "Portland"}}, {_id: 3, name: "Grace", age: 85}])"#)
        .unwrap();
    assert_eq!(docs(&mut db, "db.people.find()").len(), 3);
    assert_eq!(docs(&mut db, "db.people.find({_id: 2})")[0].get("name"), Some(&t("Linus")));
    assert_eq!(docs(&mut db, "db.people.find({age: {$gt: 40, $lt: 80}})").len(), 1);
    assert_eq!(docs(&mut db, r#"db.people.find({"addr.city": "London"})"#).len(), 1);
    assert_eq!(docs(&mut db, "db.people.find({tags: 'code'})").len(), 1);
    assert_eq!(docs(&mut db, "db.people.find({$or: [{age: 36}, {name: 'Grace'}]})").len(), 2);
    assert_eq!(docs(&mut db, "db.people.find({addr: {$exists: false}})").len(), 1);
    assert_eq!(docs(&mut db, "db.people.find({name: {$in: ['Ada', 'Linus', 'Nobody']}})").len(), 2);
    assert_eq!(docs(&mut db, "db.people.find({age: {$ne: 36}})").len(), 2);
    let sorted = docs(&mut db, "db.people.find({}).sort({age: -1}).skip(1).limit(1)");
    assert_eq!(sorted[0].get("name"), Some(&t("Linus")));
    assert_eq!(scalar(&mut db, "db.people.count({age: {$gte: 54}})"), Value::Int(2));
    assert!(matches!(db.execute("db.people.insert({_id: 1})"), Err(Error::Constraint(_))));
}

#[test]
fn doc_update_delete_generated_ids() {
    let mut db = Database::in_memory();
    let id = scalar(&mut db, "db.notes.insert({text: 'hello'})");
    assert!(matches!(id, Value::Text(_)));
    let id2 = scalar(&mut db, "db.notes.insert({text: 'world'})");
    assert_ne!(id, id2);
    db.execute("db.notes.update({text: 'hello'}, {$set: {'meta.pinned': true}, $inc: {views: 2}, $push: {tags: 'x'}})")
        .unwrap();
    let d = &docs(&mut db, "db.notes.find({text: 'hello'})")[0];
    assert_eq!(d.get("meta").and_then(|m| m.get("pinned")), Some(&Value::Bool(true)));
    assert_eq!(d.get("views"), Some(&Value::Int(2)));
    assert_eq!(d.get("tags"), Some(&Value::Array(vec![t("x")])));
    assert_eq!(d.get("_id"), Some(&id));
    db.execute("db.notes.update({text: 'hello'}, {$unset: {views: 1}})").unwrap();
    assert_eq!(docs(&mut db, "db.notes.find({views: {$exists: true}})").len(), 0);
    // replacement keeps the id
    db.execute("db.notes.update({text: 'world'}, {text: 'replaced'})").unwrap();
    assert_eq!(docs(&mut db, "db.notes.find({text: 'replaced'})")[0].get("_id"), Some(&id2));
    db.execute("db.notes.delete({text: 'replaced'})").unwrap();
    assert_eq!(scalar(&mut db, "db.notes.count()"), Value::Int(1));
    db.execute("db.notes.drop()").unwrap();
    assert_eq!(scalar(&mut db, "db.notes.count()"), Value::Int(0));
}

#[test]
fn doc_typed_api() {
    let mut db = Database::in_memory();
    let doc = Value::parse(r#"{"sku": "A1", "qty": 3, "price": 2.5}"#).unwrap();
    db.doc_insert("stock", doc).unwrap();
    let filter = Value::parse(r#"{"qty": {"$gte": 3}}"#).unwrap();
    let found = db.doc_find("stock", &filter, &FindOptions::default()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].get("price"), Some(&Value::Float(2.5)));
    assert_eq!(db.doc_delete("stock", &filter).unwrap(), 1);
}

// -------------------------------------------------------------- key-value

#[test]
fn kv_commands() {
    let mut db = Database::in_memory();
    db.execute("SET greeting hello world").unwrap();
    assert_eq!(scalar(&mut db, "GET greeting"), t("hello world"));
    db.execute(r#"SET "key with space" "quoted \"value\"""#).unwrap();
    assert_eq!(scalar(&mut db, r#"GET "key with space""#), t("quoted \"value\""));
    assert_eq!(scalar(&mut db, "GET missing"), Value::Null);
    assert_eq!(scalar(&mut db, "EXISTS greeting"), Value::Bool(true));
    assert_eq!(scalar(&mut db, "INCR hits"), Value::Int(1));
    assert_eq!(scalar(&mut db, "INCR hits 10"), Value::Int(11));
    assert!(matches!(db.execute("INCR greeting"), Err(Error::Type(_))));
    db.execute("SET user:1 a").unwrap();
    db.execute("SET user:2 b").unwrap();
    assert_eq!(rows(&mut db, "KEYS user:"), vec![vec![t("user:1")], vec![t("user:2")]]);
    assert_eq!(scalar(&mut db, "DEL user:1"), Value::Int(1));
    assert_eq!(scalar(&mut db, "DEL user:1"), Value::Int(0));
    // binary-safe through the typed API
    db.kv_set("blob", &[0, 159, 255, 0]).unwrap();
    assert_eq!(db.kv_get("blob").unwrap(), Some(vec![0, 159, 255, 0]));
}

// ------------------------------------------------- cross-model behaviour

#[test]
fn models_are_isolated_from_each_other() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE things (id INT PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO things VALUES (1)").unwrap();
    db.execute("db.things.insert({_id: 1, kind: 'doc'})").unwrap();
    db.execute("SET things kv").unwrap();
    assert_eq!(rows(&mut db, "SELECT * FROM things").len(), 1);
    assert_eq!(docs(&mut db, "db.things.find()").len(), 1);
    assert_eq!(rows(&mut db, "KEYS").len(), 1);
    db.execute("DROP TABLE things").unwrap();
    assert_eq!(docs(&mut db, "db.things.find()").len(), 1);
    assert_eq!(scalar(&mut db, "GET things"), t("kv"));
    assert_eq!(rows(&mut db, "SHOW TABLES").len(), 0);
    assert_eq!(rows(&mut db, "SHOW COLLECTIONS"), vec![vec![t("things")]]);
}

#[test]
fn transaction_spans_all_models_and_rolls_back() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
    db.execute("db.audit.insert({account: 1, delta: -30})").unwrap();
    db.execute("SET last_txn withdrawal").unwrap();
    // the transaction sees its own writes
    assert_eq!(rows(&mut db, "SELECT balance FROM accounts WHERE id = 1"), vec![vec![Value::Int(70)]]);
    assert_eq!(scalar(&mut db, "db.audit.count()"), Value::Int(1));
    // a failing statement inside the transaction undoes only itself
    assert!(db.execute("INSERT INTO accounts VALUES (2, 5), (1, 0)").is_err());
    assert!(rows(&mut db, "SELECT * FROM accounts WHERE id = 2").is_empty());
    db.execute("ROLLBACK").unwrap();

    assert_eq!(rows(&mut db, "SELECT balance FROM accounts WHERE id = 1"), vec![vec![Value::Int(100)]]);
    assert_eq!(scalar(&mut db, "db.audit.count()"), Value::Int(0));
    assert_eq!(scalar(&mut db, "GET last_txn"), Value::Null);

    db.execute("BEGIN").unwrap();
    db.execute("UPDATE accounts SET balance = balance - 30 WHERE id = 1").unwrap();
    db.execute("db.audit.insert({account: 1, delta: -30})").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(rows(&mut db, "SELECT balance FROM accounts"), vec![vec![Value::Int(70)]]);
    assert_eq!(scalar(&mut db, "db.audit.count()"), Value::Int(1));
    assert!(matches!(db.execute("COMMIT"), Err(Error::Txn(_))));
}

// ------------------------------------------------------------- durability

fn seed_all(db: &mut Database) {
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')").unwrap();
    db.execute("db.c.insert({_id: 'a', n: 1.5, nested: {x: [1, 2, 3]}})").unwrap();
    db.execute("SET k v").unwrap();
}

fn check_all(db: &mut Database) {
    assert_eq!(rows(db, "SELECT v FROM t ORDER BY id"), vec![vec![t("one")], vec![t("two")]]);
    let d = docs(db, "db.c.find({_id: 'a'})");
    assert_eq!(d[0].get("n"), Some(&Value::Float(1.5)));
    assert_eq!(scalar(db, "GET k"), t("v"));
}

#[test]
fn data_survives_reopen() {
    let dir = tmp("reopen");
    {
        let mut db = Database::open(&dir).unwrap();
        seed_all(&mut db);
        db.execute("BEGIN").unwrap();
        db.execute("SET uncommitted yes").unwrap();
        // dropped without COMMIT
    }
    let mut db = Database::open(&dir).unwrap();
    check_all(&mut db);
    assert_eq!(scalar(&mut db, "GET uncommitted"), Value::Null);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn checkpoint_then_more_writes_then_reopen() {
    let dir = tmp("checkpoint");
    {
        let mut db = Database::open(&dir).unwrap();
        seed_all(&mut db);
        db.execute("CHECKPOINT").unwrap();
        assert_eq!(fs::metadata(dir.join("data.wal")).unwrap().len(), 0);
        db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
        db.execute("DELETE FROM t WHERE id = 3").unwrap();
        db.execute("SET after checkpoint").unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    check_all(&mut db);
    assert_eq!(scalar(&mut db, "GET after"), t("checkpoint"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn torn_wal_tail_is_discarded() {
    let dir = tmp("torn");
    {
        let mut db = Database::open(&dir).unwrap();
        seed_all(&mut db);
    }
    let wal = dir.join("data.wal");
    let good_len = fs::metadata(&wal).unwrap().len();
    // simulate a crash in the middle of writing a commit record
    let mut f = OpenOptions::new().append(true).open(&wal).unwrap();
    f.write_all(&[200, 0, 0, 0, 1, 2, 3, 4, 9, 9, 9]).unwrap();
    drop(f);
    {
        let mut db = Database::open(&dir).unwrap();
        check_all(&mut db);
        assert_eq!(fs::metadata(&wal).unwrap().len(), good_len);
        db.execute("SET post recovery").unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(scalar(&mut db, "GET post"), t("recovery"));

    // a flipped bit in the last record invalidates only that commit
    drop(db);
    let mut bytes = fs::read(&wal).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&wal, bytes).unwrap();
    let mut db = Database::open(&dir).unwrap();
    check_all(&mut db);
    assert_eq!(scalar(&mut db, "GET post"), Value::Null);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn json_round_trip() {
    let src = r#"{"a":[1,2.5,-3,1e20,"x\ny \"q\" é 😀"],"b":{"c":null,"d":true},"e":1.0}"#;
    let v = Value::parse(src).unwrap();
    assert_eq!(Value::parse(&v.to_json()).unwrap(), v);
    assert_eq!(v.get("e"), Some(&Value::Float(1.0)));
    assert!(Value::parse("{a: 1,").is_err());
    assert!(Value::parse("[1 2] x").is_err());
}

// ---------------------------------------------------------------- indexes

#[test]
fn sql_secondary_index_stays_correct() {
    let mut db = Database::in_memory();
    seed_sql(&mut db);
    db.execute("CREATE INDEX users_city ON users (city)").unwrap();
    let london = "SELECT id FROM users WHERE city = 'London' ORDER BY id";
    assert_eq!(rows(&mut db, london), vec![vec![Value::Int(1)], vec![Value::Int(4)]]);
    db.execute("UPDATE users SET city = 'Paris' WHERE id = 1").unwrap();
    db.execute("INSERT INTO users VALUES (5, 'Edsger', 72, 'London')").unwrap();
    assert_eq!(rows(&mut db, london), vec![vec![Value::Int(4)], vec![Value::Int(5)]]);
    assert_eq!(rows(&mut db, "SELECT id FROM users WHERE city = 'Paris' AND age > 1"), vec![vec![Value::Int(1)]]);
    db.execute("DELETE FROM users WHERE id = 4").unwrap();
    db.execute("UPDATE users SET id = 50 WHERE id = 5").unwrap();
    assert_eq!(rows(&mut db, london), vec![vec![Value::Int(50)]]);
    assert!(rows(&mut db, "SELECT id FROM users WHERE city = 'Nowhere'").is_empty());
    // numeric index: 54 and 54.0 are the same key
    db.execute("CREATE INDEX users_age ON users (age)").unwrap();
    assert_eq!(rows(&mut db, "SELECT name FROM users WHERE age = 54.0"), vec![vec![t("Linus")]]);
    // index entries really exist and really go away
    let before = db.entry_count();
    db.execute("DROP INDEX users_city ON users").unwrap();
    assert!(db.entry_count() < before);
    assert_eq!(rows(&mut db, london), vec![vec![Value::Int(50)]]);
    db.execute("DROP TABLE users").unwrap();
    db.execute("DROP TABLE orders").unwrap();
    assert_eq!(db.entry_count(), 0);
}

#[test]
fn doc_index_stays_correct() {
    let mut db = Database::in_memory();
    db.execute("db.p.insert([{_id: 1, city: 'London', tags: ['a', 'b']}, {_id: 2, city: 'NYC', tags: ['b']}, {_id: 3}])").unwrap();
    db.execute("db.p.createIndex({city: 1})").unwrap();
    db.execute("db.p.createIndex({tags: 1})").unwrap();
    assert_eq!(docs(&mut db, "db.p.find({city: 'London'})").len(), 1);
    assert_eq!(docs(&mut db, "db.p.find({tags: 'b'})").len(), 2);
    assert_eq!(docs(&mut db, "db.p.find({city: null})").len(), 1);
    db.execute("db.p.update({_id: 2}, {$set: {city: 'London'}, $pull: {tags: 'b'}})").unwrap();
    db.execute("db.p.insert({_id: 4, city: 'London'})").unwrap();
    assert_eq!(docs(&mut db, "db.p.find({city: 'London'})").len(), 3);
    assert_eq!(docs(&mut db, "db.p.find({city: 'NYC'})").len(), 0);
    assert_eq!(docs(&mut db, "db.p.find({tags: 'b'})").len(), 1);
    db.execute("db.p.deleteOne({city: 'London'})").unwrap();
    assert_eq!(docs(&mut db, "db.p.find({city: {$eq: 'London'}})").len(), 2);
    db.execute("db.p.drop()").unwrap();
    assert_eq!(db.entry_count(), 0);
}

// ------------------------------------------------------------ concurrency

#[test]
fn concurrent_sessions_lose_no_updates() {
    let dir = tmp("concurrent");
    let db = Database::open(&dir).unwrap();
    {
        let mut s = db.session();
        s.set_sync(false); // keep the test fast; correctness does not depend on it
        s.execute("CREATE TABLE acct (id INT PRIMARY KEY, bal INT)").unwrap();
        s.execute("INSERT INTO acct VALUES (1, 0)").unwrap();
    }
    let mut handles = Vec::new();
    for _ in 0..8 {
        let mut s = db.session();
        handles.push(std::thread::spawn(move || {
            let mut retries = 0;
            for i in 0..100 {
                // single statements are atomic
                s.execute("INCR hits").unwrap();
                s.execute("UPDATE acct SET bal = bal + 1 WHERE id = 1").unwrap();
                // multi-statement read-modify-write must retry on conflict
                loop {
                    s.execute("BEGIN").unwrap();
                    let cur = match s.execute("GET txn_counter").unwrap() {
                        Output::Scalar(Value::Text(v)) => v.parse::<i64>().unwrap(),
                        _ => 0,
                    };
                    if i % 10 == 0 {
                        std::thread::yield_now();
                    }
                    s.execute(&format!("SET txn_counter {}", cur + 1)).unwrap();
                    match s.execute("COMMIT") {
                        Ok(_) => break,
                        Err(Error::Conflict(_)) => retries += 1,
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            retries
        }));
    }
    let _retries: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let mut s = db.session();
    assert_eq!(scalar(&mut s, "GET hits"), t("800"));
    assert_eq!(rows(&mut s, "SELECT bal FROM acct"), vec![vec![Value::Int(800)]]);
    assert_eq!(scalar(&mut s, "GET txn_counter"), t("800"));
    drop((s, db));
    // and all of it is on disk
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(scalar(&mut db, "GET txn_counter"), t("800"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn conflicting_transaction_is_rejected() {
    let db = Database::in_memory();
    let (mut a, mut b) = (db.session(), db.session());
    a.execute("CREATE TABLE stock (id INT PRIMARY KEY, qty INT)").unwrap();
    a.execute("INSERT INTO stock VALUES (1, 1)").unwrap();

    // both sessions see qty = 1 and both try to sell the last item
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    assert_eq!(rows(&mut a, "SELECT qty FROM stock WHERE id = 1"), vec![vec![Value::Int(1)]]);
    assert_eq!(rows(&mut b, "SELECT qty FROM stock WHERE id = 1"), vec![vec![Value::Int(1)]]);
    a.execute("UPDATE stock SET qty = qty - 1 WHERE id = 1").unwrap();
    b.execute("UPDATE stock SET qty = qty - 1 WHERE id = 1").unwrap();
    a.execute("COMMIT").unwrap();
    assert!(matches!(b.execute("COMMIT"), Err(Error::Conflict(_))));
    assert_eq!(rows(&mut a, "SELECT qty FROM stock"), vec![vec![Value::Int(0)]]);

    // a scan conflicts with a row inserted into the scanned table (no phantoms)
    a.execute("BEGIN").unwrap();
    assert_eq!(rows(&mut a, "SELECT COUNT(*) FROM stock"), vec![vec![Value::Int(1)]]);
    b.execute("INSERT INTO stock VALUES (2, 5)").unwrap();
    a.execute("SET stock_rows 1").unwrap();
    assert!(matches!(a.execute("COMMIT"), Err(Error::Conflict(_))));
    assert_eq!(scalar(&mut a, "GET stock_rows"), Value::Null);

    // transactions touching different data do not disturb each other
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    a.execute("INCR x").unwrap();
    b.execute("INCR y").unwrap();
    a.execute("COMMIT").unwrap();
    b.execute("COMMIT").unwrap();
    // uncommitted writes are invisible to other sessions
    a.execute("BEGIN").unwrap();
    a.execute("SET secret 1").unwrap();
    assert_eq!(scalar(&mut b, "GET secret"), Value::Null);
    a.execute("ROLLBACK").unwrap();
}

// ----------------------------------------------------------------- server

#[test]
fn native_protocol_over_tcp() {
    use std::io::{BufRead, BufReader};
    use std::net::TcpStream;
    let db = Database::in_memory();
    let cfg = polydb::server::Config { host: "127.0.0.1".into(), native: Some(0), redis: Some(0), postgres: None, mongo: None };
    let running = polydb::server::start(&db, &cfg).unwrap();

    let stream = TcpStream::connect(running.native.unwrap()).unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    let mut ask = |cmd: &str| {
        w.write_all(format!("{cmd}\n").as_bytes()).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        Value::parse(line.trim()).unwrap()
    };
    assert_eq!(ask("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").get("ok"), Some(&Value::Bool(true)));
    ask("INSERT INTO t VALUES (1, 'x')");
    let reply = ask("SELECT * FROM t");
    assert_eq!(reply.get("type"), Some(&t("rows")));
    assert_eq!(reply.get("rows"), Some(&Value::Array(vec![Value::Array(vec![Value::Int(1), t("x")])])));
    assert_eq!(ask("db.c.insert({_id: 7, a: [1, 2]})").get("value"), Some(&Value::Int(7)));
    assert_eq!(ask("db.c.find({a: 2})").get("type"), Some(&t("docs")));
    let bad = ask("SELEKT 1");
    assert_eq!(bad.get("ok"), Some(&Value::Bool(false)));
    assert_eq!(bad.get("kind"), Some(&t("parse")));

    // the same data through the Redis port, speaking raw RESP
    ask("SET greeting hello");
    let mut redis = TcpStream::connect(running.redis.unwrap()).unwrap();
    redis.write_all(b"*2\r\n$3\r\nGET\r\n$8\r\ngreeting\r\n").unwrap();
    let mut buf = [0u8; 11];
    std::io::Read::read_exact(&mut redis, &mut buf).unwrap();
    assert_eq!(&buf, b"$5\r\nhello\r\n");
    // and the embedding process sees it too
    let mut local = db.session();
    assert_eq!(scalar(&mut local, "GET greeting"), t("hello"));
}
