# One entry point per pipeline stage. Every target is safe to rerun.
# SEED and SCALE can be overridden: make data SEED=2 SCALE=1
SEED  ?= 1
SCALE ?= 0.01
PY    := uv run python
OUT   := bench/data/out/seed$(SEED)_s$(SCALE)

.PHONY: help sync up up-native down down-native wipe-native lock-digests data validate load \
        equiv test test-unit test-db oracle pilot env-snapshot reproduce clean-data

help:
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | sed 's/:.*##/\t/'

sync: ## install the locked environment
	uv sync --all-extras --frozen

up: ## start the three DB containers (project laptop)
	docker compose up -d --wait
	docker compose ps

down:
	docker compose down -v

up-native: ## no-Docker fallback (sandbox only, see scripts/native_up.sh)
	scripts/native_up.sh start

down-native:
	scripts/native_up.sh stop

wipe-native:
	scripts/native_up.sh wipe

lock-digests: ## record pulled image digests into compose.yaml and docs/ENVIRONMENT.md
	$(PY) scripts/lock_digests.py

data: ## T-D1: generate twice with the same seed and require identical checksums
	$(PY) -m bench.data.gen --seed $(SEED) --scale $(SCALE) --out $(OUT)
	$(PY) -m bench.data.gen --seed $(SEED) --scale $(SCALE) --out $(OUT).again
	$(PY) -m bench.data.determinism $(OUT) $(OUT).again
	rm -rf $(OUT).again

validate: ## HS2 §11.1 schema, referential and distribution checks
	$(PY) -m bench.data.validate $(OUT)

load: validate ## load both placements
	$(PY) -m bench.data.load $(OUT)

equiv: ## T-D2: federated vs consolidated counts and checksums
	$(PY) -m bench.data.equivalence $(OUT)

test-unit: ## no databases needed
	uv run pytest tests/unit --cov --cov-report=term-missing:skip-covered

test-db: ## needs `make up load`
	uv run pytest tests/db

test: test-unit test-db

oracle: ## T-O1: whole workload, engine (A1 and A4) vs reference SQL
	$(PY) -m oracle.run --out $(OUT)

pilot: ## HS3 pilot: 3 sweep points, all arms, writes bench/runs/<id>/
	$(PY) -m bench.run --points 10,100,1000 --warmup 3 --reps 7 --data $(OUT)

env-snapshot: ## print the environment fingerprint that goes into every manifest
	$(PY) -m bench.manifest

reproduce: data load equiv test oracle pilot ## full chain from a clean database

clean-data:
	rm -rf bench/data/out
