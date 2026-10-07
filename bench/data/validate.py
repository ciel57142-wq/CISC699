"""HS2 §11.1 data-validation steps, run on generated files before every load.

1. Schema check   - every record has exactly the expected fields with the expected types
2. Referential    - every foreign key resolves; orphan count must be 0
3. Distribution   - `sel` uniform (chi-square, p > 0.01); Zipf exponent of orders per
                    customer within 5% of the configured value (MLE on rank-sorted counts)

Any failure exits non-zero so `make load` stops. Distribution checks need enough rows to
mean anything; below the minimums they report SKIPPED rather than a pass (ISSUE-005).
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

import numpy as np
from scipy.optimize import minimize_scalar
from scipy.stats import chisquare

from bench.data.gen import ENTITIES

ISO = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$")

SCHEMA: dict[str, dict[str, str]] = {
    "customer": {"id": "int", "email": "str", "country": "str", "sel": "int", "createdAt": "ts"},
    "vendor": {"id": "int", "name": "str", "country": "str"},
    "product": {"id": "int", "vendorId": "int", "title": "str", "priceCents": "int"},
    "order": {"id": "int", "customerId": "int", "orderedAt": "ts", "totalCents": "int",
              "lines": "lines"},
    "feedback": {"id": "int", "customerId": "int", "productId": "int", "rating": "int",
                 "text": "str", "postedAt": "ts"},
}
LINE = {"lineNo": "int", "productId": "int", "quantity": "int", "priceCents": "int"}
INT32 = 2**31 - 1


def _ok(kind: str, v) -> bool:
    if kind == "int":
        return type(v) is int and -INT32 <= v <= INT32
    if kind == "str":
        return isinstance(v, str) and v != ""
    if kind == "ts":
        return isinstance(v, str) and bool(ISO.match(v))
    if kind == "lines":
        return isinstance(v, list) and len(v) > 0 and all(
            isinstance(x, dict) and set(x) == set(LINE) and all(_ok(LINE[k], x[k]) for k in LINE)
            for x in v)
    raise ValueError(kind)


def load_dir(d: Path) -> dict[str, list[dict]]:
    return {e: [json.loads(l) for l in (d / f"{e}.jsonl").open()] for e in ENTITIES}


def schema_check(data) -> list[str]:
    errs = []
    for ent, recs in data.items():
        spec = SCHEMA[ent]
        ids = set()
        for r in recs:
            if set(r) != set(spec):
                errs.append(f"{ent} id={r.get('id')}: fields {sorted(r)} != {sorted(spec)}")
                continue
            for k, kind in spec.items():
                if not _ok(kind, r[k]):
                    errs.append(f"{ent} id={r['id']}: {k}={r[k]!r} is not {kind}")
            ids.add(r["id"])
        if ids != set(range(1, len(recs) + 1)):
            errs.append(f"{ent}: ids are not exactly 1..{len(recs)}")
        if ent == "order":
            for r in recs:
                if r["totalCents"] != sum(x["quantity"] * x["priceCents"] for x in r["lines"]):
                    errs.append(f"order {r['id']}: totalCents != sum of lines")
    return errs


def referential_check(data) -> dict[str, int]:
    ids = {e: {r["id"] for r in data[e]} for e in ENTITIES}
    return {
        "order.customerId": sum(r["customerId"] not in ids["customer"] for r in data["order"]),
        "order.lines.productId": sum(x["productId"] not in ids["product"]
                                     for r in data["order"] for x in r["lines"]),
        "feedback.customerId": sum(r["customerId"] not in ids["customer"] for r in data["feedback"]),
        "feedback.productId": sum(r["productId"] not in ids["product"] for r in data["feedback"]),
        "product.vendorId": sum(r["vendorId"] not in ids["vendor"] for r in data["product"]),
    }


def sel_uniformity(data, bins: int = 100, min_per_bin: int = 5) -> dict:
    sel = np.array([r["sel"] for r in data["customer"]])
    if len(sel) < bins * min_per_bin:
        return {"status": "SKIPPED", "reason": f"n={len(sel)} < {bins * min_per_bin}"}
    obs = np.bincount(sel * bins // 10_000, minlength=bins)
    p = float(chisquare(obs).pvalue)
    return {"status": "PASS" if p > 0.01 else "FAIL", "p_value": round(p, 4), "bins": bins}


def zipf_fit(data, configured: float, min_customers: int = 500, tol: float = 0.05) -> dict:
    n = len(data["customer"])
    if n < min_customers:
        return {"status": "SKIPPED", "reason": f"customers={n} < {min_customers}"}
    counts = np.bincount([r["customerId"] for r in data["order"]], minlength=n + 1)[1:]
    srt = np.sort(counts)[::-1]
    ranks = np.repeat(np.arange(1, n + 1), srt)
    slog = np.log(ranks).sum()
    r = np.arange(1, n + 1, dtype=float)

    def nll(s):
        return s * slog + len(ranks) * np.log((r ** -s).sum())

    est = float(minimize_scalar(nll, bounds=(0.2, 4.0), method="bounded").x)
    rel = abs(est - configured) / configured
    return {"status": "PASS" if rel <= tol else "FAIL", "configured": configured,
            "estimated": round(est, 4), "rel_error": round(rel, 4)}


def run(d: Path) -> dict:
    data = load_dir(d)
    meta = json.loads((d / "meta.json").read_text())
    s_errs = schema_check(data)
    orphans = referential_check(data)
    report = {
        "schema": {"status": "PASS" if not s_errs else "FAIL", "errors": s_errs[:20],
                   "records": {e: len(v) for e, v in data.items()}},
        "referential": {"status": "PASS" if not any(orphans.values()) else "FAIL",
                        "orphans": orphans},
        "sel_uniformity": sel_uniformity(data),
        "zipf_fanout": zipf_fit(data, meta["zipf_exponent"]),
    }
    report["overall"] = "FAIL" if any(v.get("status") == "FAIL" for v in report.values()) else "PASS"
    (d / "validation.json").write_text(json.dumps(report, indent=2))
    return report


if __name__ == "__main__":
    rep = run(Path(sys.argv[1]))
    for k, v in rep.items():
        if k != "overall":
            extra = {kk: vv for kk, vv in v.items() if kk not in ("status", "errors")}
            print(f"{k:15s} {v['status']:8s} {json.dumps(extra)}")
    print("validation", rep["overall"])
    sys.exit(0 if rep["overall"] == "PASS" else 1)
