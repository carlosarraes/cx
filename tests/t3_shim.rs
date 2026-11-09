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
    let claims = json!({"email":format!("{alias}@example.test"), "exp":1,
        "https://api.openai.com/auth":{"chatgpt_account_id":alias}});
    let token = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    Store::new(paths(dir)).transaction(|state| state.add(alias, Account {
        email:format!("{alias}@example.test"), account_id:alias.into(), usage:None,
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
