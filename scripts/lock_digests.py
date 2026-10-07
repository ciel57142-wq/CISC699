"""Pin compose images by digest (HS2 §8.1). Run once on the project laptop after `make up`.

Reads the RepoDigest of each running service's image, rewrites the matching `image:` line in
compose.yaml to `name@sha256:...`, and appends the digests to docs/ENVIRONMENT.md.
Never run this against `latest` tags.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
COMPOSE = REPO / "compose.yaml"
ENV_DOC = REPO / "docs" / "ENVIRONMENT.md"


def sh(*cmd: str) -> str:
    return subprocess.run(cmd, capture_output=True, text=True, check=True, cwd=REPO).stdout.strip()


def main() -> int:
    services = json.loads(sh("docker", "compose", "config", "--format", "json"))["services"]
    pins = {}
    for name, svc in services.items():
        image = svc["image"]
        if "@sha256:" in image:
            pins[name] = image
            continue
        digests = json.loads(sh("docker", "image", "inspect", image, "--format", "{{json .RepoDigests}}"))
        if not digests:
            print(f"{name}: no RepoDigest for {image}; pull it first", file=sys.stderr)
            return 1
        pins[name] = digests[0]
    text = COMPOSE.read_text()
    for name, pinned in pins.items():
        base = pinned.split("@")[0]
        text = re.sub(rf"(\n  {re.escape(name)}:\n    image: )[^\n]+", rf"\g<1>{pinned}", text)
        print(f"{name}: {pinned}  ({base})")
    COMPOSE.write_text(text)
    with ENV_DOC.open("a") as f:
        f.write(f"\n### Image digests locked {time.strftime('%Y-%m-%d')}\n\n")
        for name, pinned in pins.items():
            f.write(f"- `{name}`: `{pinned}`\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
