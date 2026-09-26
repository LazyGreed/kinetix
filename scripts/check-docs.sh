#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

python3 scripts/render-doc-pages.py --check
python3 scripts/render-protocol-v1-compat.py --check

python3 - <<'PY'
from pathlib import Path
import re
import sys

pattern = re.compile(
    r"\bFR-[0-9]|\bNFR-[0-9]|\bpost-v1\b|documented deviation|"
    r"docs/DESIGN\.md|docs/KINETIX-PLUGIN-ARCHITECTURE\.md",
    re.IGNORECASE,
)
failures = []
for path in sorted(Path("docs/wiki").glob("*.md")):
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        if pattern.search(line):
            failures.append(f"{path}:{number}: historical design reference: {line}")

if failures:
    print("Historical design references found in live wiki pages:", file=sys.stderr)
    print("\n".join(failures), file=sys.stderr)
    sys.exit(1)

print("No historical design references found in live wiki pages")
PY
