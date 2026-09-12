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
fn help_exposes_cs_commands_and_has_no_run_subcommand() {
    let dir = TempDir::new().unwrap();
    let help = success(command(&dir, &["--help"]));
    for name in ["add", "switch", "list", "del", "whoami", "refresh", "usage"] {
        assert!(help.contains(name), "{help}");
    }
    assert!(!help
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
