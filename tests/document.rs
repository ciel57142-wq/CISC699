//! Document engine: query planning, index maintenance, aggregation,
//! storage layout and crash consistency.

use polydb::{Database, Error, FindOptions, Output, Value};
use std::fs;
use std::path::PathBuf;

fn v(json: &str) -> Value {
    Value::parse(json).unwrap_or_else(|e| panic!("{json}: {e}"))
}

fn docs(db: &mut Database, cmd: &str) -> Vec<Value> {
    match db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")) {
        Output::Docs(d) => d,
        other => panic!("{cmd}: expected documents, got {other:?}"),
    }
}

fn explain(db: &mut Database, coll: &str, filter: &str, sort: Option<&str>) -> Value {
    let opts = FindOptions { sort: sort.map(v), ..Default::default() };
    db.doc_explain(coll, &v(filter), &opts).unwrap()
}

fn access(p: &Value) -> &str {
    p.get("access").and_then(Value::as_str).unwrap()
}

fn assert_valid(db: &mut Database, coll: &str) {
    let r = db.doc_validate(coll).unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(true)), "validate({coll}) = {}", r.to_json());
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("polydb-doctest-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

/// Small deterministic PRNG (xorshift64*), so failures reproduce.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

// Values chosen to hit the edges of the index encoding: -0.0 vs 0.0, 1 vs
// 1.0, integers above 2^53 that share a float encoding, NUL bytes in text,
// mixed types, arrays, objects, null and missing fields.
const SCALARS: &[&str] = &[
    "0", "-0.0", "0.0", "1", "1.0", "2", "-3", "2.5", "9007199254740992", "9007199254740993", "'a'", "'b'", "'ab'",
    "'a\\u0000b'", "''", "true", "false",
];
const A_VALUES: &[&str] = &[
    "0", "-0.0", "1", "1.0", "2", "-3", "2.5", "9007199254740992", "9007199254740993", "'a'", "'b'", "'ab'",
    "'a\\u0000b'", "true", "null", "[1, 2]", "['a', 'b', 'a']", "[]", "[null]", "{x: 1}", "MISSING",
];
const B_VALUES: &[&str] =
    &["0", "-0.0", "0.0", "1", "1.0", "7", "9007199254740992", "9007199254740993", "'x'", "'y'", "true"];

fn random_doc(r: &mut Rng, id: usize) -> String {
    let mut fields = vec![format!("_id: {id}")];
    let a = r.pick(A_VALUES);
    if a != "MISSING" {
        fields.push(format!("a: {a}"));
    }
    fields.push(format!("b: {}", r.pick(B_VALUES))); // always a scalar: its index is total
    if r.below(4) > 0 {
        fields.push(format!("n: {{x: {}, y: 'k{}'}}", r.pick(SCALARS), r.below(3)));
    }
    if r.below(2) == 0 {
        fields.push(format!("tags: ['t{}', 't{}']", r.below(4), r.below(4)));
    }
    format!("{{{}}}", fields.join(", "))
}

fn random_cond(r: &mut Rng) -> String {
    let s = r.pick(SCALARS);
    let t = r.pick(SCALARS);
    match r.below(11) {
        0 => s.to_string(),
        1 => format!("{{$gt: {s}}}"),
        2 => format!("{{$gte: {s}, $lt: {t}}}"),
        3 => format!("{{$lte: {s}}}"),
        4 => format!("{{$in: [{s}, {t}]}}"),
        5 => "null".to_string(),
        6 => format!("{{$exists: {}}}", r.below(2) == 0),
        7 => format!("{{$ne: {s}}}"),
        8 => format!("{{$eq: {s}}}"),
        9 => format!("{{$in: [{s}, null]}}"),
        _ => format!("{{$not: {{$gt: {s}}}}}"),
    }
}

fn random_filter(r: &mut Rng) -> String {
    let field = |r: &mut Rng| r.pick(&["a", "b", "n.x", "tags", "_id", "n.y"]).to_string();
    let one = |r: &mut Rng| {
        let f = field(r);
        let cond = match f.as_str() {
            "tags" => format!("'t{}'", r.below(5)),
            "_id" => match r.below(4) {
                0 => format!("{}", r.below(70)),
                1 => format!("{{$gte: {}}}", r.below(70)),
                2 => format!("{{$in: [{}, {}, 'zz']}}", r.below(70), r.below(70)),
                _ => format!("{{$gt: {}, $lte: {}}}", r.below(40), r.below(70)),
            },
            "n.y" => format!("'k{}'", r.below(3)),
            _ => random_cond(r),
        };
        format!("\"{f}\": {cond}")
    };
    match r.below(6) {
        0 => "{}".to_string(),
        1 | 2 => format!("{{{}}}", one(r)),
        3 => format!("{{{}, {}}}", one(r), one(r)),
        4 => format!("{{$and: [{{{}}}, {{{}}}]}}", one(r), one(r)),
        _ => format!("{{$or: [{{{}}}, {{{}}}]}}", one(r), one(r)),
    }
}

fn find(db: &mut Database, coll: &str, filter: &str, opts: &FindOptions) -> Result<Vec<Value>, Error> {
    db.doc_find(coll, &v(filter), opts)
}

/// Every query returns exactly the same documents, in the same order, with
/// indexes as without: indexes may only change speed, never results.
#[test]
fn indexed_and_unindexed_queries_agree() {
    let seeds: Vec<u64> = match std::env::var("POLYDB_DIFF_SEEDS") {
        Ok(n) => (1..=n.parse::<u64>().unwrap()).map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).collect(),
        Err(_) => vec![0x5eed_1234_abcd_0001, 2, 0xdead_beef, 77],
    };
    for seed in seeds {
        differential(seed);
    }
}

fn differential(seed: u64) {
    let mut db = Database::in_memory();
    for f in ["a", "b", "n.x", "tags"] {
        db.doc_create_index("ix", f).unwrap();
    }
    let mut r = Rng(seed);
    let body: Vec<String> = (0..70).map(|i| random_doc(&mut r, i)).collect();
    for coll in ["ix", "plain"] {
        db.execute(&format!("db.{coll}.insertMany([{}])", body.join(","))).unwrap();
    }
    assert_valid(&mut db, "ix");
    let sorts = [None, Some("{a: 1}"), Some("{a: -1}"), Some("{b: 1}"), Some("{b: -1}"), Some("{b: 1, a: -1}"), Some("{_id: -1}"), Some("{'n.x': 1}")];
    let mut used_index = 0;
    let mut sorted_by_index = 0;
    for round in 0..3 {
        for _ in 0..600 {
            let filter = random_filter(&mut r);
            let opts = FindOptions {
                sort: sorts[r.below(sorts.len())].map(v),
                skip: [0, 0, 1, 3][r.below(4)],
                limit: [None, None, Some(1), Some(5)][r.below(4)],
                projection: None,
            };
            let a = find(&mut db, "ix", &filter, &opts);
            let b = find(&mut db, "plain", &filter, &opts);
            match (&a, &b) {
                (Ok(x), Ok(y)) => assert_eq!(x, y, "seed {seed} round {round}: filter {filter}, sort {:?}, skip {}, limit {:?}", opts.sort.as_ref().map(|s| s.to_json()), opts.skip, opts.limit),
                _ => assert_eq!(a.is_ok(), b.is_ok(), "{filter}: {a:?} vs {b:?}"),
            }
            assert_eq!(db.doc_count("ix", &v(&filter)).ok(), db.doc_count("plain", &v(&filter)).ok(), "count {filter}");
            let p = db.doc_explain("ix", &v(&filter), &opts).unwrap();
            used_index += (access(&p) == "index_scan") as usize;
            sorted_by_index += (access(&p) == "index_scan" && p.get("sort") == Some(&v("'from_access'"))) as usize;
        }
        // Mutate both collections identically, then compare again.
        for _ in 0..40 {
            let filter = random_filter(&mut r);
            let upd = match r.below(5) {
                0 => format!("{{$set: {{a: {}}}}}", r.pick(A_VALUES).replace("MISSING", "null")),
                1 => format!("{{$set: {{'n.x': {}}}}}", r.pick(SCALARS)),
                2 => "{$unset: {a: 1}}".to_string(),
                3 => format!("{{$push: {{tags: 't{}'}}}}", r.below(5)),
                _ => format!("{{$set: {{b: {}}}}}", r.pick(B_VALUES)),
            };
            let multi = r.below(2) == 0;
            let x = db.doc_update_with("ix", &v(&filter), &v(&upd), multi, false);
            let y = db.doc_update_with("plain", &v(&filter), &v(&upd), multi, false);
            assert_eq!(x.as_ref().ok(), y.as_ref().ok(), "update {filter} {upd}");
        }
        for _ in 0..5 {
            let filter = random_filter(&mut r);
            let limit = [None, Some(1)][r.below(2)];
            assert_eq!(
                db.doc_delete_with("ix", &v(&filter), limit).ok(),
                db.doc_delete_with("plain", &v(&filter), limit).ok(),
                "delete {filter}"
            );
        }
        let n = db.doc_count("ix", &v("{}")).unwrap();
        let more: Vec<String> = (0..10).map(|i| random_doc(&mut r, 1000 + round * 100 + i)).collect();
        for coll in ["ix", "plain"] {
            db.execute(&format!("db.{coll}.insertMany([{}])", more.join(","))).unwrap();
        }
        assert_eq!(db.doc_count("ix", &v("{}")).unwrap(), n + 10);
        assert_valid(&mut db, "ix");
    }
    // the planner really exercised the index paths
    assert!(used_index > 500, "only {used_index} queries used an index");
    assert!(sorted_by_index > 30, "only {sorted_by_index} queries were sorted by an index");
}

#[test]
fn planner_picks_the_expected_access_path() {
    let mut db = Database::in_memory();
    db.execute("db.p.createIndex({age: 1})").unwrap();
    db.execute("db.p.createIndex({city: 1})").unwrap();
    db.execute("db.p.createIndex({'addr.zip': 1})").unwrap();
    for i in 0..50 {
        db.execute(&format!("db.p.insert({{_id: {i}, age: {}, city: 'c{}', addr: {{zip: 'z{}'}}, x: {i}}})", i % 10, i % 5, i % 7))
            .unwrap();
    }
    let cases = [
        ("{_id: 3}", None, "id_lookup"),
        ("{_id: {$in: [1, 2, 99]}}", None, "id_lookup"),
        ("{_id: {$gte: 10, $lt: 20}}", None, "id_range"),
        ("{age: 3}", None, "index_scan"),
        ("{age: {$gt: 3, $lte: 7}}", None, "index_scan"),
        ("{city: {$in: ['c1', 'c2']}}", None, "index_scan"),
        ("{'addr.zip': 'z3'}", None, "index_scan"),
        ("{age: null}", None, "index_scan"),
        ("{age: {$exists: false}}", None, "index_scan"),
        ("{x: 5}", None, "collection_scan"),
        ("{age: {$ne: 3}}", None, "collection_scan"),
        ("{$or: [{age: 1}, {city: 'c2'}]}", None, "collection_scan"),
        ("{x: {$gt: 5}}", Some("{age: -1}"), "index_scan"),
        ("{}", Some("{_id: -1}"), "collection_scan"),
    ];
    for (filter, sort, want) in cases {
        let p = explain(&mut db, "p", filter, sort);
        assert_eq!(access(&p), want, "{filter} sort {sort:?}: {}", p.to_json());
    }
    // equality beats a range; _id beats any secondary index
    let p = explain(&mut db, "p", "{age: {$gt: 1}, city: 'c1'}", None);
    assert_eq!(p.get("index"), Some(&v("'city'")));
    assert_eq!(access(&explain(&mut db, "p", "{_id: 4, city: 'c1'}", None)), "id_lookup");
    // sort from the index, also in reverse; the full spec is honoured.
    // (A sort spec is an object, so its keys apply in alphabetical order.)
    let p = explain(&mut db, "p", "{}", Some("{age: -1, x: 1}"));
    assert_eq!((p.get("sort"), p.get("reverse")), (Some(&v("'from_access'")), Some(&Value::Bool(true))));
    let got = docs(&mut db, "db.p.find({}).sort({age: -1, x: 1}).limit(6)");
    let ids: Vec<i64> = got.iter().map(|d| d.get("_id").unwrap().as_i64().unwrap()).collect();
    assert_eq!(ids, vec![9, 19, 29, 39, 49, 8]);
    // once a document lacks the field, the index no longer holds every
    // document and the sort falls back to memory (missing sorts first)
    db.execute("db.p.insert({_id: 100})").unwrap();
    let p = explain(&mut db, "p", "{}", Some("{age: 1}"));
    assert_eq!((access(&p), p.get("sort")), ("collection_scan", Some(&v("'in_memory'"))));
    assert_eq!(docs(&mut db, "db.p.find({}).sort({age: 1}).limit(1)")[0].get("_id"), Some(&Value::Int(100)));
    // explain() is also a find() modifier
    assert_eq!(access(&docs(&mut db, "db.p.find({age: 2}).explain()")[0]), "index_scan");
}

#[test]
fn updates_and_deletes_keep_every_index_exact() {
    let mut db = Database::in_memory();
    db.execute("db.u.createIndex({k: 1})").unwrap();
    db.execute("db.u.createIndex({'m.n': 1})").unwrap();
    db.execute("db.u.createIndex({arr: 1})").unwrap();
    db.execute("db.u.insertMany([{_id: 1, k: 'a', m: {n: 1}, arr: [1, 2]}, {_id: 2, k: 'b', arr: []}, {_id: 3, m: {n: 3}, arr: 5}])")
        .unwrap();
    assert_valid(&mut db, "u");
    let steps = [
        "db.u.updateOne({_id: 1}, {$set: {k: 'z'}})",
        "db.u.updateMany({}, {$inc: {'m.n': 1}})",
        "db.u.update({_id: 2}, {$push: {arr: 9}})",
        "db.u.update({_id: 1}, {$pull: {arr: 1}})",
        "db.u.update({_id: 3}, {$unset: {m: 1}})",
        "db.u.update({_id: 2}, {other: 'replaced'})",
        "db.u.update({k: 'z'}, {$set: {arr: [2, 2, 3], k: null}})",
        "db.u.deleteOne({arr: 3})",
    ];
    for s in steps {
        db.execute(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_valid(&mut db, "u");
    }
    assert_eq!(docs(&mut db, "db.u.find({arr: 2})").len(), 0);
    assert_eq!(docs(&mut db, "db.u.find({arr: 9})").len(), 0, "_id 2 was replaced, its arr is gone");
    assert_eq!(docs(&mut db, "db.u.find({k: null})").len(), 2);
    assert_eq!(docs(&mut db, "db.u.find({'m.n': {$exists: false}})").len(), 2);
    // a failed update (changing _id) leaves documents and indexes untouched
    assert!(db.execute("db.u.updateMany({}, {$set: {_id: 7}})").is_err());
    assert_valid(&mut db, "u");
    // creating an index over existing documents, and reIndex
    db.execute("db.u.createIndex({other: 1})").unwrap();
    assert_valid(&mut db, "u");
    assert!(matches!(db.execute("db.u.reIndex()").unwrap(), Output::Done(m) if m.starts_with("4 index")));
    assert_valid(&mut db, "u");
    db.execute("db.u.drop()").unwrap();
    assert_eq!(db.entry_count(), 0);
}

#[test]
fn transactions_see_index_reads_as_reads() {
    let mut a = Database::in_memory();
    let mut b = a.session();
    a.execute("db.t.createIndex({k: 1})").unwrap();
    a.execute("db.t.insert({_id: 1, k: 1})").unwrap();
    a.begin().unwrap();
    assert_eq!(docs(&mut a, "db.t.find({k: {$gte: 1}})").len(), 1);
    b.execute("db.t.insert({_id: 2, k: 5})").unwrap(); // lands in the range a read
    a.execute("SET marker 1").unwrap();
    assert!(matches!(a.commit(), Err(Error::Conflict(_))));
    // inside a transaction, uncommitted writes are visible to index scans and sorts
    a.begin().unwrap();
    a.execute("db.t.insert({_id: 3, k: 3})").unwrap();
    a.execute("db.t.deleteOne({_id: 2})").unwrap();
    let got = docs(&mut a, "db.t.find({k: {$gt: 0}}).sort({k: -1})");
    assert_eq!(got.iter().map(|d| d.get("_id").cloned().unwrap()).collect::<Vec<_>>(), vec![Value::Int(3), Value::Int(1)]);
    a.rollback().unwrap();
    assert_eq!(docs(&mut a, "db.t.find({k: {$gt: 0}})").len(), 2);
}

#[test]
fn aggregation_pipeline_stages() {
    let mut db = Database::in_memory();
    db.execute("db.s.createIndex({region: 1})").unwrap();
    db.execute(
        "db.s.insertMany([
        {_id: 1, region: 'eu', item: 'a', qty: 2, price: 10.0, tags: ['x', 'y']},
        {_id: 2, region: 'eu', item: 'b', qty: 1, price: 4.5, tags: ['y']},
        {_id: 3, region: 'us', item: 'a', qty: 5, price: 10.0, tags: []},
        {_id: 4, region: 'us', item: 'c', qty: 3, price: 1.0},
        {_id: 5, region: 'apac', item: 'a', qty: 4, price: 9.0, tags: ['x']}])",
    )
    .unwrap();
    let agg = |db: &mut Database, p: &str| docs(db, &format!("db.s.aggregate({p})"));

    let g = agg(
        &mut db,
        "[{$group: {_id: '$region', n: {$sum: 1}, qty: {$sum: '$qty'}, avg: {$avg: '$price'}, lo: {$min: '$price'}, hi: {$max: '$qty'},
           first: {$first: '$item'}, last: {$last: '$item'}, items: {$addToSet: '$item'}, all: {$push: '$qty'}, c: {$count: {}}}},
          {$sort: {qty: -1}}]",
    );
    assert_eq!(g.len(), 3);
    assert_eq!(
        g[0],
        v("{_id: 'us', n: 2, qty: 8, avg: 5.5, lo: 1.0, hi: 5, first: 'a', last: 'c', items: ['a', 'c'], all: [5, 3], c: 2}")
    );
    assert_eq!(g[2].get("_id"), Some(&v("'eu'")));
    // $match pushdown uses the index; $count; whole-set groups
    assert_eq!(agg(&mut db, "[{$match: {region: 'eu'}}, {$count: 'n'}]"), vec![v("{n: 2}")]);
    assert_eq!(agg(&mut db, "[{$match: {region: 'none'}}, {$count: 'n'}]"), vec![]);
    assert_eq!(agg(&mut db, "[{$match: {qty: {$gte: 3}}}, {$group: {_id: null, total: {$sum: '$qty'}}}]"), vec![v("{_id: null, total: 12}")]);
    assert_eq!(agg(&mut db, "[{$group: {_id: 1, n: {$sum: 1}}}]"), vec![v("{_id: 1, n: 5}")]);
    // compound group keys; 1 and 1.0 group together
    let g = agg(&mut db, "[{$group: {_id: {r: '$region', i: '$item'}, n: {$sum: 1}}}, {$sort: {'_id.r': 1, '_id.i': 1}}, {$limit: 2}]");
    assert_eq!(g, vec![v("{_id: {r: 'apac', i: 'a'}, n: 1}"), v("{_id: {r: 'eu', i: 'a'}, n: 1}")]);
    // sort / skip / limit pushed into the query, then $project
    let p = agg(&mut db, "[{$sort: {qty: -1}}, {$skip: 1}, {$limit: 2}, {$project: {item: 1, _id: 0}}]");
    assert_eq!(p, vec![v("{item: 'a'}"), v("{item: 'c'}")]);
    // $unwind, then group on the unwound value
    let u = agg(&mut db, "[{$unwind: '$tags'}, {$group: {_id: '$tags', n: {$sum: 1}}}, {$sort: {_id: 1}}]");
    assert_eq!(u, vec![v("{_id: 'x', n: 2}"), v("{_id: 'y', n: 2}")]);
    let u = agg(&mut db, "[{$unwind: {path: '$tags', preserveNullAndEmptyArrays: true}}, {$count: 'n'}]");
    assert_eq!(u, vec![v("{n: 6}")]);
    // computed fields
    let c = agg(&mut db, "[{$match: {_id: 2}}, {$addFields: {where: '$region', 'meta.q': '$qty', k: {$literal: '$x'}}}, {$project: {where: 1, meta: 1, k: 1}}]");
    assert_eq!(c, vec![v("{_id: 2, where: 'eu', meta: {q: 1}, k: '$x'}")]);
    let c = agg(&mut db, "[{$match: {_id: 1}}, {$project: {_id: 0, region: 1, total: '$qty'}}]");
    assert_eq!(c, vec![v("{region: 'eu', total: 2}")]);
    // stage order matters: $limit before $sort
    let s = agg(&mut db, "[{$limit: 2}, {$sort: {qty: 1}}]");
    assert_eq!(s.iter().map(|d| d.get("_id").cloned().unwrap()).collect::<Vec<_>>(), vec![Value::Int(2), Value::Int(1)]);
    // errors are typed
    for bad in ["[{$bogus: 1}]", "[{$group: {n: {$sum: 1}}}]", "[{$group: {_id: 1, n: {$median: '$x'}}}]", "[{$limit: -1}]", "[{$match: {$where: 1}}]"] {
        assert!(db.execute(&format!("db.s.aggregate({bad})")).is_err(), "{bad} should fail");
    }
}

#[test]
fn projection_nested_and_large_documents() {
    let mut db = Database::in_memory();
    db.execute("db.p.insert({_id: 1, a: 1, b: {c: [10, {d: 'deep'}]}, e: 'x'})").unwrap();
    assert_eq!(docs(&mut db, "db.p.find({}, {a: 1})"), vec![v("{_id: 1, a: 1}")]);
    assert_eq!(docs(&mut db, "db.p.find({}, {a: 1, _id: 0})"), vec![v("{a: 1}")]);
    assert_eq!(docs(&mut db, "db.p.find({}, {b: 0, e: 0})"), vec![v("{_id: 1, a: 1}")]);
    assert_eq!(docs(&mut db, "db.p.find({}, {_id: 1})"), vec![v("{_id: 1}")]);
    assert_eq!(docs(&mut db, "db.p.find({}).projection({e: 1})"), vec![v("{_id: 1, e: 'x'}")]);
    assert_eq!(docs(&mut db, "db.p.find({'b.c.1.d': 'deep'})").len(), 1);
    assert_eq!(docs(&mut db, "db.p.find({'b.c.0': {$gt: 5}})").len(), 1);
    assert_eq!(docs(&mut db, "db.p.find({'b.c.d': 'deep'})").len(), 0, "array parts are indexes, as before");

    // a ~1 MiB document with 20,000 array elements round-trips exactly
    let items: Vec<String> = (0..20_000).map(|i| format!("{{i: {i}, s: 'v{i}', f: {i}.5}}")).collect();
    let big = format!("{{_id: 'big', blob: '{}', items: [{}], tail: {{last: true}}}}", "z".repeat(512 * 1024), items.join(","));
    let parsed = v(&big);
    db.doc_insert("big", parsed.clone()).unwrap();
    db.doc_create_index("big", "tail.last").unwrap();
    assert_eq!(docs(&mut db, "db.big.find({_id: 'big'})"), vec![parsed.clone()]);
    assert_eq!(docs(&mut db, "db.big.find({'items.19999.s': 'v19999'})").len(), 1);
    assert_eq!(docs(&mut db, "db.big.find({'tail.last': true}, {tail: 1})"), vec![v("{_id: 'big', tail: {last: true}}")]);
    db.execute("db.big.update({_id: 'big'}, {$set: {'items.0.s': 'changed'}})").unwrap();
    assert_eq!(docs(&mut db, "db.big.find({'items.0.s': 'changed'})").len(), 1);
    assert_valid(&mut db, "big");
}

// ----------------------------------------------------------- durability

/// CRC-32 (IEEE), to write WAL records the way older versions did.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn wal_record(ops: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut p = (ops.len() as u32).to_le_bytes().to_vec();
    for (k, val) in ops {
        p.push(1);
        p.extend((k.len() as u32).to_le_bytes());
        p.extend(k);
        p.extend((val.len() as u32).to_le_bytes());
        p.extend(val);
    }
    let mut out = (p.len() as u32).to_le_bytes().to_vec();
    out.extend(crc32(&p).to_le_bytes());
    out.extend(p);
    out
}

/// Databases written before the binary layout hold JSON documents and
/// indexes without null entries. They stay readable and correct, updates
/// rewrite documents in the new layout, and reIndex upgrades the indexes.
#[test]
fn legacy_json_documents_and_indexes_still_work() {
    let dir = tempdir("legacy");
    fs::create_dir_all(&dir).unwrap();
    let int_id = |i: i64| {
        let mut k = b"d\0old\0".to_vec();
        k.push(2);
        k.extend(((i as u64) ^ (1 << 63)).to_be_bytes());
        k
    };
    let num_index = |field: &str, n: f64, i: i64| {
        let bits = n.to_bits();
        let mut k = format!("i\0c:old\0{field}\0").into_bytes();
        k.push(2);
        k.extend((if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) }).to_be_bytes());
        k.extend(&int_id(i)[6..]);
        k
    };
    let mut ops = vec![(b"m\0c\0old".to_vec(), br#"{"indexes":["a"]}"#.to_vec())];
    for i in 0..6 {
        ops.push((int_id(i), format!(r#"{{"_id":{i},"a":{},"s":"row{i}"}}"#, i % 3).into_bytes()));
        ops.push((num_index("a", (i % 3) as f64, i), vec![]));
    }
    ops.push((int_id(6), br#"{"_id":6,"s":"no a"}"#.to_vec()));
    fs::write(dir.join("data.wal"), wal_record(&ops)).unwrap();

    let mut db = Database::open(&dir).unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 1})").len(), 2);
    assert_eq!(access(&explain(&mut db, "old", "{a: 1}", None)), "index_scan");
    // the old index has no null entries: null queries and sorts scan instead, and stay correct
    assert_eq!(access(&explain(&mut db, "old", "{a: null}", None)), "collection_scan");
    assert_eq!(docs(&mut db, "db.old.find({a: null})"), vec![v(r#"{"_id":6,"s":"no a"}"#)]);
    assert_eq!(docs(&mut db, "db.old.find({}).sort({a: -1}).limit(1)")[0].get("_id"), Some(&Value::Int(2)));
    // updates rewrite JSON documents in the binary layout and keep the index right
    db.execute("db.old.update({_id: 1}, {$set: {a: 2}})").unwrap();
    db.execute("db.old.insert({_id: 7, a: 0})").unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 2})").len(), 3);
    // reIndex upgrades the index to the form with null entries
    db.execute("db.old.reIndex()").unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(access(&explain(&mut db, "old", "{a: null}", None)), "index_scan");
    assert_eq!(docs(&mut db, "db.old.find({a: null})").len(), 1);
    drop(db);
    // everything survives a reopen
    let mut db = Database::open(&dir).unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 2})").len(), 3);
    let _ = fs::remove_dir_all(&dir);
}

/// A crash can cut the WAL at any byte. Whatever survives, documents and
/// their index entries must agree, and each commit is all-or-nothing.
#[test]
fn torn_wal_at_every_byte_keeps_documents_and_indexes_consistent() {
    let dir = tempdir("torn");
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("db.c.createIndex({k: 1})").unwrap();
        db.execute("db.c.createIndex({'n.m': 1})").unwrap();
        db.execute("db.c.insertMany([{_id: 1, k: 'a', n: {m: 1}}, {_id: 2, k: 'b', n: {m: 2}, tags: [1, 2]}])").unwrap();
        db.checkpoint().unwrap();
        // the commits that will be torn
        db.execute("db.c.updateMany({}, {$set: {k: 'z'}, $inc: {'n.m': 10}})").unwrap();
        db.execute("db.c.insertMany([{_id: 3, k: 'c'}, {_id: 4, k: 'd', n: {m: 4}}])").unwrap();
        db.execute("db.c.deleteOne({_id: 1})").unwrap();
    }
    let wal = fs::read(dir.join("data.wal")).unwrap();
    let snap = fs::read(dir.join("data.snap")).unwrap();
    assert!(wal.len() > 100);
    let states = ["{k: 'a'}", "{k: 'z'}", "{k: 'z', _id: {$gte: 1}}"];
    let mut seen = std::collections::BTreeSet::new();
    for cut in 0..=wal.len() {
        let d = tempdir(&format!("torn-{cut}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("data.snap"), &snap).unwrap();
        fs::write(d.join("data.wal"), &wal[..cut]).unwrap();
        let mut db = Database::open(&d).unwrap();
        assert_valid(&mut db, "c");
        let n = db.doc_count("c", &v("{}")).unwrap();
        let z = db.doc_count("c", &v(states[1])).unwrap();
        let state = match (n, z) {
            (2, 0) => "before",
            (2, 2) => "updated",
            (4, 2) => "inserted",
            (3, 1) => "deleted",
            other => panic!("cut at {cut}: impossible state {other:?}"),
        };
        if state != "before" {
            // the update's index changes are all there with it
            assert_eq!(docs(&mut db, "db.c.find({'n.m': {$gte: 11}})").len(), z, "cut {cut}");
            assert_eq!(docs(&mut db, "db.c.find({k: 'a'})").len(), 0, "cut {cut}");
        }
        seen.insert(state);
        drop(db);
        let _ = fs::remove_dir_all(&d);
    }
    assert_eq!(seen.len(), 4, "every commit boundary was reached: {seen:?}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn bulk_insert_is_one_atomic_statement() {
    let mut db = Database::in_memory();
    db.execute("db.b.createIndex({k: 1})").unwrap();
    let ids = db.doc_insert_many("b", vec![v("{k: 1}"), v("{k: 2}"), v("{_id: 'x', k: 3}")]).unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[2], v("'x'"));
    assert_ne!(ids[0], ids[1], "generated ids are unique within a batch");
    // a duplicate in the middle of a batch fails the whole batch
    let err = db.execute("db.b.insertMany([{_id: 'y', k: 4}, {_id: 'x', k: 5}])").unwrap_err();
    assert!(matches!(err, Error::Constraint(_)), "{err:?}");
    assert_eq!(db.doc_count("b", &v("{}")).unwrap(), 3);
    assert_valid(&mut db, "b");
    // generated ids keep increasing across batches
    let more = db.doc_insert_many("b", vec![v("{k: 6}")]).unwrap();
    assert!(more[0].as_str().unwrap() > ids[1].as_str().unwrap());
    assert!(db.execute("db.b.insertMany([{k: 1}, 5])").is_err(), "non-objects are rejected");
    assert_eq!(db.doc_count("b", &v("{}")).unwrap(), 4);
}
