# cx — Codex account switcher

binary_name := "cx"
install_dir := env_var("HOME") / ".local/bin"
version := `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`

default: build

# Build release binary and copy to ~/.local/bin
build:
    cargo build --release
    mkdir -p {{install_dir}}
    cp target/release/{{binary_name}} {{install_dir}}/
    @echo "Installed {{binary_name}} -> {{install_dir}}/{{binary_name}}"

# Run tests
test:
    cargo test

# Format code
fmt:
    cargo fmt

# Check formatting (CI gate)
fmt-check:
    cargo fmt --check

# Lint with warnings as errors
lint:
    cargo clippy --all-targets -- -D warnings

# Format check + lint + tests
check: fmt-check lint test

# Build and run, e.g. `just run list`
run *ARGS:
    cargo run -- {{ARGS}}

# Print the current version
version:
    @echo {{version}}

# Cut a release: bump Cargo.toml, commit, tag vX.Y.Z, push (CI builds binaries).
# Usage: just release 0.0.2
release new_version:
    #!/usr/bin/env bash
    set -euo pipefail
    ver="{{new_version}}"; ver="${ver#v}"
    if ! printf '%s' "$ver" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
        echo "error: version must be semver like 0.0.2 (got '{{new_version}}')" >&2; exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
        echo "error: working tree is dirty; commit or stash first" >&2; exit 1
    fi
    if git rev-parse "v$ver" >/dev/null 2>&1; then
        echo "error: tag v$ver already exists" >&2; exit 1
    fi
    cargo test
    sed -i -E "s/^version = \".*\"/version = \"$ver\"/" Cargo.toml
    cargo build --quiet
    git add Cargo.toml Cargo.lock
    if git diff --cached --quiet; then
        echo "Cargo.toml already at $ver; tagging the current commit"
    else
        git commit -m "chore: release v$ver"
    fi
    git tag -a "v$ver" -m "v$ver"
    remote="$(git config "branch.$(git branch --show-current).remote" 2>/dev/null || git remote | head -n1)"
    git push "$remote" HEAD
    git push "$remote" "v$ver"
    echo "Pushed v$ver — GitHub Actions will build and publish the release."
