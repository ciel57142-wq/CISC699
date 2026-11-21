//! Reproducible performance measurements (separate from correctness tests).
//!
//!     cargo bench --bench perf                      # small: 1,000 records
//!     POLYDB_DATASET=medium cargo bench --bench perf   # 100,000 (large, stress also accepted)
//!
//! Every workload reports operations, wall time, operations per second and,
//! for per-operation workloads, p50 / p95 / p99 latency. Peak memory (VmHWM),
//! database directory, WAL and index sizes are reported at the end, and the
//! relational workloads are repeated in SQLite (python3's sqlite3, timed in
//! Python) when available. Results are also written to
//! target/perf-results.json. Data and query parameters come from a fixed
//! seed, so runs are comparable.

use polydb::{Database, Value};
use std::time::{Duration, Instant};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

struct Result {
    name: String,
    ops: u64,
    total: Duration,
    lat: Vec<Duration>,
}

impl Result {
    fn pct(&self, p: f64) -> String {
        if self.lat.is_empty() {
            return "-".into();
        }
        let mut v = self.lat.clone();
        v.sort();
        let i = ((v.len() as f64 - 1.0) * p).round() as usize;
        format!("{:.1}", v[i].as_secs_f64() * 1e6)
    }
    fn ops_per_s(&self) -> f64 {
        self.ops as f64 / self.total.as_secs_f64().max(1e-9)
    }
}

/// Time `ops` calls of `f`, recording per-call latency.
fn per_op(name: &str, ops: u64, mut f: impl FnMut(u64)) -> Result {
    let mut lat = Vec::with_capacity(ops as usize);
    let start = Instant::now();
    for i in 0..ops {
        let t = Instant::now();
        f(i);
        lat.push(t.elapsed());
    }
    Result {
        name: name.into(),
        ops,
        total: start.elapsed(),
        lat,
    }
}

/// Time one bulk operation that processes `ops` items.
fn bulk(name: &str, ops: u64, f: impl FnOnce()) -> Result {
    let start = Instant::now();
    f();
    Result {
        name: name.into(),
        ops,
        total: start.elapsed(),
        lat: vec![],
    }
}

fn peak_rss_kb() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find(|l| l.starts_with("VmHWM:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn q(db: &mut Database, s: &str) {
    db.execute(s).unwrap_or_else(|e| panic!("{s}: {e}"));
}

const SQLITE: &str = r#"
import json, sqlite3, sys, time, random
n = int(sys.argv[1]); random.seed(7)
db = sqlite3.connect(":memory:")
out = {}
def t(name, ops, f):
    s = time.perf_counter(); f(); out[name] = [ops, time.perf_counter() - s]
db.execute("CREATE TABLE c (id INTEGER PRIMARY KEY, city TEXT, age INT, score REAL)")
def ins():
    with db:
        db.executemany("INSERT INTO c VALUES (?,?,?,?)", [(i, "city%d" % (i % 50), i % 90, i * 0.5) for i in range(n)])
t("insert (batched txn)", n, ins)
db.execute("CREATE INDEX c_age ON c (age)")
db.execute("CREATE TABLE o (id INTEGER PRIMARY KEY, cid INT, amt INT)")
with db:
    db.executemany("INSERT INTO o VALUES (?,?,?)", [(i, i % n, i % 100) for i in range(n * 2)])
ids = [random.randrange(n) for _ in range(min(n, 5000))]
t("point lookup (pk)", len(ids), lambda: [db.execute("SELECT city FROM c WHERE id = ?", (i,)).fetchall() for i in ids])
t("range scan (index, 10%)", 1, lambda: db.execute("SELECT id FROM c WHERE age BETWEEN 10 AND 18").fetchall())
t("sequential scan + filter", 1, lambda: db.execute("SELECT id FROM c WHERE score > 10 AND city <> 'x'").fetchall())
t("group by aggregate", 1, lambda: db.execute("SELECT city, COUNT(*), AVG(score) FROM c GROUP BY city").fetchall())
t("join (customers x orders)", 1, lambda: db.execute("SELECT c.city, SUM(o.amt) FROM c JOIN o ON o.cid = c.id GROUP BY c.city").fetchall())
json.dump(out, sys.stdout)
"#;

fn main() {
    let n: u64 = match std::env::var("POLYDB_DATASET").as_deref() {
        Ok("medium") => 100_000,
        Ok("large") => 1_000_000,
        Ok("stress") => 10_000_000,
        Ok(x) if x.parse::<u64>().is_ok() => x.parse().unwrap(),
        _ => 1_000,
    };
    let point_ops = n.min(5000);
    let dir = std::env::temp_dir().join(format!("polydb-perf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut db = Database::open(&dir).unwrap();
    let mut r = Rng(42);
    let mut res = Vec::new();
    println!("polydb perf: {n} records, data directory {}", dir.display());

    q(
        &mut db,
        "CREATE TABLE c (id INT PRIMARY KEY, city TEXT, age INT, score REAL)",
    );
    q(
        &mut db,
        "CREATE TABLE o (id INT PRIMARY KEY, cid INT, amt INT)",
    );
    let wal0 = std::fs::metadata(dir.join("data.wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    res.push(bulk("insert (batched txn)", n, || {
        db.begin().unwrap();
        let mut i = 0;
        while i < n {
            let vals: Vec<String> = (i..(i + 500).min(n))
                .map(|k| format!("({k}, 'city{}', {}, {})", k % 50, k % 90, k as f64 * 0.5))
                .collect();
            q(&mut db, &format!("INSERT INTO c VALUES {}", vals.join(",")));
            i += 500;
        }
        db.commit().unwrap();
    }));
    let wal_after_insert = std::fs::metadata(dir.join("data.wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    res.push(per_op("insert (autocommit, fsync)", n.min(300), |i| {
        q(
            &mut db,
            &format!("INSERT INTO o VALUES ({}, {}, {})", i, i % n, i % 100),
        )
    }));
    db.begin().unwrap();
    let mut i = n.min(300);
    while i < n * 2 {
        let vals: Vec<String> = (i..(i + 500).min(n * 2))
            .map(|k| format!("({k}, {}, {})", k % n, k % 100))
            .collect();
        q(&mut db, &format!("INSERT INTO o VALUES {}", vals.join(",")));
        i += 500;
    }
    db.commit().unwrap();
    let entries_before_index = db.entry_count();
    res.push(bulk("create index (age)", n, || {
        q(&mut db, "CREATE INDEX c_age ON c (age)")
    }));
    let index_entries = db.entry_count() - entries_before_index;
    let ids: Vec<u64> = (0..point_ops).map(|_| r.below(n)).collect();
    res.push(per_op("point lookup (pk)", point_ops, |k| {
        q(
            &mut db,
            &format!("SELECT city FROM c WHERE id = {}", ids[k as usize]),
        )
    }));
    res.push(per_op("range scan (index, 10%)", 20, |_| {
        q(&mut db, "SELECT id FROM c WHERE age BETWEEN 10 AND 18")
    }));
    res.push(per_op("sequential scan + filter", 20, |_| {
        q(&mut db, "SELECT id FROM c WHERE score > 10 AND city <> 'x'")
    }));
    res.push(per_op("ORDER BY index LIMIT 10", 200, |_| {
        q(
            &mut db,
            "SELECT id FROM c WHERE age > 50 ORDER BY age LIMIT 10",
        )
    }));
    res.push(per_op("group by aggregate", 10, |_| {
        q(
            &mut db,
            "SELECT city, COUNT(*), AVG(score) FROM c GROUP BY city",
        )
    }));
    res.push(per_op("join (customers x orders)", 5, |_| {
        q(
            &mut db,
            "SELECT c.city, SUM(o.amt) FROM c JOIN o ON o.cid = c.id GROUP BY c.city",
        )
    }));
    res.push(per_op("update by pk", point_ops.min(1000), |k| {
        q(
            &mut db,
            &format!(
                "UPDATE c SET score = score + 1 WHERE id = {}",
                ids[k as usize]
            ),
        )
    }));
    res.push(per_op("transaction (3 stmts)", point_ops.min(500), |k| {
        db.begin().unwrap();
        q(
            &mut db,
            &format!("UPDATE c SET age = age WHERE id = {}", ids[k as usize]),
        );
        q(&mut db, &format!("SET txn:{k} 1"));
        q(&mut db, &format!("db.audit.insert({{_id: {k}}})"));
        db.commit().unwrap();
    }));
    res.push(per_op("delete by pk", point_ops.min(500), |k| {
        q(&mut db, &format!("DELETE FROM o WHERE id = {k}"))
    }));
    // documents
    res.push(bulk("document insert (batched)", n, || {
        db.begin().unwrap();
        let mut i = 0;
        while i < n {
            let docs: Vec<String> = (i..(i + 500).min(n))
                .map(|k| {
                    format!(
                        "{{_id: {k}, cid: {}, status: '{}', total: {}, ship: {{city: 'c{}'}}}}",
                        k % n,
                        ["new", "paid", "shipped"][(k % 3) as usize],
                        k % 997,
                        k % 40
                    )
                })
                .collect();
            q(&mut db, &format!("db.orders.insert([{}])", docs.join(",")));
            i += 500;
        }
        db.commit().unwrap();
    }));
    q(&mut db, "db.orders.createIndex({cid: 1})");
    res.push(per_op("document find (index)", point_ops, |k| {
        q(
            &mut db,
            &format!("db.orders.find({{cid: {}}})", ids[k as usize]),
        )
    }));
    res.push(per_op("document aggregate ($group)", 10, |_| q(&mut db, "db.orders.aggregate([{$group: {_id: '$status', n: {$sum: 1}, avg: {$avg: '$total'}}}])")));
    res.push(per_op("cross-model join (rel x doc x kv)", 5, |_| {
        q(&mut db, "SELECT c.city, COUNT(*) FROM c JOIN DOCUMENT.orders o ON o.cid = c.id LEFT JOIN KV s ON s.key = CONCAT('txn:', c.id) WHERE c.age < 30 GROUP BY c.city")
    }));
    // key-value
    res.push(per_op("kv set (autocommit, fsync)", n.min(300), |k| {
        db.kv_set(&format!("k{k}"), b"value").unwrap()
    }));
    db.set_sync(false);
    res.push(per_op("kv set (no fsync)", point_ops, |k| {
        db.kv_set(&format!("k{k}"), b"value").unwrap()
    }));
    db.set_sync(true);
    res.push(per_op("kv get", point_ops, |k| {
        let _ = db.kv_get(&format!("k{}", k % point_ops)).unwrap();
    }));

    let rss = peak_rss_kb();
    let size = dir_size(&dir);
    let wal = std::fs::metadata(dir.join("data.wal"))
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "\n{:<36} {:>9} {:>11} {:>12} {:>9} {:>9} {:>9}",
        "workload", "ops", "time (ms)", "ops/s", "p50 us", "p95 us", "p99 us"
    );
    for x in &res {
        println!(
            "{:<36} {:>9} {:>11.2} {:>12.0} {:>9} {:>9} {:>9}",
            x.name,
            x.ops,
            x.total.as_secs_f64() * 1e3,
            x.ops_per_s(),
            x.pct(0.5),
            x.pct(0.95),
            x.pct(0.99)
        );
    }
    println!(
        "\npeak RSS: {} KB",
        rss.map_or("n/a".into(), |k| k.to_string())
    );
    println!("database directory: {size} bytes (WAL {wal} bytes; WAL grew {} bytes during the bulk insert)", wal_after_insert.saturating_sub(wal0));
    println!(
        "index c_age: {index_entries} entries; total entries {}",
        db.entry_count()
    );

    // SQLite reference for the relational workloads
    let mut sqlite_json = Value::Null;
    match std::process::Command::new("python3")
        .args(["-c", SQLITE, &n.to_string()])
        .output()
    {
        Ok(o) if o.status.success() => {
            if let Ok(v @ Value::Object(_)) = Value::parse(&String::from_utf8_lossy(&o.stdout)) {
                println!(
                    "\nSQLite (python3 sqlite3, in-memory, timed in Python incl. driver overhead):"
                );
                println!(
                    "{:<36} {:>9} {:>11} {:>12} {:>14}",
                    "workload", "ops", "time (ms)", "ops/s", "polydb ops/s"
                );
                if let Value::Object(m) = &v {
                    for (name, x) in m {
                        if let Value::Array(a) = x {
                            let (ops, secs) =
                                (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0));
                            let ours = res
                                .iter()
                                .find(|r| &r.name == name)
                                .map(|r| format!("{:.0}", r.ops_per_s()))
                                .unwrap_or("-".into());
                            println!(
                                "{:<36} {:>9} {:>11.2} {:>12.0} {:>14}",
                                name,
                                ops,
                                secs * 1e3,
                                ops / secs.max(1e-9),
                                ours
                            );
                        }
                    }
                }
                sqlite_json = v;
            }
        }
        _ => println!("\nSQLite comparison skipped: python3 with sqlite3 not available"),
    }

    let json = Value::Object(
        [
            ("records".to_string(), Value::Int(n as i64)),
            (
                "peak_rss_kb".to_string(),
                rss.map_or(Value::Null, |k| Value::Int(k as i64)),
            ),
            ("db_bytes".to_string(), Value::Int(size as i64)),
            ("wal_bytes".to_string(), Value::Int(wal as i64)),
            (
                "index_entries".to_string(),
                Value::Int(index_entries as i64),
            ),
            (
                "workloads".to_string(),
                Value::Array(
                    res.iter()
                        .map(|x| {
                            Value::Object(
                                [
                                    ("name".to_string(), Value::Text(x.name.clone())),
                                    ("ops".to_string(), Value::Int(x.ops as i64)),
                                    ("ms".to_string(), Value::Float(x.total.as_secs_f64() * 1e3)),
                                    ("ops_per_s".to_string(), Value::Float(x.ops_per_s())),
                                    ("p50_us".to_string(), Value::Text(x.pct(0.5))),
                                    ("p95_us".to_string(), Value::Text(x.pct(0.95))),
                                    ("p99_us".to_string(), Value::Text(x.pct(0.99))),
                                ]
                                .into_iter()
                                .collect(),
                            )
                        })
                        .collect(),
                ),
            ),
            ("sqlite".to_string(), sqlite_json),
        ]
        .into_iter()
        .collect(),
    );
    let _ = std::fs::create_dir_all("target");
    let _ = std::fs::write("target/perf-results.json", json.to_json());
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
