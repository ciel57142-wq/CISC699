//! Document engine: CRUD, nested paths, arrays, missing/null, projection,
//! sorting, pagination, isolation, persistence, and aggregation edge cases.

mod common;
use common::*;
use polydb::{Database, Error, Value};

fn j(s: &str) -> Value {
    Value::parse(s).unwrap()
}

fn find(db: &mut Database, q: &str) -> Vec<Value> {
    rows(db, q).into_iter().map(|r| r[0].clone()).collect()
}

fn agg(db: &mut Database, coll: &str, p: &str) -> Result<Vec<Value>, Error> {
    let Value::Array(stages) = j(p) else { panic!() };
    db.doc_aggregate(coll, &stages)
}

#[test]
fn crud_one_and_many() {
    let mut db = Database::in_memory();
    ok(&mut db, "db.c.insertOne({_id: 1, a: 1})");
    ok(
        &mut db,
        "db.c.insertMany([{_id: 2, a: 2}, {_id: 3, a: 3}, {_id: 4, a: 3}])",
    );
    assert!(matches!(
        db.execute("db.c.insert({_id: 1})"),
        Err(Error::Constraint(_))
    ));
    assert_eq!(
        find(&mut db, "db.c.findOne({a: 3})"),
        vec![j("{_id: 3, a: 3}")]
    );
    ok(&mut db, "db.c.updateOne({a: 3}, {$set: {b: true}})");
    assert_eq!(find(&mut db, "db.c.find({b: true})").len(), 1);
    ok(&mut db, "db.c.updateMany({a: {$gte: 2}}, {$inc: {a: 10}})");
    assert_eq!(find(&mut db, "db.c.find({a: {$gt: 10}})").len(), 3);
    ok(&mut db, "db.c.deleteOne({a: {$gt: 10}})");
    assert_eq!(find(&mut db, "db.c.find({})").len(), 3);
    ok(&mut db, "db.c.deleteMany({a: {$gt: 10}})");
    assert_eq!(find(&mut db, "db.c.find({})"), vec![j("{_id: 1, a: 1}")]);
    assert!(matches!(
        db.execute("db.c.update({_id: 1}, {$set: {_id: 9}})"),
        Err(Error::Constraint(_))
    ));
    ok(&mut db, "db.c.drop()");
    assert_eq!(rows(&mut db, "SHOW COLLECTIONS"), Vec::<Vec<Value>>::new());
}

#[test]
fn nested_paths_arrays_missing_and_null() {
    let mut db = Database::in_memory();
    ok(&mut db, "db.p.insert([{_id: 1, n: {a: {b: 1}}, tags: ['x', 'y']}, {_id: 2, n: {a: null}}, {_id: 3, tags: []}, {_id: 4, n: {a: {b: 2}}, tags: ['y']}])");
    assert_eq!(find(&mut db, "db.p.find({'n.a.b': 1})").len(), 1);
    assert_eq!(
        find(&mut db, "db.p.find({'n.a': null})").len(),
        2,
        "null matches missing and explicit null"
    );
    assert_eq!(
        find(&mut db, "db.p.find({'n.a.b': {$exists: false}})").len(),
        2
    );
    assert_eq!(
        find(&mut db, "db.p.find({tags: 'y'})").len(),
        2,
        "array element match"
    );
    assert_eq!(find(&mut db, "db.p.find({'tags.0': 'x'})").len(), 1);
    assert_eq!(
        find(&mut db, "db.p.find({tags: {$in: ['x', 'z']}})").len(),
        1
    );
    ok(
        &mut db,
        "db.p.update({_id: 3}, {$push: {tags: 'z'}, $set: {'n.a.b': 5}})",
    );
    assert_eq!(
        find(&mut db, "db.p.find({_id: 3})"),
        vec![j("{_id: 3, tags: ['z'], n: {a: {b: 5}}}")]
    );
    ok(
        &mut db,
        "db.p.update({_id: 1}, {$pull: {tags: 'x'}, $unset: {'n.a.b': 1}})",
    );
    assert_eq!(
        find(&mut db, "db.p.find({_id: 1})"),
        vec![j("{_id: 1, tags: ['y'], n: {a: {}}}")]
    );
}

#[test]
fn projection_sort_pagination() {
    let mut db = Database::in_memory();
    for i in 0..30 {
        ok(
            &mut db,
            &format!(
                "db.s.insert({{_id: {i}, g: {}, v: {}, x: 'x{i}'}})",
                i % 3,
                30 - i
            ),
        );
    }
    let page = |db: &mut Database, skip: usize| {
        find(
            db,
            &format!("db.s.find({{g: 1}}).sort({{v: 1}}).skip({skip}).limit(4)"),
        )
    };
    let all: Vec<Value> = (0..3).flat_map(|p| page(&mut db, p * 4)).collect();
    let vs: Vec<i64> = all
        .iter()
        .map(|d| d.get("v").unwrap().as_i64().unwrap())
        .collect();
    assert!(
        vs.windows(2).all(|w| w[0] < w[1]) && vs.len() == 10,
        "{vs:?}"
    );
    assert_eq!(page(&mut db, 40), Vec::<Value>::new());
    // same pages through an index
    ok(&mut db, "db.s.createIndex({g: 1, v: 1})");
    let again: Vec<Value> = (0..3).flat_map(|p| page(&mut db, p * 4)).collect();
    assert_eq!(again, all);
    // projection is applied by the Mongo protocol; PBSQL projects fields
    assert_eq!(
        rows(&mut db, "SELECT x, v FROM DOCUMENT.s WHERE _id = 5"),
        vec![vec![j("'x5'"), Value::Int(25)]]
    );
}

#[test]
fn database_and_collection_isolation_and_persistence() {
    let dir = tmp("docs-persist");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "db.users.insert({_id: 1, n: 'default'})");
        ok(&mut db, "db.app.users.insert({_id: 1, n: 'app'})");
        ok(&mut db, "db.other.users.insert({_id: 1, n: 'other'})");
        ok(&mut db, "db.app.users.createIndex({n: 1}, {unique: true})");
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(
        find(&mut db, "db.users.find({})"),
        vec![j("{_id: 1, n: 'default'}")]
    );
    assert_eq!(
        find(&mut db, "db.app.users.find({})"),
        vec![j("{_id: 1, n: 'app'}")]
    );
    assert_eq!(
        rows(&mut db, "SELECT n FROM DOCUMENT.other.users"),
        vec![vec![j("'other'")]]
    );
    assert!(
        matches!(
            db.execute("db.app.users.insert({_id: 2, n: 'app'})"),
            Err(Error::Constraint(_))
        ),
        "unique index persisted"
    );
    ok(&mut db, "db.users.insert({_id: 2, n: 'app'})"); // a different collection: no conflict
    assert!(matches!(
        db.execute("db.bad-name.insert({})"),
        Err(Error::Parse(_)) | Err(Error::Schema(_))
    ));
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn aggregation_edge_cases() {
    let mut db = Database::in_memory();
    ok(&mut db, "db.m.insert([{_id: 1, k: 'a', v: 1}, {_id: 2, k: 'a', v: 2.5}, {_id: 3, k: 'b', v: 'text'}, {_id: 4, k: 'b'}, {_id: 5, v: null, arr: [1, 2, 3]}, {_id: 6, k: 'a', v: [4]}])");
    // mixed types: $sum ignores non-numbers, $min/$max compare across types in BSON order
    let out = agg(&mut db, "m", "[{$group: {_id: '$k', s: {$sum: '$v'}, mn: {$min: '$v'}, mx: {$max: '$v'}, n: {$sum: 1}}}, {$sort: {_id: 1}}]").unwrap();
    assert_eq!(out[0], j("{_id: null, s: 0, mn: null, mx: null, n: 1}"));
    assert_eq!(out[1].get("s"), Some(&Value::Float(3.5)));
    assert_eq!(out[1].get("mx"), Some(&j("[4]")));
    assert_eq!(out[2].get("mn"), Some(&j("'text'")));
    // $unwind over a missing field drops the document; preserveNullAndEmptyArrays keeps it
    assert_eq!(agg(&mut db, "m", "[{$unwind: '$arr'}]").unwrap().len(), 3);
    assert_eq!(
        agg(
            &mut db,
            "m",
            "[{$unwind: {path: '$arr', preserveNullAndEmptyArrays: true}}]"
        )
        .unwrap()
        .len(),
        8
    );
    // a long pipeline of consecutive stages
    let mut stages = vec!["{$match: {}}".to_string()];
    for i in 0..30 {
        stages.push(format!("{{$addFields: {{f{i}: {{$add: ['$_id', {i}]}}}}}}"));
    }
    stages.push("{$project: {f29: 1, _id: 0}}".into());
    stages.push("{$sort: {f29: -1}}".into());
    stages.push("{$limit: 2}".into());
    let out = agg(&mut db, "m", &format!("[{}]", stages.join(", "))).unwrap();
    assert_eq!(out, vec![j("{f29: 35}"), j("{f29: 34}")]);
    // invalid pipelines are errors
    for bad in [
        "[{$group: {total: {$sum: 1}}}]",
        "[{$nope: 1}]",
        "[{$limit: -1}]",
        "[{$sort: 1}]",
        "[{$unwind: 'arr'}]",
        "[{$group: {_id: 1, x: {$median: '$v'}}}]",
        "[{a: 1, b: 2}]",
    ] {
        assert!(agg(&mut db, "m", bad).is_err(), "{bad} should fail");
    }
    // $lookup stays inside the database namespace
    ok(&mut db, "db.shop.items.insert([{_id: 1, sku: 'x'}])");
    ok(&mut db, "db.shop.orders.insert([{_id: 1, sku: 'x'}])");
    ok(&mut db, "db.items.insert([{_id: 9, sku: 'x'}])");
    let out = agg(
        &mut db,
        "shop.orders",
        "[{$lookup: {from: 'shop.items', localField: 'sku', foreignField: 'sku', as: 'it'}}]",
    )
    .unwrap();
    assert_eq!(out[0].get("it"), Some(&j("[{_id: 1, sku: 'x'}]")));
    // empty input
    assert_eq!(
        agg(&mut db, "nothing", "[{$group: {_id: '$k', n: {$sum: 1}}}]").unwrap(),
        Vec::<Value>::new()
    );
    assert_eq!(
        agg(&mut db, "nothing", "[{$count: 'n'}]").unwrap(),
        Vec::<Value>::new()
    );
}

#[test]
fn aggregation_on_generated_orders_matches_pbsql_grouping() {
    let mut db = Database::in_memory();
    load_business(&mut db, dataset_size().min(3000), seed());
    let out = agg(&mut db, "orders", "[{$match: {status: {$ne: 'returned'}}}, {$group: {_id: '$status', n: {$sum: 1}, avg: {$avg: '$total'}}}, {$sort: {_id: 1}}]").unwrap();
    let sql = rows(&mut db, "SELECT status, COUNT(*), AVG(total) FROM DOCUMENT.orders WHERE status <> 'returned' GROUP BY status ORDER BY status");
    assert_eq!(out.len(), sql.len());
    for (d, r) in out.iter().zip(&sql) {
        assert_eq!(d.get("_id"), Some(&r[0]));
        assert_eq!(d.get("n"), Some(&r[1]));
        let (a, b) = (
            d.get("avg").and_then(Value::as_f64).unwrap(),
            r[2].as_f64().unwrap(),
        );
        assert!((a - b).abs() < 1e-9, "{a} vs {b}");
    }
}
