//! Document storage layout and query execution: the binary layout, filters
//! and sort keys read from stored bytes, top-k sorting, aggregation
//! pushdown, index maintenance on update, legacy JSON databases and crash
//! consistency.

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

fn assert_valid(db: &mut Database, coll: &str) {
    let r = db.doc_validate(coll).unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(true)), "validate({coll}) = {}", r.to_json());
}

fn access(db: &mut Database, coll: &str, filter: &str) -> String {
    db.doc_explain(coll, &v(filter)).unwrap().get("access").and_then(Value::as_str).unwrap().to_string()
}

fn tempdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("polydb-docstore-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    d
}

/// xorshift64*, so failures reproduce from the seed.
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

// Values at the edges of the index encoding and of type ordering.
const SCALARS: &[&str] = &[
    "0", "-0.0", "1", "1.0", "2", "-3", "2.5", "9007199254740992", "9007199254740993", "'a'", "'b'", "'ab'", "''",
    "true", "false",
];
const A_VALUES: &[&str] = &[
    "0", "-0.0", "1", "1.0", "2", "-3", "2.5", "9007199254740992", "9007199254740993", "'a'", "'b'", "'ab'", "true",
    "null", "[1, 2]", "['a', 'b', 'a']", "[]", "[null]", "{x: 1}", "MISSING",
];

fn random_doc(r: &mut Rng, id: usize) -> String {
    let mut f = vec![format!("_id: {id}")];
    let a = r.pick(A_VALUES);
    if a != "MISSING" {
        f.push(format!("a: {a}"));
    }
    f.push(format!("b: {}", r.pick(SCALARS)));
    if r.below(4) > 0 {
        f.push(format!("n: {{x: {}, y: 'k{}'}}", r.pick(SCALARS), r.below(3)));
    }
    if r.below(2) == 0 {
        // array of objects: paths like items.sku fan out over the elements
        let items: Vec<String> = (0..r.below(3)).map(|_| format!("{{sku: {}, q: {}}}", r.pick(SCALARS), r.below(5))).collect();
        f.push(format!("items: [{}]", items.join(", ")));
    }
    format!("{{{}}}", f.join(", "))
}

fn random_filter(r: &mut Rng) -> String {
    let one = |r: &mut Rng| {
        let f = r.pick(&["a", "b", "n.x", "items.sku", "items.q", "_id", "n.y"]);
        let s = r.pick(SCALARS);
        let t = r.pick(SCALARS);
        let cond = match f {
            "_id" => format!("{{$in: [{}, {}]}}", r.below(60), r.below(60)),
            "n.y" => format!("'k{}'", r.below(3)),
            "items.q" => format!("{{$gte: {}}}", r.below(5)),
            _ => match r.below(9) {
                0 => s.to_string(),
                1 => format!("{{$gt: {s}}}"),
                2 => format!("{{$gte: {s}, $lt: {t}}}"),
                3 => format!("{{$in: [{s}, {t}]}}"),
                4 => "null".to_string(),
                5 => format!("{{$exists: {}}}", r.below(2) == 0),
                6 => format!("{{$ne: {s}}}"),
                7 => "{$type: 'array'}".to_string(),
                _ => format!("{{$elemMatch: {{$gte: {s}}}}}"),
            },
        };
        format!("\"{f}\": {cond}")
    };
    match r.below(5) {
        0 => "{}".to_string(),
        1 | 2 => format!("{{{}}}", one(r)),
        3 => format!("{{{}, {}}}", one(r), one(r)),
        _ => format!("{{$or: [{{{}}}, {{{}}}]}}", one(r), one(r)),
    }
}

/// Sort, skip and limit (top-k selection on keys read from stored bytes)
/// give exactly the documents and order of an indexed and an unindexed
/// collection, through updates and deletes.
#[test]
fn sorted_limited_queries_agree_with_and_without_indexes() {
    for seed in [0x5eed_0001u64, 7, 0xdead_beef, 99] {
        let mut db = Database::in_memory();
        for f in ["a", "b", "n.x", "items.sku"] {
            db.doc_create_index("ix", f).unwrap();
        }
        let mut r = Rng(seed);
        let body: Vec<String> = (0..60).map(|i| random_doc(&mut r, i)).collect();
        for coll in ["ix", "plain"] {
            db.execute(&format!("db.{coll}.insertMany([{}])", body.join(","))).unwrap();
        }
        let sorts = [None, Some("{a: 1}"), Some("{a: -1}"), Some("{b: 1, a: -1}"), Some("[['b', -1], ['a', 1]]"), Some("{_id: -1}"), Some("{'n.x': 1}"), Some("{'items.sku': 1}")];
        let mut indexed = 0;
        for round in 0..3 {
            for _ in 0..400 {
                let filter = random_filter(&mut r);
                let opts = FindOptions {
                    sort: sorts[r.below(sorts.len())].map(v),
                    skip: [0, 0, 1, 4][r.below(4)],
                    limit: [None, None, Some(1), Some(3), Some(0)][r.below(5)],
                    projection: None,
                };
                let a = db.doc_find("ix", &v(&filter), &opts);
                let b = db.doc_find("plain", &v(&filter), &opts);
                match (&a, &b) {
                    (Ok(x), Ok(y)) => assert_eq!(x, y, "seed {seed} round {round}: {filter} {:?} skip {} limit {:?}", opts.sort.as_ref().map(|s| s.to_json()), opts.skip, opts.limit),
                    _ => assert_eq!(a.is_ok(), b.is_ok(), "{filter}: {a:?} vs {b:?}"),
                }
                assert_eq!(db.doc_count("ix", &v(&filter)).ok(), db.doc_count("plain", &v(&filter)).ok(), "count {filter}");
                if a.is_ok() {
                    indexed += (access(&mut db, "ix", &filter) == "index_scan") as usize;
                }
            }
            for _ in 0..30 {
                let filter = random_filter(&mut r);
                let upd = match r.below(4) {
                    0 => format!("{{$set: {{a: {}}}}}", r.pick(A_VALUES).replace("MISSING", "null")),
                    1 => format!("{{$set: {{'n.x': {}}}}}", r.pick(SCALARS)),
                    2 => "{$unset: {a: 1}}".to_string(),
                    _ => format!("{{$push: {{items: {{sku: {}, q: 1}}}}}}", r.pick(SCALARS)),
                };
                let multi = r.below(2) == 0;
                let x = db.doc_update_with("ix", &v(&filter), &v(&upd), multi, false);
                let y = db.doc_update_with("plain", &v(&filter), &v(&upd), multi, false);
                assert_eq!(x.as_ref().ok(), y.as_ref().ok(), "update {filter} {upd}");
            }
            let filter = random_filter(&mut r);
            assert_eq!(db.doc_delete_with("ix", &v(&filter), Some(2)).ok(), db.doc_delete_with("plain", &v(&filter), Some(2)).ok());
            assert_valid(&mut db, "ix");
        }
        assert!(indexed > 100, "seed {seed}: only {indexed} queries used an index");
    }
}

/// Pushed-down pipelines ($match/$sort/$skip/$limit into the query, partial
/// decoding for $group/$count) give the same result as running every stage
/// in memory. A leading no-op $project disables the pushdown for the reference.
#[test]
fn aggregation_pushdown_matches_in_memory_pipeline() {
    let mut db = Database::in_memory();
    db.doc_create_index("s", "region").unwrap();
    let mut r = Rng(42);
    let rows: Vec<String> = (0..200)
        .map(|i| {
            format!(
                "{{_id: {i}, region: '{}', qty: {}, price: {}, tags: [{}], meta: {{w: {}, k: '{}'}}}}",
                ["eu", "us", "apac"][r.below(3)],
                r.below(10),
                r.pick(&["1.5", "2", "null", "'x'", "3.25"]),
                (0..r.below(3)).map(|t| format!("'t{t}'")).collect::<Vec<_>>().join(","),
                r.below(4),
                ["a", "b"][r.below(2)]
            )
        })
        .collect();
    db.execute(&format!("db.s.insertMany([{}])", rows.join(","))).unwrap();
    let pipelines = [
        "[{$match: {region: 'eu'}}, {$group: {_id: '$meta.k', n: {$sum: 1}, q: {$sum: '$qty'}, p: {$avg: '$price'}}}]",
        "[{$match: {qty: {$gte: 3}}}, {$sort: {qty: -1, _id: 1}}, {$skip: 2}, {$limit: 7}]",
        "[{$sort: {price: 1}}, {$limit: 5}, {$project: {price: 1}}]",
        "[{$match: {region: {$in: ['us', 'apac']}}}, {$match: {'meta.w': 2}}, {$count: 'n'}]",
        "[{$group: {_id: {r: '$region', w: '$meta.w'}, top: {$max: '$qty'}, all: {$push: '$_id'}, tags: {$addToSet: '$tags'}}}, {$sort: {'_id.r': 1, '_id.w': 1}}]",
        "[{$match: {region: 'us'}}, {$limit: 10}, {$skip: 3}, {$limit: 4}, {$group: {_id: null, f: {$first: '$_id'}, l: {$last: '$_id'}}}]",
        "[{$unwind: '$tags'}, {$group: {_id: '$tags', n: {$sum: 1}}}]",
        "[{$match: {region: 'eu'}}, {$group: {_id: {$literal: 'all'}, n: {$count: {}}}}]",
    ];
    for p in pipelines {
        let got = docs(&mut db, &format!("db.s.aggregate({p})"));
        let reference = docs(&mut db, &format!("db.s.aggregate([{{$project: {{nothing_here: 0}}}}, {})", &p[1..]));
        assert_eq!(got, reference, "{p}");
        assert!(!got.is_empty(), "{p} should produce output");
    }
    // stage errors are still reported where pushdown takes the stage
    for bad in ["[{$limit: 0}]", "[{$sort: {}}]", "[{$skip: -1}]", "[{$match: {region: 'eu'}}, {$group: {_id: '$$ROOT'}}]", "[{$group: {_id: 1, x: {$median: '$qty'}}}]"] {
        assert!(db.execute(&format!("db.s.aggregate({bad})")).is_err(), "{bad} should fail");
    }
}

/// Updates change only the index entries that differ, and unique indexes
/// still catch collisions with other documents.
#[test]
fn updates_keep_indexes_exact_and_unique() {
    let mut db = Database::in_memory();
    db.doc_create_index_on("u", &["k"], true).unwrap();
    db.doc_create_index("u", "tags").unwrap();
    db.doc_create_index_on("u", &["g", "h"], false).unwrap();
    db.execute("db.u.insertMany([{_id: 1, k: 'a', tags: [1, 2], g: 1, h: 'x'}, {_id: 2, k: 'b', tags: [2, 3], g: 2}])").unwrap();
    for s in [
        "db.u.updateOne({_id: 1}, {$set: {k: 'a'}})",
        "db.u.updateOne({_id: 1}, {$push: {tags: 9}})",
        "db.u.updateOne({_id: 2}, {$pull: {tags: 2}})",
        "db.u.updateMany({}, {$inc: {g: 10}})",
        "db.u.updateOne({_id: 2}, {$set: {h: 'y', k: 'c'}})",
        "db.u.updateOne({_id: 1}, {$unset: {h: 1}})",
    ] {
        db.execute(s).unwrap_or_else(|e| panic!("{s}: {e}"));
        assert_valid(&mut db, "u");
    }
    assert!(matches!(db.execute("db.u.updateOne({_id: 1}, {$set: {k: 'c'}})"), Err(Error::Constraint(_))));
    assert_valid(&mut db, "u");
    assert_eq!(docs(&mut db, "db.u.find({tags: 9})").len(), 1);
    assert_eq!(docs(&mut db, "db.u.find({tags: 2})").len(), 1);
    assert_eq!(docs(&mut db, "db.u.find({g: 12, h: 'y'})").len(), 1);
    assert_eq!(docs(&mut db, "db.u.find({k: 'a'})").len(), 1);
}

#[test]
fn large_and_deeply_nested_documents_round_trip() {
    let mut db = Database::in_memory();
    let items: Vec<String> = (0..20_000).map(|i| format!("{{i: {i}, s: 'v{i}', f: {i}.5, deep: {{x: [{i}, {{y: {i}}}]}}}}")).collect();
    let big = format!("{{_id: 'big', blob: '{}', items: [{}], tail: {{last: true}}}}", "z".repeat(512 * 1024), items.join(","));
    let parsed = v(&big);
    db.doc_insert("big", parsed.clone()).unwrap();
    db.doc_create_index("big", "items.s").unwrap();
    assert_eq!(docs(&mut db, "db.big.find({_id: 'big'})"), vec![parsed]);
    assert_eq!(docs(&mut db, "db.big.find({'items.s': 'v19999'})").len(), 1);
    assert_eq!(access(&mut db, "big", "{'items.s': 'v5'}"), "index_scan");
    assert_eq!(docs(&mut db, "db.big.find({'items.deep.x.1.y': 7})").len(), 1);
    assert_eq!(docs(&mut db, "db.big.find({'items.19999.deep.x.0': 19999})").len(), 1);
    assert_eq!(docs(&mut db, "db.big.find({}, {tail: 1, _id: 0})"), vec![v("{tail: {last: true}}")]);
    db.execute("db.big.update({_id: 'big'}, {$set: {'items.0.s': 'changed'}})").unwrap();
    assert_eq!(docs(&mut db, "db.big.find({'items.s': 'changed'})").len(), 1);
    assert_eq!(docs(&mut db, "db.big.find({'items.s': 'v0'})").len(), 0);
    assert_valid(&mut db, "big");
    // a document at the nesting limit is stored and read back
    let deep = format!("{{_id: 1, a: {}1{}}}", "[".repeat(98), "]".repeat(98));
    db.execute(&format!("db.d.insert({deep})")).unwrap();
    assert_eq!(docs(&mut db, "db.d.find({_id: 1})"), vec![v(&deep)]);
}

// ----------------------------------------------------------- durability

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

/// One WAL record in the storage format (the format is unchanged).
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

/// Databases written before the binary layout hold JSON documents. They are
/// read, queried through their indexes, updated (rewritten in the binary
/// layout) and mixed with new documents, and everything survives a reopen.
#[test]
fn legacy_json_documents_still_work() {
    let dir = tempdir("legacy");
    fs::create_dir_all(&dir).unwrap();
    let int_id = |i: i64| {
        let mut k = b"d\0old\0".to_vec();
        k.push(2);
        k.extend(((i as u64) ^ (1 << 63)).to_be_bytes());
        k
    };
    let num_index = |n: f64, i: i64| {
        let bits = n.to_bits();
        let mut k = b"i\0c:old\0a\0".to_vec();
        k.push(2);
        k.extend((if bits >> 63 == 1 { !bits } else { bits ^ (1 << 63) }).to_be_bytes());
        k.extend(&int_id(i)[6..]);
        k
    };
    let mut ops = vec![(b"m\0c\0old".to_vec(), br#"{"indexes":["a"]}"#.to_vec())];
    for i in 0..6 {
        ops.push((int_id(i), format!(r#"{{"_id":{i},"a":{},"s":"row{i}","n":{{"t":[{i}]}}}}"#, i % 3).into_bytes()));
        ops.push((num_index((i % 3) as f64, i), vec![]));
    }
    fs::write(dir.join("data.wal"), wal_record(&ops)).unwrap();

    let mut db = Database::open(&dir).unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 1})").len(), 2);
    assert_eq!(access(&mut db, "old", "{a: 1}"), "index_scan");
    assert_eq!(docs(&mut db, "db.old.find({'n.t': 4})").len(), 1);
    assert_eq!(docs(&mut db, "db.old.find({}).sort({s: -1}).limit(1)")[0].get("_id"), Some(&Value::Int(5)));
    db.execute("db.old.update({_id: 1}, {$set: {a: 2}})").unwrap();
    db.execute("db.old.insert({_id: 9, a: 0, s: 'new'})").unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 2})").len(), 3);
    assert_eq!(docs(&mut db, "db.old.aggregate([{$group: {_id: '$a', n: {$sum: 1}}}])"), vec![v("{_id: 0, n: 3}"), v("{_id: 1, n: 1}"), v("{_id: 2, n: 3}")]);
    db.doc_reindex("old").unwrap();
    assert_valid(&mut db, "old");
    drop(db);
    let mut db = Database::open(&dir).unwrap();
    assert_valid(&mut db, "old");
    assert_eq!(docs(&mut db, "db.old.find({a: 2})").len(), 3);
    assert_eq!(docs(&mut db, "db.old.find({_id: 0})"), vec![v(r#"{"_id":0,"a":0,"s":"row0","n":{"t":[0]}}"#)]);
    let _ = fs::remove_dir_all(&dir);
}

/// A crash can cut the WAL at any byte. Whatever survives, documents and
/// index entries agree and each commit is all or nothing.
#[test]
fn torn_wal_at_every_byte_keeps_documents_and_indexes_consistent() {
    let dir = tempdir("torn");
    {
        let mut db = Database::open(&dir).unwrap();
        db.doc_create_index("c", "k").unwrap();
        db.doc_create_index("c", "n.m").unwrap();
        db.execute("db.c.insertMany([{_id: 1, k: 'a', n: {m: 1}}, {_id: 2, k: 'b', n: {m: 2}, tags: [1, 2]}])").unwrap();
        db.checkpoint().unwrap();
        db.execute("db.c.updateMany({}, {$set: {k: 'z'}, $inc: {'n.m': 10}})").unwrap();
        db.execute("db.c.insertMany([{_id: 3, k: 'c'}, {_id: 4, k: 'd', n: {m: 4}}])").unwrap();
        db.execute("db.c.deleteOne({_id: 1})").unwrap();
    }
    let wal = fs::read(dir.join("data.wal")).unwrap();
    let snap = fs::read(dir.join("data.snap")).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for cut in 0..=wal.len() {
        let d = tempdir(&format!("torn-{cut}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("data.snap"), &snap).unwrap();
        fs::write(d.join("data.wal"), &wal[..cut]).unwrap();
        let mut db = Database::open(&d).unwrap();
        assert_valid(&mut db, "c");
        let n = db.doc_count("c", &v("{}")).unwrap();
        let z = db.doc_count("c", &v("{k: 'z'}")).unwrap();
        let state = match (n, z) {
            (2, 0) => "before",
            (2, 2) => "updated",
            (4, 2) => "inserted",
            (3, 1) => "deleted",
            other => panic!("cut at {cut}: impossible state {other:?}"),
        };
        if state != "before" {
            assert_eq!(docs(&mut db, "db.c.find({'n.m': {$gte: 11}})").len() as u64, z, "cut {cut}");
            assert_eq!(docs(&mut db, "db.c.find({k: 'a'})").len(), 0, "cut {cut}");
        }
        seen.insert(state);
        drop(db);
        let _ = fs::remove_dir_all(&d);
    }
    assert_eq!(seen.len(), 4, "every commit boundary was reached: {seen:?}");
    let _ = fs::remove_dir_all(&dir);
}
