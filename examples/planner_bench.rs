//! Planner benchmark: full scans vs indexed equality, range, composite and
//! join execution on datasets of different cardinalities and distributions.
//!
//!     cargo run --release --example planner_bench [rows]
//!
//! For each query the indexed form is timed against the same predicate
//! written so no index can serve it (`col + 0 = v`), or for joins against
//! copies of the tables with no key or index (so only hash joins and full
//! scans are possible), and both results are compared before any time is
//! reported. Times are the median of several
//! runs on an in-memory database. The "plan" column is the access path or
//! join algorithm the planner chose (from EXPLAIN), and "est"/"act" are its
//! estimated and measured row counts (from EXPLAIN ANALYZE).

use polydb::{Database, Output, Value};
use std::time::Instant;

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{other:?}"),
    }
}

fn median_ms(db: &mut Database, sql: &str, runs: usize) -> (f64, usize) {
    let mut t = Vec::new();
    let mut n = 0;
    for _ in 0..runs {
        let s = Instant::now();
        n = rows(db, sql).len();
        t.push(s.elapsed().as_secs_f64() * 1e3);
    }
    t.sort_by(|a, b| a.total_cmp(b));
    (t[t.len() / 2], n)
}

fn canon(mut r: Vec<Vec<Value>>) -> Vec<String> {
    let mut v: Vec<String> = r.drain(..).map(|x| x.iter().map(|v| v.to_json()).collect::<Vec<_>>().join("|")).collect();
    v.sort();
    v
}

/// The access path / join line of the plan, and its estimated and actual rows.
fn plan_info(db: &mut Database, sql: &str) -> (String, String, String) {
    let p: Vec<String> = rows(db, &format!("EXPLAIN ANALYZE {sql}")).into_iter().map(|r| r[0].display()).collect();
    let key = p
        .iter()
        .find(|l| l.contains("Join") || l.contains("Nested Loop"))
        .or_else(|| p.iter().find(|l| l.contains(" on ")))
        .cloned()
        .unwrap_or_default();
    let name = key.trim_start().trim_start_matches("-> ");
    let name = name.split("  (").next().unwrap_or("").to_string();
    let grab = |k: &str| {
        key.split(&format!("{k}="))
            .nth(1)
            .map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect())
            .unwrap_or_default()
    };
    (name, grab("est. rows"), grab("actual rows"))
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(100_000);
    let mut db = Database::in_memory();
    for s in [
        "CREATE TABLE t (id INT PRIMARY KEY, uni INT, grp INT, skew INT, flag BOOL, cat TEXT, score REAL, pad TEXT)",
        "CREATE UNIQUE INDEX t_uni ON t (uni)",
        "CREATE INDEX t_grp ON t (grp)",
        "CREATE INDEX t_skew ON t (skew)",
        "CREATE INDEX t_flag ON t (flag)",
        "CREATE INDEX t_score ON t (score)",
        "CREATE INDEX t_cat_score ON t (cat, score)",
        "CREATE TABLE d (id INT PRIMARY KEY, region TEXT, tier INT)",
        "CREATE INDEX d_region ON d (region)",
        // copies with no primary key and no indexes: joins on them can only hash
        "CREATE TABLE t_p (id INT, uni INT, grp INT, skew INT, flag BOOL, cat TEXT, score REAL, pad TEXT)",
        "CREATE TABLE d_p (id INT, region TEXT, tier INT)",
    ] {
        db.execute(s).unwrap();
    }
    let mut seed = 0x9e3779b97f4a7c15u64;
    let mut rnd = |m: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % m
    };
    let t0 = Instant::now();
    let mut batch = Vec::new();
    for i in 0..n {
        let mut skew = 0;
        while rnd(2) == 1 && skew < 16 {
            skew += 1;
        }
        let uni = (i as u64 * 1_000_003) % n as u64;
        batch.push(format!(
            "({i}, {uni}, {}, {skew}, {}, 'c{}', {}.{}, 'padding-{}')",
            rnd(1000),
            rnd(100) == 0,
            rnd(20),
            rnd(10_000),
            rnd(10),
            i % 97
        ));
        if batch.len() == 1000 {
            db.execute(&format!("INSERT INTO t VALUES {}", batch.join(","))).unwrap();
            db.execute(&format!("INSERT INTO t_p VALUES {}", batch.join(","))).unwrap();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        db.execute(&format!("INSERT INTO t VALUES {}", batch.join(","))).unwrap();
        db.execute(&format!("INSERT INTO t_p VALUES {}", batch.join(","))).unwrap();
    }
    let regions = ["north", "south", "east", "west", "central"];
    let dn = 1000;
    let d: Vec<String> = (0..dn).map(|i| format!("({i}, '{}', {})", regions[i % 5], i % 4)).collect();
    db.execute(&format!("INSERT INTO d VALUES {}", d.join(","))).unwrap();
    db.execute(&format!("INSERT INTO d_p VALUES {}", d.join(","))).unwrap();
    println!("loaded {n} rows in {:.0} ms", t0.elapsed().as_secs_f64() * 1e3);
    let t0 = Instant::now();
    rows(&mut db, "ANALYZE");
    println!("ANALYZE took {:.0} ms\n", t0.elapsed().as_secs_f64() * 1e3);

    // (label, indexed query, the same query without any usable index)
    let cases: Vec<(&str, String, String)> = vec![
        (
            "pk equality",
            "SELECT * FROM t WHERE id = 4242".into(),
            "SELECT * FROM t WHERE id + 0 = 4242".into(),
        ),
        (
            "unique index equality",
            "SELECT * FROM t WHERE uni = 777".into(),
            "SELECT * FROM t WHERE uni + 0 = 777".into(),
        ),
        (
            "index eq, 0.1% (grp)",
            "SELECT * FROM t WHERE grp = 17".into(),
            "SELECT * FROM t WHERE grp + 0 = 17".into(),
        ),
        (
            "index eq, 1% (flag)",
            "SELECT * FROM t WHERE flag = true".into(),
            "SELECT * FROM t WHERE NOT (flag = false)".into(),
        ),
        (
            "index eq, 5% (cat)",
            "SELECT * FROM t WHERE cat = 'c3' AND score >= 0".into(),
            "SELECT * FROM t WHERE cat || '' = 'c3'".into(),
        ),
        (
            "index eq, skewed rare (skew=8)",
            "SELECT * FROM t WHERE skew = 8".into(),
            "SELECT * FROM t WHERE skew + 0 = 8".into(),
        ),
        (
            "index eq, skewed common (skew=0)",
            "SELECT * FROM t WHERE skew = 0".into(),
            "SELECT * FROM t WHERE skew + 0 = 0".into(),
        ),
        (
            "IN list on index",
            "SELECT * FROM t WHERE grp IN (1, 2, 3, 4)".into(),
            "SELECT * FROM t WHERE grp + 0 IN (1, 2, 3, 4)".into(),
        ),
        (
            "pk range, 1%",
            format!("SELECT * FROM t WHERE id BETWEEN 5000 AND {}", 5000 + n / 100),
            format!("SELECT * FROM t WHERE id + 0 BETWEEN 5000 AND {}", 5000 + n / 100),
        ),
        (
            "index range, 0.5%",
            "SELECT * FROM t WHERE score > 9950".into(),
            "SELECT * FROM t WHERE score + 0 > 9950".into(),
        ),
        (
            "index range, 10%",
            "SELECT * FROM t WHERE score < 1000".into(),
            "SELECT * FROM t WHERE score + 0 < 1000".into(),
        ),
        (
            "index range, 60%",
            "SELECT * FROM t WHERE score > 4000".into(),
            "SELECT * FROM t WHERE score + 0 > 4000".into(),
        ),
        (
            "composite eq + range",
            "SELECT * FROM t WHERE cat = 'c7' AND score BETWEEN 100 AND 600".into(),
            "SELECT * FROM t WHERE cat || '' = 'c7' AND score + 0 BETWEEN 100 AND 600".into(),
        ),
        (
            "composite eq + eq",
            "SELECT * FROM t WHERE cat = 'c7' AND score = 123.4".into(),
            "SELECT * FROM t WHERE cat || '' = 'c7' AND score + 0 = 123.4".into(),
        ),
        (
            "join, selective dimension",
            "SELECT t.id FROM t JOIN d ON d.id = t.grp WHERE d.region = 'east' AND d.tier = 1".into(),
            "SELECT t.id FROM t_p t JOIN d_p d ON d.id = t.grp WHERE d.region = 'east' AND d.tier = 1".into(),
        ),
        (
            "join, point lookup on dimension",
            "SELECT * FROM d JOIN t ON t.grp = d.id WHERE d.id = 5".into(),
            "SELECT * FROM d_p d JOIN t_p t ON t.grp = d.id WHERE d.id = 5".into(),
        ),
        (
            "join, filtered fact probes dimension pk",
            "SELECT * FROM t JOIN d ON d.id = t.grp WHERE t.score > 9990".into(),
            "SELECT * FROM t_p t JOIN d_p d ON d.id = t.grp WHERE t.score > 9990".into(),
        ),
        (
            "join, large table written first",
            "SELECT COUNT(*) FROM t JOIN d ON d.id = t.grp WHERE d.id < 10".into(),
            "SELECT COUNT(*) FROM t_p t JOIN d_p d ON d.id = t.grp WHERE d.id < 10".into(),
        ),
        (
            "join, both large (hash)",
            "SELECT COUNT(*) FROM t a JOIN t b ON b.id = a.uni".into(),
            "SELECT COUNT(*) FROM t_p a JOIN t_p b ON b.id = a.uni".into(),
        ),
    ];
    println!(
        "{:<38} {:>7} {:>10} {:>10} {:>8}  {:<58} {:>7} {:>7}",
        "query", "rows", "planned", "no index", "speedup", "plan", "est", "act"
    );
    for (label, fast, slow) in &cases {
        let a = canon(rows(&mut db, fast));
        let b = canon(rows(&mut db, slow));
        assert_eq!(a, b, "results differ for {label}");
        let runs = 5;
        let (tf, nrows) = median_ms(&mut db, fast, runs);
        let (ts, _) = median_ms(&mut db, slow, runs);
        let (p, est, act) = plan_info(&mut db, fast);
        println!(
            "{label:<38} {nrows:>7} {tf:>8.2}ms {ts:>8.2}ms {:>7.1}x  {p:<58} {est:>7} {act:>7}",
            ts / tf
        );
    }
}
