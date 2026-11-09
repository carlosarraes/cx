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
    auth_exp(dir, id, 4102444800);
}

fn auth_exp(dir: &TempDir, id: &str, exp: u64) {
    let jwt = jwt_with(id, exp);
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

fn jwt_for(id: &str) -> String {
    jwt_with(id, 4102444800)
}

fn jwt_with(id: &str, exp: u64) -> String {
    let claims = json!({"email":format!("{id}@example.test"),"exp":exp,
        "https://api.openai.com/auth":{"chatgpt_account_id":id,"chatgpt_plan_type":"plus","chatgpt_user_id":id}});
    format!(
        "e30.{}.sig",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
    )
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
        .env("CX_CODEX_BIN", &stub)
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
fn routed_passthrough_reuses_overlay_and_syncs_newer_credentials_before_exec() {
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
import json, os, pathlib, signal, stat
home = pathlib.Path(os.environ["CODEX_HOME"])
assert stat.S_IMODE(home.stat().st_mode) == 0o700
assert stat.S_IMODE((home / "auth.json").stat().st_mode) == 0o600
assert 'cli_auth_credentials_store = "file"' in (home / "config.toml").read_text()
print("MARKER", flush=True)
print(home, flush=True)
auth_path = home / "auth.json"
blob = json.loads(auth_path.read_text())
blob["tokens"]["access_token"] = "updated-access"
blob["last_refresh"] = "2099-01-01T00:00:00Z"
auth_path.write_text(json.dumps(blob))
if os.environ.get("FAKE_EXIT") == "signal":
    os.kill(os.getpid(), signal.SIGTERM)
raise SystemExit(int(os.environ.get("FAKE_EXIT", "0")))
"#,
    )
    .unwrap();
    fs::set_permissions(&stub, fs::Permissions::from_mode(0o700)).unwrap();
    let output = cli(&dir, &["t3-codex", "app-server"])
        .env("CX_CODEX_BIN", &stub)
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
    let overlay = std::path::PathBuf::from(lines[1]);
    assert!(overlay.exists());
    assert_eq!(fs::read(&auth_path).unwrap(), before);
    assert_eq!(
        fs::metadata(&auth_path).unwrap().modified().unwrap(),
        modified
    );
    let saved = cx::state::Store::new(paths.clone()).read().unwrap();
    assert!(saved.accounts["b"].data["tokens"]["access_token"] != "updated-access");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("updated-access"));
    assert!(!stderr.contains("secret-refresh"));
    for (exit, code) in [("7", Some(7)), ("signal", None)] {
        let output = cli(&dir, &["t3-codex", "app-server"])
            .env("CX_CODEX_BIN", &stub)
            .env("FAKE_EXIT", exit)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), code);
        if code.is_none() {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(output.status.signal(), Some(libc::SIGTERM));
        }
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().nth(1), overlay.to_str());
        let saved = cx::state::Store::new(paths.clone()).read().unwrap();
        assert!(saved.accounts["b"].data["tokens"]["access_token"] == "updated-access");
        assert_eq!(fs::read(&auth_path).unwrap(), before);
    }
    assert_eq!(cx::sessions::Registry::new(&paths).list().unwrap().len(), 1);
}

#[test]
fn borrowed_overlay_is_private_and_released() {
    use std::os::unix::fs::PermissionsExt;
    for code in [0, 7, 143] {
        let dir = TempDir::new().unwrap();
        auth(&dir, "local");
        let shared = dir.path().join("codex/auth.json");
        let before = fs::read(&shared).unwrap();
        let ssh = dir.path().join("ssh");
        fs::write(&ssh, r#"#!/bin/sh
printf '%s\n' "$@" >> "$RECORD"
case "$*" in
*lend-release*) exit 0;;
esac
printf '{"alias":"owner","email":"owner@example.test","account_id":"owner","lease_id":"lease-1","expires_at":4102444800,"access_token":"test-access-aaa","id_token":"@ID@"}'
"#.replace("@ID@", &jwt_for("owner"))).unwrap();
        let codex = dir.path().join("codex-stub");
        fs::write(
            &codex,
            r#"#!/usr/bin/env python3
import os, pathlib, json, sys, signal
p = pathlib.Path(os.environ["CODEX_HOME"])
assert p.stat().st_mode & 0o777 == 0o700
a = p / "auth.json"
assert a.stat().st_mode & 0o777 == 0o600
v = json.loads(a.read_text())
assert v["tokens"]["access_token"] == "test-access-aaa"
assert v["tokens"]["id_token"].count(".") == 2
assert v["tokens"]["refresh_token"] == ""
assert v["tokens"]["account_id"] == "owner"
assert v["auth_mode"] == "chatgpt" and v["last_refresh"]
assert 'cli_auth_credentials_store = "file"' in (p / "config.toml").read_text()
assert sys.argv[1:] == ["app-server"]
pathlib.Path(os.environ["OVERLAY"]).write_text(str(p))
print("MARKER", flush=True)
code = int(os.environ["CHILD_CODE"])
if code == 143: os.kill(os.getpid(), signal.SIGTERM)
sys.exit(code)
"#,
        )
        .unwrap();
        for p in [&ssh, &codex] {
            fs::set_permissions(p, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let record = dir.path().join("record");
        let overlay = dir.path().join("overlay");
        let out = cli(&dir, &["run", "--borrow=owner", "--", "app-server"])
            .env("CX_SSH", ssh)
            .env("CX_CODEX_BIN", codex)
            .env("RECORD", &record)
            .env("OVERLAY", &overlay)
            .env("CHILD_CODE", code.to_string())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(code));
        assert!(out.stdout.starts_with(b"MARKER"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("test-access-aaa"));
        assert!(!stderr.contains("secret-refresh"));
        assert_eq!(fs::read(shared).unwrap(), before);
        assert!(!dir.path().join("data/cx/state.json").exists());
        assert!(!std::path::Path::new(&fs::read_to_string(overlay).unwrap()).exists());
        let calls = fs::read_to_string(record).unwrap();
        assert!(calls.contains("BatchMode=yes"));
        assert!(calls.contains("ControlPath=none"));
        assert!(calls.contains("lend-release 'lease-1'"));
    }
}

#[test]
fn lend_survives_owner_exit_and_release_is_idempotent() {
    let dir = TempDir::new().unwrap();
    assert!(!command(&dir, &["lend"]).status.success());
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
    let out = command(&dir, &["lend"]);
    assert!(out.status.success());
    let grant: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(grant.get("refresh_token").is_none());
    let id_token = grant["id_token"].as_str().unwrap();
    assert_eq!(id_token.split('.').count(), 3);
    assert_eq!(id_token, jwt_for("a"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(id_token));
    assert!(!String::from_utf8_lossy(&out.stderr).contains(grant["access_token"].as_str().unwrap()));
    assert_eq!(success(command(&dir, &["pick"])), "b b@example.test\n");
    let id = grant["lease_id"].as_str().unwrap();
    success(command(&dir, &["lend-release", id]));
    success(command(&dir, &["lend-release", id]));
    assert_eq!(success(command(&dir, &["pick"])), "a a@example.test\n");
}

#[test]
fn invalid_lend_response_does_not_echo_credentials() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let ssh = dir.path().join("ssh");
    fs::write(&ssh, "#!/bin/sh\nprintf 'test-access-aaa invalid json'\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let out = cli(&dir, &["pick", "--borrow", "owner"])
        .env("CX_SSH", ssh)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid lend response from owner"));
    assert!(!stderr.contains("test-access-aaa"));
}

mod borrow_flow {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn set_usage(dir: &TempDir, alias: &str, allowed: bool) {
        let path = dir.path().join("data/cx/state.json");
        let mut state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let now = chrono::Utc::now().timestamp();
        state["accounts"][alias]["usage"] = json!({
            "primary":{"used_percent":10,"resets_at":now+3600,"limit_window_seconds":18000},
            "secondary":null,"observed_at":now,"allowed":allowed
        });
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    }

    fn owner_with(aliases: &[&str]) -> TempDir {
        let owner = TempDir::new().unwrap();
        for alias in aliases {
            auth(&owner, alias);
            success(command(&owner, &["add", alias, "--current"]));
        }
        owner
    }

    /// CX_SSH stand-in that runs the remote command against a real owner-side cx.
    struct Fake {
        dir: std::path::PathBuf,
        record: std::path::PathBuf,
    }
    impl Fake {
        fn new(borrower: &TempDir, owner: &TempDir) -> Self {
            let dir = borrower.path().join("fake-bin");
            fs::create_dir_all(&dir).unwrap();
            let bin = owner.path().join("home/.local/bin");
            fs::create_dir_all(&bin).unwrap();
            symlink(env!("CARGO_BIN_EXE_cx"), bin.join("cx")).unwrap();
            let script = dir.join("ssh");
            fs::write(
                &script,
                format!(
                    "#!/bin/sh\nfor last; do :; done\nprintf '%s\\n' \"$last\" >> \"{}\"\nexport HOME=\"{}\" XDG_DATA_HOME=\"{}\" CODEX_HOME=\"{}\"\nexec sh -c \"$last\"\n",
                    borrower.path().join("ssh-record").display(),
                    owner.path().join("home").display(),
                    owner.path().join("data").display(),
                    owner.path().join("codex").display(),
                ),
            )
            .unwrap();
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
            Fake {
                dir,
                record: borrower.path().join("ssh-record"),
            }
        }
        fn apply<'a>(&self, cmd: &'a mut Command) -> &'a mut Command {
            let path = format!(
                "{}:{}",
                self.dir.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            cmd.env("CX_SSH", self.dir.join("ssh")).env("PATH", path)
        }
        fn calls(&self) -> Vec<String> {
            fs::read_to_string(&self.record)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }
    }

    fn codex_stub(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("codex-stub");
        fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" \"$CODEX_HOME\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn t3(dir: &TempDir, fake: &Fake) -> Command {
        let mut cmd = cli(dir, &["t3-codex", "app-server"]);
        cmd.env("HOME", dir.path())
            .env("XDG_CONFIG_HOME", dir.path().join("config"))
            .env("CX_BORROW", "owner")
            .env("CX_CODEX_BIN", codex_stub(dir))
            .env("CX_USAGE_URL", "http://127.0.0.1:9")
            .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", "http://127.0.0.1:9");
        fake.apply(&mut cmd);
        cmd
    }

    fn home_of(output: &Output) -> std::path::PathBuf {
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        std::path::PathBuf::from(stdout.lines().nth(1).unwrap())
    }

    fn refresh_server(reply: String) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buffer = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if buffer.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        (url, hits)
    }

    #[test]
    fn lend_renews_only_the_account_it_hands_out() {
        for explicit in [true, false] {
            let owner = TempDir::new().unwrap();
            for alias in ["a", "b"] {
                auth_exp(&owner, alias, 1_000);
                success(command(&owner, &["add", alias, "--current"]));
            }
            let state = owner.path().join("data/cx/state.json");
            let before: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
            let reply = json!({"access_token": jwt_for("a"), "id_token": jwt_for("a"), "refresh_token": "rotated-refresh"}).to_string();
            let (url, hits) = refresh_server(reply);
            let args: &[&str] = if explicit {
                &["lend", "--alias", "a"]
            } else {
                &["lend"]
            };
            let out = cli(&owner, args)
                .env("CODEX_REFRESH_TOKEN_URL_OVERRIDE", url)
                .output()
                .unwrap();
            let stdout = success(out);
            let grant: Value = serde_json::from_str(&stdout).unwrap();
            assert_eq!(grant["alias"], "a");
            assert_eq!(grant["access_token"], jwt_for("a"));
            assert_eq!(
                hits.load(Ordering::SeqCst),
                1,
                "only the lent alias refreshes"
            );
            let after: Value = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
            assert_eq!(after["accounts"]["b"], before["accounts"]["b"]);
            assert_ne!(after["accounts"]["a"], before["accounts"]["a"]);
        }
    }

    #[test]
    fn inventory_lists_usable_aliases_without_any_token() {
        let owner = owner_with(&["a", "b", "c"]);
        set_usage(&owner, "a", true);
        set_usage(&owner, "b", false);
        let stdout = success(command(&owner, &["lend", "--inventory"]));
        let value: Value = serde_json::from_str(&stdout).unwrap();
        let aliases: Vec<_> = value["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["alias"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            aliases,
            ["a", "c"],
            "fresh exhausted account is not offered"
        );
        assert_eq!(value["accounts"][0]["email"], "a@example.test");
        assert_eq!(value["accounts"][0]["expires_at"], 4102444800_u64);
        assert!(value["accounts"][0]["usage"]["allowed"].as_bool().unwrap());
        for needle in [
            "access_token",
            "id_token",
            "refresh_token",
            "secret-refresh",
            &jwt_for("a"),
        ] {
            assert!(!stdout.contains(needle), "inventory leaked {needle}");
        }
    }

    #[test]
    fn borrow_refresh_caches_new_owner_accounts_privately_without_refresh_tokens() {
        let owner = owner_with(&["a", "b"]);
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(&dir, &owner);
        let run = |dir: &TempDir| {
            let mut cmd = cli(dir, &["borrow", "refresh", "--from", "owner"]);
            fake.apply(&mut cmd);
            cmd.output().unwrap()
        };
        let stdout = success(run(&dir));
        assert!(stdout.contains("a  expires 2100-"), "{stdout}");
        assert!(stdout.contains("b  expires 2100-"), "{stdout}");
        assert!(!stdout.contains("eyJ") && !stdout.contains("e30."));
        let cache = dir.path().join("data/cx/borrow/owner.json");
        let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o600);
        assert_eq!(mode(cache.parent().unwrap()), 0o700);
        let text = fs::read_to_string(&cache).unwrap();
        assert!(!text.contains("refresh_token") && !text.contains("secret-refresh"));
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["accounts"]["a"]["id_token"], jwt_for("a"));

        // A newly added owner account is picked up; valid ones are not lent again.
        auth(&owner, "c");
        success(command(&owner, &["add", "c", "--current"]));
        let before = fake.calls().len();
        let stdout = success(run(&dir));
        assert!(stdout.contains("c  expires"), "{stdout}");
        let calls = fake.calls();
        let new_calls = &calls[before..];
        assert_eq!(new_calls.len(), 2, "{new_calls:?}");
        assert!(new_calls[0].ends_with("exec cx lend --inventory"));
        assert!(new_calls[1].contains("lend --no-lease --alias 'c'"));
    }

    #[test]
    fn borrow_refresh_hops_through_the_jump_host() {
        let owner = owner_with(&["a"]);
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(&dir, &owner);
        let mut cmd = cli(
            &dir,
            &["borrow", "refresh", "--from", "owner", "--via", "jump"],
        );
        fake.apply(&mut cmd);
        success(cmd.output().unwrap());
        let calls = fake.calls();
        assert!(
            calls[0].starts_with(
                "ssh -o BatchMode=yes -o ControlPath=none -o ConnectTimeout=10 owner 'export PATH="
            ),
            "{calls:?}"
        );
        assert!(calls[1].starts_with("export PATH="), "{calls:?}");
    }

    #[test]
    fn borrow_hosts_are_validated_before_ssh() {
        let owner = owner_with(&["a"]);
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(&dir, &owner);
        for args in [
            &["borrow", "refresh", "--from", "-oProxyCommand=evil"][..],
            &[
                "borrow",
                "refresh",
                "--from",
                "owner",
                "--via",
                "-oProxyCommand=evil",
            ][..],
            &["borrow", "refresh", "--from", "bad host"][..],
        ] {
            let mut cmd = cli(&dir, args);
            fake.apply(&mut cmd);
            let out = cmd.output().unwrap();
            assert!(!out.status.success());
        }
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn t3_with_warm_cache_execs_without_ssh_and_rotates_accounts() {
        let owner = owner_with(&["a", "b"]);
        let dir = TempDir::new().unwrap();
        auth(&dir, "local");
        let shared = dir.path().join("codex/auth.json");
        let shared_before = fs::read(&shared).unwrap();
        let fake = Fake::new(&dir, &owner);
        let mut refresh = cli(&dir, &["borrow", "refresh", "--from", "owner"]);
        fake.apply(&mut refresh);
        success(refresh.output().unwrap());
        let calls_after_refresh = fake.calls().len();

        let mut homes = Vec::new();
        for _ in 0..3 {
            let child = t3(&dir, &fake)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let pid = child.id();
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            assert_eq!(
                stdout.lines().next(),
                Some(pid.to_string().as_str()),
                "exec keeps the pid"
            );
            assert_eq!(stdout.lines().nth(2), Some("app-server"));
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(!stderr.contains("e30.") && !stderr.contains("secret-refresh"));
            homes.push(home_of(&out));
        }
        assert_eq!(
            fake.calls().len(),
            calls_after_refresh,
            "warm cache must not use ssh"
        );
        assert_ne!(homes[0], homes[1], "accounts rotate");
        assert_eq!(homes[0], homes[2]);

        let home = &homes[0];
        assert!(home.starts_with(dir.path().join("data/cx/borrow-overlays/owner")));
        assert_eq!(
            fs::metadata(home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let auth_path = home.join("auth.json");
        assert_eq!(
            fs::metadata(&auth_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = fs::read_to_string(&auth_path).unwrap();
        assert!(!text.contains("secret-refresh"));
        let value: Value = serde_json::from_str(&text).unwrap();
        let alias = home.file_name().unwrap().to_str().unwrap();
        assert_eq!(value["auth_mode"], "chatgpt");
        assert_eq!(value["tokens"]["refresh_token"], "");
        assert_eq!(value["tokens"]["account_id"], alias);
        assert_eq!(value["tokens"]["access_token"], jwt_for(alias));
        assert_eq!(value["tokens"]["id_token"], jwt_for(alias));
        chrono::DateTime::parse_from_rfc3339(value["last_refresh"].as_str().unwrap()).unwrap();
        assert!(fs::read_to_string(home.join("config.toml"))
            .unwrap()
            .contains("cli_auth_credentials_store = \"file\""));
        assert_eq!(
            fs::read(&shared).unwrap(),
            shared_before,
            "shared login untouched"
        );
        assert!(!dir.path().join("data/cx/t3-overlays").exists());
    }

    #[test]
    fn t3_cold_cache_refreshes_once_for_concurrent_probes() {
        let owner = owner_with(&["a", "b"]);
        let dir = TempDir::new().unwrap();
        let fake = Fake::new(&dir, &owner);
        let children: Vec<_> = (0..4)
            .map(|_| {
                let mut cmd = t3(&dir, &fake);
                cmd.env("CX_BORROW_JUMP", "jump")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(home_of(&out).starts_with(dir.path().join("data/cx/borrow-overlays/owner")));
        }
        let inventories = fake
            .calls()
            .iter()
            .filter(|c| c.starts_with("ssh ") && c.contains("--inventory"))
            .count();
        assert_eq!(inventories, 1, "single-flight refresh");
        assert!(
            fake.calls().iter().any(|c| c.starts_with("ssh ")),
            "env jump host is used"
        );
    }

    #[test]
    fn t3_falls_back_to_local_routing_when_the_owner_is_unreachable() {
        for saved_local in [true, false] {
            let dir = TempDir::new().unwrap();
            if saved_local {
                auth(&dir, "local");
                success(command(&dir, &["add", "local", "--current"]));
            }
            let marker = dir.path().join("ssh-calls");
            let failing = dir.path().join("failing-ssh");
            fs::write(
                &failing,
                format!("#!/bin/sh\necho x >> \"{}\"\nexit 255\n", marker.display()),
            )
            .unwrap();
            fs::set_permissions(&failing, fs::Permissions::from_mode(0o700)).unwrap();
            let owner = TempDir::new().unwrap();
            let fake = Fake::new(&dir, &owner);
            for round in 0..2 {
                let out = t3(&dir, &fake).env("CX_SSH", &failing).output().unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let stderr = String::from_utf8_lossy(&out.stderr);
                if round == 0 {
                    assert!(stderr.contains("borrow from owner unavailable"), "{stderr}");
                }
                let home = home_of(&out);
                if saved_local {
                    assert!(
                        home.starts_with(dir.path().join("data/cx/t3-overlays")),
                        "{home:?}"
                    );
                } else {
                    assert_eq!(home, dir.path().join("codex"));
                }
            }
            let tries = fs::read_to_string(&marker).unwrap().lines().count();
            assert_eq!(tries, 1, "a recent failure is not retried on every spawn");
        }
    }

    #[test]
    fn environment_borrow_does_not_break_unrelated_commands() {
        let dir = TempDir::new().unwrap();
        auth(&dir, "a");
        success(command(&dir, &["add", "a", "--current"]));
        let out = cli(&dir, &["list"])
            .env("CX_BORROW", "owner")
            .output()
            .unwrap();
        assert!(success(out).contains("a  a@example.test"));
        let out = cli(&dir, &["lend", "--inventory"])
            .env("CX_BORROW", "owner")
            .output()
            .unwrap();
        assert!(success(out).contains("\"alias\":\"a\""));
    }
}
