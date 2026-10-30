#!/usr/bin/env bash
# Shows what changed in nextcloud-gpodder between the revision this project targets
# (Cargo.toml [package.metadata.upstream].commit) and the latest upstream revision.
#
# Usage: upstream-diff.sh [TARGET_REF]
#   TARGET_REF  upstream ref to compare against (default: origin/HEAD, the default branch).
#               Pass a tag such as 3.19.0 to target a release instead.
#
# The upstream repo is cloned into a cache dir and only read with git; none of its code is run.
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cache_dir="${XDG_CACHE_HOME:-$HOME/.cache}/nc-gpodder-lite/upstream"

metadata() {
    sed -n '/^\[package.metadata.upstream\]/,/^\[/p' "$project_root/Cargo.toml" |
        sed -n "s/^$1 *= *\"\(.*\)\"/\1/p"
}
repo="$(metadata repository)"
current_commit="$(metadata commit)"
current_version="$(metadata version)"
if [[ -z "$repo" || -z "$current_commit" ]]; then
    echo "error: [package.metadata.upstream] repository/commit missing from Cargo.toml" >&2
    exit 1
fi

if [[ -d "$cache_dir/.git" ]]; then
    git -C "$cache_dir" fetch --quiet --tags --force origin
    git -C "$cache_dir" remote set-head origin --auto >/dev/null
else
    mkdir -p "$(dirname "$cache_dir")"
    git clone --quiet --filter=blob:none --no-checkout "$repo" "$cache_dir"
fi
git() { command git -C "$cache_dir" "$@"; }

if ! current_commit="$(git rev-parse --verify --quiet "$current_commit^{commit}")"; then
    echo "error: targeted commit from Cargo.toml not found upstream" >&2
    exit 1
fi
target_ref="${1:-origin/HEAD}"
target_commit="$(git rev-parse "$target_ref^{commit}")"
target_version="$(git show "$target_commit:appinfo/info.xml" | sed -n 's:.*<version>\(.*\)</version>.*:\1:p' | head -1)"

echo "upstream repository: $repo"
echo "upstream clone:      $cache_dir"
echo "currently targeted:  $current_version @ $current_commit"
echo "latest ($target_ref): $target_version @ $target_commit"
echo "latest release tag:  $(git tag --sort=-creatordate | grep -E '^v?[0-9]+\.[0-9]+' | head -1)"
echo

if [[ "$current_commit" == "$target_commit" ]]; then
    echo "UP TO DATE: no upstream changes since the targeted commit."
    exit 0
fi
if ! git merge-base --is-ancestor "$current_commit" "$target_commit"; then
    echo "WARNING: targeted commit is not an ancestor of $target_ref (history rewritten?)."
    echo
fi

echo "=== Commits"
git log --no-merges --format='%h %ad %s' --date=short "$current_commit..$target_commit"
echo
echo "=== Changed files (all)"
git diff --stat=200 "$current_commit" "$target_commit"
echo
# Server-side behavior lives in appinfo/ and lib/; tests document intended behavior.
# The Vue frontend (src/), translations, CI and build files rarely affect the API.
echo "=== Diff of server-side code and tests (appinfo/ lib/ tests/)"
git diff "$current_commit" "$target_commit" -- appinfo lib tests
echo
echo "=== Diff of README.md"
git diff "$current_commit" "$target_commit" -- README.md
