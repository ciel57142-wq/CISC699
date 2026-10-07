"""T-O1 differential oracle: every workload template, engine (arms A1 and A4) vs the
reference SQL on pg-cons. Any divergence exits non-zero (HS2 §4.7: it fails CI).

Usage: python -m oracle.run [--out DATA_DIR] [--only W5,W9]
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

import psycopg

from bench.workload import WORKLOAD, Template, sql_params
from engine.api import Engine
from engine.config import settings
from engine.rewrite.rules import RuleConfig
from oracle.compare import compare


def reference(conn, t: Template, variables: dict):
    with conn.cursor() as cur:
        cur.execute(t.reference_sql, sql_params(variables))
        return cur.fetchone()[0]


def check(engines: dict[str, Engine], conn, t: Template, variables: dict | None = None) -> dict:
    v = dict(t.variables if variables is None else variables)
    ref = reference(conn, t, v)
    row = {"template": t.id, "variables": v, "reference_rows": len(ref), "arms": {}}
    for arm, eng in engines.items():
        r = eng.execute(t.query, v)
        if r.errors:
            row["arms"][arm] = {"verdict": "ERROR", "errors": r.errors}
            continue
        got = next(iter(r.data.values()))
        d = compare(got, ref, t.ordered)
        row["arms"][arm] = {"verdict": "match" if not d else "DIVERGE", "diff": d[:10],
                            "rows": len(got), **r.trace.summary()}
    row["verdict"] = "match" if all(a["verdict"] == "match" for a in row["arms"].values()) else "DIVERGE"
    return row


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=None)
    ap.add_argument("--only", default="")
    a = ap.parse_args()
    only = [x for x in a.only.split(",") if x]
    engines = {"A1": Engine(rules=RuleConfig.a1()), "A4": Engine(rules=RuleConfig.a4())}
    rows = []
    with psycopg.connect(settings().pg_cons_dsn) as conn:
        for t in WORKLOAD.values():
            if only and t.id not in only:
                continue
            row = check(engines, conn, t)
            rows.append(row)
            a1, a4 = row["arms"]["A1"], row["arms"]["A4"]
            b1, b4 = a1.get("payload_bytes"), a4.get("payload_bytes")
            red = f"{100 * (1 - b1 / b4):5.1f}%" if b1 is not None and b4 else "   n/a"
            print(f"T-O1 {t.id:4s} {t.stores:12s} rows={row['reference_rows']:>6}  "
                  f"A1={a1['verdict']:7s} A4={a4['verdict']:7s} bytes A1={b1} A4={b4} reduction={red}"
                  f"{'  (held out)' if t.held_out else ''}")
            for arm in ("A1", "A4"):
                for d in row["arms"][arm].get("diff", []) + row["arms"][arm].get("errors", []):
                    print(f"      {arm}: {d}")
    ok = all(r["verdict"] == "match" for r in rows)
    report = {"generated_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "overall": "PASS" if ok else "FAIL", "templates": rows}
    if a.out:
        (a.out / "oracle.json").write_text(json.dumps(report, indent=2, default=str))
    print("T-O1", report["overall"], f"({sum(r['verdict'] == 'match' for r in rows)}/{len(rows)} templates agree on both arms)")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
