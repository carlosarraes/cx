use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::{
    fs,
    process::{Command, Output},
};
use tempfile::TempDir;

fn cli(dir: &TempDir, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cx"));
    cmd.args(args)
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY");
    cmd
}

fn command(dir: &TempDir, args: &[&str]) -> Output {
    cli(dir, args).output().unwrap()
}

fn auth(dir: &TempDir, id: &str) {
    let claims = json!({"email":format!("{id}@example.test"),"exp":4102444800_u64,
        "https://api.openai.com/auth":{"chatgpt_account_id":id,"chatgpt_plan_type":"plus","chatgpt_user_id":id}});
    let jwt = format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    );
    let home = dir.path().join("codex");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "cli_auth_credentials_store = \"file\"\n",
    )
    .unwrap();
    fs::write(
        home.join("auth.json"),
        serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{
        "access_token":jwt,"id_token":jwt,"refresh_token":"secret-refresh", "account_id":id
    },"last_refresh":"2026-09-11T00:00:00Z"}))
        .unwrap(),
    )
    .unwrap();
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn account_commands_toggle_previous_without_repointing_saved_aliases() {
    let dir = TempDir::new().unwrap();
    auth(&dir, "personal");
    success(command(&dir, &["add", "personal", "--current"]));
    auth(&dir, "work");
    success(command(&dir, &["add", "work", "--current"]));
    success(command(&dir, &["switch", "personal"]));
    success(command(&dir, &["switch", "work"]));
    success(command(&dir, &["switch", "-"]));
    let output = success(command(&dir, &["whoami"]));
    assert!(output.contains("personal@example.test"), "{output}");
    let live: Value =
        serde_json::from_slice(&fs::read(dir.path().join("codex/auth.json")).unwrap()).unwrap();
    assert_eq!(live["tokens"]["account_id"], "personal");
    assert!(!output.contains("secret-refresh"));
}

#[test]
fn invalid_alias_and_missing_account_fail_without_printing_credentials() {
    let dir = TempDir::new().unwrap();
    auth(&dir, "personal");
    for args in [
        &["add", "../outside", "--current"][..],
        &["switch", "missing"][..],
    ] {
        let output = command(&dir, args);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-refresh"));
    }
}

#[test]
fn help_exposes_cs_commands_and_managed_run() {
    let dir = TempDir::new().unwrap();
    let help = success(command(&dir, &["--help"]));
    for name in ["add", "switch", "list", "del", "whoami", "refresh", "usage"] {
        assert!(help.contains(name), "{help}");
    }
    assert!(help
        .lines()
        .any(|line| line.trim_start().starts_with("run ")));
}

#[test]
fn corrupt_state_does_not_echo_a_secret_in_a_typed_json_field() {
    let dir = TempDir::new().unwrap();
    let state_dir = dir.path().join("data/cx");
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.json"),
        r#"{"accounts":"secret-refresh-token"}"#,
    )
    .unwrap();
    let output = command(&dir, &["list"]);
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-refresh-token"));
}

#[test]
fn refresh_refuses_to_replace_newer_saved_tokens_with_old_live_tokens() {
    let dir = TempDir::new().unwrap();
    auth(&dir, "personal");
    success(command(&dir, &["add", "personal", "--current"]));
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["accounts"]["personal"]["data"]["last_refresh"] = json!("2026-09-12T00:00:00Z");
    state["accounts"]["personal"]["data"]["tokens"]["refresh_token"] = json!("newer-refresh");
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let output = command(&dir, &["refresh"]);
    assert!(!output.status.success());
    let after: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        after["accounts"]["personal"]["data"]["tokens"]["refresh_token"],
        "newer-refresh"
    );
}

#[test]
fn next_does_not_use_cached_usage_when_the_live_check_fails() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    for alias in ["personal", "work"] {
        auth(&dir, alias);
        success(command(&dir, &["add", alias, "--current"]));
    }
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["accounts"]["personal"]["usage"] = json!({
        "primary": {"used_percent": 0, "resets_at": null},
        "secondary": null, "observed_at": chrono::Utc::now().timestamp(), "allowed": true
    });
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        }
    });
    let output = cli(&dir, &["switch", "next", "--yes"])
        .env("CX_USAGE_URL", endpoint)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        !output.status.success(),
        "a failed live check must not select cached usage"
    );
    let after: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(after["current"], "work");
}

#[test]
fn usage_matches_cs_layout_and_labels_a_weekly_primary_window() {
    let dir = TempDir::new().unwrap();
    for alias in ["carlos", "carraes"] {
        auth(&dir, alias);
        success(command(&dir, &["add", alias, "--current"]));
    }
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let now = chrono::Utc::now().timestamp();
    state["current_since"] = json!(now - 13 * 60 - 10);
    state["last_active_at"] = json!({"carlos": now - 78 * 60 - 10});
    state["accounts"]["carlos"]["usage"] = json!({
        "primary": {"used_percent": 83, "resets_at": now + 5 * 86400 + 18 * 3600 + 30, "limit_window_seconds": 604800},
        "secondary": null, "observed_at": now, "allowed": true
    });
    state["accounts"]["carraes"]["usage"] = json!({
        "primary": {"used_percent": 10, "resets_at": now + 2 * 3600 + 53 * 60 + 30, "limit_window_seconds": 18000},
        "secondary": {"used_percent": 94, "resets_at": now + 86400 + 3 * 3600 + 30, "limit_window_seconds": 604800},
        "observed_at": now, "allowed": true
    });
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(success(command(&dir, &["usage"])),
        "carlos  (0)  7d 83% (resets 5d18h) · idle 1h18m\ncarraes (0)  5h 10% (resets 2h53m) · 7d 94% (resets 1d3h) · running 13m\n");
}

#[test]
fn switching_tracks_selection_times_without_resetting_a_reselected_account() {
    let dir = TempDir::new().unwrap();
    for alias in ["personal", "work"] {
        auth(&dir, alias);
        success(command(&dir, &["add", alias, "--current"]));
    }
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(state["last_active_at"]["personal"].is_i64());
    assert!(state["current_since"].is_i64());
    let earlier = chrono::Utc::now().timestamp() - 3600;
    state["current_since"] = json!(earlier);
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    success(command(&dir, &["switch", "work", "--yes"]));
    let state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(state["current_since"], earlier);
    success(command(&dir, &["switch", "-", "--yes"]));
    let state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert!(state["current_since"].as_i64().unwrap() > earlier);
    assert!(state["last_active_at"]["work"].as_i64().unwrap() > earlier);
    success(command(&dir, &["del", "personal"]));
    let state: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert!(state["current_since"].is_null());
    assert!(state["last_active_at"].get("personal").is_none());
}

#[test]
fn live_usage_preserves_the_server_window_duration() {
    use std::io::{BufRead, BufReader, Write};
    let dir = TempDir::new().unwrap();
    auth(&dir, "carlos");
    success(command(&dir, &["add", "carlos", "--current"]));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let body = json!({"rate_limit": {"allowed": true,
        "primary_window": {"used_percent": 83, "limit_window_seconds": 604800,
            "reset_at": chrono::Utc::now().timestamp() + 5 * 86400 + 18 * 3600 + 30},
        "secondary_window": null}})
    .to_string();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(&mut stream);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
        }
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let output = success(
        cli(&dir, &["usage", "--live"])
            .env("CX_USAGE_URL", endpoint)
            .output()
            .unwrap(),
    );
    server.join().unwrap();
    assert_eq!(output, "carlos (0)  7d 83% (resets 5d18h) · running <1m\n");
    assert_eq!(success(command(&dir, &["usage"])), output);
}

#[test]
fn legacy_usage_keeps_unknown_durations_and_activity_honest() {
    let dir = TempDir::new().unwrap();
    for alias in ["old", "unknown"] {
        auth(&dir, alias);
        success(command(&dir, &["add", alias, "--current"]));
    }
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state.as_object_mut().unwrap().remove("current_since");
    state.as_object_mut().unwrap().remove("last_active_at");
    let now = chrono::Utc::now().timestamp();
    state["accounts"]["old"]["usage"] = json!({
        "primary": {"used_percent": 100, "resets_at": now - 60},
        "secondary": null, "observed_at": now - 190, "allowed": false
    });
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(success(command(&dir, &["usage"])),
        "old     (0)  primary 100% (resets <1m) · [limited] · idle · as of 3m ago\nunknown (0)  ?? usage unknown (run `cx usage --live`) · running\n");
}

#[test]
fn reset_spends_the_soonest_expiring_credit_after_confirmation() {
    use std::io::{BufRead, BufReader, Read, Write};
    let dir = TempDir::new().unwrap();
    auth(&dir, "carlos");
    success(command(&dir, &["add", "carlos", "--current"]));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let path = request_line.split_whitespace().nth(1).unwrap().to_string();
            let reply = match path.as_str() {
                "/resets" => json!({"available_count": 2, "credits": [
                    {"id": "late", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "2026-11-12T09:30:00Z"},
                    {"id": "soon", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-10-01T00:00:00Z", "expires_at": "2026-11-07T14:00:00Z"}
                ]}),
                "/resets/consume" => json!({"code": "reset", "windows_reset": 1}),
                _ => json!({"rate_limit": {"allowed": true,
                    "primary_window": {"used_percent": 0, "limit_window_seconds": 18000, "reset_at": null},
                    "secondary_window": null}}),
            }
            .to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).unwrap();
            requests.push((path, body));
        }
        requests
    });
    let run = |args: &[&str]| {
        cli(&dir, args)
            .env("CX_RESETS_URL", format!("{endpoint}/resets"))
            .env("CX_USAGE_URL", format!("{endpoint}/usage"))
            .env("TZ", "UTC")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let refused = run(&["reset"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yes"));
    let output = success(run(&["reset", "--yes"]));
    assert_eq!(
        output,
        "Reset 'carlos' with #1 expires 07 Nov 14:00 (1 window(s) reset).\ncarlos (0)  5h 0% · running <1m\n"
    );
    let requests = server.join().unwrap();
    let paths: Vec<_> = requests.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(paths, ["/resets", "/resets", "/resets/consume", "/usage"]);
    let consume: Value = serde_json::from_slice(&requests[2].1).unwrap();
    assert_eq!(consume["credit_id"], "soon");
    assert_eq!(consume["redeem_request_id"].as_str().unwrap().len(), 36);
}

#[test]
fn passthrough_fallback_preserves_arguments_stdio_and_alias_dispatch() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = TempDir::new().unwrap();
    auth(&dir, "current");
    let auth_path = dir.path().join("codex/auth.json");
    let before = fs::read(&auth_path).unwrap();
    let modified = fs::metadata(&auth_path).unwrap().modified().unwrap();
    let stub = dir.path().join("codex-stub");
    fs::write(
        &stub,
        "#!/bin/sh\nprintf 'MARKER\\n'\nprintf '%s\\n' \"$CODEX_HOME\" \"$@\"\n",
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();
    let output = cli(&dir, &["t3-codex", "app-server", "-c", "foo=bar"])
        .env("CX_CODEX_BIN", &stub)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "MARKER\n{}\napp-server\n-c\nfoo=bar\n",
            dir.path().join("codex").display()
        )
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("fallback"));
    let alias = dir.path().join("cx-t3-codex");
    symlink(env!("CARGO_BIN_EXE_cx"), &alias).unwrap();
    let output = Command::new(alias)
        .arg("app-server")
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .env("CX_CODEX_BIN", stub)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.starts_with(b"MARKER\n"));
    assert_eq!(success(command(&dir, &["pick"])), "current\n");
    assert_eq!(fs::read(&auth_path).unwrap(), before);
    assert_eq!(
        fs::metadata(auth_path).unwrap().modified().unwrap(),
        modified
    );
}

#[test]
fn routed_passthrough_isolates_credentials_and_matches_pick() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    for alias in ["a", "b"] {
        auth(&dir, alias);
        success(command(&dir, &["add", alias, "--current"]));
    }
    let path = dir.path().join("data/cx/state.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let now = chrono::Utc::now().timestamp();
    for alias in ["a", "b"] {
        state["accounts"][alias]["usage"] = json!({
            "primary":{"used_percent":10,"resets_at":now+3600,"limit_window_seconds":18000},
            "secondary":null,"observed_at":now,"allowed":true
        });
    }
    fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    let paths = cx::state::Paths {
        data: dir.path().join("data/cx"),
        codex_home: dir.path().join("codex"),
    };
    let reserved = cx::routing::reserve(
        &paths,
        &cx::routing::RunOptions::default(),
        None,
        cx::sessions::Mode::Interactive,
    )
    .unwrap();
    assert_eq!(reserved.get().unwrap().account.alias, "a");
    assert_eq!(success(command(&dir, &["pick"])), "b b@example.test\n");
    let auth_path = paths.codex_home.join("auth.json");
    let before = fs::read(&auth_path).unwrap();
    let modified = fs::metadata(&auth_path).unwrap().modified().unwrap();
    let stub = dir.path().join("stub");
    fs::write(
        &stub,
        r#"#!/usr/bin/env python3
import os, pathlib, stat
home = pathlib.Path(os.environ["CODEX_HOME"])
assert stat.S_IMODE(home.stat().st_mode) == 0o700
assert stat.S_IMODE((home / "auth.json").stat().st_mode) == 0o600
assert 'cli_auth_credentials_store = "file"' in (home / "config.toml").read_text()
print("MARKER", flush=True)
print(home)
"#,
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();
    let output = cli(&dir, &["t3-codex", "app-server"])
        .env("CX_CODEX_BIN", stub)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("-> b (b@example.test)"));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(lines[0], "MARKER");
    assert_ne!(std::path::Path::new(lines[1]), paths.codex_home);
    assert!(!std::path::Path::new(lines[1]).exists());
    assert_eq!(fs::read(&auth_path).unwrap(), before);
    assert_eq!(
        fs::metadata(auth_path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(cx::sessions::Registry::new(&paths).list().unwrap().len(), 1);
}
