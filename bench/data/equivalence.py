"""T-D2 / gate M3: both placements hold the same data (HS2 §5.5).

For each entity: row counts equal, and an order-insensitive checksum equal.
checksum = SHA-256 over the sorted list of SHA-256 hashes of each record's canonical JSON
(keys sorted, §5.4 rules applied). Three legs are compared: the generated files, the
federated placement (pg-fed + mongo-fed) and the consolidated placement (pg-cons).
"""

from __future__ import annotations

import hashlib
import json
import sys
from pathlib import Path

import psycopg
from pymongo import MongoClient

from bench.data.gen import ENTITIES
from bench.data.validate import load_dir
from engine.config import settings
from engine.naming import iso_ms


def canon(rec: dict) -> str:
    return json.dumps(rec, sort_keys=True, separators=(",", ":"))


def checksum(records) -> tuple[int, str]:
    hs = sorted(hashlib.sha256(canon(r).encode()).hexdigest() for r in records)
    return len(hs), hashlib.sha256("".join(hs).encode()).hexdigest()


def _pg_rows(conn, sql):
    with conn.cursor() as cur:
        cur.execute(sql)
        cols = [c.name for c in cur.description]
        return [dict(zip(cols, r)) for r in cur.fetchall()]


def pg_common(conn) -> dict[str, list[dict]]:
    return {
        "customer": [{"id": r["id"], "email": r["email"], "country": r["country"], "sel": r["sel"],
                      "createdAt": iso_ms(r["created_at"])}
                     for r in _pg_rows(conn, "SELECT * FROM customer")],
        "vendor": _pg_rows(conn, "SELECT id, name, country FROM vendor"),
        "product": [{"id": r["id"], "vendorId": r["vendor_id"], "title": r["title"],
                     "priceCents": r["price_cents"]} for r in _pg_rows(conn, "SELECT * FROM product")],
    }


def federated(s) -> dict[str, list[dict]]:
    with psycopg.connect(s.pg_fed_dsn) as conn:
        out = pg_common(conn)
    db = MongoClient(s.mongo_uri, serverSelectionTimeoutMS=5000)[s.mongo_db]
    out["order"] = [{"id": o["_id"], "customerId": o["customerId"], "orderedAt": iso_ms(o["orderedAt"]),
                     "totalCents": o["totalCents"],
                     "lines": sorted(o["lines"], key=lambda x: x["lineNo"])} for o in db.orders.find()]
    out["feedback"] = [{"id": f["_id"], "customerId": f["customerId"], "productId": f["productId"],
                        "rating": f["rating"], "text": f["text"], "postedAt": iso_ms(f["postedAt"])}
                       for f in db.feedback.find()]
    return out


def consolidated(s) -> dict[str, list[dict]]:
    with psycopg.connect(s.pg_cons_dsn) as conn:
        out = pg_common(conn)
        lines: dict[int, list] = {}
        for r in _pg_rows(conn, "SELECT * FROM order_line ORDER BY order_id, line_no"):
            lines.setdefault(r["order_id"], []).append(
                {"lineNo": r["line_no"], "productId": r["product_id"], "quantity": r["quantity"],
                 "priceCents": r["price_cents"]})
        out["order"] = [{"id": r["id"], "customerId": r["customer_id"], "orderedAt": iso_ms(r["ordered_at"]),
                         "totalCents": r["total_cents"], "lines": lines.get(r["id"], [])}
                        for r in _pg_rows(conn, "SELECT * FROM orders")]
        out["feedback"] = [{"id": r["id"], "customerId": r["customer_id"], "productId": r["product_id"],
                            "rating": r["rating"], "text": r["text"], "postedAt": iso_ms(r["posted_at"])}
                           for r in _pg_rows(conn, "SELECT * FROM feedback")]
    return out


def run(d: Path) -> dict:
    s = settings()
    legs = {"generated": load_dir(d), "federated": federated(s), "consolidated": consolidated(s)}
    rep = {"entities": {}, "overall": "PASS"}
    for ent in ENTITIES:
        row = {leg: dict(zip(("count", "sha256"), checksum(v[ent]))) for leg, v in legs.items()}
        same = len({(x["count"], x["sha256"]) for x in row.values()}) == 1
        row["status"] = "PASS" if same else "FAIL"
        if not same:
            rep["overall"] = "FAIL"
        rep["entities"][ent] = row
    (d / "equivalence.json").write_text(json.dumps(rep, indent=2))
    return rep


if __name__ == "__main__":
    rep = run(Path(sys.argv[1]))
    for ent, row in rep["entities"].items():
        print(f"T-D2 {ent:9s} n={row['federated']['count']:>7}  "
              f"gen={row['generated']['sha256'][:12]} fed={row['federated']['sha256'][:12]} "
              f"cons={row['consolidated']['sha256'][:12]}  {row['status']}")
    print("T-D2", rep["overall"])
    sys.exit(0 if rep["overall"] == "PASS" else 1)
