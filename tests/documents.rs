//! Document engine: aggregation pipelines, MongoDB cursors and database namespaces.

use polydb::mongo::{decode_doc, encode_doc};
use polydb::server::{self, Config, Limits};
use polydb::{Database, Output, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn agg(db: &mut Database, coll: &str, pipeline: &str) -> Vec<Value> {
    let p = Value::parse(pipeline).unwrap();
    let Value::Array(stages) = p else {
        panic!("pipeline must be an array")
    };
    db.doc_aggregate(coll, &stages)
        .unwrap_or_else(|e| panic!("{pipeline}: {e}"))
}

fn j(s: &str) -> Value {
    Value::parse(s).unwrap()
}

fn seed(db: &mut Database) {
    for d in [
        r#"{_id: 1, name: 'a', status: 'active', department: 'eng', salary: 100, tags: ['x', 'y'], addr: {city: 'Berlin'}}"#,
        r#"{_id: 2, name: 'b', status: 'active', department: 'eng', salary: 150, tags: ['y']}"#,
        r#"{_id: 3, name: 'c', status: 'active', department: 'ops', salary: 90, tags: [], addr: {city: 'Paris'}}"#,
        r#"{_id: 4, name: 'd', status: 'inactive', department: 'ops', salary: 300}"#,
        r#"{_id: 5, name: 'e', status: 'active', department: null, salary: 'n/a'}"#,
        r#"{_id: 6, name: 'f', status: 'active', salary: 10.5}"#,
    ] {
        db.execute(&format!("db.staff.insert({d})")).unwrap();
    }
}

#[test]
fn match_group_sort_limit_pipeline() {
    let mut db = Database::in_memory();
    seed(&mut db);
    let out = agg(
        &mut db,
        "staff",
        r#"[{$match: {status: "active"}}, {$group: {_id: "$department", total: {$sum: "$salary"}, n: {$count: {}}}}, {$sort: {total: -1}}, {$limit: 10}]"#,
    );
    // missing and null departments group together under null; non-numeric salaries are ignored by $sum
    assert_eq!(
        out,
        vec![
            j(r#"{_id: 'eng', total: 250, n: 2}"#),
            j(r#"{_id: 'ops', total: 90, n: 1}"#),
            j(r#"{_id: null, total: 10.5, n: 2}"#),
        ]
    );
}

#[test]
fn accumulators() {
    let mut db = Database::in_memory();
    seed(&mut db);
    let out = agg(
        &mut db,
        "staff",
        r#"[{$match: {department: {$in: ['eng', 'ops']}}}, {$sort: {_id: 1}},
            {$group: {_id: "$department", avg: {$avg: "$salary"}, min: {$min: "$salary"}, max: {$max: "$salary"},
                      first: {$first: "$name"}, last: {$last: "$name"}, names: {$push: "$name"},
                      tags: {$addToSet: "$addr.city"}, cnt: {$sum: 1}}},
            {$sort: {_id: 1}}]"#,
    );
    assert_eq!(out.len(), 2);
    let eng = &out[0];
    assert_eq!(eng.get("avg"), Some(&Value::Float(125.0)));
    assert_eq!(eng.get("min"), Some(&Value::Int(100)));
    assert_eq!(eng.get("max"), Some(&Value::Int(150)));
    assert_eq!(eng.get("first"), Some(&j("'a'")));
    assert_eq!(eng.get("last"), Some(&j("'b'")));
    assert_eq!(eng.get("names"), Some(&j("['a', 'b']")));
    assert_eq!(
        eng.get("tags"),
        Some(&j("['Berlin']")),
        "missing values are not pushed"
    );
    assert_eq!(eng.get("cnt"), Some(&Value::Int(2)));
    // grouping by a compound key and by a constant
    // (as in MongoDB, a missing field and an explicit null make different compound keys)
    let out = agg(
        &mut db,
        "staff",
        r#"[{$group: {_id: {d: "$department", s: "$status"}, n: {$sum: 1}}}, {$sort: {"_id.d": 1, "_id.s": 1}}]"#,
    );
    assert_eq!(out.len(), 5);
    assert!(out.contains(&j(r#"{_id: {d: 'eng', s: 'active'}, n: 2}"#)));
    assert!(out.contains(&j(r#"{_id: {d: null, s: 'active'}, n: 1}"#)));
    assert!(out.contains(&j(r#"{_id: {s: 'active'}, n: 1}"#)));
    let out = agg(
        &mut db,
        "staff",
        r#"[{$group: {_id: null, n: {$sum: 1}, avg: {$avg: "$salary"}}}]"#,
    );
    assert_eq!(out, vec![j(r#"{_id: null, n: 6, avg: 130.1}"#)]);
    // empty input gives no groups
    assert_eq!(
        agg(
            &mut db,
            "nothing",
            r#"[{$group: {_id: null, n: {$sum: 1}}}]"#
        ),
        Vec::<Value>::new()
    );
}

#[test]
fn unwind_project_count_skip_lookup() {
    let mut db = Database::in_memory();
    seed(&mut db);
    let out = agg(
        &mut db,
        "staff",
        r#"[{$unwind: "$tags"}, {$group: {_id: "$tags", n: {$sum: 1}}}, {$sort: {_id: 1}}]"#,
    );
    assert_eq!(out, vec![j("{_id: 'x', n: 1}"), j("{_id: 'y', n: 2}")]);
    let out = agg(
        &mut db,
        "staff",
        r#"[{$unwind: {path: "$tags", preserveNullAndEmptyArrays: true, includeArrayIndex: "i"}}, {$count: "rows"}]"#,
    );
    assert_eq!(out, vec![j("{rows: 7}")]);
    let out = agg(
        &mut db,
        "staff",
        r#"[{$match: {_id: {$lte: 2}}}, {$project: {name: 1, city: "$addr.city", double: {$multiply: ["$salary", 2]}, label: {$concat: ["$name", "-", "$status"]}, _id: 0}}]"#,
    );
    assert_eq!(
        out[0],
        j("{name: 'a', city: 'Berlin', double: 200, label: 'a-active'}")
    );
    assert_eq!(
        out[1],
        j("{name: 'b', double: 300, label: 'b-active'}"),
        "missing computed fields are omitted"
    );
    let out = agg(
        &mut db,
        "staff",
        r#"[{$project: {tags: 0, addr: 0, salary: 0, status: 0, department: 0}}, {$sort: {_id: -1}}, {$skip: 1}, {$limit: 2}]"#,
    );
    assert_eq!(
        out,
        vec![j("{_id: 5, name: 'e'}"), j("{_id: 4, name: 'd'}")]
    );
    assert_eq!(
        agg(
            &mut db,
            "staff",
            r#"[{$match: {status: 'gone'}}, {$count: "n"}]"#
        ),
        Vec::<Value>::new()
    );
    // $lookup joins another collection
    db.execute("db.depts.insert([{_id: 'eng', floor: 3}, {_id: 'ops', floor: 1}])")
        .unwrap();
    let out = agg(
        &mut db,
        "staff",
        r#"[{$match: {_id: {$in: [1, 3, 6]}}}, {$lookup: {from: "depts", localField: "department", foreignField: "_id", as: "dept"}}, {$project: {f: {$arrayElemAt: ["$dept.0.floor", 0]}, n: {$size: "$dept"}}}, {$sort: {_id: 1}}]"#,
    );
    assert_eq!(out[0].get("n"), Some(&Value::Int(1)));
    assert_eq!(out[2].get("n"), Some(&Value::Int(0)));
    // the shell form runs the same pipeline
    match db
        .execute(r#"db.staff.aggregate([{$match: {status: 'inactive'}}, {$project: {name: 1}}])"#)
        .unwrap()
    {
        Output::Docs(d) => assert_eq!(d, vec![j("{_id: 4, name: 'd'}")]),
        other => panic!("{other:?}"),
    }
    assert!(db.execute("db.staff.aggregate([{$bogus: 1}])").is_err());
}

// ------------------------------------------------------------- wire protocol

struct Mongo {
    s: TcpStream,
    id: i32,
}

impl Mongo {
    fn connect(addr: std::net::SocketAddr) -> Mongo {
        let s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Mongo { s, id: 0 }
    }

    fn cmd(&mut self, doc: &str) -> Value {
        self.id += 1;
        let body = encode_doc(&j(doc));
        let mut msg = Vec::new();
        msg.extend_from_slice(&((16 + 5 + body.len()) as i32).to_le_bytes());
        msg.extend_from_slice(&self.id.to_le_bytes());
        msg.extend_from_slice(&0i32.to_le_bytes());
        msg.extend_from_slice(&2013i32.to_le_bytes());
        msg.extend_from_slice(&0u32.to_le_bytes());
        msg.push(0);
        msg.extend_from_slice(&body);
        self.s.write_all(&msg).unwrap();
        let mut hdr = [0u8; 16];
        self.s.read_exact(&mut hdr).unwrap();
        let len = i32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
        let mut rest = vec![0u8; len - 16];
        self.s.read_exact(&mut rest).unwrap();
        decode_doc(&rest[5..]).unwrap().0
    }
}

fn cursor_id(reply: &Value) -> i64 {
    match reply.get("cursor").and_then(|c| c.get("id")) {
        Some(Value::Int(n)) => *n,
        other => panic!("no cursor id in {}: {other:?}", reply.to_json()),
    }
}

fn batch(reply: &Value, key: &str) -> Vec<Value> {
    match reply.get("cursor").and_then(|c| c.get(key)) {
        Some(Value::Array(a)) => a.clone(),
        _ => panic!("no {key} in {}", reply.to_json()),
    }
}

#[test]
fn mongo_cursors_batches_and_namespaces() {
    let db = Database::in_memory();
    let cfg = Config {
        host: "127.0.0.1".into(),
        native: None,
        redis: None,
        postgres: None,
        mongo: Some(0),
    };
    let r = server::start_with_limits(
        &db,
        &cfg,
        Limits {
            mongo_default_db: "app".into(),
            ..Default::default()
        },
    )
    .unwrap();
    let mut m = Mongo::connect(r.mongo.unwrap());
    let docs: Vec<String> = (0..250)
        .map(|i| format!("{{_id: {i}, v: {}}}", i % 7))
        .collect();
    let reply = m.cmd(&format!(
        "{{insert: 'nums', documents: [{}], $db: 'app'}}",
        docs.join(",")
    ));
    assert_eq!(
        reply.get("n"),
        Some(&Value::Int(250)),
        "{}",
        reply.to_json()
    );

    // first batch of 101, then getMore until exhausted
    let first = m.cmd("{find: 'nums', filter: {}, $db: 'app'}");
    assert_eq!(batch(&first, "firstBatch").len(), 101);
    let id = cursor_id(&first);
    assert_ne!(id, 0);
    let more = m.cmd(&format!(
        "{{getMore: {id}, collection: 'nums', batchSize: 100, $db: 'app'}}"
    ));
    assert_eq!(batch(&more, "nextBatch").len(), 100);
    assert_eq!(cursor_id(&more), id);
    let last = m.cmd(&format!(
        "{{getMore: {id}, collection: 'nums', $db: 'app'}}"
    ));
    assert_eq!(batch(&last, "nextBatch").len(), 49);
    assert_eq!(cursor_id(&last), 0, "an exhausted cursor reports id 0");
    let gone = m.cmd(&format!(
        "{{getMore: {id}, collection: 'nums', $db: 'app'}}"
    ));
    assert_eq!(gone.get("codeName"), Some(&j("'CursorNotFound'")));

    // batchSize on find, killCursors, aggregate cursors
    let f = m.cmd("{find: 'nums', filter: {v: 3}, batchSize: 5, $db: 'app'}");
    assert_eq!(batch(&f, "firstBatch").len(), 5);
    let id = cursor_id(&f);
    let k = m.cmd(&format!(
        "{{killCursors: 'nums', cursors: [{id}], $db: 'app'}}"
    ));
    assert_eq!(
        k.get("cursorsKilled"),
        Some(&Value::Array(vec![Value::Int(id)]))
    );
    let gone = m.cmd(&format!(
        "{{getMore: {id}, collection: 'nums', $db: 'app'}}"
    ));
    assert_eq!(gone.get("codeName"), Some(&j("'CursorNotFound'")));
    let a = m.cmd("{aggregate: 'nums', pipeline: [{$group: {_id: '$v', n: {$sum: 1}}}, {$sort: {_id: 1}}], cursor: {batchSize: 3}, $db: 'app'}");
    let rows = batch(&a, "firstBatch");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0], j("{_id: 0, n: 36}"));
    let rest = m.cmd(&format!(
        "{{getMore: {}, collection: 'nums', $db: 'app'}}",
        cursor_id(&a)
    ));
    assert_eq!(batch(&rest, "nextBatch").len(), 4);

    // namespaces: the default database maps to the native collection names
    let mut native = db.session();
    assert_eq!(
        native.execute("db.nums.count({})").unwrap(),
        Output::Scalar(Value::Int(250))
    );
    m.cmd("{insert: 'nums', documents: [{_id: 1, other: true}], $db: 'shop'}");
    let c = m.cmd("{count: 'nums', query: {}, $db: 'shop'}");
    assert_eq!(c.get("n"), Some(&Value::Int(1)), "databases are isolated");
    assert_eq!(
        native.execute("db.shop.nums.count({})").unwrap(),
        Output::Scalar(Value::Int(1))
    );
    let dbs = m.cmd("{listDatabases: 1, $db: 'admin'}");
    let names: Vec<String> = match dbs.get("databases") {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|d| d.get("name").and_then(Value::as_str).map(String::from))
            .collect(),
        _ => vec![],
    };
    assert!(
        names.contains(&"app".to_string()) && names.contains(&"shop".to_string()),
        "{names:?}"
    );
    let lc = m.cmd("{listCollections: 1, $db: 'shop'}");
    assert_eq!(batch(&lc, "firstBatch").len(), 1);
    m.cmd("{dropDatabase: 1, $db: 'shop'}");
    assert_eq!(
        m.cmd("{count: 'nums', query: {}, $db: 'shop'}").get("n"),
        Some(&Value::Int(0))
    );
    assert_eq!(
        m.cmd("{count: 'nums', query: {}, $db: 'app'}").get("n"),
        Some(&Value::Int(250))
    );
}
