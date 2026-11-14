#!/usr/bin/env bash
# Run only the tests that map to the files changed on this branch.
#
# Usage: scripts/test-quick.sh [--print] [--stdin] [--base <ref>] [nextest args...]
#
#   --base <ref>  compare against the merge base with <ref> (default origin/main)
#   --print       print the nextest filterset and exit; nothing is built or run
#   --stdin       read the changed paths from stdin instead of asking git
#
# Changed files are everything that differs from the merge base: committed,
# staged, unstaged and untracked. The mapping is a heuristic for the inner
# loop; CI still runs the whole suite. Doctests are skipped.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

print=false
stdin=false
base=origin/main
args=()
while (($#)); do
    case "$1" in
        --print) print=true; shift ;;
        --stdin) stdin=true; shift ;;
        --base)
            if (($# < 2)); then echo "--base requires a value" >&2; exit 2; fi
            base="$2"; shift 2 ;;
        --base=*) base="${1#*=}"; shift ;;
        --) args+=("$@"); break ;;
        *) args+=("$1"); shift ;;
    esac
done

changed_files() {
    if $stdin; then
        cat
        return
    fi
    local merge_base
    if ! merge_base=$(git merge-base "$base" HEAD 2>/dev/null); then
        echo "test quick: no merge base with '$base'; fetch it or pass --base <ref>" >&2
        return 2
    fi
    git -c core.quotePath=false diff --name-only --no-renames "$merge_base" --
    git -c core.quotePath=false ls-files --others --exclude-standard
}

full=""        # first path that needs the whole workspace
dwctl_all=false
dwctl_parts="" # filters inside package(dwctl), one per line
parts=""       # filters for the other crates, one per line
memory=false

dwctl_test() { dwctl_parts+="test(/^$1::/)"$'\n'; }
is_module() { [[ "$1" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; }
# First path component without its extension: users.rs and users/x.rs -> users.
stem() { local first="${1%%/*}"; printf '%s' "${first%%.*}"; }

# Test names in the dwctl library are module paths (api::handlers::users::...),
# so a source path maps to an anchored module prefix.
map_dwctl_src() {
    local rel="$1" layer rest kind name
    case "$rel" in
        # Crate root, binaries and the shared test harness reach every test.
        lib.rs|main.rs|bin/*|test/mod.rs|test/utils.rs|test/databases.rs|test/template.rs)
            dwctl_all=true ;;
        api/handlers/*|api/models/*|db/handlers/*|db/models/*)
            layer="${rel%%/*}"; rest="${rel#*/}"; kind="${rest%%/*}"; name=$(stem "${rest#*/}")
            if [[ "$name" == mod || "$name" == repository ]]; then
                dwctl_test "$layer::$kind"
            elif is_module "$name"; then
                # A resource's repository is exercised through its API handler.
                dwctl_test "api::handlers::$name"
                dwctl_test "api::models::$name"
                if [[ "$layer" == db ]]; then
                    dwctl_test "db::handlers::$name"
                    dwctl_test "db::models::$name"
                fi
            else
                dwctl_all=true
            fi ;;
        api/*|db/*|test/*)
            layer="${rel%%/*}"; name=$(stem "${rel#*/}")
            if [[ "$name" == mod ]]; then dwctl_test "$layer"
            elif is_module "$name"; then dwctl_test "$layer::$name"
            else dwctl_all=true; fi ;;
        *.rs|*/*)
            name=$(stem "$rel")
            if is_module "$name"; then dwctl_test "$name"; else dwctl_all=true; fi ;;
        *) dwctl_all=true ;;
    esac
}

map_path() {
    local path="$1" name
    case "$path" in
        Cargo.toml|Cargo.lock|.config/*|.cargo/*|rust-toolchain|rust-toolchain.toml)
            [[ -n "$full" ]] || full="$path" ;;
        *.md|*/LICENSE|*/Dockerfile|onwards/docs/*) ;;
        dwctl/test-macros/*)
            dwctl_all=true; parts+="package(dwctl-test-macros)"$'\n' ;;
        dwctl/src/*) map_dwctl_src "${path#dwctl/src/}" ;;
        # Integration binaries can embed the migrations themselves, so run them too.
        dwctl/migrations/*|dwctl/underway-migrations/*)
            dwctl_parts+="kind(test)"$'\n'; dwctl_test migrations ;;
        # Excluded by the default nextest profile, so a filter cannot select them.
        dwctl/tests/memory_budget/*) memory=true ;;
        dwctl/tests/*)
            name="${path#dwctl/tests/}"
            # A deleted test file has no binary, and nextest rejects unknown names.
            if [[ "$name" == *.rs && "$name" != */* && -f "$path" && "${name%.rs}" =~ ^[A-Za-z0-9_-]+$ ]]; then
                dwctl_parts+="binary(${name%.rs})"$'\n'
            else
                # Shared fixtures: the integration binaries and the migration tests read them.
                dwctl_parts+="kind(test)"$'\n'
                dwctl_test migrations
            fi ;;
        dwctl/*) dwctl_all=true ;;
        fusillade-arsenal/migrations/*)
            parts+="package(fusillade-arsenal)"$'\n'; dwctl_test migrations ;;
        fusillade/*|fusillade-arsenal/*|fusillade-core/*|onwards/*|openai-reassembler/*)
            parts+="package(${path%%/*})"$'\n' ;;
    esac
}

files=$(changed_files)
while IFS= read -r path; do
    [[ -n "$path" ]] && map_path "$path"
done <<<"$files"

unique() { printf '%s' "$1" | LC_ALL=C sort -u; }
join_or() {
    local line out=""
    while IFS= read -r line; do
        [[ -n "$line" ]] && out+="${out:+ | }$line"
    done
    printf '%s' "$out"
}

if $dwctl_all; then
    parts+="package(dwctl)"$'\n'
elif [[ -n "$dwctl_parts" ]]; then
    inner=$(unique "$dwctl_parts" | join_or)
    [[ "$inner" == *" | "* ]] && inner="($inner)"
    parts+="package(dwctl) & $inner"$'\n'
fi

if [[ -n "$full" ]]; then
    expr="all()"
    echo "test quick: $full changed; running the whole workspace" >&2
else
    expr=$(unique "$parts" | join_or)
fi

if $memory; then
    echo "test quick: memory-budget tests changed; run them with 'just test memory'" >&2
fi

if [[ -z "$expr" ]]; then
    echo "test quick: no Rust changes against $base; nothing to run" >&2
    if $print; then echo "none()"; fi
    exit 0
fi

if $print; then
    echo "$expr"
    exit 0
fi

echo "test quick: -E '$expr'" >&2
# No -p: test-rust.sh keeps --workspace --all-features, so the build matches a
# full run and cargo does not rebuild for a different feature set. A changed
# file whose module has no tests is not a failure.
filter=()
if [[ -z "$full" ]]; then filter=(-E "$expr"); fi
exec ./scripts/test-rust.sh --no-doctests ${filter[@]+"${filter[@]}"} --no-tests=warn ${args[@]+"${args[@]}"}
