# Contributing

## Testing

Run the test suite:

```bash
cargo test
```

## Release process

This project uses automated releases through [release-plz](https://release-plz.dev/).

### How releases work

1. **Make changes** using [conventional commits](https://www.conventionalcommits.org/):
   - `feat:` for new features (minor version bump)
   - `fix:` for bug fixes (patch version bump)
   - `feat!:` or `fix!:` for breaking changes (major version bump)

2. **Create a pull request** with your changes

3. **Merge the PR** -- this triggers the release-plz workflow

4. **Release PR appears** -- release-plz automatically creates a PR with:
   - Updated version in `Cargo.toml`
   - Generated changelog
   - All changes since last release

5. **Review and merge** the release PR

6. **Automated publishing** -- when the release PR is merged:
   - release-plz publishes the crate to crates.io
   - Creates a GitHub release with changelog

### Conventional commit examples

```
feat: add new proxy authentication method
fix: resolve connection timeout issues
docs: update API documentation
chore: update dependencies
feat!: change configuration file format (BREAKING CHANGE)
```

The release workflow automatically handles version bumping and publishing based on your commit messages.
