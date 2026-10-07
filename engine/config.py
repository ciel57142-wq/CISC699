"""Connection settings, read from the environment (.env is loaded if present).

Nothing secret is ever written to the repository; see .env.example.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from dotenv import load_dotenv

REPO = Path(__file__).resolve().parent.parent
load_dotenv(REPO / ".env")


@dataclass(frozen=True)
class Settings:
    pg_fed_dsn: str
    pg_cons_dsn: str
    mongo_uri: str
    mongo_db: str
    timeout_s: float = 5.0


def settings() -> Settings:
    return Settings(
        pg_fed_dsn=os.getenv("PG_FED_DSN", "postgresql://polystore:polystore@localhost:5432/fed"),
        pg_cons_dsn=os.getenv("PG_CONS_DSN", "postgresql://polystore:polystore@localhost:5433/cons"),
        mongo_uri=os.getenv("MONGO_URI", "mongodb://localhost:27017"),
        mongo_db=os.getenv("MONGO_DB", "fed"),
    )
