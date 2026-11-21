//! Cross-model integration on a realistic dataset: relational customers and
//! transactions, document orders, key-value sessions.

mod common;
use common::*;
use polydb::{Database, Value};

fn business(n: usize) -> Database {
    let mut db = Database::in_memory();
    load_business(&mut db, n, seed());
    ok(&mut db, "db.orders.createIndex({customer_id: 1})");
    db
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not an int: {v:?}"))
}

#[test]
fn customers_join_document_orders() {
    let mut db = business(dataset_size().min(2000));
    // relational -> document via the document index on customer_id
    let q = "SELECT c.id, c.name, o.status, o.total FROM customers c JOIN DOCUMENT.orders o ON o.customer_id = c.id WHERE c.country = 'DE' ORDER BY c.id";
    let p = plan(&mut db, q);
    assert!(
        p.contains("DocumentIndexScan on DOCUMENT.orders") && p.contains("cross-model"),
        "{p}"
    );
    let joined = rows(&mut db, q);
    // the same answer computed independently through the native APIs
    let de: Vec<i64> = rows(&mut db, "SELECT id FROM customers WHERE country = 'DE'")
        .iter()
        .map(|r| int(&r[0]))
        .collect();
    let mut expected = 0;
    for id in &de {
        expected += rows(&mut db, &format!("db.orders.find({{customer_id: {id}}})")).len();
    }
    assert_eq!(joined.len(), expected);
    assert!(joined.windows(2).all(|w| int(&w[0][0]) <= int(&w[1][0])));
}

#[test]
fn documents_join_kv_and_all_three_models() {
    let mut db = business(dataset_size().min(2000));
    // document -> KV lookup
    let q = "SELECT o._id, s.value FROM DOCUMENT.orders o JOIN KV.sessions s ON s.key = CONCAT('session:', o.customer_id) WHERE o.status = 'paid'";
    assert!(plan(&mut db, q).contains("KVGet on KV.sessions"));
    let rows_dk = rows(&mut db, q);
    for r in &rows_dk {
        assert!(r[1].display().starts_with("tok-"));
    }
    // all three models in one query, with aggregation
    let q = "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue, COUNT(s.key) AS with_session \
             FROM RELATIONAL.customers c \
             JOIN DOCUMENT.orders o ON o.customer_id = c.id \
             LEFT JOIN KV.sessions s ON s.key = CONCAT('session:', c.id) \
             GROUP BY c.country ORDER BY c.country";
    let by_country = rows(&mut db, q);
    let total_orders: i64 = by_country.iter().map(|r| int(&r[1])).sum();
    let all_orders = int(&scalar(
        &mut db,
        "SELECT COUNT(*) FROM DOCUMENT.orders o JOIN customers c ON c.id = o.customer_id",
    ));
    assert_eq!(total_orders, all_orders);
    for r in &by_country {
        assert!(int(&r[3]) <= int(&r[1]));
    }
    // document arrays and nested fields in a cross-model filter
    let n = rows(&mut db, "SELECT o._id FROM DOCUMENT.orders o JOIN customers c ON c.id = o.customer_id WHERE o.shipping.express = true AND o.items.0.qty >= 3 AND c.city IS NOT NULL").len();
    let native = rows(
        &mut db,
        "db.orders.find({'shipping.express': true, 'items.0.qty': {$gte: 3}})",
    )
    .len();
    assert!(n <= native);
}

#[test]
fn one_transaction_updates_all_models_and_rolls_back_together() {
    let mut db = business(300);
    let before = dump(&mut db);
    ok(&mut db, "BEGIN");
    ok(&mut db, "UPDATE customers SET city = 'Moved' WHERE id < 50");
    ok(
        &mut db,
        "UPDATE DOCUMENT.orders SET status = 'cancelled' WHERE customer_id < 50",
    );
    ok(
        &mut db,
        "DELETE FROM KV.sessions WHERE key LIKE 'session:1%'",
    );
    ok(
        &mut db,
        "INSERT INTO DOCUMENT.audit VALUES {\"what\": \"bulk move\"}",
    );
    assert_eq!(
        scalar(&mut db, "SELECT COUNT(*) FROM DOCUMENT.audit"),
        Value::Int(1)
    );
    ok(&mut db, "ROLLBACK");
    assert_eq!(
        dump(&mut db),
        before,
        "nothing of the transaction is visible after rollback"
    );
    ok(&mut db, "BEGIN");
    ok(&mut db, "UPDATE customers SET city = 'Moved' WHERE id < 50");
    ok(
        &mut db,
        "UPDATE DOCUMENT.orders SET status = 'cancelled' WHERE customer_id < 50",
    );
    ok(&mut db, "SET KV 'last_move' = 'done'");
    ok(&mut db, "COMMIT");
    assert_eq!(
        scalar(
            &mut db,
            "SELECT COUNT(*) FROM customers WHERE city = 'Moved'"
        ),
        Value::Int(50)
    );
    let cancelled = int(&scalar(
        &mut db,
        "SELECT COUNT(*) FROM DOCUMENT.orders WHERE status = 'cancelled'",
    ));
    let expected = int(&scalar(
        &mut db,
        "SELECT COUNT(*) FROM DOCUMENT.orders WHERE customer_id < 50",
    ));
    assert_eq!(cancelled, expected);
}

#[test]
fn cross_model_data_and_indexes_survive_restart() {
    let dir = tmp("xmodel-restart");
    let (before, q);
    {
        let mut db = Database::open(&dir).unwrap();
        load_business(&mut db, 400, seed());
        ok(
            &mut db,
            "db.orders.createIndex({customer_id: 1, status: 1})",
        );
        q = "SELECT c.id, o._id, s.value FROM customers c JOIN DOCUMENT.orders o ON o.customer_id = c.id LEFT JOIN KV.sessions s ON s.key = CONCAT('session:', c.id) ORDER BY c.id, o._id";
        before = rows(&mut db, q);
        db.checkpoint().unwrap();
        ok(&mut db, "SET KV.sessions 'session:0' = 'after-checkpoint'");
    }
    let mut db = Database::open(&dir).unwrap();
    let after = rows(&mut db, q);
    assert_eq!(after.len(), before.len());
    assert!(
        plan(&mut db, q).contains("customer_id_1_status_1"),
        "the compound document index is used after restart"
    );
    assert_eq!(
        rows(&mut db, "GET FROM KV.sessions 'session:0'"),
        vec![vec![Value::Text("after-checkpoint".into())]]
    );
    drop(db);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cross_model_subqueries_and_unions() {
    let mut db = business(500);
    let active = rows(&mut db, "SELECT id FROM customers WHERE id IN (SELECT customer_id FROM DOCUMENT.orders WHERE status = 'shipped') ORDER BY id");
    let native: std::collections::BTreeSet<i64> =
        rows(&mut db, "db.orders.find({status: 'shipped'})")
            .iter()
            .map(|d| int(d[0].get("customer_id").unwrap()))
            .collect();
    assert_eq!(
        active.iter().map(|r| int(&r[0])).collect::<Vec<_>>(),
        native.into_iter().collect::<Vec<_>>()
    );
    let u = rows(&mut db, "SELECT CAST(id AS TEXT) FROM customers WHERE id < 3 UNION ALL SELECT key FROM KV.sessions WHERE key < 'session:2'");
    assert!(u.len() >= 3);
    let n = int(&scalar(&mut db, "SELECT COUNT(*) FROM customers c WHERE EXISTS (SELECT 1 FROM KV.sessions s WHERE s.key = CONCAT('session:', c.id))"));
    let sessions = int(&scalar(&mut db, "SELECT COUNT(*) FROM KV.sessions"));
    assert_eq!(n, sessions, "every session belongs to an existing customer");
}
