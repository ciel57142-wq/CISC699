"""T-D1 at unit scale, the §11.1 validators, and the oracle's comparison rules."""

import hashlib
import json

from bench.data import gen, validate
from oracle.compare import compare


def _digest(data):
    return hashlib.sha256(json.dumps(data, sort_keys=True).encode()).hexdigest()


def test_generator_is_deterministic():
    assert _digest(gen.generate(7, 0.01)) == _digest(gen.generate(7, 0.01))


def test_different_seeds_differ():
    assert _digest(gen.generate(1, 0.01)) != _digest(gen.generate(2, 0.01))


def test_generated_data_passes_schema_and_referential_checks():
    d = gen.generate(3, 0.02)
    assert validate.schema_check(d) == []
    assert not any(validate.referential_check(d).values())


def test_schema_check_catches_corruption():
    d = gen.generate(3, 0.01)
    d["order"][0]["totalCents"] += 1
    d["customer"][0]["createdAt"] = "2025-01-01"
    errs = validate.schema_check(d)
    assert any("totalCents != sum" in e for e in errs) and any("createdAt" in e for e in errs)


def test_referential_check_catches_orphans():
    d = gen.generate(3, 0.01)
    d["order"][0]["customerId"] = 10**6
    assert validate.referential_check(d)["order.customerId"] == 1


def test_distribution_checks_skip_when_too_small_and_pass_when_large():
    small = gen.generate(1, 0.01)
    assert validate.sel_uniformity(small)["status"] == "SKIPPED"
    big = gen.generate(1, 0.1)
    assert validate.sel_uniformity(big)["status"] == "PASS"
    z = validate.zipf_fit(big, 1.0)
    assert z["status"] == "PASS" and z["rel_error"] < 0.05


def test_money_is_integer_cents_and_timestamps_are_ms_utc():
    d = gen.generate(1, 0.01)
    assert all(type(o["totalCents"]) is int for o in d["order"])
    assert all(o["orderedAt"].endswith("Z") and len(o["orderedAt"]) == 24 for o in d["order"])


def test_compare_order_insensitive_and_nested():
    a = [{"id": "1", "xs": [{"v": 1}, {"v": 2}]}, {"id": "2", "xs": []}]
    b = [{"id": "2", "xs": []}, {"id": "1", "xs": [{"v": 2}, {"v": 1}]}]
    assert compare(a, b, ordered=False) == []
    assert compare(a, b, ordered=True) != []


def test_compare_float_tolerance_only_for_floats():
    assert compare([{"avg": 1.0000000000001}], [{"avg": 1.0}], False) == []
    assert compare([{"avg": 1.001}], [{"avg": 1.0}], False) != []
    assert compare([{"n": 1}], [{"n": "1"}], False) != []          # type matters
    assert compare([{"n": 1}], [{"m": 1}], False) != []            # missing field


def test_compare_integral_float_vs_int_regression():
    """ISSUE-011 regression: PG emits avg 415482 as an int, the engine has 415482.0. Rows
    with near-equal averages must still pair by group key, whatever the input order."""
    engine = [{"key": {"c": 27}, "avg": 415482.0}, {"key": {"c": 300}, "avg": 415481.6666666667}]
    ref = [{"key": {"c": 300}, "avg": 415481.6666666667}, {"key": {"c": 27}, "avg": 415482}]
    assert compare(engine, ref, ordered=False) == []
    assert compare(list(reversed(engine)), ref, ordered=False) == []
