#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fixture="$(mktemp -d)"
trap 'rm -rf "$fixture"' EXIT

mkdir -p "$fixture/dwctl" "$fixture/fusillade" \
  "$fixture/fusillade-core" "$fixture/fusillade-arsenal"

cat >"$fixture/.release-please-manifest.json" <<'JSON'
{
  "dwctl": "8.95.0",
  "fusillade": "24.0.0",
  "fusillade-core": "3.0.0",
  "fusillade-arsenal": "3.0.0"
}
JSON

cat >"$fixture/fusillade/Cargo.toml" <<'TOML'
[package]
name = "fusillade"
version = "24.0.0"

[dependencies]
fusillade-core = { version = "2.1.0", path = "../fusillade-core" }
fusillade-arsenal = { version = "2.1.0", path = "../fusillade-arsenal" }
TOML

cat >"$fixture/fusillade-core/Cargo.toml" <<'TOML'
[package]
name = "fusillade-core"
version = "3.0.0"
TOML

cat >"$fixture/fusillade-arsenal/Cargo.toml" <<'TOML'
[package]
name = "fusillade-arsenal"
version = "3.0.0"

[dependencies]
fusillade-core = { version = ">=2.1.0, <4.0.0", path = "../fusillade-core" }
TOML

cat >"$fixture/dwctl/Cargo.toml" <<'TOML'
[package]
name = "dwctl"
version = "8.95.0"

[dependencies]
fusillade = { version = "23.0.2", path = "../fusillade" }
fusillade-arsenal = { version = "2.1.4", path = "../fusillade-arsenal" }
TOML

script="$repo_root/.github/scripts/sync-fusillade-release-dependencies.py"

if python3 "$script" --check "$fixture"; then
  echo "check mode should reject stale dependency requirements" >&2
  exit 1
fi

python3 "$script" "$fixture"

grep -Fq 'fusillade-core = { version = "3.0.0", path = "../fusillade-core" }' \
  "$fixture/fusillade/Cargo.toml"
grep -Fq 'fusillade-arsenal = { version = "3.0.0", path = "../fusillade-arsenal" }' \
  "$fixture/fusillade/Cargo.toml"
grep -Fq 'fusillade = { version = "24.0.0", path = "../fusillade" }' \
  "$fixture/dwctl/Cargo.toml"
grep -Fq 'fusillade-arsenal = { version = "3.0.0", path = "../fusillade-arsenal" }' \
  "$fixture/dwctl/Cargo.toml"
grep -Fq 'fusillade-core = { version = ">=2.1.0, <4.0.0", path = "../fusillade-core" }' \
  "$fixture/fusillade-arsenal/Cargo.toml"

python3 "$script" --check "$fixture"

cp "$fixture/fusillade/Cargo.toml" "$fixture/fusillade/Cargo.toml.once"
cp "$fixture/dwctl/Cargo.toml" "$fixture/dwctl/Cargo.toml.once"

python3 "$script" "$fixture"

diff -u "$fixture/fusillade/Cargo.toml.once" "$fixture/fusillade/Cargo.toml"
diff -u "$fixture/dwctl/Cargo.toml.once" "$fixture/dwctl/Cargo.toml"
