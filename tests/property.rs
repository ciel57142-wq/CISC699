//! Property-based tests with a deterministic generator (no external crates).
//! Each property runs `POLYDB_CASES` random cases (default 40); failures
//! print the seed and case so they can be reproduced with POLYDB_SEED.

mod common;
use common::*;
use polydb::{Database, FindOptions, Output, Value};

fn cases() -> u64 {
    std::env::var("POLYDB_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40)
}

fn sorted(mut v: Vec<Vec<Value>>) -> Vec<String> {
    let mut s: Vec<String> = v.drain(..).map(|r| Value::Array(r).to_json()).collect();
    s.sort();
    s
}

fn try_rows(db: &mut Database, q: &str) -> Result<Vec<Vec<Value>>, String> {
    match db.execute(q) {
        Ok(Output::Rows { rows, .. }) => Ok(rows),
        Ok(Output::Docs(d)) => Ok(d.into_iter().map(|x| vec![x]).collect()),
        Ok(other) => Err(format!("unexpected {other:?}")),
        Err(e) => Err(format!("{:?}", std::mem::discriminant(&e))),
    }
}

fn literal(r: &mut Rng, kind: u64) -> String {
    match kind {
        0 => r.range(-5, 25).to_string(),
        1 => sql_str(&format!(
            "{}{}",
            r.pick(&["a", "b", "c", "ab", "ba", ""]),
            r.below(3)
        )),
        2 => format!("{}.{}", r.range(-2, 12), r.below(10)),
        _ => "NULL".into(),
    }
}

/// A random predicate over columns a (INT), b (TEXT), c (REAL), d (INT).
fn predicate(r: &mut Rng, depth: u32) -> String {
    if depth > 0 && r.chance(35) {
        let op = r.pick(&["AND", "OR"]);
        return format!(
            "({} {op} {})",
            predicate(r, depth - 1),
            predicate(r, depth - 1)
        );
    }
    let (col, kind) = *r.pick(&[("a", 0u64), ("b", 1), ("c", 2), ("d", 0), ("id", 0)]);
    // usually a literal of the column's type, sometimes another type
    let lit_kind = if r.chance(85) { kind } else { r.below(4) };
    match r.below(10) {
        0 => format!("{col} IS NULL"),
        1 => format!("{col} IS NOT NULL"),
        2 => format!(
            "{col} BETWEEN {} AND {}",
            literal(r, lit_kind),
            literal(r, lit_kind)
        ),
        3 => format!(
            "{col} IN ({}, {}, {})",
            literal(r, lit_kind),
            literal(r, lit_kind),
            literal(r, kind)
        ),
        4 if kind == 1 => format!("{col} LIKE '{}%'", r.pick(&["a", "b", "ab", ""])),
        5 => format!("NOT ({col} = {})", literal(r, lit_kind)),
        _ => format!(
            "{col} {} {}",
            r.pick(&["=", "<", "<=", ">", ">=", "<>"]),
            literal(r, lit_kind)
        ),
    }
}

#[test]
fn indexed_queries_match_sequential_scans() {
    for case in 0..cases() {
        let mut r = Rng::new(seed() * 1000 + case);
        let mut db = Database::in_memory();
        for t in ["ix", "plain"] {
            ok(
                &mut db,
                &format!("CREATE TABLE {t} (id INT PRIMARY KEY, a INT, b TEXT, c REAL, d INT)"),
            );
        }
        let n = r.range(0, 120);
        for id in 0..n {
            let v = |r: &mut Rng, k: u64| {
                if r.chance(10) {
                    "NULL".into()
                } else {
                    literal(r, k)
                }
            };
            let row = format!(
                "({id}, {}, {}, {}, {})",
                v(&mut r, 0),
                v(&mut r, 1),
                v(&mut r, 2),
                v(&mut r, 0)
            );
            ok(&mut db, &format!("INSERT INTO ix VALUES {row}"));
            ok(&mut db, &format!("INSERT INTO plain VALUES {row}"));
        }
        // random index configuration on `ix` only
        let configs = [
            "(a)",
            "(b)",
            "(c)",
            "(a, b)",
            "(b, a)",
            "(d, c)",
            "(a, d, b)",
        ];
        for k in 0..r.range(1, 4) {
            let cols = r.pick(&configs);
            let _ = db.execute(&format!("CREATE INDEX ix_{k} ON ix {cols}"));
        }
        // random updates and deletes applied to both, keeping the indexes busy
        for _ in 0..r.range(0, 15) {
            let p = predicate(&mut r, 1);
            let (q1, q2) = if r.chance(50) {
                let set = format!("a = {}, b = {}", literal(&mut r, 0), literal(&mut r, 1));
                (
                    format!("UPDATE ix SET {set} WHERE {p}"),
                    format!("UPDATE plain SET {set} WHERE {p}"),
                )
            } else {
                (
                    format!("DELETE FROM ix WHERE {p}"),
                    format!("DELETE FROM plain WHERE {p}"),
                )
            };
            let (x, y) = (db.execute(&q1).is_ok(), db.execute(&q2).is_ok());
            assert_eq!(x, y, "case {case}: {q1} succeeded differently");
        }
        for _ in 0..25 {
            let p = predicate(&mut r, 2);
            let tail = match r.below(4) {
                0 => " ORDER BY id".to_string(),
                1 => format!(" ORDER BY a, id LIMIT {}", r.range(0, 10)),
                2 => " ORDER BY a DESC, id".to_string(),
                _ => String::new(),
            };
            let q = |t: &str| format!("SELECT id, a, b, c, d FROM {t} WHERE {p}{tail}");
            let (a, b) = (try_rows(&mut db, &q("ix")), try_rows(&mut db, &q("plain")));
            match (a, b) {
                (Ok(a), Ok(b)) if tail.is_empty() => {
                    assert_eq!(sorted(a), sorted(b), "case {case}: {}", q("ix"))
                }
                (Ok(a), Ok(b)) => assert_eq!(
                    a,
                    b,
                    "case {case}: {}\nplan:\n{}",
                    q("ix"),
                    plan(&mut db, &q("ix"))
                ),
                (Err(a), Err(b)) => assert_eq!(a, b, "case {case}: {}", q("ix")),
                (a, b) => panic!("case {case}: {} -> {a:?} vs {b:?}", q("ix")),
            }
        }
    }
}

fn random_doc(r: &mut Rng, id: i64) -> String {
    let mut f = vec![format!("_id: {id}")];
    let val = |r: &mut Rng| match r.below(6) {
        0 => "null".to_string(),
        1 => format!("'{}'", r.pick(&["x", "y", "z", "xy"])),
        2 => format!("{}.5", r.range(0, 5)),
        3 => format!("[{}, {}]", r.range(0, 5), r.range(0, 5)),
        _ => r.range(0, 8).to_string(),
    };
    if r.chance(85) {
        f.push(format!("a: {}", val(r)));
    }
    if r.chance(85) {
        f.push(format!("b: {}", val(r)));
    }
    if r.chance(70) {
        f.push(format!("n: {{m: {}, k: {}}}", val(r), r.range(0, 3)));
    }
    format!("{{{}}}", f.join(", "))
}

fn random_filter(r: &mut Rng, depth: u32) -> String {
    if depth > 0 && r.chance(25) {
        return format!(
            "{{{}: [{}, {}]}}",
            r.pick(&["$and", "$or"]),
            random_filter(r, depth - 1),
            random_filter(r, depth - 1)
        );
    }
    let field = r.pick(&["a", "b", "n.m", "n.k"]);
    let v = match r.below(5) {
        0 => "null".to_string(),
        1 => format!("'{}'", r.pick(&["x", "y", "xy"])),
        2 => format!("{}.5", r.range(0, 5)),
        _ => r.range(0, 8).to_string(),
    };
    match r.below(6) {
        0 => format!("{{'{field}': {v}}}"),
        1 => format!("{{'{field}': {{$gte: {v}, $lt: {}}}}}", r.range(2, 9)),
        2 => format!("{{'{field}': {{$in: [{v}, {}]}}}}", r.range(0, 8)),
        3 => format!("{{'{field}': {{$ne: {v}}}}}"),
        4 => format!("{{'{field}': {{$exists: {}}}}}", r.chance(50)),
        _ => format!(
            "{{'{field}': {{{}: {v}}}}}",
            r.pick(&["$gt", "$lt", "$lte"])
        ),
    }
}

#[test]
fn document_indexes_match_collection_scans() {
    for case in 0..cases() {
        let mut r = Rng::new(seed() * 2000 + case);
        let mut db = Database::in_memory();
        let specs = [
            "{a: 1}",
            "{b: -1}",
            "{'n.m': 1}",
            "{a: 1, b: 1}",
            "{'n.k': 1, a: -1}",
        ];
        for k in 0..r.range(1, 3) {
            let s = r.pick(&specs);
            let _ = db.execute(&format!("db.ix.createIndex({s}, {{name: 'i{k}'}})"));
        }
        for id in 0..r.range(0, 80) {
            let d = random_doc(&mut r, id);
            let x = db.execute(&format!("db.ix.insert({d})")).is_ok();
            assert!(x || d.contains('['), "case {case}: insert {d} failed");
            if x {
                ok(&mut db, &format!("db.plain.insert({d})"));
            }
        }
        for _ in 0..r.range(0, 10) {
            let f = random_filter(&mut r, 1);
            let u = format!("{{$set: {{b: {}}}}}", r.range(0, 8));
            let x = db.execute(&format!("db.ix.update({f}, {u})")).is_ok();
            let y = db.execute(&format!("db.plain.update({f}, {u})")).is_ok();
            assert_eq!(x, y, "case {case}");
        }
        assert!(
            db.doc_validate_indexes("ix").unwrap().is_empty(),
            "case {case}: index inconsistent"
        );
        for _ in 0..25 {
            let f = Value::parse(&random_filter(&mut r, 2)).unwrap();
            let sort = if r.chance(50) {
                Some(Value::parse(r.pick(&["{a: 1}", "{b: -1}", "{'n.k': 1}"])).unwrap())
            } else {
                None
            };
            let opts = FindOptions {
                sort: sort.clone(),
                skip: 0,
                limit: None,
            };
            let a = db.doc_find("ix", &f, &opts);
            let b = db.doc_find("plain", &f, &opts);
            match (a, b) {
                (Ok(a), Ok(b)) if sort.is_none() => {
                    let s = |v: Vec<Value>| sorted(v.into_iter().map(|d| vec![d]).collect());
                    assert_eq!(s(a), s(b), "case {case}: {}", f.to_json())
                }
                (Ok(a), Ok(b)) => {
                    // equal sort keys may tie in any order: compare the key sequence and the set
                    let key = |v: &[Value], p: &str| {
                        v.iter()
                            .map(|d| polydb::doc::get_path(d, p).cloned().unwrap_or(Value::Null))
                            .collect::<Vec<_>>()
                    };
                    let p = sort
                        .as_ref()
                        .and_then(|s| match s {
                            Value::Object(o) => o.keys().next().cloned(),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(
                        key(&a, &p),
                        key(&b, &p),
                        "case {case}: {} sort {}",
                        f.to_json(),
                        sort.unwrap().to_json()
                    );
                    let s = |v: Vec<Value>| sorted(v.into_iter().map(|d| vec![d]).collect());
                    assert_eq!(s(a), s(b), "case {case}");
                }
                (Err(_), Err(_)) => {}
                (a, b) => panic!("case {case}: {} -> {a:?} vs {b:?}", f.to_json()),
            }
        }
    }
}

/// Random operations across all three models.
fn random_op(r: &mut Rng) -> String {
    let id = r.range(0, 30);
    match r.below(9) {
        0 => format!(
            "INSERT INTO t VALUES ({id}, {}, {})",
            r.range(0, 9),
            sql_str(&r.word(3))
        ),
        1 => format!(
            "UPDATE t SET v = v + {} WHERE id % 3 = {}",
            r.range(-2, 2),
            r.below(3)
        ),
        2 => format!("DELETE FROM t WHERE id = {id}"),
        3 => format!("db.c.insert({{_id: {id}, v: {}}})", r.range(0, 9)),
        4 => format!(
            "db.c.update({{v: {{$lt: {}}}}}, {{$inc: {{v: 1}}}})",
            r.range(0, 9)
        ),
        5 => format!("db.c.delete({{_id: {id}}})"),
        6 => format!("SET k{id} {}", r.range(0, 99)),
        7 => format!("DEL k{id}"),
        _ => format!("INCR n{}", r.below(3)),
    }
}

#[test]
fn rollback_restores_the_exact_previous_state() {
    for case in 0..cases() {
        let mut r = Rng::new(seed() * 3000 + case);
        let mut db = Database::in_memory();
        ok(
            &mut db,
            "CREATE TABLE t (id INT PRIMARY KEY, v INT, s TEXT)",
        );
        ok(&mut db, "CREATE INDEX t_v ON t (v)");
        ok(&mut db, "db.c.createIndex({v: 1})");
        for _ in 0..r.range(0, 40) {
            let _ = db.execute(&random_op(&mut r));
        }
        let before = dump(&mut db);
        ok(&mut db, "BEGIN");
        for _ in 0..r.range(1, 40) {
            let _ = db.execute(&random_op(&mut r)); // failures inside the transaction are fine
        }
        ok(&mut db, "ROLLBACK");
        assert_eq!(
            dump(&mut db),
            before,
            "case {case}: rollback did not restore the state"
        );
        assert!(db.doc_validate_indexes("c").unwrap().is_empty());
        // indexes agree with the table after the rollback
        let via_index = sorted(rows(&mut db, "SELECT id FROM t WHERE v >= 0"));
        let via_scan = sorted(rows(&mut db, "SELECT id FROM t WHERE v + 0 >= 0"));
        assert_eq!(via_index, via_scan, "case {case}");
    }
}

#[test]
fn committed_data_survives_restart() {
    let dir = tmp("prop-restart");
    let mut r = Rng::new(seed() * 4000);
    for case in 0..cases().min(15) {
        let expected = {
            let mut db = Database::open(&dir).unwrap();
            if case == 0 {
                ok(
                    &mut db,
                    "CREATE TABLE t (id INT PRIMARY KEY, v INT, s TEXT)",
                );
            }
            for _ in 0..r.range(0, 30) {
                if r.chance(20) {
                    ok(&mut db, "BEGIN");
                    for _ in 0..r.range(1, 5) {
                        let _ = db.execute(&random_op(&mut r));
                    }
                    let end = if r.chance(70) { "COMMIT" } else { "ROLLBACK" };
                    let _ = db.execute(end);
                } else {
                    let _ = db.execute(&random_op(&mut r));
                }
            }
            if r.chance(30) {
                db.checkpoint().unwrap();
            }
            dump(&mut db)
        };
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(
            dump(&mut db),
            expected,
            "case {case}: state changed across restart"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unique_and_foreign_key_invariants_hold() {
    for case in 0..cases() {
        let mut r = Rng::new(seed() * 5000 + case);
        let mut db = Database::in_memory();
        ok(
            &mut db,
            "CREATE TABLE p (id INT PRIMARY KEY, code INT UNIQUE)",
        );
        ok(&mut db, "CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id) ON DELETE CASCADE, x INT, y INT, UNIQUE (x, y), pc INT REFERENCES p(code) ON DELETE SET NULL)");
        for _ in 0..120 {
            let q = match r.below(7) {
                0 | 1 => format!(
                    "INSERT INTO p VALUES ({}, {})",
                    r.range(0, 15),
                    r.range(0, 15)
                ),
                2 | 3 => format!(
                    "INSERT INTO c VALUES ({}, {}, {}, {}, {})",
                    r.range(0, 40),
                    r.range(0, 17),
                    r.range(0, 4),
                    r.range(0, 4),
                    r.range(0, 17)
                ),
                4 => format!("DELETE FROM p WHERE id = {}", r.range(0, 15)),
                5 => format!(
                    "UPDATE c SET x = {}, y = {} WHERE id = {}",
                    r.range(0, 4),
                    r.range(0, 4),
                    r.range(0, 40)
                ),
                _ => format!(
                    "UPDATE p SET code = {} WHERE id = {}",
                    r.range(0, 15),
                    r.range(0, 15)
                ),
            };
            let _ = db.execute(&q);
        }
        let dup = |db: &mut Database, q: &str| {
            assert_eq!(rows(db, q), Vec::<Vec<Value>>::new(), "case {case}: {q}")
        };
        dup(
            &mut db,
            "SELECT code FROM p GROUP BY code HAVING COUNT(*) > 1",
        );
        dup(
            &mut db,
            "SELECT x, y FROM c GROUP BY x, y HAVING COUNT(*) > 1",
        );
        dup(&mut db, "SELECT id FROM c WHERE pid IS NOT NULL AND NOT EXISTS (SELECT 1 FROM p WHERE p.id = c.pid)");
        dup(&mut db, "SELECT id FROM c WHERE pc IS NOT NULL AND NOT EXISTS (SELECT 1 FROM p WHERE p.code = c.pc)");
    }
}

#[test]
fn expired_keys_never_come_back() {
    let dir = tmp("prop-ttl");
    let mut r = Rng::new(seed() * 6000);
    let mut db = Database::open(&dir).unwrap();
    // key -> Some(expiry) or None (persistent), per the last write
    let mut model: std::collections::HashMap<String, Option<std::time::Instant>> =
        Default::default();
    let start = std::time::Instant::now();
    for _ in 0..300 {
        let k = format!("k{}", r.below(25));
        match r.below(4) {
            0 => {
                let ms = r.range(20, 200) as u64;
                db.kv_set_ttl(&k, b"v", Some(ms as i64)).unwrap();
                model.insert(
                    k,
                    Some(std::time::Instant::now() + std::time::Duration::from_millis(ms)),
                );
            }
            1 => {
                db.kv_set(&k, b"v").unwrap();
                model.insert(k, None);
            }
            2 => {
                db.kv_del(&k).unwrap();
                model.remove(&k);
            }
            _ => {
                // PERSIST keeps the value only if it has not expired yet
                if let Some(Some(at)) = model.get(&k).cloned() {
                    if at > std::time::Instant::now() + std::time::Duration::from_millis(5)
                        && db.kv_expire(&k, None).unwrap()
                    {
                        model.insert(k, None);
                    }
                }
            }
        }
    }
    let _ = start;
    std::thread::sleep(std::time::Duration::from_millis(400));
    let check =
        |db: &mut Database,
         model: &std::collections::HashMap<String, Option<std::time::Instant>>| {
            for i in 0..25 {
                let k = format!("k{i}");
                let want = matches!(model.get(&k), Some(None));
                assert_eq!(
                    db.kv_get(&k).unwrap().is_some(),
                    want,
                    "{k}: visibility wrong"
                );
            }
            let visible: Vec<String> = db.kv_keys("k").unwrap();
            assert!(
                visible.iter().all(|k| matches!(model.get(k), Some(None))),
                "{visible:?}"
            );
        };
    check(&mut db, &model);
    drop(db);
    let mut db = Database::open(&dir).unwrap();
    check(&mut db, &model);
    // and they are physically gone after a purge: no x/X metadata left for them
    db.purge_expired().unwrap();
    let persistent = model.values().filter(|v| v.is_none()).count();
    assert_eq!(db.kv_keys("").unwrap().len(), persistent);
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn pbsql_and_sql_forms_agree() {
    for case in 0..cases() {
        let mut r = Rng::new(seed() * 7000 + case);
        let mut db = Database::in_memory();
        ok(
            &mut db,
            "CREATE TABLE t (id INT PRIMARY KEY, a INT, b TEXT, c REAL, d INT)",
        );
        for id in 0..r.range(0, 60) {
            ok(
                &mut db,
                &format!(
                    "INSERT INTO t VALUES ({id}, {}, {}, {}, {})",
                    literal(&mut r, 0),
                    literal(&mut r, 1),
                    literal(&mut r, 2),
                    literal(&mut r, 0)
                ),
            );
        }
        // the same rows as documents
        for row in rows(&mut db, "SELECT id, a, b, c, d FROM t") {
            let mut o = std::collections::BTreeMap::new();
            for (k, v) in ["_id", "a", "b", "c", "d"].iter().zip(row) {
                if !v.is_null() {
                    o.insert(k.to_string(), v);
                }
            }
            db.doc_insert("t", Value::Object(o)).unwrap();
        }
        for _ in 0..15 {
            let p = predicate(&mut r, 2);
            let sql = try_rows(&mut db, &format!("SELECT id FROM t WHERE {p}"));
            let pbsql = try_rows(&mut db, &format!("SELECT id FROM RELATIONAL.t WHERE {p}"));
            assert_eq!(
                sql.clone().map(sorted),
                pbsql.map(sorted),
                "case {case}: {p}"
            );
            // the document form: same predicate over fields (id is _id); missing fields are NULL
            let dp = p.replace("id ", "_id ").replace("(id", "(_id");
            if let Ok(s) = sql {
                let d = try_rows(&mut db, &format!("SELECT _id FROM DOCUMENT.t WHERE {dp}"));
                assert_eq!(
                    Ok(sorted(s)),
                    d.map(sorted),
                    "case {case}: document form of {p}"
                );
            }
        }
    }
}
