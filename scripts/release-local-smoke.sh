#!/usr/bin/env bash
# Regression tests for scripts/release-local.sh without real Rust/Node/cross builds.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

REPO="$WORK/repo"
FAKE_BIN="$WORK/bin"
mkdir -p "$REPO/scripts" "$REPO/dashboard" "$FAKE_BIN"
cp "$ROOT/scripts/release-local.sh" "$REPO/scripts/release-local.sh"
chmod +x "$REPO/scripts/release-local.sh"

cat > "$FAKE_BIN/npm" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -eq 1 ] && [ "$1" = "ci" ]; then
  exit 0
fi
if [ "$#" -eq 2 ] && [ "$1" = "run" ] && [ "$2" = "build" ]; then
  mkdir -p dist
  printf 'dashboard\n' > dist/index.html
  exit 0
fi
exit 2
SH

cat > "$FAKE_BIN/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
target=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$target" ] || exit 2
mkdir -p "target/$target/release"
marker="$(cat SOURCE_MARKER)"
cat > "target/$target/release/kinetix" <<EOF_BIN
#!/usr/bin/env sh
printf '%s\n' '$marker'
EOF_BIN
chmod +x "target/$target/release/kinetix"
SH

cat > "$FAKE_BIN/cross" <<'SH'
#!/usr/bin/env bash
exec "$(dirname "$0")/cargo" "$@"
SH

cat > "$FAKE_BIN/rustup" <<'SH'
#!/usr/bin/env bash
exit 0
SH

chmod +x "$FAKE_BIN/npm" "$FAKE_BIN/cargo" "$FAKE_BIN/cross" "$FAKE_BIN/rustup"

cat > "$REPO/Cargo.toml" <<'EOF_CARGO'
[package]
name = "kinetix"
version = "1.2.3"
edition = "2021"
EOF_CARGO
printf 'A\n' > "$REPO/SOURCE_MARKER"
printf '{}\n' > "$REPO/dashboard/package-lock.json"
cat > "$REPO/.gitignore" <<'EOF_IGNORE'
target/
dashboard/dist/
EOF_IGNORE

(
  cd "$REPO"
  git init -q
  git config user.name "Kinetix release test"
  git config user.email "release-test@example.invalid"
  git add .
  git commit -qm "source A"
  git tag -a v1.2.3 -m "v1.2.3"

  sed -i 's/version = "1.2.3"/version = "1.2.4"/' Cargo.toml
  printf 'B\n' > SOURCE_MARKER
  git add Cargo.toml SOURCE_MARKER
  git commit -qm "source B"
)

# Existing tags must build the tagged commit, not current HEAD.
(
  cd "$REPO"
  PATH="$FAKE_BIN:$PATH" scripts/release-local.sh v1.2.3 >/dev/null
)
mkdir -p "$WORK/extract"
tar -xzf "$REPO/target/dist/kinetix-v1.2.3-x86_64-unknown-linux-gnu.tar.gz" -C "$WORK/extract"
[ "$("$WORK/extract/kinetix")" = "A" ] || {
  echo "release script built HEAD instead of the existing tag" >&2
  exit 1
}

# A new tag must match the package version at HEAD.
if (
  cd "$REPO"
  PATH="$FAKE_BIN:$PATH" scripts/release-local.sh v1.2.5
) >"$WORK/version-mismatch.log" 2>&1; then
  echo "release script accepted a tag that does not match Cargo.toml" >&2
  exit 1
fi
grep -q 'does not match Cargo package version 1.2.4' "$WORK/version-mismatch.log"

# New tags must never silently include uncommitted source changes.
printf 'dirty\n' >> "$REPO/SOURCE_MARKER"
if (
  cd "$REPO"
  PATH="$FAKE_BIN:$PATH" scripts/release-local.sh v1.2.4
) >"$WORK/dirty.log" 2>&1; then
  echo "release script accepted a dirty working tree for a new tag" >&2
  exit 1
fi
grep -q 'working tree is not clean' "$WORK/dirty.log"
(
  cd "$REPO"
  git checkout -- SOURCE_MARKER
)

# Release tags are deliberately limited to stable vX.Y.Z tags.
if (
  cd "$REPO"
  PATH="$FAKE_BIN:$PATH" scripts/release-local.sh v1.2.4-rc1
) >"$WORK/tag-format.log" 2>&1; then
  echo "release script accepted a non-vX.Y.Z tag" >&2
  exit 1
fi
grep -q 'must match vX.Y.Z exactly' "$WORK/tag-format.log"

echo "release-local regression tests passed"
