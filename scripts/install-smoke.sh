#!/usr/bin/env bash
# Exercise install.sh's source-build fallback without real Rust or Node builds.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

SOURCE="$WORK/source"
FAKE_BIN="$WORK/bin"
HOME_DIR="$WORK/home"
PREFIX="$WORK/prefix"
LOG="$WORK/order.log"

mkdir -p "$SOURCE/dashboard" "$FAKE_BIN" "$HOME_DIR"
printf '{}\n' > "$SOURCE/dashboard/package-lock.json"

cat > "$FAKE_BIN/node" <<'SH'
#!/usr/bin/env bash
exit 0
SH

cat > "$FAKE_BIN/npm" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -eq 1 ] && [ "$1" = "ci" ]; then
  printf 'npm ci\n' >> "$INSTALL_TEST_LOG"
  exit 0
fi
if [ "$#" -eq 2 ] && [ "$1" = "run" ] && [ "$2" = "build" ]; then
  printf 'npm run build\n' >> "$INSTALL_TEST_LOG"
  mkdir -p dist
  printf '<!doctype html>\n' > dist/index.html
  exit 0
fi
exit 2
SH

cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo %s\n' "$*" >> "$INSTALL_TEST_LOG"
[ -f dashboard/dist/index.html ] || {
  echo "cargo ran before dashboard/dist/index.html existed" >&2
  exit 42
}
mkdir -p target/release
cat > target/release/kinetix <<'EOF_BIN'
#!/usr/bin/env bash
exit 0
EOF_BIN
chmod +x target/release/kinetix
SH

chmod +x "$FAKE_BIN/node" "$FAKE_BIN/npm" "$FAKE_BIN/cargo"

(
  cd "$SOURCE"
  git init -q -b main
  git config user.name "Kinetix installer test"
  git config user.email "installer-test@example.invalid"
  git add .
  git commit -qm "fixture"
)

INSTALL_TEST_LOG="$LOG" \
HOME="$HOME_DIR" \
PATH="$FAKE_BIN:$PATH" \
KINETIX_REPO="$SOURCE" \
KINETIX_VERSION=main \
KINETIX_PREFIX="$PREFIX" \
  "$ROOT/install.sh" >/dev/null

[ -x "$PREFIX/bin/kinetix" ] || {
  echo "installer did not install an executable binary" >&2
  exit 1
}

expected="$(cat <<'EOF_EXPECTED'
npm ci
npm run build
cargo build --release --locked
EOF_EXPECTED
)"
actual="$(head -n 3 "$LOG")"
[ "$actual" = "$expected" ] || {
  echo "unexpected source-build order:" >&2
  cat "$LOG" >&2
  exit 1
}

echo "installer source-build smoke passed"
