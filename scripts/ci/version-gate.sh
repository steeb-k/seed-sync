#!/usr/bin/env bash
# Version gate for a SEED Sync release: the tag being published must agree
# exactly with Cargo.toml's workspace version and with Android's versionName /
# versionCode, or nothing gets published. See docs/ci-release.md §4.
#
# Usage: scripts/ci/version-gate.sh <tag> [cargo-toml] [gradle-kts]
#   tag          e.g. v0.8.0 (Latest) or v0.8.0-test1 (prerelease/rehearsal)
#   cargo-toml   defaults to Cargo.toml
#   gradle-kts   defaults to android/app/build.gradle.kts
#
# Prints `tag=`, `version=`, `prerelease=` lines on success, meant to be
# appended straight to "$GITHUB_OUTPUT":
#   scripts/ci/version-gate.sh "$tag" >> "$GITHUB_OUTPUT"
#
# Exits non-zero with an ::error:: line (and prints nothing) on any mismatch.
set -euo pipefail

tag="${1:?usage: version-gate.sh <tag> [cargo-toml] [gradle-kts]}"
cargo_toml="${2:-Cargo.toml}"
gradle_kts="${3:-android/app/build.gradle.kts}"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$cargo_toml" | head -1)"
if [ -z "$version" ]; then
  echo "::error::could not read version from $cargo_toml" >&2
  exit 1
fi

# vX.Y.Z or vX.Y.Z-test1 (rehearsal) — only the part before the first "-" is
# compared to Cargo.toml.
base="${tag#v}"
base="${base%%-*}"
if [ "$base" != "$version" ]; then
  echo "::error::tag $tag does not match $cargo_toml version $version" >&2
  exit 1
fi

vname="$(sed -n 's/^[[:space:]]*versionName = "\(.*\)"/\1/p' "$gradle_kts")"
vcode="$(sed -n 's/^[[:space:]]*versionCode = \([0-9]*\)/\1/p' "$gradle_kts")"

IFS=. read -r maj min pat <<EOF
$version
EOF
want=$((maj * 10000 + min * 100 + pat))

if [ "$vname" != "$version" ]; then
  echo "::error::android versionName $vname != $version" >&2
  exit 1
fi
if [ "$vcode" != "$want" ]; then
  echo "::error::android versionCode $vcode != $want (MAJOR*10000+MINOR*100+PATCH)" >&2
  exit 1
fi

case "$tag" in
  *-*) prerelease=true ;;
  *) prerelease=false ;;
esac

echo "tag=$tag"
echo "version=$version"
echo "prerelease=$prerelease"
