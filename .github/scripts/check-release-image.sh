#!/usr/bin/env bash
# Fail closed if an existing exact release image belongs to another commit.
# Requires registry login and Buildx. Run after the SHA image has created/linked
# the package, but before assigning version aliases. Never delete registry content.
set -euo pipefail
IMAGE=${1:?Expected canonical image name}
VERSION=${2:?Expected exact release version}
SHA=${3:?Expected full Git SHA}

ERROR=$(mktemp "${RUNNER_TEMP:-${TMPDIR:-.}}/ebc-image-check.XXXXXX")
trap 'rm -f "$ERROR"' EXIT
if CONFIGS=$(docker buildx imagetools inspect "$IMAGE:$VERSION" --format '{{json .Image}}' 2>"$ERROR"); then
  REVISIONS=$(printf '%s' "$CONFIGS" | jq -c '[.. | objects | select(has("config") or has("Config")) | (.config // .Config) | (.Labels // .labels // {})["org.opencontainers.image.revision"] // "missing"] | unique')
  if [ "$REVISIONS" != "[\"$SHA\"]" ]; then
    echo "Refusing to overwrite $IMAGE:$VERSION: existing revisions $REVISIONS, expected $SHA" >&2
    exit 1
  fi
else
  # Match the entire response, not substrings from credential-helper, auth or
  # network failures. Only a proven missing manifest may bypass this guard.
  case "$(cat "$ERROR")" in
    "ERROR: $IMAGE:$VERSION: not found"|"ERROR: $IMAGE:$VERSION: manifest unknown"|\
    'ERROR: manifest unknown'|'ERROR: manifest unknown: manifest unknown') ;;
    *) cat "$ERROR" >&2; exit 1 ;;
  esac
fi
