use std::fs;

use anyhow::Result;
use cx::state::{Account, Paths, State, Store};
use cx::usage::{choose_next, Observation, Window};
use serde_json::json;
use tempfile::TempDir;

static REFRESH_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn account(email: &str, id: &str) -> Account {
    Account {
        email: email.into(),
        account_id: id.into(),
        data: json!({"tokens":{"access_token":"secret"}}),
        usage: None,
    }
}

fn store() -> (TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("codex"),
    };
    (temp, Store::new(paths))
}

#[test]
fn select_toggles_current_and_previous() -> Result<()> {
    let mut state = State::default();
    state.add("one", account("one@example.com", "workspace-1"), false)?;
    state.add("two", account("two@example.com", "workspace-2"), false)?;
    assert_eq!(state.select("one")?, "one");
    assert_eq!(state.select("two")?, "two");
    assert_eq!(state.select("-")?, "one");
    assert_eq!(
        (state.current.as_deref(), state.previous.as_deref()),
        (Some("one"), Some("two"))
    );
    Ok(())
}

#[test]
fn duplicate_user_workspace_identity_is_rejected_without_renaming() -> Result<()> {
    let mut state = State::default();
    state.add("old", account("user@example.com", "workspace-1"), false)?;
    assert!(state
        .add("new", account("user@example.com", "workspace-1"), false)
        .is_err());
    assert!(state
        .add("new", account("user@example.com", "workspace-1"), true)
        .is_err());
    state.add("other", account("other@example.com", "workspace-1"), false)?;
    state.add("old", account("updated@example.com", "workspace-2"), true)?;
    assert_eq!(state.accounts.len(), 2);
    Ok(())
}

#[test]
fn invalid_alias_and_missing_identity_are_rejected() {
    let mut state = State::default();
    assert!(state
        .add("bad alias", account("a@b.com", "workspace"), false)
        .is_err());
    assert!(state.add("ok", account("a@b.com", "  "), false).is_err());
}

#[test]
fn public_alias_validation_matches_add_validation() {
    assert!(cx::state::validate_alias("work-2").is_ok());
    assert!(cx::state::validate_alias("work.2").is_ok());
    assert!(cx::state::validate_alias("bad alias").is_err());
    for reserved in ["-", "next", ".", ".."] {
        assert!(cx::state::validate_alias(reserved).is_err());
    }
}

#[test]
fn transaction_is_atomic_and_corruption_is_reported() -> Result<()> {
    let (_temp, store) = store();
    store.transaction(|state| state.add("one", account("a@b.com", "workspace"), false))?;
    let failed: Result<()> = store.transaction(|state| {
        state.current = Some("one".into());
        anyhow::bail!("stop")
    });
    assert!(failed.is_err());
    assert_eq!(store.read()?.current, None);
    fs::write(store.paths.data.join("state.json"), b"not json")?;
    assert!(store.read().is_err());
    Ok(())
}

#[test]
fn read_rejects_state_with_missing_workspace_identity() -> Result<()> {
    let (_temp, store) = store();
    fs::create_dir_all(&store.paths.data)?;
    fs::write(
        store.paths.data.join("state.json"),
        serde_json::to_vec(&json!({
            "current": "bad", "previous": null,
            "accounts": {"bad": {"email":"a@b.com", "account_id":"", "data":{}, "usage":null}}
        }))?,
    )?;
    assert!(store.read().is_err());
    Ok(())
}

#[test]
fn choose_next_ignores_unknown_stale_and_disallowed_usage() -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let fresh = |used| {
        Some(Observation {
            primary: Some(Window {
                used_percent: used,
                resets_at: None,
            }),
            secondary: None,
            observed_at: now,
            allowed: true,
        })
    };
    let mut state = State::default();
    state.add("current", account("c@x", "c"), false)?;
    state.add("unknown", account("u@x", "u"), false)?;
    let mut low = account("l@x", "l");
    low.usage = fresh(12.0);
    state.add("low", low, false)?;
    let mut high = account("h@x", "h");
    high.usage = fresh(70.0);
    state.add("high", high, false)?;
    let mut stale = account("s@x", "s");
    stale.usage = Some(Observation {
        primary: Some(Window {
            used_percent: 1.0,
            resets_at: None,
        }),
        secondary: None,
        observed_at: now - 7200,
        allowed: true,
    });
    state.add("stale", stale, false)?;
    let mut exhausted = account("e@x", "e");
    exhausted.usage = Some(Observation {
        primary: Some(Window {
            used_percent: 2.0,
            resets_at: None,
        }),
        secondary: Some(Window {
            used_percent: 100.0,
            resets_at: None,
        }),
        observed_at: now,
        allowed: true,
    });
    state.add("exhausted", exhausted, false)?;
    let mut future = account("f@x", "f");
    future.usage = Some(Observation {
        primary: Some(Window {
            used_percent: 0.0,
            resets_at: None,
        }),
        secondary: None,
        observed_at: now + 60,
        allowed: true,
    });
    state.add("future", future, false)?;
    state.select("current")?;
    assert_eq!(choose_next(&state)?, "low");
    Ok(())
}

#[test]
fn secret_values_do_not_appear_in_debug_output() {
    let value = account("a@b.com", "workspace");
    assert!(!format!("{value:?}").contains("secret"));
}

fn jwt(payload: serde_json::Value) -> String {
    use base64::Engine;
    format!(
        "e30.{}.sig",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

#[test]
fn import_current_reads_file_mode_and_workspace_identity() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("codex"),
    };
    fs::create_dir_all(&paths.codex_home)?;
    fs::write(
        paths.codex_home.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )?;
    let id_token = jwt(json!({"email":"person@example.com","chatgpt_account_id":"workspace-7"}));
    fs::write(
        paths.codex_home.join("auth.json"),
        serde_json::to_vec(&json!({
            "auth_mode":"chatgpt", "tokens": {"id_token": id_token, "access_token":"access", "refresh_token":"refresh", "account_id":"workspace-7"}
        }))?,
    )?;
    let imported = cx::auth::import_current(&paths)?;
    assert_eq!(
        (imported.email.as_str(), imported.account_id.as_str()),
        ("person@example.com", "workspace-7")
    );
    Ok(())
}

#[test]
fn import_current_reads_namespaced_openai_claims() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("codex"),
    };
    fs::create_dir_all(&paths.codex_home)?;
    let id_token = jwt(json!({
        "https://api.openai.com/profile": {"email":"nested@example.com"},
        "https://api.openai.com/auth": {"chatgpt_account_id":"workspace-nested"}
    }));
    fs::write(
        paths.codex_home.join("auth.json"),
        serde_json::to_vec(&json!({
            "auth_mode":"chatgpt", "tokens": {"id_token": id_token, "access_token":"access", "refresh_token":"refresh"}
        }))?,
    )?;
    let imported = cx::auth::import_current(&paths)?;
    assert_eq!(
        (imported.email.as_str(), imported.account_id.as_str()),
        ("nested@example.com", "workspace-nested")
    );
    Ok(())
}

#[test]
fn import_current_rejects_non_file_storage_without_reading_stale_auth() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("codex"),
    };
    fs::create_dir_all(&paths.codex_home)?;
    fs::write(
        paths.codex_home.join("config.toml"),
        "cli_auth_credentials_store = \"keyring\"\n",
    )?;
    fs::write(paths.codex_home.join("auth.json"), b"not json")?;
    let error = cx::auth::import_current(&paths).unwrap_err().to_string();
    assert!(error.contains("keyring"), "{error}");
    assert!(!error.contains("corrupt"), "{error}");
    Ok(())
}

#[test]
fn activate_writes_file_credentials_and_rejects_keyring_mode() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("codex"),
    };
    fs::create_dir_all(&paths.codex_home)?;
    let stored = account("a@b.com", "workspace");
    cx::auth::activate(&paths, &stored)?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(
            paths.codex_home.join("auth.json")
        )?)?,
        stored.data
    );
    fs::write(
        paths.codex_home.join("config.toml"),
        "cli_auth_credentials_store = \"keyring\"\n",
    )?;
    assert!(cx::auth::activate(&paths, &stored).is_err());
    Ok(())
}

#[cfg(unix)]
#[test]
fn failed_isolated_login_preserves_real_codex_home() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir()?;
    let paths = Paths {
        data: temp.path().join("cx"),
        codex_home: temp.path().join("real-codex"),
    };
    fs::create_dir_all(&paths.codex_home)?;
    fs::write(paths.codex_home.join("auth.json"), b"original")?;
    let fake = temp.path().join("codex");
    fs::write(
        &fake,
        "#!/bin/sh\ntest \"$CODEX_HOME\" != '".to_owned()
            + &paths.codex_home.display().to_string()
            + "' || exit 9\nexit 7\n",
    )?;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700))?;
    assert!(cx::auth::login(&paths, fake.as_os_str(), false).is_err());
    assert_eq!(fs::read(paths.codex_home.join("auth.json"))?, b"original");
    Ok(())
}

#[test]
fn malformed_refresh_response_does_not_replace_stored_tokens() -> Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let _environment_guard = REFRESH_ENV_LOCK.lock().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}").unwrap();
    });
    let (_temp, store) = store();
    let access = jwt(json!({"exp": 4_000_000_000_i64, "chatgpt_account_id":"workspace"}));
    let mut stored = account("a@b.com", "workspace");
    stored.data = json!({"auth_mode":"chatgpt", "tokens": {
        "access_token": access, "refresh_token":"rotating-secret", "id_token":"id"
    }});
    store.transaction(|state| state.add("one", stored, false))?;
    std::env::set_var(
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        format!("http://{address}"),
    );
    let result = cx::auth::credentials(&store, "one", true);
    std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
    server.join().unwrap();
    assert!(result.is_err());
    assert_eq!(
        store.read()?.accounts["one"]
            .data
            .pointer("/tokens/refresh_token")
            .and_then(serde_json::Value::as_str),
        Some("rotating-secret")
    );
    Ok(())
}

#[test]
fn pinned_credentials_reject_an_alias_replaced_by_another_identity() -> Result<()> {
    let (_temp, store) = store();
    store.transaction(|state| state.add("one", account("new@b.com", "workspace-new"), false))?;
    let error = cx::auth::credentials_for(&store, "one", "old@b.com", "workspace-old", false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("identity changed"), "{error}");
    Ok(())
}

fn oauth_account(
    email: &str,
    account_id: &str,
    access_marker: &str,
    refreshed_at: &str,
) -> Account {
    let access = jwt(json!({
        "exp": 4_000_000_000_i64,
        "email": email,
        "chatgpt_account_id": account_id,
        "marker": access_marker,
    }));
    let id = jwt(json!({"email": email, "chatgpt_account_id": account_id}));
    Account {
        email: email.into(),
        account_id: account_id.into(),
        data: json!({
            "auth_mode":"chatgpt",
            "tokens":{"access_token":access,"id_token":id,"refresh_token":format!("refresh-{access_marker}"),"account_id":account_id},
            "last_refresh":refreshed_at,
        }),
        usage: None,
    }
}

#[test]
fn credentials_import_strictly_newer_matching_live_bundle() -> Result<()> {
    let (_temp, store) = store();
    fs::create_dir_all(&store.paths.codex_home)?;
    let saved = oauth_account("a@b.com", "workspace", "saved", "2026-09-11T10:00:00Z");
    let live = oauth_account("a@b.com", "workspace", "live", "2026-09-11T11:00:00Z");
    store.transaction(|state| state.add("one", saved, false))?;
    fs::write(
        store.paths.codex_home.join("auth.json"),
        serde_json::to_vec(&live.data)?,
    )?;
    let loaded = cx::auth::credentials(&store, "one", false)?;
    assert_eq!(
        loaded
            .data
            .pointer("/tokens/refresh_token")
            .and_then(serde_json::Value::as_str),
        Some("refresh-live")
    );
    assert_eq!(store.read()?.accounts["one"].data, live.data);
    Ok(())
}

#[test]
fn credentials_ignore_stale_or_different_live_bundle() -> Result<()> {
    for live in [
        oauth_account("a@b.com", "workspace", "stale", "2026-09-11T09:00:00Z"),
        oauth_account("other@b.com", "workspace", "other", "2026-09-11T12:00:00Z"),
    ] {
        let (_temp, store) = store();
        fs::create_dir_all(&store.paths.codex_home)?;
        let saved = oauth_account("a@b.com", "workspace", "saved", "2026-09-11T10:00:00Z");
        store.transaction(|state| state.add("one", saved.clone(), false))?;
        fs::write(
            store.paths.codex_home.join("auth.json"),
            serde_json::to_vec(&live.data)?,
        )?;
        let loaded = cx::auth::credentials(&store, "one", false)?;
        assert_eq!(loaded.data, saved.data);
    }
    Ok(())
}

#[test]
fn refreshed_credentials_are_persisted_before_publishing_to_matching_live_file() -> Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let _environment_guard = REFRESH_ENV_LOCK.lock().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let refreshed_access = jwt(json!({
        "exp": 4_000_000_000_i64,
        "email":"a@b.com",
        "chatgpt_account_id":"workspace"
    }));
    let response = serde_json::to_vec(&json!({
        "access_token": refreshed_access,
        "refresh_token":"refresh-rotated"
    }))?;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request).unwrap();
        let headers = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", response.len());
        stream.write_all(headers.as_bytes()).unwrap();
        stream.write_all(&response).unwrap();
    });
    let (_temp, store) = store();
    fs::create_dir_all(&store.paths.codex_home)?;
    let saved = oauth_account("a@b.com", "workspace", "old", "2026-09-11T10:00:00Z");
    store.transaction(|state| state.add("one", saved.clone(), false))?;
    fs::write(
        store.paths.codex_home.join("auth.json"),
        serde_json::to_vec(&saved.data)?,
    )?;
    std::env::set_var(
        "CODEX_REFRESH_TOKEN_URL_OVERRIDE",
        format!("http://{address}"),
    );
    let refreshed = cx::auth::credentials(&store, "one", true);
    std::env::remove_var("CODEX_REFRESH_TOKEN_URL_OVERRIDE");
    server.join().unwrap();
    let refreshed = refreshed?;
    let durable = store.read()?.accounts["one"].data.clone();
    let live: serde_json::Value =
        serde_json::from_slice(&fs::read(store.paths.codex_home.join("auth.json"))?)?;
    assert_eq!(
        refreshed
            .data
            .pointer("/tokens/refresh_token")
            .and_then(serde_json::Value::as_str),
        Some("refresh-rotated")
    );
    assert_eq!(durable, refreshed.data);
    assert_eq!(live, durable);
    Ok(())
}
