"""Seeded workload generator on the UniBench-shaped schema (HS2 §5.3, ADR-0005).

Same seed and scale -> byte-identical output files (T-D1). Output is JSON Lines, one file
per entity, keys sorted, every value an int or a string, so there is no float formatting
that could drift between machines.

Representation rules (HS2 §5.4) are applied here at the source:
  * money is integer cents
  * timestamps are UTC with millisecond precision, as ISO-8601 strings ending in "Z"
  * no field is ever omitted and nothing is null
  * all ids are integers starting at 1
"""

from __future__ import annotations

import argparse
import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
RATIOS = json.loads((HERE / "ratios.json").read_text())

ENTITIES = ("customer", "vendor", "product", "order", "feedback")

# Fixed synthetic vocabulary: no real text anywhere (HS1 assumption A5).
VOCAB = (
    "able acid amber angle apple arch area atom axis bake band bark base beam bell bend bird "
    "blue bolt bond bone book brass brick brush cable calm camp card cave cell chalk chart "
    "clay cloud coal coil cork crate crisp cube curve dawn deck delta dial disk dock dome "
    "draft drum dune dust echo edge ember fern fiber field flint foam fold forge frame frost "
    "gate gear glass grain grid gust harbor haze helm hinge hollow honey index iron ivory "
    "jade jet kettle kite knot lace ladder lamp lane lava leaf lens lever lime linen loop "
    "lunar maple marble mast meadow mesh mill mint mist moss nest node north oak oasis opal "
    "orbit oven paddle panel pearl pebble pine pivot plain plank plume polar pond prism pulse "
    "quartz quill rail range reed ridge ring river rope rust sail salt sand scale shell "
    "signal silk slate slope smoke socket solar spark spoke spring stem stone storm stream "
    "summit swift table thorn tide timber torch tower trail tulip valve vapor velvet vine "
    "violet wave wheat willow wind wire yarn zenith zinc"
).split()


def _iso(ms: int) -> str:
    dt = datetime.fromtimestamp(ms / 1000, tz=timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}Z"


def _parse_iso_ms(s: str) -> int:
    dt = datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)
    return int(round(dt.timestamp() * 1000))


def counts_for(scale: float) -> dict[str, int]:
    return {e: max(1, int(round(n * scale))) for e, n in RATIOS["per_scale_1"].items()}


def zipf_pmf(n: int, s: float) -> np.ndarray:
    ranks = np.arange(1, n + 1, dtype=np.float64)
    w = ranks ** (-s)
    return w / w.sum()


def generate(seed: int, scale: float, zipf_s: float | None = None) -> dict[str, list[dict]]:
    rng = np.random.Generator(np.random.PCG64(seed))
    n = counts_for(scale)
    zipf_s = RATIOS["zipf_exponent"] if zipf_s is None else zipf_s
    countries = RATIOS["countries"]
    t0, t1 = (_parse_iso_ms(x) for x in RATIOS["time_window_utc"])

    def words(k_lo: int, k_hi: int) -> str:
        k = int(rng.integers(k_lo, k_hi + 1))
        return " ".join(VOCAB[i] for i in rng.integers(0, len(VOCAB), size=k))

    customers = []
    sel = rng.integers(0, 10_000, size=n["customer"])  # uniform 0..9999 (HS2 §5.3)
    for i in range(n["customer"]):
        cid = i + 1
        customers.append({
            "id": cid,
            "email": f"c{cid:07d}@example.invalid",
            "country": countries[int(rng.integers(0, len(countries)))],
            "sel": int(sel[i]),
            "createdAt": _iso(int(rng.integers(t0, t1))),
        })

    vendors = [{
        "id": i + 1,
        "name": f"vendor-{i + 1:05d}",
        "country": countries[int(rng.integers(0, len(countries)))],
    } for i in range(n["vendor"])]

    products = []
    for i in range(n["product"]):
        products.append({
            "id": i + 1,
            "vendorId": int(rng.integers(1, n["vendor"] + 1)),
            "title": words(2, 4),
            "priceCents": int(rng.integers(100, 100_001)),
        })
    price = {p["id"]: p["priceCents"] for p in products}

    # Zipf fan-out: rank r has weight r^-s; ranks are assigned to customers by a random
    # permutation so fan-out is independent of `sel` (otherwise the sweep would be biased).
    rank_to_customer = rng.permutation(n["customer"]) + 1
    owner_rank = rng.choice(n["customer"], size=n["order"], p=zipf_pmf(n["customer"], zipf_s))
    lo, hi = RATIOS["order_lines"]["min"], RATIOS["order_lines"]["max"]
    orders = []
    for i in range(n["order"]):
        lines = []
        for ln in range(int(rng.integers(lo, hi + 1))):
            pid = int(rng.integers(1, n["product"] + 1))
            lines.append({
                "lineNo": ln + 1,
                "productId": pid,
                "quantity": int(rng.integers(1, 5)),
                "priceCents": price[pid],
            })
        orders.append({
            "id": i + 1,
            "customerId": int(rank_to_customer[owner_rank[i]]),
            "orderedAt": _iso(int(rng.integers(t0, t1))),
            "totalCents": sum(x["quantity"] * x["priceCents"] for x in lines),
            "lines": lines,
        })

    feedback = []
    for i in range(n["feedback"]):
        feedback.append({
            "id": i + 1,
            "customerId": int(rng.integers(1, n["customer"] + 1)),
            "productId": int(rng.integers(1, n["product"] + 1)),
            "rating": int(rng.integers(1, 6)),
            "text": words(8, 20),
            "postedAt": _iso(int(rng.integers(t0, t1))),
        })

    return {"customer": customers, "vendor": vendors, "product": products,
            "order": orders, "feedback": feedback}


def write(data: dict[str, list[dict]], out: Path, meta: dict) -> dict[str, str]:
    out.mkdir(parents=True, exist_ok=True)
    sums = {}
    for ent in ENTITIES:
        p = out / f"{ent}.jsonl"
        with p.open("w", encoding="utf-8", newline="\n") as f:
            for rec in data[ent]:
                f.write(json.dumps(rec, sort_keys=True, separators=(",", ":")) + "\n")
        sums[ent] = hashlib.sha256(p.read_bytes()).hexdigest()
    (out / "meta.json").write_text(json.dumps({**meta, "sha256": sums}, indent=2, sort_keys=True))
    return sums


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--seed", type=int, required=True)
    ap.add_argument("--scale", type=float, required=True)
    ap.add_argument("--zipf", type=float, default=None)
    ap.add_argument("--out", type=Path, required=True)
    a = ap.parse_args()
    data = generate(a.seed, a.scale, a.zipf)
    meta = {"seed": a.seed, "scale": a.scale,
            "zipf_exponent": RATIOS["zipf_exponent"] if a.zipf is None else a.zipf,
            "counts": {k: len(v) for k, v in data.items()},
            "ratios_status": "provisional" if "PROVISIONAL" in RATIOS["_status"] else "unibench"}
    sums = write(data, a.out, meta)
    print(json.dumps({"out": str(a.out), "counts": meta["counts"], "sha256": sums}, indent=1))


if __name__ == "__main__":
    main()
