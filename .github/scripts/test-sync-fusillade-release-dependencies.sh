#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

mkdir -p "$fixture/fusillade-arsenal"

cat >"$fixture/.release-please-manifest.json" <<'JSON'
{
  "fusillade-core": "5.0.0",
  "fusillade-arsenal": "3.1.0"
}
JSON

cat >"$fixture/fusillade-arsenal/Cargo.toml" <<'TOML'
[package]
name = "fusillade-arsenal"
version = "3.1.0"

[dependencies]
fusillade-core = { version = "4.0.0", path = "../fusillade-core" }
TOML

script="$repo_root/.github/scripts/sync-fusillade-release-dependencies.py"

if python3 "$script" --check "$fixture"; then
  echo "check mode accepted an incompatible Core dependency requirement" >&2
  exit 1
fi

python3 "$script" "$fixture"
grep -Fq \
  'fusillade-core = { version = "5.0.0", path = "../fusillade-core" }' \
  "$fixture/fusillade-arsenal/Cargo.toml"
python3 "$script" --check "$fixture"

cp \
  "$fixture/fusillade-arsenal/Cargo.toml" \
  "$fixture/fusillade-arsenal/Cargo.toml.once"
python3 "$script" "$fixture"
diff -u \
  "$fixture/fusillade-arsenal/Cargo.toml.once" \
  "$fixture/fusillade-arsenal/Cargo.toml"
