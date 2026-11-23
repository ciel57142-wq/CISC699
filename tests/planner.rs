//! Query planner and index tests: access-path choice, statistics and
//! estimates, predicate pushdown, join ordering and algorithms, EXPLAIN /
//! EXPLAIN ANALYZE output, index consistency under modification, and index
//! repair after recovery.
//!
//! Correctness first: every indexed or reordered query is compared with the
//! same query over an identical copy of the data that has no indexes (so it
//! can only be answered by full scans and the written join order).

use polydb::storage::{Store, WriteSet};
use polydb::{Database, Output, Value};
use std::fs;
use std::path::PathBuf;

// ------------------------------------------------------------------ helpers

fn ok(db: &mut Database, sql: &str) -> Output {
    db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match ok(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows from {sql}, got {other:?}"),
    }
}

fn sorted(db: &mut Database, sql: &str) -> Vec<String> {
    let mut v: Vec<String> = rows(db, sql)
        .iter()
        .map(|r| r.iter().map(|x| x.to_json()).collect::<Vec<_>>().join("|"))
        .collect();
    v.sort();
    v
}

fn plan(db: &mut Database, sql: &str) -> String {
    rows(db, &format!("EXPLAIN {sql}"))
        .into_iter()
        .map(|r| r[0].display())
        .collect::<Vec<_>>()
        .join("\n")
}

fn analyze_plan(db: &mut Database, sql: &str) -> String {
    rows(db, &format!("EXPLAIN ANALYZE {sql}"))
        .into_iter()
        .map(|r| r[0].display())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `key=<number>` from a plan line.
fn num(line: &str, key: &str) -> Option<f64> {
    let at = line.find(&format!("{key}="))? + key.len() + 1;
    let digits: String = line[at..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    digits.parse().ok()
}

/// The first plan line containing `needle`.
fn line<'p>(plan: &'p str, needle: &str) -> &'p str {
    plan.lines()
        .find(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?} in\n{plan}"))
}

fn consistent(db: &mut Database) {
    for r in rows(db, "CHECK TABLES") {
        assert_eq!(r[2], Value::Text("ok".into()), "integrity check failed: {r:?}");
    }
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-planner-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    p
}

/// Deterministic pseudo-random numbers (xorshift64*).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

// ------------------------------------------------------------------ dataset

const N: usize = 3000;

/// `facts` (indexed) and `plain` (identical rows, no secondary indexes),
/// with columns of very different cardinalities and distributions:
///
/// * `id`    primary key
/// * `uni`   unique, a permutation of 0..N
/// * `grp`   100 values, uniform
/// * `skew`  geometric: half the rows 0, a quarter 1, an eighth 2, ...
/// * `flag`  true for 2% of rows
/// * `cat`   10 text values
/// * `score` uniform 0..1000 with 10% NULL
fn dataset(db: &mut Database) {
    let cols = "id INT PRIMARY KEY, uni INT, grp INT, skew INT, flag BOOL, cat TEXT, score REAL";
    ok(db, &format!("CREATE TABLE facts ({cols})"));
    ok(db, &format!("CREATE TABLE plain ({cols})"));
    for ix in [
        "CREATE UNIQUE INDEX facts_uni ON facts (uni)",
        "CREATE INDEX facts_grp ON facts (grp)",
        "CREATE INDEX facts_skew ON facts (skew)",
        "CREATE INDEX facts_flag ON facts (flag)",
        "CREATE INDEX facts_score ON facts (score)",
        "CREATE INDEX facts_cat_score ON facts (cat, score)",
    ] {
        ok(db, ix);
    }
    let mut rng = Rng(42);
    let mut vals = Vec::new();
    for i in 0..N {
        let uni = (i * 7919) % N; // 7919 is prime and coprime with N
        let grp = rng.below(100);
        let mut skew = 0;
        while rng.below(2) == 1 && skew < 12 {
            skew += 1;
        }
        let flag = rng.below(50) == 0;
        let cat = format!("c{}", rng.below(10));
        let score = if rng.below(10) == 0 {
            "NULL".to_string()
        } else {
            format!("{}.{}", rng.below(1000), rng.below(10))
        };
        vals.push(format!("({i}, {uni}, {grp}, {skew}, {flag}, '{cat}', {score})"));
    }
    for chunk in vals.chunks(500) {
        ok(db, &format!("INSERT INTO facts VALUES {}", chunk.join(",")));
        ok(db, &format!("INSERT INTO plain VALUES {}", chunk.join(",")));
    }
}

const PREDICATES: &[&str] = &[
    "id = 17",
    "id = 17.0",
    "id IN (5, 3, 3, 99999, 2)",
    "id BETWEEN 100 AND 140",
    "id > 2990",
    "id >= 2990 AND id < 2995",
    "uni = 1234",
    "uni = 1234 AND grp = 5",
    "grp = 42",
    "grp IN (1, 2, 3)",
    "grp > 97",
    "grp >= 10 AND grp <= 11",
    "skew = 0",
    "skew = 9",
    "skew > 3",
    "flag = true",
    "flag = false",
    "cat = 'c3'",
    "cat = 'c3' AND score > 900",
    "cat = 'c3' AND score >= 100 AND score < 105",
    "cat IN ('c1', 'c2') AND score < 50",
    "score > 995",
    "score < 3",
    "score = 500.5",
    "score IS NULL",
    "score IS NULL AND grp = 7",
    "score > 999.9",
    "score >= 10 AND score <= 9",
    "grp = 'x'",
    "cat = 3",
    "grp = 42 OR skew = 7",
    "NOT (grp < 99)",
];

// ============================================================ correctness

#[test]
fn indexed_lookups_return_exactly_what_full_scans_return() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    for round in 0..2 {
        for p in PREDICATES {
            let a = sorted(&mut db, &format!("SELECT * FROM facts WHERE {p}"));
            let b = sorted(&mut db, &format!("SELECT * FROM plain WHERE {p}"));
            assert_eq!(a, b, "round {round}: {p}");
            assert!(
                plan(&mut db, &format!("SELECT * FROM plain WHERE {p}")).contains("Seq Scan") || p.starts_with("id"),
                "{p}"
            );
        }
        ok(&mut db, "ANALYZE");
    }
    consistent(&mut db);
}

#[test]
fn access_paths_follow_the_predicates_and_the_statistics() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    let path = |db: &mut Database, w: &str| {
        let p = plan(db, &format!("SELECT * FROM facts WHERE {w}"));
        line(&p, " on facts").to_string()
    };
    // without statistics: any usable key or index beats a scan
    assert!(
        path(&mut db, "id = 17").starts_with("   -> Primary Key Lookup on facts"),
        "{}",
        path(&mut db, "id = 17")
    );
    assert!(path(&mut db, "id IN (1, 2)").contains("Primary Key IN Lookup"));
    assert!(path(&mut db, "id > 2990").contains("Primary Key Range Scan"));
    assert!(path(&mut db, "uni = 1234 AND grp = 5").contains("using facts_uni"));
    assert!(path(&mut db, "grp = 42").contains("Index Scan on facts using facts_grp"));
    assert!(path(&mut db, "grp IN (1, 2, 3)").contains("Index IN Scan on facts using facts_grp"));
    assert!(path(&mut db, "skew = 0").contains("using facts_skew"));
    assert!(path(&mut db, "flag = false").contains("using facts_flag"));
    assert!(path(&mut db, "score > 10").contains("Index Range Scan on facts using facts_score"));
    assert!(path(&mut db, "cat = 'c3' AND score > 900").contains("using facts_cat_score"));
    assert!(path(&mut db, "cat IN ('c1', 'c2') AND score < 50").contains("Index IN Scan on facts using facts_cat_score"));
    assert!(path(&mut db, "grp + 0 = 42").contains("Seq Scan"));
    assert!(path(&mut db, "grp = 'x'").contains("Seq Scan"));

    // with statistics: low-selectivity predicates read the table instead
    ok(&mut db, "ANALYZE facts");
    assert!(
        path(&mut db, "skew < 2").contains("Seq Scan"),
        "three quarters of the table: {}",
        path(&mut db, "skew < 2")
    );
    assert!(
        path(&mut db, "skew = 0").contains("using facts_skew"),
        "half the table: still cheaper by index"
    );
    assert!(path(&mut db, "skew = 9").contains("using facts_skew"));
    assert!(path(&mut db, "flag = false").contains("Seq Scan"));
    assert!(path(&mut db, "flag = true").contains("using facts_flag"));
    assert!(path(&mut db, "score > 10").contains("Seq Scan"));
    assert!(path(&mut db, "score > 990").contains("using facts_score"));
    assert!(path(&mut db, "grp = 42").contains("using facts_grp"));
    // the narrower of two usable indexes wins
    assert!(path(&mut db, "skew = 0 AND grp = 42").contains("using facts_grp"));
    assert!(path(&mut db, "skew = 11 AND grp = 42").contains("using facts_skew"));
    // equality on the composite index's first column plus a range on the second
    let p = plan(&mut db, "SELECT * FROM facts WHERE cat = 'c3' AND score >= 100 AND score < 105");
    assert!(p.contains("Index Scan on facts using facts_cat_score"), "{p}");
    assert!(p.contains("Index Cond: cat = 'c3' AND score >= 100 AND score < 105"), "{p}");
    // statistics are dropped by ALTER TABLE and the defaults apply again
    ok(&mut db, "ALTER TABLE facts ADD COLUMN extra INT");
    assert!(path(&mut db, "skew = 0").contains("using facts_skew"));
    assert!(plan(&mut db, "SELECT * FROM facts").contains("facts (not analyzed: default estimates)"));
}

#[test]
fn estimates_track_the_data_after_analyze() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    ok(&mut db, "ANALYZE");
    for (w, tolerance) in [
        ("skew = 0", 0.15),
        ("skew = 1", 0.2),
        ("flag = true", 0.35),
        ("grp = 42", 0.6),
        ("score > 900", 0.15),
        ("score BETWEEN 100 AND 300", 0.15),
        ("score IS NULL", 0.15),
        ("cat = 'c3'", 0.2),
        ("id < 1500", 0.05),
    ] {
        let p = analyze_plan(&mut db, &format!("SELECT * FROM facts WHERE {w}"));
        let l = line(&p, " on facts");
        let est = num(l, "est. rows").unwrap();
        let act = num(l, "actual rows").unwrap();
        let err = (est - act).abs() / act.max(1.0);
        assert!(err <= tolerance, "{w}: estimated {est}, actual {act}\n{p}");
    }
    // without statistics the defaults are used, and say so
    ok(&mut db, "CREATE TABLE fresh (a INT)");
    let p = plan(&mut db, "SELECT * FROM fresh WHERE a = 1");
    assert!(p.contains("fresh (not analyzed: default estimates)"), "{p}");
}

// ======================================================= EXPLAIN / ANALYZE

#[test]
fn explain_shows_operators_and_estimates_but_no_measurements() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    let p = plan(
        &mut db,
        "SELECT cat, COUNT(*) AS n FROM facts WHERE grp = 3 AND score > 10 GROUP BY cat HAVING COUNT(*) > 1 ORDER BY n DESC LIMIT 3",
    );
    for want in [
        "Limit  (est. rows=",
        "Limit: 3",
        "-> Sort",
        "Sort Key: n DESC",
        "-> Hash Aggregate",
        "Group Key: cat",
        "Having: count(*) > 1",
        "-> Index Scan on facts using facts_grp",
        "Index Cond: grp = 3",
        "Filter: grp = 3 AND score > 10",
        "Statistics: facts (not analyzed",
    ] {
        assert!(p.contains(want), "missing {want:?} in\n{p}");
    }
    assert!(!p.contains("actual"), "EXPLAIN must not report measurements:\n{p}");
    assert!(!p.contains("time="), "{p}");
    // identical branches are separate operators, each run once
    let p = analyze_plan(&mut db, "SELECT grp FROM facts WHERE id < 5 UNION ALL SELECT grp FROM facts WHERE id < 5");
    let scans: Vec<&str> = p.lines().filter(|l| l.contains("Primary Key Range Scan on facts")).collect();
    assert_eq!(scans.len(), 2, "{p}");
    for l in scans {
        assert_eq!(num(l, "loops"), Some(1.0), "{p}");
        assert_eq!(num(l, "actual rows"), Some(5.0), "{p}");
    }
    let p = plan(&mut db, "SELECT DISTINCT grp FROM facts WHERE id < 10 UNION SELECT 1");
    for want in ["Union", "-> Distinct", "-> Result", "Primary Key Range Scan on facts"] {
        assert!(p.contains(want), "missing {want:?} in\n{p}");
    }
}

#[test]
fn explain_analyze_reports_measured_rows_work_and_time() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    let total = rows(&mut db, "SELECT * FROM facts WHERE grp = 42").len() as f64;

    // full scan: examines every row, returns the matches
    let p = analyze_plan(&mut db, "SELECT * FROM facts WHERE grp + 0 = 42");
    let scan = line(&p, "Seq Scan on facts");
    assert_eq!(num(scan, "examined"), Some(N as f64), "{p}");
    assert_eq!(num(scan, "actual rows"), Some(total), "{p}");
    assert_eq!(num(scan, "loops"), Some(1.0));
    assert!(scan.contains("time="), "{p}");
    assert!(line(&p, "Execution time:").contains(&format!("({total} row(s) returned)")), "{p}");

    // index scan: reads only the matching entries and rows
    let p = analyze_plan(&mut db, "SELECT * FROM facts WHERE grp = 42");
    let scan = line(&p, "Index Scan on facts using facts_grp");
    assert_eq!(num(scan, "index_entries"), Some(total), "{p}");
    assert_eq!(num(scan, "examined"), Some(total), "{p}");
    assert_eq!(num(scan, "actual rows"), Some(total), "{p}");
    // the top operator's measured rows are what the query returned
    assert_eq!(num(p.lines().next().unwrap(), "actual rows"), Some(total), "{p}");

    // a primary-key lookup reads one row; a miss reads none
    let p = analyze_plan(&mut db, "SELECT * FROM facts WHERE id = 7");
    assert_eq!(num(line(&p, "Primary Key Lookup"), "examined"), Some(1.0), "{p}");
    let p = analyze_plan(&mut db, "SELECT * FROM facts WHERE id = -7");
    let l = line(&p, "Primary Key Lookup");
    assert_eq!(num(l, "actual rows"), Some(0.0), "{p}");
    assert_eq!(num(l, "examined"), None, "nothing was read:\n{p}");

    // an operator that never ran is reported as such, not with estimates
    let p = analyze_plan(&mut db, "SELECT * FROM facts f JOIN plain p ON p.id = f.id WHERE f.id = -1");
    assert!(p.contains("(never executed)"), "{p}");
    // estimates and measurements stay separate on every executed line
    for l in p.lines().filter(|l| l.contains("actual rows=")) {
        assert!(l.contains("(est. rows="), "{l}");
    }
}

#[test]
fn explain_analyze_counts_correlated_subplan_loops() {
    let mut db = Database::in_memory();
    dataset(&mut db);
    let outer = rows(&mut db, "SELECT * FROM facts WHERE id < 50").len();
    let p = analyze_plan(
        &mut db,
        "SELECT id FROM facts f WHERE id < 50 AND EXISTS (SELECT 1 FROM facts g WHERE g.grp = f.grp AND g.id <> f.id)",
    );
    let sub = line(&p, "SubPlan (correlated, evaluated per row)");
    assert_eq!(num(sub, "loops"), Some(outer as f64), "{p}");
    // the subquery's predicate on the outer row uses the index
    let l = line(&p, "Index Scan on facts AS g using facts_grp");
    assert_eq!(num(l, "loops"), Some(outer as f64), "{p}");
    assert!(p.contains("Index Cond: grp = f.grp"), "{p}");
    let p = analyze_plan(&mut db, "SELECT id FROM facts WHERE grp IN (SELECT grp FROM plain WHERE id < 3)");
    assert_eq!(num(line(&p, "SubPlan (uncorrelated, evaluated once)"), "loops"), Some(1.0), "{p}");
}

// ================================================================== joins

fn join_dataset(db: &mut Database, indexed: bool) {
    let sfx = if indexed { "" } else { "_p" };
    ok(db, &format!("CREATE TABLE cust{sfx} (id INT PRIMARY KEY, region TEXT, tier INT)"));
    ok(
        db,
        &format!("CREATE TABLE ord{sfx} (id INT PRIMARY KEY, cust INT, amount INT, kind TEXT)"),
    );
    ok(
        db,
        &format!("CREATE TABLE line{sfx} (ord INT, n INT, sku INT, qty INT, PRIMARY KEY (ord, n))"),
    );
    ok(db, &format!("CREATE TABLE sku{sfx} (sku INT, label TEXT)"));
    if indexed {
        ok(db, "CREATE INDEX ord_cust ON ord (cust)");
        ok(db, "CREATE INDEX cust_region ON cust (region)");
        ok(db, "CREATE UNIQUE INDEX sku_sku ON sku (sku)");
    }
    let mut rng = Rng(7);
    let regions = ["north", "south", "east", "west"];
    let c: Vec<String> = (1..=400).map(|i| format!("({i}, '{}', {})", regions[i % 4], i % 3)).collect();
    ok(db, &format!("INSERT INTO cust{sfx} VALUES {}", c.join(",")));
    let mut o = Vec::new();
    let mut l = Vec::new();
    for i in 1..=2000 {
        // a few orders point at customers that do not exist, some have no customer
        let cust = if i % 97 == 0 {
            "NULL".to_string()
        } else {
            (rng.below(410) + 1).to_string()
        };
        o.push(format!(
            "({i}, {cust}, {}, '{}')",
            rng.below(1000),
            if i % 5 == 0 { "refund" } else { "sale" }
        ));
        for n in 0..(i % 4) {
            l.push(format!("({i}, {n}, {}, {})", rng.below(60), rng.below(9) + 1));
        }
    }
    for chunk in o.chunks(500) {
        ok(db, &format!("INSERT INTO ord{sfx} VALUES {}", chunk.join(",")));
    }
    for chunk in l.chunks(500) {
        ok(db, &format!("INSERT INTO line{sfx} VALUES {}", chunk.join(",")));
    }
    let s: Vec<String> = (0..50).map(|i| format!("({i}, 'sku{i}')")).collect();
    ok(db, &format!("INSERT INTO sku{sfx} VALUES {}", s.join(",")));
}

const JOINS: &[&str] = &[
    "SELECT * FROM {ord} JOIN {cust} ON cust.id = ord.cust",
    "SELECT * FROM {cust} JOIN {ord} ON cust.id = ord.cust WHERE cust.region = 'north' AND ord.amount > 900",
    "SELECT * FROM {ord o} JOIN {cust c} ON c.id = o.cust WHERE c.id = 17",
    "SELECT o.id, l.n, s.label FROM {ord o} JOIN {line l} ON l.ord = o.id JOIN {sku s} ON s.sku = l.sku WHERE o.kind = 'refund' AND o.amount < 100",
    "SELECT * FROM {line l} JOIN {ord o} ON o.id = l.ord JOIN {cust c} ON c.id = o.cust WHERE c.tier = 2 AND l.qty > 7",
    "SELECT * FROM {sku s} JOIN {line l} ON l.sku = s.sku JOIN {ord o} ON o.id = l.ord WHERE s.sku IN (3, 4) AND o.amount > 500",
    "SELECT * FROM {cust c}, {sku s} WHERE c.id < 4 AND s.sku > 47",
    "SELECT * FROM {cust c} JOIN {ord o} ON o.cust = c.id AND o.amount > c.tier * 300",
    "SELECT c.id, COUNT(*), SUM(o.amount) FROM {cust c} JOIN {ord o} ON o.cust = c.id GROUP BY c.id",
    "SELECT * FROM {cust c} LEFT JOIN {ord o} ON o.cust = c.id AND o.kind = 'refund' WHERE c.id < 30",
    "SELECT * FROM {cust c} LEFT JOIN {ord o} ON o.cust = c.id WHERE o.id IS NULL",
    "SELECT * FROM {ord o} LEFT JOIN {cust c} ON c.id = o.cust WHERE c.region = 'east' OR c.id IS NULL",
    "SELECT * FROM {cust c} RIGHT JOIN {ord o} ON o.cust = c.id WHERE o.amount < 20",
    "SELECT * FROM {cust c} FULL JOIN {ord o} ON o.cust = c.id WHERE c.id IS NULL OR o.id IS NULL",
    "SELECT * FROM {ord o} JOIN {line l} ON l.ord = o.id LEFT JOIN {sku s} ON s.sku = l.sku + 25 WHERE o.id < 200",
    "SELECT * FROM (SELECT cust, COUNT(*) AS n FROM {ord} GROUP BY cust) t JOIN {cust c} ON c.id = t.cust WHERE t.n > 7",
    "SELECT c.id FROM {cust c} WHERE NOT EXISTS (SELECT 1 FROM {ord o} WHERE o.cust = c.id)",
    "SELECT c.id FROM {cust c} WHERE c.id IN (SELECT cust FROM {ord} WHERE amount > 990)",
];

/// Expand `{table}` / `{table alias}` to the indexed tables, or to their
/// unindexed copies (`cust_p`, ...), which can only be scanned.
fn expand(sql: &str, indexed: bool) -> String {
    let mut out = String::new();
    let mut rest = sql;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let j = rest[i..].find('}').unwrap() + i;
        let mut parts = rest[i + 1..j].split(' ');
        let t = parts.next().unwrap();
        let alias = parts.next().unwrap_or(t);
        out.push_str(&if indexed { format!("{t} {alias}") } else { format!("{t}_p {alias}") });
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn reordered_and_indexed_joins_return_what_the_written_order_returns() {
    let mut db = Database::in_memory();
    join_dataset(&mut db, true);
    join_dataset(&mut db, false);
    for round in 0..2 {
        for q in JOINS {
            let (a, b) = (expand(q, true), expand(q, false));
            assert_eq!(
                sorted(&mut db, &a),
                sorted(&mut db, &b),
                "round {round}: {a}\nvs {b}\n{}",
                plan(&mut db, &a)
            );
        }
        ok(&mut db, "ANALYZE");
    }
    // SELECT * keeps the written column order whatever the join order
    let r = rows(
        &mut db,
        "SELECT * FROM ord o JOIN cust c ON c.id = o.cust WHERE c.id = 17 AND o.amount >= 0",
    );
    assert!(!r.is_empty());
    for row in &r {
        assert_eq!(row[1], row[4], "ord columns first, then cust: {row:?}");
    }
    assert!(plan(&mut db, "SELECT * FROM ord o JOIN cust c ON c.id = o.cust WHERE c.id = 17").contains("Primary Key Lookup on cust AS c"));
}

#[test]
fn join_order_and_algorithms_follow_the_estimates() {
    let mut db = Database::in_memory();
    join_dataset(&mut db, true);
    ok(&mut db, "ANALYZE");
    // the selective side drives index lookups into the other, whatever the written order
    for q in [
        "SELECT * FROM ord o JOIN cust c ON c.id = o.cust WHERE c.id = 17",
        "SELECT * FROM cust c JOIN ord o ON c.id = o.cust WHERE c.id = 17",
    ] {
        let p = plan(&mut db, q);
        assert!(p.contains("Index Nested Loop (inner)"), "{q}\n{p}");
        assert!(p.contains("Index Probe on ord AS o using ord_cust"), "{q}\n{p}");
        assert!(
            p.find("Primary Key Lookup on cust").unwrap() < p.find("Index Probe on ord").unwrap(),
            "{p}"
        );
    }
    // two large unfiltered inputs: one hash join instead of one lookup per row
    let p = plan(&mut db, "SELECT * FROM ord o JOIN line l ON l.ord = o.id");
    assert!(p.contains("Hash Join (inner)"), "{p}");
    assert!(p.contains("Hash Cond:"), "{p}");
    // a filtered fact table probes the primary key of its dimension
    let p = plan(&mut db, "SELECT * FROM line l JOIN ord o ON o.id = l.ord WHERE l.ord = 1500");
    assert!(p.contains("Index Probe on ord AS o using primary key"), "{p}");
    // three tables: start from the most selective, and never form a cross product
    let p = plan(
        &mut db,
        "SELECT * FROM line l JOIN ord o ON o.id = l.ord JOIN sku s ON s.sku = l.sku WHERE s.sku = 3",
    );
    let first_scan = p.lines().find(|l| l.contains(" on ")).unwrap();
    assert!(first_scan.contains("sku AS s"), "{p}");
    assert!(!p.contains("Nested Loop (cross)"), "{p}");
    // no equality between the sides: a nested loop
    let p = plan(&mut db, "SELECT * FROM cust c JOIN sku s ON s.sku < c.tier");
    assert!(p.contains("Nested Loop (inner)"), "{p}");
    // estimated join cardinality is in the right range
    let p = analyze_plan(&mut db, "SELECT * FROM ord o JOIN cust c ON c.id = o.cust");
    let l = line(&p, "Join (inner)");
    let (est, act) = (num(l, "est. rows").unwrap(), num(l, "actual rows").unwrap());
    assert!(est > act * 0.7 && est < act * 1.4, "estimated {est}, actual {act}\n{p}");
}

#[test]
fn predicates_are_pushed_below_joins_only_where_that_is_safe() {
    let mut db = Database::in_memory();
    join_dataset(&mut db, true);
    // inner join: each table's own condition filters it while it is read
    let p = plan(
        &mut db,
        "SELECT * FROM cust c JOIN ord o ON o.cust = c.id WHERE c.region = 'north' AND o.kind = 'refund'",
    );
    assert!(p.contains("Index Scan on cust AS c using cust_region"), "{p}");
    assert!(p.contains("Filter: o.kind = 'refund'"), "{p}");
    assert!(
        !p.lines().any(|l| l.trim_start().starts_with("Filter  (")),
        "nothing left above the join:\n{p}"
    );
    // LEFT JOIN: a WHERE condition on the NULL-extended side stays above the join
    let p = plan(&mut db, "SELECT * FROM cust c LEFT JOIN ord o ON o.cust = c.id WHERE o.id IS NULL");
    let filter_at = p.find("Filter: o.id IS NULL").expect(&p);
    assert!(filter_at < p.find("Join (left)").unwrap(), "{p}");
    // ... but the preserved side, and ON conditions on the inner side, are pushed
    let p = plan(
        &mut db,
        "SELECT * FROM cust c LEFT JOIN ord o ON o.cust = c.id AND o.kind = 'sale' WHERE c.region = 'west'",
    );
    assert!(p.contains("Index Scan on cust AS c using cust_region"), "{p}");
    assert!(p.contains("Filter: o.kind = 'sale'"), "{p}");
    // RIGHT JOIN: the left side can be NULL-extended, so its WHERE stays above
    let p = plan(&mut db, "SELECT * FROM cust c RIGHT JOIN ord o ON o.cust = c.id WHERE c.region = 'west'");
    assert!(p.find("Filter: c.region = 'west'").unwrap() < p.find("Join (right)").unwrap(), "{p}");
}

#[test]
fn pushed_conditions_keep_runtime_error_semantics() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE a (id INT PRIMARY KEY, d INT)");
    ok(&mut db, "CREATE TABLE b (id INT PRIMARY KEY, d INT)");
    ok(&mut db, "INSERT INTO a VALUES (1, 1), (2, 2)");
    ok(&mut db, "INSERT INTO b VALUES (1, 5), (2, 0), (3, 0)");
    // division by zero on a row that is returned is still an error
    assert!(db.execute("SELECT * FROM b WHERE 10 / d > 1").is_err());
    assert!(db.execute("SELECT * FROM a JOIN b ON a.id = b.id WHERE 10 / b.d > 1").is_err());
    // a row the join removes cannot fail the WHERE clause (b.id = 3 has no partner)
    ok(&mut db, "DELETE FROM b WHERE id = 2");
    assert_eq!(
        rows(&mut db, "SELECT a.id FROM a JOIN b ON a.id = b.id WHERE 10 / b.d > 1"),
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(
        rows(&mut db, "SELECT a.id FROM b JOIN a ON a.id = b.id WHERE 10 / b.d > 1"),
        vec![vec![Value::Int(1)]]
    );
    // a non-boolean condition is still a type error
    assert!(db.execute("SELECT * FROM a WHERE d").is_err());
}

// ====================================================== index maintenance

#[test]
fn indexes_stay_consistent_under_random_modifications() {
    let mut db = Database::in_memory();
    for t in ["ix", "px"] {
        ok(&mut db, &format!("CREATE TABLE {t} (id INT PRIMARY KEY, a INT, b TEXT, c REAL)"));
    }
    ok(&mut db, "CREATE INDEX ix_a ON ix (a)");
    ok(&mut db, "CREATE INDEX ix_ba ON ix (b, a)");
    ok(&mut db, "CREATE INDEX ix_c ON ix (c)");
    ok(&mut db, "CREATE UNIQUE INDEX ix_bc ON ix (b, c)");
    let mut rng = Rng(99);
    let mut next_id = 0;
    for step in 0..60 {
        let stmt = match rng.below(6) {
            0 | 1 => {
                let vals: Vec<String> = (0..20)
                    .map(|_| {
                        next_id += 1;
                        let c = if rng.below(8) == 0 { "NULL".into() } else { format!("{}.25", next_id) };
                        format!("({next_id}, {}, 'b{}', {c})", rng.below(15), rng.below(6))
                    })
                    .collect();
                format!("INSERT INTO T VALUES {}", vals.join(","))
            }
            2 => format!("UPDATE T SET a = a + 1, b = b || 'x' WHERE a = {}", rng.below(15)),
            3 => format!("UPDATE T SET id = id + 100000, c = NULL WHERE id % 13 = {}", rng.below(13)),
            4 => format!("DELETE FROM T WHERE b = 'b{}' AND a > {}", rng.below(6), rng.below(15)),
            _ => format!(
                "UPDATE T SET c = c + 1000 WHERE c BETWEEN {} AND {}",
                rng.below(500),
                rng.below(500) + 300
            ),
        };
        // half the steps run in a transaction that is rolled back first
        if step % 2 == 1 {
            ok(&mut db, "BEGIN");
            ok(&mut db, &stmt.replace(" T ", " ix "));
            ok(&mut db, "ROLLBACK");
        }
        for t in ["ix", "px"] {
            ok(&mut db, &stmt.replace(" T ", &format!(" {t} ")));
        }
        if step % 10 == 9 {
            consistent(&mut db);
            if step == 29 {
                ok(&mut db, "ANALYZE");
            }
            for w in [
                "a = 3",
                "b = 'b2' AND a < 5",
                "b = 'b1x'",
                "c > 300",
                "c IS NULL",
                "a IN (1, 4, 9)",
                "b = 'b3' AND c = 41.25",
            ] {
                assert_eq!(
                    sorted(&mut db, &format!("SELECT * FROM ix WHERE {w}")),
                    sorted(&mut db, &format!("SELECT * FROM px WHERE {w}")),
                    "step {step}: {w}"
                );
            }
        }
    }
    assert_eq!(sorted(&mut db, "SELECT * FROM ix"), sorted(&mut db, "SELECT * FROM px"));
}

#[test]
fn damaged_indexes_are_rebuilt_when_the_database_is_reopened_verified() {
    let dir = tmp("repair");
    {
        let mut db = Database::open(&dir).unwrap();
        ok(&mut db, "CREATE TABLE items (id INT PRIMARY KEY, grp INT, name TEXT)");
        ok(&mut db, "CREATE TABLE other (id INT PRIMARY KEY, v INT)");
        ok(&mut db, "CREATE INDEX items_grp ON items (grp)");
        ok(&mut db, "CREATE INDEX items_name_grp ON items (name, grp)");
        ok(&mut db, "CREATE INDEX other_v ON other (v)");
        let vals: Vec<String> = (1..=500).map(|i| format!("({i}, {}, 'n{}')", i % 20, i % 7)).collect();
        ok(&mut db, &format!("INSERT INTO items VALUES {}", vals.join(",")));
        ok(&mut db, "INSERT INTO other VALUES (1, 10), (2, 20)");
        ok(&mut db, "ANALYZE");
    }
    // a consistent database: verification finds nothing and changes nothing
    {
        let (mut db, report) = Database::open_verified(&dir).unwrap();
        assert_eq!(report.tables_checked, 2);
        assert!(report.rebuilt.is_empty(), "{report:?}");
        consistent(&mut db);
    }
    // damage the stored index entries of `items` behind the engine's back:
    // drop every third entry and add one pointing at a row that does not exist
    {
        let mut store = Store::open(&dir).unwrap();
        let prefix = b"i\0t:items\0";
        let entries: Vec<Vec<u8>> = store.scan(prefix).map(|(k, _)| k.clone()).collect();
        assert!(entries.len() > 900);
        let mut ws = WriteSet::new();
        for k in entries.iter().step_by(3) {
            ws.insert(k.clone(), None);
        }
        let mut orphan = entries[0].clone();
        orphan.truncate(orphan.len() - 8);
        orphan.extend_from_slice(&[0x80, 0, 0, 0, 0, 0, 0x99, 0x99]);
        ws.insert(orphan, Some(Vec::new()));
        store.commit(ws).unwrap();
    }
    {
        let mut db = Database::open(&dir).unwrap();
        let bad: Vec<_> = rows(&mut db, "CHECK TABLE items")
            .into_iter()
            .filter(|r| r[2] != Value::Text("ok".into()))
            .collect();
        assert!(!bad.is_empty(), "the damage is visible");
        // and the index now gives wrong answers
        let via_index = rows(&mut db, "SELECT id FROM items WHERE grp = 3").len();
        assert!(via_index < 25, "{via_index}");
    }
    let (mut db, report) = Database::open_verified(&dir).unwrap();
    assert_eq!(report.rebuilt, vec!["items".to_string()], "{report:?}");
    assert!(report.problems.iter().any(|p| p.contains("missing")), "{report:?}");
    assert!(report.problems.iter().any(|p| p.contains("orphaned")), "{report:?}");
    consistent(&mut db);
    assert_eq!(rows(&mut db, "SELECT id FROM items WHERE grp = 3").len(), 25);
    assert_eq!(rows(&mut db, "SELECT id FROM items WHERE name = 'n2' AND grp = 2").len(), 4);
    drop(db);
    // the repair was committed: a plain reopen sees consistent indexes
    let mut db = Database::open(&dir).unwrap();
    consistent(&mut db);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn analyze_statement_and_statistics_lifecycle() {
    let mut db = Database::in_memory();
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    ok(&mut db, "INSERT INTO t VALUES (1, 1), (2, 1), (3, 2)");
    match ok(&mut db, "ANALYZE t") {
        Output::Rows { columns, rows } => {
            assert_eq!(columns, vec!["table", "rows", "columns"]);
            assert_eq!(rows, vec![vec![Value::Text("t".into()), Value::Int(3), Value::Int(2)]]);
        }
        other => panic!("{other:?}"),
    }
    assert!(plan(&mut db, "SELECT * FROM t").contains("t (analyzed, 3 rows)"));
    assert_eq!(rows(&mut db, "ANALYZE TABLE t").len(), 1);
    assert!(db.execute("ANALYZE nope").is_err());
    // statistics never change results, and inside a transaction ANALYZE is
    // undone by ROLLBACK like any other write
    ok(&mut db, "BEGIN");
    ok(&mut db, "TRUNCATE t");
    assert!(plan(&mut db, "SELECT * FROM t").contains("t (not analyzed"));
    ok(&mut db, "ROLLBACK");
    assert!(plan(&mut db, "SELECT * FROM t").contains("t (analyzed, 3 rows)"));
    // a concurrent ANALYZE does not make a reading transaction conflict
    let mut other = db.session();
    ok(&mut db, "BEGIN");
    assert_eq!(rows(&mut db, "SELECT * FROM t WHERE v = 1").len(), 2);
    ok(&mut other, "ANALYZE t");
    ok(&mut db, "INSERT INTO t VALUES (4, 4)");
    ok(&mut db, "COMMIT");
    ok(&mut db, "DROP TABLE t");
    ok(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, v INT)");
    assert!(plan(&mut db, "SELECT * FROM t").contains("t (not analyzed"));
}
