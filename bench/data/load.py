"""Load one generated dataset into both placements (HS2 §5.2).

  federated:    customer, vendor, product -> pg-fed      orders, feedback -> mongo-fed
  consolidated: everything -> pg-cons (orders split into orders + order_line)

Indexes are matched across placements: primary keys everywhere, and the same foreign-key
and `sel` indexes on both sides, so neither arm wins on an index the other lacks.
Each load drops and recreates everything; the result is a function of the files only.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path

import psycopg
from pymongo import ASCENDING, MongoClient

from bench.data.validate import load_dir
from engine.config import settings
from engine.naming import parse_iso_ms

DDL_FED = """
DROP TABLE IF EXISTS customer, vendor, product CASCADE;
CREATE TABLE customer (id integer PRIMARY KEY, email text NOT NULL, country text NOT NULL,
                       sel integer NOT NULL, created_at timestamptz(3) NOT NULL);
CREATE TABLE vendor   (id integer PRIMARY KEY, name text NOT NULL, country text NOT NULL);
CREATE TABLE product  (id integer PRIMARY KEY, vendor_id integer NOT NULL, title text NOT NULL,
                       price_cents integer NOT NULL);
"""
IDX_FED = """
CREATE INDEX customer_sel ON customer (sel);
CREATE INDEX product_vendor ON product (vendor_id);
ANALYZE;
"""
DDL_CONS = DDL_FED.replace("customer, vendor, product", "customer, vendor, product, orders, order_line, feedback") + """
CREATE TABLE orders     (id integer PRIMARY KEY, customer_id integer NOT NULL,
                         ordered_at timestamptz(3) NOT NULL, total_cents integer NOT NULL);
CREATE TABLE order_line (order_id integer NOT NULL, line_no integer NOT NULL,
                         product_id integer NOT NULL, quantity integer NOT NULL,
                         price_cents integer NOT NULL, PRIMARY KEY (order_id, line_no));
CREATE TABLE feedback   (id integer PRIMARY KEY, customer_id integer NOT NULL,
                         product_id integer NOT NULL, rating integer NOT NULL, text text NOT NULL,
                         posted_at timestamptz(3) NOT NULL);
"""
IDX_CONS = IDX_FED.replace("ANALYZE;", "") + """
CREATE INDEX orders_customer ON orders (customer_id);
CREATE INDEX feedback_product ON feedback (product_id);
CREATE INDEX feedback_customer ON feedback (customer_id);
ANALYZE;
"""


def _copy(cur, table: str, cols: list[str], rows) -> None:
    with cur.copy(f"COPY {table} ({', '.join(cols)}) FROM STDIN") as cp:
        for r in rows:
            cp.write_row(r)


def load_pg_common(conn, data) -> None:
    with conn.cursor() as cur:
        _copy(cur, "customer", ["id", "email", "country", "sel", "created_at"],
              ((c["id"], c["email"], c["country"], c["sel"], parse_iso_ms(c["createdAt"]))
               for c in data["customer"]))
        _copy(cur, "vendor", ["id", "name", "country"],
              ((v["id"], v["name"], v["country"]) for v in data["vendor"]))
        _copy(cur, "product", ["id", "vendor_id", "title", "price_cents"],
              ((p["id"], p["vendorId"], p["title"], p["priceCents"]) for p in data["product"]))


def load_fed(data, s) -> dict:
    t = time.perf_counter()
    with psycopg.connect(s.pg_fed_dsn, autocommit=False) as conn:
        conn.execute(DDL_FED)
        load_pg_common(conn, data)
        conn.commit()
        conn.autocommit = True
        conn.execute(IDX_FED)
    db = MongoClient(s.mongo_uri, serverSelectionTimeoutMS=5000)[s.mongo_db]
    for c in ("orders", "feedback"):
        db.drop_collection(c)
    db.orders.insert_many(
        [{"_id": o["id"], "customerId": o["customerId"], "orderedAt": parse_iso_ms(o["orderedAt"]),
          "totalCents": o["totalCents"], "lines": o["lines"]} for o in data["order"]],
        ordered=False)
    db.feedback.insert_many(
        [{"_id": f["id"], "customerId": f["customerId"], "productId": f["productId"],
          "rating": f["rating"], "text": f["text"], "postedAt": parse_iso_ms(f["postedAt"])}
         for f in data["feedback"]], ordered=False)
    db.orders.create_index([("customerId", ASCENDING)], name="orders_customer")
    db.feedback.create_index([("productId", ASCENDING)], name="feedback_product")
    db.feedback.create_index([("customerId", ASCENDING)], name="feedback_customer")
    return {"seconds": round(time.perf_counter() - t, 2)}


def load_cons(data, s) -> dict:
    t = time.perf_counter()
    with psycopg.connect(s.pg_cons_dsn, autocommit=False) as conn:
        conn.execute(DDL_CONS)
        load_pg_common(conn, data)
        with conn.cursor() as cur:
            _copy(cur, "orders", ["id", "customer_id", "ordered_at", "total_cents"],
                  ((o["id"], o["customerId"], parse_iso_ms(o["orderedAt"]), o["totalCents"])
                   for o in data["order"]))
            _copy(cur, "order_line", ["order_id", "line_no", "product_id", "quantity", "price_cents"],
                  ((o["id"], x["lineNo"], x["productId"], x["quantity"], x["priceCents"])
                   for o in data["order"] for x in o["lines"]))
            _copy(cur, "feedback", ["id", "customer_id", "product_id", "rating", "text", "posted_at"],
                  ((f["id"], f["customerId"], f["productId"], f["rating"], f["text"],
                    parse_iso_ms(f["postedAt"])) for f in data["feedback"]))
        conn.commit()
        conn.autocommit = True
        conn.execute(IDX_CONS)
    return {"seconds": round(time.perf_counter() - t, 2)}


if __name__ == "__main__":
    d = Path(sys.argv[1])
    data = load_dir(d)
    s = settings()
    rep = {"federated": load_fed(data, s), "consolidated": load_cons(data, s),
           "dataset": json.loads((d / "meta.json").read_text())}
    (d / "load.json").write_text(json.dumps(rep, indent=2))
    print(json.dumps({k: rep[k] for k in ("federated", "consolidated")}))
    print("load done:", {k: len(v) for k, v in data.items()})
