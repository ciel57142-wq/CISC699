//! Integration tests for the document model: representation, CRUD, query
//! and update operators, indexes, aggregation, transactions and restarts.

use polydb::{Database, Error, FindOptions, Output, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-doc-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

fn j(s: &str) -> Value {
    Value::parse(s).unwrap_or_else(|e| panic!("bad test JSON {s}: {e}"))
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

fn done(db: &mut Database, cmd: &str) -> String {
    match db.execute(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}")) {
        Output::Done(m) => m,
        other => panic!("expected a status from {cmd}, got {other:?}"),
    }
}

/// `_id`s of the documents a find command returns, in returned order.
fn ids(db: &mut Database, cmd: &str) -> Vec<i64> {
    docs(db, cmd).iter().map(|d| d.get("_id").and_then(Value::as_i64).expect("integer _id")).collect()
}

fn count(db: &mut Database, coll: &str, filter: &str) -> u64 {
    db.doc_count(coll, &j(filter)).unwrap_or_else(|e| panic!("count {filter}: {e}"))
}

fn assert_valid(db: &mut Database, coll: &str) {
    let r = db.doc_validate(coll).unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(true)), "validate({coll}) = {}", r.to_json());
}

fn access(db: &mut Database, coll: &str, filter: &str) -> String {
    let e = db.doc_explain(coll, &j(filter)).unwrap();
    e.get("access").and_then(Value::as_str).unwrap().to_string()
}

fn people(db: &mut Database) {
    db.execute(
        r#"db.people.insertMany([
        {_id: 1, name: "Ada", age: 36, active: true, tags: ["math", "code"], addr: {city: "London", zip: "N1"},
         scores: [90, 72], pets: [{kind: "cat", age: 3}, {kind: "dog", age: 9}]},
        {_id: 2, name: "Linus", age: 54, active: false, tags: ["code"], addr: {city: "Portland"}, scores: [60]},
        {_id: 3, name: "Grace", age: 85.5, active: true, tags: [], nick: null, pets: [{kind: "dog", age: 2}]},
        {_id: 4, name: "Edsger", age: "unknown", tags: ["math"], addr: {city: "Austin", zip: null}},
        {_id: 5, name: "Barbara", active: true, scores: [88, 95, 70]}
    ])"#,
    )
    .unwrap();
}

// -------------------------------------------------------- representation

#[test]
fn every_json_type_round_trips_exactly() {
    let mut db = Database::in_memory();
    let original = j(r#"{"_id": "x", "s": "héllo \"q\" \n\t\u0001", "i": -9007199254740993, "f": 2.5, "whole": 1.0,
        "big": 1e300, "neg0": -0.0, "t": true, "fa": false, "n": null, "arr": [1, "a", null, [true, {"k": []}]],
        "obj": {"deep": {"er": {"est": [{}]}}}, "empty": {}, "ea": [], "unicode": "日本語 🚀"}"#);
    db.doc_insert("types", original.clone()).unwrap();
    let back = db.doc_find("types", &j("{}"), &FindOptions::default()).unwrap();
    assert_eq!(back, vec![original.clone()]);
    // Int and float stay distinct types even though they compare equal.
    assert_eq!(back[0].get("whole"), Some(&Value::Float(1.0)));
    assert_eq!(back[0].get("i"), Some(&Value::Int(-9007199254740993)));
    assert_eq!(count(&mut db, "types", r#"{"whole": 1}"#), 1);
    assert_eq!(count(&mut db, "types", r#"{"obj.deep.er.est": {"$size": 1}}"#), 1);
}

#[test]
fn documents_are_type_checked_on_the_way_in() {
    let mut db = Database::in_memory();
    for bad in ["db.c.insert(5)", "db.c.insert('text')", "db.c.insert([1, 2])", "db.c.insert(null)"] {
        assert!(matches!(db.execute(bad), Err(Error::Type(_))), "{bad}");
    }
    // Field names a dotted path could not address, or that look like operators.
    for bad in [r#"{"a.b": 1}"#, r#"{"$set": 1}"#, r#"{"x": {"$inc": 2}}"#, r#"{"x": [{"a.b": 1}]}"#] {
        assert!(matches!(db.doc_insert("c", j(bad)), Err(Error::Type(_))), "{bad}");
    }
    // _id must be an integer, a string or an ObjectId.
    for bad in ["{_id: null}", "{_id: [1]}", "{_id: {a: 1}}", "{_id: 1.5}", "{_id: true}"] {
        assert!(db.execute(&format!("db.c.insert({bad})")).is_err(), "{bad}");
    }
    // Non-finite numbers cannot be represented in storage.
    let mut nan = j("{_id: 1}");
    if let Value::Object(o) = &mut nan {
        o.insert("x".into(), Value::Float(f64::NAN));
    }
    assert!(matches!(db.doc_insert("c", nan), Err(Error::Type(_))));
    // Extended-JSON wrappers are values, not operators.
    db.execute(r#"db.c.insert({_id: {"$oid": "0123456789abcdef01234567"}, at: {"$date": 0}})"#).unwrap();
    assert_eq!(count(&mut db, "c", r#"{"at": {"$type": "date"}}"#), 1);
    assert_eq!(count(&mut db, "c", r#"{"_id": {"$oid": "0123456789abcdef01234567"}}"#), 1);
    // Nothing else was stored by the failed attempts.
    assert_eq!(count(&mut db, "c", "{}"), 1);
    assert!(matches!(db.execute("db.bad-name.insert({})"), Err(Error::Parse(_) | Error::Schema(_))));
}

#[test]
fn missing_null_and_nested_paths() {
    let mut db = Database::in_memory();
    people(&mut db);
    // null matches both null and missing; $exists distinguishes them.
    assert_eq!(ids(&mut db, "db.people.find({nick: null})"), vec![1, 2, 3, 4, 5]);
    assert_eq!(ids(&mut db, "db.people.find({nick: {$exists: true}})"), vec![3]);
    assert_eq!(ids(&mut db, "db.people.find({nick: {$ne: null}})"), Vec::<i64>::new());
    assert_eq!(ids(&mut db, "db.people.find({'addr.zip': null})"), vec![2, 3, 4, 5]);
    assert_eq!(ids(&mut db, "db.people.find({'addr.zip': {$exists: true}})"), vec![1, 4]);
    // Dotted paths into objects, array positions, and through arrays of objects.
    assert_eq!(ids(&mut db, "db.people.find({'addr.city': 'London'})"), vec![1]);
    assert_eq!(ids(&mut db, "db.people.find({'tags.0': 'math'})"), vec![1, 4]);
    assert_eq!(ids(&mut db, "db.people.find({'scores.1': {$gte: 90}})"), vec![5]);
    assert_eq!(ids(&mut db, "db.people.find({'pets.kind': 'dog'})"), vec![1, 3]);
    assert_eq!(ids(&mut db, "db.people.find({'pets.age': {$gt: 5}})"), vec![1]);
    // A path through a scalar reaches nothing.
    assert_eq!(ids(&mut db, "db.people.find({'name.first': {$exists: true}})"), Vec::<i64>::new());
    // Bad paths fail even when nothing would be scanned.
    for bad in ["{'a..b': 1}", "{'': 1}", "{'a.$': 1}"] {
        assert!(db.execute(&format!("db.empty.find({bad})")).is_err(), "{bad}");
    }
}

// ------------------------------------------------------------- operators

#[test]
fn comparison_and_set_operators() {
    let mut db = Database::in_memory();
    people(&mut db);
    assert_eq!(ids(&mut db, "db.people.find({age: {$eq: 36}})"), vec![1]);
    assert_eq!(ids(&mut db, "db.people.find({age: 36.0})"), vec![1]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$ne: 36}})"), vec![2, 3, 4, 5]);
    // Comparisons only see values of the same type class: "unknown" is not > 40.
    assert_eq!(ids(&mut db, "db.people.find({age: {$gt: 40}})"), vec![2, 3]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$gte: 54, $lt: 85.5}})"), vec![2]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$lte: 54}})"), vec![1, 2]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$gt: 'a'}})"), vec![4]);
    assert_eq!(ids(&mut db, "db.people.find({name: {$gte: 'E', $lt: 'H'}})"), vec![3, 4]);
    // Arrays: any element may satisfy a comparison or equality.
    assert_eq!(ids(&mut db, "db.people.find({scores: {$gt: 89}})"), vec![1, 5]);
    assert_eq!(ids(&mut db, "db.people.find({scores: 60})"), vec![2]);
    assert_eq!(ids(&mut db, "db.people.find({tags: ['math', 'code']})"), vec![1]);
    assert_eq!(ids(&mut db, "db.people.find({tags: []})"), vec![3]);
    // $in / $nin, including null meaning "missing or null".
    assert_eq!(ids(&mut db, "db.people.find({name: {$in: ['Ada', 'Grace', 'Nobody']}})"), vec![1, 3]);
    assert_eq!(ids(&mut db, "db.people.find({tags: {$in: ['code']}})"), vec![1, 2]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$in: [null, 54]}})"), vec![2, 5]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$nin: [null, 54]}})"), vec![1, 3, 4]);
    assert_eq!(ids(&mut db, "db.people.find({name: {$in: []}})"), Vec::<i64>::new());
    // Booleans.
    assert_eq!(ids(&mut db, "db.people.find({active: true})"), vec![1, 3, 5]);
    assert_eq!(ids(&mut db, "db.people.find({active: {$ne: true}})"), vec![2, 4]);
    // $type, $size, $elemMatch.
    assert_eq!(ids(&mut db, "db.people.find({age: {$type: 'string'}})"), vec![4]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$type: 'double'}})"), vec![3]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$type: 'number'}})"), vec![1, 2, 3]);
    assert_eq!(ids(&mut db, "db.people.find({nick: {$type: 'null'}})"), vec![3]);
    assert_eq!(ids(&mut db, "db.people.find({scores: {$size: 3}})"), vec![5]);
    assert_eq!(ids(&mut db, "db.people.find({tags: {$size: 0}})"), vec![3]);
    assert_eq!(ids(&mut db, "db.people.find({scores: {$elemMatch: {$gt: 70, $lt: 80}}})"), vec![1]);
    assert_eq!(ids(&mut db, "db.people.find({pets: {$elemMatch: {kind: 'dog', age: {$lt: 5}}}})"), vec![3]);
    // Without $elemMatch the two conditions may be met by different elements.
    assert_eq!(ids(&mut db, "db.people.find({'pets.kind': 'dog', 'pets.age': {$lt: 5}})"), vec![1, 3]);
}

#[test]
fn logical_operators() {
    let mut db = Database::in_memory();
    people(&mut db);
    assert_eq!(ids(&mut db, "db.people.find({$and: [{active: true}, {age: {$gt: 50}}]})"), vec![3]);
    assert_eq!(ids(&mut db, "db.people.find({$or: [{name: 'Ada'}, {age: {$type: 'string'}}]})"), vec![1, 4]);
    assert_eq!(ids(&mut db, "db.people.find({$nor: [{active: true}, {tags: 'math'}]})"), vec![2]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$not: {$gt: 50}}})"), vec![1, 4, 5]);
    assert_eq!(ids(&mut db, "db.people.find({age: {$not: {$exists: true}}})"), vec![5]);
    assert_eq!(ids(&mut db, "db.people.find({tags: {$not: {$in: ['code']}}})"), vec![3, 4, 5]);
    assert_eq!(
        ids(&mut db, "db.people.find({$or: [{$and: [{active: true}, {tags: 'code'}]}, {name: 'Linus'}], age: {$lt: 60}})"),
        vec![1, 2]
    );
    // Implicit AND across fields and across operators on one field.
    assert_eq!(ids(&mut db, "db.people.find({active: true, scores: {$exists: true, $size: 2}})"), vec![1]);
}

#[test]
fn malformed_filters_fail_up_front() {
    let mut db = Database::in_memory();
    let bad = [
        "{a: {$bogus: 1}}",
        "{$bogus: [{}]}",
        "{$and: []}",
        "{$or: {a: 1}}",
        "{a: {$in: 5}}",
        "{a: {$nin: 'x'}}",
        "{a: {$not: 5}}",
        "{a: {$exists: 'yes'}}",
        "{a: {$size: -1}}",
        "{a: {$size: 'two'}}",
        "{a: {$type: 'float'}}",
        "{a: {$elemMatch: 3}}",
        "{a: {$gt: 1, b: 2}}",
    ];
    // Checked against an empty (even non-existent) collection: nothing to scan.
    for f in bad {
        assert!(db.execute(&format!("db.nothing.find({f})")).is_err(), "find {f}");
        assert!(db.execute(&format!("db.nothing.delete({f})")).is_err(), "delete {f}");
        assert!(db.execute(&format!("db.nothing.count({f})")).is_err(), "count {f}");
    }
    assert!(matches!(db.doc_find("nothing", &j("[1]"), &FindOptions::default()), Err(Error::Type(_))));
}

// ---------------------------------------------------------------- updates

#[test]
fn update_operators() {
    let mut db = Database::in_memory();
    db.execute("db.u.insert({_id: 1, n: 1, f: 1.5, tags: ['a'], nested: {x: 1}, list: [1, 2, 3, 2, 1]})").unwrap();
    let get = |db: &mut Database| docs(db, "db.u.find({_id: 1})").remove(0);

    db.execute("db.u.updateOne({_id: 1}, {$set: {'nested.y.z': 'deep', s: 'str', 'list.1': 20}})").unwrap();
    let d = get(&mut db);
    assert_eq!(d.get("nested"), Some(&j(r#"{"x": 1, "y": {"z": "deep"}}"#)));
    assert_eq!(d.get("list"), Some(&j("[1, 20, 3, 2, 1]")));

    db.execute("db.u.updateOne({_id: 1}, {$inc: {n: 5, f: 1, newcount: 2, 'nested.x': -3}})").unwrap();
    let d = get(&mut db);
    assert_eq!(d.get("n"), Some(&Value::Int(6)));
    assert_eq!(d.get("f"), Some(&Value::Float(2.5)));
    assert_eq!(d.get("newcount"), Some(&Value::Int(2)));
    assert_eq!(polydb::doc::get_path(&d, "nested.x"), Some(&Value::Int(-2)));
    db.execute("db.u.updateOne({_id: 1}, {$inc: {n: 0.5}})").unwrap();
    assert_eq!(get(&mut db).get("n"), Some(&Value::Float(6.5)));

    db.execute("db.u.updateOne({_id: 1}, {$push: {tags: 'b', fresh: 1}})").unwrap();
    db.execute("db.u.updateOne({_id: 1}, {$push: {tags: {$each: ['c', 'a']}}})").unwrap();
    assert_eq!(get(&mut db).get("tags"), Some(&j(r#"["a", "b", "c", "a"]"#)));
    assert_eq!(get(&mut db).get("fresh"), Some(&j("[1]")));

    db.execute("db.u.updateOne({_id: 1}, {$addToSet: {tags: 'b', set: {$each: [1, 1.0, 2, {k: 1}, {k: 1}]}}})").unwrap();
    assert_eq!(get(&mut db).get("tags"), Some(&j(r#"["a", "b", "c", "a"]"#)));
    assert_eq!(get(&mut db).get("set"), Some(&j(r#"[1, 2, {"k": 1}]"#)));

    db.execute("db.u.updateOne({_id: 1}, {$pull: {tags: 'a', list: {$gte: 3}}})").unwrap();
    let d = get(&mut db);
    assert_eq!(d.get("tags"), Some(&j(r#"["b", "c"]"#)));
    assert_eq!(d.get("list"), Some(&j("[1, 2, 1]")));
    db.execute("db.u.updateOne({_id: 1}, {$push: {objs: {$each: [{k: 1, v: 'a'}, {k: 2, v: 'b'}]}}})").unwrap();
    db.execute("db.u.updateOne({_id: 1}, {$pull: {objs: {k: 1}, missingArray: 5}})").unwrap();
    assert_eq!(get(&mut db).get("objs"), Some(&j(r#"[{"k": 2, "v": "b"}]"#)));

    db.execute("db.u.updateOne({_id: 1}, {$unset: {s: '', 'nested.y': 1, 'list.0': 1, nothing: 1}})").unwrap();
    let d = get(&mut db);
    assert_eq!(d.get("s"), None);
    assert_eq!(d.get("nested"), Some(&j(r#"{"x": -2}"#)));
    assert_eq!(d.get("list"), Some(&j("[null, 2, 1]")), "unsetting an element leaves a null");
}

#[test]
fn invalid_updates_are_rejected_and_change_nothing() {
    let mut db = Database::in_memory();
    db.execute("db.u.insertMany([{_id: 1, n: 1, s: 'x', a: [1]}, {_id: 2, n: 'two', a: 5}])").unwrap();
    let before = docs(&mut db, "db.u.find()");
    let bad = [
        "{$inc: {n: 'x'}}",
        "{$inc: {s: 1}}",
        "{$push: {s: 1}}",
        "{$addToSet: {s: 1}}",
        "{$pull: {s: 1}}",
        "{$push: {a: {$each: 1}}}",
        "{$push: {a: {$slice: 1}}}",
        "{$set: {'s.x': 1}}",
        "{$set: {_id: 9}}",
        "{$unset: {_id: 1}}",
        "{$set: {a: 1}, $inc: {a: 1}}",
        "{$set: {'a.b': 1, a: 2}}",
        "{$rename: {a: 'b'}}",
        "{$set: 5}",
        "{$set: {x: 1}, plain: 2}",
        "{$set: {'bad.': 1}}",
        "{$set: {'x.$': 1}}",
        "{$set: {'k.a.b': {'$oops': 1}}}",
        "{$inc: {big: 1}, $set: {big2: 1}}",
    ];
    for u in bad {
        // updateMany over both documents: the second fails for most of these, and
        // the first document's change must not survive.
        let r = db.execute(&format!("db.u.updateMany({{}}, {u})"));
        if u.starts_with("{$inc: {big") {
            continue; // valid, checked below
        }
        assert!(r.is_err(), "{u} should fail");
        assert_eq!(docs(&mut db, "db.u.find()"), before, "{u} left a partial change");
    }
    db.execute("db.u.updateMany({}, {$unset: {big: 1, big2: 1}})").unwrap();
    // $inc overflow is an error, not a wrap.
    db.execute("db.u.insert({_id: 3, n: 9223372036854775807})").unwrap();
    assert!(matches!(db.execute("db.u.updateOne({_id: 3}, {$inc: {n: 1}})"), Err(Error::Type(_))));
    // A malformed update fails even when nothing matches.
    assert!(db.execute("db.u.updateMany({_id: 999}, {$bogus: {a: 1}})").is_err());
    assert!(db.execute("db.u.updateMany({_id: 999}, {$set: {a: 1}, $unset: {a: 1}})").is_err());
    // Replacement may not change _id.
    assert!(matches!(db.execute("db.u.replaceOne({_id: 1}, {_id: 5, x: 1})"), Err(Error::Constraint(_))));
    assert!(db.execute("db.u.updateOne({_id: 1}, {x: 1})").is_err(), "updateOne needs operators");
    assert!(db.execute("db.u.replaceOne({_id: 1}, {$set: {x: 1}})").is_err(), "replaceOne takes a document");
}

#[test]
fn update_counts_replacement_and_upsert() {
    let mut db = Database::in_memory();
    db.execute("db.u.insertMany([{_id: 1, g: 'a', v: 1}, {_id: 2, g: 'a', v: 2}, {_id: 3, g: 'b', v: 3}])").unwrap();
    let r = db.doc_update_full("u", &j("{g: 'a'}"), &j("{$set: {v: 2}}"), true, false).unwrap();
    assert_eq!((r.matched, r.modified, r.upserted_id), (2, 1, None), "a no-op change is matched, not modified");
    let r = db.doc_update_full("u", &j("{g: 'a'}"), &j("{$set: {w: 1}}"), false, false).unwrap();
    assert_eq!((r.matched, r.modified), (1, 1), "multi=false stops after the first match");
    assert_eq!(count(&mut db, "u", "{w: 1}"), 1);
    // Plain update() changes every match unless told {multi: false}.
    assert!(done(&mut db, "db.u.update({g: 'a'}, {$set: {z: 1}}, {multi: false})").starts_with("1 document(s) matched"));
    assert!(done(&mut db, "db.u.update({g: 'a'}, {$set: {z: 2}})").starts_with("2 document(s) matched"));

    db.execute("db.u.replaceOne({_id: 3}, {g: 'c', fresh: true})").unwrap();
    assert_eq!(docs(&mut db, "db.u.find({_id: 3})"), vec![j(r#"{"_id": 3, "g": "c", "fresh": true}"#)]);

    // Upsert seeds the new document from the filter's equality conditions.
    let msg = done(&mut db, "db.u.updateOne({_id: 10, g: 'new', v: {$gt: 5}}, {$set: {x: 1}, $setOnInsert: {created: true}}, {upsert: true})");
    assert!(msg.contains("upserted _id 10"), "{msg}");
    assert_eq!(docs(&mut db, "db.u.find({_id: 10})"), vec![j(r#"{"_id": 10, "g": "new", "x": 1, "created": true}"#)]);
    // A second run matches instead, and $setOnInsert does nothing.
    db.execute("db.u.updateOne({_id: 10}, {$set: {x: 2}, $setOnInsert: {created: false}}, {upsert: true})").unwrap();
    assert_eq!(docs(&mut db, "db.u.find({_id: 10})")[0].get("created"), Some(&Value::Bool(true)));
    // Upsert with a generated _id.
    let r = db.doc_update_full("u", &j("{g: 'gen'}"), &j("{$inc: {n: 1}}"), false, true).unwrap();
    let id = r.upserted_id.expect("upserted");
    assert!(matches!(id, Value::Text(_)));
    assert_eq!(db.doc_find("u", &j("{g: 'gen'}"), &FindOptions::default()).unwrap()[0].get("n"), Some(&Value::Int(1)));
}

// ------------------------------------------------------------------- CRUD

#[test]
fn crud_count_delete_drop() {
    let mut db = Database::in_memory();
    people(&mut db);
    assert_eq!(scalar(&mut db, "db.people.count()"), Value::Int(5));
    assert_eq!(scalar(&mut db, "db.people.countDocuments({active: true})"), Value::Int(3));
    assert_eq!(scalar(&mut db, "db.people.find({active: true}).count()"), Value::Int(3));
    assert_eq!(docs(&mut db, "db.people.findOne({active: true})").len(), 1);
    assert_eq!(scalar(&mut db, "db.people.distinct('tags')"), j(r#"["code", "math"]"#));
    assert_eq!(scalar(&mut db, "db.people.distinct('addr.city', {active: true})"), j(r#"["London"]"#));
    assert_eq!(scalar(&mut db, "db.people.distinct('pets.kind')"), j(r#"["cat", "dog"]"#));

    // Projection, sort, skip, limit.
    let d = docs(&mut db, "db.people.find({active: true}, {name: 1, 'addr.city': 1, _id: 0}).sort({name: 1})");
    assert_eq!(d, vec![j(r#"{"name":"Ada","addr":{"city":"London"}}"#), j(r#"{"name":"Barbara"}"#), j(r#"{"name":"Grace"}"#)]);
    let d = docs(&mut db, "db.people.find({_id: 1}, {pets: 0, scores: 0, addr: 0, tags: 0})");
    assert_eq!(d, vec![j(r#"{"_id": 1, "name": "Ada", "age": 36, "active": true}"#)]);
    let d = docs(&mut db, "db.people.find({_id: 1}, {'pets.kind': 1})");
    assert_eq!(d, vec![j(r#"{"_id": 1, "pets": [{"kind": "cat"}, {"kind": "dog"}]}"#)]);
    let d = docs(&mut db, "db.people.find({_id: 2}, {who: '$name', city: '$addr.city', k: {$literal: 1}})");
    assert_eq!(d, vec![j(r#"{"_id": 2, "who": "Linus", "city": "Portland", "k": 1}"#)]);
    assert!(db.execute("db.people.find({}, {name: 1, age: 0})").is_err(), "mixed projection");
    assert_eq!(ids(&mut db, "db.people.find().sort({age: -1})"), vec![4, 3, 2, 1, 5], "string > number > missing");
    assert_eq!(ids(&mut db, "db.people.find().sort([['active', -1], ['name', 1]])"), vec![1, 5, 3, 2, 4]);
    assert_eq!(ids(&mut db, "db.people.find().sort({_id: -1}).skip(1).limit(2)"), vec![4, 3]);
    assert!(db.execute("db.people.find().sort({age: 2})").is_err());

    // Delete with and without a limit.
    assert_eq!(done(&mut db, "db.people.deleteOne({active: true})"), "1 document(s) deleted");
    assert_eq!(ids(&mut db, "db.people.find({active: true})"), vec![3, 5]);
    assert_eq!(done(&mut db, "db.people.deleteMany({tags: 'math'})"), "1 document(s) deleted");
    assert_eq!(scalar(&mut db, "db.people.count()"), Value::Int(3));
    assert!(db.execute("db.people.delete()").is_err(), "delete needs an explicit filter");
    assert_eq!(done(&mut db, "db.people.delete({})"), "3 document(s) deleted");

    // Drop removes documents, indexes and metadata.
    people(&mut db);
    db.execute("db.people.createIndex({name: 1})").unwrap();
    let before = db.entry_count();
    assert!(done(&mut db, "db.people.drop()").contains("dropped"));
    assert!(db.entry_count() < before);
    assert_eq!(db.doc_collections().unwrap(), Vec::<String>::new());
    assert!(db.doc_indexes("people").unwrap().is_empty());
    assert!(done(&mut db, "db.people.drop()").contains("does not exist"));
    // Ids start over and an old index does not come back.
    people(&mut db);
    assert_eq!(db.doc_indexes("people").unwrap().len(), 1);
}

#[test]
fn bulk_operations_are_atomic() {
    let mut db = Database::in_memory();
    // insertMany with a duplicate stores nothing.
    db.execute("db.b.insert({_id: 1})").unwrap();
    assert!(matches!(db.execute("db.b.insertMany([{_id: 2}, {_id: 3}, {_id: 1}])"), Err(Error::Constraint(_))));
    assert_eq!(count(&mut db, "b", "{}"), 1);
    assert_eq!(db.doc_insert_many("b", vec![j("{_id: 2}"), j("{_id: 3}")]).unwrap(), vec![Value::Int(2), Value::Int(3)]);

    let r = scalar(
        &mut db,
        "db.b.bulkWrite([
            {insertOne: {document: {_id: 4, v: 1}}},
            {updateOne: {filter: {_id: 2}, update: {$set: {v: 2}}}},
            {updateMany: {filter: {v: {$exists: false}}, update: {$set: {v: 0}}}},
            {replaceOne: {filter: {_id: 3}, replacement: {v: 3}}},
            {updateOne: {filter: {_id: 9}, update: {$set: {v: 9}}, upsert: true}},
            {deleteOne: {filter: {v: 0}}},
            {deleteMany: {filter: {v: {$gt: 8}}}}
        ])",
    );
    assert_eq!(r.get("insertedCount"), Some(&Value::Int(1)));
    assert_eq!(r.get("matchedCount"), Some(&Value::Int(4)));
    assert_eq!(r.get("modifiedCount"), Some(&Value::Int(4)));
    assert_eq!(r.get("upsertedCount"), Some(&Value::Int(1)));
    assert_eq!(r.get("deletedCount"), Some(&Value::Int(2)));
    assert_eq!(ids(&mut db, "db.b.find()"), vec![2, 3, 4]);

    // A failing operation anywhere undoes the whole batch.
    let snapshot = docs(&mut db, "db.b.find()");
    let err = db
        .execute("db.b.bulkWrite([{insertOne: {document: {_id: 50}}}, {deleteMany: {filter: {}}}, {updateOne: {filter: {}, update: {$inc: {v: 'x'}}}}])")
        .unwrap_err();
    assert!(err.to_string().contains("operation 2"), "{err}");
    assert_eq!(docs(&mut db, "db.b.find()"), snapshot);
    for bad in ["[{bogus: {}}]", "[{insertOne: {}}]", "[{updateOne: {filter: {}, update: {v: 1}}}]", "[{replaceOne: {filter: {}, replacement: {$set: {v: 1}}}}]", "{}"] {
        assert!(db.execute(&format!("db.b.bulkWrite({bad})")).is_err(), "{bad}");
    }
    assert_eq!(docs(&mut db, "db.b.find()"), snapshot);
}

// ---------------------------------------------------------------- indexes

#[test]
fn indexes_are_used_and_listed() {
    let mut db = Database::in_memory();
    people(&mut db);
    assert_eq!(access(&mut db, "people", "{name: 'Ada'}"), "collection_scan");
    assert_eq!(access(&mut db, "people", "{_id: 3}"), "id_lookup");
    assert_eq!(access(&mut db, "people", "{_id: {$in: [1, 3]}}"), "id_lookup");
    db.execute("db.people.createIndex({name: 1})").unwrap();
    db.execute("db.people.createIndex({'addr.city': 1})").unwrap();
    db.execute("db.people.createIndex([['active', 1], ['age', -1]])").unwrap();
    assert!(done(&mut db, "db.people.createIndex({name: 1})").contains("already indexed"));
    assert!(db.execute("db.people.createIndex({name: 1}, {unique: true})").is_err(), "different options");
    assert!(db.execute("db.people.createIndex({_id: 1})").is_err());
    assert!(db.execute("db.people.createIndex({name: 'text'})").is_err(), "only 1/-1 keys");

    let names: Vec<Value> = db.doc_indexes("people").unwrap().iter().map(|i| i.get("name").unwrap().clone()).collect();
    assert_eq!(names, vec![j(r#""_id_""#), j(r#""name_1""#), j(r#""addr.city_1""#), j(r#""active_1_age_1""#)]);

    let e = db.doc_explain("people", &j("{name: 'Ada'}")).unwrap();
    assert_eq!(e.get("access"), Some(&j(r#""index_scan""#)));
    assert_eq!(e.get("index"), Some(&j(r#""name_1""#)));
    assert_eq!(e.get("docsExamined"), Some(&Value::Int(1)));
    assert_eq!(access(&mut db, "people", "{'addr.city': {$in: ['London', 'Austin']}}"), "index_scan");
    assert_eq!(access(&mut db, "people", "{name: {$gte: 'B', $lt: 'H'}}"), "index_scan");
    assert_eq!(access(&mut db, "people", "{active: true, age: {$gt: 40}}"), "index_scan");
    let e = db.doc_explain("people", &j("{active: true, age: {$gt: 40}}")).unwrap();
    assert_eq!((e.get("equalityFields"), e.get("range")), (Some(&Value::Int(1)), Some(&Value::Bool(true))));
    assert_eq!(e.get("matched"), Some(&Value::Int(1)));
    // The second field alone cannot use the compound index.
    assert_eq!(access(&mut db, "people", "{age: 36}"), "collection_scan");
    // Non-indexable operands fall back to a scan.
    assert_eq!(access(&mut db, "people", "{name: null}"), "collection_scan");
    assert_eq!(access(&mut db, "people", "{name: {$ne: 'Ada'}}"), "collection_scan");
    assert_eq!(access(&mut db, "people", "{$or: [{name: 'Ada'}, {name: 'Grace'}]}"), "collection_scan");
    assert_eq!(access(&mut db, "people", "{$and: [{name: 'Ada'}, {age: 36}]}"), "index_scan");
    assert_eq!(scalar(&mut db, "db.people.find({name: 'Ada'}).explain()").get("index"), Some(&j(r#""name_1""#)));

    assert!(done(&mut db, "db.people.dropIndex('name_1')").contains("dropped"));
    assert!(done(&mut db, "db.people.dropIndex({'addr.city': 1})").contains("dropped"));
    assert!(done(&mut db, "db.people.dropIndex('nope')").contains("no such index"));
    assert_eq!(access(&mut db, "people", "{name: 'Ada'}"), "collection_scan");
    assert_eq!(db.doc_indexes("people").unwrap().len(), 2);
    assert_valid(&mut db, "people");
}

#[test]
fn unique_indexes() {
    let mut db = Database::in_memory();
    db.execute("db.users.insertMany([{_id: 1, email: 'a@x'}, {_id: 2, email: 'b@x'}, {_id: 3}])").unwrap();
    db.execute("db.users.createIndex({email: 1}, {unique: true})").unwrap();
    assert!(matches!(db.execute("db.users.insert({_id: 4, email: 'a@x'})"), Err(Error::Constraint(_))));
    assert!(matches!(db.execute("db.users.updateOne({_id: 2}, {$set: {email: 'a@x'}})"), Err(Error::Constraint(_))));
    // Missing fields are not indexed, so several documents may lack the field.
    db.execute("db.users.insert({_id: 5})").unwrap();
    // Re-setting a document's own value is fine; so is moving a value after freeing it.
    db.execute("db.users.updateOne({_id: 1}, {$set: {email: 'a@x', n: 1}})").unwrap();
    db.execute("db.users.updateOne({_id: 1}, {$set: {email: 'c@x'}})").unwrap();
    db.execute("db.users.updateOne({_id: 2}, {$set: {email: 'a@x'}})").unwrap();
    // Array elements are keys too.
    db.execute("db.tags.insert({_id: 1, t: ['x', 'y']})").unwrap();
    db.execute("db.tags.createIndex({t: 1}, {unique: true})").unwrap();
    assert!(db.execute("db.tags.insert({_id: 2, t: ['z', 'y']})").is_err());
    db.execute("db.tags.insert({_id: 3, t: ['z']})").unwrap();
    // An index cannot be built over existing duplicates.
    db.execute("db.d.insertMany([{_id: 1, k: 1}, {_id: 2, k: 1.0}])").unwrap();
    assert!(matches!(db.execute("db.d.createIndex({k: 1}, {unique: true})"), Err(Error::Constraint(_))));
    assert_eq!(db.doc_indexes("d").unwrap().len(), 1, "the failed index was not recorded");
    // Unique compound index: the combination must be unique.
    db.execute("db.c.createIndex(['a', 'b'], {unique: true})").unwrap();
    db.execute("db.c.insertMany([{_id: 1, a: 1, b: 1}, {_id: 2, a: 1, b: 2}, {_id: 3, a: 1}, {_id: 4, a: 1}])").unwrap();
    assert!(db.execute("db.c.insert({_id: 5, a: 1, b: 2})").is_err());
    for c in ["users", "tags", "d", "c"] {
        assert_valid(&mut db, c);
    }
}

/// Small deterministic pseudo-random generator so the test is reproducible.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 33) % n
    }
}

fn random_doc(r: &mut Lcg, id: i64) -> String {
    // `c` is never an array, so compound indexes never see parallel arrays.
    let val = |r: &mut Lcg, scalar_only: bool| -> String {
        match if scalar_only { [0, 1, 2, 3, 4, 7, 8][r.next(7) as usize] } else { r.next(9) } {
            0 => "null".into(),
            1 => format!("{}", r.next(10)),
            2 => format!("{}.5", r.next(10)),
            3 => format!("'s{}'", r.next(6)),
            4 => if r.next(2) == 0 { "true".into() } else { "false".into() },
            5 => format!("[{}, {}]", r.next(10), r.next(10)),
            6 => format!("['s{}', {}]", r.next(6), r.next(10)),
            7 => format!("{{v: {}}}", r.next(5)),
            _ => format!("{}", r.next(10) as i64 - 5),
        }
    };
    let mut fields = vec![format!("_id: {id}")];
    for name in ["a", "b", "c"] {
        if r.next(5) != 0 {
            fields.push(format!("{name}: {}", val(r, name == "c")));
        }
    }
    if r.next(3) != 0 {
        fields.push(format!("n: {{x: {}, arr: [{{y: {}}}, {{y: {}}}]}}", val(r, false), r.next(6), r.next(6)));
    }
    format!("{{{}}}", fields.join(", "))
}

/// Every filter must return the same documents with and without indexes,
/// through inserts, updates and deletes.
#[test]
fn indexed_and_unindexed_queries_agree() {
    let mut db = Database::in_memory();
    let mut r = Lcg(42);
    for id in 0..300 {
        let d = random_doc(&mut r, id);
        db.execute(&format!("db.plain.insert({d})")).unwrap();
        db.execute(&format!("db.ix.insert({d})")).unwrap();
    }
    for spec in ["{a: 1}", "{'n.x': 1}", "{'n.arr.y': 1}", "['b', 'c']", "['c', 'a']", "{'n.v': 1}"] {
        db.execute(&format!("db.ix.createIndex({spec})")).unwrap();
    }
    let filters = [
        "{a: 3}", "{a: 3.0}", "{a: 3.5}", "{a: 's2'}", "{a: true}", "{a: false}", "{a: null}", "{a: -2}",
        "{a: {$gt: 4}}", "{a: {$gte: 4, $lt: 7}}", "{a: {$lte: 2}}", "{a: {$gt: 's1'}}", "{a: {$lt: 's3', $gte: 's0'}}",
        "{a: {$gt: 2, $lt: 's9'}}", "{a: {$gt: false}}", "{a: {$in: [1, 's3', true]}}", "{a: {$in: [1, null]}}",
        "{a: {$eq: 5}}", "{a: {$ne: 5}}", "{a: {$nin: [1, 2]}}", "{a: {$exists: false}}", "{a: [1, 2]}",
        "{a: {v: 2}}", "{'a.v': 2}", "{'n.x': 7}", "{'n.x': {$gte: 3}}", "{'n.arr.y': 2}", "{'n.arr.y': {$in: [0, 5]}}",
        "{'n.arr.y': {$lt: 2}}", "{'n.arr.0.y': 3}", "{b: 1, c: 2}", "{b: 1}", "{b: 's1', c: {$gt: 3}}",
        "{b: {$in: [1, 2, 3]}, c: {$in: [4, 5]}}", "{c: 3, a: 4, b: 5}", "{c: 3, a: {$gt: 2}}", "{c: 3}",
        "{c: {$in: [1, 2]}, a: {$in: [3, 4]}, b: {$gte: 5}}", "{$and: [{b: 2}, {c: {$lt: 5}}]}",
        "{a: 3, b: {$gt: 1}}", "{a: {$gt: 3, $lt: 3}}", "{a: {$gte: 3, $lte: 3}}", "{'n.v': 1}",
        "{a: {$gt: 9007199254740992}}", "{a: {$type: 'array'}}", "{a: 5, $or: [{b: 1}, {c: 2}]}",
    ];
    let compare = |db: &mut Database, stage: &str| {
        for f in filters {
            let a = db.doc_find("plain", &j(f), &FindOptions::default()).unwrap_or_else(|e| panic!("{f}: {e}"));
            let b = db.doc_find("ix", &j(f), &FindOptions::default()).unwrap();
            assert_eq!(a, b, "{stage}: filter {f} differs with indexes");
        }
    };
    compare(&mut db, "after insert");
    let used = filters.iter().filter(|f| access(&mut db, "ix", f) == "index_scan").count();
    assert!(used >= 30, "only {used} filters used an index");

    let changes = [
        "db.X.updateMany({a: {$type: 'number', $not: {$type: 'array'}}}, {$inc: {a: 1}})",
        "db.X.updateMany({b: 's1'}, {$set: {b: ['s1', 7], c: 2}})",
        "db.X.updateMany({c: 2}, {$set: {a: ['s1', 3, true]}})",
        "db.X.updateMany({'n.x': {$gte: 5}}, {$unset: {'n.x': 1}})",
        "db.X.updateMany({c: 4}, {$push: {'n.arr': {y: 99}}})",
        "db.X.updateMany({a: null}, {$set: {a: 's4'}})",
        "db.X.deleteMany({c: {$in: [1, 's2']}})",
        "db.X.updateMany({'n.arr.y': 3}, {$pull: {'n.arr': {y: 3}}})",
        "db.X.replaceOne({_id: 7}, {a: 3, b: 1, c: 2})",
    ];
    for c in changes {
        db.execute(&c.replace("X", "plain")).unwrap();
        db.execute(&c.replace("X", "ix")).unwrap();
        compare(&mut db, c);
    }
    assert_valid(&mut db, "ix");
    // Rebuilding the index entries gives the same set.
    let before = db.doc_validate("ix").unwrap().get("indexEntries").cloned();
    db.doc_reindex("ix").unwrap();
    assert_eq!(db.doc_validate("ix").unwrap().get("indexEntries").cloned(), before);
    assert_valid(&mut db, "ix");
}

#[test]
fn compound_index_rejects_parallel_arrays() {
    let mut db = Database::in_memory();
    db.execute("db.p.createIndex(['a', 'b'])").unwrap();
    db.execute("db.p.insert({_id: 1, a: [1, 2], b: 1})").unwrap();
    assert!(matches!(db.execute("db.p.insert({_id: 2, a: [1, 2], b: [3, 4]})"), Err(Error::Constraint(_))));
    assert!(db.execute("db.p.updateOne({_id: 1}, {$set: {b: [5, 6]}})").is_err());
    assert_eq!(ids(&mut db, "db.p.find({a: 2, b: 1})"), vec![1]);
    assert_valid(&mut db, "p");
}

// ------------------------------------------------------------ aggregation

fn sales(db: &mut Database) {
    db.execute(
        "db.sales.insertMany([
        {_id: 1, region: 'east', item: 'pen', qty: 10, price: 1.5, tags: ['office', 'cheap']},
        {_id: 2, region: 'east', item: 'ink', qty: 2, price: 20},
        {_id: 3, region: 'west', item: 'pen', qty: 5, price: 1.5, tags: ['office']},
        {_id: 4, region: 'west', item: 'pad', qty: 7, price: 3, tags: []},
        {_id: 5, region: 'north', item: 'pen', qty: 1, price: 2},
        {_id: 6, item: 'gift', qty: 'n/a'}
    ])",
    )
    .unwrap();
}

#[test]
fn aggregation_pipeline() {
    let mut db = Database::in_memory();
    sales(&mut db);
    let agg = |db: &mut Database, p: &str| docs(db, &format!("db.sales.aggregate({p})"));

    let r = agg(
        &mut db,
        "[{$match: {qty: {$type: 'number'}}},
          {$group: {_id: '$region', total: {$sum: '$qty'}, n: {$sum: 1}, avg: {$avg: '$price'},
                    lo: {$min: '$price'}, hi: {$max: '$qty'}, items: {$addToSet: '$item'}, cnt: {$count: {}}}},
          {$sort: {total: -1}}]",
    );
    assert_eq!(
        r,
        vec![
            j(r#"{"_id":"east","total":12,"n":2,"avg":10.75,"lo":1.5,"hi":10,"items":["pen","ink"],"cnt":2}"#),
            j(r#"{"_id":"west","total":12,"n":2,"avg":2.25,"lo":1.5,"hi":7,"items":["pen","pad"],"cnt":2}"#),
            j(r#"{"_id":"north","total":1,"n":1,"avg":2.0,"lo":2,"hi":1,"items":["pen"],"cnt":1}"#),
        ]
    );
    // Missing group key is null; non-numbers are ignored by $sum/$avg.
    let r = agg(&mut db, "[{$group: {_id: '$region', s: {$sum: '$qty'}, a: {$avg: '$qty'}, f: {$first: '$item'}, l: {$last: '$item'}}}]");
    assert_eq!(r[0], j(r#"{"_id": null, "s": 0, "a": null, "f": "gift", "l": "gift"}"#));
    assert_eq!(r.len(), 4, "groups ordered by key: null, east, north, west");
    // Whole-collection group, as drivers send for countDocuments.
    assert_eq!(agg(&mut db, "[{$group: {_id: 1, n: {$sum: 1}}}]"), vec![j(r#"{"_id": 1, "n": 6}"#)]);
    assert_eq!(agg(&mut db, "[{$match: {qty: 999}}, {$group: {_id: 1, n: {$sum: 1}}}]"), Vec::<Value>::new());
    // Compound group keys and $push.
    let r = agg(&mut db, "[{$match: {item: 'pen'}}, {$group: {_id: {i: '$item', p: '$price'}, q: {$push: '$qty'}}}]");
    assert_eq!(r, vec![j(r#"{"_id": {"i": "pen", "p": 1.5}, "q": [10, 5]}"#), j(r#"{"_id": {"i": "pen", "p": 2}, "q": [1]}"#)]);
    // Int sums stay ints; mixing floats gives a float.
    assert_eq!(agg(&mut db, "[{$group: {_id: null, s: {$sum: '$price'}}}]"), vec![j(r#"{"_id": null, "s": 28.0}"#)]);

    // $project with inclusion, renames and literals; $sort, $skip, $limit, $count.
    let r = agg(&mut db, "[{$match: {region: 'west'}}, {$project: {_id: 0, item: 1, cost: '$price', one: {$literal: 1}}}, {$sort: {item: 1}}]");
    assert_eq!(r, vec![j(r#"{"item":"pad","cost":3,"one":1}"#), j(r#"{"item":"pen","cost":1.5,"one":1}"#)]);
    let r = agg(&mut db, "[{$sort: {qty: -1}}, {$skip: 1}, {$limit: 2}, {$project: {qty: 1}}]");
    assert_eq!(r, vec![j(r#"{"_id": 1, "qty": 10}"#), j(r#"{"_id": 4, "qty": 7}"#)]);
    assert_eq!(agg(&mut db, "[{$match: {item: 'pen'}}, {$count: 'pens'}]"), vec![j(r#"{"pens": 3}"#)]);
    assert_eq!(agg(&mut db, "[{$match: {item: 'none'}}, {$count: 'n'}]"), Vec::<Value>::new());
    // $unwind, then group on the elements.
    let r = agg(&mut db, "[{$unwind: '$tags'}, {$group: {_id: '$tags', n: {$sum: 1}}}]");
    assert_eq!(r, vec![j(r#"{"_id": "cheap", "n": 1}"#), j(r#"{"_id": "office", "n": 2}"#)]);
    let r = agg(&mut db, "[{$unwind: {path: '$tags', preserveNullAndEmptyArrays: true}}, {$count: 'n'}]");
    assert_eq!(r, vec![j(r#"{"n": 7}"#)]);
    // A later $match works on computed fields.
    let r = agg(&mut db, "[{$group: {_id: '$item', q: {$sum: '$qty'}}}, {$match: {q: {$gte: 7}}}, {$sort: [['q', -1]]}]");
    assert_eq!(r, vec![j(r#"{"_id": "pen", "q": 16}"#), j(r#"{"_id": "pad", "q": 7}"#)]);
    // The typed API agrees with the shell.
    let typed = db.doc_aggregate("sales", &[j("{$match: {region: 'east'}}"), j("{$count: 'n'}")]).unwrap();
    assert_eq!(typed, vec![j(r#"{"n": 2}"#)]);

    for bad in [
        "[{$bogus: {}}]",
        "[{$group: {total: {$sum: 1}}}]",
        "[{$group: {_id: null, x: {$median: '$qty'}}}]",
        "[{$group: {_id: null, x: 5}}]",
        "[{$group: {_id: null, 'a.b': {$sum: 1}}}]",
        "[{$project: {}}]",
        "[{$project: {a: 1, b: 0}}]",
        "[{$project: {a: {$add: [1, 2]}}}]",
        "[{$limit: 0}]",
        "[{$limit: -1}]",
        "[{$skip: 'x'}]",
        "[{$sort: {qty: 0}}]",
        "[{$count: '$x'}]",
        "[{$unwind: 'tags'}]",
        "[{$match: {qty: {$bad: 1}}}]",
        "[{$match: {}, $sort: {a: 1}}]",
        "[5]",
        "{}",
    ] {
        assert!(db.execute(&format!("db.sales.aggregate({bad})")).is_err(), "{bad}");
    }
}

// --------------------------------------------------------------- schemas

#[test]
fn collection_schema_validation() {
    let mut db = Database::in_memory();
    db.execute("db.acct.insertMany([{_id: 1, name: 'a', bal: 10}, {_id: 2, name: 'b', bal: 2.5}])").unwrap();
    db.execute("db.acct.setSchema({required: ['name', 'bal'], fields: {name: 'string', bal: 'number', 'meta.tags': ['array', 'null'], flag: 'bool'}})").unwrap();
    assert!(scalar(&mut db, "db.acct.getSchema()").get("required").is_some());
    db.execute("db.acct.insert({_id: 3, name: 'c', bal: 0, meta: {tags: null}, extra: 'free'})").unwrap();
    for bad in ["{_id: 4, bal: 1}", "{_id: 4, name: 4, bal: 1}", "{_id: 4, name: 'd', bal: '1'}", "{_id: 4, name: 'd', bal: 1, meta: {tags: 'x'}}", "{_id: 4, name: 'd', bal: 1, flag: 1}"] {
        assert!(matches!(db.execute(&format!("db.acct.insert({bad})")), Err(Error::Constraint(_))), "{bad}");
    }
    assert!(matches!(db.execute("db.acct.updateOne({_id: 1}, {$set: {bal: 'lots'}})"), Err(Error::Constraint(_))));
    assert!(matches!(db.execute("db.acct.updateOne({_id: 1}, {$unset: {name: 1}})"), Err(Error::Constraint(_))));
    db.execute("db.acct.updateOne({_id: 1}, {$inc: {bal: 5}})").unwrap();
    // A schema that existing documents violate is refused.
    assert!(db.execute("db.acct.setSchema({fields: {bal: 'int'}})").is_err());
    for bad in ["{fields: {a: 'float'}}", "{required: 'name'}", "{fields: {a: 5}}", "{other: 1}", "5"] {
        assert!(db.execute(&format!("db.acct.setSchema({bad})")).is_err(), "{bad}");
    }
    db.execute("db.acct.setSchema(null)").unwrap();
    assert_eq!(scalar(&mut db, "db.acct.getSchema()"), Value::Null);
    db.execute("db.acct.insert({_id: 9})").unwrap();
    assert_valid(&mut db, "acct");
}

// ------------------------------------------------------ transactions

#[test]
fn document_changes_are_transactional() {
    let mut db = Database::in_memory();
    db.execute("db.t.createIndex({k: 1})").unwrap();
    db.execute("db.t.insert({_id: 1, k: 'a'})").unwrap();
    let base = db.entry_count();

    db.execute("BEGIN").unwrap();
    db.execute("db.t.insert({_id: 2, k: 'b'})").unwrap();
    db.execute("db.t.updateOne({_id: 1}, {$set: {k: 'z'}})").unwrap();
    // Inside the transaction the changes are visible, through the index too.
    assert_eq!(ids(&mut db, "db.t.find({k: 'z'})"), vec![1]);
    assert_eq!(ids(&mut db, "db.t.find({k: 'b'})"), vec![2]);
    // Another session does not see them.
    let mut other = db.session();
    assert_eq!(ids(&mut other, "db.t.find({k: 'a'})"), vec![1]);
    assert_eq!(count(&mut other, "t", "{}"), 1);
    // A failing statement inside the transaction is undone alone.
    assert!(db.execute("db.t.insertMany([{_id: 3, k: 'c'}, {_id: 1}])").is_err());
    assert_eq!(count(&mut db, "t", "{}"), 2);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(db.entry_count(), base);
    assert_eq!(ids(&mut db, "db.t.find({k: 'a'})"), vec![1]);
    assert_valid(&mut db, "t");

    db.execute("BEGIN").unwrap();
    db.execute("db.t.insert({_id: 2, k: 'b'})").unwrap();
    db.execute("db.t.updateOne({_id: 1}, {$set: {k: 'z'}})").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(ids(&mut other, "db.t.find({k: {$in: ['b', 'z']}})"), vec![1, 2]);
    assert_valid(&mut db, "t");
}

#[test]
fn concurrent_document_transactions_conflict() {
    let mut a = Database::in_memory();
    a.execute("db.acct.insertMany([{_id: 1, bal: 100}, {_id: 2, bal: 0}])").unwrap();
    let mut b = a.session();
    a.execute("BEGIN").unwrap();
    b.execute("BEGIN").unwrap();
    // Both read and write the same document.
    assert_eq!(docs(&mut a, "db.acct.find({_id: 1})")[0].get("bal"), Some(&Value::Int(100)));
    assert_eq!(docs(&mut b, "db.acct.find({_id: 1})")[0].get("bal"), Some(&Value::Int(100)));
    a.execute("db.acct.updateOne({_id: 1}, {$inc: {bal: -30}})").unwrap();
    b.execute("db.acct.updateOne({_id: 1}, {$inc: {bal: -50}})").unwrap();
    a.execute("COMMIT").unwrap();
    assert!(matches!(b.execute("COMMIT"), Err(Error::Conflict(_))));
    assert_eq!(docs(&mut b, "db.acct.find({_id: 1})")[0].get("bal"), Some(&Value::Int(70)));
    // A query that scanned the collection conflicts with a later insert (no phantoms).
    a.execute("BEGIN").unwrap();
    assert_eq!(count(&mut a, "acct", "{bal: {$gte: 0}}"), 2);
    b.execute("db.acct.insert({_id: 3, bal: 5})").unwrap();
    a.execute("db.acct.insert({_id: 4, bal: 2})").unwrap();
    assert!(matches!(a.execute("COMMIT"), Err(Error::Conflict(_))));
    assert_eq!(count(&mut b, "acct", "{}"), 3);
}

// ---------------------------------------------------------------- restarts

#[test]
fn documents_indexes_and_schema_survive_restart() {
    let dir = tmp("restart");
    let snapshot;
    {
        let mut db = Database::open(&dir).unwrap();
        people(&mut db);
        sales(&mut db);
        db.execute("db.people.createIndex({name: 1}, {unique: true})").unwrap();
        db.execute("db.people.createIndex([['addr.city', 1], ['age', 1]])").unwrap();
        db.execute("db.sales.setSchema({required: ['item']})").unwrap();
        db.execute("db.people.updateMany({active: true}, {$push: {tags: 'vip'}, $inc: {visits: 1}})").unwrap();
        db.execute("db.people.deleteOne({_id: 4})").unwrap();
        db.execute("db.gone.insert({_id: 1})").unwrap();
        db.execute("db.gone.drop()").unwrap();
        snapshot = docs(&mut db, "db.people.find()");
    }
    for round in 0..3 {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(docs(&mut db, "db.people.find()"), snapshot, "round {round}");
        assert_eq!(db.doc_collections().unwrap(), vec!["people".to_string(), "sales".to_string()]);
        assert_eq!(access(&mut db, "people", "{name: 'Ada'}"), "index_scan");
        assert_eq!(ids(&mut db, "db.people.find({'addr.city': 'London', age: {$lt: 40}})"), vec![1]);
        assert!(db.execute("db.people.insert({_id: 99, name: 'Ada'})").is_err(), "unique index survives");
        assert!(db.execute("db.sales.insert({_id: 99})").is_err(), "schema survives");
        assert_eq!(count(&mut db, "gone", "{}"), 0);
        assert_valid(&mut db, "people");
        assert_valid(&mut db, "sales");
        if round == 1 {
            db.checkpoint().unwrap();
        }
    }
    // Generated ids keep increasing after a restart (no reuse).
    let first = {
        let mut db = Database::open(&dir).unwrap();
        db.doc_insert("gen", j("{v: 1}")).unwrap()
    };
    let mut db = Database::open(&dir).unwrap();
    let second = db.doc_insert("gen", j("{v: 2}")).unwrap();
    assert!(first.as_str().unwrap()[12..] < second.as_str().unwrap()[12..], "{first:?} then {second:?}");
    assert_eq!(count(&mut db, "gen", "{}"), 2);
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn uncommitted_and_torn_document_writes_are_discarded_on_restart() {
    let dir = tmp("torn");
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("db.c.createIndex({k: 1})").unwrap();
        db.execute("db.c.insertMany([{_id: 1, k: 'a'}, {_id: 2, k: 'b'}])").unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("db.c.insert({_id: 3, k: 'c'})").unwrap();
        db.execute("db.c.deleteMany({})").unwrap();
        // Dropped without COMMIT: the transaction never reaches the log.
    }
    {
        let mut db = Database::open(&dir).unwrap();
        assert_eq!(ids(&mut db, "db.c.find()"), vec![1, 2]);
        db.execute("db.c.updateMany({}, {$set: {k: 'z'}})").unwrap();
    }
    // Tear the last commit (the update) in half, as a crash mid-write would.
    let wal = dir.join("data.wal");
    let len = fs::metadata(&wal).unwrap().len();
    OpenOptions::new().write(true).open(&wal).unwrap().set_len(len - 7).unwrap();
    {
        let mut db = Database::open(&dir).unwrap();
        // The torn update is gone entirely: both documents and their index entries.
        assert_eq!(ids(&mut db, "db.c.find({k: {$in: ['a', 'b']}})"), vec![1, 2]);
        assert_eq!(count(&mut db, "c", "{k: 'z'}"), 0);
        assert_valid(&mut db, "c");
        db.execute("db.c.insert({_id: 3, k: 'c'})").unwrap();
    }
    // Garbage appended after the last record is ignored too.
    OpenOptions::new().append(true).open(&wal).unwrap().write_all(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).unwrap();
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(ids(&mut db, "db.c.find()"), vec![1, 2, 3]);
    assert_valid(&mut db, "c");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn indexes_from_the_old_metadata_format_still_load() {
    // Before compound indexes, collection metadata listed indexed fields as
    // bare strings; such a collection must keep working.
    let dir = tmp("oldmeta");
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("db.c.insertMany([{_id: 1, k: 'a'}, {_id: 2, k: 'b'}])").unwrap();
        db.execute("db.c.createIndex({k: 1})").unwrap();
    }
    {
        // Rewrite the metadata entry to the old layout through a raw store commit.
        let mut store = polydb::storage::Store::open(&dir).unwrap();
        let mut ws = polydb::storage::WriteSet::new();
        ws.insert(b"m\0c\0c".to_vec(), Some(br#"{"indexes":["k"]}"#.to_vec()));
        store.commit(ws).unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    assert_eq!(access(&mut db, "c", "{k: 'b'}"), "index_scan");
    assert_eq!(ids(&mut db, "db.c.find({k: 'b'})"), vec![2]);
    db.execute("db.c.updateOne({_id: 2}, {$set: {k: 'c'}})").unwrap();
    assert_eq!(ids(&mut db, "db.c.find({k: 'c'})"), vec![2]);
    assert_valid(&mut db, "c");
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn validate_detects_damaged_index_entries() {
    let dir = tmp("damage");
    {
        let mut db = Database::open(&dir).unwrap();
        db.execute("db.c.createIndex({k: 1})").unwrap();
        db.execute("db.c.insertMany([{_id: 1, k: 'a'}, {_id: 2, k: 'b'}])").unwrap();
    }
    {
        // Delete every index entry behind the engine's back.
        let mut store = polydb::storage::Store::open(&dir).unwrap();
        let keys: Vec<Vec<u8>> = store.scan(b"i\0c:c\0").map(|(k, _)| k.clone()).collect();
        let mut ws = polydb::storage::WriteSet::new();
        for k in keys {
            ws.insert(k, None);
        }
        store.commit(ws).unwrap();
    }
    let mut db = Database::open(&dir).unwrap();
    let r = db.doc_validate("c").unwrap();
    assert_eq!(r.get("ok"), Some(&Value::Bool(false)), "{}", r.to_json());
    assert_eq!(scalar(&mut db, "db.c.validate()").get("ok"), Some(&Value::Bool(false)));
    assert!(done(&mut db, "db.c.reindex()").starts_with("2 "));
    assert_valid(&mut db, "c");
    assert_eq!(ids(&mut db, "db.c.find({k: 'b'})"), vec![2]);
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn range_queries_on_multikey_fields_stay_correct() {
    let mut db = Database::in_memory();
    db.execute("db.m.createIndex({v: 1})").unwrap();
    for i in 0..50 {
        db.execute(&format!("db.m.insert({{_id: {i}, v: {i}}})")).unwrap();
    }
    // Scalars only: both bounds narrow the scan. Bounds are scanned
    // inclusively (13 is read, then rejected by the filter).
    let e = db.doc_explain("m", &j("{v: {$gte: 10, $lt: 13}}")).unwrap();
    assert_eq!((e.get("docsExamined"), e.get("matched")), (Some(&Value::Int(4)), Some(&Value::Int(3))));
    // [80, 3]: 80 meets $gte and 3 meets $lt, so the document matches even
    // though no single element lies in [10, 13).
    db.execute("db.m.insert({_id: 100, v: [80, 3]})").unwrap();
    assert_eq!(ids(&mut db, "db.m.find({v: {$gte: 10, $lt: 13}})"), vec![10, 11, 12, 100]);
    // $elemMatch needs one element meeting both, and only matches arrays.
    db.execute("db.m.insert({_id: 101, v: [11, 50]})").unwrap();
    assert_eq!(ids(&mut db, "db.m.find({v: {$elemMatch: {$gte: 10, $lt: 13}}})"), vec![101]);
    assert_eq!(access(&mut db, "m", "{v: {$gte: 10, $lt: 13}}"), "index_scan");
    // The multikey mark is persistent metadata and is rebuilt by reindex.
    let marked = |db: &mut Database| db.doc_validate("m").unwrap().get("ok") == Some(&Value::Bool(true));
    assert!(marked(&mut db));
    db.execute("db.m.deleteMany({_id: {$in: [100, 101]}})").unwrap();
    db.doc_reindex("m").unwrap();
    let e = db.doc_explain("m", &j("{v: {$gte: 10, $lt: 13}}")).unwrap();
    assert_eq!(e.get("docsExamined"), Some(&Value::Int(4)), "after reindex the field is single-valued again");
}
