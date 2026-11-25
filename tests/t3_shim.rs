use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use cx::state::{Account, Paths, Store};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    process::{Command, Stdio},
};
use tempfile::TempDir;

fn isolated_command(dir: &TempDir, binary: &std::path::Path) -> Command {
    let mut cmd = Command::new(binary);
    cmd.env("HOME", dir.path())
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("XDG_CONFIG_HOME", dir.path().join("config"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .env_remove("CX_BORROW")
        .env("CX_USAGE_URL", "http://127.0.0.1:9")
        .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", "http://127.0.0.1:9");
    cmd
}

fn command(dir: &TempDir) -> Command {
    let mut cmd = isolated_command(dir, std::path::Path::new(env!("CARGO_BIN_EXE_cx")));
    cmd.arg("t3-codex");
    cmd
}

fn stub(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("fake-codex");
    fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$$\" \"$CODEX_HOME\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn paths(dir: &TempDir) -> Paths {
    Paths {
        data: dir.path().join("data/cx"),
        codex_home: dir.path().join("codex"),
    }
}

fn save(dir: &TempDir, alias: &str) {
    save_expiring(dir, alias, 1);
}

fn save_expiring(dir: &TempDir, alias: &str, exp: i64) {
    let claims = json!({"email":format!("{alias}@example.test"), "exp":exp,
        "https://api.openai.com/auth":{"chatgpt_account_id":alias}});
    let token = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    Store::new(paths(dir)).transaction(|state| state.add(alias, Account {
        email:format!("{alias}@example.test"), account_id:alias.into(), usage:None, login_revoked_at:None,
        data:json!({"auth_mode":"chatgpt", "tokens":{"access_token":token,"id_token":token,
            "refresh_token":"synthetic-stored-refresh", "account_id":alias}, "last_refresh":"2026-01-01T00:00:00Z"})
    }, false)).unwrap();
}

fn launch(dir: &TempDir) -> (u32, String) {
    let child = command(dir)
        .args(["app-server", "-c", "foo=bar"])
        .env("CX_CODEX_BIN", stub(dir))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    (pid, String::from_utf8(output.stdout).unwrap())
}

#[test]
fn no_account_execs_at_the_pid_t3_started_without_an_overlay() {
    let dir = TempDir::new().unwrap();
    let (pid, output) = launch(&dir);
    assert_eq!(
        output,
        format!(
            "{pid}\n{}\napp-server\n-c\nfoo=bar\n",
            paths(&dir).codex_home.display()
        )
    );
    assert!(!paths(&dir).data.exists());
}

#[test]
fn expired_credentials_and_missing_usage_still_exec_locally_into_a_stable_overlay() {
    let dir = TempDir::new().unwrap();
    save(&dir, "work");
    fs::create_dir_all(paths(&dir).codex_home.join("sessions")).unwrap();
    fs::write(
        paths(&dir).codex_home.join("config.toml"),
        "model = 'test-model'\n",
    )
    .unwrap();
    let (pid, output) = launch(&dir);
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines[0], pid.to_string());
    let overlay = std::path::Path::new(lines[1]);
    assert!(overlay.starts_with(paths(&dir).data));
    assert!(overlay.exists());
    assert!(overlay.join("sessions").is_symlink());
    assert_eq!(
        fs::metadata(overlay).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(overlay.join("auth.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(fs::read_to_string(overlay.join("config.toml"))
        .unwrap()
        .contains("test-model"));
    let (_, second) = launch(&dir);
    assert_eq!(second.lines().nth(1), Some(lines[1]));
    assert!(!paths(&dir).codex_home.join("auth.json").exists());
    Store::new(paths(&dir))
        .transaction(|state| state.delete("work"))
        .unwrap();
    save(&dir, "other");
    let (_, third) = launch(&dir);
    assert_ne!(third.lines().nth(1), Some(lines[1]));
}

#[test]
fn t3_expands_literal_tilde_in_codex_home_even_without_saved_accounts() {
    let dir = TempDir::new().unwrap();
    let output = command(&dir)
        .arg("--version")
        .env("CODEX_HOME", "~/custom-codex")
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout.lines().nth(1),
        dir.path().join("custom-codex").to_str()
    );
    assert!(!dir.path().join("custom-codex").exists());
}

#[test]
fn overlay_sync_requires_matching_identity_and_a_strictly_newer_timestamp() {
    let dir = TempDir::new().unwrap();
    save(&dir, "work");
    let store = Store::new(paths(&dir));
    let original = store.read().unwrap().accounts["work"].clone();
    let (_, output) = launch(&dir);
    let home = std::path::Path::new(output.lines().nth(1).unwrap());
    for (timestamp, identity) in [
        ("2010-01-01T00:00:00Z", "work"),
        ("2026-01-01T00:00:00Z", "work"),
        ("2099-01-01T00:00:00Z", "other"),
    ] {
        let mut changed = original.data.clone();
        changed["last_refresh"] = json!(timestamp);
        changed["tokens"]["access_token"] = json!("untrusted-overlay-access");
        if identity == "other" {
            let claims = json!({"email":"other@example.test", "https://api.openai.com/auth":{"chatgpt_account_id":"other"}});
            changed["tokens"]["id_token"] = json!(format!(
                "e30.{}.sig",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            ));
            changed["tokens"]["account_id"] = json!("other");
        }
        fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        launch(&dir);
        assert!(store.read().unwrap().accounts["work"].data == original.data);
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(home.join("auth.json")).unwrap()).unwrap();
        assert!(rewritten == original.data);
    }
    let mut newer = original.data.clone();
    newer["last_refresh"] = json!("2099-01-01T00:00:00Z");
    newer["tokens"]["access_token"] = json!("updated-overlay-access");
    newer["tokens"]["refresh_token"] = json!("synthetic-unstored-refresh");
    fs::write(home.join("auth.json"), serde_json::to_vec(&newer).unwrap()).unwrap();
    launch(&dir);
    let saved = store.read().unwrap();
    assert!(
        saved.accounts["work"].data["tokens"]["access_token"] == newer["tokens"]["access_token"]
    );
    assert!(
        saved.accounts["work"].data["tokens"]["refresh_token"]
            == original.data["tokens"]["refresh_token"]
    );
}

#[test]
fn overlay_directory_symlinks_cannot_redirect_writes_into_shared_home() {
    for symlink_root in [false, true] {
        let dir = TempDir::new().unwrap();
        save(&dir, "work");
        let shared = paths(&dir).codex_home;
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join("auth.json"), "shared sentinel").unwrap();
        fs::write(shared.join("config.toml"), "model = 'shared-sentinel'\n").unwrap();
        let root = paths(&dir).data.join("t3-overlays");
        if symlink_root {
            symlink(&shared, &root).unwrap();
        } else {
            fs::create_dir(&root).unwrap();
            symlink(&shared, root.join("work")).unwrap();
        }
        let output = command(&dir)
            .arg("app-server")
            .env("CX_CODEX_BIN", stub(&dir))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert_eq!(
            fs::read_to_string(shared.join("auth.json")).unwrap(),
            "shared sentinel"
        );
        assert_eq!(
            fs::read_to_string(shared.join("config.toml")).unwrap(),
            "model = 'shared-sentinel'\n"
        );
    }
}

#[test]
fn overlay_local_data_conflicts_fail_without_discarding_either_history() {
    let dir = TempDir::new().unwrap();
    save(&dir, "work");
    let (_, output) = launch(&dir);
    let home = std::path::Path::new(output.lines().nth(1).unwrap());
    fs::create_dir(home.join("sessions")).unwrap();
    fs::write(home.join("sessions/local-history"), "local").unwrap();
    fs::create_dir_all(paths(&dir).codex_home.join("sessions")).unwrap();
    fs::write(
        paths(&dir).codex_home.join("sessions/shared-history"),
        "shared",
    )
    .unwrap();
    let output = command(&dir)
        .arg("app-server")
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("overlay has local data"));
    assert_eq!(
        fs::read_to_string(home.join("sessions/local-history")).unwrap(),
        "local"
    );
    assert_eq!(
        fs::read_to_string(paths(&dir).codex_home.join("sessions/shared-history")).unwrap(),
        "shared"
    );
}

#[test]
fn pin_to_revoked_login_fails_fast_without_starting_codex() {
    let dir = TempDir::new().unwrap();
    save(&dir, "dead");
    save(&dir, "live");
    Store::new(paths(&dir))
        .transaction(|state| {
            state.accounts.get_mut("dead").unwrap().login_revoked_at = Some(1);
            Ok(())
        })
        .unwrap();
    let output = command(&dir)
        .arg("app-server")
        .env("CX_PIN", "dead")
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(21));
    assert!(output.stdout.is_empty(), "Codex must not start");
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("dead: login revoked; run `cx add dead --device-auth --force`; no fallback"));
    let (_, unpinned) = launch(&dir);
    assert!(unpinned.lines().nth(1).unwrap().ends_with("/live"));
}

#[test]
fn stale_cache_choice_is_probed_live_and_a_revoked_login_is_skipped() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    for alias in ["alpha", "beta"] {
        save_expiring(&dir, alias, 4_102_444_800);
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut account = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if let Some(v) = line
                    .to_ascii_lowercase()
                    .strip_prefix("chatgpt-account-id:")
                {
                    account = v.trim().to_string();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let (status, body) = if account == "alpha" {
                (
                    "401 Unauthorized",
                    json!({"error":{"code":"token_revoked"}}),
                )
            } else {
                (
                    "200 OK",
                    json!({"rate_limit":{"allowed":true,
                    "primary_window":{"used_percent":5,"limit_window_seconds":18000,"reset_at":null},
                    "secondary_window":null}}),
                )
            };
            let body = body.to_string();
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            seen.push(account);
        }
        seen
    });
    let output = command(&dir)
        .arg("app-server")
        .env("CX_USAGE_URL", endpoint)
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert_eq!(server.join().unwrap(), ["alpha", "beta"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("alpha: login revoked"), "{stderr}");
    assert!(stderr.contains("t3-codex -> beta"), "{stderr}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.lines().nth(1).unwrap().ends_with("/beta"));
    let state = Store::new(paths(&dir)).read().unwrap();
    assert!(state.accounts["alpha"].revoked());
    assert!(state.accounts["beta"].usage.is_some());
}

/// Accepts connections and never answers, like a blackholed backend.
fn hanging_listener() -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/x", listener.local_addr().unwrap());
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            held.push(stream);
        }
    });
    (endpoint, accepted)
}

fn concurrent_versions(dir: &TempDir, endpoint: &str, spawns: usize) -> Vec<std::time::Duration> {
    let start = std::time::Instant::now();
    let children: Vec<_> = (0..spawns)
        .map(|_| {
            command(dir)
                .arg("--version")
                .env("CX_USAGE_URL", endpoint)
                .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", endpoint)
                .env("CX_CODEX_BIN", stub(dir))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            start.elapsed()
        })
        .collect()
}

#[test]
fn concurrent_spawns_against_a_hanging_backend_stay_within_the_probe_budget() {
    let budget = std::time::Duration::from_millis(4000 + 1500);
    for (exp, max_connections) in [
        (4_102_444_800_i64, 1),
        (chrono::Utc::now().timestamp() + 30, 0),
    ] {
        let dir = TempDir::new().unwrap();
        for alias in ["a", "b", "c"] {
            save_expiring(&dir, alias, exp);
        }
        let (endpoint, accepted) = hanging_listener();
        let times = concurrent_versions(&dir, &endpoint, 8);
        let slowest = times.iter().max().unwrap();
        assert!(
            slowest < &budget,
            "exp={exp}: slowest spawn took {slowest:?}"
        );
        let connections = accepted.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            connections <= max_connections,
            "exp={exp}: {connections} spawns hit the backend; probing must be single-flight"
        );
        // A recent unreachable probe makes the next spawn skip the network entirely.
        let again = concurrent_versions(&dir, &endpoint, 1);
        assert!(
            again[0] < std::time::Duration::from_secs(1),
            "{:?}",
            again[0]
        );
        assert_eq!(
            accepted.load(std::sync::atomic::Ordering::SeqCst),
            connections
        );
    }
}

fn set_usage(dir: &TempDir, alias: &str, usage: serde_json::Value) {
    Store::new(paths(dir))
        .transaction(|state| {
            state.accounts.get_mut(alias).unwrap().usage =
                Some(serde_json::from_value(usage).unwrap());
            Ok(())
        })
        .unwrap();
}

#[test]
fn stale_limited_account_is_skipped_and_all_unusable_fails_with_one_line() {
    let dir = TempDir::new().unwrap();
    save(&dir, "alpha");
    save(&dir, "beta");
    // 2030-01-01T00:00:00Z: alpha reported 7d 100% hours ago, reset still ahead.
    let reset = 1_893_456_000_i64;
    set_usage(
        &dir,
        "alpha",
        json!({"primary": null, "observed_at": 1_000,
        "secondary": {"used_percent": 100, "resets_at": reset, "limit_window_seconds": 604800},
        "allowed": false}),
    );
    let (_, output) = launch(&dir);
    assert!(
        output.lines().nth(1).unwrap().ends_with("/beta"),
        "{output}"
    );

    Store::new(paths(&dir))
        .transaction(|state| {
            state.accounts.get_mut("beta").unwrap().login_revoked_at = Some(1);
            Ok(())
        })
        .unwrap();
    let expected = "error: no usable account: alpha limited until 2030-01-01 00:00 +00, \
beta revoked (run `cx add beta --device-auth --force`)\n";
    let shim = command(&dir)
        .arg("app-server")
        .env("TZ", "UTC")
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert_eq!(shim.status.code(), Some(20));
    assert!(shim.stdout.is_empty(), "Codex must not start");
    assert_eq!(String::from_utf8_lossy(&shim.stderr), expected);
    let pick = isolated_command(&dir, std::path::Path::new(env!("CARGO_BIN_EXE_cx")))
        .arg("pick")
        .env_remove("CX_PIN")
        .env("TZ", "UTC")
        .output()
        .unwrap();
    assert_eq!(pick.status.code(), Some(20));
    assert_eq!(String::from_utf8_lossy(&pick.stderr), expected);

    // A pin keeps exec-ing on the limited account, with a warning.
    let pinned = command(&dir)
        .arg("app-server")
        .env("TZ", "UTC")
        .env("CX_PIN", "alpha")
        .env("CX_CODEX_BIN", stub(&dir))
        .output()
        .unwrap();
    assert!(pinned.status.success());
    assert!(String::from_utf8_lossy(&pinned.stderr)
        .contains("cx: pinned alpha is limited until 2030-01-01 00:00 +00"));
}

#[test]
fn limited_account_becomes_pickable_once_its_reset_has_passed() {
    let dir = TempDir::new().unwrap();
    save(&dir, "alpha");
    save(&dir, "beta");
    for alias in ["alpha", "beta"] {
        set_usage(
            &dir,
            alias,
            json!({"primary": null, "observed_at": 1_000,
            "secondary": {"used_percent": 100, "resets_at": 2_000, "limit_window_seconds": 604800},
            "allowed": false}),
        );
    }
    let (_, output) = launch(&dir);
    assert!(
        output.lines().nth(1).unwrap().ends_with("/alpha"),
        "{output}"
    );
}
