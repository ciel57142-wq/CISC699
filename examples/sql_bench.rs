//! Relational micro-benchmark through the public API.
//!
//!     cargo run --release --example sql_bench [rows]
//!
//! Uses only SQL that every polydb version accepts, so the same file can be
//! run against older versions for comparison. Timings are wall-clock for
//! the whole operation on an in-memory database (no fsync).

use polydb::{Database, Output};
use std::time::Instant;

fn time<T>(label: &str, n: usize, f: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let out = f();
    let el = start.elapsed();
    let per = if n > 0 { el.as_secs_f64() * 1e6 / n as f64 } else { 0.0 };
    println!("{label:<44} {:>9.1} ms  {:>9.2} us/op  ({n} ops)", el.as_secs_f64() * 1e3, per);
    out
}

fn count(db: &mut Database, sql: &str) -> usize {
    match db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}")) {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    }
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(20_000);
    let mut db = Database::in_memory();
    db.execute("CREATE TABLE users (id INT PRIMARY KEY, city TEXT, age INT)").unwrap();
    db.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, total REAL)").unwrap();
    db.execute("CREATE INDEX users_city ON users (city)").unwrap();
    db.execute("CREATE INDEX orders_user ON orders (user_id)").unwrap();

    time("insert users (batches of 500)", n, || {
        for chunk in (0..n).collect::<Vec<_>>().chunks(500) {
            let vals: Vec<String> = chunk.iter().map(|i| format!("({i}, 'c{}', {})", i % 100, i % 90)).collect();
            db.execute(&format!("INSERT INTO users VALUES {}", vals.join(","))).unwrap();
        }
    });
    time("insert orders (batches of 500)", n, || {
        for chunk in (0..n).collect::<Vec<_>>().chunks(500) {
            let vals: Vec<String> = chunk.iter().map(|i| format!("({i}, {}, {}.5)", (i * 7) % n, i % 1000)).collect();
            db.execute(&format!("INSERT INTO orders VALUES {}", vals.join(","))).unwrap();
        }
    });
    let lookups = 2_000;
    time("primary-key point lookup", lookups, || {
        for i in 0..lookups {
            count(&mut db, &format!("SELECT * FROM users WHERE id = {}", (i * 13) % n));
        }
    });
    time("secondary-index equality lookup", 200, || {
        for i in 0..200 {
            count(&mut db, &format!("SELECT id FROM users WHERE city = 'c{}'", i % 100));
        }
    });
    time("primary-key range (100 rows)", 200, || {
        for i in 0..200 {
            let lo = (i * 37) % (n - 100);
            count(&mut db, &format!("SELECT id FROM users WHERE id >= {lo} AND id < {}", lo + 100));
        }
    });
    time("full scan with filter", 20, || {
        for i in 0..20 {
            count(&mut db, &format!("SELECT id FROM users WHERE age = {}", i));
        }
    });
    time("equi-join users x orders (whole tables)", 1, || {
        count(&mut db, "SELECT u.id, o.total FROM users u JOIN orders o ON o.user_id = u.id")
    });
    time("GROUP BY city with aggregates", 5, || {
        for _ in 0..5 {
            count(&mut db, "SELECT city, COUNT(*), AVG(age), MAX(age) FROM users GROUP BY city");
        }
    });
    time("UPDATE through the index", 100, || {
        for i in 0..100 {
            db.execute(&format!("UPDATE users SET age = age + 1 WHERE city = 'c{i}'")).unwrap();
        }
    });
    time("DELETE by primary key", 1_000, || {
        for i in 0..1_000 {
            db.execute(&format!("DELETE FROM orders WHERE id = {i}")).unwrap();
        }
    });
}
