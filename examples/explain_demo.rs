//! Prints EXPLAIN and EXPLAIN ANALYZE output for a small shop schema.
//!
//!     cargo run --example explain_demo
use polydb::{Database, Output};

fn show(db: &mut Database, sql: &str) {
    println!("\n> {sql}");
    match db.execute(sql) {
        Ok(Output::Rows { rows, .. }) => rows.iter().for_each(|r| println!("{}", r[0].display())),
        other => println!("{other:?}"),
    }
}

fn main() {
    let mut db = Database::in_memory();
    for s in [
        "CREATE TABLE customers (id INT PRIMARY KEY, name TEXT NOT NULL, city TEXT, vip BOOL)",
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT NOT NULL, total REAL, status TEXT)",
        "CREATE INDEX orders_customer ON orders (customer_id)",
        "CREATE INDEX customers_city_name ON customers (city, name)",
    ] {
        db.execute(s).unwrap();
    }
    let cities = ["London", "Paris", "Oslo", "Rome", "Lima"];
    let vals: Vec<String> = (1..=2000)
        .map(|i| format!("({i}, 'c{i}', '{}', {})", cities[i % 5], i % 50 == 0))
        .collect();
    db.execute(&format!("INSERT INTO customers VALUES {}", vals.join(","))).unwrap();
    let vals: Vec<String> = (1..=20000)
        .map(|i| {
            format!(
                "({i}, {}, {}.5, '{}')",
                (i * 7) % 2000 + 1,
                i % 500,
                if i % 10 == 0 { "open" } else { "done" }
            )
        })
        .collect();
    db.execute(&format!("INSERT INTO orders VALUES {}", vals.join(","))).unwrap();
    show(&mut db, "EXPLAIN SELECT c.name, o.total FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.city = 'Oslo' AND c.name = 'c3' ORDER BY o.total DESC LIMIT 5");
    show(&mut db, "ANALYZE");
    show(&mut db, "EXPLAIN ANALYZE SELECT c.name, o.total FROM orders o JOIN customers c ON c.id = o.customer_id WHERE c.city = 'Oslo' AND c.name = 'c3' ORDER BY o.total DESC LIMIT 5");
    show(
        &mut db,
        "EXPLAIN ANALYZE SELECT city, COUNT(*) FROM customers c JOIN orders o ON o.customer_id = c.id WHERE o.status = 'open' GROUP BY city",
    );
    show(&mut db, "EXPLAIN ANALYZE SELECT * FROM orders WHERE total > 400");
    show(&mut db, "EXPLAIN ANALYZE SELECT * FROM orders WHERE total > 490 AND id < 100");
    show(
        &mut db,
        "EXPLAIN ANALYZE SELECT name FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.total > 498)",
    );
    show(
        &mut db,
        "EXPLAIN ANALYZE SELECT * FROM customers c LEFT JOIN orders o ON o.customer_id = c.id AND o.status = 'open' WHERE c.id IN (1, 2, 3)",
    );
}
