//! Shared test support: a deterministic PRNG, realistic data generators and
//! dataset sizing.
//!
//! Dataset size comes from `POLYDB_DATASET` (small = 1,000, medium =
//! 100,000, large = 1,000,000, stress = 10,000,000 records; default small),
//! and every generator takes a seed (`POLYDB_SEED`, default 42) so a failure
//! can be reproduced exactly.
#![allow(dead_code)]

use polydb::{Database, Output, Value};

/// SplitMix64: tiny, fast, deterministic.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, n).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    pub fn word(&mut self, len: usize) -> String {
        (0..len)
            .map(|_| (b'a' + self.below(26) as u8) as char)
            .collect()
    }
}

pub fn seed() -> u64 {
    std::env::var("POLYDB_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(42)
}

/// Number of records for the configured dataset size.
pub fn dataset_size() -> usize {
    match std::env::var("POLYDB_DATASET").as_deref() {
        Ok("medium") => 100_000,
        Ok("large") => 1_000_000,
        Ok("stress") => 10_000_000,
        Ok(n) if n.parse::<usize>().is_ok() => n.parse().unwrap(),
        _ => 1_000,
    }
}

pub fn tmp(name: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("polydb-t-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    p
}

pub fn rows(db: &mut Database, q: &str) -> Vec<Vec<Value>> {
    match db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        Output::Rows { rows, .. } => rows,
        Output::Docs(d) => d.into_iter().map(|x| vec![x]).collect(),
        Output::Scalar(v) => vec![vec![v]],
        other => panic!("{q}: expected rows, got {other:?}"),
    }
}

pub fn scalar(db: &mut Database, q: &str) -> Value {
    let r = rows(db, q);
    assert_eq!(r.len(), 1, "{q}: expected one row, got {r:?}");
    r[0][0].clone()
}

pub fn ok(db: &mut Database, q: &str) -> Output {
    db.execute(q).unwrap_or_else(|e| panic!("{q}: {e}"))
}

pub fn plan(db: &mut Database, q: &str) -> String {
    rows(db, &format!("EXPLAIN {q}"))
        .into_iter()
        .map(|r| r[0].display())
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

const FIRST: &[&str] = &[
    "Ada", "Linus", "Grace", "Alan", "Edsger", "Barbara", "Ken", "Dennis", "Margaret", "Donald",
];
const CITIES: &[&str] = &[
    "Berlin", "Paris", "Rome", "Oslo", "Lisbon", "Madrid", "Vienna", "Prague",
];
const COUNTRIES: &[&str] = &["DE", "FR", "IT", "NO", "PT", "ES", "AT", "CZ"];
const DEPTS: &[&str] = &["eng", "ops", "sales", "hr", "legal", "research"];
const CATEGORIES: &[&str] = &["books", "tools", "food", "toys", "garden"];

/// Relational schema for the generated business data.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE departments (id INT PRIMARY KEY, name TEXT NOT NULL UNIQUE, budget REAL)",
    "CREATE TABLE employees (id INT PRIMARY KEY, name TEXT NOT NULL, dept_id INT REFERENCES departments(id), salary INT CHECK (salary > 0), hired DATE)",
    "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT NOT NULL, city TEXT, country TEXT, created TIMESTAMP)",
    "CREATE TABLE products (id INT PRIMARY KEY, name TEXT NOT NULL, category TEXT, price REAL CHECK (price >= 0))",
    "CREATE TABLE transactions (id INT PRIMARY KEY, customer_id INT REFERENCES customers(id), product_id INT REFERENCES products(id), qty INT, at TIMESTAMP)",
];

/// Load `n` customers (and proportional other entities) in batches.
/// Relational: departments, employees, customers, products, transactions.
/// Documents: `orders` (nested, with arrays) referencing customers.
/// Key-value: `sessions` namespace and `cache:` keys in the default space.
pub fn load_business(db: &mut Database, n: usize, seed: u64) {
    let mut r = Rng::new(seed);
    for s in SCHEMA {
        ok(db, s);
    }
    let batch = |db: &mut Database, table: &str, rows: &mut Vec<String>| {
        if !rows.is_empty() {
            ok(
                db,
                &format!("INSERT INTO {table} VALUES {}", rows.join(", ")),
            );
            rows.clear();
        }
    };
    let mut buf = Vec::new();
    for (i, d) in DEPTS.iter().enumerate() {
        buf.push(format!("({i}, '{d}', {}.0)", r.range(10, 500) * 1000));
    }
    batch(db, "departments", &mut buf);
    let n_emp = (n / 10).max(10);
    for i in 0..n_emp {
        buf.push(format!(
            "({i}, '{} {}', {}, {}, '20{:02}-{:02}-{:02}')",
            r.pick(FIRST),
            r.word(5),
            r.below(DEPTS.len() as u64),
            r.range(30, 200) * 1000,
            r.range(0, 23),
            r.range(1, 12),
            r.range(1, 28)
        ));
        if buf.len() == 500 {
            batch(db, "employees", &mut buf);
        }
    }
    batch(db, "employees", &mut buf);
    for i in 0..n {
        let c = r.below(CITIES.len() as u64) as usize;
        let city = if r.chance(5) {
            "NULL".to_string()
        } else {
            format!("'{}'", CITIES[c])
        };
        buf.push(format!(
            "({i}, '{} {}', {city}, '{}', '2024-{:02}-{:02} 10:00:00')",
            r.pick(FIRST),
            r.word(6),
            COUNTRIES[c],
            r.range(1, 12),
            r.range(1, 28)
        ));
        if buf.len() == 500 {
            batch(db, "customers", &mut buf);
        }
    }
    batch(db, "customers", &mut buf);
    let n_prod = (n / 20).max(20);
    for i in 0..n_prod {
        buf.push(format!(
            "({i}, 'product-{i}', '{}', {}.{:02})",
            r.pick(CATEGORIES),
            r.range(1, 300),
            r.range(0, 99)
        ));
        if buf.len() == 500 {
            batch(db, "products", &mut buf);
        }
    }
    batch(db, "products", &mut buf);
    for i in 0..n * 2 {
        buf.push(format!(
            "({i}, {}, {}, {}, '2024-{:02}-{:02} {:02}:00:00')",
            r.below(n as u64),
            r.below(n_prod as u64),
            r.range(1, 10),
            r.range(1, 12),
            r.range(1, 28),
            r.range(0, 23)
        ));
        if buf.len() == 500 {
            batch(db, "transactions", &mut buf);
        }
    }
    batch(db, "transactions", &mut buf);
    // documents: orders with nested shipping and an item array
    let mut docs = Vec::new();
    for i in 0..n {
        let items: Vec<String> = (0..r.range(0, 3))
            .map(|_| {
                format!(
                    "{{sku: 'product-{}', qty: {}}}",
                    r.below(n_prod as u64),
                    r.range(1, 5)
                )
            })
            .collect();
        let status = r.pick(&["new", "paid", "shipped", "returned"]);
        let total = if r.chance(5) {
            "null".to_string()
        } else {
            format!("{}.{}", r.range(5, 900), r.below(10))
        };
        let note = if r.chance(30) {
            format!(", note: '{}'", r.word(8))
        } else {
            String::new()
        };
        docs.push(format!(
            "{{_id: {i}, customer_id: {}, status: '{status}', total: {total}, shipping: {{city: '{}', express: {}}}, items: [{}]{note}}}",
            r.below(n as u64),
            r.pick(CITIES),
            r.chance(20),
            items.join(", ")
        ));
        if docs.len() == 500 {
            ok(db, &format!("db.orders.insert([{}])", docs.join(", ")));
            docs.clear();
        }
    }
    if !docs.is_empty() {
        ok(db, &format!("db.orders.insert([{}])", docs.join(", ")));
    }
    // key-value: sessions namespace and cache keys
    db.begin().unwrap();
    for i in 0..n {
        if r.chance(40) {
            ok(
                db,
                &format!("SET KV.sessions 'session:{i}' = 'tok-{}'", r.word(10)),
            );
        }
        if r.chance(10) {
            ok(db, &format!("SET cache:{i} {}", r.range(0, 1000)));
        }
    }
    db.commit().unwrap();
}

/// A full, ordered dump of every table, collection and key (for state comparisons).
pub fn dump(db: &mut Database) -> Vec<String> {
    let mut out = Vec::new();
    for t in rows(db, "SHOW TABLES") {
        let name = t[0].display();
        for r in rows(db, &format!("SELECT * FROM {name}")) {
            out.push(format!("t:{name}:{}", Value::Array(r).to_json()));
        }
    }
    for c in rows(db, "SHOW COLLECTIONS") {
        let name = c[0].display();
        for d in rows(db, &format!("db.{name}.find({{}})")) {
            out.push(format!("c:{name}:{}", d[0].to_json()));
        }
    }
    for k in db.kv_keys("").unwrap() {
        out.push(format!("k:{k}:{:?}", db.kv_get(&k).unwrap()));
    }
    out.sort();
    out
}
