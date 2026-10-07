"""HS3 pilot benchmark: W5 at a few sweep points, all four arms, one run directory.

Arms (HS2 §3.2, §6.5):
  A1  engine, all rewrite rules on (pushdown + bind join)
  A2  consolidated: hand-written reference SQL on pg-cons, timed until rows are in the
      same nested form (the JSON the query returns, decoded)
  A3  hand-written full fetches from both federated stores + Python dict join (context only)
  A4  engine, every rewrite rule off (full scans, residual everything)

Protocol (HS2 §6.6, §10.3): per point, W warm-ups discarded, then R timed repetitions;
arm order shuffled per repetition with a recorded seed; gc.collect() before each timed call.
The run is REFUSED unless the oracle agrees at every point first (HS2 §11.3).

Usage: python -m bench.run --points 10,100,1000 --warmup 3 --reps 7 --data <dataset dir>
"""

from __future__ import annotations

import argparse
import csv
import gc
import json
import random
import statistics
import sys
import time
from pathlib import Path

import psycopg
from pymongo import MongoClient

from bench.manifest import environment, new_run_dir
from bench.workload import WORKLOAD, sql_params
from engine.api import Engine
from engine.config import settings
from engine.naming import iso_ms
from engine.rewrite.rules import RuleConfig
from oracle.run import check

T = WORKLOAD["W5"]


class ArmA2:
    def __init__(self, dsn):
        self.conn = psycopg.connect(dsn, autocommit=True)

    def __call__(self, v):
        t0 = time.perf_counter_ns()
        cur = self.conn.execute(T.reference_sql, sql_params(v))
        data = cur.fetchone()[0]                # psycopg decodes json -> nested Python
        ns = time.perf_counter_ns() - t0
        payload = len(cur.pgresult.get_value(0, 0) or b"")
        return ns, payload, data


class ArmA3:
    """Hand-written: fetch both collections whole, filter and join in Python."""

    def __init__(self, s):
        self.pg = psycopg.connect(s.pg_fed_dsn, autocommit=True)
        self.mongo = MongoClient(s.mongo_uri)[s.mongo_db]

    def __call__(self, v):
        t0 = time.perf_counter_ns()
        cur = self.pg.execute("SELECT id, email, sel FROM customer")
        custs = [c for c in cur.fetchall() if c[2] < v["t"]]
        by_cust: dict[int, list] = {}
        n_docs = 0
        for o in self.mongo.orders.find({}, {"customerId": 1, "totalCents": 1, "orderedAt": 1}):
            n_docs += 1
            by_cust.setdefault(o["customerId"], []).append(
                {"id": str(o["_id"]), "totalCents": o["totalCents"], "orderedAt": iso_ms(o["orderedAt"])})
        data = [{"id": str(c[0]), "email": c[1], "orders": by_cust.get(c[0], [])} for c in custs]
        return time.perf_counter_ns() - t0, None, data


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--points", default="10,100,1000")
    ap.add_argument("--warmup", type=int, default=3)
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--seed", type=int, default=20261011)
    ap.add_argument("--data", type=Path, required=True)
    a = ap.parse_args()
    points = [int(x) for x in a.points.split(",")]
    s = settings()

    a1, a4 = Engine(rules=RuleConfig.a1()), Engine(rules=RuleConfig.a4())
    with psycopg.connect(s.pg_cons_dsn) as cons:
        gate = [check({"A1": a1, "A4": a4}, cons, T, {"t": t}) for t in points]
    if not all(g["verdict"] == "match" for g in gate):
        print("RUN REFUSED: oracle does not agree at every point", file=sys.stderr)
        return 2

    def eng_arm(e):
        def f(v):
            gc_count = gc.get_count()
            t0 = time.perf_counter_ns()
            r = e.execute(T.query, v)
            ns = time.perf_counter_ns() - t0
            assert not r.errors, r.errors
            return ns, r.trace.payload_bytes, r.trace, gc_count
        return f

    # Bytes come from the untimed oracle-gate pass above; timed repetitions run with
    # byte counting off so instrumentation never sits inside a latency (ISSUE-024).
    gate_bytes = {(g["variables"]["t"], arm): r for g in gate for arm, r in g["arms"].items()}
    for e in (a1, a4):
        for ad in e.adapters.values():
            ad.count_bytes = False
    arms = {"A1": eng_arm(a1), "A4": eng_arm(a4), "A2": ArmA2(s.pg_cons_dsn), "A3": ArmA3(s)}
    rng = random.Random(a.seed)
    run = new_run_dir("pilot-W5")
    env = environment(a1.adapters)
    timing_rows, byte_rows = [], []

    for t in points:
        v = {"t": t}
        for _ in range(a.warmup):
            for f in arms.values():
                f(v)
        for rep in range(a.reps):
            order = list(arms)
            rng.shuffle(order)
            for pos, arm in enumerate(order):
                gc.collect()
                out = arms[arm](v)
                ns, payload = out[0], out[1]
                row = {"template": "W5", "t": t, "rep": rep, "arm": arm, "position": pos,
                       "latency_ms": round(ns / 1e6, 4)}
                if arm in ("A1", "A4"):
                    tr = out[2]
                    row.update({f"span_{k}_ms": round(x / 1e6, 4) for k, x in tr.spans_ns.items()})
                    row["k"] = tr.driving_rows
                    if rep == 0:
                        g = gate_bytes[(t, arm)]
                        byte_rows.append({"template": "W5", "t": t, "arm": arm, "k": g["k"],
                                          "payload_bytes": g["payload_bytes"],
                                          "rows_transferred": g["rows_transferred"],
                                          "statements": g["statements"]})
                elif arm == "A2" and rep == 0:
                    byte_rows.append({"template": "W5", "t": t, "arm": "A2", "k": None,
                                      "payload_bytes": payload, "rows_transferred": 1, "statements": 1})
                timing_rows.append(row)

    cols = sorted({k for r in timing_rows for k in r}, key=lambda k: (not k.startswith(("template", "t", "rep", "arm", "pos", "lat")), k))
    with (run / "timings.csv").open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols)
        w.writeheader()
        w.writerows(timing_rows)
    with (run / "bytes.csv").open("w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(byte_rows[0]))
        w.writeheader()
        w.writerows(byte_rows)

    summary = []
    for t in points:
        for arm in ("A1", "A2", "A3", "A4"):
            xs = sorted(r["latency_ms"] for r in timing_rows if r["t"] == t and r["arm"] == arm)
            q = statistics.quantiles(xs, n=4) if len(xs) >= 2 else [xs[0]] * 3
            summary.append({"t": t, "arm": arm, "median_ms": round(statistics.median(xs), 3),
                            "iqr_ms": round(q[2] - q[0], 3), "n": len(xs)})
    dataset = json.loads((a.data / "meta.json").read_text())
    manifest = {
        "run": run.name, "template": "W5", "points": points, "warmup": a.warmup, "reps": a.reps,
        "arm_order_seed": a.seed, "bind_batch": a1.executor.batch,
        "dataset": {k: dataset[k] for k in ("seed", "scale", "counts", "zipf_exponent", "ratios_status")},
        "dataset_sha256": dataset["sha256"],
        "validation": json.loads((a.data / "validation.json").read_text()) if (a.data / "validation.json").exists() else None,
        "equivalence": json.loads((a.data / "equivalence.json").read_text()) if (a.data / "equivalence.json").exists() else None,
        "oracle_gate": [{"t": g["variables"]["t"], "verdict": g["verdict"]} for g in gate],
        "environment": env,
        "summary": summary,
    }
    (run / "manifest.json").write_text(json.dumps(manifest, indent=2, default=str))

    print(f"run dir: {run.relative_to(run.parents[2])}")
    print(f"setup:   {env['setup']}")
    print(f"{'t':>6} {'arm':4} {'median ms':>10} {'IQR':>8}   bytes")
    bmap = {(b["t"], b["arm"]): b["payload_bytes"] for b in byte_rows}
    for r in summary:
        print(f"{r['t']:>6} {r['arm']:4} {r['median_ms']:>10.3f} {r['iqr_ms']:>8.3f}   {bmap.get((r['t'], r['arm']), '')}")
    a1.close(); a4.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
