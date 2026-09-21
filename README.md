# cx

Switch between Codex accounts with the same commands as [cs](https://github.com/carlosarraes/cs).

Run `cx` to open the native Codex terminal UI. From another terminal, run
`cx switch work` to change its account without restarting it. Busy sessions
finish their current turn before applying the change.

Rust. Linux and macOS. ChatGPT OAuth accounts.

## Install

Install [Codex CLI](https://developers.openai.com/codex/cli/) first, then build:

```sh
just build                   # builds release binary and installs ~/.local/bin/cx
```

Or use `cargo install --path .`. The repository pins Rust 1.97.0 via rustup.

Version 0.1 supports Codex file credential storage. If your Codex configuration
uses `keyring` or `auto`, configure file storage before adding accounts:

```toml
# ~/.codex/config.toml, or $CODEX_HOME/config.toml
cli_auth_credentials_store = "file"
```

`cx` rejects incompatible storage modes explicitly. Changing that setting does
not export an existing keyring login; use `cx add personal` to sign in again.

## Start

```sh
cx add personal --current    # save the current Codex login
cx add work                  # sign in to the other account and activate it
cx switch personal
cx                           # start Codex

# In another terminal:
cx switch work
cx switch -                  # switch back
```

Adding an account delegates login to Codex in a temporary home. Failed login
leaves the existing login alone. A successful add saves and selects the account,
like `cs add`. The browser must sign in to the intended account.

## Commands

| Command | Behavior |
| --- | --- |
| `cx` | Launch native Codex with live account switching |
| `cx -- resume --last` | Resume the latest conversation with live switching |
| `cx -- -C /path/to/project` | Start in another project |
| `cx add <alias> [--current] [--force]` | Save and activate an account |
| `cx add <alias> --device-auth` | Use Codex device-code login |
| `cx switch <alias>` | Select an account and notify every managed session |
| `cx switch -` | Toggle to the previous account |
| `cx switch next` | Fetch usage and select another eligible account with lowest primary-window usage |
| `cx list` | List aliases, `*` current and `-` previous |
| `cx del <alias>` | Forget an alias without logging out |
| `cx whoami` | Show live login, selected account, and managed session identities |
| `cx refresh` | Capture newer live credentials after an external login |
| `cx usage [--live]` | Show cached usage, or fetch fresh observations |
| `cx --help` / `cx --version` | Help and version |

Usage follows the `cs` layout, with aligned aliases and reset countdowns:

```text
- personal  5h 10% (resets 2h53m) · 7d 94% (resets 1d3h) · idle 13m
* work      7d 83% (resets 5d11h) · running 1h18m
```

Window labels come from Codex's reported duration. Accounts with only a weekly
limit show `7d`. Older cached readings lack that duration and use `primary` or
`secondary` until refreshed with `cx usage --live`.

Like `cs`, `running` and `idle` measure time since selecting or leaving an
account, not whether a model request is active. Existing accounts show the label
without a timer until a selection is recorded. Cached readings older than two
minutes also show `as of ... ago`.

`cx switch --yes` retains the corresponding `cs` flag. It suppresses the
unmanaged-session reminder; it never interrupts an active turn.

Native Codex arguments go after `--`. Interactive launches, `resume`, and `fork`
use the remote TUI transport. There is no `cx run` subcommand. For `codex exec`
and other noninteractive commands, use Codex directly.

Usage windows are colored in interactive terminals: green below 70%, yellow
from 70% to below 90%, red from 90%, and bold red at 100% or above. Each window's
percentage and reset countdown share its color. Piped output stays plain;
`NO_COLOR=1` or `TERM=dumb` disables colors.

## Live switching

Each `cx` process owns a Codex app-server and a native TUI. A private Unix socket
forwards their protocol messages. Account changes use the app-server's
`chatgptAuthTokens` login method; `cx` handles its token refresh callbacks.

`cx switch` reports each reachable session as applied, pending until idle, or
failed. If a session cannot apply a selection, the command reports that partial
failure. `cx whoami` shows the account actually loaded in each session. Retry
`cx switch <alias>` after fixing the reported problem.

An already-running plain `codex`, IDE extension, or desktop app is outside this
controller. Launch through `cx` to enable live switching. Existing conversations
can be resumed with `cx -- resume`.

The controller shares your Codex home, settings, and conversation history across
accounts. It does not fork Codex or proxy model HTTP traffic. It changes accounts
between turns, including review and compaction turns; it does not replay tool
calls or restart a failed inference request.

Account deletion is refused while a managed session uses or is waiting for that
alias. Switch or close those sessions first so their refresh credentials remain
available.

## Storage and compatibility

Aliases and credentials live in `$XDG_DATA_HOME/cx/state.json`, defaulting to
`~/.local/share/cx/state.json`. Writes use a lock and atomic replacement, with
directory mode `0700` and credential file mode `0600`. Credentials are plaintext
on disk, like Codex file storage. Treat the file as a password.

`CODEX_HOME` is honored. `CX_CODEX_BIN` selects a different Codex executable.
Account identity includes the user email and workspace ID; sharing a workspace
does not make two users the same account. Unknown, stale, or exhausted usage
does not qualify an account for `switch next`. A failed live usage check also
excludes that account from the selection, even when cached usage exists.

The live-login protocol is **unstable** in Codex. Tested against Codex CLI
`0.154.0`; an incompatible version produces an explicit error. The installed
Codex smoke test verifies changing synthetic account identities in the same
app-server. Real account authentication and model requests still require a
manual two-account check.

Version 0.1 covers manual switching. Automatic rotation, service installation,
logs commands, and session snapshots are follow-up work.

## Development

The Justfile is copied from `cs`, with the binary name changed to `cx`.

```sh
just check                   # formatting, Clippy, tests
just run list
just build
just sync                    # install locally and on mac, no release needed
just sync another-host       # use another SSH host alias

# Optional installed-Codex protocol smoke test, using synthetic credentials:
cargo test --test runtime installed_codex_applies_account_changes_without_restarting -- --ignored
```

Integration tests use temporary homes, local HTTP fixtures, and fake Codex
subprocesses communicating over real Unix WebSockets. Python 3 is needed for
the fake subprocess fixture; no third-party Python packages are used.

`just sync` installs the current working tree, including uncommitted changes.
It copies the binary when OS and architecture match; otherwise it syncs only
Cargo manifests, the Rust toolchain file, and `src/` into `~/.cache/cx-src` on
the host and builds there. The remote Cargo build cache is reused. The new
binary is checked before replacing `~/.local/bin/cx`; credentials and account
state stay on their own machine. The destination needs SSH and, for a native
build, rsync and Rust via rustup.

Sync does not bump `Cargo.toml`, commit, push, tag, or create a GitHub release.
`cx --version` stays at the package version even when you sync newer code.

The copied `just release` recipe commits, tags, and pushes. The release workflow
builds Linux and macOS binaries for x86_64 and ARM64, then publishes them with
SHA-256 checksums on GitHub.

## License

[MIT](LICENSE)
