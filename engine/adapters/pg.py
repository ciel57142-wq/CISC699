"""PostgreSQL adapter: ScanFragment -> one parameterised SQL statement (psycopg 3).

String comparisons and sorts use COLLATE "C" so they match MongoDB's binary string order;
without it the oracle diverges on any string sort (ISSUE-007).
"""

from __future__ import annotations

import time

import psycopg
from psycopg import sql

from engine.adapters.base import ByteCount, ScanResult
from engine.errors import BACKEND_UNAVAILABLE, INTERNAL, EngineError
from engine.naming import pg_col
from engine.rewrite.physical import ScanFragment

_OPS = {"eq": "=", "lt": "<", "gt": ">", "lte": "<=", "gte": ">="}


def _col(frag: ScanFragment, f: str) -> sql.Composable:
    c = sql.Identifier(pg_col(f))
    return sql.SQL('{} COLLATE "C"').format(c) if frag.field_types.get(f) == "String" else c


def build_sql(frag: ScanFragment) -> tuple[sql.Composed, list]:
    params: list = []
    where: list[sql.Composable] = []
    for p in frag.predicates:
        if p.op == "in":
            where.append(sql.SQL("{} = ANY(%s)").format(_col(frag, p.field)))
            params.append(list(p.value))
        else:
            where.append(sql.SQL("{} {} %s").format(_col(frag, p.field), sql.SQL(_OPS[p.op])))
            params.append(p.value)
    if frag.key_filter:
        k, keys = frag.key_filter
        where.append(sql.SQL("{} = ANY(%s)").format(sql.Identifier(pg_col(k))))
        params.append(list(keys))
    table = sql.Identifier(frag.collection)
    wsql = sql.SQL(" WHERE ") + sql.SQL(" AND ").join(where) if where else sql.SQL("")

    if frag.aggregate:
        g = frag.aggregate
        sel = [sql.SQL("{} AS {}").format(sql.Identifier(pg_col(f)), sql.Identifier(f)) for f in g.group_by]
        sel.append(sql.SQL("count(*) AS count"))
        for fn, f in g.measures:
            expr = {"sum": "sum({})::bigint", "avg": "avg({})::float8",
                    "min": "min({})", "max": "max({})"}[fn]
            sel.append(sql.SQL(expr + " AS {}").format(sql.Identifier(pg_col(f)), sql.Identifier(f"{fn}:{f}")))
        q = sql.SQL("SELECT {} FROM {}").format(sql.SQL(", ").join(sel), table) + wsql
        if g.group_by:
            q += sql.SQL(" GROUP BY ") + sql.SQL(", ").join(sql.Identifier(pg_col(f)) for f in g.group_by)
        return q, params

    fields = frag.projection if frag.projection is not None else list(frag.all_fields)
    cols = sql.SQL(", ").join(
        sql.SQL("{} AS {}").format(sql.Identifier(pg_col(f)), sql.Identifier(f)) for f in fields)
    q = sql.SQL("SELECT {} FROM {}").format(cols, table) + wsql
    if frag.order:
        q += sql.SQL(" ORDER BY ") + sql.SQL(", ").join(
            sql.SQL("{} {}").format(_col(frag, f), sql.SQL("DESC" if d == "DESC" else "ASC"))
            for f, d in frag.order)
    if frag.limit is not None:
        q += sql.SQL(" LIMIT %s")
        params.append(frag.limit)
    if frag.offset:
        q += sql.SQL(" OFFSET %s")
        params.append(frag.offset)
    return q, params


class PostgresAdapter:
    kind = "postgres"

    def __init__(self, name: str, dsn: str, timeout_s: float = 5.0):
        self.name = name
        try:
            self.conn = psycopg.connect(dsn, autocommit=True, connect_timeout=int(timeout_s))
        except psycopg.OperationalError as e:
            raise EngineError(BACKEND_UNAVAILABLE, f"{name}: {e}".strip()) from e
        self.server = f"PostgreSQL {self.conn.info.server_version}"
        self.capabilities = {"agg_fns": {"sum", "avg", "min", "max"}}

    def scan(self, frag: ScanFragment) -> ScanResult:
        q, params = build_sql(frag)
        bc = ByteCount(statements=1)
        try:
            with self.conn.cursor() as cur:
                t0 = time.perf_counter_ns()
                cur.execute(q, params)
                t1 = time.perf_counter_ns()
                res = cur.pgresult
                names = [c.name for c in cur.description]
                raw = cur.fetchall()
                rows = [dict(zip(names, r)) for r in raw]
                t2 = time.perf_counter_ns()
                # Instrumentation, outside the timed spans: text-format payload bytes.
                for i in range(res.ntuples):
                    for j in range(res.nfields):
                        v = res.get_value(i, j)
                        bc.payload_bytes += 0 if v is None else len(v)
        except psycopg.OperationalError as e:
            raise EngineError(BACKEND_UNAVAILABLE, f"{self.name}: {e}".strip()) from e
        except psycopg.Error as e:
            raise EngineError(INTERNAL, f"{self.name} rejected SQL: {e}".strip()) from e
        bc.rows, bc.fetch_ns, bc.decode_ns = len(rows), t1 - t0, t2 - t1
        if frag.aggregate:
            g = frag.aggregate.group_by
            rows = [{"key": {f: r[f] for f in g}, **{k: v for k, v in r.items() if k not in g}}
                    for r in rows]
        return ScanResult(rows, bc, q.as_string(self.conn))

    def row_count(self, collection: str) -> int:
        with self.conn.cursor() as cur:
            cur.execute(sql.SQL("SELECT count(*) FROM {}").format(sql.Identifier(collection)))
            return cur.fetchone()[0]

    def close(self) -> None:
        self.conn.close()
