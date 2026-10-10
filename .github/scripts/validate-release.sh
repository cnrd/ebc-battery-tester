#!/usr/bin/env bash
set -euo pipefail

node .github/scripts/container-tags.mjs validate-release "$GITHUB_REF" "$GITHUB_SHA"
# Checkout must use fetch-depth: 0. Fetch main explicitly even for tag-only clones.
git fetch --no-tags origin '+refs/heads/main:refs/remotes/origin/main'
# Reject stale runs after a tag was moved, peeling annotated tags to commits.
git fetch --no-tags origin "+$GITHUB_REF:$GITHUB_REF"
TAG_SHA=$(git rev-parse "$GITHUB_REF^{commit}")
test "$TAG_SHA" = "$GITHUB_SHA" || {
  echo 'Release tag no longer points to the workflow commit' >&2
  exit 1
}
git merge-base --is-ancestor "$GITHUB_SHA" origin/main || {
  echo 'Release tag must point to a commit contained in main' >&2
  exit 1
}
