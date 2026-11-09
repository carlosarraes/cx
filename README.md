# cx

Switch between Codex accounts with the same commands as [cs](https://github.com/carlosarraes/cs).

Run `cx` or `cx run -- ...` to open the native Codex terminal UI on an automatically
selected account. From another terminal, use `cx sessions` to find its conversation
ID, then `cx switch work --session <ID>` to move it. Busy sessions finish their
current turn before applying the change. `--interrupt` cancels the turn first.

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
| `cx` / `cx run -- <args>` | Launch native Codex with automatic account selection |
| `cx run --account <alias-or-email> -- <args>` | Pin identity, with no fallback |
| `cx sessions` | Show managed conversation IDs, accounts, labels and pending moves |
| `cx switch work --session <UUID> [--interrupt]` | Move one interactive conversation |
| `cx switch work --from personal [--interrupt]` | Move all sessions on an account |
| `cx --llm` | Concise agent instructions, supported headless flags and exit codes |
| `cx -- resume --last` | Resume the latest conversation with live switching |
| `cx -- -C /path/to/project` | Start in another project |
| `cx add <alias> [--current] [--force]` | Save and activate an account |
| `cx add <alias> --device-auth` | Use Codex device-code login |
| `cx switch <alias>` | Change shared login and broadcast to unpinned interactive sessions |
| `cx switch -` | Toggle to the previous account |
| `cx switch next` | Fetch usage and select another eligible account with lowest primary-window usage |
| `cx list` | List aliases, `*` current and `-` previous |
| `cx del <alias>` | Forget an alias without logging out |
| `cx whoami` | Show live login, selected account, and managed session identities |
| `cx refresh` | Capture newer live credentials after an external login |
| `cx usage [--live]` | Show cached usage, or fetch fresh observations |
| `cx resets [alias]` | List available rate-limit reset credits, soonest expiry first |
| `cx reset [alias] [--credit N] [-y]` | Spend a reset credit; defaults to the current account and the soonest-expiring credit |
| `cx --help` / `cx --version` | Help and version |

Usage follows the `cs` layout, with aligned aliases and reset countdowns:

```text
personal (2)  5h 10% (resets 2h53m) · 7d 94% (resets 1d3h) · idle 13m
work     (0)  7d 83% (resets 5d11h) · running 1h18m
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
use the remote TUI transport. `cx run -- exec ...` uses a non-interactive app-server
adapter with an explicit supported subset of exec-style flags. Run `cx --llm` for
that list, JSONL compatibility limits and outcome codes.

Usage windows are colored in interactive terminals: green below 70%, yellow
from 70% to below 90%, red from 90%, and bold red at 100% or above. Each window's
percentage and reset countdown share its color. Piped output stays plain;
`NO_COLOR=1` or `TERM=dumb` disables colors.

## Smart routing and bots

New runs check all reported quota windows and refresh stale observations. Eligible
accounts are ranked by live session count, then remaining quota per hour until
reset, then alias. Reservations are atomic, so concurrent starts see each other.
Recent conversation affinity is preferred only while that identity remains eligible.
An exhausted account does not trap a resumed conversation. Explicit pins never fall back.

```sh
cx run --account work --label QA-123 --usage-file run.json -- exec -m MODEL -c model_reasoning_effort=high --json -o answer.txt "prompt"
```

Headless runs use the same account registry and have a per-account concurrency cap.
The adapter fails on unsupported arguments or interactive approval requests.
JSON stdout contains core thread, turn and item events; it is not a full native
`codex exec` JSONL replacement. Routing messages and errors use stderr.

Usage records are written atomically, including failures. Requested/resolved model
and effort are separate fields. App-server model evidence is not backend attestation;
observed mismatches fail. Missing token evidence stays null with a reason. Resumed
runs subtract a pre-turn baseline when available rather than reporting historical
tokens as new usage. `native_exit_status` is null because no native exec is launched.

Configure `$XDG_CONFIG_HOME/cx/config.toml` or `~/.config/cx/config.toml`:

```toml
[routing]
usage_max_age_seconds = 300
affinity_seconds = 3600
max_headless_per_account = 2
```

The affinity interval is routing policy, not a claim about Codex cache lifetime.
Session counts include launches reserved before the model starts. Installing the
binary does not move existing sessions. Older controllers remain visible but must
be reopened with updated `cx run` before targeted switching works.

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
`0.164.0`; an incompatible version produces an explicit error. The installed
Codex smoke test verifies login acknowledgements for synthetic identities in the
same app-server. Authenticated workspace discovery and model requests require
a separate real-account smoke run.

Automatic mid-run quota recovery, service installation, logs commands and session
snapshots remain follow-up work. No failed task is automatically replayed.

## Development

The Justfile is copied from `cs`, with the binary name changed to `cx`.

```sh
just check                   # formatting, Clippy, tests
just run list
just build
just sync                    # build on mact, install on mac; no local build
just sync destination builder # builder must match destination OS/architecture

# Optional installed-Codex protocol smoke test, using synthetic credentials:
cargo test --test runtime installed_codex_applies_account_changes_without_restarting -- --ignored
```

Integration tests use temporary homes, local HTTP fixtures, and fake Codex
subprocesses communicating over real Unix WebSockets. Tests require Python 3
and just; no third-party Python packages are used. Sync tests mock SSH, rsync
and scp, so they never connect to or compile on a real host.

Never compile cs or cx on `mac`, Carlos's busy MacBook Air.
`just sync` defaults to `just sync mac mact`. It sends the current source,
including uncommitted changes, to a private source directory under `~/.cache/cx-build` on `mact` and builds there.
It copies the binary through the caller and atomically installs it on `mac`.
Sync never compiles on the destination or implicitly builds/installs locally.

For another platform, pass a matching builder: `just sync DESTINATION BUILDER`.
Mismatched OS/architecture, failed builds or failed transfers leave the installed
binary untouched. The caller needs SSH, rsync and scp access to both hosts; the
remotes do not need SSH access to each other. Only the builder needs Rust/rustup and Python 3.
Existing checkouts and account credentials are not copied or overwritten.
Concurrent syncs have separate sources and artifacts. A lock protects the shared
Cargo cache through compilation and artifact capture. Temporary sources are removed.

Local compilation recipes refuse Carlos's Mac by hostname or the persistent
`~/.config/cx/no-build` marker. Use `just build` only on permitted hosts.

Sync does not bump `Cargo.toml`, commit, push, tag, or create a GitHub release.
`cx --version` stays at the package version even when you sync newer code.

The copied `just release` recipe commits, tags, and pushes. The release workflow
builds Linux and macOS binaries for x86_64 and ARM64, then publishes them with
SHA-256 checksums on GitHub.

## License

[MIT](LICENSE)

`cx t3-codex` and the `cx-t3-codex` symlink pass t3 arguments and stdio through to Codex after account routing. `cx pick` names the routed account without launching Codex.
