# polydb container image.
#
#   docker build -t polydb:0.2.0 .          # compiles and runs the test suite
#   docker run --rm -p 7878:7878 -p 6380:6380 -p 5433:5433 -p 27018:27018 \
#              -v polydb-data:/data polydb:0.2.0
#
# One stage on purpose: the build image and the runtime image share the same
# C library, so the binary cannot fail to start because of a library mismatch.
# The price is a large image; see docs/ENVIRONMENT.md.
FROM rust:1.97

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests

# Build, then prove the image works before it is tagged: the Rust tests and
# the golden-output smoke run both execute at build time.
RUN cargo build --release --locked \
 && cargo test --release --locked \
 && ./target/release/polydb --memory < tests/fixtures/smoke.commands | diff - tests/fixtures/smoke.expected \
 && install -m 0755 target/release/polydb /usr/local/bin/polydb

# Run as an unprivileged user; /data holds data.wal and data.snap.
RUN useradd --system --home /data --create-home polydb
USER polydb
VOLUME /data
EXPOSE 7878 6380 5433 27018

# 0.0.0.0 is required inside a container so published ports are reachable.
# polydb has no authentication: publish these ports only to a trusted network.
ENTRYPOINT ["polydb"]
CMD ["serve", "/data", "--host", "0.0.0.0"]
