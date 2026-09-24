#!/usr/bin/env bash
# Bump nova-veil-search version across Cargo.toml and Cargo.lock.
#
# Usage:
#   scripts/bump-version.sh 0.1.5            # bump only
#   scripts/bump-version.sh 0.1.5 --tag      # also create commit + tag
#   scripts/bump-version.sh 0.1.5 --push     # commit + tag + push (triggers CI release)
#
# Pre-flight: write CHANGELOG.md entry for the new version first.

set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <version> [--tag|--push]" >&2
  exit 64
fi

VERSION="$1"
shift || true
MODE="bump"
if [[ $# -gt 0 ]]; then
  case "$1" in
    --tag)  MODE="tag" ;;
    --push) MODE="push" ;;
    *) echo "unknown flag: $1" >&2; exit 64 ;;
  esac
fi

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "version '$VERSION' is not valid semver (X.Y.Z[-pre])" >&2
  exit 65
fi

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

if [[ "$MODE" != "bump" ]]; then
  if [[ -n "$(git status --porcelain)" ]]; then
    echo "working tree is dirty; commit or stash first" >&2
    exit 66
  fi
  if git rev-parse "refs/tags/v$VERSION" >/dev/null 2>&1; then
    echo "tag v$VERSION already exists" >&2
    exit 67
  fi
fi

echo "==> Updating Cargo.toml -> $VERSION"
python3 - "$VERSION" <<'PY'
import pathlib, re, sys
v = sys.argv[1]
p = pathlib.Path("Cargo.toml")
text = p.read_text()
new_text, n = re.subn(r'(?m)^version\s*=\s*"[^"]+"', f'version = "{v}"', text, count=1)
if n != 1:
    raise SystemExit('failed to find `version = "..."` in Cargo.toml')
p.write_text(new_text)
PY

echo "==> Refreshing Cargo.lock"
cargo update -p nova-veil-search

if ! grep -qE "^## ${VERSION}( |$)" CHANGELOG.md; then
  echo "WARNING: CHANGELOG.md has no section for ${VERSION}. Add it before publishing." >&2
fi

echo "==> Files updated. Review with: git --no-pager diff --stat"

if [[ "$MODE" == "bump" ]]; then
  exit 0
fi

echo "==> Committing"
git add Cargo.toml Cargo.lock
git commit -m "Release nova-veil-search $VERSION"
git tag -a "v$VERSION" -m "v$VERSION"

if [[ "$MODE" == "push" ]]; then
  echo "==> Pushing commit and tag"
  CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
  git push origin "$CURRENT_BRANCH"
  git push origin "v$VERSION"
  echo "Tag v$VERSION pushed. CI will build and publish."
else
  echo "Commit and tag created locally. Push with:"
  echo "  git push origin \$(git rev-parse --abbrev-ref HEAD) && git push origin v$VERSION"
fi
