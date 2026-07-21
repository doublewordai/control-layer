#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

cat >"$fixture/curl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

saw_user_agent=0
for argument in "$@"; do
  case "$argument" in
    --user-agent | --user-agent=* | -A | User-Agent:*) saw_user_agent=1 ;;
  esac
done

if [[ "$saw_user_agent" != 1 ]]; then
  echo "curl was called without a User-Agent" >&2
  exit 22
fi

if [[ -n "${CURL_LOG:-}" ]]; then
  printf '%s\n' "$*" >>"$CURL_LOG"
fi
STUB

cat >"$fixture/cargo" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail

echo "cargo should not run when crates.io reports the version as available" >&2
exit 1
STUB

chmod +x "$fixture/curl" "$fixture/cargo"

manifest_version() {
  sed -n 's/^version = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

core_version="$(manifest_version fusillade-core/Cargo.toml)"
arsenal_version="$(manifest_version fusillade-arsenal/Cargo.toml)"
fusillade_version="$(manifest_version fusillade/Cargo.toml)"

PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-core-v${core_version}"

CURL_LOG="$fixture/arsenal-curl.log" PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-arsenal-v${arsenal_version}"
grep -Fq "/fusillade-core/${core_version}" "$fixture/arsenal-curl.log"

CURL_LOG="$fixture/fusillade-curl.log" PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-v${fusillade_version}"
grep -Fq "/fusillade-core/${core_version}" "$fixture/fusillade-curl.log"
grep -Fq "/fusillade-arsenal/${arsenal_version}" "$fixture/fusillade-curl.log"

if PATH="$fixture:$PATH" \
  .github/scripts/publish-fusillade-crate.sh "fusillade-v0.0.0"; then
  echo "publisher accepted a release tag that does not match Cargo.toml" >&2
  exit 1
fi

PATH="$fixture:$PATH" .github/scripts/publish-fusillade-crate.sh "v8.94.0"

CURL_LOG="$fixture/dwctl-curl.log" PATH="$fixture:$PATH" \
  .github/scripts/wait-for-fusillade-crates.sh
grep -Fq "/fusillade/${fusillade_version}" "$fixture/dwctl-curl.log"
grep -Fq "/fusillade-arsenal/${arsenal_version}" "$fixture/dwctl-curl.log"

grep -Fq "startsWith(github.event.release.tag_name, 'v')" .github/workflows/release.yml
grep -Fq "startsWith(github.event.release.tag_name, 'fusillade-')" .github/workflows/release.yml
grep -Fq "bash .release-tools/.github/scripts/publish-fusillade-crate.sh \"\$RELEASE_TAG\"" \
  .github/workflows/release.yml
grep -Fq 'sync-fusillade-release-dependencies.py' \
  .github/workflows/release-please.yaml

if ! jq -e '.packages.fusillade["release-type"] == "simple"' \
  release-please-config.json >/dev/null; then
  echo "the Fusillade root crate must use the annotated simple release strategy" >&2
  exit 1
fi

if jq -e '.plugins // [] | index("cargo-workspace") != null' \
  release-please-config.json >/dev/null; then
  echo "the cargo-workspace plugin must not stamp independent crate versions" >&2
  exit 1
fi

if ! grep -q 'x-release-please-version' fusillade/Cargo.toml; then
  echo "fusillade/Cargo.toml must retain its generic release annotation" >&2
  exit 1
fi

release_manifest_version() {
  jq -r --arg package "$1" '.[$package]' .release-please-manifest.json
}

for package_path in dwctl fusillade fusillade-core fusillade-arsenal; do
  declared_version="$(manifest_version "$package_path/Cargo.toml")"
  tracked_version="$(release_manifest_version "$package_path")"
  if [[ "$declared_version" != "$tracked_version" ]]; then
    echo "$package_path declares $declared_version but release-please tracks $tracked_version" >&2
    exit 1
  fi
done

python3 .github/scripts/sync-fusillade-release-dependencies.py --check
