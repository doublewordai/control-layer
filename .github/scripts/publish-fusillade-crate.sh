#!/usr/bin/env bash
set -euo pipefail

release_tag="${1:?release tag is required}"

case "$release_tag" in
  fusillade-core-v*) package="fusillade-core" ;;
  fusillade-arsenal-v*) package="fusillade-arsenal" ;;
  fusillade-v*) package="fusillade" ;;
  *)
    echo "Release tag '$release_tag' is not a Fusillade crate tag; skipping publish."
    exit 0
    ;;
esac

manifest_for_package() {
  case "$1" in
    fusillade) echo "fusillade/Cargo.toml" ;;
    fusillade-core) echo "fusillade-core/Cargo.toml" ;;
    fusillade-arsenal) echo "fusillade-arsenal/Cargo.toml" ;;
    *) echo "unknown package '$1'" >&2; exit 1 ;;
  esac
}

manifest_version() {
  sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

release_manifest_version() {
  local package_path="$1"
  awk -v key="\"${package_path}\":" '
    $1 == key {
      gsub(/[",]/, "", $2)
      print $2
    }
  ' .release-please-manifest.json
}

crate_version_available() {
  local crate="$1"
  local version="$2"
  curl --fail --silent --show-error \
    --user-agent "control-layer-release-script (https://github.com/doublewordai/control-layer)" \
    "https://crates.io/api/v1/crates/${crate}/${version}" \
    >/dev/null
}

wait_for_crate_version() {
  local crate="$1"
  local version="$2"

  if [[ -z "$version" ]]; then
    echo "No dependency version found for ${crate}; refusing to publish dependent crate." >&2
    exit 1
  fi

  for attempt in $(seq 1 30); do
    if crate_version_available "$crate" "$version"; then
      echo "${crate} ${version} is available on crates.io."
      return 0
    fi

    echo "Waiting for ${crate} ${version} to appear on crates.io (${attempt}/30)..."
    sleep 10
  done

  echo "${crate} ${version} did not appear on crates.io in time." >&2
  exit 1
}

publish_package() {
  local package="$1"
  local manifest
  local tag_version
  local version

  manifest="$(manifest_for_package "$package")"
  version="$(manifest_version "$manifest")"
  tag_version="${release_tag##*-v}"

  if [[ "$version" != "$tag_version" ]]; then
    echo "Release tag '$release_tag' points at ${package} ${tag_version}, but ${manifest} contains ${version}." >&2
    exit 1
  fi

  if crate_version_available "$package" "$version"; then
    echo "${package} ${version} is already published; skipping."
    return 0
  fi

  cargo publish --package "$package" --token "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN is required}"
}

case "$package" in
  fusillade-core)
    publish_package "$package"
    ;;
  fusillade-arsenal)
    wait_for_crate_version fusillade-core "$(release_manifest_version fusillade-core)"
    publish_package "$package"
    ;;
  fusillade)
    wait_for_crate_version fusillade-core "$(release_manifest_version fusillade-core)"
    wait_for_crate_version fusillade-arsenal "$(release_manifest_version fusillade-arsenal)"
    publish_package "$package"
    ;;
esac
