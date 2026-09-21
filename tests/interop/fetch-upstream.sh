#!/bin/sh
# Clones the pinned upstream DryDB commit next to this script.
#
# The interop tests compare against exactly this commit. CI must never take
# upstream from a moving branch: a format change would silently become the new
# expectation instead of failing the build.
set -eu

COMMIT=6b175929491793948e63430c20c2d6f58300d97f
REPO=https://github.com/hadashiA/DryDB.git
DIR="$(cd "$(dirname "$0")" && pwd)"
TARGET="$DIR/upstream"

if [ -d "$TARGET/.git" ]; then
    git -C "$TARGET" fetch --quiet origin || true
else
    rm -rf "$TARGET"
    git clone --quiet "$REPO" "$TARGET"
fi
git -C "$TARGET" checkout --quiet "$COMMIT"
echo "upstream DryDB at $(git -C "$TARGET" rev-parse HEAD)"
