# polydb task runner. Every target is a thin wrapper; the real commands are
# shown in README.md so nothing here is required to use the project.
.PHONY: help check-env build test smoke drivers lint serve clean-run docker clean

help:
	@echo "make check-env   show tool versions and what is missing"
	@echo "make build       cargo build --release --locked"
	@echo "make test        Rust tests: 22 integration + 23 hardening + 1 doc test"
	@echo "make smoke       scripted run compared with tests/fixtures/smoke.expected"
	@echo "make drivers     optional: 39 checks through redis-py, psycopg2, pymongo"
	@echo "make lint        clippy and rustfmt report (advisory, does not fail)"
	@echo "make serve       start the server from config/polydb.env"
	@echo "make clean-run   full reproducibility pass in a fresh copy, with a log"
	@echo "make docker      build the container image (runs the tests during build)"
	@echo "make clean       remove target/, .venv/ and local data directories"

check-env:
	@bash scripts/check_env.sh

build:
	cargo build --release --locked

test:
	cargo test --release --locked

smoke:
	@bash scripts/smoke.sh

drivers:
	@bash scripts/driver_checks.sh

lint:
	-cargo clippy --all-targets --locked
	-cargo fmt --check >/dev/null && echo "rustfmt: clean" || echo "rustfmt: formatting differences (advisory)"

serve:
	@bash scripts/serve.sh

clean-run:
	@bash scripts/clean_run.sh

docker:
	docker build -t polydb:0.2.0 .

clean:
	rm -rf target .venv polydb-data
