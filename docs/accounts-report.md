# Account storage, authentication, and usage report

Implemented the account layer in `src/state.rs`, `src/auth.rs`, and `src/usage.rs`, with integration coverage in `tests/accounts.rs`.

## Interfaces

- `Paths`, `Store`, and `Account` implement `Clone`; `Store::paths` is public.
- State transactions use an interprocess file lock, validate state after deserialization, and replace `state.json` atomically with private permissions.
- `State::add` validates aliases and account identity. Identity is the email and workspace/account ID pair. `--force` may overwrite the requested alias, but never silently removes a different alias with the same identity.
- `auth::import_current` and `auth::activate` honor file storage and explicitly reject keyring, auto, and ephemeral modes.
- `auth::login` runs `codex login` in a temporary `CODEX_HOME` configured for file credentials. Failed login leaves the real Codex home untouched.
- `auth::credentials_for` pins email and workspace ID inside the same locked transaction used for refresh, protecting a runtime from alias replacement.
- Credential reads import a strictly newer live file bundle only when its user and workspace match. Stale or different live credentials cannot replace saved tokens.
- OAuth refresh is blocking, bounded to 15 seconds, serialized by the store transaction, validates the refreshed user and workspace, preserves a rotated refresh token, and only persists after a complete valid response. After that durable write, a second locked transaction publishes the latest saved bundle to the live file only if its identity still matches. Publication errors cannot roll back the saved rotated token.
- `auth::login_params` emits the current generated app-server `chatgptAuthTokens` shape.
- Usage reads the current Codex `rate_limit.primary_window` and `secondary_window` response fields from `/backend-api/wham/usage`. Selection excludes the current alias, unknown/stale/future observations, disallowed accounts, malformed percentages, and exhausted primary or secondary windows. Missing usage is never treated as zero.

## Verification

- `cargo test --lib`: 4 passed.
- `cargo test --test accounts`: 18 passed.
- `cargo clippy --lib --tests -- -D warnings`: passed.

The tests use isolated temporary homes and a local HTTP refresh fixture. They do not run a real login or contact OpenAI. Live two-account switching remains an integration verification item.
