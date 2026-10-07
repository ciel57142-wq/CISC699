import psycopg
import pytest
from pymongo import MongoClient

from engine.api import Engine
from engine.config import settings
from engine.rewrite.rules import RuleConfig


def _up() -> str | None:
    s = settings()
    try:
        for dsn in (s.pg_fed_dsn, s.pg_cons_dsn):
            psycopg.connect(dsn, connect_timeout=2).close()
        MongoClient(s.mongo_uri, serverSelectionTimeoutMS=2000).admin.command("ping")
        with psycopg.connect(s.pg_cons_dsn) as c:
            if c.execute("SELECT count(*) FROM customer").fetchone()[0] == 0:
                return "databases are empty: run `make load`"
    except Exception as e:  # noqa: BLE001
        return f"databases not reachable ({type(e).__name__}): run `make up load`"
    return None


_reason = _up()


def pytest_collection_modifyitems(config, items):
    if _reason:
        skip = pytest.mark.skip(reason=_reason)
        for it in items:
            if "tests/db" in str(it.fspath):
                it.add_marker(skip)


@pytest.fixture(scope="session")
def engines():
    if _reason:
        pytest.skip(_reason)
    e = {"A1": Engine(rules=RuleConfig.a1()), "A4": Engine(rules=RuleConfig.a4())}
    yield e
    for x in e.values():
        x.close()


@pytest.fixture(scope="session")
def cons():
    with psycopg.connect(settings().pg_cons_dsn) as c:
        yield c
