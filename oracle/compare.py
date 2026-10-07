"""Order-insensitive result comparison (HS2 §4.7, §5.4).

Unordered mode sorts every list recursively by canonical JSON before comparing. Ordered mode
keeps list order (used where the root order is part of the answer, W1 and W9). Floats (only
`avg` produces them) compare with relative tolerance 1e-9; everything else compares exactly.
"""

from __future__ import annotations

import json
import math

REL_TOL = 1e-9


def _num(x: float):
    """Sort-key form of a float: 9 significant digits, and integral values as int.
    PostgreSQL renders an integral float8 (avg = 415482) as the JSON integer 415482 while
    the engine holds 415482.0; without this the two sides sort differently and the oracle
    pairs the wrong rows (ISSUE-011). Only the sort key is rounded; diff() still compares
    the original values with REL_TOL. Known limit: two values within 1e-9 that straddle a
    9th-digit rounding boundary would sort apart; diff() would then report it."""
    r = float(f"{x:.9g}")
    return int(r) if r.is_integer() else r


def _sort_key(v) -> str:
    def norm(x):
        if isinstance(x, float):
            return _num(x)
        if isinstance(x, dict):
            return {k: norm(y) for k, y in x.items()}
        if isinstance(x, list):
            return sorted((norm(y) for y in x), key=lambda z: json.dumps(z, sort_keys=True))
        return x
    return json.dumps(norm(v), sort_keys=True)


def canonical(v, ordered: bool):
    if isinstance(v, dict):
        return {k: canonical(x, ordered) for k, x in v.items()}
    if isinstance(v, list):
        items = [canonical(x, ordered) for x in v]
        return items if ordered else sorted(items, key=_sort_key)
    return v


def diff(a, b, path: str = "$") -> list[str]:
    """Return human-readable differences (empty list = match)."""
    if isinstance(a, float) or isinstance(b, float):
        if isinstance(a, (int, float)) and isinstance(b, (int, float)) and \
                math.isclose(a, b, rel_tol=REL_TOL, abs_tol=0.0):
            return []
        return [f"{path}: {a!r} != {b!r}"]
    if type(a) is not type(b):
        return [f"{path}: type {type(a).__name__} != {type(b).__name__} ({a!r} vs {b!r})"]
    if isinstance(a, dict):
        out = []
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                out.append(f"{path}.{k}: missing on {'engine' if k not in a else 'reference'} side")
            else:
                out += diff(a[k], b[k], f"{path}.{k}")
        return out
    if isinstance(a, list):
        if len(a) != len(b):
            return [f"{path}: length {len(a)} != {len(b)}"]
        out = []
        for i, (x, y) in enumerate(zip(a, b)):
            out += diff(x, y, f"{path}[{i}]")
            if len(out) > 10:
                break
        return out
    return [] if a == b else [f"{path}: {a!r} != {b!r}"]


def compare(engine_value, reference_value, ordered: bool) -> list[str]:
    return diff(canonical(engine_value, ordered), canonical(reference_value, ordered))
