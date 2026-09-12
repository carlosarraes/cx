use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Write;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::{Account, Paths, Store};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

pub fn import_current(paths: &Paths) -> Result<Account> {
    ensure_file_store(paths)?;
    read_account(&paths.codex_home.join("auth.json"))
}

pub fn activate(paths: &Paths, account: &Account) -> Result<()> {
    ensure_file_store(paths)?;
    if !account.data.is_object() {
        bail!("stored credentials are malformed");
    }
    fs::create_dir_all(&paths.codex_home)?;
    set_private_dir(&paths.codex_home)?;
    let target = paths.codex_home.join("auth.json");
    let mut temporary = tempfile::NamedTempFile::new_in(&paths.codex_home)?;
    temporary.write_all(&serde_json::to_vec_pretty(&account.data)?)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    set_private_file(temporary.as_file())?;
    temporary.persist(&target).map_err(|error| error.error)?;
    File::open(&paths.codex_home)?.sync_all()?;
    Ok(())
}

pub fn login(_paths: &Paths, codex: &OsStr, device_auth: bool) -> Result<Account> {
    let temporary_home = tempfile::tempdir().context("cannot create isolated login directory")?;
    fs::write(
        temporary_home.path().join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let mut command = Command::new(codex);
    command
        .arg("login")
        .env("CODEX_HOME", temporary_home.path());
    if device_auth {
        command.arg("--device-auth");
    }
    let status = command.status().context("cannot start Codex login")?;
    if !status.success() {
        bail!("Codex login failed with status {status}");
    }
    read_account(&temporary_home.path().join("auth.json"))
        .context("Codex login completed without usable ChatGPT credentials")
}

pub fn credentials(store: &Store, alias: &str, force_refresh: bool) -> Result<Account> {
    credentials_inner(store, alias, None, force_refresh)
}

pub fn credentials_for(
    store: &Store,
    alias: &str,
    expected_email: &str,
    expected_account_id: &str,
    force_refresh: bool,
) -> Result<Account> {
    credentials_inner(
        store,
        alias,
        Some((expected_email, expected_account_id)),
        force_refresh,
    )
}

fn credentials_inner(
    store: &Store,
    alias: &str,
    expected: Option<(&str, &str)>,
    force_refresh: bool,
) -> Result<Account> {
    let (mut account, refreshed) = store.transaction(|state| {
        let account = state
            .accounts
            .get_mut(alias)
            .with_context(|| format!("unknown account alias '{alias}'"))?;
        if expected.is_some_and(|(email, account_id)| {
            account.email != email || account.account_id != account_id
        }) {
            bail!("account identity changed while credentials were in use");
        }
        import_newer_live(&store.paths, account)?;
        let refreshed = force_refresh || expires_soon(account)?;
        if refreshed {
            refresh(account)?;
        }
        Ok((account.clone(), refreshed))
    })?;
    if refreshed {
        account = publish_latest_if_matching(store, alias, &account.email, &account.account_id)?;
    }
    Ok(account)
}

pub fn login_params(account: &Account) -> Result<Value> {
    let access_token = token(account, "access_token")?;
    let plan = jwt_claims(access_token)
        .ok()
        .and_then(|claims| auth_claim(&claims, "chatgpt_plan_type").map(str::to_owned));
    Ok(json!({
        "type": "chatgptAuthTokens",
        "accessToken": access_token,
        "chatgptAccountId": account.account_id,
        "chatgptPlanType": plan,
    }))
}

fn ensure_file_store(paths: &Paths) -> Result<()> {
    let mode = credential_store_mode(paths)?;
    match mode.as_str() {
        "file" => Ok(()),
        "keyring" | "auto" | "ephemeral" => {
            bail!("cannot import current credentials when cli_auth_credentials_store is '{mode}'")
        }
        other => bail!("unsupported cli_auth_credentials_store value '{other}'"),
    }
}

fn credential_store_mode(paths: &Paths) -> Result<String> {
    let config_path = paths.codex_home.join("config.toml");
    let mode = match fs::read_to_string(&config_path) {
        Ok(contents) => contents
            .parse::<toml::Value>()
            .map_err(|_| anyhow::anyhow!("Codex config is malformed: {}", config_path.display()))?
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            .unwrap_or("file")
            .to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "file".to_owned(),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", config_path.display()))
        }
    };
    Ok(mode)
}

fn import_newer_live(paths: &Paths, saved: &mut Account) -> Result<()> {
    if credential_store_mode(paths)? != "file" {
        return Ok(());
    }
    let Ok(live) = read_account(&paths.codex_home.join("auth.json")) else {
        return Ok(());
    };
    if live.email == saved.email
        && live.account_id == saved.account_id
        && refresh_time(&live.data) > refresh_time(&saved.data)
    {
        saved.data = live.data;
    }
    Ok(())
}

fn publish_latest_if_matching(
    store: &Store,
    alias: &str,
    expected_email: &str,
    expected_account_id: &str,
) -> Result<Account> {
    store.transaction(|state| {
        let latest = state
            .accounts
            .get(alias)
            .context("account was deleted after refresh")?;
        if latest.email != expected_email || latest.account_id != expected_account_id {
            bail!("account identity changed after credentials were refreshed");
        }
        if credential_store_mode(&store.paths)? != "file" {
            return Ok(latest.clone());
        }
        let Ok(live) = read_account(&store.paths.codex_home.join("auth.json")) else {
            return Ok(latest.clone());
        };
        if live.email == latest.email
            && live.account_id == latest.account_id
            && refresh_time(&latest.data) > refresh_time(&live.data)
        {
            activate(&store.paths, latest)
                .context("refreshed credentials were saved, but publishing them to Codex failed")?;
        }
        Ok(latest.clone())
    })
}

fn refresh_time(data: &Value) -> Option<chrono::DateTime<Utc>> {
    data.get("last_refresh")
        .and_then(Value::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn read_account(path: &std::path::Path) -> Result<Account> {
    let data: Value = serde_json::from_slice(
        &fs::read(path)
            .with_context(|| format!("cannot read credentials from {}", path.display()))?,
    )
    .with_context(|| format!("credentials in {} are malformed", path.display()))?;
    if data.get("auth_mode").and_then(Value::as_str) != Some("chatgpt") {
        bail!("only ChatGPT OAuth credentials are supported");
    }
    let id_token = data
        .pointer("/tokens/id_token")
        .and_then(Value::as_str)
        .context("ChatGPT credentials lack an ID token")?;
    let claims = jwt_claims(id_token).context("ChatGPT ID token is malformed")?;
    let email = claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| profile_claim(&claims, "email"))
        .filter(|value| !value.trim().is_empty())
        .context("ChatGPT credentials lack an email identity")?;
    let token_account_id = data
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty());
    let claim_account_id =
        auth_claim(&claims, "chatgpt_account_id").filter(|value| !value.trim().is_empty());
    if let (Some(stored), Some(claimed)) = (token_account_id, claim_account_id) {
        if stored != claimed {
            bail!("ChatGPT credential workspace identity does not match its token");
        }
    }
    let account_id = token_account_id
        .or(claim_account_id)
        .context("ChatGPT credentials lack a workspace identity")?;
    token_value(&data, "access_token")?;
    token_value(&data, "refresh_token")?;
    Ok(Account {
        email: email.to_owned(),
        account_id: account_id.to_owned(),
        data,
        usage: None,
    })
}

fn jwt_claims(token: &str) -> Result<Value> {
    let payload = token.split('.').nth(1).context("token is not a JWT")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("JWT payload is not base64url")?;
    serde_json::from_slice(&bytes).context("JWT payload is not JSON")
}

fn auth_claim<'a>(claims: &'a Value, name: &str) -> Option<&'a str> {
    claims.get(name).and_then(Value::as_str).or_else(|| {
        claims
            .get("https://api.openai.com/auth")?
            .get(name)?
            .as_str()
    })
}

fn profile_claim<'a>(claims: &'a Value, name: &str) -> Option<&'a str> {
    claims
        .get("https://api.openai.com/profile")?
        .get(name)?
        .as_str()
}

fn token<'a>(account: &'a Account, name: &str) -> Result<&'a str> {
    token_value(&account.data, name)
}

fn token_value<'a>(data: &'a Value, name: &str) -> Result<&'a str> {
    data.get("tokens")
        .and_then(|tokens| tokens.get(name))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("stored credentials lack {name}"))
}

fn expires_soon(account: &Account) -> Result<bool> {
    let claims = jwt_claims(token(account, "access_token")?)?;
    let expires_at = claims
        .get("exp")
        .and_then(Value::as_i64)
        .context("access token lacks expiry")?;
    Ok(expires_at <= Utc::now().timestamp() + 300)
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: Option<String>,
    id_token: Option<String>,
    refresh_token: Option<String>,
}

fn refresh(account: &mut Account) -> Result<()> {
    let old_refresh = token(account, "refresh_token")?.to_owned();
    let endpoint = std::env::var("CODEX_REFRESH_TOKEN_URL_OVERRIDE")
        .unwrap_or_else(|_| "https://auth.openai.com/oauth/token".to_owned());
    let response = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build()?
        .post(endpoint)
        .json(&json!({"client_id": CLIENT_ID, "grant_type": "refresh_token", "refresh_token": old_refresh}))
        .send().context("token refresh request failed")?;
    if !response.status().is_success() {
        bail!(
            "token refresh was rejected with status {}",
            response.status()
        );
    }
    let refreshed: RefreshResponse = response
        .json()
        .context("token refresh response is malformed")?;
    let access = refreshed
        .access_token
        .filter(|value| !value.is_empty())
        .context("token refresh response lacks an access token")?;
    let claims = jwt_claims(&access).context("refreshed access token is malformed")?;
    let refreshed_id = auth_claim(&claims, "chatgpt_account_id")
        .context("refreshed token lacks a workspace identity")?;
    let refreshed_email = claims
        .get("email")
        .and_then(Value::as_str)
        .or_else(|| profile_claim(&claims, "email"))
        .context("refreshed token lacks an email identity")?;
    if refreshed_id != account.account_id || refreshed_email != account.email {
        bail!("refreshed token belongs to a different account identity");
    }
    let tokens = account
        .data
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .context("stored credentials have malformed tokens")?;
    tokens.insert("access_token".to_owned(), Value::String(access));
    if let Some(id_token) = refreshed.id_token.filter(|value| !value.is_empty()) {
        tokens.insert("id_token".to_owned(), Value::String(id_token));
    }
    if let Some(refresh_token) = refreshed.refresh_token.filter(|value| !value.is_empty()) {
        tokens.insert("refresh_token".to_owned(), Value::String(refresh_token));
    }
    account
        .data
        .as_object_mut()
        .context("stored credentials are malformed")?
        .insert(
            "last_refresh".to_owned(),
            Value::String(Utc::now().to_rfc3339()),
        );
    Ok(())
}

#[cfg(unix)]
fn set_private_dir(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_dir(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file(_file: &File) -> Result<()> {
    Ok(())
}
