#!/usr/bin/env bash
set -euo pipefail

release_tag="${1:?release tag is required}"

case "$release_tag" in
  onwards-v*) ;;
  *)
    echo "Release tag '$release_tag' is not an Onwards tag; skipping publish."
    exit 0
    ;;
esac

manifest_version="$({
  sed -n 's/^version = "\([^"]*\)".*/\1/p' onwards/Cargo.toml
} | head -n 1)"
tag_version="${release_tag#onwards-v}"

if [[ "$manifest_version" != "$tag_version" ]]; then
  echo "Release tag '$release_tag' points at Onwards ${tag_version}, but onwards/Cargo.toml contains ${manifest_version}." >&2
  exit 1
fi

if ! crate_status="$(curl --silent --show-error \
  --output /dev/null \
  --write-out '%{http_code}' \
  --user-agent "control-layer-release-script (https://github.com/doublewordai/control-layer)" \
  "https://crates.io/api/v1/crates/onwards/${manifest_version}")"; then
  echo "Failed to determine whether onwards ${manifest_version} is already published." >&2
  exit 1
fi

case "$crate_status" in
  200)
    echo "onwards ${manifest_version} is already published; skipping."
    exit 0
    ;;
  404) ;;
  *)
    echo "Unexpected crates.io response while checking onwards ${manifest_version}: HTTP ${crate_status}." >&2
    exit 1
    ;;
esac

cargo publish --locked --package onwards --all-features --registry crates-io \
  --token "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is required}"
