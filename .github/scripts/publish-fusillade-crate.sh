#!/usr/bin/env bash
set -euo pipefail

release_tag="${1:?release tag is required}"

case "$release_tag" in
  fusillade-core-v*) package="fusillade-core" ;;
  fusillade-arsenal-v*) package="fusillade-arsenal" ;;
  *)
    echo "Release tag '$release_tag' is not a publishable Fusillade crate tag; skipping."
    exit 0
    ;;
esac

manifest_for_package() {
  case "$1" in
    fusillade-core) echo "fusillade-core/Cargo.toml" ;;
    fusillade-arsenal) echo "fusillade-arsenal/Cargo.toml" ;;
    *) echo "unknown package '$1'" >&2; exit 1 ;;
  esac
}

manifest_version() {
  sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

release_manifest_version() {
  python3 - "$1" <<'PY'
import json
import sys

with open(".release-please-manifest.json") as manifest:
    print(json.load(manifest).get(sys.argv[1], ""))
PY
}

crate_status() {
  local crate="$1"
  local version="$2"

  curl --silent --show-error \
    --output /dev/null \
    --write-out '%{http_code}' \
    --user-agent "control-layer-release-script (https://github.com/doublewordai/control-layer)" \
    "https://crates.io/api/v1/crates/${crate}/${version}"
}

crate_version_available() {
  local crate="$1"
  local version="$2"
  local status

  if ! status="$(crate_status "$crate" "$version")"; then
    echo "Failed to determine whether ${crate} ${version} is published." >&2
    return 2
  fi

  case "$status" in
    200) return 0 ;;
    404) return 1 ;;
    *)
      echo "Unexpected crates.io response while checking ${crate} ${version}: HTTP ${status}." >&2
      return 2
      ;;
  esac
}

wait_for_crate_version() {
  local crate="$1"
  local version="$2"

  if [[ -z "$version" ]]; then
    echo "No tracked version found for ${crate}; refusing to publish a dependent crate." >&2
    exit 1
  fi

  for attempt in $(seq 1 30); do
    if crate_version_available "$crate" "$version"; then
      if cargo info "${crate}@${version}" --registry crates-io >/dev/null 2>&1; then
        echo "${crate} ${version} is available through Cargo's crates.io index."
        return 0
      fi
      probe_status=1
    else
      probe_status=$?
    fi

    if [[ "$probe_status" -eq 2 ]]; then
      exit 1
    fi

    echo "Waiting for ${crate} ${version} to appear on crates.io (${attempt}/30)..."
    sleep 10
  done

  echo "${crate} ${version} did not appear on crates.io in time." >&2
  exit 1
}

package_manifest="$(manifest_for_package "$package")"
package_version="$(manifest_version "$package_manifest")"
tag_version="${release_tag##*-v}"

if [[ "$package_version" != "$tag_version" ]]; then
  echo "Release tag '$release_tag' points at ${package} ${tag_version}, but ${package_manifest} contains ${package_version}." >&2
  exit 1
fi

if crate_version_available "$package" "$package_version"; then
  echo "${package} ${package_version} is already published; skipping."
  exit 0
else
  probe_status=$?
fi

if [[ "$probe_status" -eq 2 ]]; then
  exit 1
fi

case "$package" in
  fusillade-core)
    if ! cargo publish --locked --manifest-path fusillade-core/Cargo.toml --registry crates-io; then
      if crate_version_available fusillade-core "$package_version"; then
        echo "fusillade-core ${package_version} was published concurrently; continuing."
        exit 0
      fi
      exit 1
    fi
    ;;
  fusillade-arsenal)
    wait_for_crate_version \
      fusillade-core \
      "$(release_manifest_version fusillade-core)"
    if ! cargo publish --locked --package fusillade-arsenal --registry crates-io; then
      if crate_version_available fusillade-arsenal "$package_version"; then
        echo "fusillade-arsenal ${package_version} was published concurrently; continuing."
        exit 0
      fi
      exit 1
    fi
    ;;
esac
