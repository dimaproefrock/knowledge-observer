#!/usr/bin/env bash
# Release helper for knowledge-observer.
#
# Bumps the version in LOCKSTEP across the three places that must agree, then
# commits, tags `vX.Y.Z` and pushes — which triggers the release CI that builds
# the per-OS binaries the launcher downloads:
#
#   .claude-plugin/plugin.json  "version"   → what Claude Code delivers as an update
#   VERSION                      vX.Y.Z      → which release the launcher fetches the binary from
#   Cargo.toml                   version     → the crate/binary version
#
# Keeping them coupled means one bump = a coherent update of BOTH the plugin
# content AND the downloaded binary (no drift).
#
# Usage:  ./release.sh 0.1.1
set -euo pipefail

ver="${1:-}"
case "$ver" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "usage: ./release.sh X.Y.Z   (semver, without a leading 'v')" >&2; exit 1 ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
cd "$here"

# Refuse to release a dirty tree or an existing tag.
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "release.sh: working tree has uncommitted changes — commit or stash first." >&2
  exit 1
fi
if git rev-parse "v$ver" >/dev/null 2>&1; then
  echo "release.sh: tag v$ver already exists." >&2
  exit 1
fi

echo "==> bumping to $ver"
# plugin.json: the top-level "version": "..."
sed -i -E 's/("version"[[:space:]]*:[[:space:]]*")[^"]*(")/\1'"$ver"'\2/' .claude-plugin/plugin.json
# Cargo.toml: the [package] version (the first standalone `version = "..."` line)
sed -i -E '0,/^version = ".*"/s//version = "'"$ver"'"/' Cargo.toml
# VERSION: the release tag the launcher downloads from (with the leading v)
printf 'v%s\n' "$ver" > VERSION

echo "==> sanity: build + test"
cargo build --release --bin observer >/dev/null
cargo test >/dev/null

echo "==> commit + tag + push"
git add -A
git commit -q -m "release v$ver"
git tag -a "v$ver" -m "knowledge-observer v$ver"
git push origin HEAD
git push origin "v$ver"

cat <<EOF

Released v$ver. The release workflow is now building the per-OS binaries.
Once it is green, users move to v$ver via:

  /plugin update knowledge-observer@knowledge-observer-marketplace
  /reload-plugins

(or automatically, if they enabled auto-update for the marketplace.)
EOF
