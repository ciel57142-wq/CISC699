"""Adapter interface (HS2 §4.9). Adapters translate a ScanFragment into the backend's native
query, run it, and report how many payload bytes came back.

Rows are returned as dicts keyed by GraphQL field name with internal values (int, str,
tz-aware datetime, list of dicts for embedded fields). Aggregate rows are
{"key": {field: value}, "count": n, "sum:f": v, "avg:f": v, "min:f": v, "max:f": v}.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol

from engine.rewrite.physical import ScanFragment


@dataclass
class ByteCount:
    payload_bytes: int = 0      # PG: text-format field lengths; Mongo: BSON document sizes
    rows: int = 0
    fetch_ns: int = 0           # send query .. last byte received
    decode_ns: int = 0          # wire format -> Python values
    statements: int = 0


@dataclass
class ScanResult:
    rows: list[dict]
    bytes: ByteCount = field(default_factory=ByteCount)
    native: str = ""            # the SQL or pipeline actually sent, for EXPLAIN-style evidence


class Adapter(Protocol):
    name: str
    kind: str

    def scan(self, frag: ScanFragment) -> ScanResult: ...

    def row_count(self, collection: str) -> int: ...

    def close(self) -> None: ...
