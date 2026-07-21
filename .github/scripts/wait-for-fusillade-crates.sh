#!/usr/bin/env bash
set -euo pipefail

manifest_version() {
  sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

wait_for_crate_version() {
  local crate="$1"
  local version="$2"

  for attempt in $(seq 1 30); do
    if curl --fail --silent --show-error \
      --user-agent "control-layer-release-script (https://github.com/doublewordai/control-layer)" \
      "https://crates.io/api/v1/crates/${crate}/${version}" \
      >/dev/null; then
      echo "${crate} ${version} is available on crates.io."
      return 0
    fi

    echo "Waiting for ${crate} ${version} to appear on crates.io (${attempt}/30)..."
    sleep 10
  done

  echo "${crate} ${version} did not appear on crates.io in time." >&2
  exit 1
}

wait_for_crate_version fusillade "$(manifest_version fusillade/Cargo.toml)"
wait_for_crate_version fusillade-arsenal "$(manifest_version fusillade-arsenal/Cargo.toml)"
