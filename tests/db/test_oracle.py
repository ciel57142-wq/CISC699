"""T-C1 / T-O1: every template, both arms, against the reference SQL on pg-cons."""

import pytest

from bench.workload import WORKLOAD
from engine.api import Engine
from engine.rewrite.rules import RuleConfig
from oracle.run import check

SWEEP = [0, 1, 3, 10, 30, 100, 300, 1000, 3000, 10000]   # HS2 §6.6 points (+ 0, the empty case)


@pytest.mark.parametrize("tid", list(WORKLOAD))
def test_template_matches_reference(engines, cons, tid):
    row = check(engines, cons, WORKLOAD[tid])
    for arm, r in row["arms"].items():
        assert r["verdict"] == "match", f"{tid} {arm}: {r.get('diff') or r.get('errors')}"


@pytest.mark.parametrize("t", SWEEP)
def test_w5_sweep_points_match(engines, cons, t):
    row = check(engines, cons, WORKLOAD["W5"], {"t": t})
    assert row["verdict"] == "match", row


def test_pushdown_reduces_bytes_on_selective_queries(engines, cons):
    """T-E2 shape check at development scale (not the SC-3 measurement itself)."""
    row = check(engines, cons, WORKLOAD["W5"], {"t": 100})
    assert row["arms"]["A1"]["payload_bytes"] < 0.3 * row["arms"]["A4"]["payload_bytes"]


@pytest.mark.parametrize("batch", [1, 2, 7, 1000])
def test_bind_join_batch_boundaries(cons, batch):
    """R20: batch size must never change the answer (keys split across many $in batches)."""
    eng = Engine(rules=RuleConfig.a1(), batch=batch)
    try:
        row = check({"A1": eng}, cons, WORKLOAD["W5"], {"t": 3000})
        assert row["verdict"] == "match", row
        if batch == 1:
            assert row["arms"]["A1"]["statements"] == row["arms"]["A1"]["k"] + 1
    finally:
        eng.close()


def test_k_is_recorded(engines, cons):
    row = check(engines, cons, WORKLOAD["W5"], {"t": 10000})
    assert row["arms"]["A1"]["k"] == row["reference_rows"]


def test_bind_join_skips_backend_when_no_keys(engines):
    r = engines["A1"].execute("{ customers(where: {sel: {lt: 0}}) { id orders { id } } }")
    assert r.data == {"customers": []}
    assert len(r.trace.fragments) == 1        # the Mongo side was never contacted
