#!/usr/bin/env bash
# Print the tool versions this machine has and compare with the verified ones.
# Exit 0 if polydb can be built and tested here, 1 if something required is missing.
set -u
VERIFIED_RUST="1.97.0"
status=0

have() { command -v "$1" >/dev/null 2>&1; }

echo "== required =="
if have cargo && have rustc; then
  rust_version="$(rustc --version | awk '{print $2}')"
  echo "rustc   $rust_version"
  echo "cargo   $(cargo --version | awk '{print $2}')"
  if [ "$rust_version" != "$VERIFIED_RUST" ]; then
    echo "  note: verified with Rust $VERIFIED_RUST; other recent stable versions are expected to work but were not tested"
  fi
else
  echo "MISSING rustc/cargo: install Rust from https://rustup.rs"
  status=1
fi

echo "== optional: driver check (make drivers) =="
if have python3; then
  echo "python3 $(python3 --version 2>&1 | awk '{print $2}')"
  python3 -c 'import venv, ensurepip' 2>/dev/null || echo "  note: python3 venv module missing (Debian/Ubuntu: apt install python3-venv)"
else
  echo "python3 not found: the driver check will be skipped"
fi

echo "== optional: command-line clients =="
for tool in redis-cli psql mongosh docker; do
  if have "$tool"; then echo "$tool: $($tool --version 2>&1 | head -1)"; else echo "$tool: not installed (not needed)"; fi
done
if have nc; then echo "nc: installed"; else echo "nc: not installed (not needed)"; fi

echo "== machine =="
echo "os      $(uname -srm)"
[ -r /etc/os-release ] && . /etc/os-release && echo "distro  ${PRETTY_NAME:-unknown}"
if have nproc; then echo "cpus    $(nproc)"; fi
if have free; then echo "memory  $(free -h | awk '/^Mem:/ {print $2}')"; fi

exit $status
