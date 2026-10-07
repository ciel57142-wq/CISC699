"""T-D1: two generator runs with the same seed must produce byte-identical files."""

from __future__ import annotations

import hashlib
import sys
from pathlib import Path

from bench.data.gen import ENTITIES


def compare(a: Path, b: Path) -> list[str]:
    bad = []
    for ent in ENTITIES:
        ha = hashlib.sha256((a / f"{ent}.jsonl").read_bytes()).hexdigest()
        hb = hashlib.sha256((b / f"{ent}.jsonl").read_bytes()).hexdigest()
        print(f"T-D1 {ent:9s} {ha[:16]}  {hb[:16]}  {'identical' if ha == hb else 'DIFFERENT'}")
        if ha != hb:
            bad.append(ent)
    return bad


if __name__ == "__main__":
    bad = compare(Path(sys.argv[1]), Path(sys.argv[2]))
    print("T-D1 PASS" if not bad else f"T-D1 FAIL: {bad}")
    sys.exit(1 if bad else 0)
