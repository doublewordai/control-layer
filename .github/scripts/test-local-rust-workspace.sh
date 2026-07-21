#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

python3 - <<'PY'
import json
import pathlib
import subprocess
import tomllib

root = pathlib.Path.cwd()
metadata = json.loads(
    subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        text=True,
    )
)

expected_packages = {
    "dwctl",
    "onwards",
    "fusillade",
    "fusillade-core",
    "fusillade-arsenal",
}
workspace_packages = {
    package["name"]
    for package in metadata["packages"]
    if package["id"] in metadata["workspace_members"]
}
if workspace_packages != expected_packages:
    raise SystemExit(
        "workspace packages differ: "
        f"expected {sorted(expected_packages)}, got {sorted(workspace_packages)}"
    )

packages_by_name = {package["name"]: package for package in metadata["packages"]}
for package_name in expected_packages - {"onwards"}:
    if packages_by_name[package_name]["publish"] != []:
        raise SystemExit(f"{package_name} must declare publish = false")
if packages_by_name["onwards"]["publish"] is not None:
    raise SystemExit("onwards must remain publishable to crates.io")

workspace_manifest = tomllib.loads((root / "Cargo.toml").read_text())
if "patch" in workspace_manifest:
    raise SystemExit("local workspace must not rely on [patch.crates-io]")
if workspace_manifest["workspace"].get("default-members") != ["dwctl"]:
    raise SystemExit("dwctl must remain the default package for root cargo run/build")

local_dependencies = {
    "dwctl/Cargo.toml": {
        "fusillade": "../fusillade",
        "fusillade-arsenal": "../fusillade-arsenal",
        "onwards": "../onwards",
    },
    "onwards/Cargo.toml": {"fusillade": "../fusillade"},
    "fusillade/Cargo.toml": {
        "fusillade-core": "../fusillade-core",
        "fusillade-arsenal": "../fusillade-arsenal",
    },
    "fusillade-arsenal/Cargo.toml": {"fusillade-core": "../fusillade-core"},
}

for manifest_path, dependencies in local_dependencies.items():
    manifest = tomllib.loads((root / manifest_path).read_text())
    declared = manifest["dependencies"]
    for dependency, expected_path in dependencies.items():
        specification = declared.get(dependency)
        if not isinstance(specification, dict):
            raise SystemExit(f"{manifest_path}: {dependency} must be a path dependency")
        if specification.get("path") != expected_path:
            raise SystemExit(
                f"{manifest_path}: {dependency} path must be {expected_path}, "
                f"got {specification.get('path')}"
            )
        if manifest_path == "onwards/Cargo.toml" and dependency == "fusillade":
            local_version = packages_by_name["fusillade"]["version"]
            if specification.get("version") != local_version:
                raise SystemExit(
                    "onwards/Cargo.toml: fusillade must retain a registry fallback "
                    f"at local version {local_version} for crates.io packaging"
                )
            if specification.get("default-features") is not False:
                raise SystemExit(
                    "Onwards must not pull Fusillade's PostgreSQL storage feature"
                )
        elif "version" in specification:
            raise SystemExit(
                f"{manifest_path}: {dependency} must not retain a crates.io version"
            )

release_config = json.loads((root / "release-please-config.json").read_text())
if set(release_config["packages"]) != {".", "onwards"}:
    raise SystemExit("Release Please must manage the application and Onwards independently")
root_release = release_config["packages"]["."]
if root_release.get("release-type") != "simple":
    raise SystemExit("root application release must use the annotated simple strategy")
extra_paths = {
    entry.get("path")
    for entry in root_release.get("extra-files", [])
    if isinstance(entry, dict)
}
if "dwctl/Cargo.toml" not in extra_paths:
    raise SystemExit("Release Please must update dwctl/Cargo.toml")
onwards_release = release_config["packages"]["onwards"]
if onwards_release.get("release-type") != "rust":
    raise SystemExit("Onwards releases must use the Rust strategy")
if onwards_release.get("component") != "onwards":
    raise SystemExit("Onwards releases must use component-prefixed tags")
if onwards_release.get("include-component-in-tag") is not True:
    raise SystemExit("Onwards tags must use the onwards-v<version> namespace")

release_manifest = json.loads((root / ".release-please-manifest.json").read_text())
if set(release_manifest) != {".", "onwards"}:
    raise SystemExit("release manifest must track dwctl and Onwards independently")
PY

release_workflow=".github/workflows/release.yml"
justfile="justfile"
backfill_script="scripts/backfill_responses_to_batchless.sql"

if [[ ! -f "$backfill_script" ]] || \
   [[ "$(shasum -a 256 "$backfill_script" | awk '{print $1}')" != \
      "c56c1ceb0ba3cd0dad9632f424022c66f9dd5f3c1dddbd2d1154df9542872f0e" ]]; then
  echo "Fusillade's operational response backfill must retain its imported bytes" >&2
  exit 1
fi

if grep -Eq 'publish-(dwctl|fusillade)|cargo publish (--package )?(dwctl|fusillade)( |$)' \
  "$release_workflow" "$justfile"; then
  echo "application and Fusillade releases must not publish Rust crates" >&2
  exit 1
fi

if ! grep -Fq 'publish-onwards:' "$release_workflow" || \
   ! grep -Fq 'publish-onwards-crate.sh' "$release_workflow" || \
   ! grep -Fq 'cargo publish --locked' .github/scripts/publish-onwards-crate.sh || \
   ! grep -Fq -- '--package onwards --all-features --registry crates-io' \
     .github/scripts/publish-onwards-crate.sh; then
  echo "Onwards releases must publish the crate" >&2
  exit 1
fi

if ! grep -Fq 'path: .release-tools' "$release_workflow" || \
   ! grep -Fq '.release-tools/.github/scripts/publish-onwards-crate.sh' \
     "$release_workflow"; then
  echo "Onwards release retries must use current release tooling against tagged source" >&2
  exit 1
fi

if ! grep -Fq "make_latest: 'false'" "$release_workflow"; then
  echo "Onwards releases must not replace the application as GitHub's latest release" >&2
  exit 1
fi

if ! grep -Fq 'onwards-image:' "$release_workflow" || \
   ! grep -Fq 'ghcr.io/doublewordai/onwards' "$release_workflow" || \
   ! grep -Fq 'file: ./onwards/Dockerfile' "$release_workflow"; then
  echo "Onwards releases must publish the standalone image" >&2
  exit 1
fi

if ! grep -Fq "type=semver,pattern={{major}}.{{minor}},value=\${{ steps.version.outputs.version }},enable=\${{ github.event_name == 'release' }}" \
     "$release_workflow" || \
   ! grep -Fq "type=raw,value=latest,enable=\${{ github.event_name == 'release' }}" \
     "$release_workflow"; then
  echo "Manual Onwards retries must not move floating image tags" >&2
  exit 1
fi

if [[ ! -f onwards/Dockerfile ]]; then
  echo "Onwards must retain a standalone image definition" >&2
  exit 1
fi

if ! grep -Fq 'USER ubuntu' onwards/Dockerfile; then
  echo "The standalone Onwards image must run as a non-root user" >&2
  exit 1
fi

if ! grep -Fq 'onwards-pr-image:' .github/workflows/ci.yaml || \
   ! grep -Fq 'ghcr.io/doublewordai/onwards:sha-' .github/workflows/ci.yaml; then
  echo "Onwards pull requests must retain SHA image publication" >&2
  exit 1
fi


if ! grep -Fq 'cargo package --locked --package onwards --all-features' \
     .github/workflows/ci.yaml; then
  echo "CI must validate Onwards against its packaged dependency graph" >&2
  exit 1
fi

if ! grep -Fq 'onwards-openresponses-compliance:' .github/workflows/ci.yaml || \
   ! grep -Fq 'mode: [adapter, passthrough]' .github/workflows/ci.yaml || \
   ! grep -Fq 'git checkout fa29df5' .github/workflows/ci.yaml; then
  echo "CI must retain standalone Onwards adapter and passthrough compliance" >&2
  exit 1
fi

if grep -Fq 'trace!("{:?}", new_targets)' onwards/src/target.rs || \
   grep -Eq 'Per-key (rate|concurrency) limit exceeded for token' \
     onwards/src/handlers.rs; then
  echo "Onwards must not log provider or caller credentials" >&2
  exit 1
fi

if [[ "$(grep -Fc 'COPY onwards/ onwards/' Dockerfile)" != 2 ]]; then
  echo "Docker planner and builder must both receive the local Onwards crate" >&2
  exit 1
fi

if grep -Eq '^/?onwards/$' .dockerignore; then
  echo "Docker context must not exclude the local Onwards crate" >&2
  exit 1
fi

if ! grep -Fq 'x-release-please-version' dwctl/Cargo.toml; then
  echo "dwctl version must retain its generic Release Please annotation" >&2
  exit 1
fi

if grep -Fq 'release-please--' .github/workflows/ci.yaml; then
  echo "Release Please pull requests must run the normal validation pipeline" >&2
  exit 1
fi

for obsolete_script in \
  .github/scripts/publish-fusillade-crate.sh \
  .github/scripts/sync-fusillade-release-dependencies.py \
  .github/scripts/test-fusillade-publish.sh \
  .github/scripts/test-sync-fusillade-release-dependencies.sh \
  .github/scripts/wait-for-fusillade-crates.sh; do
  if [[ -e "$obsolete_script" ]]; then
    echo "obsolete crates.io release script remains: $obsolete_script" >&2
    exit 1
  fi
done
