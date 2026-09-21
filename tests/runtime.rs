#![cfg(unix)]
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

const RELAY_THREAD: &str = "11111111-1111-4111-8111-111111111111";
const RELAY_ATTEMPT: &str = "22222222-2222-4222-8222-222222222222";
const RELAY_BINDING: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Fixture {
    dir: TempDir,
    child: Option<Child>,
}
impl Fixture {
    fn new(scenario: &str) -> Self {
        let dir = TempDir::new().unwrap();
        let script = dir.path().join("codex");
        fs::write(&script, include_str!("fake_codex.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(dir.path().join("home")).unwrap();
        fs::create_dir(dir.path().join("private-tmp")).unwrap();
        symlink(dir.path().join("private-tmp"), dir.path().join("tmp-link")).unwrap();
        let mut fixture = Self { dir, child: None };
        for account in ["personal", "work"] {
            let claims = json!({"email":format!("{account}@example.test"),"exp":4102444800_i64,"https://api.openai.com/auth":{"chatgpt_account_id":account,"chatgpt_user_id":account,"chatgpt_plan_type":"plus"}});
            let jwt = format!(
                "e30.{}.sig",
                URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
            );
            fs::write(fixture.dir.path().join("home/auth.json"),serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{"account_id":account,"access_token":jwt,"id_token":jwt,"refresh_token":"secret"}})).unwrap()).unwrap();
            fixture.ok(&["add", account, "--current"]);
        }
        fixture.ok(&["switch", "personal", "--yes"]);
        let child = fixture
            .command()
            .env("CX_TEST_SCENARIO", scenario)
            .stdin(Stdio::null())
            .stdout(Stdio::from(
                fs::File::create(fixture.dir.path().join("stdout")).unwrap(),
            ))
            .stderr(Stdio::from(
                fs::File::create(fixture.dir.path().join("stderr")).unwrap(),
            ))
            .process_group(0)
            .spawn()
            .unwrap();
        fixture.child = Some(child);
        fixture
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cx"));
        cmd.env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("CODEX_HOME", self.dir.path().join("home"))
            .env("CX_CODEX_BIN", self.dir.path().join("codex"))
            .env("CX_TEST_DIR", self.dir.path())
            .env("TMPDIR", self.dir.path().join("tmp-link"))
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY");
        cmd
    }
    fn ok(&self, args: &[&str]) -> String {
        let out = self.command().args(args).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
    fn touch(&self, name: &str) {
        fs::write(self.dir.path().join(name), "").unwrap();
    }
    fn events(&self) -> Vec<Value> {
        fs::read_to_string(self.dir.path().join("events"))
            .unwrap_or_default()
            .lines()
            .filter_map(|s| serde_json::from_str(s).ok())
            .collect()
    }
    fn relay(&self, request: Value) -> Value {
        self.relay_result(request).unwrap()
    }
    fn relay_result(&self, request: Value) -> Result<Value, String> {
        let path = fs::read_to_string(self.dir.path().join("relay_path")).unwrap();
        let mut socket = UnixStream::connect(path.trim()).map_err(|error| error.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .map_err(|error| error.to_string())?;
        socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .map_err(|error| error.to_string())?;
        let payload = serde_json::to_vec(&request).unwrap();
        socket
            .write_all(&(payload.len() as u32).to_be_bytes())
            .map_err(|error| error.to_string())?;
        socket
            .write_all(&payload)
            .map_err(|error| error.to_string())?;
        let mut length = [0_u8; 4];
        socket
            .read_exact(&mut length)
            .map_err(|error| error.to_string())?;
        let mut response = vec![0_u8; u32::from_be_bytes(length) as usize];
        socket
            .read_exact(&mut response)
            .map_err(|error| error.to_string())?;
        serde_json::from_slice(&response).map_err(|error| error.to_string())
    }
    fn wait(&mut self, condition: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !condition(self) {
            if let Some(exit) = self.child.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "cx exited {exit}: {}",
                    fs::read_to_string(self.dir.path().join("stderr")).unwrap()
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out; stderr: {}",
                fs::read_to_string(self.dir.path().join("stderr")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn finish(&mut self) -> std::process::ExitStatus {
        self.touch("quit");
        let start = Instant::now();
        loop {
            if let Some(exit) = self.child.as_mut().unwrap().try_wait().unwrap() {
                return exit;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "cx did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[test]
fn lam_relay_binds_inspects_and_queues_the_exact_thread() {
    let mut f = Fixture::new("lam-relay");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|event| event.get("relay_bind").is_some())
    });
    let bind = f
        .events()
        .into_iter()
        .find_map(|event| event.get("relay_bind").cloned())
        .unwrap();
    assert_eq!(
        bind["ok"],
        true,
        "{bind}; stderr: {}",
        fs::read_to_string(f.dir.path().join("stderr")).unwrap()
    );
    let relay_path = fs::read_to_string(f.dir.path().join("relay_path")).unwrap();
    let relay_parent = std::path::Path::new(relay_path.trim()).parent().unwrap();
    assert_eq!(relay_parent, relay_parent.canonicalize().unwrap());
    assert_eq!(
        fs::metadata(relay_path.trim())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(f
        .events()
        .iter()
        .any(|event| event["server_has_relay"] == true));
    assert!(f
        .events()
        .iter()
        .any(|event| event["tui_has_relay"] == false));

    let inspect = f.relay(json!({
        "version": 1,
        "operation": "inspect",
        "thread_id": RELAY_THREAD,
        "binding": RELAY_BINDING
    }));
    assert_eq!(inspect, json!({"version":1,"ok":true,"state":"idle"}));

    let queue = f.relay(json!({
        "version": 1,
        "operation": "queue",
        "thread_id": RELAY_THREAD,
        "binding": RELAY_BINDING,
        "attempt_id": RELAY_ATTEMPT,
        "text": "peer body"
    }));
    assert_eq!(queue["ok"], true);
    assert!(uuid::Uuid::parse_str(queue["receipt"].as_str().unwrap()).is_ok());
    f.wait(|f| {
        f.events().iter().any(|event| {
            event["relay_queue"]["threadId"] == RELAY_THREAD
                && event["relay_queue"]["clientUserMessageId"] == RELAY_ATTEMPT
                && event["relay_queue"]["input"]
                    == json!([{"type":"text","text":"peer body","text_elements":[]}])
        })
    });

    let rejected = f.relay(json!({
        "version": 1,
        "operation": "inspect",
        "thread_id": RELAY_THREAD,
        "binding": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    }));
    assert_eq!(rejected["ok"], false);
    assert_eq!(rejected["error"], "unauthorized");
    assert_eq!(rejected["submission"], "not_started");
    let unrelated_bind = f.relay(json!({
        "version": 1,
        "operation": "bind",
        "thread_id": RELAY_THREAD,
        "binding": RELAY_BINDING
    }));
    assert_eq!(unrelated_bind["error"], "unauthorized");
    assert_eq!(unrelated_bind["submission"], "not_started");
    assert!(!f
        .events()
        .iter()
        .any(|event| event.get("relay_response_leaked_to_tui").is_some()));
    assert!(f.finish().success());
}

#[test]
fn lam_relay_rejects_thread_changes_and_conflicting_rebinds() {
    let mut wrong = Fixture::new("lam-wrong-thread");
    wrong.wait(|f| {
        f.events()
            .iter()
            .any(|event| event.get("relay_bind").is_some())
    });
    let response = wrong
        .events()
        .into_iter()
        .find_map(|event| event.get("relay_bind").cloned())
        .unwrap();
    assert_eq!(response["error"], "thread_mismatch");
    assert!(wrong.finish().success());

    let mut conflict = Fixture::new("lam-conflict");
    conflict.wait(|f| {
        f.events()
            .iter()
            .any(|event| event.get("relay_conflict").is_some())
    });
    let response = conflict
        .events()
        .into_iter()
        .find_map(|event| event.get("relay_conflict").cloned())
        .unwrap();
    assert_eq!(response["error"], "conflict");
    assert_eq!(response["submission"], "not_started");
    assert!(conflict.finish().success());
}

#[test]
fn lam_relay_never_accepts_malformed_or_refused_queue_receipts() {
    for scenario in [
        "lam-missing-receipt",
        "lam-wrong-attempt",
        "lam-queue-error",
    ] {
        let mut f = Fixture::new(scenario);
        f.wait(|f| {
            f.events()
                .iter()
                .any(|event| event["relay_bind"]["ok"] == true)
        });
        let response = f.relay(json!({
            "version": 1,
            "operation": "queue",
            "thread_id": RELAY_THREAD,
            "binding": RELAY_BINDING,
            "attempt_id": RELAY_ATTEMPT,
            "text": "peer body"
        }));
        assert_eq!(response["ok"], false, "{scenario}: {response}");
        assert_eq!(
            response["error"], "upstream_error",
            "{scenario}: {response}"
        );
        assert_eq!(
            response["submission"], "uncertain",
            "{scenario}: {response}"
        );
        assert!(!response.to_string().contains("provider-private-text"));
        assert!(f.finish().success());
    }
}

#[test]
fn lam_relay_lost_post_write_response_is_not_safe_to_retry() {
    let mut f = Fixture::new("lam-queue-drop");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|event| event["relay_bind"]["ok"] == true)
    });
    let response = f.relay_result(json!({
        "version": 1,
        "operation": "queue",
        "thread_id": RELAY_THREAD,
        "binding": RELAY_BINDING,
        "attempt_id": RELAY_ATTEMPT,
        "text": "peer body"
    }));
    if let Ok(response) = response {
        assert_eq!(response["ok"], false);
        assert_eq!(response["error"], "timeout");
        assert_eq!(response["submission"], "uncertain");
    }
    assert!(f
        .events()
        .iter()
        .any(|event| event.get("relay_queue").is_some()));
    assert!(f.finish().success());
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            if child.try_wait().ok().flatten().is_none() {
                unsafe {
                    libc::kill(child.id() as i32, libc::SIGTERM);
                }
                for _ in 0..50 {
                    if child.try_wait().ok().flatten().is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[test]
fn bare_cx_keeps_same_children_when_switching_an_idle_session() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let output = f.ok(&["switch", "work", "--yes"]);
    assert!(!output.contains("pending"), "{output}");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert_eq!(
        f.events()
            .iter()
            .filter(|e| e.get("tui_pid").is_some())
            .count(),
        1
    );
    let logins: Vec<_> = f
        .events()
        .into_iter()
        .filter(|e| e.get("login").is_some())
        .collect();
    assert_eq!(logins.len(), 2);
    assert_eq!(logins[0]["server_pid"], logins[1]["server_pid"]);
    assert!(f.finish().success());
    assert_eq!(
        fs::read_dir(f.dir.path().join("data/cx/runtimes"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn model_selection_does_not_write_shared_config() {
    let mut f = Fixture::new("model-isolation");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e.get("model_write_response").is_some())
    });
    assert!(
        !f.events().iter().any(|e| e.get("config_write").is_some()),
        "model selection reached the shared Codex config"
    );
    assert!(!f.dir.path().join("home/config.toml").exists());
    assert!(f
        .events()
        .iter()
        .any(|e| e["thread_model"] == "gpt-5.6-luna"));
    let response = f
        .events()
        .into_iter()
        .find(|e| e.get("model_write_response").is_some())
        .unwrap();
    assert_eq!(response["model_write_response"]["status"], "ok");
    assert!(f.finish().success());
}

#[test]
fn single_model_config_write_does_not_reach_shared_config() {
    let mut f = Fixture::new("model-single-write");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e.get("model_write_response").is_some())
    });
    assert!(!f.events().iter().any(|e| e.get("config_write").is_some()));
    assert!(!f.dir.path().join("home/config.toml").exists());
    assert!(f.finish().success());
}

#[test]
fn mixed_config_write_preserves_unrelated_changes() {
    let mut f = Fixture::new("mixed-config-write");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e.get("mixed_write_response").is_some())
    });
    assert!(f
        .events()
        .iter()
        .any(|e| e["config_write"] == json!(["tui.notifications"])));
    assert_eq!(
        fs::read_to_string(f.dir.path().join("home/config.toml")).unwrap(),
        "tui.notifications\n"
    );
    assert!(f.finish().success());
}

#[test]
fn clear_reads_the_panes_model_instead_of_shared_defaults() {
    let mut f = Fixture::new("clear-model-isolation");
    f.wait(|f| f.events().iter().any(|e| e.get("clear_defaults").is_some()));
    let event = f
        .events()
        .into_iter()
        .find(|e| e.get("clear_defaults").is_some())
        .unwrap();
    assert_eq!(event["clear_defaults"]["model"], "gpt-5.6-luna");
    assert_eq!(event["clear_defaults"]["effort"], "high");
    assert!(f.finish().success());
}

#[test]
fn native_turn_creating_commands_defer_switch_until_completion() {
    for method in [
        "turn/start",
        "review/start",
        "thread/compact/start",
        "thread/queue/start",
    ] {
        let mut f = Fixture::new(method);
        f.wait(|f| f.dir.path().join("busy").exists());
        let report = f.ok(&["switch", "work", "--yes"]);
        assert!(report.contains("pending"), "{method}: {report}");
        assert!(!f.events().iter().any(|e| e["login"] == "work"));
        f.touch("release");
        f.touch("complete");
        f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
        assert!(f.finish().success());
    }
}

#[test]
fn rejected_switch_reports_failure_without_echoing_provider_payload() {
    let mut f = Fixture::new("reject");
    f.wait(|f| f.dir.path().join("ready").exists());
    let output = f
        .command()
        .args(["switch", "work", "--yes"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("secret-do-not-print"));
    assert!(!f.finish().success());
}

#[test]
fn replacing_the_active_alias_updates_the_runtime_identity() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(f.dir.path().join("data/cx/state.json")).unwrap())
            .unwrap();
    let work = state["accounts"]["work"]["data"].clone();
    f.ok(&["del", "work"]);
    fs::write(
        f.dir.path().join("home/auth.json"),
        serde_json::to_vec(&work).unwrap(),
    )
    .unwrap();
    let report = f.ok(&["add", "personal", "--current", "--force"]);
    assert!(report.contains("work@example.test"), "{report}");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert!(f.finish().success());
}

#[test]
fn an_alias_replaced_during_login_is_not_acknowledged_as_the_old_identity() {
    let mut f = Fixture::new("hold-login");
    f.wait(|f| f.dir.path().join("auth_pending").exists());
    let state: Value =
        serde_json::from_slice(&fs::read(f.dir.path().join("data/cx/state.json")).unwrap())
            .unwrap();
    let work = state["accounts"]["work"]["data"].clone();
    f.ok(&["del", "work"]);
    fs::write(
        f.dir.path().join("home/auth.json"),
        serde_json::to_vec(&work).unwrap(),
    )
    .unwrap();
    let report = f.ok(&["add", "personal", "--current", "--force"]);
    assert!(report.contains("pending"), "{report}");
    f.touch("auth_release");
    f.wait(|f| f.events().iter().any(|e| e["login"] == "work"));
    assert!(f.finish().success());
}

#[test]
fn terminal_interrupt_does_not_kill_the_app_server() {
    let mut f = Fixture::new("idle");
    f.wait(|f| f.dir.path().join("ready").exists());
    let pid = f.child.as_ref().unwrap().id() as i32;
    assert_eq!(unsafe { libc::kill(-pid, libc::SIGINT) }, 0);
    f.wait(|f| f.dir.path().join("interrupted").exists());
    f.ok(&["switch", "work", "--yes"]);
    assert!(f.finish().success());
}

#[test]
fn termination_during_startup_cleans_both_children_and_socket() {
    let mut f = Fixture::new("startup");
    f.wait(|f| {
        f.events().iter().any(|e| e.get("tui_pid").is_some())
            && f.events().iter().any(|e| e.get("server_pid").is_some())
    });
    let pid = f.child.as_ref().unwrap().id() as i32;
    assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    assert_eq!(f.finish().code(), Some(143));
    for event in f.events() {
        for key in ["server_pid", "tui_pid"] {
            if let Some(pid) = event[key].as_i64() {
                assert_eq!(
                    unsafe { libc::kill(pid as i32, 0) },
                    -1,
                    "child {pid} still alive"
                );
            }
        }
    }
    assert_eq!(
        fs::read_dir(f.dir.path().join("data/cx/runtimes"))
            .unwrap()
            .count(),
        0
    );
}

/// Uses isolated homes and synthetic JWT claims: validates actual Codex account
/// loading, without real credentials, inference, or a browser login.
#[test]
#[ignore = "requires Codex CLI on PATH; no real credentials needed"]
fn installed_codex_applies_account_changes_without_restarting() {
    let mut f = Fixture::new("actual");
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e["actual_email"] == "personal@example.test")
    });
    f.ok(&["switch", "work", "--yes"]);
    f.wait(|f| {
        f.events()
            .iter()
            .any(|e| e["actual_email"] == "work@example.test")
    });
    assert_eq!(
        f.events()
            .iter()
            .filter(|e| e.get("server_pid").is_some())
            .count(),
        1
    );
    assert!(f.finish().success());
}
