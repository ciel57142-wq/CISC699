# Environment

Everything polydb needs from the machine it runs on, and exactly what the reproducibility pass used.

## Runtime versions

| Component | Verified version | Needed for | Pinned by |
|---|---|---|---|
| Rust (`rustc`, `cargo`) | 1.97.0 | build and tests | `Dockerfile` (`FROM rust:1.97`), CI workflow; printed and compared by `scripts/check_env.sh` |
| Rust edition | 2021 | build | `Cargo.toml` |
| Third-party Rust crates | none | | `Cargo.lock` (contains only `polydb` itself); builds use `--locked` |
| C linker (`cc`) | GCC 13.3 | linking | system package |
| Python | 3.13.18 | driver check only | not pinned; only 3.13 was verified |
| redis (redis-py) | 8.1.0 | driver check only | `tests/requirements-drivers.txt` |
| psycopg2-binary | 2.9.13 | driver check only | `tests/requirements-drivers.txt` |
| pymongo | 4.19.0 | driver check only | `tests/requirements-drivers.txt` |
| bash | 5.2 | helper scripts | not pinned |
| GNU make | 4.3 | optional shortcuts | not pinned |

**Minimum Rust version.** `Cargo.lock` is in lock-file format 4, which Cargo reads from Rust 1.78 onward, so 1.78 is the practical floor (the code itself uses `std::io::IsTerminal`, which needs 1.70). No version other than 1.97.0 has been tested, because the verification environment could not download other toolchains. For the same reason there is no `rust-toolchain.toml`: a pin to an exact version makes `cargo` try to download that toolchain, which fails on an offline machine that already has a working compiler. The Dockerfile and the CI workflow pin 1.97.0 instead.

**Transitive Python packages.** `pymongo` pulls in `dnspython` (2.9.2 in the clean run); pip resolves its version. The driver check does not depend on its behaviour (the connection string uses an IP address).

## Verification machine

The clean run recorded in `docs/evidence/` used:

| Property | Value |
|---|---|
| Operating system | Ubuntu 24.04.5 LTS, Linux kernel 6.18, x86-64 |
| CPU cores | 2 |
| Memory | 7.8 GB |
| Disk | local filesystem that supports `fsync` |
| Network | PyPI reachable (driver check only); nothing else used |
| Docker | client installed, no daemon available: container build not verified |

## Hardware assumptions

- **CPU:** any 64-bit CPU. Two cores are enough; the concurrency tests start up to 8 client threads but do not need 8 cores.
- **Memory:** the build peaks well under 1 GB. At run time the whole dataset lives in memory, so plan for the size of the data plus overhead.
- **Disk:** about 200 MB for `target/` after building and testing, and about 40 MB for `.venv/` if the driver check is run. The data directory needs a filesystem that honours `fsync`; on network filesystems or some container volume drivers the durability guarantee may not hold.
- **GPU:** not used.
- **Ports:** 7878, 6380, 5433 and 27018 free on localhost for the default server and the driver check. The Rust tests use ports assigned by the operating system and need no fixed port.
- **Timing:** on the verification machine the build took 9 to 17 seconds, the Rust tests under 1 second after compiling, and the driver check about 5 seconds.

## Platforms

| Platform | Status |
|---|---|
| Linux x86-64 | verified |
| macOS, Linux arm64 | expected to work (standard library only); not verified |
| Windows | the engine is expected to build; the helper scripts need bash (use WSL). `acknowledged_writes_survive_killing_the_server` kills a child process, which behaves differently on Windows. Not verified |

## Configuration

polydb reads no environment variables. All configuration is by command-line flag; the table is in the [runbook](RUNBOOK.md#configuration-map), and a template for the helper script is in `config/polydb.env.example`. There are no secrets to configure.

Environment variables that affect the build tools, not polydb:

| Variable | Used by | Purpose |
|---|---|---|
| `CARGO_TARGET_DIR` | cargo | where build output goes; `scripts/clean_run.sh` sets it inside the fresh copy |
| `PIP_INDEX_URL`, `PIP_CERT`, `HTTPS_PROXY` | pip | only if PyPI must be reached through a mirror or proxy |

## Container

`Dockerfile` builds from `rust:1.97`, runs the Rust tests and the smoke comparison during the build, and starts the server on `/data` as an unprivileged user. It uses one stage, so the image is large (the full Rust toolchain is included); that was chosen over a smaller two-stage image because it cannot fail from a C library mismatch between stages and it has not been possible to test either variant. See [REPRODUCIBILITY.md](REPRODUCIBILITY.md#what-was-not-verified).
