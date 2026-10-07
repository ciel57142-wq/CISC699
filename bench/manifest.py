"""Run manifest (HS2 §8.4): everything needed to say exactly what produced a number.

`python -m bench.manifest` prints the environment fingerprint on its own.
"""

from __future__ import annotations

import hashlib
import json
import os
import platform
import subprocess
import time
from pathlib import Path

from engine.config import REPO

CONFIG_FILES = ["schema.graphql", "backends.toml", "compose.yaml", "pyproject.toml",
                "bench/data/ratios.json", "Makefile"]


def _sh(*cmd: str) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=20, cwd=REPO).stdout.strip()
    except Exception:  # noqa: BLE001
        return ""


def _sha(p: Path) -> str | None:
    return hashlib.sha256(p.read_bytes()).hexdigest() if p.exists() else None


def git_state() -> dict:
    return {"sha": _sh("git", "rev-parse", "HEAD") or None,
            "dirty": bool(_sh("git", "status", "--porcelain", "--untracked-files=no"))}


def hardware() -> dict:
    lscpu = _sh("lscpu")
    model = next((l.split(":", 1)[1].strip() for l in lscpu.splitlines() if l.startswith("Model name")), None)
    mem_kb = None
    try:
        mem_kb = int(next(l.split()[1] for l in open("/proc/meminfo") if l.startswith("MemTotal")))
    except Exception:  # noqa: BLE001
        pass
    osr = {}
    try:
        osr = dict(l.strip().split("=", 1) for l in open("/etc/os-release") if "=" in l)
    except Exception:  # noqa: BLE001
        pass
    return {"cpu_model": model, "logical_cpus": os.cpu_count(),
            "ram_gb": round(mem_kb / 1024 / 1024, 1) if mem_kb else None,
            "os": osr.get("PRETTY_NAME", "").strip('"') or platform.platform(),
            "kernel": platform.release(), "wsl": "microsoft" in platform.release().lower(),
            "python": platform.python_version(),
            "docker": _sh("docker", "version", "--format", "{{.Server.Version}}") or None,
            "compose": _sh("docker", "compose", "version", "--short") or None}


def images() -> dict:
    out = {}
    for svc in ("pg-fed", "pg-cons", "mongo-fed"):
        cid = _sh("docker", "compose", "ps", "-q", svc)
        if cid:
            out[svc] = _sh("docker", "inspect", "--format", "{{.Image}}", cid)
    return out


def environment(adapters: dict | None = None) -> dict:
    servers = {n: getattr(a, "server", None) for n, a in (adapters or {}).items()}
    sandbox = any("FerretDB" in (s or "") for s in servers.values())
    return {
        "captured_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "setup": "sandbox-native (FerretDB stand-in; timings NOT reportable)" if sandbox else "compose",
        "git": git_state(),
        "uv_lock_sha256": _sha(REPO / "uv.lock"),
        "config_sha256": {f: _sha(REPO / f) for f in CONFIG_FILES},
        "servers": servers,
        "image_ids": images(),
        "hardware": hardware(),
    }


def new_run_dir(tag: str = "") -> Path:
    sha = (git_state()["sha"] or "nogit")[:8]
    d = REPO / "bench" / "runs" / f"{time.strftime('%Y%m%dT%H%M%SZ', time.gmtime())}-{sha}{('-' + tag) if tag else ''}"
    d.mkdir(parents=True, exist_ok=False)
    return d


if __name__ == "__main__":
    from engine.api import Engine
    e = Engine()
    print(json.dumps(environment(e.adapters), indent=2))
    e.close()
