"""Error contract at run time (HS2 §7.2)."""

import pytest

from engine.adapters.mongo import MongoAdapter
from engine.adapters.pg import PostgresAdapter
from engine.errors import EngineError


def test_postgres_unreachable_is_backend_unavailable():
    with pytest.raises(EngineError) as e:
        PostgresAdapter("pg", "postgresql://polystore@127.0.0.1:1/x", timeout_s=2)
    assert e.value.code == "BACKEND_UNAVAILABLE"


def test_mongo_unreachable_is_backend_unavailable():
    with pytest.raises(EngineError) as e:
        MongoAdapter("mongo", "mongodb://127.0.0.1:1", "x", timeout_s=1)
    assert e.value.code == "BACKEND_UNAVAILABLE"


def test_unsupported_operation_returns_graphql_error(engines):
    r = engines["A1"].execute("mutation { x }")
    assert r.data is None and r.errors[0]["extensions"]["code"] == "UNSUPPORTED_OPERATION"


def test_explain_is_available_for_evidence(engines):
    r = engines["A1"].execute("{ customers(where: {sel: {lt: 5}}) { id orders { id } } }")
    text = r.explain()
    assert "bind join (keys pushed)" in text and "pushed preds: sel lt 5" in text
