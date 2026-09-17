#!/usr/bin/env bash
# Collect exactly the SEED Sync release asset set (docs/ci-release.md §3) from
# downloaded build.yml workflow artifacts into dist/, or fail naming the
# missing one. Never publishes a release short an asset for a platform whose
# job did not run or did not produce what it should have.
#
# Usage: scripts/ci/collect-assets.sh <version> [artifacts-dir] [dist-dir]
set -euo pipefail

version="${1:?usage: collect-assets.sh <version> [artifacts-dir] [dist-dir]}"
artifacts_dir="${2:-artifacts}"
dist_dir="${3:-dist}"

mkdir -p "$dist_dir"

names=(
  "seed-sync-$version-windows-x86_64.msi"
  "seed-sync-$version-windows-arm64.msi"
  "seed-sync-$version-linux-x86_64.tar.gz"
  "seed-sync-$version-macos-universal.tar.gz"
  "seed-sync-$version-android-universal.apk"
  "seed-sync_$version-1_amd64.deb"
  "seed-sync-$version-1.x86_64.rpm"
  "seed-sync-$version-1-x86_64.pkg.tar.zst"
  "io.github.steeb_k.SeedSync-$version-x86_64.flatpak"
)

for name in "${names[@]}"; do
  found="$(find "$artifacts_dir" -type f -name "$name" | head -1)"
  if [ -z "$found" ]; then
    echo "::error::missing asset $name — a platform job did not produce it" >&2
    exit 1
  fi
  cp "$found" "$dist_dir/"
done

ls -la "$dist_dir"
