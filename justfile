# cx — Codex account switcher

binary_name := "cx"
install_dir := env_var("HOME") / ".local/bin"
version := `grep -m1 '^version' Cargo.toml | cut -d'"' -f2`

default: build

# Carlos's Mac is an install-only host for this tool.
[private]
assert-build-host:
    #!/usr/bin/env bash
    set -euo pipefail
    case "$(hostname -s)" in
        Carloss-MacBook-Air|Carloss-MacBook-Air.*)
            echo "error: do not compile cx on mac; run just sync mac mact" >&2
            exit 1;;
    esac
    if [[ -e "$HOME/.config/cx/no-build" ]]; then
        echo "error: compilation disabled on this host; run just sync mac mact" >&2
        exit 1
    fi

# Build release binary and copy to ~/.local/bin
build: assert-build-host
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

# Build on a separate host, then atomically install on the destination.
[positional-arguments]
sync host="mac" builder="mact":
    #!/usr/bin/env bash
    set -euo pipefail
    sync_host="$1"
    build_host="$2"
    for host in "$sync_host" "$build_host"; do
        if [[ ! "$host" =~ ^[a-zA-Z0-9][a-zA-Z0-9._@-]*$ ]]; then
            echo "error: use an SSH host alias, hostname, or user@host" >&2
            exit 1
        fi
    done
    if [[ "$build_host" == mac ]]; then
        echo "error: mac is an install-only host; use mact as the builder" >&2
        exit 1
    fi
    ssh_options=(-o BatchMode=yes -o ControlPath=none -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2)
    builder_name=$(ssh "${ssh_options[@]}" "$build_host" 'hostname -s')
    case "$builder_name" in
        Carloss-MacBook-Air|Carloss-MacBook-Air.*)
            echo "error: Carlos's Mac is an install-only host; use mact as the builder" >&2
            exit 1;;
    esac
    builder_platform=$(ssh "${ssh_options[@]}" "$build_host" 'uname -sm')
    destination_platform=$(ssh "${ssh_options[@]}" "$sync_host" 'uname -sm')
    if [[ "$builder_platform" != "$destination_platform" ]]; then
        echo "error: builder $build_host is $builder_platform, destination $sync_host is $destination_platform; choose a matching builder" >&2
        exit 1
    fi
    stage=$(mktemp -d "${TMPDIR:-/tmp}/cx-sync.XXXXXX")
    pending=""
    builder_stage=""
    cleanup() {
        rm -rf "$stage"
        if [[ -n "$builder_stage" ]]; then
            ssh "${ssh_options[@]}" "$build_host" "rm -rf -- '$builder_stage'" || true
        fi
        if [[ -n "$pending" ]]; then
            ssh "${ssh_options[@]}" "$sync_host" "rm -f -- '$pending'" || true
        fi
    }
    trap cleanup EXIT
    candidate=$(ssh "${ssh_options[@]}" "$build_host" 'test ! -e "$HOME/.config/cx/no-build" || { echo "error: compilation disabled on this host" >&2; exit 1; }; mkdir -p ~/.cache/cx-build; mktemp -d "$HOME/.cache/cx-build/source.XXXXXX"')
    if [[ ! "$candidate" =~ ^/[a-zA-Z0-9_./-]+/\.cache/cx-build/source\.[a-zA-Z0-9]+$ ]]; then
        echo "error: invalid builder staging path" >&2
        exit 1
    fi
    builder_stage="$candidate"
    echo "Building cx on $build_host ($builder_platform); installing on $sync_host"
    rsync -az \
        --include='/Cargo.toml' --include='/Cargo.lock' --include='/build.rs' \
        --include='/rust-toolchain.toml' --include='/src/***' \
        --exclude='*' -e 'ssh -o BatchMode=yes -o ControlPath=none -o ConnectTimeout=10 -o ServerAliveInterval=15 -o ServerAliveCountMax=2' ./ "$build_host:$builder_stage/"
    ssh "${ssh_options[@]}" "$build_host" "bash -s -- '$builder_stage'" <<'BUILD'
    set -euo pipefail
    test ! -e "$HOME/.config/cx/no-build" || { echo "error: compilation disabled on this host" >&2; exit 1; }
    export PATH="$HOME/.cargo/bin:$PATH"
    python3 - "$1" <<'PYBUILD'
    import fcntl, os, pathlib, shutil, subprocess, sys
    source = pathlib.Path(sys.argv[1])
    cache = pathlib.Path.home() / ".cache/cx-build"
    with (cache / "build.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        env = dict(os.environ, CARGO_TARGET_DIR=str(cache / "target"))
        result = subprocess.run(["cargo", "build", "--locked", "--release"], cwd=source, env=env)
        if result.returncode:
            sys.exit(result.returncode)
        shutil.copy2(cache / "target/release/cx", source / "cx")
    PYBUILD
    BUILD
    scp -q "${ssh_options[@]}" "$build_host:$builder_stage/cx" "$stage/cx"
    candidate=$(ssh "${ssh_options[@]}" "$sync_host" 'mkdir -p ~/.local/bin; mktemp "$HOME/.local/bin/.cx-sync.XXXXXX"')
    if [[ ! "$candidate" =~ ^/[a-zA-Z0-9_./-]+$ ]]; then
        echo "error: invalid destination staging path" >&2
        exit 1
    fi
    pending="$candidate"
    scp -q "${ssh_options[@]}" "$stage/cx" "$sync_host:$pending"
    ssh "${ssh_options[@]}" "$sync_host" "bash -s -- '$pending'" <<'SH'
    set -euo pipefail
    pending="$1"
    trap 'rm -f "$pending"' EXIT
    chmod 755 "$pending"
    "$pending" --version
    mv -f "$pending" "$HOME/.local/bin/cx"
    echo "Installed $HOME/.local/bin/cx"
    SH
    pending=""

# Run tests
test: assert-build-host
    cargo test

# Format code
fmt:
    cargo fmt

# Check formatting (CI gate)
fmt-check:
    cargo fmt --check

# Lint with warnings as errors
lint: assert-build-host
    cargo clippy --all-targets -- -D warnings

# Format check + lint + tests
check: fmt-check lint test

# Build and run, e.g. `just run list`
run *ARGS: assert-build-host
    cargo run -- {{ARGS}}

# Print the current version
version:
    @echo {{version}}

# Cut a release: bump Cargo.toml, commit, tag vX.Y.Z, push (CI builds binaries).
# Usage: just release 0.0.2
release new_version: assert-build-host
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
