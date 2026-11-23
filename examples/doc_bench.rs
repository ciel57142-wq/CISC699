//! Document-model micro-benchmark through the public API.
//!
//!     cargo run --release --example doc_bench [docs]
//!
//! Uses only long-standing calls (`execute`, `doc_insert`, `doc_find`,
//! `doc_create_index`), so the same file runs against the version before
//! the document optimisation work and the one after it. On a version
//! without `db.c.aggregate([...])` that line prints "unsupported"; the
//! "client-side group" line computes the same answer in the application.
//!
//! Timings are wall-clock for the whole operation. Every query's result size
//! is checked so a faster version cannot be faster by returning less.

use polydb::{Database, FindOptions, Output, Value};
use std::collections::BTreeMap;
use std::time::Instant;

fn time<T>(label: &str, n: usize, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let el = start.elapsed();
    let per = if n > 0 { el.as_secs_f64() * 1e6 / n as f64 } else { 0.0 };
    println!("{label:<46} {:>9.1} ms  {:>10.2} us/op  ({n} ops)", el.as_secs_f64() * 1e3, per);
    out
}

fn docs(db: &mut Database, cmd: &str) -> Vec<Value> {
    match db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")) {
        Output::Docs(d) => d,
        other => panic!("{cmd}: {other:?}"),
    }
}

fn user_json(i: usize) -> String {
    format!(
        "{{_id: {i}, name: 'user{i}', age: {}, city: 'c{}', score: {}.5, \
         addr: {{street: '{i} Main St', zip: 'z{}', geo: {{lat: {}, lon: {}}}}}, \
         tags: ['t{}', 't{}'], active: {}}}",
        i % 90,
        i % 100,
        i % 1000,
        i % 500,
        i % 180,
        i % 360,
        i % 7,
        i % 11,
        i % 2 == 0
    )
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let mut db = Database::in_memory();
    db.execute("db.users.createIndex({city: 1})").unwrap();
    db.execute("db.users.createIndex({'addr.zip': 1})").unwrap();
    db.execute("db.users.createIndex({age: 1})").unwrap();

    time("bulk insertMany (batches of 500, 3 indexes)", n, || {
        for chunk in (0..n).collect::<Vec<_>>().chunks(500) {
            let body: Vec<String> = chunk.iter().map(|&i| user_json(i)).collect();
            db.execute(&format!("db.users.insertMany([{}])", body.join(","))).unwrap();
        }
    });

    let singles = 5_000;
    let parsed: Vec<Value> = (0..singles).map(|i| Value::parse(&user_json(1_000_000 + i)).unwrap()).collect();
    time("single doc_insert (typed API, own commit)", singles, || {
        for d in parsed {
            db.doc_insert("users2", d).unwrap();
        }
    });

    let lookups = 5_000;
    time("_id point lookup (typed API)", lookups, || {
        for i in 0..lookups {
            let f = Value::parse(&format!("{{\"_id\": {}}}", (i * 13) % n)).unwrap();
            assert_eq!(db.doc_find("users", &f, &FindOptions::default()).unwrap().len(), 1);
        }
    });
    time("_id point lookup (command text)", lookups, || {
        for i in 0..lookups {
            assert_eq!(docs(&mut db, &format!("db.users.find({{_id: {}}})", (i * 13) % n)).len(), 1);
        }
    });

    let per_city = n / 100;
    time("indexed equality (city, ~1% of docs)", 200, || {
        for i in 0..200 {
            assert_eq!(docs(&mut db, &format!("db.users.find({{city: 'c{}'}})", i % 100)).len(), per_city);
        }
    });
    let per_zip = n / 500;
    time("indexed nested equality (addr.zip)", 500, || {
        for i in 0..500 {
            assert_eq!(docs(&mut db, &format!("db.users.find({{'addr.zip': 'z{}'}})", i % 500)).len(), per_zip);
        }
    });
    time("indexed equality + residual predicate", 200, || {
        for i in 0..200 {
            let got = docs(&mut db, &format!("db.users.find({{city: 'c{}', active: true}})", i % 100));
            assert!(got.len() <= per_city);
        }
    });
    let expect_range = (0..n).filter(|i| (40..42).contains(&(i % 90))).count();
    time("indexed range (age >= 40 and < 42)", 50, || {
        for _ in 0..50 {
            assert_eq!(docs(&mut db, "db.users.find({age: {$gte: 40, $lt: 42}})").len(), expect_range);
        }
    });
    let expect_in = (0..n).filter(|i| matches!(i % 100, 3 | 4 | 5)).count();
    time("indexed $in (3 cities)", 100, || {
        for _ in 0..100 {
            assert_eq!(docs(&mut db, "db.users.find({city: {$in: ['c3', 'c4', 'c5']}})").len(), expect_in);
        }
    });
    let expect_geo = (0..n).filter(|i| i % 180 == 17).count();
    time("unindexed nested scan (addr.geo.lat)", 20, || {
        for _ in 0..20 {
            assert_eq!(docs(&mut db, "db.users.find({'addr.geo.lat': 17})").len(), expect_geo);
        }
    });
    time("full scan, no match", 20, || {
        for _ in 0..20 {
            assert_eq!(docs(&mut db, "db.users.find({name: 'nobody'})").len(), 0);
        }
    });
    time("sort by score desc, limit 10 (whole coll)", 20, || {
        for _ in 0..20 {
            assert_eq!(docs(&mut db, "db.users.find({}).sort({score: -1}).limit(10)").len(), 10);
        }
    });
    time("sort by indexed age, limit 10", 20, || {
        for _ in 0..20 {
            let d = docs(&mut db, "db.users.find({}).sort({age: 1}).limit(10)");
            assert_eq!(d.len(), 10);
            assert_eq!(d[0].get("age"), Some(&Value::Int(0)));
        }
    });
    time("count (indexed equality)", 200, || {
        for i in 0..200 {
            match db.execute(&format!("db.users.count({{city: 'c{}'}})", i % 100)).unwrap() {
                Output::Scalar(Value::Int(c)) => assert_eq!(c as usize, per_city),
                other => panic!("{other:?}"),
            }
        }
    });

    // Aggregation: average score and count per city, over active users.
    let pipeline = "db.users.aggregate([{$match: {active: true}}, \
                    {$group: {_id: '$city', n: {$sum: 1}, avg: {$avg: '$score'}}}, \
                    {$sort: {n: -1, _id: 1}}, {$limit: 5}])";
    match db.execute(pipeline) {
        Ok(_) => {
            time("aggregate $match/$group/$sort/$limit", 10, || {
                for _ in 0..10 {
                    assert_eq!(docs(&mut db, pipeline).len(), 5);
                }
            });
        }
        Err(e) => println!("{:<46} unsupported ({e})", "aggregate $match/$group/$sort/$limit"),
    }
    time("client-side group (find + fold in the app)", 10, || {
        for _ in 0..10 {
            let all = db.doc_find("users", &Value::parse("{\"active\": true}").unwrap(), &FindOptions::default()).unwrap();
            let mut g: BTreeMap<String, (i64, f64)> = BTreeMap::new();
            for d in &all {
                let e = g.entry(d.get("city").and_then(Value::as_str).unwrap_or("").to_string()).or_default();
                e.0 += 1;
                e.1 += d.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            }
            assert_eq!(g.len(), 50);
        }
    });

    time("update $inc through index (city)", 100, || {
        for i in 0..100 {
            db.execute(&format!("db.users.updateMany({{city: 'c{i}'}}, {{$inc: {{score: 1}}}})")).unwrap();
        }
    });
    time("update indexed field ($set age) by _id", 2_000, || {
        for i in 0..2_000 {
            db.execute(&format!("db.users.updateOne({{_id: {i}}}, {{$set: {{age: {}}}}})", (i + 1) % 90)).unwrap();
        }
    });
    time("delete by _id", 1_000, || {
        for i in 0..1_000 {
            db.execute(&format!("db.users.deleteOne({{_id: {}}})", n - 1 - i)).unwrap();
        }
    });

    // Large documents: ~64 KiB each (a 500-element array of objects plus a long string).
    let big = 200;
    let big_doc = |i: usize| {
        let items: Vec<String> = (0..500).map(|j| format!("{{k: {j}, v: 'item-{i}-{j}', w: {}.25}}", j % 13)).collect();
        format!("{{_id: {i}, kind: 'k{}', blob: '{}', items: [{}]}}", i % 4, "x".repeat(32 * 1024), items.join(","))
    };
    let bodies: Vec<String> = (0..big).map(big_doc).collect();
    println!("large document size: {} bytes of JSON", bodies[0].len());
    time("large doc insert (~64 KiB each)", big, || {
        for b in &bodies {
            db.execute(&format!("db.big.insert({b})")).unwrap();
        }
    });
    time("large doc _id lookup", 2_000, || {
        for i in 0..2_000 {
            assert_eq!(docs(&mut db, &format!("db.big.find({{_id: {}}})", i % big)).len(), 1);
        }
    });
    time("large doc scan on top-level field", 20, || {
        for _ in 0..20 {
            assert_eq!(docs(&mut db, "db.big.find({kind: 'k1'})").len(), big / 4);
        }
    });
    time("large doc $set one field by _id", 500, || {
        for i in 0..500 {
            db.execute(&format!("db.big.updateOne({{_id: {}}}, {{$set: {{kind: 'k{}'}}}})", i % big, i % 4)).unwrap();
        }
    });

    // Durable path: WAL framing and fsync-free commit of bulk inserts.
    let dir = std::env::temp_dir().join(format!("polydb-docbench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut ddb = Database::open(&dir).unwrap();
        ddb.set_sync(false);
        ddb.execute("db.users.createIndex({city: 1})").unwrap();
        time("durable insertMany (no fsync, batches of 500)", n, || {
            for chunk in (0..n).collect::<Vec<_>>().chunks(500) {
                let body: Vec<String> = chunk.iter().map(|&i| user_json(i)).collect();
                ddb.execute(&format!("db.users.insertMany([{}])", body.join(","))).unwrap();
            }
        });
    }
    time("reopen (WAL replay)", 5, || {
        for _ in 0..5 {
            let mut ddb = Database::open(&dir).unwrap();
            assert_eq!(docs(&mut ddb, "db.users.find({city: 'c7'})").len(), per_city);
        }
    });
    let _ = std::fs::remove_dir_all(&dir);
}
