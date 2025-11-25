#!/usr/bin/env bash
# Simple script to update control-layer after fusillade is published to crates.io
set -euo pipefail

echo "🔄 Control-Layer Post-Extraction Update"
echo "========================================"
echo ""
echo "This script will:"
echo "  1. Update Cargo.toml workspace to remove fusillade"
echo "  2. Update dwctl/Cargo.toml to use published fusillade crate"
echo "  3. Update release-please-config.json"
echo "  4. Remove fusillade/ directory"
echo "  5. Create a commit with these changes"
echo ""
echo "⚠️  Prerequisites:"
echo "  - Fusillade must be published to crates.io first"
echo "  - Run this from the control-layer repository root"
echo ""

# Check if we're in the right directory
if [ ! -f "Cargo.toml" ] || [ ! -d "dwctl" ]; then
    echo "❌ Error: This doesn't appear to be the control-layer repository root"
    exit 1
fi

if [ ! -d "fusillade" ]; then
    echo "❌ Error: fusillade/ directory not found. Has it already been removed?"
    exit 1
fi

# Check if we're on main branch
CURRENT_BRANCH=$(git rev-parse --abbrev-ref HEAD)
if [ "$CURRENT_BRANCH" = "main" ]; then
    echo "⚠️  You are on the main branch!"
    BRANCH_NAME="extract-fusillade-$(date +%Y%m%d-%H%M%S)"
    read -p "Create a new branch '$BRANCH_NAME'? (y/N) " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        git checkout -b "$BRANCH_NAME"
        echo "✅ Created and switched to branch: $BRANCH_NAME"
    else
        echo "❌ Please create a branch first or switch off main"
        exit 1
    fi
fi

# Check for uncommitted changes
if ! git diff-index --quiet HEAD --; then
    echo "⚠️  You have uncommitted changes"
    read -p "Continue anyway? (y/N) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
fi

echo ""
read -p "Proceed with control-layer update? (y/N) " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Yy]$ ]]; then
    echo "Update cancelled."
    exit 0
fi

echo ""
echo "Step 1: Extracting fusillade version..."
FUSILLADE_VERSION=$(grep -A5 '\[dependencies\]' dwctl/Cargo.toml | grep '^fusillade' | sed -n 's/.*version = "\([^"]*\)".*/\1/p')

if [ -z "$FUSILLADE_VERSION" ]; then
    echo "❌ Error: Could not extract fusillade version from dwctl/Cargo.toml"
    exit 1
fi

echo "Found fusillade version: $FUSILLADE_VERSION"

echo ""
echo "Step 2: Updating root Cargo.toml..."
cat > Cargo.toml << 'EOF'
[workspace]
members = ["dwctl"]
resolver = "2"
EOF
echo "✅ Updated Cargo.toml"

echo ""
echo "Step 3: Updating dwctl/Cargo.toml..."
# Remove the fusillade path dependency line and add the crates.io version
sed -i.bak "s|^fusillade = { path = \"../fusillade\", version = \".*\" }|fusillade = \"$FUSILLADE_VERSION\"|" dwctl/Cargo.toml
rm dwctl/Cargo.toml.bak
echo "✅ Updated dwctl/Cargo.toml"

echo ""
echo "Step 4: Updating release-please-config.json..."
cat > release-please-config.json << 'EOF'
{
  "packages": {
    "dwctl": {
      "release-type": "rust"
    }
  },
  "plugins": [
    {
      "type": "cargo-workspace"
    }
  ],
  "include-component-in-tag": false
}
EOF
echo "✅ Updated release-please-config.json"

echo ""
echo "Step 5: Testing if fusillade $FUSILLADE_VERSION is available on crates.io..."
if cargo update -p fusillade 2>&1 | grep -q "no matching package named"; then
    echo "❌ Error: fusillade $FUSILLADE_VERSION not found on crates.io"
    echo ""
    echo "Please publish fusillade first:"
    echo "  cd ../fusillade-repo"
    echo "  cargo publish"
    echo ""
    echo "Reverting changes..."
    git checkout Cargo.toml dwctl/Cargo.toml release-please-config.json
    exit 1
fi
echo "✅ fusillade $FUSILLADE_VERSION found on crates.io"

echo ""
echo "Step 6: Testing build..."
if ! cargo build 2>&1 | tee /tmp/cargo-build.log; then
    echo "❌ Build failed!"
    echo ""
    echo "Last 20 lines of build output:"
    tail -20 /tmp/cargo-build.log
    echo ""
    echo "Reverting changes..."
    git checkout Cargo.toml dwctl/Cargo.toml release-please-config.json
    cargo update
    exit 1
fi
echo "✅ Build successful!"

echo ""
echo "Step 7: Removing fusillade directory..."
git rm -r fusillade/
echo "✅ Removed fusillade/"

echo ""
echo "Step 8: Staging changes..."
git add Cargo.toml dwctl/Cargo.toml release-please-config.json Cargo.lock

echo ""
echo "Step 9: Creating commit..."
git commit -m "chore: extract fusillade into separate repository

Fusillade has been moved to its own repository:
https://github.com/doublewordai/fusillade

The control-layer now uses fusillade v$FUSILLADE_VERSION from crates.io.

Changes:
- Removed fusillade/ from workspace
- Updated dwctl to depend on published fusillade crate
- Updated release-please config to manage only dwctl
"

echo ""
echo "✅ Control-layer update complete!"
echo ""
echo "Summary of changes:"
git show --stat HEAD
echo ""
echo "Next steps:"
echo "  1. Review the changes: git show HEAD"
echo "  2. Run tests: cargo test"
echo "  3. Update documentation (README.md, CLAUDE.md) to reference new fusillade repo"
echo "  4. Push branch: git push -u origin $(git rev-parse --abbrev-ref HEAD)"
echo "  5. Create pull request on GitHub"
echo ""
