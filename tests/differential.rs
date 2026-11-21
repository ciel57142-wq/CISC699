//! Differential testing against SQLite (through python3's built-in sqlite3
//! module). Random tables and queries run on both engines; results must be
//! identical. Skipped (with a message) when python3 is not available.
//!
//! Semantic differences deliberately kept out of the generated queries:
//! LIKE (SQLite folds ASCII case), division and modulo (SQLite returns NULL
//! on division by zero), comparisons between different types (SQLite orders
//! them, PolyDB makes them NULL), bare non-grouped columns, LIMIT without a
//! total order.

mod common;
use common::*;
use polydb::{Database, Output, Value};
use std::io::Write;
use std::process::{Command, Stdio};

const SQLITE_RUNNER: &str = r#"
import json, sqlite3, sys
job = json.load(sys.stdin)
db = sqlite3.connect(":memory:")
for s in job["setup"]:
    db.execute(s)
out = []
for q in job["queries"]:
    try:
        out.append({"rows": [list(r) for r in db.execute(q).fetchall()]})
    except Exception as e:
        out.append({"error": str(e)})
json.dump(out, sys.stdout)
"#;

fn sqlite(setup: &[String], queries: &[String]) -> Option<Vec<Result<Vec<Vec<Value>>, String>>> {
    let job = Value::Object(
        [
            (
                "setup".to_string(),
                Value::Array(setup.iter().map(|s| Value::Text(s.clone())).collect()),
            ),
            (
                "queries".to_string(),
                Value::Array(queries.iter().map(|s| Value::Text(s.clone())).collect()),
            ),
        ]
        .into_iter()
        .collect(),
    );
    let mut child = Command::new("python3")
        .args(["-c", SQLITE_RUNNER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    child
        .stdin
        .take()?
        .write_all(job.to_json().as_bytes())
        .ok()?;
    let out = child.wait_with_output().ok()?;
    let v = Value::parse(&String::from_utf8_lossy(&out.stdout)).ok()?;
    let Value::Array(items) = v else { return None };
    Some(
        items
            .into_iter()
            .map(|it| match (it.get("rows"), it.get("error")) {
                (Some(Value::Array(rs)), _) => Ok(rs
                    .iter()
                    .map(|r| {
                        if let Value::Array(c) = r {
                            c.clone()
                        } else {
                            vec![]
                        }
                    })
                    .collect()),
                (_, Some(e)) => Err(e.display()),
                _ => Err("bad reply".into()),
            })
            .collect(),
    )
}

/// Normalise values so both engines' outputs compare: floats rounded, bools as ints.
fn norm(v: &Value) -> String {
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Value::Float(f) => format!("{:.9}", f),
        Value::Bool(b) => (*b as i64).to_string(),
        Value::Null => "NULL".into(),
        Value::Int(i) => i.to_string(),
        other => other.display(),
    }
}

fn norm_rows(rows: &[Vec<Value>], ordered: bool) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| r.iter().map(norm).collect::<Vec<_>>().join("|"))
        .collect();
    if !ordered {
        v.sort();
    }
    v
}

fn int_or_null(r: &mut Rng) -> String {
    if r.chance(15) {
        "NULL".into()
    } else {
        r.range(-3, 12).to_string()
    }
}

fn text_or_null(r: &mut Rng) -> String {
    if r.chance(15) {
        "NULL".into()
    } else {
        format!("'{}'", r.pick(&["a", "b", "c", "ab", "B", "ba", ""]))
    }
}

fn cond(r: &mut Rng, alias: &str, depth: u32) -> String {
    if depth > 0 && r.chance(30) {
        let op = r.pick(&["AND", "OR"]);
        return format!(
            "({} {op} {})",
            cond(r, alias, depth - 1),
            cond(r, alias, depth - 1)
        );
    }
    if depth > 0 && r.chance(10) {
        return format!("NOT ({})", cond(r, alias, depth - 1));
    }
    let (col, txt) = *r.pick(&[("x", false), ("y", false), ("s", true)]);
    let lit = |r: &mut Rng| if txt { text_or_null(r) } else { int_or_null(r) };
    match r.below(8) {
        0 => format!("{alias}{col} IS NULL"),
        1 => format!("{alias}{col} IS NOT NULL"),
        2 => format!("{alias}{col} BETWEEN {} AND {}", lit(r), lit(r)),
        3 => format!("{alias}{col} IN ({}, {})", lit(r), lit(r)),
        4 if !txt => format!("{alias}x + {alias}y > {}", r.range(-2, 15)),
        5 => format!("COALESCE({alias}{col}, {}) = {}", lit(r), lit(r)),
        _ => format!(
            "{alias}{col} {} {}",
            r.pick(&["=", "<>", "<", "<=", ">", ">="]),
            lit(r)
        ),
    }
}

fn query(r: &mut Rng) -> (String, bool) {
    let w = cond(r, "", 2);
    match r.below(12) {
        0 => (format!("SELECT id, x, y, s FROM t WHERE {w} ORDER BY id"), true),
        1 => (format!("SELECT x, COUNT(*), SUM(y), MIN(s), MAX(y) FROM t WHERE {w} GROUP BY x"), false),
        2 => (format!("SELECT s, COUNT(y), AVG(x) FROM t GROUP BY s HAVING COUNT(*) > {}", r.range(0, 3)), false),
        3 => (format!("SELECT DISTINCT x FROM t WHERE {w}"), false),
        4 => ("SELECT t.id, u.v FROM t JOIN u ON u.tid = t.id".to_string(), false),
        5 => (format!("SELECT t.id, u.v FROM t LEFT JOIN u ON u.tid = t.id AND u.v > {} ORDER BY t.id, u.v", r.range(0, 5)), false),
        6 => (format!("SELECT id FROM t WHERE x IN (SELECT tid FROM u WHERE v < {})", r.range(0, 6)), false),
        7 => ("SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.tid = t.id AND u.v > t.y)".to_string(), false),
        8 => (format!("SELECT x FROM t WHERE {w} UNION SELECT tid FROM u"), false),
        9 => (format!("SELECT y FROM t WHERE {w} UNION ALL SELECT v FROM u WHERE v > 2"), false),
        10 => ("SELECT id, CASE WHEN x > y THEN 'gt' WHEN x = y THEN 'eq' ELSE 'other' END, COALESCE(s, 'none') FROM t".to_string(), false),
        _ => (format!("SELECT COUNT(*), SUM(x), (SELECT COUNT(*) FROM u) FROM t WHERE {w}"), false),
    }
}

#[test]
fn random_queries_match_sqlite() {
    if Command::new("python3")
        .arg("-c")
        .arg("import sqlite3")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("SKIPPED: python3 with sqlite3 is not available");
        return;
    }
    let cases = std::env::var("POLYDB_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(25u64);
    for case in 0..cases {
        let mut r = Rng::new(seed() * 9000 + case);
        let mut setup = vec![
            "CREATE TABLE t (id INT PRIMARY KEY, x INT, y INT, s TEXT)".to_string(),
            "CREATE TABLE u (id INT PRIMARY KEY, tid INT, v INT)".to_string(),
        ];
        for id in 0..r.range(0, 40) {
            setup.push(format!(
                "INSERT INTO t VALUES ({id}, {}, {}, {})",
                int_or_null(&mut r),
                int_or_null(&mut r),
                text_or_null(&mut r)
            ));
        }
        for id in 0..r.range(0, 40) {
            setup.push(format!(
                "INSERT INTO u VALUES ({id}, {}, {})",
                r.range(0, 30),
                int_or_null(&mut r)
            ));
        }
        // PolyDB gets indexes too, so index paths are compared against SQLite
        let mut db = Database::in_memory();
        for s in &setup {
            ok(&mut db, s);
        }
        if r.chance(60) {
            ok(&mut db, "CREATE INDEX t_x ON t (x, y)");
            ok(&mut db, "CREATE INDEX u_tid ON u (tid)");
        }
        let qs: Vec<(String, bool)> = (0..20).map(|_| query(&mut r)).collect();
        let theirs = sqlite(&setup, &qs.iter().map(|q| q.0.clone()).collect::<Vec<_>>())
            .expect("sqlite runner failed");
        for ((q, ordered), want) in qs.iter().zip(theirs) {
            let ours = match db.execute(q) {
                Ok(Output::Rows { rows, .. }) => Ok(rows),
                Ok(other) => panic!("{q}: unexpected {other:?}"),
                Err(e) => Err(e.to_string()),
            };
            match (ours, want) {
                (Ok(a), Ok(b)) => assert_eq!(
                    norm_rows(&a, *ordered),
                    norm_rows(&b, *ordered),
                    "case {case}: {q}"
                ),
                (Err(a), Err(_)) => panic!("case {case}: both engines failed {q}: {a}"),
                (a, b) => panic!("case {case}: {q}\n polydb: {a:?}\n sqlite: {b:?}"),
            }
        }
    }
}
