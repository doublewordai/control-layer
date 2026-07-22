#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

cat >"$fixture/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

if [[ "$*" != *"--user-agent"* ]] ||
  [[ "$*" != *"--output /dev/null"* ]] ||
  [[ "$*" != *"--write-out %{http_code}"* ]] ||
  [[ "$*" != *"/onwards/"* ]]; then
  echo "unexpected crates.io probe: $*" >&2
  exit 2
fi

printf '%s' "${CRATE_STATUS:-404}"
exit "${CRATE_PROBE_EXIT:-0}"
STUB

cat >"$fixture/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >>"${CARGO_LOG:?CARGO_LOG is required}"
STUB

chmod +x "$fixture/curl" "$fixture/cargo"

manifest_version="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' onwards/Cargo.toml | head -n 1)"

PATH="$fixture:$PATH" CRATE_STATUS=200 \
  .github/scripts/publish-onwards-crate.sh "onwards-v${manifest_version}"

if PATH="$fixture:$PATH" \
  .github/scripts/publish-onwards-crate.sh "onwards-v0.0.0"; then
  echo "publisher accepted a release tag that does not match Cargo.toml" >&2
  exit 1
fi

PATH="$fixture:$PATH" .github/scripts/publish-onwards-crate.sh "v8.94.0"

CARGO_LOG="$fixture/cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CRATE_STATUS=404 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-onwards-crate.sh "onwards-v${manifest_version}"

grep -Fq "publish --locked --package onwards --all-features --registry crates-io --token test-token" \
  "$fixture/cargo.log"

if CARGO_LOG="$fixture/unexpected-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CRATE_STATUS=503 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-onwards-crate.sh "onwards-v${manifest_version}"; then
  echo "publisher treated a crates.io server error as an unpublished version" >&2
  exit 1
fi

if [[ -e "$fixture/unexpected-cargo.log" ]]; then
  echo "publisher invoked cargo after a crates.io server error" >&2
  exit 1
fi

if CARGO_LOG="$fixture/unexpected-network-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CRATE_PROBE_EXIT=7 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-onwards-crate.sh "onwards-v${manifest_version}"; then
  echo "publisher treated a crates.io network failure as an unpublished version" >&2
  exit 1
fi

if [[ -e "$fixture/unexpected-network-cargo.log" ]]; then
  echo "publisher invoked cargo after a crates.io network failure" >&2
  exit 1
fi
