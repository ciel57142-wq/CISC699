//! Unit tests that need direct access to the store: damaging index entries
//! behind the engine's back, and reading schemas in the original format.

use super::catalog::{component_len, enc_component, prefix_end};
use super::eval::like;
use super::parser::{parse_expr, to_sql};
use super::*;
use crate::keys;
use crate::storage::{Store, WriteSet};

/// Run one statement-sized unit of work and commit it.
fn with<T>(store: &mut Store, f: impl FnOnce(&mut Ctx) -> T) -> T {
    let (out, ws) = {
        let mut ctx = Ctx { store: &*store, writes: WriteSet::new(), reads: None };
        let out = f(&mut ctx);
        (out, ctx.writes)
    };
    store.commit(ws).unwrap();
    out
}

fn sql(store: &mut Store, q: &str) -> Output {
    with(store, |ctx| execute(ctx, q)).unwrap_or_else(|e| panic!("{q}: {e}"))
}

fn rows(store: &mut Store, q: &str) -> Vec<Vec<Value>> {
    match sql(store, q) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn problems(store: &mut Store, table: &str) -> Vec<String> {
    rows(store, &format!("CHECK TABLE {table}"))
        .into_iter()
        .filter(|r| r[2] != Value::Text("ok".into()))
        .map(|r| format!("{}: {}", r[1].display(), r[3].display()))
        .collect()
}

#[test]
fn check_table_finds_missing_and_orphaned_index_entries_and_reindex_repairs_them() {
    let mut store = Store::memory();
    sql(&mut store, "CREATE TABLE t (id INT PRIMARY KEY, c TEXT, n INT)");
    sql(&mut store, "CREATE INDEX t_c ON t (c)");
    sql(&mut store, "CREATE UNIQUE INDEX t_cn ON t (c, n)");
    sql(&mut store, "INSERT INTO t VALUES (1, 'a', 1), (2, 'b', 2), (3, 'a', 3)");
    assert!(problems(&mut store, "t").is_empty());

    // remove one entry of t_c and plant an entry that points nowhere
    let one_col = keys::index_prefix("t:t", "c");
    with(&mut store, |ctx| {
        let (victim, _) = ctx.scan(&one_col).into_iter().next().unwrap();
        ctx.delete(victim);
        let mut stray = one_col.clone();
        stray.extend(keys::encode_index_value(&Value::Text("zz".into())).unwrap());
        stray.extend(keys::encode_id(&Value::Int(99)).unwrap());
        ctx.put(stray, Vec::new());
    });
    let p = problems(&mut store, "t");
    assert_eq!(p, vec!["index t_c: 1 missing entry; 1 orphaned entry".to_string()], "{p:?}");
    // the damage is visible to queries that use the index: that is why it matters
    assert_eq!(rows(&mut store, "SELECT id FROM t WHERE c = 'a' ORDER BY id").len(), 1);

    sql(&mut store, "REINDEX TABLE t");
    assert!(problems(&mut store, "t").is_empty());
    assert_eq!(rows(&mut store, "SELECT id FROM t WHERE c = 'a' ORDER BY id").len(), 2);
}

#[test]
fn check_table_finds_rows_that_break_their_schema() {
    let mut store = Store::memory();
    sql(&mut store, "CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL CHECK (v > 0))");
    sql(&mut store, "CREATE TABLE c (id INT PRIMARY KEY, t_id INT REFERENCES t(id))");
    sql(&mut store, "INSERT INTO t VALUES (1, 5)");
    sql(&mut store, "INSERT INTO c VALUES (1, 1)");
    with(&mut store, |ctx| {
        let mut k = keys::row_prefix("t");
        k.extend(keys::encode_id(&Value::Int(7)).unwrap());
        ctx.put(k, b"[8, -1]".to_vec()); // wrong key for its primary key, and violates the CHECK
        let mut k = keys::row_prefix("t");
        k.extend(keys::encode_id(&Value::Int(9)).unwrap());
        ctx.put(k, b"[9, null]".to_vec()); // NOT NULL violated
        ctx.delete({
            let mut k = keys::row_prefix("t");
            k.extend(keys::encode_id(&Value::Int(1)).unwrap());
            k
        }); // orphan the child row
    });
    let p = problems(&mut store, "t");
    assert!(p.iter().any(|x| x.contains("violate a column type or NOT NULL")), "{p:?}");
    assert!(p.iter().any(|x| x.contains("does not match their primary key")), "{p:?}");
    assert!(p.iter().any(|x| x.starts_with("check t_v_check")), "{p:?}");
    let p = problems(&mut store, "c");
    assert_eq!(p, vec!["foreign key c_t_id_fkey: 1 row(s) reference missing keys in 't'".to_string()]);
}

#[test]
fn schemas_and_indexes_in_the_original_format_still_work() {
    let mut store = Store::memory();
    // exactly what the engine wrote before constraints and composite indexes existed
    with(&mut store, |ctx| {
        ctx.put(
            keys::table_meta("legacy"),
            br#"{"cols":[{"name":"id","not_null":false,"pk":true,"type":"int"},{"name":"city","not_null":false,"pk":false,"type":"text"}],"indexes":[["legacy_city","city"]]}"#.to_vec(),
        );
        for (id, city) in [(1, "Oslo"), (2, "Rome"), (3, "Oslo")] {
            let mut k = keys::row_prefix("legacy");
            k.extend(keys::encode_id(&Value::Int(id)).unwrap());
            ctx.put(k, format!("[{id}, \"{city}\"]").into_bytes());
            let mut ik = keys::index_prefix("t:legacy", "city");
            ik.extend(keys::encode_index_value(&Value::Text(city.into())).unwrap());
            ik.extend(keys::encode_id(&Value::Int(id)).unwrap());
            ctx.put(ik, Vec::new());
        }
    });
    assert!(problems(&mut store, "legacy").is_empty());
    assert_eq!(rows(&mut store, "SELECT id FROM legacy WHERE city = 'Oslo' ORDER BY id").len(), 2);
    match sql(&mut store, "EXPLAIN SELECT id FROM legacy WHERE city = 'Oslo'") {
        Output::Rows { rows, .. } => assert!(rows[0][0].display().contains("index legacy_city"), "{rows:?}"),
        other => panic!("{other:?}"),
    }
    // writing through the new engine keeps the old index layout readable and consistent
    sql(&mut store, "INSERT INTO legacy VALUES (4, 'Oslo')");
    sql(&mut store, "ALTER TABLE legacy ADD COLUMN pop INT DEFAULT 0");
    sql(&mut store, "UPDATE legacy SET city = 'Rome' WHERE id = 1");
    assert!(problems(&mut store, "legacy").is_empty());
    assert_eq!(rows(&mut store, "SELECT id FROM legacy WHERE city = 'Rome' ORDER BY id").len(), 2);
    assert!(with(&mut store, |ctx| execute(ctx, "INSERT INTO legacy VALUES (2, 'dup', 1)")).is_err());
}

#[test]
fn check_expressions_survive_a_print_and_parse_round_trip() {
    for src in [
        "a > 0 AND (b IS NULL OR b <> 'x''y')",
        "NOT (a BETWEEN -5 AND 5) OR a IN (1, 2, -3)",
        "CASE WHEN a > 1 THEN lower(b) ELSE 'z' END LIKE 'a%'",
        "CAST(a AS TEXT) || \"Odd \"\" Name\" = '1x'",
        "-a * 2.5 >= -9223372036854775807 - 1",
    ] {
        let e = parse_expr(src).unwrap();
        let printed = to_sql(&e);
        let again = parse_expr(&printed).unwrap_or_else(|err| panic!("{printed}: {err}"));
        assert_eq!(to_sql(&again), printed, "{src}");
    }
}

#[test]
fn like_matches_wildcards() {
    let m = |s: &str, p: &str| like(&s.chars().collect::<Vec<_>>(), &p.chars().collect::<Vec<_>>());
    assert!(m("", ""));
    assert!(m("", "%"));
    assert!(!m("", "_"));
    assert!(m("abc", "abc"));
    assert!(m("abc", "a%"));
    assert!(m("abc", "%c"));
    assert!(m("abc", "a_c"));
    assert!(m("abc", "%%b%%"));
    assert!(!m("abc", "a_"));
    assert!(m("aXbXc", "a%b%c"));
    assert!(!m("aXbXd", "a%b%c"));
    assert!(m("mississippi", "%iss%pi"));
    assert!(!m("mississippi", "%iss%px"));
    assert!(m("ünï", "_n_"));
}

#[test]
fn key_encoding_helpers() {
    for v in [Value::Null, Value::Bool(true), Value::Int(-3), Value::Float(2.5), Value::Text("a\0b".into())] {
        let mut b = enc_component(&v);
        let n = b.len();
        b.extend_from_slice(&[7, 7, 7]);
        assert_eq!(component_len(&b), Some(n), "{v:?}");
    }
    // encodings sort like values do, NULL first
    let vals = [Value::Null, Value::Bool(false), Value::Int(-10), Value::Float(-0.5), Value::Int(3), Value::Text("".into()), Value::Text("a".into()), Value::Text("a\0".into()), Value::Text("ab".into())];
    for w in vals.windows(2) {
        assert!(enc_component(&w[0]) < enc_component(&w[1]), "{:?} < {:?}", w[0], w[1]);
    }
    assert_eq!(prefix_end(&[1, 2, 0xff]), Some(vec![1, 3]));
    assert_eq!(prefix_end(&[0xff, 0xff]), None);
}
