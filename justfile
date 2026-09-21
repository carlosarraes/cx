# cx — Codex account switcher

binary_name := "cx"
install_dir := env_var("HOME") / ".local/bin"
version := `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`

default: build

# Build release binary and copy to ~/.local/bin
build:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo build --release
    mkdir -p "{{install_dir}}"
    pending=$(mktemp "{{install_dir}}/.cx-install.XXXXXX")
    trap 'rm -f "$pending"' EXIT
    install -m 755 target/release/{{binary_name}} "$pending"
    "$pending" --version
    mv -f "$pending" "{{install_dir}}/{{binary_name}}"
    echo "Installed {{binary_name}} -> {{install_dir}}/{{binary_name}}"

# Build locally and install on an SSH host without a release (defaults to mac).
[positional-arguments]
sync host="mac": build
    #!/usr/bin/env bash
    set -euo pipefail
    sync_host="$1"
    if [[ ! "$sync_host" =~ ^[a-zA-Z0-9][a-zA-Z0-9._@-]*$ ]]; then
        echo "error: use an SSH host alias, hostname, or user@host" >&2
        exit 1
    fi
    remote=$(ssh -o BatchMode=yes "$sync_host" 'uname -sm')
    ssh -o BatchMode=yes "$sync_host" 'mkdir -p ~/.local/bin ~/.cache/cx-src'
    if [ "$remote" = "$(uname -sm)" ]; then
        scp -q target/release/{{binary_name}} "$sync_host":.cache/cx-src/cx-sync-binary
        sync_binary='.cache/cx-src/cx-sync-binary'
    else
        echo "$sync_host is $remote; syncing source and building there"
        rsync -az --delete \
            --include='/Cargo.toml' --include='/Cargo.lock' \
            --include='/rust-toolchain.toml' --include='/src/***' \
            --exclude='*' -e 'ssh -o BatchMode=yes' ./ "$sync_host":.cache/cx-src/
        ssh -o BatchMode=yes "$sync_host" 'export PATH="$HOME/.cargo/bin:$PATH"; cd ~/.cache/cx-src && cargo build --locked --release'
        sync_binary='.cache/cx-src/target/release/cx'
    fi
    ssh -o BatchMode=yes "$sync_host" "bash -s -- $sync_binary" <<'SH'
    set -euo pipefail
    pending=$(mktemp "$HOME/.local/bin/.cx-sync.XXXXXX")
    trap 'rm -f "$pending"' EXIT
    install -m 755 "$HOME/$1" "$pending"
    "$pending" --version
    mv -f "$pending" "$HOME/.local/bin/cx"
    echo "Installed $HOME/.local/bin/cx"
    "$HOME/.local/bin/cx" --version
    SH

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
