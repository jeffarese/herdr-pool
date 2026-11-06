#!/bin/sh
# Releases herdr-pool to GitHub and to Homebrew (jeffarese/tap):
#
#   sh scripts/release.sh 0.2.0
#
# From a clean main that matches origin, it sets the version (committing and
# pushing the bump if it changed), tests, tags vX.Y.Z and publishes the GitHub
# release. It then opens the formula update on the tap, waits for the tap's CI
# to build the bottles, and runs the tap's publish workflow, which uploads them
# and merges the update. From then on `brew upgrade herdr-pool` installs it.
# Meanwhile the tag's Release workflow adds static binaries for macOS and
# Linux (WSL included) to the GitHub release, for scripts/install.sh.
set -eu
die() { echo "release: $*" >&2; exit 1; }

version=${1:-}
echo "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || die "usage: sh scripts/release.sh X.Y.Z"
tag=v$version
repo=jeffarese/herdr-pool
tap=jeffarese/homebrew-tap
formula=jeffarese/tap/herdr-pool

cd "$(dirname "$0")/.."
for tool in git gh cargo brew perl; do
  command -v "$tool" >/dev/null || die "needs $tool"
done
[ "$(git branch --show-current)" = main ] || die "switch to main first"
[ -z "$(git status --porcelain)" ] || die "commit or stash your changes first"
git fetch -q origin main --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] || die "main differs from origin/main"
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then die "$tag exists already"; fi

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)
if [ "$current" != "$version" ]; then
  echo "release: $current -> $version"
  perl -pi -e 'if (!$done && s/^version = "[^"]*"$/version = "'"$version"'"/) { $done = 1 }' Cargo.toml
  cargo update --workspace --offline -q
fi
if ! cargo test --locked -q; then
  git checkout -q -- Cargo.toml Cargo.lock
  die "tests fail; nothing was released"
fi
if [ -n "$(git status --porcelain)" ]; then
  git commit -q -m "Release $version" -- Cargo.toml Cargo.lock
  git push -q origin main
fi

git tag -a "$tag" -m "herdr-pool $version"
git push -q origin "$tag"
# The Release workflow creates it if it gets there first.
gh release view "$tag" -R "$repo" >/dev/null 2>&1 ||
  gh release create "$tag" -R "$repo" --verify-tag --title "herdr-pool $version" --generate-notes

# The formula update. bump-formula-pr works in the local clone of the tap and
# pushes its branch there (--no-fork), so the PR runs the tap's CI.
brew tap jeffarese/tap >/dev/null
brew trust --formula "$formula" >/dev/null
git -C "$(brew --repository jeffarese/tap)" pull -q --ff-only
HOMEBREW_GITHUB_API_TOKEN=${HOMEBREW_GITHUB_API_TOKEN:-$(gh auth token)}
export HOMEBREW_GITHUB_API_TOKEN
brew bump-formula-pr --no-browse --no-fork --strict \
  --url="https://github.com/$repo/archive/refs/tags/$tag.tar.gz" "$formula"

pr=$(gh pr list -R "$tap" --state open --json number,title \
  -q ".[] | select(.title == \"herdr-pool $version\") | .number" | head -n 1)
[ -n "$pr" ] || die "can't find the formula PR for $version on $tap"
publish="gh workflow run publish.yml -R $tap -f pull_request=$pr"

echo "release: waiting for the tap's CI to build the bottles: https://github.com/$tap/pull/$pr"
waited=0
until [ "$(gh pr view "$pr" -R "$tap" --json statusCheckRollup -q '.statusCheckRollup | length')" -gt 0 ]; do
  [ "$waited" -lt 300 ] || die "no CI started on PR #$pr; once it passes, run: $publish"
  sleep 10
  waited=$((waited + 10))
done
gh pr checks "$pr" -R "$tap" --watch --interval 30 >/dev/null ||
  die "the tap's CI failed on PR #$pr; fix it, then run: $publish"

$publish -f head_sha="$(gh pr view "$pr" -R "$tap" --json headRefOid -q .headRefOid)"
echo "release: herdr-pool $version is out; the publish workflow uploads the bottles and merges PR #$pr"
echo "release: https://github.com/$tap/actions/workflows/publish.yml"
