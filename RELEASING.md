# Releasing nova-veil-search

## The only thing you do

```bash
# 1. (optional) add a section to CHANGELOG.md and push it
$EDITOR CHANGELOG.md
git commit -am "docs: changelog for 0.1.5"
git push

# 2. Tag and push
git tag v0.1.5
git push origin v0.1.5
```

That's all. Pushing the tag triggers `release.yml`, which then:

1. Injects `0.1.5` into `Cargo.toml` (in CI working tree) and builds cross-platform binaries — stdio builds for all five platforms, plus `-http` server builds (`--features http`, `release-http` profile) for Linux x86_64/aarch64
2. Creates the GitHub Release with archives + `SHA256SUMS`
3. Pushes the multi-arch (`amd64`/`arm64`) server image to `ghcr.io/kingsunb/nova-veil-search` tagged `X.Y.Z` + `latest` (built from the `-http` binaries via `Dockerfile.deploy`)
4. **Commits the version bump back to `main`** so `Cargo.toml` and `Cargo.lock` stay in sync with the latest release

## Manual fallback (rarely needed)

If CI is unavailable and you want to bump manifests by hand:

- **Local script**: `scripts/bump-version.sh 0.1.5 --push` (bumps, commits, tags, pushes)
- **GitHub UI**: Actions → Bump Version → Run workflow

Both predate the tag-triggered auto-sync and remain for offline use.

## Where version numbers live

- `Cargo.toml` — auto-synced to `main` by the `sync-main` job
- `Cargo.lock` — refreshed alongside `Cargo.toml`

## Prerequisites

- No branch protection rule on `main` blocking `github-actions[bot]`
- **One-time, after the first tagged release that pushes the image**: set
  `ghcr.io/kingsunb/nova-veil-search` to **public** in the package settings
  (profile → Packages → nova-veil-search → Package settings → Change visibility).
  GHCR creates new packages private by default and offers no API to change
  visibility from CI, so until this flip anonymous `docker pull` returns 401
  even though the workflow succeeded. Later pushes keep the public setting.

## Verify after release

- GitHub release page lists 7 archives (5 stdio + 2 `-http` Linux) + `SHA256SUMS`
- `docker pull ghcr.io/kingsunb/nova-veil-search:X.Y.Z` resolves on both amd64 and arm64 **without logging in** (a 401 means the package is still private — see Prerequisites)
- `main` has a `chore: sync manifests to X.Y.Z` commit from `github-actions[bot]`