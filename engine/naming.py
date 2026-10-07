"""Storage naming convention, shared by the loader and the adapters.

GraphQL fields are camelCase. PostgreSQL columns are their snake_case form. MongoDB fields
keep the GraphQL name, except `id`, which is stored as `_id`. Keeping this in one place is
what lets the schema stay free of storage names (connection details live in backends.toml,
column names are derived here).
"""

from __future__ import annotations

import re
from datetime import datetime, timezone

_CAMEL = re.compile(r"(?<!^)(?=[A-Z])")


def pg_col(field: str) -> str:
    return _CAMEL.sub("_", field).lower()


def mongo_field(field: str) -> str:
    return "_id" if field == "id" else field


def iso_ms(dt: datetime) -> str:
    """Canonical timestamp text: UTC, millisecond precision, trailing Z (HS2 §5.4)."""
    if dt.tzinfo is None:              # pymongo returns naive UTC datetimes by default
        dt = dt.replace(tzinfo=timezone.utc)
    dt = dt.astimezone(timezone.utc)
    return dt.strftime("%Y-%m-%dT%H:%M:%S.") + f"{dt.microsecond // 1000:03d}Z"


def parse_iso_ms(s: str) -> datetime:
    return datetime.strptime(s, "%Y-%m-%dT%H:%M:%S.%fZ").replace(tzinfo=timezone.utc)
