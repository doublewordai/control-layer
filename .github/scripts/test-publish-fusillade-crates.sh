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
  [[ "$*" != *"--write-out %{http_code}"* ]]; then
  echo "unexpected crates.io probe: $*" >&2
  exit 2
fi

case "$*" in
  */fusillade-core/*)
    if [[ -n "${CORE_STATUS_FILE:-}" ]]; then
      head -n 1 "$CORE_STATUS_FILE"
      tail -n +2 "$CORE_STATUS_FILE" >"${CORE_STATUS_FILE}.next"
      mv "${CORE_STATUS_FILE}.next" "$CORE_STATUS_FILE"
    else
      printf '%s' "${CORE_STATUS:-${CRATE_STATUS:-404}}"
    fi
    ;;
  */fusillade-arsenal/*)
    printf '%s' "${ARSENAL_STATUS:-${CRATE_STATUS:-404}}"
    ;;
  *) echo "unexpected crate probe: $*" >&2; exit 2 ;;
esac
exit "${CRATE_PROBE_EXIT:-0}"
STUB

cat >"$fixture/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$*" >>"${CARGO_LOG:?CARGO_LOG is required}"

if [[ "$1" == "publish" && -n "${CARGO_PUBLISH_EXIT:-}" ]]; then
  exit "$CARGO_PUBLISH_EXIT"
fi
STUB

chmod +x "$fixture/curl" "$fixture/cargo"

manifest_version() {
  sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

core_version="$(manifest_version fusillade-core/Cargo.toml)"
arsenal_version="$(manifest_version fusillade-arsenal/Cargo.toml)"

PATH="$fixture:$PATH" CRATE_STATUS=200 \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"
PATH="$fixture:$PATH" CRATE_STATUS=200 \
  .github/scripts/publish-fusillade-crate.sh \
  "fusillade-arsenal-v${arsenal_version}"

if PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v0.0.0"; then
  echo "publisher accepted a release tag that does not match Cargo.toml" >&2
  exit 1
fi

PATH="$fixture:$PATH" .github/scripts/publish-fusillade-crate.sh "v8.103.2"
PATH="$fixture:$PATH" .github/scripts/publish-fusillade-crate.sh "fusillade-v24.1.0"

CARGO_LOG="$fixture/core-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CORE_STATUS=404 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"
grep -Fq \
  "publish --locked --manifest-path fusillade-core/Cargo.toml --registry crates-io" \
  "$fixture/core-cargo.log"
if grep -Fq -- "--token" "$fixture/core-cargo.log"; then
  echo "publisher exposed the registry token in process arguments" >&2
  exit 1
fi

CARGO_LOG="$fixture/arsenal-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CORE_STATUS=200 \
  ARSENAL_STATUS=404 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh \
  "fusillade-arsenal-v${arsenal_version}"
grep -Fq \
  "info fusillade-core@${core_version} --registry crates-io" \
  "$fixture/arsenal-cargo.log"
grep -Fq \
  "publish --locked --package fusillade-arsenal --registry crates-io" \
  "$fixture/arsenal-cargo.log"
if grep -Fq -- "--token" "$fixture/arsenal-cargo.log"; then
  echo "publisher exposed the registry token in process arguments" >&2
  exit 1
fi

printf '404\n200\n' >"$fixture/core-statuses"
CARGO_LOG="$fixture/racing-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CARGO_PUBLISH_EXIT=1 \
  CORE_STATUS_FILE="$fixture/core-statuses" \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"

if CARGO_LOG="$fixture/unexpected-cargo.log" \
  CARGO_REGISTRY_TOKEN="test-token" \
  CRATE_STATUS=503 \
  PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"; then
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
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"; then
  echo "publisher treated a crates.io network failure as an unpublished version" >&2
  exit 1
fi

if [[ -e "$fixture/unexpected-network-cargo.log" ]]; then
  echo "publisher invoked cargo after a crates.io network failure" >&2
  exit 1
fi
