"""MongoDB adapter: ScanFragment -> one aggregate() pipeline (pymongo 4).

Documents come back as RawBSONDocument so the payload size is the exact BSON length;
decoding to Python values is timed separately.
"""

from __future__ import annotations

import time
from datetime import timezone

import bson
from bson.codec_options import CodecOptions
from bson.raw_bson import RawBSONDocument
from pymongo import MongoClient
from pymongo.errors import ConnectionFailure, OperationFailure, PyMongoError, ServerSelectionTimeoutError

from engine.adapters.base import ByteCount, ScanResult
from engine.errors import BACKEND_UNAVAILABLE, INTERNAL, EngineError
from engine.naming import mongo_field
from engine.rewrite.physical import ScanFragment

_OPS = {"eq": "$eq", "lt": "$lt", "gt": "$gt", "lte": "$lte", "gte": "$gte", "in": "$in"}
RAW = CodecOptions(document_class=RawBSONDocument)
DECODE = CodecOptions(tz_aware=True, tzinfo=timezone.utc)


def build_pipeline(frag: ScanFragment) -> list[dict]:
    conds = []
    for p in frag.predicates:
        v = list(p.value) if p.op == "in" else p.value
        conds.append({mongo_field(p.field): {_OPS[p.op]: v}})
    if frag.key_filter:
        k, keys = frag.key_filter
        conds.append({mongo_field(k): {"$in": list(keys)}})
    pipe: list[dict] = []
    if conds:
        pipe.append({"$match": conds[0] if len(conds) == 1 else {"$and": conds}})
    if frag.aggregate:
        g = frag.aggregate
        grp: dict = {"_id": {f: f"${mongo_field(f)}" for f in g.group_by} if g.group_by else None,
                     "count": {"$sum": 1}}
        for fn, f in g.measures:
            grp[f"{fn}__{f}"] = {f"${fn}": f"${mongo_field(f)}"}
        pipe.append({"$group": grp})
        return pipe
    if frag.order:
        pipe.append({"$sort": {mongo_field(f): (-1 if d == "DESC" else 1) for f, d in frag.order}})
    if frag.offset:
        pipe.append({"$skip": frag.offset})
    if frag.limit is not None:
        pipe.append({"$limit": frag.limit})
    if frag.projection is not None:
        proj = {mongo_field(f): 1 for f in frag.projection}
        if "id" not in frag.projection:
            proj["_id"] = 0
        pipe.append({"$project": proj})
    return pipe


class MongoAdapter:
    kind = "mongodb"

    def __init__(self, name: str, uri: str, db: str, timeout_s: float = 5.0):
        self.name = name
        ms = int(timeout_s * 1000)
        self.client = MongoClient(uri, serverSelectionTimeoutMS=ms, connectTimeoutMS=ms)
        self.db = self.client[db]
        try:
            info = self.client.admin.command("buildInfo")
        except ServerSelectionTimeoutError as e:
            raise EngineError(BACKEND_UNAVAILABLE, f"{name}: {e}") from e
        self.server = f"MongoDB {info.get('version')}"
        self.capabilities = {"agg_fns": {"sum", "avg", "min", "max"}}
        if "ferretdbVersion" in info:
            # FerretDB 1.x (sandbox stand-in, ISSUE-003) has no $avg/$min/$max in $group.
            # Declaring it here makes R1b leave those aggregates residual at PLAN time,
            # as HS2 §4.6 requires, instead of failing at run time (ISSUE-010).
            self.server = f"FerretDB {info['ferretdbVersion']} (MongoDB {info.get('version')} wire)"
            self.capabilities = {"agg_fns": {"sum"}}

    def scan(self, frag: ScanFragment) -> ScanResult:
        pipe = build_pipeline(frag)
        bc = ByteCount(statements=1)
        coll = self.db.get_collection(frag.collection, codec_options=RAW)
        try:
            t0 = time.perf_counter_ns()
            raw = list(coll.aggregate(pipe))
            t1 = time.perf_counter_ns()
        except (ConnectionFailure, ServerSelectionTimeoutError) as e:
            raise EngineError(BACKEND_UNAVAILABLE, f"{self.name}: {e}") from e
        except (OperationFailure, PyMongoError) as e:
            # The backend rejected a pipeline the planner believed it could run: a bug.
            msg = e.details.get("errmsg") if isinstance(e, OperationFailure) and e.details else str(e)
            raise EngineError(INTERNAL, f"{self.name} rejected pipeline: {msg}") from e
        rows = []
        for d in raw:
            bc.payload_bytes += len(d.raw)
            rows.append(bson.decode(d.raw, codec_options=DECODE))
        if frag.aggregate:
            g = frag.aggregate
            out = []
            for r in rows:
                o = {"key": dict(r["_id"]) if r["_id"] else {}, "count": r["count"]}
                for fn, f in g.measures:
                    o[f"{fn}:{f}"] = r[f"{fn}__{f}"]
                out.append(o)
            if not out and not g.group_by:
                # $group with _id: null emits nothing on empty input; SQL emits one row
                # with count 0. Normalise to SQL semantics (ISSUE-009).
                out = [{"key": {}, "count": 0, **{f"{fn}:{f}": None for fn, f in g.measures}}]
            rows = out
        else:
            for r in rows:
                if "_id" in r:
                    r["id"] = r.pop("_id")
        t2 = time.perf_counter_ns()
        bc.rows, bc.fetch_ns, bc.decode_ns = len(rows), t1 - t0, t2 - t1
        return ScanResult(rows, bc, repr(pipe))

    def row_count(self, collection: str) -> int:
        return self.db[collection].estimated_document_count()

    def close(self) -> None:
        self.client.close()
