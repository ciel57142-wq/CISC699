# Environment notes

What the project runs on, what has been verified where, and what a new reader must do to get
from a clean checkout to green checks. Every run also writes a full fingerprint into
`bench/runs/<id>/manifest.json` (`make env-snapshot` prints it on its own).

## 1. Two setups, one codebase

| | **Project laptop (measurement)** | **Cloud sandbox (HS3 build, 2026-10-07)** |
|---|---|---|
| Start databases | `make up` (compose.yaml) | `make up-native` (scripts/native_up.sh) |
| pg-fed / pg-cons | `postgres:16` containers, `shared_buffers=768MB` | two native PostgreSQL 16.15 clusters, `shared_buffers=128MB` |
| mongo-fed | `mongo:7.0` container, WiredTiger cache 0.75 GB | **FerretDB 1.24.0** over a third PG cluster (MongoDB 7.0 wire protocol) |
| Resource parity (HS2 §8.3) | shared `cpuset`, identical CPU and memory limits | none: plain processes on 2 vCPU |
| Timings usable? | yes, once ISSUE-002 is closed | **no** (ISSUE-003, ISSUE-014); correctness and byte counts only |
| Verified so far | **not yet run** (ISSUE-002) | everything in `evidence/sandbox-2026-10-07-run{1,2}/` |

Same Python environment (`uv.lock`), same code, same `.env` keys in both. The manifest's `setup`
field says which one produced a run.

## 2. Toolchain (pinned)

| Layer | Version | Pinned by |
|---|---|---|
| Python | 3.12.13 | `.python-version`, `requires-python = ">=3.12,<3.13"` |
| Package manager | uv 0.11 | `uv.lock` with hashes (`make sync` = `uv sync --frozen`) |
| graphql-core | 3.2.13 | lock (ADR-0002) |
| psycopg (binary) | 3.3.6 | lock |
| pymongo | 4.18.2 | lock |
| numpy / scipy | 2.5.3 / 1.18.1 | lock |
| pytest / hypothesis / pytest-cov | 8.4.2 / 6.168.5 / 6.3.0 | lock (dev group) |
| DB images | `postgres:16`, `mongo:7.0` | tags today; digests after `make lock-digests` (ISSUE-004) |

`uv.lock` SHA-256 at HS3: `bbb7d3fcaf76282d…` (full hash in every manifest).

## 3. Sandbox snapshot (where the HS3 evidence came from)

Intel Xeon @ 2.80 GHz, 2 logical CPUs, 7 GB RAM, Ubuntu 24.04.5 LTS, kernel 6.18.44, PostgreSQL
16.15 (Ubuntu package), FerretDB v1.24.0 (GitHub release binary), Docker CLI present but no registry
access. Outbound network limited to PyPI, npm and GitHub.

## 4. Configuration and secrets

- `.env` (gitignored) holds DSNs and limits; copy from `.env.example`. Nothing secret is committed.
- `backends.toml` names environment variables, never values.
- `DB_CPUSET`, `DB_CPUS`, `DB_MEM` feed compose.yaml. Defaults follow HS2 §8.3 (`2,3`, 2 CPUs, 3 GB).
  If `nproc` on the laptop is below 4, set `DB_CPUSET=1`, `DB_CPUS=1` and record an ADR (HS2 §8.2).
- On Windows: keep the repository inside the WSL2 filesystem (`~/…`), never under `/mnt/c`.

## 5. From a clean checkout on the project laptop

```bash
# once
curl -LsSf https://astral.sh/uv/install.sh | sh     # installs uv
cp .env.example .env                                 # then edit DB_CPUSET etc. after `nproc`
make sync                                            # exact locked environment
make up                                              # three containers, healthchecked
make lock-digests                                    # pins images by digest (ISSUE-004)

# every time (or: scripts/clean_restart.sh laptop-<date> compose)
make data load equiv SCALE=0.1                       # T-D1, validation, T-D2
make test                                            # unit + DB tests
make oracle SCALE=0.1                                # T-O1 on all ten templates
make pilot SCALE=0.1                                 # pilot run -> bench/runs/<id>/
```

## 6. Permissions, files and ports

| Need | Where | Notes |
|---|---|---|
| Ports | 5432 (pg-fed), 5433 (pg-cons), 27017 (mongo-fed) | change in `.env` if taken |
| Writes | `bench/data/out/` (generated, gitignored), `bench/runs/` (committed), `evidence/` | nothing outside the repo |
| Native fallback | runs `initdb` as the `postgres` user | initdb refuses to run as root |
| Docker | user in the `docker` group, Compose ≥ 2.20 | also Hasura's requirement |
| GPU | unused | — |
