//! Document indexes (compound, unique, nested, ordered), the streaming
//! executor (early termination, spilling sort / aggregation / hash join),
//! RIGHT / FULL joins, sort elimination and cancellation.

use polydb::{Database, Error, FindOptions, Output, Value};

fn rows(db: &mut Database, q: &str) -> Vec<Vec<Value>> {
    match db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{q}: expected rows, got {other:?}"),
    }
}

fn docs(db: &mut Database, q: &str) -> Vec<Value> {
    match db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        Output::Docs(d) => d,
        other => panic!("{q}: expected docs, got {other:?}"),
    }
}

fn plan(db: &mut Database, q: &str) -> String {
    rows(db, &format!("EXPLAIN {q}"))
        .into_iter()
        .map(|r| r[0].display())
        .collect::<Vec<_>>()
        .join("\n")
}

fn analyze(db: &mut Database, q: &str) -> String {
    rows(db, &format!("EXPLAIN ANALYZE {q}"))
        .into_iter()
        .map(|r| r[0].display())
        .collect::<Vec<_>>()
        .join("\n")
}

fn j(s: &str) -> Value {
    Value::parse(s).unwrap()
}

fn seed_people(db: &mut Database, n: i64) {
    for i in 0..n {
        let city = ["Berlin", "Paris", "Rome", "Oslo"][(i % 4) as usize];
        let extra = if i % 10 == 0 {
            String::new()
        } else {
            format!(", age: {}", i % 50)
        };
        db.execute(&format!("db.people.insert({{_id: {i}, name: 'p{i:04}', city: '{city}', addr: {{zip: {}}}{extra}}})", 10000 + i % 7))
            .unwrap();
    }
}

#[test]
fn compound_unique_and_nested_document_indexes() {
    let mut db = Database::in_memory();
    seed_people(&mut db, 200);
    let q = "db.people.find({city: 'Paris', age: {$gte: 20, $lt: 30}}).sort({_id: 1})";
    let before = docs(&mut db, q);
    db.execute("db.people.createIndex({city: 1, age: -1})")
        .unwrap();
    db.execute("db.people.createIndex({'addr.zip': 1})")
        .unwrap();
    assert_eq!(
        docs(&mut db, q),
        before,
        "compound index changed the result"
    );
    // the planner uses the compound index for PBSQL too
    let p = plan(
        &mut db,
        "SELECT name FROM DOCUMENT.people WHERE city = 'Paris' AND age >= 20",
    );
    assert!(
        p.contains("DocumentIndexRangeScan") && p.contains("city_1_age_-1"),
        "{p}"
    );
    let p = plan(
        &mut db,
        "SELECT name FROM DOCUMENT.people WHERE addr.zip = 10003",
    );
    assert!(
        p.contains("DocumentIndexScan") && p.contains("addr.zip_1"),
        "{p}"
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(*) FROM DOCUMENT.people WHERE addr.zip = 10003"
        )
        .len(),
        1
    );
    assert_eq!(
        docs(&mut db, "db.people.find({'addr.zip': 10003})").len(),
        docs(&mut db, "db.people.find({}).limit(1000)")
            .iter()
            .filter(|d| d.get("addr").and_then(|a| a.get("zip")) == Some(&Value::Int(10003)))
            .count()
    );
    // missing fields are indexed as null in new indexes: {age: null} uses the index and is complete
    assert_eq!(
        docs(&mut db, "db.people.find({city: 'Berlin', age: null})").len(),
        10
    );

    // unique index: refuses duplicates on insert and update, NULLs count once
    db.execute("db.people.createIndex({name: 1}, {unique: true})")
        .unwrap();
    assert!(matches!(
        db.execute("db.people.insert({_id: 999, name: 'p0001'})"),
        Err(Error::Constraint(_))
    ));
    assert!(matches!(
        db.execute("db.people.update({_id: 2}, {$set: {name: 'p0001'}})"),
        Err(Error::Constraint(_))
    ));
    db.execute("db.people.update({_id: 2}, {$set: {name: 'renamed'}})")
        .unwrap();
    assert_eq!(docs(&mut db, "db.people.find({name: 'renamed'})").len(), 1);
    db.execute("db.tags.createIndex({t: 1}, {unique: true})")
        .unwrap();
    db.execute("db.tags.insert({_id: 1})").unwrap();
    assert!(
        matches!(
            db.execute("db.tags.insert({_id: 2})"),
            Err(Error::Constraint(_))
        ),
        "a unique index allows one missing value"
    );
    // creating a unique index over duplicates fails and leaves nothing behind
    assert!(matches!(
        db.execute("db.people.createIndex({city: 1}, {unique: true})"),
        Err(Error::Constraint(_))
    ));
    assert_eq!(db.doc_indexes("people").unwrap().len(), 3);
    // multikey: arrays index each element; two arrays in one compound index are refused
    db.execute("db.m.createIndex({tags: 1})").unwrap();
    db.execute(
        "db.m.insert([{_id: 1, tags: ['a', 'b']}, {_id: 2, tags: ['b']}, {_id: 3, tags: []}])",
    )
    .unwrap();
    assert_eq!(docs(&mut db, "db.m.find({tags: 'b'})").len(), 2);
    db.execute("db.m2.createIndex({a: 1, b: 1})").unwrap();
    assert!(matches!(
        db.execute("db.m2.insert({a: [1], b: [2]})"),
        Err(Error::Constraint(_))
    ));
    // drop, validate, reindex
    assert!(db.doc_validate_indexes("people").unwrap().is_empty());
    assert_eq!(db.doc_reindex("people").unwrap(), 3);
    assert!(db.execute("db.people.dropIndex('addr.zip_1')").is_ok());
    assert!(plan(
        &mut db,
        "SELECT name FROM DOCUMENT.people WHERE addr.zip = 10003"
    )
    .contains("DocumentScan"));
    match db.execute("db.people.getIndexes()").unwrap() {
        Output::Docs(d) => assert_eq!(d.len(), 3, "_id plus two"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn document_index_order_serves_sorts_with_early_termination() {
    let mut db = Database::in_memory();
    seed_people(&mut db, 300);
    let in_memory = docs(
        &mut db,
        "db.people.find({}).sort({age: -1, _id: 1}).limit(15)",
    );
    db.execute("db.people.createIndex({age: 1})").unwrap();
    let by_index: Vec<i64> = db
        .doc_find(
            "people",
            &j("{}"),
            &FindOptions {
                sort: Some(j("{age: -1}")),
                skip: 0,
                limit: Some(15),
            },
        )
        .unwrap()
        .iter()
        .map(|d| d.get("age").and_then(Value::as_i64).unwrap())
        .collect();
    let expect: Vec<i64> = in_memory
        .iter()
        .map(|d| d.get("age").and_then(Value::as_i64).unwrap())
        .collect();
    assert_eq!(by_index, expect);
    // missing ages (null) sort first ascending, as in MongoDB
    let first = docs(&mut db, "db.people.find({}).sort({age: 1}).limit(3)");
    assert!(first.iter().all(|d| d.get("age").is_none()), "{first:?}");
    // PBSQL: ORDER BY on the indexed field with a range predicate needs no sort
    let p = plan(
        &mut db,
        "SELECT name, age FROM DOCUMENT.people WHERE age > 10 ORDER BY age DESC LIMIT 5",
    );
    assert!(!p.contains("Sort ("), "{p}");
    assert!(p.contains("Scan Direction: backward"), "{p}");
    let r = rows(
        &mut db,
        "SELECT age FROM DOCUMENT.people WHERE age > 10 ORDER BY age DESC LIMIT 5",
    );
    assert!(r.iter().all(|x| x[0] == Value::Int(49)), "{r:?}");
    // compound sort order is preserved from the shell
    let s = docs(
        &mut db,
        "db.people.find({city: 'Rome'}).sort({age: -1, name: 1}).limit(3)",
    );
    let ages: Vec<_> = s.iter().map(|d| d.get("age").cloned()).collect();
    assert!(
        ages.windows(2)
            .all(|w| w[0].as_ref().unwrap().total_cmp(w[1].as_ref().unwrap())
                != std::cmp::Ordering::Less),
        "{ages:?}"
    );
}

#[test]
fn limit_stops_scans_early() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)")
        .unwrap();
    for i in 0..500 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i % 13))
            .unwrap();
    }
    // a filter that is not pushed into the scan still stops as soon as the limit is reached
    let a = analyze(&mut db, "SELECT id FROM t WHERE v + 0 = 3 LIMIT 2");
    let examined: Vec<&str> = a.lines().filter(|l| l.contains("TableScan")).collect();
    assert!(examined[0].contains("examined=17"), "{a}");
    // ORDER BY the primary key: no sort, reverse scan, reads only what LIMIT needs
    let a = analyze(&mut db, "SELECT id FROM t ORDER BY id DESC LIMIT 3");
    assert!(!a.contains("Sort ("), "{a}");
    assert!(a.contains("examined=3"), "{a}");
    assert_eq!(
        rows(&mut db, "SELECT id FROM t ORDER BY id DESC LIMIT 3"),
        vec![
            vec![Value::Int(499)],
            vec![Value::Int(498)],
            vec![Value::Int(497)]
        ]
    );
    // ORDER BY an indexed column with a range predicate uses index order
    db.execute("CREATE INDEX t_v ON t (v)").unwrap();
    let p = plan(
        &mut db,
        "SELECT id, v FROM t WHERE v >= 10 ORDER BY v LIMIT 4",
    );
    assert!(p.contains("IndexRangeScan") && !p.contains("Sort ("), "{p}");
    let r = rows(&mut db, "SELECT v FROM t WHERE v >= 10 ORDER BY v LIMIT 4");
    assert!(r.iter().all(|x| x[0] == Value::Int(10)), "{r:?}");
    // EXISTS stops at the first row of its subquery
    let a = analyze(
        &mut db,
        "SELECT 1 WHERE EXISTS (SELECT 1 FROM t WHERE v = 5)",
    );
    assert!(a.contains("actual rows=1"), "{a}");
}

#[test]
fn spilling_operators_match_in_memory_results() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE a (id INT PRIMARY KEY, g INT, s TEXT)")
        .unwrap();
    db.execute("CREATE TABLE b (id INT PRIMARY KEY, aid INT, w INT)")
        .unwrap();
    for i in 0..1500 {
        db.execute(&format!(
            "INSERT INTO a VALUES ({i}, {}, 'text-{:05}')",
            (i * 7) % 97,
            (i * 31) % 1500
        ))
        .unwrap();
        db.execute(&format!(
            "INSERT INTO b VALUES ({i}, {}, {})",
            (i * 13) % 1500,
            i % 5
        ))
        .unwrap();
    }
    let queries = [
        "SELECT id, s FROM a ORDER BY s DESC, id",
        "SELECT g, COUNT(*), SUM(id), MIN(s), MAX(s), AVG(id) FROM a GROUP BY g ORDER BY g",
        "SELECT a.id, b.w FROM a JOIN b ON b.aid + 0 = a.id ORDER BY a.id, b.w",
        "SELECT a.id, b.id FROM a LEFT JOIN b ON b.aid + 0 = a.id AND b.w = 0 ORDER BY a.id, b.id",
        "SELECT a.g, b.id FROM b RIGHT JOIN a ON a.id = b.aid + 0 AND b.w = 1 ORDER BY a.id, b.id",
        "SELECT COUNT(*) FROM a FULL JOIN b ON a.id = b.aid + 0 AND b.w = 4",
    ];
    let expected: Vec<_> = queries.iter().map(|q| rows(&mut db, q)).collect();
    db.set_memory_limit(16 << 10); // 16 KB: every blocking operator spills
    for (q, want) in queries.iter().zip(&expected) {
        assert_eq!(&rows(&mut db, q), want, "{q} differs when spilling");
    }
    let a = analyze(&mut db, queries[0]);
    assert!(a.contains("spilled to disk"), "{a}");
    let a = analyze(&mut db, queries[1]);
    assert!(a.contains("spilled to disk"), "{a}");
    let a = analyze(&mut db, queries[2]);
    assert!(
        a.contains("HashJoin") && a.contains("spilled to disk"),
        "{a}"
    );
    // DISTINCT beyond the limit fails clearly instead of exhausting memory
    assert!(matches!(
        db.execute("SELECT DISTINCT s FROM a"),
        Err(Error::Cancelled(_))
    ));
    // no spill files are left behind
    let leftover = std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("polydb-spill-{}-", std::process::id()))
        })
        .count();
    assert_eq!(leftover, 0);
}

#[test]
fn right_and_full_outer_joins() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE l (id INT PRIMARY KEY, v TEXT)")
        .unwrap();
    db.execute("CREATE TABLE r (id INT PRIMARY KEY, lid INT)")
        .unwrap();
    db.execute("INSERT INTO l VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        .unwrap();
    db.execute("INSERT INTO r VALUES (10, 1), (11, 1), (12, 4), (13, NULL)")
        .unwrap();
    let n = |v: i64| Value::Int(v);
    assert_eq!(
        rows(
            &mut db,
            "SELECT l.id, r.id FROM l RIGHT JOIN r ON r.lid = l.id ORDER BY r.id"
        ),
        vec![
            vec![n(1), n(10)],
            vec![n(1), n(11)],
            vec![Value::Null, n(12)],
            vec![Value::Null, n(13)]
        ]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT l.id, r.id FROM l FULL OUTER JOIN r ON r.lid = l.id ORDER BY l.id, r.id"
        ),
        vec![
            vec![Value::Null, n(12)],
            vec![Value::Null, n(13)],
            vec![n(1), n(10)],
            vec![n(1), n(11)],
            vec![n(2), Value::Null],
            vec![n(3), Value::Null],
        ]
    );
    // WHERE on the preserved side after a RIGHT JOIN is not pushed below the join
    assert_eq!(
        rows(
            &mut db,
            "SELECT r.id FROM l RIGHT JOIN r ON r.lid = l.id WHERE l.id IS NULL ORDER BY r.id"
        ),
        vec![vec![n(12)], vec![n(13)]]
    );
    // non-equi full join uses a nested loop
    let p = plan(&mut db, "SELECT l.id FROM l FULL JOIN r ON r.lid > l.id");
    assert!(p.contains("NestedLoopJoin (FULL)"), "{p}");
    assert_eq!(
        rows(
            &mut db,
            "SELECT COUNT(*) FROM l FULL JOIN r ON r.lid > l.id"
        )
        .len(),
        1
    );
}

#[test]
fn a_running_query_can_be_cancelled() {
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    for i in 0..300 {
        db.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    let flag = db.cancel_handle();
    let h = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    // a 27-million-row cross join: only finishes if cancellation fails
    let res = db.execute("SELECT COUNT(*) FROM t a, t b, t c");
    h.join().unwrap();
    assert!(matches!(res, Err(Error::Cancelled(_))), "{res:?}");
    // the flag is cleared for the next statement
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM t"),
        vec![vec![Value::Int(300)]]
    );
}
